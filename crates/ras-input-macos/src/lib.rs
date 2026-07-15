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
        ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use ras_control::{InputError, OsInputSink};
    use ras_protocol::{ErrorCode, PointerButton, RasError};

    // PostEvent TCC (macOS 10.15+): preflight WITHOUT prompting, and request (prompts once). Declared
    // here because `core-graphics` does not wrap them; they live in the CoreGraphics framework the
    // crate already links. Both return a C `bool`.
    extern "C" {
        fn CGPreflightPostEventAccess() -> bool;
        fn CGRequestPostEventAccess() -> bool;
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
            let x = ox + f64::from(nx.clamp(0.0, 1.0)) * w;
            let y = oy + f64::from(ny.clamp(0.0, 1.0)) * h;
            *self.last_point.lock().unwrap_or_else(|e| e.into_inner()) = (x, y);
            CGPoint::new(x, y)
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

        fn pointer_button(
            &self,
            display: u32,
            nx: f32,
            ny: f32,
            button: PointerButton,
            down: bool,
        ) -> Result<(), InputError> {
            let point = self.to_point(display, nx, ny);
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
            // Release every key we believe is down …
            let keys: Vec<CGKeyCode> = {
                let mut pressed = self.pressed_keys.lock().unwrap_or_else(|e| e.into_inner());
                pressed.drain().collect()
            };
            for keycode in keys {
                if let Ok(event) = CGEvent::new_keyboard_event(source()?, keycode, false) {
                    event.post(CGEventTapLocation::HID);
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
                if let Ok(event) = CGEvent::new_mouse_event(source()?, event_type, point, cg_button)
                {
                    event.post(CGEventTapLocation::HID);
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
            // Arrows (HID 0x4F..=0x52).
            0x4F => 0x7C, // Right
            0x50 => 0x7B, // Left
            0x51 => 0x7D, // Down
            0x52 => 0x7E, // Up
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
    }
}

#[cfg(target_os = "macos")]
pub use macos::CgEventSink;
