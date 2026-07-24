//! Wayland-capable Linux OS-input backend (docs/19 §3 follow-up to ADR-070): a second
//! [`ras_control::OsInputSink`] implemented over the kernel **`uinput`** device via the pure-Rust-wrapper
//! [`input_linux`] crate (MIT; `input-linux-sys` re-exports the libc `input_event` structs, Linux-only).
//!
//! # Why this exists alongside the XTEST backend
//! The [`super::X11InputSink`] injects through the X11 server, so it works only inside an X11/Xwayland
//! session and can never drive native Wayland windows or the compositor. `uinput` creates a **virtual
//! HID device at the kernel evdev layer, *below* the display server**, so the very same injected events
//! are seen by X11 and pure-Wayland compositors alike. That lifts the current
//! "X11/Xwayland-only" limitation — at the cost of needing elevated device access (a udev rule granting
//! write on `/dev/uinput`, plus the `uinput` module loaded), which XTEST does not require. See
//! [`UInputSink::input_permitted`] for the fail-closed preflight and [`best_input_sink`](super::best_input_sink)
//! for the automatic selection.
//!
//! # Coordinates (Inv 6, multi-monitor)
//! The trait receives only **normalized** `0.0..=1.0` fractions of a specific display. Unlike XTEST
//! (absolute *screen pixels*), `uinput` posts into a single virtual device-space abs range we declare as
//! `0..=65535`, which the compositor maps across the **whole logical output** (there is one virtual
//! pointer device, not one per monitor). So a display id is not a separate device — it selects a
//! **sub-rectangle** of that shared abs range: [`UInputSink::set_display_bounds`] records each display's
//! global-pixel bounds (fed from the host's capture geometry, the same convention as the X11/macOS/
//! Windows backends), and [`crate::pure::norm_on_display_to_abs`] maps a normalized fraction of display
//! `id` to the global pixel it names and then renormalizes that pixel over the union of every registered
//! display (the desktop bounds) onto `0..=ABS_MAX`. An unknown display id falls back to the desktop union
//! (fail-safe: input still lands somewhere on the shared output, never off it).
//!
//! # Keycodes
//! `uinput` speaks raw **Linux evdev** keycodes (`KEY_A == 30`), *not* X11 keycodes (evdev + 8). The
//! shared HID-usage → keycode map here therefore emits evdev codes directly — it is the XTEST table
//! **minus 8** per entry. An unmapped usage fails closed at the call site (never a wrong key).
//!
//! # Held state (Inv 4)
//! `uinput` key state is edge-driven like a real keyboard (no per-event modifier flag on the wire, same
//! as XTEST), so we realize the requested modifier bitset by pressing/releasing modifier keys and track
//! every pressed key/button so [`OsInputSink::release_all`] can release them all on emergency-stop /
//! teardown.
//!
//! # `unsafe`
//! Confined to the single fd-from-file bridge ([`std::os::fd::OwnedFd::from_raw_fd`]) at the FFI edge —
//! `input_linux` itself wraps every ioctl safely. This is the CONTRIBUTING §5 pattern (minimal `unsafe`
//! at the FFI boundary), and it is why this module carries a local `unsafe_code` allow while the crate's
//! X11 path stays `unsafe`-free.
//!
//! **Runtime status:** cross-compile-*checks* for `x86_64-unknown-linux-gnu`; live `uinput` injection is
//! an on-device step (a machine with the `uinput` module + udev rule). The pure logic (HID→keycode map,
//! normalized→abs scaling) is unit-tested off-device.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::os::fd::OwnedFd;
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::sync::Mutex;

use input_linux::sys as ils;
use input_linux::{
    AbsoluteAxis, AbsoluteEvent, AbsoluteInfo, AbsoluteInfoSetup, EventKind, EventTime, InputEvent,
    InputId, Key, KeyEvent, KeyState, RelativeAxis, RelativeEvent, SynchronizeEvent,
    SynchronizeKind, UInputHandle,
};
use ras_control::{InputError, OsInputSink};
use ras_protocol::{ErrorCode, PointerButton, RasError};

