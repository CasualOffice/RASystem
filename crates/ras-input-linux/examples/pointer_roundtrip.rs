//! On-device self-check for the Linux XTEST input path (the Linux analogue of the macOS
//! `pointer_roundtrip`, §3 of the input verification checklist).
//!
//! Closed-loop, **no human eye required**: it injects a pointer move to a known normalized point via
//! the real [`X11InputSink`], then reads the live cursor back through X11 `QueryPointer` and asserts it
//! landed within tolerance. The remaining rows (keyboard focus, the X11-vs-Wayland reach) still want a
//! human — see the checklist.
//!
//! Run on a Linux X11 / Xwayland session:
//!   `cargo run -p ras-input-linux --example pointer_roundtrip`
//!
//! Exit code 0 = injection verified; 2 = no reachable X server (expected under a headless/CI or
//! pure-Wayland run — it prints why); 1 = an X server was reachable but the cursor did not land (a bug).

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pointer_roundtrip is Linux-only (the XTEST backend is empty elsewhere).");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() {
    use ras_control::OsInputSink;
    use ras_input_linux::X11InputSink;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    let sink = X11InputSink::new();

    // 1) Fail-closed: no reachable X server ⇒ the host would refuse the lease. That is the *correct*
    //    behaviour on a headless/CI box or a pure-Wayland session (XTEST reaches only Xwayland).
    if !sink.input_permitted() {
        eprintln!(
            "No reachable X server ($DISPLAY unset, headless, or pure-Wayland) — cannot inject."
        );
        eprintln!("This is expected off an X11/Xwayland session. To verify injection:");
        eprintln!("  • run this from a terminal inside an X11 (or Xwayland) login session.");
        eprintln!("RESULT: SKIPPED (no X server) — the fail-closed path itself is correct.");
        std::process::exit(2);
    }

    // A second connection, used only to read the cursor position back.
    let (conn, screen_num) = match RustConnection::connect(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("RESULT: INCONCLUSIVE — could not open a read connection: {e}");
            std::process::exit(1);
        }
    };
    let screen = &conn.setup().roots[screen_num];
    let (root, sw, sh) = (screen.root, screen.width_in_pixels, screen.height_in_pixels);

    // 2) Register the screen's real bounds, exactly as the host feeds capture geometry.
    sink.set_display_bounds(0, 0.0, 0.0, f64::from(sw), f64::from(sh));

    // 3) Inject a move to the screen centre (normalized 0.5, 0.5) and read the cursor back.
    let (tx, ty) = (i32::from(sw) / 2, i32::from(sh) / 2);
    if let Err(e) = sink.pointer_move(0, 0.5, 0.5) {
        eprintln!("RESULT: FAIL — pointer_move errored: {e:?}");
        std::process::exit(1);
    }
    // Give the X server a beat to apply the posted event before reading back.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let reply = match conn.query_pointer(root).map(|c| c.reply()) {
        Ok(Ok(r)) => r,
        _ => {
            eprintln!("RESULT: INCONCLUSIVE — QueryPointer failed.");
            std::process::exit(1);
        }
    };
    let (px, py) = (i32::from(reply.root_x), i32::from(reply.root_y));
    let (dx, dy) = ((px - tx).abs(), (py - ty).abs());
    // 2px tolerance for any pointer-acceleration snapping.
    println!("screen        : {sw}x{sh}");
    println!("target centre : ({tx}, {ty})");
    println!("cursor after  : ({px}, {py})  Δ=({dx}, {dy})");
    if dx <= 2 && dy <= 2 {
        println!("RESULT: PASS — injected move landed within 2px of target.");
    } else {
        println!("RESULT: FAIL — cursor did not land on the injected point.");
        std::process::exit(1);
    }
}
