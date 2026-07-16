//! Linux OS-input backend (ADR-070, implements ADR-054/055): a [`ras_control::OsInputSink`] over the
//! X11 **XTEST** extension via the pure-Rust [`x11rb`] client.
//!
//! Unlike `ras-input-macos` (CGEvent FFI), `x11rb` is a pure-Rust X11 protocol client — **no C
//! bindings, no `unsafe`** — so this crate keeps the workspace default `unsafe_code = "deny"`. The
//! crate is **empty on non-Linux** so macOS/Windows CI stays green.
//!
//! # Deliberately unprivileged (Inv 14, ADR-055)
//! It connects to `$DISPLAY` as the logged-in user — no root, no `/dev/uinput`. Consequence, surfaced
//! honestly: it works only inside an **X11 / Xwayland** session; on a pure-Wayland compositor XTEST
//! reaches only Xwayland clients, not the Wayland desktop, and never a locked greeter. Fail-closed: no
//! reachable X server ⇒ [`OsInputSink::input_permitted`] is `false` and the host refuses the lease
//! (never a silent no-op) — the same contract as the macOS PostEvent preflight.
//!
//! # Coordinates (Inv 6)
//! The trait receives only **normalized** `0.0..=1.0` fractions of a display; this backend maps them
//! to global root-window pixels using bounds fed from the host's capture geometry. The controller
//! never sends pixels.
//!
//! **Runtime status:** cross-compile-*checks* for `x86_64-unknown-linux-gnu`; the live XTEST injection
//! is an on-device step on a Linux X11/Xwayland session (the Linux analogue of the macOS on-device
//! row, `docs/19 §7`). `uinput` + libei backends are additive follow-ups behind the same trait.

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use ras_control::{InputError, OsInputSink};
    use ras_protocol::{ErrorCode, PointerButton, RasError};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::xproto::{
        Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
        MOTION_NOTIFY_EVENT,
    };
    use x11rb::protocol::xtest::ConnectionExt as _;
    use x11rb::rust_connection::RustConnection;

    // X keycodes for the modifier bits (Linux evdev keycode + 8). Left-hand variants are used to
    // realize a requested modifier bitset (the X11 wire has no per-event modifier flag).
    const SHIFT_KC: u8 = 50; // KEY_LEFTSHIFT (42) + 8
    const CTRL_KC: u8 = 37; // KEY_LEFTCTRL  (29) + 8
    const ALT_KC: u8 = 64; // KEY_LEFTALT   (56) + 8
    const META_KC: u8 = 133; // KEY_LEFTMETA (125) + 8

    /// (bit, X keycode) for each modifier, in a stable order.
    const MODS: [(u8, u8); 4] = [
        (0x01, SHIFT_KC),
        (0x02, CTRL_KC),
        (0x04, ALT_KC),
        (0x08, META_KC),
    ];

    // X11 pointer button numbers.
    const BTN_LEFT: u8 = 1;
    const BTN_MIDDLE: u8 = 2;
    const BTN_RIGHT: u8 = 3;
    const BTN_WHEEL_UP: u8 = 4;
    const BTN_WHEEL_DOWN: u8 = 5;
    const BTN_WHEEL_LEFT: u8 = 6;
    const BTN_WHEEL_RIGHT: u8 = 7;
    /// Cap wheel notches per event so a hostile delta can't spin an unbounded click loop.
    const MAX_WHEEL_NOTCHES: i32 = 64;

    // Lock keys, X keycodes (evdev + 8). Caps = KEY_CAPSLOCK(58)+8; Num = KEY_NUMLOCK(69)+8.
    const CAPS_KC: u8 = 66;
    const NUM_KC: u8 = 77;
    // KeyButMask bits in a QueryPointer reply: CapsLock = Lock (0x02), NumLock = Mod2 (0x10).
    const MASK_CAPS: u16 = 0x02;
    const MASK_NUM: u16 = 0x10;

    /// Display bounds in global root-window pixels, for normalized→pixel mapping.
    #[derive(Debug, Clone, Copy)]
    struct DisplayBounds {
        id: u32,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    /// Mutable tracking state (behind a `Mutex` so the sink is `Send + Sync`).
    #[derive(Debug, Default)]
    struct State {
        pressed_keys: HashSet<u8>,
        pressed_buttons: HashSet<u8>,
        held_mods: u8,
        last_point: (i16, i16),
        displays: Vec<DisplayBounds>,
    }

    /// An XTEST-backed [`OsInputSink`]. Holds one X11 connection; `None` means no reachable X server
    /// (fail-closed — the host refuses the lease).
    #[derive(Debug)]
    pub struct X11InputSink {
        conn: Option<RustConnection>,
        root: Window,
        screen_w: u16,
        screen_h: u16,
        state: Mutex<State>,
    }

    impl Default for X11InputSink {
        fn default() -> Self {
            Self::new()
        }
    }

    impl X11InputSink {
        /// Connect to `$DISPLAY`. Never panics: an unreachable X server yields a sink whose
        /// [`OsInputSink::input_permitted`] is `false`.
        #[must_use]
        pub fn new() -> Self {
            match RustConnection::connect(None) {
                Ok((conn, screen_num)) => {
                    let screen = &conn.setup().roots[screen_num];
                    let (root, w, h) =
                        (screen.root, screen.width_in_pixels, screen.height_in_pixels);
                    Self {
                        conn: Some(conn),
                        root,
                        screen_w: w,
                        screen_h: h,
                        state: Mutex::new(State::default()),
                    }
                }
                Err(_) => Self {
                    conn: None,
                    root: 0,
                    screen_w: 0,
                    screen_h: 0,
                    state: Mutex::new(State::default()),
                },
            }
        }

        /// Register a display's global bounds (pixels), from the host's capture geometry. Replaces any
        /// prior bounds for the same display id.
        pub fn set_display_bounds(&self, id: u32, x: f64, y: f64, w: f64, h: f64) {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.displays.retain(|b| b.id != id);
            st.displays.push(DisplayBounds { id, x, y, w, h });
        }

        /// Map a normalized `(nx, ny)` on `display` to a global root-window point, updating `last_point`.
        /// Falls back to the connection's default screen if the id is unknown (input still lands on the
        /// primary screen). Non-finite fractions map to the display origin (defense-in-depth, Inv 6).
        fn to_point(&self, display: u32, nx: f32, ny: f32, st: &mut State) -> (i16, i16) {
            let (ox, oy, w, h) = st.displays.iter().find(|b| b.id == display).map_or(
                (0.0, 0.0, f64::from(self.screen_w), f64::from(self.screen_h)),
                |b| (b.x, b.y, b.w, b.h),
            );
            let frac = |v: f32| {
                if v.is_finite() {
                    v.clamp(0.0, 1.0)
                } else {
                    0.0
                }
            };
            // f64→i16 `as` saturates (Rust ≥1.45), so out-of-range coordinates clamp rather than wrap.
            let xi = (ox + f64::from(frac(nx)) * w).round() as i16;
            let yi = (oy + f64::from(frac(ny)) * h).round() as i16;
            st.last_point = (xi, yi);
            (xi, yi)
        }

        /// Send one XTEST event and flush. Fire-and-forget (unchecked) to keep latency low.
        fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<(), InputError> {
            let conn = self.conn.as_ref().ok_or_else(|| {
                RasError::recoverable(ErrorCode::InputFailed, "no X11 connection")
            })?;
            conn.xtest_fake_input(type_, detail, 0, self.root, x, y, 0)
                .map_err(|_| {
                    RasError::recoverable(ErrorCode::InputFailed, "XTEST fake_input failed")
                })?;
            conn.flush()
                .map_err(|_| RasError::recoverable(ErrorCode::InputFailed, "X11 flush failed"))?;
            Ok(())
        }

        /// Realize the requested modifier bitset by faking press/release of the modifier keycodes so the
        /// held set matches `want` (the X11 wire has no per-event modifier flag).
        fn reconcile_mods(&self, want: u8, st: &mut State) -> Result<(), InputError> {
            let (x, y) = st.last_point;
            for (bit, kc) in MODS {
                let want_on = want & bit != 0;
                let is_on = st.held_mods & bit != 0;
                if want_on && !is_on {
                    self.fake(KEY_PRESS_EVENT, kc, x, y)?;
                    st.held_mods |= bit;
                } else if !want_on && is_on {
                    self.fake(KEY_RELEASE_EVENT, kc, x, y)?;
                    st.held_mods &= !bit;
                }
            }
            Ok(())
        }

        fn click(&self, button: u8, x: i16, y: i16) -> Result<(), InputError> {
            self.fake(BUTTON_PRESS_EVENT, button, x, y)?;
            self.fake(BUTTON_RELEASE_EVENT, button, x, y)
        }
    }

    impl OsInputSink for X11InputSink {
        fn pointer_move(&self, display: u32, nx: f32, ny: f32) -> Result<(), InputError> {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (x, y) = self.to_point(display, nx, ny, &mut st);
            self.fake(MOTION_NOTIFY_EVENT, 0, x, y)
        }

        fn pointer_button(
            &self,
            display: u32,
            nx: f32,
            ny: f32,
            button: PointerButton,
            down: bool,
        ) -> Result<(), InputError> {
            let btn = match button {
                PointerButton::Left => BTN_LEFT,
                PointerButton::Right => BTN_RIGHT,
                PointerButton::Middle => BTN_MIDDLE,
                // Fail-closed for an unrecognized future button variant.
                _ => {
                    return Err(RasError::fatal(
                        ErrorCode::InputFailed,
                        "unknown pointer button",
                    ))
                }
            };
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (x, y) = self.to_point(display, nx, ny, &mut st);
            let type_ = if down {
                BUTTON_PRESS_EVENT
            } else {
                BUTTON_RELEASE_EVENT
            };
            self.fake(type_, btn, x, y)?;
            if down {
                st.pressed_buttons.insert(btn);
            } else {
                st.pressed_buttons.remove(&btn);
            }
            Ok(())
        }

        fn pointer_wheel(&self, dx: i16, dy: i16) -> Result<(), InputError> {
            let (x, y) = {
                let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.last_point
            };
            // Vertical: X convention is button 4 = up, 5 = down; our dy is down-positive.
            let vsteps = i32::from(dy).abs().min(MAX_WHEEL_NOTCHES);
            let vbtn = if dy > 0 { BTN_WHEEL_DOWN } else { BTN_WHEEL_UP };
            for _ in 0..vsteps {
                self.click(vbtn, x, y)?;
            }
            // Horizontal: button 6 = left, 7 = right; our dx is right-positive.
            let hsteps = i32::from(dx).abs().min(MAX_WHEEL_NOTCHES);
            let hbtn = if dx > 0 {
                BTN_WHEEL_RIGHT
            } else {
                BTN_WHEEL_LEFT
            };
            for _ in 0..hsteps {
                self.click(hbtn, x, y)?;
            }
            Ok(())
        }

        fn key(&self, hid_usage: u16, down: bool, modifiers: u8) -> Result<(), InputError> {
            let kc = hid_to_keycode(hid_usage)
                .ok_or_else(|| RasError::fatal(ErrorCode::InputFailed, "unmapped physical key"))?;
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.reconcile_mods(modifiers, &mut st)?;
            let (x, y) = st.last_point;
            let type_ = if down {
                KEY_PRESS_EVENT
            } else {
                KEY_RELEASE_EVENT
            };
            self.fake(type_, kc, x, y)?;
            if down {
                st.pressed_keys.insert(kc);
            } else {
                st.pressed_keys.remove(&kc);
            }
            Ok(())
        }

        fn text(&self, _utf8: &str) -> Result<(), InputError> {
            // Layout-independent Unicode text over XTEST needs server keymap remapping; not supported
            // in v1. `keyboard.text` is withheld by `phase3_default_policy`, so this is never reached
            // on the default path — fail closed rather than mis-type.
            Err(RasError::fatal(
                ErrorCode::InputFailed,
                "text input not supported on X11",
            ))
        }

        fn release_all(&self) -> Result<(), InputError> {
            // Best-effort key-state cleanup on the emergency-stop / teardown path (Inv 4): release as
            // much as possible and never abort early — a per-event failure skips that one release.
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (x, y) = st.last_point;
            for kc in st.pressed_keys.drain().collect::<Vec<_>>() {
                let _ = self.fake(KEY_RELEASE_EVENT, kc, x, y);
            }
            for btn in st.pressed_buttons.drain().collect::<Vec<_>>() {
                let _ = self.fake(BUTTON_RELEASE_EVENT, btn, x, y);
            }
            for (bit, kc) in MODS {
                if st.held_mods & bit != 0 {
                    let _ = self.fake(KEY_RELEASE_EVENT, kc, x, y);
                }
            }
            st.held_mods = 0;
            Ok(())
        }

        fn set_lock_state(&self, caps_lock: bool, num_lock: bool) -> Result<(), InputError> {
            let conn = self.conn.as_ref().ok_or_else(|| {
                RasError::recoverable(ErrorCode::InputFailed, "no X11 connection")
            })?;
            // Read the live lock state from the pointer's modifier mask, then tap the lock key only on
            // a mismatch — idempotent, never blindly toggles.
            let mask = conn
                .query_pointer(self.root)
                .map_err(|_| RasError::recoverable(ErrorCode::InputFailed, "QueryPointer failed"))?
                .reply()
                .map_err(|_| RasError::recoverable(ErrorCode::InputFailed, "QueryPointer reply"))?
                .mask;
            let m = u16::from(mask);
            let (cur_caps, cur_num) = (m & MASK_CAPS != 0, m & MASK_NUM != 0);
            let (x, y) = {
                let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.last_point
            };
            if cur_caps != caps_lock {
                self.fake(KEY_PRESS_EVENT, CAPS_KC, x, y)?;
                self.fake(KEY_RELEASE_EVENT, CAPS_KC, x, y)?;
            }
            if cur_num != num_lock {
                self.fake(KEY_PRESS_EVENT, NUM_KC, x, y)?;
                self.fake(KEY_RELEASE_EVENT, NUM_KC, x, y)?;
            }
            Ok(())
        }

        fn input_permitted(&self) -> bool {
            // Fail-closed: no reachable X server ⇒ the host refuses the lease.
            self.conn.is_some()
        }
    }

    /// Map a USB-HID Keyboard/Keypad usage (page 0x07) to an **X keycode** (Linux evdev keycode + 8).
    /// Covers the common alphanumeric, whitespace, arrow, and modifier keys; an unmapped usage fails
    /// closed at the call site (never a keysym — Inv 6). The full table is completed on-device.
    #[allow(clippy::too_many_lines)]
    fn hid_to_keycode(hid: u16) -> Option<u8> {
        let kc: u8 = match hid {
            // Letters a–z (HID 0x04..=0x1D) → evdev + 8.
            0x04 => 38, // a
            0x05 => 56, // b
            0x06 => 54, // c
            0x07 => 40, // d
            0x08 => 26, // e
            0x09 => 41, // f
            0x0A => 42, // g
            0x0B => 43, // h
            0x0C => 31, // i
            0x0D => 44, // j
            0x0E => 45, // k
            0x0F => 46, // l
            0x10 => 58, // m
            0x11 => 57, // n
            0x12 => 32, // o
            0x13 => 33, // p
            0x14 => 24, // q
            0x15 => 27, // r
            0x16 => 39, // s
            0x17 => 28, // t
            0x18 => 30, // u
            0x19 => 55, // v
            0x1A => 25, // w
            0x1B => 53, // x
            0x1C => 29, // y
            0x1D => 52, // z
            // Digits 1–9,0 (HID 0x1E..=0x27).
            0x1E => 10, // 1
            0x1F => 11, // 2
            0x20 => 12, // 3
            0x21 => 13, // 4
            0x22 => 14, // 5
            0x23 => 15, // 6
            0x24 => 16, // 7
            0x25 => 17, // 8
            0x26 => 18, // 9
            0x27 => 19, // 0
            // Whitespace / editing.
            0x28 => 36, // Return/Enter
            0x29 => 9,  // Escape
            0x2A => 22, // Backspace
            0x2B => 23, // Tab
            0x2C => 65, // Space
            0x2D => 20, // - _
            0x2E => 21, // = +
            0x2F => 34, // [ {
            0x30 => 35, // ] }
            0x31 => 51, // \ |
            0x33 => 47, // ; :
            0x34 => 48, // ' "
            0x35 => 49, // ` ~
            0x36 => 59, // , <
            0x37 => 60, // . >
            0x38 => 61, // / ?
            0x39 => 66, // Caps Lock
            // Arrows (HID 0x4F..=0x52).
            0x4F => 114, // Right
            0x50 => 113, // Left
            0x51 => 116, // Down
            0x52 => 111, // Up
            // Modifiers (HID 0xE0..=0xE7).
            0xE0 => 37,  // Left Control
            0xE1 => 50,  // Left Shift
            0xE2 => 64,  // Left Option/Alt
            0xE3 => 133, // Left Command/Meta
            0xE4 => 105, // Right Control
            0xE5 => 62,  // Right Shift
            0xE6 => 108, // Right Option/Alt
            0xE7 => 134, // Right Command/Meta
            _ => return None,
        };
        Some(kc)
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]
        use super::*;

        #[test]
        fn hid_maps_common_keys_and_rejects_unknown() {
            assert_eq!(hid_to_keycode(0x04), Some(38)); // a
            assert_eq!(hid_to_keycode(0x2C), Some(65)); // space
            assert_eq!(hid_to_keycode(0xE1), Some(50)); // left shift
            assert_eq!(hid_to_keycode(0xFFFF), None); // unmapped → fail-closed
        }

        #[test]
        fn modifier_keycodes_match_the_left_hand_hid_entries() {
            assert_eq!(hid_to_keycode(0xE1), Some(SHIFT_KC));
            assert_eq!(hid_to_keycode(0xE0), Some(CTRL_KC));
            assert_eq!(hid_to_keycode(0xE2), Some(ALT_KC));
            assert_eq!(hid_to_keycode(0xE3), Some(META_KC));
        }

        #[test]
        fn display_bounds_map_normalized_center() {
            let sink = X11InputSink::new();
            sink.set_display_bounds(7, 100.0, 200.0, 800.0, 600.0);
            let mut st = sink.state.lock().unwrap();
            let (x, y) = sink.to_point(7, 0.5, 0.5, &mut st);
            assert_eq!(x, 500); // 100 + 0.5*800
            assert_eq!(y, 500); // 200 + 0.5*600
            assert_eq!(st.last_point, (500, 500));
        }

        #[test]
        fn normalized_coords_are_clamped() {
            let sink = X11InputSink::new();
            sink.set_display_bounds(0, 0.0, 0.0, 100.0, 100.0);
            let mut st = sink.state.lock().unwrap();
            let (x, y) = sink.to_point(0, 2.0, -1.0, &mut st); // out of range → clamp to [0,1]
            assert_eq!(x, 100);
            assert_eq!(y, 0);
        }

        #[test]
        fn non_finite_coords_map_to_the_display_origin() {
            let sink = X11InputSink::new();
            sink.set_display_bounds(0, 50.0, 60.0, 100.0, 100.0);
            let mut st = sink.state.lock().unwrap();
            let (x, y) = sink.to_point(0, f32::NAN, f32::INFINITY, &mut st);
            assert_eq!(x, 50);
            assert_eq!(y, 60);
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::X11InputSink;