use crate::pure::{hid_to_keycode, norm_on_display_to_abs, ABS_MAX};

/// Cap wheel notches per event so a hostile delta can't spin an unbounded scroll loop (mirrors the
/// X11 backend's guard).
const MAX_WHEEL_NOTCHES: i32 = 64;

// evdev keycodes for the modifier bits, in the same stable bit order as the X11 backend but WITHOUT the
// +8 X offset (uinput speaks raw evdev). Left-hand variants realize a requested modifier bitset.
const SHIFT_KC: u16 = 42; // KEY_LEFTSHIFT
const CTRL_KC: u16 = 29; // KEY_LEFTCTRL
const ALT_KC: u16 = 56; // KEY_LEFTALT
const META_KC: u16 = 125; // KEY_LEFTMETA

/// (bit, evdev keycode) for each modifier, in a stable order (matches the X11 backend's `MODS`).
const MODS: [(u8, u16); 4] = [
    (0x01, SHIFT_KC),
    (0x02, CTRL_KC),
    (0x04, ALT_KC),
    (0x08, META_KC),
];

// Lock keys, evdev codes.
const CAPS_KC: u16 = 58; // KEY_CAPSLOCK
const NUM_KC: u16 = 69; // KEY_NUMLOCK

/// A registered display's bounds in **global pixels** (the same convention the host's capture geometry
/// and the X11/macOS/Windows backends use), for indexing a normalized fraction into the right
/// sub-rectangle of the shared uinput abs range. See the module-level "Coordinates" doc.
#[derive(Debug, Clone, Copy)]
struct DisplayBounds {
    id: u32,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Mutable tracking state (behind a `Mutex` so the sink is `Send + Sync`). Tracks the last posted
/// absolute position (device-space, for wheel/key framing) and everything currently held down so
/// `release_all` can clear it (Inv 4).
#[derive(Debug, Default)]
struct State {
    pressed_keys: HashSet<u16>,
    pressed_buttons: HashSet<u16>,
    held_mods: u8,
    /// Last posted absolute position in device space (`0..=ABS_MAX`).
    last_abs: (i32, i32),
    /// Best-effort mirror of CapsLock/NumLock state (uinput gives us no way to read it back, so — unlike
    /// XTEST's `QueryPointer` — we track our own toggles and tap only on a requested mismatch).
    lock_caps: bool,
    lock_num: bool,
    /// Registered display bounds (global px), keyed by id — see [`DisplayBounds`].
    displays: Vec<DisplayBounds>,
}

/// A `uinput`-backed [`OsInputSink`]. Holds one virtual-device handle; `None` means the device could not
/// be created (no `/dev/uinput`, no permission, or the ioctls failed) — fail-closed, so
/// [`OsInputSink::input_permitted`] is `false` and the host refuses the lease rather than granting one
/// whose every event would silently fail.
pub struct UInputSink {
    handle: Option<UInputHandle<OwnedFd>>,
    state: Mutex<State>,
}

impl std::fmt::Debug for UInputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UInputSink")
            .field("device_created", &self.handle.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for UInputSink {
    fn default() -> Self {
        Self::new()
    }
}

impl UInputSink {
    /// Open `/dev/uinput` and create the virtual device. Never panics: any failure (missing device,
    /// permission denied, ioctl error) yields a sink whose [`OsInputSink::input_permitted`] is `false`.
    #[must_use]
    pub fn new() -> Self {
        let handle = Self::try_create().ok();
        Self {
            handle,
            state: Mutex::new(State::default()),
        }
    }

    /// Open `/dev/uinput`, declare capabilities, and run `UI_DEV_CREATE`. Returns the handle or the
    /// first I/O error. Kept separate so `new` can swallow it into the fail-closed `None`.
    fn try_create() -> std::io::Result<UInputHandle<OwnedFd>> {
        let file = OpenOptions::new().write(true).open("/dev/uinput")?;
        // SAFETY: `into_raw_fd` yields a live, owned, writable fd we have just opened and no longer
        // otherwise hold; wrapping it in an `OwnedFd` transfers that sole ownership so it is closed
        // exactly once when the handle drops. This is the only `unsafe` in the crate (FFI edge,
        // CONTRIBUTING §5).
        let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) };
        let uhandle = UInputHandle::new(fd);

        uhandle.set_evbit(EventKind::Key)?;
        uhandle.set_evbit(EventKind::Absolute)?;
        uhandle.set_evbit(EventKind::Relative)?;
        uhandle.set_evbit(EventKind::Synchronize)?;
        uhandle.set_absbit(AbsoluteAxis::X)?;
        uhandle.set_absbit(AbsoluteAxis::Y)?;
        uhandle.set_relbit(RelativeAxis::X)?;
        uhandle.set_relbit(RelativeAxis::Y)?;
        uhandle.set_relbit(RelativeAxis::Wheel)?;
        uhandle.set_relbit(RelativeAxis::HorizontalWheel)?;
        // Pointer buttons + a full keyboard (best-effort over every evdev key).
        uhandle.set_keybit(Key::ButtonLeft)?;
        uhandle.set_keybit(Key::ButtonRight)?;
        uhandle.set_keybit(Key::ButtonMiddle)?;
        for key in Key::iter() {
            let _ = uhandle.set_keybit(key);
        }

        let abs_info = AbsoluteInfo {
            value: 0,
            minimum: 0,
            maximum: ABS_MAX,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        };
        let abs_setup = [
            AbsoluteInfoSetup {
                axis: AbsoluteAxis::X,
                info: abs_info,
            },
            AbsoluteInfoSetup {
                axis: AbsoluteAxis::Y,
                info: abs_info,
            },
        ];
        let id = InputId {
            bustype: ils::BUS_USB,
            vendor: 0x1209,  // pid.codes VID (open-source)
            product: 0x0ca5, // "casual-ras" virtual device
            version: 1,
        };
        uhandle.create(&id, b"casual-ras-virtual-input", 0, &abs_setup)?;
        Ok(uhandle)
    }

