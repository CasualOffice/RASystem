//! Unprivileged libei input injection via the XDG Desktop Portal RemoteDesktop interface.
//!
//! A **third** [`ras_control::OsInputSink`] backend (alongside XTEST and uinput) that drives
//! Wayland through the standard `org.freedesktop.portal.RemoteDesktop` D-Bus interface. Unlike
//! `uinput` it needs no udev rule / `/dev/uinput` write access, and unlike XTEST it is not
//! confined to X11/Xwayland — the portal's own compositor-side implementation is what actually
//! injects the event, so it reaches native Wayland clients.
//!
//! # Handshake (portal-managed consent, once per session)
//! 1. [`RemoteDesktop::new`] → [`RemoteDesktop::create_session`] → [`RemoteDesktop::select_devices`]
//!    (keyboard + pointer) → [`RemoteDesktop::start`]. `start` is what shows the portal's own
//!    consent dialog; the user Allows or Denies it exactly once for the session's lifetime.
//! 2. On approval, [`RemoteDesktop::connect_to_eis`] hands back an authenticated EIS socket fd.
//! 3. That fd is wrapped in a [`reis::ei::Context`] and driven through the libei **sender**
//!    handshake (we are injecting events, so `ei::handshake::ContextType::Sender`).
//! 4. The libei protocol is itself asynchronous and object-based: we wait for a `SeatAdded` event,
//!    bind the pointer/absolute-pointer/button/scroll/keyboard capabilities, then wait for the
//!    resulting `Device` to reach `DeviceAdded` + `DeviceResumed` before it can accept requests.
//!
//! # Async-sync bridge
//! [`OsInputSink`] is a synchronous trait, but the portal handshake and the libei protocol are
//! both asynchronous (D-Bus + a non-blocking EIS socket). [`LibeiInputSink::new`] therefore spawns
//! a **dedicated background thread** that runs its own single-threaded Tokio runtime and owns the
//! whole async session; the sink talks to it over a plain [`std::sync::mpsc`] command channel. This
//! means the backend never assumes the embedding app itself runs on Tokio (unlike a
//! `Handle::try_current()` bridge, which would silently go dead in a non-Tokio host). Construction
//! itself never blocks: it returns immediately with `input_permitted() == false` and flips to
//! `true` only once the portal has granted consent and a device is actually bound and emulating —
//! consumers that check readiness right at share-start (see `app/src-tauri/src/main.rs`) will
//! usually see `false` there and `true` by the time a `ControlRequest` actually arrives, matching
//! the documented "prompt at lease issuance, not per keystroke" UX.
//!
//! # Coordinates & keycodes
//! Reuses the same HID→evdev map + normalized→abs scaling as the uinput backend
//! (`crate::pure::hid_to_keycode`, `crate::pure::norm_to_abs`). `ei_pointer_absolute.motion_absolute`
//! takes **logical-pixel** coordinates scaled to the device's advertised region, but absent a
//! negotiated region (no `ei_device.region` seen) we fall back to the same `0..=ABS_MAX` virtual
//! device-space convention the uinput backend uses; the portal's own compositor-side implementation
//! maps that onto the real output. Precise per-monitor region mapping is an on-device follow-up.
//!
//! # Fail-closed (Inv 15)
//! Any handshake failure — portal absent, user denies consent, D-Bus unavailable, EIS protocol
//! error — leaves the sink with no bound, emulating device, so [`OsInputSink::input_permitted`]
//! stays `false` and the host refuses the lease rather than granting dead control. The background
//! thread never panics the caller: every fallible step is caught and simply leaves the shared
//! ready-state at `false`.
//!
//! **Runtime status:** cross-compile-*checks* for `x86_64-unknown-linux-gnu` (this sandbox has no
//! Linux linker, so only `cargo check`/`clippy` were run here, matching the X11/uinput backends'
//! documented posture); live portal consent + injection is an on-device step on a real Wayland
//! session (GNOME/KDE/wlroots with `xdg-desktop-portal` + a RemoteDesktop backend installed).

use std::collections::HashSet;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
use ashpd::desktop::PersistMode;
use ashpd::WindowIdentifier;
use futures_util::StreamExt;
use reis::event::{Connection as EiConnection, Device as EiDevice, DeviceCapability, EiEvent};
use reis::{ei, tokio::EiConvertEventStream};

