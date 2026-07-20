//! Platform-independent pure logic shared by the `uinput` backend: the HID-usage → Linux-evdev keycode
//! map and the normalized-fraction → absolute-axis scaling. Kept **out of the `#[cfg(target_os = "linux")]`
//! gate** so its correctness (the load-bearing keycode table and the Inv-6 coordinate clamp) is
//! unit-tested on the macOS/Windows dev host too, not only when the crate is built for a Linux target.
//! No dependency on any `input-linux` type — only `u16`/`f32`/`i32`.
//!
//! These items are consumed only by the `#[cfg(target_os = "linux")]` `uinput` module, so on a non-Linux
//! host they appear unused — the module-level `dead_code` allow is deliberate (they are still compiled +
//! tested everywhere, which is the whole point of keeping them out of the platform gate).
#![allow(dead_code)]

/// The virtual absolute-axis range the uinput backend advertises. A normalized `0.0..=1.0` fraction
/// scales linearly onto `0..=ABS_MAX`; the compositor maps that onto the shared output. The 16-bit range
/// matches our wire's fixed-point pointer coordinates (`0..=65535`), so there is no precision loss.
pub(crate) const ABS_MAX: i32 = 65535;

/// Scale a normalized `0.0..=1.0` fraction onto the device abs range `0..=ABS_MAX`. Non-finite maps to
/// `0` (defense-in-depth, Inv 6); out-of-range clamps.
pub(crate) fn norm_to_abs(v: f32) -> i32 {
    let f = if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    };
    // `as i32` on a value already in `[0, ABS_MAX]` cannot overflow; the clamp guarantees the range.
    (f64::from(f) * f64::from(ABS_MAX)).round() as i32
}