    fn handle(&self) -> Result<&UInputHandle<OwnedFd>, InputError> {
        self.handle
            .as_ref()
            .ok_or_else(|| RasError::recoverable(ErrorCode::InputFailed, "no uinput device"))
    }

    /// Write one logical batch of typed events, terminated by a `SYN_REPORT` frame (the kernel does not
    /// dispatch a batch without it). Fire-and-forget-ish: bubbles the first write error as `InputFailed`.
    fn emit(&self, events: &[InputEvent]) -> Result<(), InputError> {
        let handle = self.handle()?;
        let t = EventTime::new(0, 0); // kernel stamps on write; zero is fine
        let mut raw: Vec<ils::input_event> = events.iter().map(|e| *e.as_raw()).collect();
        raw.push(
            *SynchronizeEvent::new(t, SynchronizeKind::Report, 0)
                .into_event()
                .as_raw(),
        );
        handle
            .write(&raw)
            .map_err(|_| RasError::recoverable(ErrorCode::InputFailed, "uinput write failed"))?;
        Ok(())
    }

    /// Post an absolute move to `(ax, ay)` in device space (already clamped to `0..=ABS_MAX`) as one
    /// SYN-terminated frame, updating `last_abs`.
    fn abs_move(&self, ax: i32, ay: i32, st: &mut State) -> Result<(), InputError> {
        let t = EventTime::new(0, 0);
        st.last_abs = (ax, ay);
        self.emit(&[
            AbsoluteEvent::new(t, AbsoluteAxis::X, ax).into_event(),
            AbsoluteEvent::new(t, AbsoluteAxis::Y, ay).into_event(),
        ])
    }

