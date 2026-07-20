//! macOS OS-input backend (ADR-068, implements ADR-055): a [`ras_control::OsInputSink`] over CGEvent.
//!
//! `unsafe`/FFI is confined here (CONTRIBUTING §5); `ras-control` stays `unsafe`-free. The crate is
//! **empty on non-macOS** so Linux/Windows CI stays green.
//!
//! # Deliberately unprivileged (Inv 14, ADR-055)
//! Injection is gated on the **PostEvent** TCC bucket (`CGPreflightPostEventAccess`), *not*
//! Accessibility, and runs in the per-user agent — never root. It therefore cannot inject into a
//! Secure-Input (password/login) field, and we never try to: that is the fraud-model boundary
//! (docs/18 §0), surfaced honestly.
//!
//! # Coordinates (Inv 6)
//! The trait receives only **normalized** `0.0..=1.0` fractions of a display; this backend maps them
//! to global points using display bounds fed from the host's capture geometry. The controller never
//! sends pixels.
//!
//! **Runtime status:** compiles on macOS; the live CGEvent injection, the PostEvent-TCC prompt, the
//! Secure-Input drop, and multi-monitor coordinate mapping are an **on-device** verification step
//! (a login session + granted PostEvent access) — the same constraint as every prior macOS backend.

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use core_graphics::display::CGDisplay;
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton,
        EventField, ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use ras_control::{InputError, OsInputSink};
    use ras_protocol::{ErrorCode, PointerButton, RasError};

    // PostEvent TCC (macOS 10.15+): preflight WITHOUT prompting, and request (prompts once). Declared
    // here because `core-graphics` does not wrap them; they live in the CoreGraphics framework the
    // crate already links. Both return a C `bool`.
    //
    // The cursor-warp discipline symbols (RustDesk two-cursor fix, PR #10314 class) also live in
    // CoreGraphics and are un-wrapped by `core-graphics`, so they are declared here too. Signatures
    // (CGRemoteOperation.h / CGDirectDisplay.h): `boolean_t` is a C `int` (map to i32), `CGError` is
    // `int32_t` (i32, `kCGErrorSuccess == 0`), `CGDirectDisplayID` is `uint32_t` (u32).
    extern "C" {
        fn CGPreflightPostEventAccess() -> bool;
        fn CGRequestPostEventAccess() -> bool;
        // Current modifier/lock flags of an event-source state (HIDSystemState = 1). Returns a
        // CGEventFlags bitset; bit `kCGEventFlagMaskAlphaShift` (0x00010000) is CapsLock.
        fn CGEventSourceFlagsState(state_id: i32) -> u64;
        // Decouple the hardware mouse from the on-screen cursor position (`connected == 0`) so an
        // absolute warp does not fight the local user's physical mouse (the "two cursors" artifact).
        // `connected != 0` re-couples it.
        fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
        // Hide / show the local cursor on a display while remote control is warping it.
        fn CGDisplayHideCursor(display: u32) -> i32;
        fn CGDisplayShowCursor(display: u32) -> i32;
        fn CGMainDisplayID() -> u32;
    }

    /// Display bounds in global points, for normalized→pixel mapping.
    #[derive(Debug, Clone, Copy)]
    struct DisplayBounds {
        id: u32,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    /// A CGEvent-backed [`OsInputSink`]. All fields are plain data behind `Mutex`es (so the sink is
    /// `Send + Sync`); the non-`Send` `CGEventSource` is created per event, never stored.
    #[derive(Debug, Default)]
    pub struct CgEventSink {
        pressed_keys: Mutex<HashSet<CGKeyCode>>,
        pressed_buttons: Mutex<HashSet<u8>>,
        last_point: Mutex<(f64, f64)>,
        displays: Mutex<Vec<DisplayBounds>>,
        /// Warp-discipline guard (RustDesk PR #10314 class). `true` while we have dissociated the
        /// hardware mouse from the cursor position and hidden the local cursor for an absolute warp.
        /// Latched on the first warp of a control burst and released **only** by [`Self::end_warp`],
        /// which the emergency-stop / teardown path (`release_all`) always calls — so a stop can
        /// never leave the local cursor dissociated or hidden (Inv 4).
        cursor_dissociated: Mutex<bool>,
    }

    impl CgEventSink {
        /// Create an input sink. Does not prompt for permission — call [`CgEventSink::request_access`]
        /// (or check [`OsInputSink::input_permitted`]) before issuing a lease.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Prompt the user for PostEvent TCC access (once). Returns whether access is now granted.
        #[must_use]
        pub fn request_access(&self) -> bool {
            // SAFETY: a plain C predicate with no arguments; safe to call from any thread.
            unsafe { CGRequestPostEventAccess() }
        }

        /// Register a display's global bounds (points), from the host's capture geometry. Replaces any
        /// prior bounds for the same display id.
        pub fn set_display_bounds(&self, id: u32, x: f64, y: f64, w: f64, h: f64) {
            let mut d = self.displays.lock().unwrap_or_else(|e| e.into_inner());
            d.retain(|b| b.id != id);
            d.push(DisplayBounds { id, x, y, w, h });
        }

        /// Map a normalized `(nx, ny)` on `display` to a global point. Falls back to the main display
        /// if the id is unknown (fail-safe: input still lands on the primary screen).
        fn to_point(&self, display: u32, nx: f32, ny: f32) -> CGPoint {
            let bounds = {
                let d = self.displays.lock().unwrap_or_else(|e| e.into_inner());
                d.iter().find(|b| b.id == display).copied()
            };
            let (ox, oy, w, h) = match bounds {
                Some(b) => (b.x, b.y, b.w, b.h),
                None => {
                    let r = CGDisplay::main().bounds();
                    (r.origin.x, r.origin.y, r.size.width, r.size.height)
                }
            };
            // Defense-in-depth: a non-finite fraction (NaN passes through `f32::clamp` unchanged)
            // maps to the display origin, never off-screen. The authorized `dispatch` path feeds only
            // bounded `u16→f32` values, so this never triggers in production — belt-and-braces for the
            // public `OsInputSink` surface (Inv 6).
            let frac = |v: f32| {
                if v.is_finite() {
                    v.clamp(0.0, 1.0)
                } else {
                    0.0
                }
            };
            let x = ox + f64::from(frac(nx)) * w;
            let y = oy + f64::from(frac(ny)) * h;
            *self.last_point.lock().unwrap_or_else(|e| e.into_inner()) = (x, y);
            CGPoint::new(x, y)
        }

        /// The global bounding box `(x, y, w, h)` of the whole desktop — the union of registered display
        /// bounds, or the main display if none are registered. Used to keep relative motion on-screen.
        fn desktop_bounds(&self) -> (f64, f64, f64, f64) {
            let d = self.displays.lock().unwrap_or_else(|e| e.into_inner());
            if d.is_empty() {
                let r = CGDisplay::main().bounds();
                (r.origin.x, r.origin.y, r.size.width, r.size.height)
            } else {
                let min_x = d.iter().map(|b| b.x).fold(f64::INFINITY, f64::min);
                let min_y = d.iter().map(|b| b.y).fold(f64::INFINITY, f64::min);
                let max_x = d
                    .iter()
                    .map(|b| b.x + b.w)
                    .fold(f64::NEG_INFINITY, f64::max);
                let max_y = d
                    .iter()
                    .map(|b| b.y + b.h)
                    .fold(f64::NEG_INFINITY, f64::max);
                (min_x, min_y, max_x - min_x, max_y - min_y)
            }
        }

        /// Enter the cursor-warp state (RustDesk two-cursor fix, PR #10314 class): decouple the
        /// hardware mouse from the on-screen cursor and hide the local cursor, so an injected absolute
        /// warp does not fight the local user's physical mouse (which otherwise shows two cursors /
        /// a fighting cursor). **Idempotent and balanced**: it fires the FFI only on the
        /// `false → true` transition (a control burst stays dissociated for its duration, re-associating
        /// between every event is what reintroduces the artifact), and every enter is guaranteed a
        /// matching exit via [`Self::end_warp`] on the `release_all` teardown path.
        fn begin_warp(&self) {
            // Latch the flag under the lock and learn whether this is the `false → true` transition
            // (the only time we touch the OS). `latch_warp` is pure so the balance logic is unit-tested
            // without a display/login session; the FFI stays on-device.
            let fire = latch_warp(
                &mut self
                    .cursor_dissociated
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            );
            if fire {
                // SAFETY: plain C predicates over a scalar/display-id; safe from any thread. The
                // display id is the live main display. Return codes are best-effort (a non-zero
                // CGError does not change our latched state — we still record `true` so the balancing
                // `end_warp` unconditionally re-associates + unhides, never leaving the cursor stuck).
                unsafe {
                    CGAssociateMouseAndMouseCursorPosition(0);
                    CGDisplayHideCursor(CGMainDisplayID());
                }
            }
        }

        /// Exit the cursor-warp state: re-couple the hardware mouse to the cursor and show the local
        /// cursor again. **Idempotent** (a no-op if we never dissociated) and the guaranteed cleanup —
        /// [`OsInputSink::release_all`] calls it on **every** teardown path (emergency stop, lease end,
        /// session end), so a stop never leaves the local cursor hidden or dissociated (Inv 4).
        fn end_warp(&self) {
            let fire = unlatch_warp(
                &mut self
                    .cursor_dissociated
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            );
            if fire {
                // SAFETY: as `begin_warp`. Unconditionally restore the local cursor even if a prior
                // hide/dissociate call returned a CGError — showing/re-associating an already-shown/
                // associated cursor is a harmless no-op on the OS side.
                unsafe {
                    CGAssociateMouseAndMouseCursorPosition(1);
                    CGDisplayShowCursor(CGMainDisplayID());
                }
            }
        }
    }

    /// Pure transition for [`CgEventSink::begin_warp`]: set the guard and report whether this call is
    /// the `false → true` edge (i.e. whether the OS dissociate/hide should fire). Idempotent on repeat.
    fn latch_warp(dissociated: &mut bool) -> bool {
        if *dissociated {
            false
        } else {
            *dissociated = true;
            true
        }
    }

    /// Pure transition for [`CgEventSink::end_warp`]: clear the guard and report whether this call is
    /// the `true → false` edge (i.e. whether the OS re-associate/show should fire). Idempotent on repeat.
    fn unlatch_warp(dissociated: &mut bool) -> bool {
        if *dissociated {
            *dissociated = false;
            true
        } else {
            false
        }
    }

    fn source() -> Result<CGEventSource, InputError> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|()| {
            RasError::recoverable(ErrorCode::InputFailed, "CGEventSource unavailable")
        })
    }

    fn make_err() -> InputError {
        RasError::recoverable(ErrorCode::InputFailed, "CGEvent creation failed")
    }

    /// Modifier bitset → CGEventFlags. Bits: 0x01 shift, 0x02 control, 0x04 option, 0x08 command.
    fn flags_from_modifiers(modifiers: u8) -> CGEventFlags {
        let mut f = CGEventFlags::empty();
        if modifiers & 0x01 != 0 {
            f |= CGEventFlags::CGEventFlagShift;
        }
        if modifiers & 0x02 != 0 {
            f |= CGEventFlags::CGEventFlagControl;
        }
        if modifiers & 0x04 != 0 {
            f |= CGEventFlags::CGEventFlagAlternate;
        }
        if modifiers & 0x08 != 0 {
            f |= CGEventFlags::CGEventFlagCommand;
        }
        f
    }

    impl OsInputSink for CgEventSink {
        fn pointer_move(&self, display: u32, nx: f32, ny: f32) -> Result<(), InputError> {
            let point = self.to_point(display, nx, ny);
            // Warp discipline (PR #10314): dissociate + hide the local cursor before an absolute warp
            // so it does not fight the physical mouse. Balanced by `end_warp` in `release_all`.
            self.begin_warp();
            let event = CGEvent::new_mouse_event(
                source()?,
                CGEventType::MouseMoved,
                point,
                CGMouseButton::Left,
            )
            .map_err(|()| make_err())?;
            event.post(CGEventTapLocation::HID);
            Ok(())
        }

        fn pointer_move_relative(&self, dx: i16, dy: i16) -> Result<(), InputError> {
            // Read the *live* cursor position (a null event reports it), so this composes with any local
            // motion — not just our own last move. Add the delta, then clamp to the desktop so relative
            // motion can never park the cursor off-screen (Inv 6 fail-safe).
            let current = CGEvent::new(source()?).map_err(|()| make_err())?.location();
            let (bx, by, bw, bh) = self.desktop_bounds();
            let nx = (current.x + f64::from(dx)).clamp(bx, bx + (bw - 1.0).max(0.0));
            let ny = (current.y + f64::from(dy)).clamp(by, by + (bh - 1.0).max(0.0));
            // This posts an absolute reposition too (MouseMoved at the computed point), so it is a warp:
            // apply the same discipline. Balanced by `end_warp` in `release_all`.
            self.begin_warp();
            let event = CGEvent::new_mouse_event(
                source()?,
                CGEventType::MouseMoved,
                CGPoint::new(nx, ny),
                CGMouseButton::Left,
            )
            .map_err(|()| make_err())?;
            // Carry the relative delta too, so relative-aware apps (games, 3D viewers) see the motion,
            // not just the absolute reposition.
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, i64::from(dx));
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, i64::from(dy));
            event.post(CGEventTapLocation::HID);
            *self.last_point.lock().unwrap_or_else(|e| e.into_inner()) = (nx, ny);
            Ok(())
        }

        fn pointer_button(
            &self,
            display: u32,
            nx: f32,
            ny: f32,
            button: PointerButton,
            down: bool,
        ) -> Result<(), InputError> {
            let point = self.to_point(display, nx, ny);
            // A button event carries an absolute location, i.e. it warps the cursor: same discipline.
            self.begin_warp();
            let (event_type, cg_button) = match (button, down) {
                (PointerButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                (PointerButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                (PointerButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
                (PointerButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
                (PointerButton::Middle, true) => {
                    (CGEventType::OtherMouseDown, CGMouseButton::Center)
                }
                (PointerButton::Middle, false) => {
                    (CGEventType::OtherMouseUp, CGMouseButton::Center)
                }
                // Fail-closed for an unrecognized future button variant.
                _ => {
                    return Err(RasError::fatal(
                        ErrorCode::InputFailed,
                        "unknown pointer button",
                    ))
                }
            };
            let event = CGEvent::new_mouse_event(source()?, event_type, point, cg_button)
                .map_err(|()| make_err())?;
            event.post(CGEventTapLocation::HID);
            // Track pressed buttons for release_all.
            let mut pressed = self
                .pressed_buttons
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if down {
                pressed.insert(button_tag(button));
            } else {
                pressed.remove(&button_tag(button));
            }
            Ok(())
        }

        fn pointer_wheel(&self, dx: i16, dy: i16) -> Result<(), InputError> {
            // wheel1 = vertical, wheel2 = horizontal (CGScrollWheelEvent convention).
            let event = CGEvent::new_scroll_event(
                source()?,
                ScrollEventUnit::LINE,
                2,
                i32::from(dy),
                i32::from(dx),
                0,
            )
            .map_err(|()| make_err())?;
            event.post(CGEventTapLocation::HID);
            Ok(())
        }

        fn key(&self, hid_usage: u16, down: bool, modifiers: u8) -> Result<(), InputError> {
            let keycode = hid_to_virtual_keycode(hid_usage)
                .ok_or_else(|| RasError::fatal(ErrorCode::InputFailed, "unmapped physical key"))?;
            let event =
                CGEvent::new_keyboard_event(source()?, keycode, down).map_err(|()| make_err())?;
            event.set_flags(flags_from_modifiers(modifiers));
            event.post(CGEventTapLocation::HID);
            let mut pressed = self.pressed_keys.lock().unwrap_or_else(|e| e.into_inner());
            if down {
                pressed.insert(keycode);
            } else {
                pressed.remove(&keycode);
            }
            Ok(())
        }

        fn text(&self, utf8: &str) -> Result<(), InputError> {
            // A single keydown/up pair carrying the whole Unicode string (layout-independent).
            let down = CGEvent::new_keyboard_event(source()?, 0, true).map_err(|()| make_err())?;
            down.set_string(utf8);
            down.post(CGEventTapLocation::HID);
            let up = CGEvent::new_keyboard_event(source()?, 0, false).map_err(|()| make_err())?;
            up.set_string(utf8);
            up.post(CGEventTapLocation::HID);
            Ok(())
        }

        fn release_all(&self) -> Result<(), InputError> {
            // Best-effort key-state cleanup on the emergency-stop / teardown path (Inv 4): it must
            // release as much as possible and **never abort early**. A transient `source()` / event
            // failure skips that one release and continues — it never `?`-propagates out, which would
            // otherwise leave the already-drained keys neither tracked nor released (physically stuck).
            //
            // FIRST, and UNCONDITIONALLY, exit any cursor-warp state (PR #10314 cleanup): re-associate
            // the hardware mouse + unhide the local cursor. This is the guaranteed balancing exit for
            // every `begin_warp` — `release_all` is the single funnel the host calls on emergency stop,
            // lease end, and graceful teardown — so a stop can NEVER leave the local cursor hidden or
            // dissociated (Inv 4). It is idempotent (a no-op if we never warped) and cannot fail, so it
            // runs before the fallible key/button releases below (which never `?`-propagate anyway).
            self.end_warp();
            let keys: Vec<CGKeyCode> = {
                let mut pressed = self.pressed_keys.lock().unwrap_or_else(|e| e.into_inner());
                pressed.drain().collect()
            };
            for keycode in keys {
                if let Ok(src) = source() {
                    if let Ok(event) = CGEvent::new_keyboard_event(src, keycode, false) {
                        event.post(CGEventTapLocation::HID);
                    }
                }
            }
            // … and every button, at the last known cursor point.
            let buttons: Vec<u8> = {
                let mut pressed = self
                    .pressed_buttons
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                pressed.drain().collect()
            };
            let (x, y) = *self.last_point.lock().unwrap_or_else(|e| e.into_inner());
            let point = CGPoint::new(x, y);
            for tag in buttons {
                let (event_type, cg_button) = match tag {
                    0 => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                    1 => (CGEventType::RightMouseUp, CGMouseButton::Right),
                    _ => (CGEventType::OtherMouseUp, CGMouseButton::Center),
                };
                if let Ok(src) = source() {
                    if let Ok(event) = CGEvent::new_mouse_event(src, event_type, point, cg_button) {
                        event.post(CGEventTapLocation::HID);
                    }
                }
            }
            Ok(())
        }

        fn set_lock_state(&self, caps_lock: bool, num_lock: bool) -> Result<(), InputError> {
            let _ = num_lock; // macOS has no NumLock concept.
                              // SAFETY: a pure query of the HID system's current event flags.
            let cur_caps = unsafe { CGEventSourceFlagsState(1) } & 0x0001_0000 != 0;
            if cur_caps != caps_lock {
                // Toggle CapsLock (virtual keycode 0x39). Best-effort: reliable programmatic CapsLock
                // on macOS may need IOKit (`IOHIDSetModifierLockState`) — verified on-device.
                if let Ok(ev) = CGEvent::new_keyboard_event(source()?, 0x39, true) {
                    ev.post(CGEventTapLocation::HID);
                }
                if let Ok(ev) = CGEvent::new_keyboard_event(source()?, 0x39, false) {
                    ev.post(CGEventTapLocation::HID);
                }
            }
            Ok(())
        }

        fn input_permitted(&self) -> bool {
            // SAFETY: a plain C predicate with no arguments; safe to call from any thread. Preflight
            // does NOT prompt — a false result means the host must refuse the lease (fail-closed).
            unsafe { CGPreflightPostEventAccess() }
        }
    }

    fn button_tag(b: PointerButton) -> u8 {
        match b {
            PointerButton::Left => 0,
            PointerButton::Right => 1,
            PointerButton::Middle => 2,
            _ => 3,
        }
    }

    /// Map a USB-HID Keyboard/Keypad usage (page 0x07) to a macOS virtual keycode. Covers the common
    /// alphanumeric, whitespace, arrow, and modifier keys; an unmapped usage fails closed at the call
    /// site. The full table is completed during on-device verification.
    #[allow(clippy::too_many_lines)]
    fn hid_to_virtual_keycode(hid: u16) -> Option<CGKeyCode> {
        let vk: u16 = match hid {
            // Letters a–z (HID 0x04..=0x1D).
            0x04 => 0x00, // a
            0x05 => 0x0B, // b
            0x06 => 0x08, // c
            0x07 => 0x02, // d
            0x08 => 0x0E, // e
            0x09 => 0x03, // f
            0x0A => 0x05, // g
            0x0B => 0x04, // h
            0x0C => 0x22, // i
            0x0D => 0x26, // j
            0x0E => 0x28, // k
            0x0F => 0x25, // l
            0x10 => 0x2E, // m
            0x11 => 0x2D, // n
            0x12 => 0x1F, // o
            0x13 => 0x23, // p
            0x14 => 0x0C, // q
            0x15 => 0x0F, // r
            0x16 => 0x01, // s
            0x17 => 0x11, // t
            0x18 => 0x20, // u
            0x19 => 0x09, // v
            0x1A => 0x0D, // w
            0x1B => 0x07, // x
            0x1C => 0x10, // y
            0x1D => 0x06, // z
            // Digits 1–9,0 (HID 0x1E..=0x27).
            0x1E => 0x12, // 1
            0x1F => 0x13, // 2
            0x20 => 0x14, // 3
            0x21 => 0x15, // 4
            0x22 => 0x17, // 5
            0x23 => 0x16, // 6
            0x24 => 0x1A, // 7
            0x25 => 0x1C, // 8
            0x26 => 0x19, // 9
            0x27 => 0x1D, // 0
            // Whitespace / editing.
            0x28 => 0x24, // Return/Enter
            0x29 => 0x35, // Escape
            0x2A => 0x33, // Backspace/Delete
            0x2B => 0x30, // Tab
            0x2C => 0x31, // Space
            0x2D => 0x1B, // - _
            0x2E => 0x18, // = +
            0x2F => 0x21, // [ {
            0x30 => 0x1E, // ] }
            0x31 => 0x2A, // \ |
            0x33 => 0x29, // ; :
            0x34 => 0x27, // ' "
            0x35 => 0x32, // ` ~
            0x36 => 0x2B, // , <
            0x37 => 0x2F, // . >
            0x38 => 0x2C, // / ?
            0x39 => 0x39, // Caps Lock
            // Function keys F1–F12 (HID 0x3A..=0x45). Apple virtual keycodes (kVK_F1..).
            0x3A => 0x7A, // F1
            0x3B => 0x78, // F2
            0x3C => 0x63, // F3
            0x3D => 0x76, // F4
            0x3E => 0x60, // F5
            0x3F => 0x61, // F6
            0x40 => 0x62, // F7
            0x41 => 0x64, // F8
            0x42 => 0x65, // F9
            0x43 => 0x6D, // F10
            0x44 => 0x67, // F11
            0x45 => 0x6F, // F12
            // Navigation cluster (HID 0x49..=0x4E). Mac hardware has no Print Screen / Scroll Lock /
            // Pause (HID 0x46..=0x48) — those stay unmapped (fail-closed), never mis-injected.
            0x49 => 0x72, // Insert    → kVK_Help (Mac has no dedicated Insert)
            0x4A => 0x73, // Home      (kVK_Home)
            0x4B => 0x74, // Page Up   (kVK_PageUp)
            0x4C => 0x75, // Delete Fwd (kVK_ForwardDelete)
            0x4D => 0x77, // End       (kVK_End)
            0x4E => 0x79, // Page Down (kVK_PageDown)
            // Arrows (HID 0x4F..=0x52).
            0x4F => 0x7C, // Right
            0x50 => 0x7B, // Left
            0x51 => 0x7D, // Down
            0x52 => 0x7E, // Up
            // Keypad (HID 0x53..=0x63). Apple kVK_ANSI_Keypad* codes.
            0x53 => 0x47, // Num Lock → kVK_ANSI_KeypadClear
            0x54 => 0x4B, // KP /     (KeypadDivide)
            0x55 => 0x43, // KP *     (KeypadMultiply)
            0x56 => 0x4E, // KP -     (KeypadMinus)
            0x57 => 0x45, // KP +     (KeypadPlus)
            0x58 => 0x4C, // KP Enter (KeypadEnter)
            0x59 => 0x53, // KP 1
            0x5A => 0x54, // KP 2
            0x5B => 0x55, // KP 3
            0x5C => 0x56, // KP 4
            0x5D => 0x57, // KP 5
            0x5E => 0x58, // KP 6
            0x5F => 0x59, // KP 7
            0x60 => 0x5B, // KP 8
            0x61 => 0x5C, // KP 9
            0x62 => 0x52, // KP 0
            0x63 => 0x41, // KP .     (KeypadDecimal)
            // Modifiers (HID 0xE0..=0xE7).
            0xE0 => 0x3B, // Left Control
            0xE1 => 0x38, // Left Shift
            0xE2 => 0x3A, // Left Option/Alt
            0xE3 => 0x37, // Left Command/GUI
            0xE4 => 0x3E, // Right Control
            0xE5 => 0x3C, // Right Shift
            0xE6 => 0x3D, // Right Option/Alt
            0xE7 => 0x36, // Right Command/GUI
            _ => return None,
        };
        Some(vk)
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]
        use super::*;

        #[test]
        fn modifier_bits_map_to_flags() {
            assert!(flags_from_modifiers(0x01).contains(CGEventFlags::CGEventFlagShift));
            assert!(flags_from_modifiers(0x08).contains(CGEventFlags::CGEventFlagCommand));
            let all = flags_from_modifiers(0x0F);
            assert!(all.contains(CGEventFlags::CGEventFlagControl));
            assert!(all.contains(CGEventFlags::CGEventFlagAlternate));
            assert!(flags_from_modifiers(0).is_empty());
        }

        #[test]
        fn hid_maps_common_keys_and_rejects_unknown() {
            assert_eq!(hid_to_virtual_keycode(0x04), Some(0x00)); // a
            assert_eq!(hid_to_virtual_keycode(0x2C), Some(0x31)); // space
            assert_eq!(hid_to_virtual_keycode(0xE1), Some(0x38)); // left shift
            assert_eq!(hid_to_virtual_keycode(0xFFFF), None); // unmapped → fail-closed
        }

        #[test]
        fn function_navigation_and_keypad_keys_are_mapped() {
            assert_eq!(hid_to_virtual_keycode(0x3A), Some(0x7A)); // F1
            assert_eq!(hid_to_virtual_keycode(0x3E), Some(0x60)); // F5
            assert_eq!(hid_to_virtual_keycode(0x45), Some(0x6F)); // F12
            assert_eq!(hid_to_virtual_keycode(0x4C), Some(0x75)); // Delete Fwd (kVK_ForwardDelete)
            assert_eq!(hid_to_virtual_keycode(0x4A), Some(0x73)); // Home
            assert_eq!(hid_to_virtual_keycode(0x4D), Some(0x77)); // End
            assert_eq!(hid_to_virtual_keycode(0x4B), Some(0x74)); // Page Up
            assert_eq!(hid_to_virtual_keycode(0x4E), Some(0x79)); // Page Down
            assert_eq!(hid_to_virtual_keycode(0x62), Some(0x52)); // KP 0
            assert_eq!(hid_to_virtual_keycode(0x63), Some(0x41)); // KP .
                                                                  // Mac hardware lacks Print Screen / Scroll Lock / Pause → still fail-closed.
            assert_eq!(hid_to_virtual_keycode(0x46), None); // Print Screen
            assert_eq!(hid_to_virtual_keycode(0x48), None); // Pause
        }

        #[test]
        fn display_bounds_round_trip_maps_normalized_center() {
            let sink = CgEventSink::new();
            sink.set_display_bounds(7, 100.0, 200.0, 800.0, 600.0);
            let p = sink.to_point(7, 0.5, 0.5);
            assert!((p.x - 500.0).abs() < 0.001); // 100 + 0.5*800
            assert!((p.y - 500.0).abs() < 0.001); // 200 + 0.5*600
        }

        #[test]
        fn normalized_coords_are_clamped() {
            let sink = CgEventSink::new();
            sink.set_display_bounds(0, 0.0, 0.0, 100.0, 100.0);
            let p = sink.to_point(0, 2.0, -1.0); // out of range → clamped to [0,1]
            assert!((p.x - 100.0).abs() < 0.001);
            assert!((p.y - 0.0).abs() < 0.001);
        }

        #[test]
        fn non_finite_coords_map_to_the_display_origin() {
            let sink = CgEventSink::new();
            sink.set_display_bounds(0, 50.0, 60.0, 100.0, 100.0);
            // NaN/±∞ must never escape off-screen — they land on the display origin.
            let p = sink.to_point(0, f32::NAN, f32::INFINITY);
            assert!((p.x - 50.0).abs() < 0.001);
            assert!((p.y - 60.0).abs() < 0.001);
        }

        #[test]
        fn warp_guard_fires_os_only_on_the_transition_edge() {
            // RustDesk PR #10314 discipline: dissociate/hide fires once at the start of a control burst
            // (not on every event — that reintroduces the two-cursor artifact), and re-associate/show
            // fires once on exit. Repeated begins/ends inside the burst are no-ops.
            let mut g = false;
            assert!(latch_warp(&mut g), "first warp dissociates + hides");
            assert!(g);
            assert!(
                !latch_warp(&mut g),
                "subsequent warps do not re-fire the OS"
            );
            assert!(!latch_warp(&mut g));
            assert!(unlatch_warp(&mut g), "teardown re-associates + unhides");
            assert!(!g);
            assert!(!unlatch_warp(&mut g), "a second teardown is a no-op");
        }

        #[test]
        fn warp_guard_cleanup_is_a_noop_when_never_warped() {
            // `release_all` on a session that never injected a warp (e.g. keyboard-only, or an
            // emergency stop before any pointer event) must not touch the cursor at all (Inv 4: a stop
            // must never leave the cursor hidden — and here there is nothing to restore).
            let mut g = false;
            assert!(!unlatch_warp(&mut g), "no warp → no OS restore call");
            assert!(!g);
        }

        #[test]
        fn warp_latch_then_unlatch_is_always_balanced() {
            // Property: however many begins precede it, a single unlatch restores the cursor exactly
            // once and leaves the guard clear — so `release_all` (which calls `end_warp` once) always
            // brings a warped session back to the un-warped state (Inv 4), no leak, no double-restore.
            for begins in 0..5u8 {
                let mut g = false;
                let mut os_dissociate_calls = 0u8;
                for _ in 0..begins {
                    if latch_warp(&mut g) {
                        os_dissociate_calls += 1;
                    }
                }
                // At most one OS dissociate regardless of how many pointer events fired.
                assert!(os_dissociate_calls <= 1);
                let restored = unlatch_warp(&mut g);
                assert_eq!(restored, begins > 0, "restore fires iff we had dissociated");
                assert!(!g, "guard always ends clear after teardown");
            }
        }

        #[test]
        fn desktop_bounds_is_the_union_of_registered_displays() {
            // The relative-motion clamp uses the whole-desktop box (ADR-087). With a HiDPI secondary at
            // a negative origin, the union spans both displays so relative moves reach either screen.
            let sink = CgEventSink::new();
            sink.set_display_bounds(0, 0.0, 0.0, 1920.0, 1080.0); // primary at origin
            sink.set_display_bounds(1, -1280.0, 0.0, 1280.0, 720.0); // to the left, negative origin
            let (x, y, w, h) = sink.desktop_bounds();
            assert!((x - (-1280.0)).abs() < 0.001, "min x is the left display");
            assert!((y - 0.0).abs() < 0.001);
            assert!((w - (1920.0 + 1280.0)).abs() < 0.001, "spans both displays");
            assert!((h - 1080.0).abs() < 0.001, "tallest display height");
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::CgEventSink;
