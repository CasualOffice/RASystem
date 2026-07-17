//! Windows OS-input backend (ADR-071, implements ADR-054): a [`ras_control::OsInputSink`] over
//! `SendInput` via the Microsoft `windows` (windows-rs) bindings.
//!
//! `unsafe`/FFI is confined here (CONTRIBUTING §5, as in `ras-input-macos`); `ras-control` stays
//! `unsafe`-free. The crate is **empty on non-Windows** so macOS/Linux CI stays green.
//!
//! # Deliberately unprivileged, no UIAccess (Inv 14, docs/19 §4)
//! It runs in the interactive user session with **no `uiAccess` manifest**, so it cannot drive
//! elevated windows or the secure desktop (UAC / lock / login) — by design, never bypassed. Emergency
//! stop stays the always-visible Stop button (+ the kernel SAS, which no user-mode injector can
//! synthesize). Microsoft's Jan-2026 credential-UI hardening enforces the same limit at the OS level.
//!
//! # Coordinates (Inv 6)
//! The trait receives only **normalized** `0.0..=1.0` fractions of a display; this backend maps them
//! to **absolute virtual-desktop** coordinates (`0..=65535`, `MOUSEEVENTF_VIRTUALDESK`) using display
//! bounds fed from the host's capture geometry. The controller never sends pixels.
//!
//! **Runtime status:** cross-compile-*checks* for `x86_64-pc-windows-msvc`; the live `SendInput` run
//! needs Windows hardware the team does not yet have, so Windows stays CI-compile-gated until a
//! device/runner exists (docs/19 §4).

#[cfg(target_os = "windows")]
mod win {
    use std::collections::HashSet;
    use std::mem::size_of;
    use std::sync::Mutex;

    use ras_control::{InputError, OsInputSink};
    use ras_protocol::{ErrorCode, PointerButton, RasError};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
        MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
        MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    /// One wheel notch (`WHEEL_DELTA`).
    const WHEEL_DELTA: i32 = 120;
    /// Cap wheel notches per event so a hostile delta can't spin an unbounded scroll.
    const MAX_WHEEL_NOTCHES: i32 = 64;

    /// (bit, generic modifier virtual-key) — VK_SHIFT/CONTROL/MENU/LWIN — to realize a modifier bitset
    /// (Windows has no per-event modifier flag; modifiers are separate key events).
    const MOD_VKS: [(u8, u16); 4] = [(0x01, 0x10), (0x02, 0x11), (0x04, 0x12), (0x08, 0x5B)];

    /// Display bounds in global desktop pixels, for normalized→absolute mapping.
    #[derive(Debug, Clone, Copy)]
    struct DisplayBounds {
        id: u32,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    #[derive(Debug, Default)]
    struct State {
        pressed_keys: HashSet<u16>,
        pressed_buttons: HashSet<u8>,
        held_mods: u8,
        displays: Vec<DisplayBounds>,
    }

    /// A `SendInput`-backed [`OsInputSink`]. Reads the virtual-screen metrics once at construction.
    #[derive(Debug)]
    pub struct SendInputSink {
        /// (x, y, width, height) of the virtual desktop in pixels.
        virt: (i32, i32, i32, i32),
        state: Mutex<State>,
    }

    impl Default for SendInputSink {
        fn default() -> Self {
            Self::new()
        }
    }

    impl SendInputSink {
        #[must_use]
        pub fn new() -> Self {
            // SAFETY: `GetSystemMetrics` is a pure query with no arguments beyond a metric index.
            let virt = unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                )
            };
            Self {
                virt,
                state: Mutex::new(State::default()),
            }
        }

        /// Register a display's global bounds (pixels), from the host's capture geometry.
        pub fn set_display_bounds(&self, id: u32, x: f64, y: f64, w: f64, h: f64) {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.displays.retain(|b| b.id != id);
            st.displays.push(DisplayBounds { id, x, y, w, h });
        }

        /// Map a normalized `(nx, ny)` on `display` to absolute virtual-desktop coords (`0..=65535`).
        /// Non-finite fractions map to the display origin (defense-in-depth, Inv 6).
        fn to_abs(&self, display: u32, nx: f32, ny: f32, st: &State) -> (i32, i32) {
            let (vx, vy, vw, vh) = self.virt;
            let (ox, oy, w, h) = st.displays.iter().find(|b| b.id == display).map_or(
                (f64::from(vx), f64::from(vy), f64::from(vw), f64::from(vh)),
                |b| (b.x, b.y, b.w, b.h),
            );
            let frac = |v: f32| {
                if v.is_finite() {
                    v.clamp(0.0, 1.0)
                } else {
                    0.0
                }
            };
            let px = ox + f64::from(frac(nx)) * w;
            let py = oy + f64::from(frac(ny)) * h;
            (to_abs_axis(px, vx, vw), to_abs_axis(py, vy, vh))
        }