    /// A key press/release edge on an evdev keycode, one SYN-terminated frame.
    fn key_edge(&self, kc: u16, down: bool) -> Result<(), InputError> {
        let key = Key::from_code(kc)
            .map_err(|_| RasError::fatal(ErrorCode::InputFailed, "invalid evdev keycode"))?;
        let t = EventTime::new(0, 0);
        let state = if down {
            KeyState::PRESSED
        } else {
            KeyState::RELEASED
        };
        self.emit(&[KeyEvent::new(t, key, state).into_event()])
    }

    /// A single wheel notch (`REL_WHEEL` / `REL_HWHEEL`), one SYN-terminated frame.
    fn wheel_notch(&self, axis: RelativeAxis, delta: i32) -> Result<(), InputError> {
        let t = EventTime::new(0, 0);
        self.emit(&[RelativeEvent::new(t, axis, delta).into_event()])
    }

    /// Realize the requested modifier bitset by pressing/releasing modifier keycodes so the held set
    /// matches `want` (uinput has no per-event modifier flag — evdev keys are stateful).
    fn reconcile_mods(&self, want: u8, st: &mut State) -> Result<(), InputError> {
        for (bit, kc) in MODS {
            let want_on = want & bit != 0;
            let is_on = st.held_mods & bit != 0;
            if want_on && !is_on {
                self.key_edge(kc, true)?;
                st.held_mods |= bit;
            } else if !want_on && is_on {
                self.key_edge(kc, false)?;
                st.held_mods &= !bit;
            }
        }
        Ok(())
    }

    /// Register a display's global-pixel bounds, from the host's capture geometry. Replaces any prior
    /// bounds for the same display id. See the module-level "Coordinates" doc: the uinput device itself
    /// has one shared abs range spanning the whole desktop, so this only affects which **sub-rectangle**
    /// of that range a given display id's normalized input maps into.
    pub fn set_display_bounds(&self, id: u32, x: f64, y: f64, w: f64, h: f64) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.displays.retain(|b| b.id != id);
        st.displays.push(DisplayBounds { id, x, y, w, h });
    }

    /// The global-pixel bounding box of the whole desktop — the union of every registered display, or a
    /// unit box at the origin if none are registered yet (so a lookup before the first
    /// `set_display_bounds` call still maps `0.0..=1.0` onto the full `0..=ABS_MAX` range 1:1, matching
    /// the pre-multi-monitor behavior rather than collapsing to a single point).
    fn desktop_bounds(st: &State) -> (f64, f64, f64, f64) {
        if st.displays.is_empty() {
            (0.0, 0.0, f64::from(ABS_MAX), f64::from(ABS_MAX))
        } else {
            let min_x = st
                .displays
                .iter()
                .map(|b| b.x)
                .fold(f64::INFINITY, f64::min);
            let min_y = st
                .displays
                .iter()
                .map(|b| b.y)
                .fold(f64::INFINITY, f64::min);
            let max_x = st
                .displays
                .iter()
                .map(|b| b.x + b.w)
                .fold(f64::NEG_INFINITY, f64::max);
            let max_y = st
                .displays
                .iter()
                .map(|b| b.y + b.h)
                .fold(f64::NEG_INFINITY, f64::max);
            (min_x, min_y, max_x - min_x, max_y - min_y)
        }
    }

    /// Map a normalized `(nx, ny)` on display `id` to device-space abs coordinates (`0..=ABS_MAX`),
    /// indexed to that display's sub-rectangle of the shared virtual output. Falls back to treating the
    /// fraction as spanning the whole desktop union if `id` is unknown (fail-safe — input still lands
    /// somewhere on the shared output, never off it or on the wrong monitor silently).
    fn to_abs(display: u32, nx: f32, ny: f32, st: &State) -> (i32, i32) {
        let (dx0, dy0, dw, dh) = Self::desktop_bounds(st);
        let (ox, oy, w, h) = st
            .displays
            .iter()
            .find(|b| b.id == display)
            .map_or((dx0, dy0, dw, dh), |b| (b.x, b.y, b.w, b.h));
        let ax = norm_on_display_to_abs(nx, ox, w, dx0, dw);
        let ay = norm_on_display_to_abs(ny, oy, h, dy0, dh);
        (ax, ay)
    }
}