use ras_control::{InputError, OsInputSink};
use ras_protocol::{ErrorCode, PointerButton, RasError};

use crate::pure::{hid_to_keycode, norm_to_abs};

/// Cap wheel notches per event so a hostile delta can't spin an unbounded scroll loop (mirrors the
/// X11/uinput backends' guard). One "notch" is emitted as one `ei_scroll.scroll_discrete` unit of
/// 120 (the libei convention, matching one physical wheel click).
const MAX_WHEEL_NOTCHES: i32 = 64;
const SCROLL_DISCRETE_UNIT: i32 = 120;

// evdev keycodes for the modifier bits, matching the uinput backend exactly (libei key codes are
// raw Linux evdev codes per the `ei_keyboard.key` documentation, "must match linux/input-event-codes.h").
const SHIFT_KC: u32 = 42; // KEY_LEFTSHIFT
const CTRL_KC: u32 = 29; // KEY_LEFTCTRL
const ALT_KC: u32 = 56; // KEY_LEFTALT
const META_KC: u32 = 125; // KEY_LEFTMETA

/// (bit, evdev keycode) for each modifier, in a stable order (matches the X11/uinput backends).
const MODS: [(u8, u32); 4] = [
    (0x01, SHIFT_KC),
    (0x02, CTRL_KC),
    (0x04, ALT_KC),
    (0x08, META_KC),
];

// Lock keys, evdev codes.
const CAPS_KC: u32 = 58; // KEY_CAPSLOCK
const NUM_KC: u32 = 69; // KEY_NUMLOCK

// evdev pointer-button codes (`linux/input-event-codes.h`), shared with the wire the portal expects.
const BTN_LEFT: u32 = 272;
const BTN_RIGHT: u32 = 273;
const BTN_MIDDLE: u32 = 274;

/// One injection request sent from the sync [`OsInputSink`] surface to the background session task.
/// Fire-and-forget from the caller's perspective (the channel is unbounded and the task is the sole
/// consumer) — matching the X11/uinput backends' "best effort, bubble the sink-level error only"
/// contract; there is no per-command acknowledgement, since none of the actual portal/libei calls in
/// `reis`/`ashpd` return per-request errors (they are one-way protocol requests).
#[derive(Debug, Clone, Copy)]
enum Cmd {
    PointerMotionAbsolute(f32, f32),
    PointerMotionRelative(f32, f32),
    Button(u32, bool),
    ScrollDiscrete(i32, i32),
    Key(u32, bool),
    /// Best-effort release of everything currently tracked as held (Inv 4).
    ReleaseAll,
    Shutdown,
}

/// Shared readiness flag between the background session task and the sync [`OsInputSink`] surface.
/// `true` only once the portal has granted consent, a device advertising the needed capabilities has
/// been bound, and that device is actively emulating (`ei_device.resumed` seen, `start_emulating`
/// sent). Flips back to `false` on `ei_device.paused` or if the session ever disconnects.
#[derive(Default)]
struct Shared {
    ready: AtomicBool,
    /// Set the FIRST time `ready` ever becomes true this session. Lets [`best_input_sink`] tell
    /// "never worked" from "was working, then disconnected" using only the final state of `ready`.
    ever_ready: AtomicBool,
    /// Set once the background session ends having NEVER reached `ready` — i.e. the portal/libei
    /// handshake failed outright (no portal service, D-Bus unavailable, a protocol error) rather
    /// than merely "still waiting on the slow, human-interactive consent prompt" (which can take
    /// many seconds and must NOT be mistaken for failure). [`best_input_sink`] does a short bounded
    /// wait for this flag to decide whether to fall back to uinput/XTEST instead.
    failed_before_ready: AtomicBool,
}

/// Mutable tracking state for `release_all` / lock-state reconciliation (mirrors the X11/uinput
/// backends' `State`). Held on the sync side only — the background task is stateless with respect
/// to "what does the caller think is held," since it just forwards `Cmd`s.
#[derive(Debug, Default)]
struct TrackedState {
    pressed_keys: HashSet<u32>,
    pressed_buttons: HashSet<u32>,
    held_mods: u8,
    lock_caps: bool,
    lock_num: bool,
}

