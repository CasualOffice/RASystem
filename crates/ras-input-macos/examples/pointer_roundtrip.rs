//! On-device self-check for the macOS CGEvent input path (§3 of the Phase-3 verification checklist).
//!
//! Closed-loop, **no human eye required**: it injects a pointer move to a known normalized point via
//! the real [`CgEventSink`], then reads the live cursor back through CGEvent and asserts it landed
//! within tolerance. This is the part of the on-device matrix that *can* be automated — the
//! Secure-Input drop and the visible-consent rows still need a human (see the checklist).
//!
//! Run on a macOS login session:  `cargo run -p ras-input-macos --example pointer_roundtrip`
//!
//! Exit code 0 = injection verified; 2 = PostEvent access not granted (expected in a headless / CI
//! context — it prints how to grant it); 1 = access granted but the cursor did not land (a real bug).

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("pointer_roundtrip is macOS-only (the CGEvent backend is empty elsewhere).");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    use core_graphics::display::CGDisplay;
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use ras_control::OsInputSink;
    use ras_input_macos::CgEventSink;

    // A fresh source, used only to read the current cursor location (CGEvent::new places a null event
    // at the live pointer position — the standard read-back trick).
    fn read_cursor() -> Option<CGPoint> {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
        CGEvent::new(src).ok().map(|e| e.location())
    }

    let sink = CgEventSink::new();

    // 1) Preflight WITHOUT prompting. Fail-closed: if this is false the host refuses the lease, so a
    //    false here is the *correct* behaviour in an un-granted context — report and exit 2, not 1.
    if !sink.input_permitted() {
        eprintln!("PostEvent access NOT granted for this process — cannot inject (fail-closed).");
        eprintln!("This is expected under a headless/CI run. To verify injection on-device:");
        eprintln!("  • run this from a Terminal in a real login session, and");
        eprintln!("  • grant the process PostEvent access when macOS prompts");
        eprintln!(
            "    (System Settings ▸ Privacy & Security ▸ the 'control your computer' bucket)."
        );
        eprintln!(
            "RESULT: SKIPPED (no PostEvent access) — the fail-closed path itself is correct."
        );
        std::process::exit(2);
    }

    // 2) Register the main display's real bounds, exactly as the host feeds capture geometry.
    let bounds = CGDisplay::main().bounds();
    let (ox, oy, w, h) = (
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width,
        bounds.size.height,
    );
    sink.set_display_bounds(0, ox, oy, w, h);

    let before = read_cursor();

    // 3) Inject a move to the display centre (normalized 0.5, 0.5) and read the cursor back.
    let target = CGPoint::new(ox + 0.5 * w, oy + 0.5 * h);
    if let Err(e) = sink.pointer_move(0, 0.5, 0.5) {
        eprintln!("RESULT: FAIL — pointer_move errored: {e:?}");
        std::process::exit(1);
    }
    // Give the window server a beat to apply the posted event before reading back.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let after = read_cursor();

    match after {
        Some(p) => {
            let dx = (p.x - target.x).abs();
            let dy = (p.y - target.y).abs();
            // 2px tolerance: HiDPI rounding / cursor-acceleration snapping.
            let ok = dx <= 2.0 && dy <= 2.0;
            println!("display bounds: origin=({ox},{oy}) size=({w}x{h})");
            println!("cursor before : {before:?}");
            println!("target centre : ({:.1}, {:.1})", target.x, target.y);
            println!(
                "cursor after  : ({:.1}, {:.1})  Δ=({dx:.2}, {dy:.2})",
                p.x, p.y
            );
            if ok {
                println!("RESULT: PASS — injected move landed within 2px of target.");
            } else {
                println!("RESULT: FAIL — cursor did not land on the injected point.");
                std::process::exit(1);
            }
        }
        None => {
            eprintln!("RESULT: INCONCLUSIVE — could not read the cursor back after injecting.");
            std::process::exit(1);
        }
    }
}