impl OsInputSink for UInputSink {
    fn pointer_move(&self, display: u32, nx: f32, ny: f32) -> Result<(), InputError> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (ax, ay) = Self::to_abs(display, nx, ny, &st);
        self.abs_move(ax, ay, &mut st)
    }

    fn pointer_move_relative(&self, dx: i16, dy: i16) -> Result<(), InputError> {
        // uinput supports true relative motion (REL_X/REL_Y) with no read-modify-write — the kernel/
        // compositor clamps to the output itself, so unlike XTEST there is no QueryPointer round-trip.
        let t = EventTime::new(0, 0);
        self.emit(&[
            RelativeEvent::new(t, RelativeAxis::X, i32::from(dx)).into_event(),
            RelativeEvent::new(t, RelativeAxis::Y, i32::from(dy)).into_event(),
        ])
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
            PointerButton::Left => Key::ButtonLeft,
            PointerButton::Right => Key::ButtonRight,
            PointerButton::Middle => Key::ButtonMiddle,
            // Fail-closed for an unrecognized future button variant.
            _ => {
                return Err(RasError::fatal(
                    ErrorCode::InputFailed,
                    "unknown pointer button",
                ))
            }
        };
        let btn_code = btn as u16;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Position the pointer first (own SYN frame), then the button edge (own SYN frame) — a real HID
        // device sends distinct reports; a batched abs+button in one frame is dispatched atomically but
        // some compositors expect the move to precede the click.
        let (ax, ay) = Self::to_abs(display, nx, ny, &st);
        self.abs_move(ax, ay, &mut st)?;
        let t = EventTime::new(0, 0);
        let state = if down {
            KeyState::PRESSED
        } else {
            KeyState::RELEASED
        };
        self.emit(&[KeyEvent::new(t, btn, state).into_event()])?;
        if down {
            st.pressed_buttons.insert(btn_code);
        } else {
            st.pressed_buttons.remove(&btn_code);
        }
        Ok(())
    }

    fn pointer_wheel(&self, dx: i16, dy: i16) -> Result<(), InputError> {
        // REL_WHEEL convention: +1 = up, -1 = down. Our `dy` is down-positive, so negate.
        let vsteps = i32::from(dy).abs().min(MAX_WHEEL_NOTCHES);
        let vdir = if dy > 0 { -1 } else { 1 };
        for _ in 0..vsteps {
            self.wheel_notch(RelativeAxis::Wheel, vdir)?;
        }
        // REL_HWHEEL: +1 = right. Our `dx` is right-positive, so pass through.
        let hsteps = i32::from(dx).abs().min(MAX_WHEEL_NOTCHES);
        let hdir = if dx > 0 { 1 } else { -1 };
        for _ in 0..hsteps {
            self.wheel_notch(RelativeAxis::HorizontalWheel, hdir)?;
        }
        Ok(())
    }

    fn key(&self, hid_usage: u16, down: bool, modifiers: u8) -> Result<(), InputError> {
        let kc = hid_to_keycode(hid_usage)
            .ok_or_else(|| RasError::fatal(ErrorCode::InputFailed, "unmapped physical key"))?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.reconcile_mods(modifiers, &mut st)?;
        self.key_edge(kc, down)?;
        if down {
            st.pressed_keys.insert(kc);
        } else {
            st.pressed_keys.remove(&kc);
        }
        Ok(())
    }

    fn text(&self, _utf8: &str) -> Result<(), InputError> {
        // Layout-independent Unicode text over uinput requires synthesizing a keymap and composing
        // keysyms — not supported in v1. `keyboard.text` is withheld by `phase3_default_policy`, so this
        // is never reached on the default path — fail closed rather than mis-type (matches XTEST).
        Err(RasError::fatal(
            ErrorCode::InputFailed,
            "text input not supported on uinput",
        ))
    }

    fn release_all(&self) -> Result<(), InputError> {
        // Best-effort key-state cleanup on emergency-stop / teardown (Inv 4): release everything held and
        // never abort early — a per-event failure just skips that one release.
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for kc in st.pressed_keys.drain().collect::<Vec<_>>() {
            let _ = self.key_edge(kc, false);
        }
        for btn in st.pressed_buttons.drain().collect::<Vec<_>>() {
            let _ = self.key_edge(btn, false);
        }
        for (bit, kc) in MODS {
            if st.held_mods & bit != 0 {
                let _ = self.key_edge(kc, false);
            }
        }
        st.held_mods = 0;
        Ok(())
    }

    fn set_lock_state(&self, caps_lock: bool, num_lock: bool) -> Result<(), InputError> {
        // uinput gives no way to read back the compositor's lock state (unlike XTEST's QueryPointer), so
        // we track our own toggles and tap the lock key only on a requested mismatch — idempotent w.r.t.
        // our own history. First-tap alignment with a pre-existing OS lock state is an on-device caveat.
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.lock_caps != caps_lock {
            self.key_edge(CAPS_KC, true)?;
            self.key_edge(CAPS_KC, false)?;
            st.lock_caps = caps_lock;
        }
        if st.lock_num != num_lock {
            self.key_edge(NUM_KC, true)?;
            self.key_edge(NUM_KC, false)?;
            st.lock_num = num_lock;
        }
        Ok(())
    }

    fn input_permitted(&self) -> bool {
        // Fail-closed: input is permitted only if the virtual device was actually created (i.e.
        // /dev/uinput was openable + writable and every ioctl succeeded). If not, the host refuses the
        // lease and the app surfaces the honest "load the uinput module / add the udev rule" banner.
        // Unlike the XTEST backend there is NO Wayland refusal — driving Wayland is the whole point.
        self.handle.is_some()
    }
}