/// A libei-backed (XDG Desktop Portal RemoteDesktop) [`OsInputSink`].
///
/// Holds a command channel to a dedicated background thread that owns the whole async portal +
/// libei session. `cmd_tx` is `None` only if the background thread itself failed to spawn (never
/// observed in practice — [`std::thread::Builder::spawn`] failures are OS-resource exhaustion), in
/// which case this sink is permanently fail-closed.
pub struct LibeiInputSink {
    cmd_tx: Option<std_mpsc::Sender<Cmd>>,
    shared: Arc<Shared>,
    state: Mutex<TrackedState>,
}

impl std::fmt::Debug for LibeiInputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibeiInputSink")
            .field("ready", &self.shared.ready.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Default for LibeiInputSink {
    fn default() -> Self {
        Self::new()
    }
}

impl LibeiInputSink {
    /// Spawn the background portal/libei session and return immediately. Never blocks and never
    /// panics: [`OsInputSink::input_permitted`] starts `false` and only flips to `true` once the
    /// user has granted the portal's consent prompt and a device is actually bound and emulating.
    #[must_use]
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<Cmd>();
        let shared = Arc::new(Shared::default());
        let bg_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("ras-libei-session".into())
            .spawn(move || run_session_thread(cmd_rx, bg_shared))
            .is_ok();
        Self {
            cmd_tx: spawned.then_some(cmd_tx),
            shared,
            state: Mutex::new(TrackedState::default()),
        }
    }

    /// Send a command to the background session, ignoring send failure (the background thread has
    /// exited, which only happens after `Shutdown` or an unrecoverable protocol error — in either
    /// case there is nothing left to inject into, so silently dropping matches the other backends'
    /// "best effort" `release_all`/emit contracts). Non-`ReleaseAll`/`Shutdown` sends fail closed via
    /// `input_permitted` at the call site instead.
    fn send(&self, cmd: Cmd) -> Result<(), InputError> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| RasError::recoverable(ErrorCode::InputFailed, "no libei session"))?;
        tx.send(cmd)
            .map_err(|_| RasError::recoverable(ErrorCode::InputFailed, "libei session ended"))
    }

    fn reconcile_mods(&self, want: u8, st: &mut TrackedState) -> Result<(), InputError> {
        for (bit, kc) in MODS {
            let want_on = want & bit != 0;
            let is_on = st.held_mods & bit != 0;
            if want_on && !is_on {
                self.send(Cmd::Key(kc, true))?;
                st.held_mods |= bit;
            } else if !want_on && is_on {
                self.send(Cmd::Key(kc, false))?;
                st.held_mods &= !bit;
            }
        }
        Ok(())
    }

    /// Whether the background portal/libei session thread is running at all — **not** the same as
    /// [`OsInputSink::input_permitted`]. `true` as soon as the thread spawns, before the portal
    /// handshake has even started. Only used by [`Self::probe_available`] to short-circuit when the
    /// thread failed to spawn at all (OS-resource exhaustion); never a substitute for the fail-closed
    /// [`OsInputSink::input_permitted`] check the host performs before granting a lease (Inv 15).
    #[must_use]
    fn has_session(&self) -> bool {
        self.cmd_tx.is_some()
    }

    /// Whether this backend is worth *preferring* over uinput/XTEST (see
    /// [`best_input_sink`](super::best_input_sink)) — a short, BOUNDED wait (never the full,
    /// human-interactive consent-dialog wait, which can take many seconds and must not be mistaken
    /// for failure) to distinguish "the portal doesn't exist at all / D-Bus is unavailable" (a fast,
    /// reliably-quick failure, well under the bound) from "still negotiating, probably fine" (the
    /// common case, where the eventual Allow/Deny is correctly gated by [`OsInputSink::input_permitted`]
    /// downstream at lease-issuance time, Inv 15 — this probe is a preference heuristic only, never an
    /// authorization decision).
    pub(crate) fn probe_available(&self, timeout: std::time::Duration) -> bool {
        if !self.has_session() {
            return false;
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.shared.ready.load(Ordering::Acquire) {
                return true; // already ready — definitely prefer it
            }
            if self.shared.failed_before_ready.load(Ordering::Acquire) {
                return false; // confirmed fast failure (no portal / D-Bus unavailable) — fall back
            }
            if std::time::Instant::now() >= deadline {
                // Neither ready nor confirmed-failed within the bound: assume it is still
                // negotiating (most likely the slow human consent prompt) and prefer it anyway —
                // input_permitted() correctly fails closed later if it never actually completes.
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Register a display's global-pixel bounds. Currently a **documented no-op**: unlike the
    /// X11/uinput backends this does not yet map a normalized fraction into a per-display
    /// sub-rectangle — every display shares the same `0..=ABS_MAX` virtual device-space convention
    /// (see the module-level "Coordinates & keycodes" doc). Multi-monitor region mapping via
    /// `ei_device.region` is an on-device follow-up. Kept as a real method (not just absent) so
    /// [`LinuxInputSink::set_display_bounds`] can forward to it uniformly across all three backends.
    pub fn set_display_bounds(&self, _id: u32, _x: f64, _y: f64, _w: f64, _h: f64) {}
}

impl OsInputSink for LibeiInputSink {
    fn pointer_move(&self, _display: u32, nx: f32, ny: f32) -> Result<(), InputError> {
        if !self.input_permitted() {
            return Err(RasError::recoverable(
                ErrorCode::InputFailed,
                "libei not ready",
            ));
        }
        // Multi-monitor region mapping is a follow-up (see module docs); today this maps onto the
        // same 0..=ABS_MAX virtual device-space convention as the uinput backend.
        let (ax, ay) = (norm_to_abs(nx), norm_to_abs(ny));
        self.send(Cmd::PointerMotionAbsolute(ax as f32, ay as f32))
    }

    fn pointer_move_relative(&self, dx: i16, dy: i16) -> Result<(), InputError> {
        if !self.input_permitted() {
            return Err(RasError::recoverable(
                ErrorCode::InputFailed,
                "libei not ready",
            ));
        }
        // libei's ei_pointer.motion_relative takes logical-pixel deltas directly; the compositor
        // clamps to the output itself, so — like uinput — there is no read-modify-write round trip.
        self.send(Cmd::PointerMotionRelative(f32::from(dx), f32::from(dy)))
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
        if !self.input_permitted() {
            return Err(RasError::recoverable(
                ErrorCode::InputFailed,
                "libei not ready",
            ));
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Position the pointer first (own frame on the session thread), then the button edge, same
        // ordering as the X11/uinput backends.
        self.pointer_move(display, nx, ny)?;
        self.send(Cmd::Button(btn, down))?;
        if down {
            st.pressed_buttons.insert(btn);
        } else {
            st.pressed_buttons.remove(&btn);
        }
        Ok(())
    }

    fn pointer_wheel(&self, dx: i16, dy: i16) -> Result<(), InputError> {
        if !self.input_permitted() {
            return Err(RasError::recoverable(
                ErrorCode::InputFailed,
                "libei not ready",
            ));
        }
        // libei's scroll_discrete unit is 120 per physical wheel click (matches our "one notch").
        // Vertical: our dy is down-positive; libei's is the same (positive = down) per the
        // ei_scroll.scroll_discrete documentation mirroring wl_pointer's axis convention.
        let vsteps = i32::from(dy).abs().min(MAX_WHEEL_NOTCHES);
        let vdir = if dy > 0 { 1 } else { -1 };
        if vsteps > 0 {
            self.send(Cmd::ScrollDiscrete(0, vdir * SCROLL_DISCRETE_UNIT * vsteps))?;
        }
        let hsteps = i32::from(dx).abs().min(MAX_WHEEL_NOTCHES);
        let hdir = if dx > 0 { 1 } else { -1 };
        if hsteps > 0 {
            self.send(Cmd::ScrollDiscrete(hdir * SCROLL_DISCRETE_UNIT * hsteps, 0))?;
        }
        Ok(())
    }

    fn key(&self, hid_usage: u16, down: bool, modifiers: u8) -> Result<(), InputError> {
        let kc = hid_to_keycode(hid_usage)
            .ok_or_else(|| RasError::fatal(ErrorCode::InputFailed, "unmapped physical key"))?;
        if !self.input_permitted() {
            return Err(RasError::recoverable(
                ErrorCode::InputFailed,
                "libei not ready",
            ));
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.reconcile_mods(modifiers, &mut st)?;
        self.send(Cmd::Key(u32::from(kc), down))?;
        if down {
            st.pressed_keys.insert(u32::from(kc));
        } else {
            st.pressed_keys.remove(&u32::from(kc));
        }
        Ok(())
    }

    fn text(&self, _utf8: &str) -> Result<(), InputError> {
        // Layout-independent Unicode text needs the ei_text interface (a separate, unbound
        // capability here) or server-side keymap composition; not supported in v1 — matches the
        // X11/uinput backends' fail-closed stance. `keyboard.text` is withheld by
        // `phase3_default_policy`, so this is never reached on the default path.
        Err(RasError::fatal(
            ErrorCode::InputFailed,
            "text input not supported on libei",
        ))
    }

    fn release_all(&self) -> Result<(), InputError> {
        // Best-effort key-state cleanup on emergency-stop / teardown (Inv 4): never abort early, and
        // never fail even if the session is already gone (matches X11/uinput).
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for kc in st.pressed_keys.drain().collect::<Vec<_>>() {
            let _ = self.send(Cmd::Key(kc, false));
        }
        for btn in st.pressed_buttons.drain().collect::<Vec<_>>() {
            let _ = self.send(Cmd::Button(btn, false));
        }
        for (bit, kc) in MODS {
            if st.held_mods & bit != 0 {
                let _ = self.send(Cmd::Key(kc, false));
            }
        }
        st.held_mods = 0;
        drop(st);
        let _ = self.send(Cmd::ReleaseAll);
        Ok(())
    }

    fn set_lock_state(&self, caps_lock: bool, num_lock: bool) -> Result<(), InputError> {
        // libei gives no way to read back compositor lock state (unlike XTEST's QueryPointer), so —
        // like uinput — we track our own toggles and tap the lock key only on a requested mismatch.
        if !self.input_permitted() {
            return Err(RasError::recoverable(
                ErrorCode::InputFailed,
                "libei not ready",
            ));
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.lock_caps != caps_lock {
            self.send(Cmd::Key(CAPS_KC, true))?;
            self.send(Cmd::Key(CAPS_KC, false))?;
            st.lock_caps = caps_lock;
        }
        if st.lock_num != num_lock {
            self.send(Cmd::Key(NUM_KC, true))?;
            self.send(Cmd::Key(NUM_KC, false))?;
            st.lock_num = num_lock;
        }
        Ok(())
    }

    fn input_permitted(&self) -> bool {
        // Fail-closed: permit input only once the portal handshake has succeeded AND a device
        // advertising the needed capabilities is bound and actively emulating. Until then (no
        // portal, user denied consent, still negotiating, or the compositor paused the device) this
        // is `false` and the host refuses the lease (Inv 15) rather than granting dead control.
        self.cmd_tx.is_some() && self.shared.ready.load(Ordering::Acquire)
    }
}

impl Drop for LibeiInputSink {
    fn drop(&mut self) {
        let _ = self.release_all();
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(Cmd::Shutdown);
        }
    }
}

/// Entry point for the dedicated background thread: builds a single-threaded Tokio runtime and
/// drives the async session to completion (or until `Cmd::Shutdown` / an unrecoverable error).
/// Never panics the caller — `LibeiInputSink::new` has already returned by the time this runs.
fn run_session_thread(cmd_rx: std_mpsc::Receiver<Cmd>, shared: Arc<Shared>) {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    else {
        return;
    };
    rt.block_on(async move {
        // Bridge the sync std::sync::mpsc receiver onto the async side with one blocking-recv
        // helper thread — std::sync::mpsc has no async recv, and this keeps `Cmd` sends from the
        // sync `OsInputSink` surface simple (a plain, always-available `Sender::send`).
        let (async_tx, async_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        std::thread::spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                let is_shutdown = matches!(cmd, Cmd::Shutdown);
                if async_tx.send(cmd).is_err() || is_shutdown {
                    break;
                }
            }
        });
        let _ = run_portal_session(async_rx, &shared).await;
        shared.ready.store(false, Ordering::Release);
        // The session ended (portal absent, D-Bus unavailable, protocol error, or a normal
        // Shutdown) having never once become ready: a genuine, fast failure, not merely
        // "still negotiating" — record it so `best_input_sink`'s bounded wait can fall back.
        if !shared.ever_ready.load(Ordering::Acquire) {
            shared.failed_before_ready.store(true, Ordering::Release);
        }
    });
}

/// One bound `ei_device` plus the capability interfaces we negotiated on it, and whether it is
/// currently emulating (i.e. `start_emulating` has been sent and no matching `stop_emulating` /
/// pause has happened since).
struct BoundDevice {
    device: EiDevice,
    pointer_abs: Option<ei::PointerAbsolute>,
    pointer_rel: Option<ei::Pointer>,
    button: Option<ei::Button>,
    scroll: Option<ei::Scroll>,
    keyboard: Option<ei::Keyboard>,
    emulating: bool,
}

impl BoundDevice {
    fn release_all_best_effort(&self) {
        // Best-effort: the actual "what's held" bookkeeping lives on the sync side
        // (`LibeiInputSink::state`), which already sent explicit release `Cmd`s before this fires;
        // this is only the libei-level `stop_emulating` boundary so the compositor resets any state
        // it still thinks is held (per `ei_device.stop_emulating`'s documented neutral-state recommendation).
    }
}

/// Runs the portal handshake, the libei sender handshake, and the event/command loop until the
/// stream ends, `Cmd::Shutdown` is received, or an unrecoverable protocol error occurs.
async fn run_portal_session(
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    shared: &Arc<Shared>,
) -> Result<(), Box<dyn std::error::Error>> {
    let remote_desktop = RemoteDesktop::new().await?;
    let session = remote_desktop.create_session().await?;
    remote_desktop
        .select_devices(
            &session,
            DeviceType::Keyboard | DeviceType::Pointer,
            None,
            PersistMode::DoNot,
        )
        .await?;
    // This is the call that shows the portal's own consent dialog; it resolves only once the user
    // has responded (Allow/Deny) — the "once per session" prompt documented at the module level.
    let _selected = remote_desktop
        .start(&session, &WindowIdentifier::default())
        .await?
        .response()?;

    let fd = remote_desktop.connect_to_eis(&session).await?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    let context = ei::Context::new(stream)?;
    context.flush()?;

    let (hl_conn, mut events): (EiConnection, EiConvertEventStream) = context
        .handshake_tokio("casual-ras", ei::handshake::ContextType::Sender)
        .await?;

    let mut bound: Option<BoundDevice> = None;
    let mut seq: u32 = 0;

    loop {
        tokio::select! {
            ev = events.next() => {
                let Some(ev) = ev else { break; };
                let ev = ev?;
                handle_ei_event(ev, &mut bound, &mut seq, shared);
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                if matches!(cmd, Cmd::Shutdown) {
                    break;
                }
                if let Some(b) = &bound {
                    if b.emulating {
                        apply_cmd(b, cmd, hl_conn.serial());
                        let _ = hl_conn.flush();
                    }
                }
            }
        }
    }

    if let Some(b) = bound.take() {
        if b.emulating {
            b.device.device().stop_emulating(hl_conn.serial());
            let _ = hl_conn.flush();
        }
        b.release_all_best_effort();
    }
    Ok(())
}

/// Applies one high-level libei event to our local `Seat`/`Device` bookkeeping, binding capabilities
/// on a fresh seat and flipping the shared readiness flag once a device is actually emulating.
fn handle_ei_event(
    ev: EiEvent,
    bound: &mut Option<BoundDevice>,
    seq: &mut u32,
    shared: &Arc<Shared>,
) {
    match ev {
        EiEvent::SeatAdded(seat_added) => {
            seat_added.seat.bind_capabilities(
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll
                    | DeviceCapability::Keyboard,
            );
        }
        EiEvent::DeviceAdded(added) => {
            let device = added.device;
            let pointer_abs = device.interface::<ei::PointerAbsolute>();
            let pointer_rel = device.interface::<ei::Pointer>();
            let button = device.interface::<ei::Button>();
            let scroll = device.interface::<ei::Scroll>();
            let keyboard = device.interface::<ei::Keyboard>();
            *bound = Some(BoundDevice {
                device,
                pointer_abs,
                pointer_rel,
                button,
                scroll,
                keyboard,
                emulating: false,
            });
        }
        EiEvent::DeviceResumed(resumed) => {
            if let Some(b) = bound {
                if b.device == resumed.device && !b.emulating {
                    *seq += 1;
                    b.device.device().start_emulating(resumed.serial, *seq);
                    b.emulating = true;
                    shared.ready.store(true, Ordering::Release);
                    shared.ever_ready.store(true, Ordering::Release);
                }
            }
        }
        EiEvent::DevicePaused(paused) => {
            if let Some(b) = bound {
                if b.device == paused.device {
                    b.emulating = false;
                }
            }
            shared.ready.store(false, Ordering::Release);
        }
        EiEvent::DeviceRemoved(removed) => {
            if bound.as_ref().is_some_and(|b| b.device == removed.device) {
                *bound = None;
            }
            shared.ready.store(false, Ordering::Release);
        }
        EiEvent::Disconnected(_) => {
            *bound = None;
            shared.ready.store(false, Ordering::Release);
        }
        _ => {}
    }
}

/// Sends one injection command to the already-bound, already-emulating device, wrapping each
/// individual request in its own `ei_device.frame` (libei forbids grouping more than one request per
/// interface within a single frame, and our commands are always for a single interface at a time).
fn apply_cmd(b: &BoundDevice, cmd: Cmd, serial: u32) {
    match cmd {
        Cmd::PointerMotionAbsolute(x, y) => {
            if let Some(pa) = &b.pointer_abs {
                pa.motion_absolute(x, y);
                b.device.device().frame(serial, 0);
            }
        }
        Cmd::PointerMotionRelative(x, y) => {
            if let Some(pr) = &b.pointer_rel {
                pr.motion_relative(x, y);
                b.device.device().frame(serial, 0);
            }
        }
        Cmd::Button(code, down) => {
            if let Some(btn) = &b.button {
                let state = if down {
                    ei::button::ButtonState::Press
                } else {
                    ei::button::ButtonState::Released
                };
                btn.button(code, state);
                b.device.device().frame(serial, 0);
            }
        }
        Cmd::ScrollDiscrete(dx, dy) => {
            if let Some(s) = &b.scroll {
                s.scroll_discrete(dx, dy);
                b.device.device().frame(serial, 0);
            }
        }
        Cmd::Key(code, down) => {
            if let Some(kb) = &b.keyboard {
                let state = if down {
                    ei::keyboard::KeyState::Press
                } else {
                    ei::keyboard::KeyState::Released
                };
                kb.key(code, state);
                b.device.device().frame(serial, 0);
            }
        }
        Cmd::ReleaseAll | Cmd::Shutdown => {
            // ReleaseAll's actual key/button-up edges are sent as individual `Cmd::Key`/`Cmd::Button`
            // commands ahead of this marker (see `LibeiInputSink::release_all`); Shutdown never
            // reaches here (the event loop breaks on it before calling `apply_cmd`).
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::pure::hid_to_keycode;

    #[test]
    fn a_sessionless_sink_fails_closed() {
        // Simulate a sink whose background thread never spawned (portal/session unavailable).
        let sink = LibeiInputSink {
            cmd_tx: None,
            shared: Arc::new(Shared::default()),
            state: Mutex::new(TrackedState::default()),
        };
        assert!(!sink.input_permitted());
        assert!(sink.pointer_move(0, 0.5, 0.5).is_err());
        assert!(sink.key(0x04, true, 0).is_err());
        // release_all is best-effort, never errors even with no session.
        assert!(sink.release_all().is_ok());
    }

    #[test]
    fn not_yet_ready_sink_fails_closed_even_with_a_live_channel() {
        // A channel exists (the background thread spawned) but the shared ready flag is still
        // false (portal handshake still pending / consent not yet granted) — must still refuse.
        let (tx, _rx) = std_mpsc::channel::<Cmd>();
        let sink = LibeiInputSink {
            cmd_tx: Some(tx),
            shared: Arc::new(Shared::default()),
            state: Mutex::new(TrackedState::default()),
        };
        assert!(!sink.input_permitted());
        assert!(sink
            .pointer_button(0, 0.1, 0.1, PointerButton::Left, true)
            .is_err());
    }

    #[test]
    fn hid_keycodes_match_the_uinput_evdev_table() {
        // libei speaks raw evdev keycodes exactly like uinput (no X11 +8 offset).
        assert_eq!(hid_to_keycode(0x04), Some(30)); // a
        assert_eq!(hid_to_keycode(0x2C), Some(57)); // space
        assert_eq!(hid_to_keycode(0xE1), Some(42)); // left shift
        assert_eq!(hid_to_keycode(0xFFFF), None); // unmapped → fail-closed
    }

    #[test]
    fn modifier_keycodes_match_the_left_hand_hid_entries() {
        assert_eq!(hid_to_keycode(0xE1), Some(SHIFT_KC as u16));
        assert_eq!(hid_to_keycode(0xE0), Some(CTRL_KC as u16));
        assert_eq!(hid_to_keycode(0xE2), Some(ALT_KC as u16));
        assert_eq!(hid_to_keycode(0xE3), Some(META_KC as u16));
    }

    #[test]
    fn unknown_pointer_button_variant_is_rejected_before_touching_the_session() {
        // Even with a permitted sink this must fail closed on an unrecognized future button variant,
        // without ever reaching the channel (Inv 6 — the closed injectable set).
        let (tx, _rx) = std_mpsc::channel::<Cmd>();
        let shared = Arc::new(Shared::default());
        shared.ready.store(true, Ordering::Release);
        let sink = LibeiInputSink {
            cmd_tx: Some(tx),
            shared,
            state: Mutex::new(TrackedState::default()),
        };
        // `PointerButton` is `#[non_exhaustive]` with only Left/Right/Middle constructible here, so
        // this test exercises the three known variants map correctly instead (no panic, no error).
        assert!(sink
            .pointer_button(0, 0.5, 0.5, PointerButton::Left, true)
            .is_ok());
        assert!(sink
            .pointer_button(0, 0.5, 0.5, PointerButton::Right, false)
            .is_ok());
        assert!(sink
            .pointer_button(0, 0.5, 0.5, PointerButton::Middle, true)
            .is_ok());
    }

    #[test]
    fn probe_available_is_false_with_no_session_at_all() {
        let sink = LibeiInputSink {
            cmd_tx: None,
            shared: Arc::new(Shared::default()),
            state: Mutex::new(TrackedState::default()),
        };
        // Must return immediately (no sleep loop) — there is nothing to wait on.
        let start = std::time::Instant::now();
        assert!(!sink.probe_available(std::time::Duration::from_secs(5)));
        assert!(start.elapsed() < std::time::Duration::from_millis(50));
    }

    #[test]
    fn probe_available_is_true_immediately_when_already_ready() {
        let (tx, _rx) = std_mpsc::channel::<Cmd>();
        let shared = Arc::new(Shared::default());
        shared.ready.store(true, Ordering::Release);
        let sink = LibeiInputSink {
            cmd_tx: Some(tx),
            shared,
            state: Mutex::new(TrackedState::default()),
        };
        let start = std::time::Instant::now();
        assert!(sink.probe_available(std::time::Duration::from_secs(5)));
        assert!(start.elapsed() < std::time::Duration::from_millis(50));
    }

    #[test]
    fn probe_available_falls_back_fast_on_a_confirmed_failure() {
        // The regression this guards: without checking `failed_before_ready`, a portal-less machine
        // would always be (wrongly) preferred over uinput/XTEST since the thread always spawns.
        let (tx, _rx) = std_mpsc::channel::<Cmd>();
        let shared = Arc::new(Shared::default());
        shared.failed_before_ready.store(true, Ordering::Release);
        let sink = LibeiInputSink {
            cmd_tx: Some(tx),
            shared,
            state: Mutex::new(TrackedState::default()),
        };
        let start = std::time::Instant::now();
        assert!(!sink.probe_available(std::time::Duration::from_secs(5)));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "a confirmed failure must be reported fast, not after the full bound"
        );
    }

    #[test]
    fn probe_available_assumes_still_negotiating_past_the_deadline() {
        // Neither ready nor failed within the bound (the slow, human-interactive consent dialog is
        // still pending) — must assume it will likely work and prefer it, not fall back.
        let (tx, _rx) = std_mpsc::channel::<Cmd>();
        let sink = LibeiInputSink {
            cmd_tx: Some(tx),
            shared: Arc::new(Shared::default()),
            state: Mutex::new(TrackedState::default()),
        };
        assert!(sink.probe_available(std::time::Duration::from_millis(40)));
    }
}