        fn send(&self, inputs: &[INPUT]) -> Result<(), InputError> {
            // SAFETY: `inputs` is a valid slice and `cbsize` is the exact element size.
            let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
            if sent as usize == inputs.len() {
                Ok(())
            } else {
                // A short count means the OS blocked injection (e.g. UIPI against a higher-integrity
                // window). Surface it — never a silent success.
                Err(RasError::recoverable(
                    ErrorCode::InputFailed,
                    "SendInput was blocked",
                ))
            }
        }

        fn reconcile_mods(&self, want: u8, st: &mut State) -> Result<(), InputError> {
            for (bit, vk) in MOD_VKS {
                let want_on = want & bit != 0;
                let is_on = st.held_mods & bit != 0;
                if want_on && !is_on {
                    self.send(&[key_input(vk, 0, KEYBD_EVENT_FLAGS(0))])?;
                    st.held_mods |= bit;
                } else if !want_on && is_on {
                    self.send(&[key_input(vk, 0, KEYEVENTF_KEYUP)])?;
                    st.held_mods &= !bit;
                }
            }
            Ok(())
        }
    }

    impl OsInputSink for SendInputSink {
        fn pointer_move(&self, display: u32, nx: f32, ny: f32) -> Result<(), InputError> {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (ax, ay) = self.to_abs(display, nx, ny, &st);
            drop(st);
            self.send(&[mouse_input(
                ax,
                ay,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )])
        }

        fn pointer_move_relative(&self, dx: i16, dy: i16) -> Result<(), InputError> {
            // `MOUSEEVENTF_MOVE` **without** `MOUSEEVENTF_ABSOLUTE` is relative motion — `dx`/`dy` are
            // pixel deltas from the current cursor, which Windows clamps to the virtual desktop natively
            // (no off-screen escape, Inv 6). Display-independent, so no geometry/`to_abs` (ADR-087).
            self.send(&[mouse_input(
                i32::from(dx),
                i32::from(dy),
                0,
                MOUSEEVENTF_MOVE,
            )])
        }

        fn pointer_button(
            &self,
            display: u32,
            nx: f32,
            ny: f32,
            button: PointerButton,
            down: bool,
        ) -> Result<(), InputError> {
            let (tag, flag) = match (button, down) {
                (PointerButton::Left, true) => (0u8, MOUSEEVENTF_LEFTDOWN),
                (PointerButton::Left, false) => (0, MOUSEEVENTF_LEFTUP),
                (PointerButton::Right, true) => (1, MOUSEEVENTF_RIGHTDOWN),
                (PointerButton::Right, false) => (1, MOUSEEVENTF_RIGHTUP),
                (PointerButton::Middle, true) => (2, MOUSEEVENTF_MIDDLEDOWN),
                (PointerButton::Middle, false) => (2, MOUSEEVENTF_MIDDLEUP),
                // Fail-closed for an unrecognized future button variant.
                _ => {
                    return Err(RasError::fatal(
                        ErrorCode::InputFailed,
                        "unknown pointer button",
                    ))
                }
            };
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (ax, ay) = self.to_abs(display, nx, ny, &st);
            // Position + button in one event (absolute move, then the button transition).
            self.send(&[mouse_input(
                ax,
                ay,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | flag,
            )])?;
            if down {
                st.pressed_buttons.insert(tag);
            } else {
                st.pressed_buttons.remove(&tag);
            }
            Ok(())
        }

        fn pointer_wheel(&self, dx: i16, dy: i16) -> Result<(), InputError> {
            // Windows wheel: positive = up/right. Our dy is down-positive, so negate it; dx is
            // right-positive, matching HWHEEL.
            let vsteps = i32::from(dy).abs().min(MAX_WHEEL_NOTCHES);
            if vsteps > 0 {
                let data = if dy > 0 { -WHEEL_DELTA } else { WHEEL_DELTA } * vsteps;
                self.send(&[mouse_input(0, 0, data as u32, MOUSEEVENTF_WHEEL)])?;
            }
            let hsteps = i32::from(dx).abs().min(MAX_WHEEL_NOTCHES);
            if hsteps > 0 {
                let data = if dx > 0 { WHEEL_DELTA } else { -WHEEL_DELTA } * hsteps;
                self.send(&[mouse_input(0, 0, data as u32, MOUSEEVENTF_HWHEEL)])?;
            }
            Ok(())
        }

        fn key(&self, hid_usage: u16, down: bool, modifiers: u8) -> Result<(), InputError> {
            let vk = hid_to_vk(hid_usage)
                .ok_or_else(|| RasError::fatal(ErrorCode::InputFailed, "unmapped physical key"))?;
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.reconcile_mods(modifiers, &mut st)?;
            let flags = if down {
                KEYBD_EVENT_FLAGS(0)
            } else {
                KEYEVENTF_KEYUP
            };
            self.send(&[key_input(vk, 0, flags)])?;
            if down {
                st.pressed_keys.insert(vk);
            } else {
                st.pressed_keys.remove(&vk);
            }
            Ok(())
        }

        fn text(&self, utf8: &str) -> Result<(), InputError> {
            // Layout-independent Unicode entry via KEYEVENTF_UNICODE (one keydown/up per UTF-16 unit).
            // `keyboard.text` is withheld by `phase3_default_policy`, so this is off the default path.
            for unit in utf8.encode_utf16() {
                self.send(&[
                    key_input(0, unit, KEYEVENTF_UNICODE),
                    key_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                ])?;
            }
            Ok(())
        }

        fn release_all(&self) -> Result<(), InputError> {
            // Best-effort key-state cleanup on the emergency-stop / teardown path (Inv 4): release as
            // much as possible and never abort early — a per-event failure skips that one release.
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            for vk in st.pressed_keys.drain().collect::<Vec<_>>() {
                let _ = self.send(&[key_input(vk, 0, KEYEVENTF_KEYUP)]);
            }
            for tag in st.pressed_buttons.drain().collect::<Vec<_>>() {
                let flag = match tag {
                    0 => MOUSEEVENTF_LEFTUP,
                    1 => MOUSEEVENTF_RIGHTUP,
                    _ => MOUSEEVENTF_MIDDLEUP,
                };
                let _ = self.send(&[mouse_input(0, 0, 0, flag)]);
            }
            for (bit, vk) in MOD_VKS {
                if st.held_mods & bit != 0 {
                    let _ = self.send(&[key_input(vk, 0, KEYEVENTF_KEYUP)]);
                }
            }
            st.held_mods = 0;
            Ok(())
        }

        fn set_lock_state(&self, caps_lock: bool, num_lock: bool) -> Result<(), InputError> {
            // Read the live toggle state (low bit of GetKeyState) and tap the lock key only on a
            // mismatch — idempotent, never blindly toggles. VK_CAPITAL = 0x14, VK_NUMLOCK = 0x90.
            // SAFETY: GetKeyState is a pure query of one virtual-key.
            let cur_caps = (unsafe { GetKeyState(0x14) } & 1) != 0;
            let cur_num = (unsafe { GetKeyState(0x90) } & 1) != 0;
            if cur_caps != caps_lock {
                self.send(&[
                    key_input(0x14, 0, KEYBD_EVENT_FLAGS(0)),
                    key_input(0x14, 0, KEYEVENTF_KEYUP),
                ])?;
            }
            if cur_num != num_lock {
                self.send(&[
                    key_input(0x90, 0, KEYBD_EVENT_FLAGS(0)),
                    key_input(0x90, 0, KEYEVENTF_KEYUP),
                ])?;
            }
            Ok(())
        }

        fn input_permitted(&self) -> bool {
            // Windows has no per-app input-permission prompt; session-level injection is available. The
            // secure desktop / higher-integrity windows are out of scope (Inv 14) and fail at the OS.
            true
        }
    }

    /// Build a mouse `INPUT`.
    fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Build a keyboard `INPUT` (`vk` = virtual key, or 0 with `scan` for `KEYEVENTF_UNICODE`).
    fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// One axis of the normalized pixel → absolute `0..=65535` virtual-desktop mapping.
    fn to_abs_axis(px: f64, v_origin: i32, v_size: i32) -> i32 {
        if v_size <= 1 {
            return 0;
        }
        let a = (px - f64::from(v_origin)) / (f64::from(v_size) - 1.0) * 65535.0;
        a.round().clamp(0.0, 65535.0) as i32
    }

    /// Map a USB-HID Keyboard/Keypad usage (page 0x07) to a Windows **virtual-key** code. Covers the
    /// common alphanumeric, whitespace, arrow, and modifier keys; an unmapped usage fails closed at the
    /// call site (never a keysym — Inv 6). The full table is completed on-device.
    #[allow(clippy::too_many_lines)]
    fn hid_to_vk(hid: u16) -> Option<u16> {
        // Letters a–z (HID 0x04..=0x1D) → VK 'A'..'Z' (0x41..0x5A).
        if (0x04..=0x1D).contains(&hid) {
            return Some(0x41 + (hid - 0x04));
        }
        // Digits 1–9 (HID 0x1E..=0x26) → VK '1'..'9' (0x31..0x39).
        if (0x1E..=0x26).contains(&hid) {
            return Some(0x31 + (hid - 0x1E));
        }
        let vk: u16 = match hid {
            0x27 => 0x30, // 0
            // Whitespace / editing.
            0x28 => 0x0D, // Return (VK_RETURN)
            0x29 => 0x1B, // Escape
            0x2A => 0x08, // Backspace
            0x2B => 0x09, // Tab
            0x2C => 0x20, // Space
            0x2D => 0xBD, // - _  (VK_OEM_MINUS)
            0x2E => 0xBB, // = +  (VK_OEM_PLUS)
            0x2F => 0xDB, // [ {  (VK_OEM_4)
            0x30 => 0xDD, // ] }  (VK_OEM_6)
            0x31 => 0xDC, // \ |  (VK_OEM_5)
            0x33 => 0xBA, // ; :  (VK_OEM_1)
            0x34 => 0xDE, // ' "  (VK_OEM_7)
            0x35 => 0xC0, // ` ~  (VK_OEM_3)
            0x36 => 0xBC, // , <  (VK_OEM_COMMA)
            0x37 => 0xBE, // . >  (VK_OEM_PERIOD)
            0x38 => 0xBF, // / ?  (VK_OEM_2)
            0x39 => 0x14, // Caps Lock (VK_CAPITAL)
            // Arrows.
            0x4F => 0x27, // Right (VK_RIGHT)
            0x50 => 0x25, // Left  (VK_LEFT)
            0x51 => 0x28, // Down  (VK_DOWN)
            0x52 => 0x26, // Up    (VK_UP)
            // Modifiers (left/right specific virtual keys).
            0xE0 => 0xA2, // Left Control  (VK_LCONTROL)
            0xE1 => 0xA0, // Left Shift    (VK_LSHIFT)
            0xE2 => 0xA4, // Left Alt      (VK_LMENU)
            0xE3 => 0x5B, // Left GUI      (VK_LWIN)
            0xE4 => 0xA3, // Right Control (VK_RCONTROL)
            0xE5 => 0xA1, // Right Shift   (VK_RSHIFT)
            0xE6 => 0xA5, // Right Alt     (VK_RMENU)
            0xE7 => 0x5C, // Right GUI     (VK_RWIN)
            _ => return None,
        };
        Some(vk)
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used)]
        use super::*;

        #[test]
        fn hid_maps_common_keys_and_rejects_unknown() {
            assert_eq!(hid_to_vk(0x04), Some(0x41)); // a → VK_A
            assert_eq!(hid_to_vk(0x1D), Some(0x5A)); // z → VK_Z
            assert_eq!(hid_to_vk(0x1E), Some(0x31)); // 1 → '1'
            assert_eq!(hid_to_vk(0x27), Some(0x30)); // 0 → '0'
            assert_eq!(hid_to_vk(0x2C), Some(0x20)); // space
            assert_eq!(hid_to_vk(0xE1), Some(0xA0)); // left shift → VK_LSHIFT
            assert_eq!(hid_to_vk(0xFFFF), None); // unmapped → fail-closed
        }

        #[test]
        fn abs_axis_maps_endpoints_and_clamps() {
            // 1920-wide virtual screen at origin 0.
            assert_eq!(to_abs_axis(0.0, 0, 1920), 0);
            assert_eq!(to_abs_axis(1919.0, 0, 1920), 65535);
            // Out of range clamps rather than overflowing.
            assert_eq!(to_abs_axis(-50.0, 0, 1920), 0);
            assert_eq!(to_abs_axis(5000.0, 0, 1920), 65535);
            // Degenerate size fails safe to origin.
            assert_eq!(to_abs_axis(100.0, 0, 1), 0);
            // Non-zero virtual-screen origin (secondary monitor left of primary).
            assert_eq!(to_abs_axis(-1920.0, -1920, 1921), 0);
        }
    }
}

#[cfg(target_os = "windows")]
pub use win::SendInputSink;