impl Drop for UInputSink {
    fn drop(&mut self) {
        // Release anything still held (Inv 4 belt) then destroy the virtual device so it does not linger.
        let _ = self.release_all();
        if let Some(h) = self.handle.as_ref() {
            let _ = h.dev_destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::pure::hid_to_keycode;

    #[test]
    fn modifier_keycodes_match_the_left_hand_hid_entries() {
        assert_eq!(hid_to_keycode(0xE1), Some(SHIFT_KC));
        assert_eq!(hid_to_keycode(0xE0), Some(CTRL_KC));
        assert_eq!(hid_to_keycode(0xE2), Some(ALT_KC));
        assert_eq!(hid_to_keycode(0xE3), Some(META_KC));
    }

    #[test]
    fn every_mapped_keycode_is_a_valid_evdev_key() {
        // Every code the table can emit must round-trip through `Key::from_code`, or a `key` call would
        // fail closed at injection. Sweep the whole HID keyboard/keypad + modifier ranges. (Linux-only —
        // it needs the real `input_linux::Key`; the pure map tests live in `crate::pure`.)
        for hid in (0x04u16..=0x65).chain(0xE0..=0xE7) {
            if let Some(kc) = hid_to_keycode(hid) {
                assert!(
                    Key::from_code(kc).is_ok(),
                    "hid {hid:#x} → evdev {kc} is not a valid input_linux::Key"
                );
            }
        }
    }

    #[test]
    fn a_device_less_sink_fails_closed() {
        // Construct a sink with no device (simulating no /dev/uinput). `input_permitted` must be false so
        // the host refuses the lease, and every emit fails rather than silently no-oping.
        let sink = UInputSink {
            handle: None,
            state: Mutex::new(State::default()),
        };
        assert!(!sink.input_permitted());
        assert!(sink.pointer_move(0, 0.5, 0.5).is_err());
        // release_all never errors (best-effort), even with no device.
        assert!(sink.release_all().is_ok());
    }

    fn device_less_sink() -> UInputSink {
        UInputSink {
            handle: None,
            state: Mutex::new(State::default()),
        }
    }

    #[test]
    fn no_registered_displays_maps_the_whole_abs_range_one_to_one() {
        // Before any `set_display_bounds` call (single-display / pre-multi-monitor behavior): a
        // normalized fraction must still scale directly onto the full 0..=ABS_MAX range.
        let sink = device_less_sink();
        let st = sink.state.lock().unwrap();
        assert_eq!(UInputSink::to_abs(0, 0.0, 0.0, &st), (0, 0));
        assert_eq!(UInputSink::to_abs(0, 1.0, 1.0, &st), (ABS_MAX, ABS_MAX));
        assert_eq!(
            UInputSink::to_abs(0, 0.5, 0.5, &st),
            ((ABS_MAX + 1) / 2, (ABS_MAX + 1) / 2)
        );
    }

    #[test]
    fn pointer_indexes_into_the_targeted_displays_sub_rectangle() {
        // A 1920x1080 primary at the origin plus a 1280x720 secondary to its right (desktop union is
        // 3200x1080). Input addressed to the SECONDARY display (id 1) must land in the right-hand portion
        // of the shared abs range, not be re-splatted across the whole device — the bug this fixes.
        let sink = device_less_sink();
        sink.set_display_bounds(0, 0.0, 0.0, 1920.0, 1080.0);
        sink.set_display_bounds(1, 1920.0, 0.0, 1280.0, 720.0);
        let st = sink.state.lock().unwrap();

        // Left edge of display 1 → 1920/3200 of the abs range, not abs 0.
        let (ax, _) = UInputSink::to_abs(1, 0.0, 0.0, &st);
        assert_eq!(ax, ((1920.0 / 3200.0) * f64::from(ABS_MAX)).round() as i32);
        assert_ne!(ax, 0, "must not collapse display 1 onto the desktop origin");

        // Right edge of display 1 → the very end of the shared abs range.
        let (ax_end, _) = UInputSink::to_abs(1, 1.0, 0.0, &st);
        assert_eq!(ax_end, ABS_MAX);

        // Display 0 (primary) still starts at abs 0 — unaffected by display 1's registration.
        let (ax0, ay0) = UInputSink::to_abs(0, 0.0, 0.0, &st);
        assert_eq!((ax0, ay0), (0, 0));
    }

    #[test]
    fn unknown_display_id_falls_back_to_the_desktop_union() {
        // A display id with no registered bounds must not error or silently do nothing (Inv 15's cousin
        // for OS-input fail-safety) — it maps as if spanning the whole known desktop.
        let sink = device_less_sink();
        sink.set_display_bounds(0, 0.0, 0.0, 1920.0, 1080.0);
        sink.set_display_bounds(1, 1920.0, 0.0, 1280.0, 720.0);
        let st = sink.state.lock().unwrap();
        let (ax, ay) = UInputSink::to_abs(99, 0.5, 0.5, &st);
        let (dx, dy, dw, dh) = UInputSink::desktop_bounds(&st);
        assert_eq!(
            ax,
            ((0.5 * dw + dx) / dw * f64::from(ABS_MAX)).round() as i32
        );
        assert_eq!(
            ay,
            ((0.5 * dh + dy) / dh * f64::from(ABS_MAX)).round() as i32
        );
    }

    #[test]
    fn set_display_bounds_replaces_not_duplicates() {
        let sink = device_less_sink();
        sink.set_display_bounds(0, 0.0, 0.0, 1920.0, 1080.0);
        sink.set_display_bounds(0, 100.0, 100.0, 800.0, 600.0); // re-register same id
        let st = sink.state.lock().unwrap();
        assert_eq!(st.displays.len(), 1, "re-registering an id replaces it");
        assert_eq!(st.displays[0].w, 800.0);
    }
}