/// Map a USB-HID Keyboard/Keypad usage (page 0x07) to a **Linux evdev keycode** (`KEY_A == 30`). This is
/// the XTEST table (which emits X keycode = evdev + 8) **minus 8** per entry — uinput speaks raw evdev.
/// An unmapped usage returns `None` and fails closed at the call site (never a wrong key — Inv 6).
#[allow(clippy::too_many_lines)]
pub(crate) fn hid_to_keycode(hid: u16) -> Option<u16> {
    let kc: u16 = match hid {
        // Letters a–z (HID 0x04..=0x1D) → evdev.
        0x04 => 30, // a
        0x05 => 48, // b
        0x06 => 46, // c
        0x07 => 32, // d
        0x08 => 18, // e
        0x09 => 33, // f
        0x0A => 34, // g
        0x0B => 35, // h
        0x0C => 23, // i
        0x0D => 36, // j
        0x0E => 37, // k
        0x0F => 38, // l
        0x10 => 50, // m
        0x11 => 49, // n
        0x12 => 24, // o
        0x13 => 25, // p
        0x14 => 16, // q
        0x15 => 19, // r
        0x16 => 31, // s
        0x17 => 20, // t
        0x18 => 22, // u
        0x19 => 47, // v
        0x1A => 17, // w
        0x1B => 45, // x
        0x1C => 21, // y
        0x1D => 44, // z
        // Digits 1–9,0 (HID 0x1E..=0x27).
        0x1E => 2,  // 1
        0x1F => 3,  // 2
        0x20 => 4,  // 3
        0x21 => 5,  // 4
        0x22 => 6,  // 5
        0x23 => 7,  // 6
        0x24 => 8,  // 7
        0x25 => 9,  // 8
        0x26 => 10, // 9
        0x27 => 11, // 0
        // Whitespace / editing.
        0x28 => 28, // Return/Enter
        0x29 => 1,  // Escape
        0x2A => 14, // Backspace
        0x2B => 15, // Tab
        0x2C => 57, // Space
        0x2D => 12, // - _
        0x2E => 13, // = +
        0x2F => 26, // [ {
        0x30 => 27, // ] }
        0x31 => 43, // \ |
        0x33 => 39, // ; :
        0x34 => 40, // ' "
        0x35 => 41, // ` ~
        0x36 => 51, // , <
        0x37 => 52, // . >
        0x38 => 53, // / ?
        0x39 => 58, // Caps Lock
        // Function keys F1–F12 (HID 0x3A..=0x45).
        0x3A => 59, // F1
        0x3B => 60, // F2
        0x3C => 61, // F3
        0x3D => 62, // F4
        0x3E => 63, // F5
        0x3F => 64, // F6
        0x40 => 65, // F7
        0x41 => 66, // F8
        0x42 => 67, // F9
        0x43 => 68, // F10
        0x44 => 87, // F11
        0x45 => 88, // F12
        // System keys + the 6-key navigation cluster (HID 0x46..=0x4E).
        0x46 => 99,  // Print Screen (KEY_SYSRQ)
        0x47 => 70,  // Scroll Lock
        0x48 => 119, // Pause
        0x49 => 110, // Insert
        0x4A => 102, // Home
        0x4B => 104, // Page Up
        0x4C => 111, // Delete Forward
        0x4D => 107, // End
        0x4E => 109, // Page Down
        // Arrows (HID 0x4F..=0x52).
        0x4F => 106, // Right
        0x50 => 105, // Left
        0x51 => 108, // Down
        0x52 => 103, // Up
        // Keypad (HID 0x53..=0x63).
        0x53 => 69,  // Num Lock
        0x54 => 98,  // KP /
        0x55 => 55,  // KP *
        0x56 => 74,  // KP -
        0x57 => 78,  // KP +
        0x58 => 96,  // KP Enter
        0x59 => 79,  // KP 1
        0x5A => 80,  // KP 2
        0x5B => 81,  // KP 3
        0x5C => 75,  // KP 4
        0x5D => 76,  // KP 5
        0x5E => 77,  // KP 6
        0x5F => 71,  // KP 7
        0x60 => 72,  // KP 8
        0x61 => 73,  // KP 9
        0x62 => 82,  // KP 0
        0x63 => 83,  // KP .
        0x64 => 86,  // Non-US \ | (KEY_102ND)
        0x65 => 127, // Application/Menu (KEY_COMPOSE)
        // Modifiers (HID 0xE0..=0xE7).
        0xE0 => 29,  // Left Control
        0xE1 => 42,  // Left Shift
        0xE2 => 56,  // Left Alt
        0xE3 => 125, // Left Meta/GUI
        0xE4 => 97,  // Right Control
        0xE5 => 54,  // Right Shift
        0xE6 => 100, // Right Alt
        0xE7 => 126, // Right Meta/GUI
        _ => return None,
    };
    Some(kc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_maps_common_keys_and_rejects_unknown() {
        assert_eq!(hid_to_keycode(0x04), Some(30)); // a (X11 was 38 = evdev+8)
        assert_eq!(hid_to_keycode(0x2C), Some(57)); // space (X11 was 65)
        assert_eq!(hid_to_keycode(0xE1), Some(42)); // left shift (X11 was 50)
        assert_eq!(hid_to_keycode(0xFFFF), None); // unmapped → fail-closed
    }

    #[test]
    fn hid_map_is_exactly_the_x11_table_minus_eight() {
        // The uinput evdev codes must be the X11 (evdev+8) codes minus 8 — the load-bearing relationship
        // (a wrong offset types the neighbouring key). Spot-check across every block.
        let cases: [(u16, u16); 10] = [
            (0x04, 38),  // a
            (0x1D, 52),  // z
            (0x1E, 10),  // 1
            (0x28, 36),  // Enter
            (0x3A, 67),  // F1
            (0x45, 96),  // F12
            (0x4A, 110), // Home
            (0x52, 111), // Up
            (0x62, 90),  // KP 0
            (0xE3, 133), // Left Meta
        ];
        for (hid, x11_kc) in cases {
            assert_eq!(
                hid_to_keycode(hid),
                Some(x11_kc - 8),
                "hid {hid:#x} should be X11 {x11_kc} minus 8"
            );
        }
    }

    #[test]
    fn function_navigation_and_keypad_keys_are_mapped() {
        assert_eq!(hid_to_keycode(0x3A), Some(59)); // F1
        assert_eq!(hid_to_keycode(0x3E), Some(63)); // F5
        assert_eq!(hid_to_keycode(0x45), Some(88)); // F12
        assert_eq!(hid_to_keycode(0x49), Some(110)); // Insert
        assert_eq!(hid_to_keycode(0x4A), Some(102)); // Home
        assert_eq!(hid_to_keycode(0x4C), Some(111)); // Delete Forward
        assert_eq!(hid_to_keycode(0x4D), Some(107)); // End
        assert_eq!(hid_to_keycode(0x4B), Some(104)); // Page Up
        assert_eq!(hid_to_keycode(0x4E), Some(109)); // Page Down
        assert_eq!(hid_to_keycode(0x53), Some(69)); // Num Lock
        assert_eq!(hid_to_keycode(0x62), Some(82)); // KP 0
        assert_eq!(hid_to_keycode(0x63), Some(83)); // KP .
    }

    #[test]
    fn normalized_scales_to_abs_range() {
        assert_eq!(norm_to_abs(0.0), 0);
        assert_eq!(norm_to_abs(1.0), ABS_MAX);
        assert_eq!(norm_to_abs(0.5), (ABS_MAX + 1) / 2); // 32768
    }

    #[test]
    fn normalized_is_clamped_and_nonfinite_maps_to_origin() {
        assert_eq!(norm_to_abs(2.0), ABS_MAX); // over-range clamps to max
        assert_eq!(norm_to_abs(-1.0), 0); // under-range clamps to 0
        assert_eq!(norm_to_abs(f32::NAN), 0); // non-finite → origin (Inv 6)
        assert_eq!(norm_to_abs(f32::INFINITY), 0);
    }
}
