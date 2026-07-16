//! Casual RAS — the single unified desktop app (ADR-062).
//!
//! One binary, **both roles**, chosen at runtime from a home screen:
//!
//! - **Connect** (viewer) — dial a host's connection ticket over iroh and render its screen with
//!   WebCodecs. Platform-independent: the viewing side only decodes, so this works on
//!   macOS/Linux/Windows. Commands: [`connect_to_host`], [`disconnect`], [`send_pointer`],
//!   [`request_keyframe`].
//! - **Share** (agent) — capture *this* screen, hold the viewer in the handshake until the local user
//!   clicks **Allow** (real consent — Invariant 1), stream it over iroh, and draw the viewer's remote
//!   pointer on a transparent overlay. macOS-only for now (needs `ras-media-macos`); on other
//!   platforms [`start_sharing`] reports it isn't available yet. Commands: [`start_sharing`],
//!   [`stop_sharing`], [`respond_consent`].
//!
//! Built with `ras-core` `default-features = false`, so the `insecure-no-auth` `AllowAllValidator` is
//! **not even linked** — there is no way to skip consent. No pixels ever cross JSON IPC: encoded
//! access units ride a **binary** Tauri `Channel` (24-byte `RAS1` header + Annex-B).

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::sync::{Arc, Mutex};

use ras_core::frame_channel::encode_frame_blob;
use ras_core::transport::Endpoint;
use ras_core::{ControllerSession, CoreError, FrameSink, LifecycleStream, PushResult};
use ras_media::{EncodedFrame, StreamConfig};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Emitter, Manager, State};

/// Framing magic for the one-shot stream-config message (`"RCFG"` big-endian, sent little-endian).
/// Distinguishes the JSON config blob from the `RAS1` frame blobs on the same channel.
const CONFIG_MAGIC: u32 = u32::from_be_bytes(*b"RCFG");

// ─── Shared app state ────────────────────────────────────────────────────────────────────────────

/// Everything the Tauri commands share. Built in `.setup()` (the Share role's consent gate needs the
/// `AppHandle`).
struct AppState {
    /// A live **viewer** session (Connect role) — dial a host's ticket over iroh.
    session: Mutex<Option<ConnectedSession>>,
    /// The **sharer** side (Share role).
    share: ShareState,
}

/// A connected viewer session: the controller + the iroh endpoint that must outlive it.
struct ConnectedSession {
    _endpoint: Arc<Endpoint>,
    controller: Arc<ControllerSession>,
    _events: LifecycleStream,
    /// Monotonic per-session input sequence (Phase 3): the host rejects any `seq ≤ last_seen`, so this
    /// must strictly increase across every `Input` this viewer sends under its lease.
    input_seq: std::sync::atomic::AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Wall-clock ms since the Unix epoch, for the Phase-2 authorization timestamps (request/grant
/// validity windows). A pre-epoch clock saturates to 0 (fail-closed: everything reads "not yet
/// valid" rather than silently valid).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ─── Connect role (viewer) ─────────────────────────────────────────────────────────────────────

/// A [`FrameSink`] forwarding the decoded-stream config + each encoded access unit to the webview
/// over the binary Tauri channel. `configure` sends one `RCFG` JSON blob; `push` sends `RAS1` frame
/// blobs. No pixels ever cross JSON IPC. Platform-independent.
struct ChannelFrameSink {
    channel: Channel<InvokeResponseBody>,
}

impl FrameSink for ChannelFrameSink {
    fn configure(&self, config: &StreamConfig) -> Result<(), CoreError> {
        let codec = config.codec.webcodecs_string(config.width, config.height);
        let json = serde_json::json!({
            "codec": codec,
            "width": config.width,
            "height": config.height,
            "fps": config.fps,
        })
        .to_string();
        let mut blob = Vec::with_capacity(4 + json.len());
        blob.extend_from_slice(&CONFIG_MAGIC.to_le_bytes());
        blob.extend_from_slice(json.as_bytes());
        // A closed channel (webview gone) isn't fatal; the session tears down via stop.
        let _ = self.channel.send(InvokeResponseBody::Raw(blob));
        Ok(())
    }

    fn push(&self, frame: EncodedFrame) -> PushResult {
        match self
            .channel
            .send(InvokeResponseBody::Raw(encode_frame_blob(&frame)))
        {
            Ok(()) => PushResult::Sent,
            Err(_) => PushResult::Dropped,
        }
    }
}

/// Dial a host's **connection ticket** over iroh and render its screen. Works on any platform (the
/// viewer only decodes).
///
/// Phase 2 is a **two-phase** dial from **one** endpoint (so both connections share the controller's
/// authenticated endpoint id, which the sender-constraint binds): first the **bootstrap ALPN** —
/// prove identity, send a signed `AccessRequest`, and receive a PASETO grant (or a denial) — then the
/// **session ALPN**, presenting that grant in the `AuthEnvelope`. No pixels flow until the host has
/// validated the grant against this endpoint.
#[tauri::command]
async fn connect_to_host(
    state: State<'_, AppState>,
    ticket: String,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    use ras_core::grant::{fresh_id, AccessRequest, MAX_REQUEST_TTL_MS};
    use ras_core::identity::SoftwareKeyStore;
    use ras_core::policy::phase3_default_policy;
    use ras_core::transport::EndpointAddr;
    use ras_core::{ControllerSessionConfig, IrohSessionTransport};
    use ras_protocol::{AccessOutcome, BootstrapMsg, PROTOCOL_VERSION};

    // Tear down any prior viewer session first.
    let _ = disconnect(state.clone()).await;

    let target = EndpointAddr::from_ticket(ticket.trim()).map_err(|e| e.to_string())?;
    let endpoint = Arc::new(Endpoint::bind().await.map_err(|e| e.to_string())?);
    let my_endpoint_id = endpoint.id().0;
    // The controller's application identity (ephemeral per run in the MVP — persistence + a paired
    // trusted-controller registry is a later step).
    let ks = SoftwareKeyStore::generate().map_err(|e| e.to_string())?;

    // ── Bootstrap phase (casual-ras/bootstrap/1): request access, receive a grant. ──
    let boot_conn = endpoint
        .connect_bootstrap(&target)
        .await
        .map_err(|e| e.to_string())?;
    let mut boot = boot_conn.bootstrap().await.map_err(|e| e.to_string())?;
    boot.send(BootstrapMsg::ClientHello {
        protocol_version: PROTOCOL_VERSION,
    })
    .await
    .map_err(|e| e.to_string())?;
    let host_id = match boot.recv().await.map_err(|e| e.to_string())? {
        BootstrapMsg::HostHello { host_id, .. } => host_id,
        _ => return Err("unexpected bootstrap reply from host".into()),
    };
    let now = now_ms();
    let request = AccessRequest::signed(
        &ks,
        fresh_id().map_err(|e| e.to_string())?,
        PROTOCOL_VERSION,
        host_id,
        "Casual RAS viewer".to_string(),
        my_endpoint_id,
        // Request the Phase-3 capability set so the grant's ceiling can include OS input. This only
        // sets what the controller *may later ask for*; actually injecting still needs a separate
        // control-lease consent (Invariant 1) — the grant is the coarse gate, the lease the fine one.
        phase3_default_policy(),
        "remote support".to_string(),
        now,
        now + MAX_REQUEST_TTL_MS,
        fresh_id().map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    boot.send(BootstrapMsg::AccessRequest {
        canonical: request.encode(),
    })
    .await
    .map_err(|e| e.to_string())?;
    let grant = match boot.recv().await.map_err(|e| e.to_string())? {
        BootstrapMsg::AccessDecision(AccessOutcome::Allowed { grant }) => grant,
        BootstrapMsg::AccessDecision(AccessOutcome::Denied { code }) => {
            return Err(format!("access denied ({code})"));
        }
        _ => return Err("unexpected bootstrap decision from host".into()),
    };
    drop(boot);
    drop(boot_conn); // close the bootstrap connection; the endpoint lives on for the session dial

    // ── Session phase (casual-ras/1): present the grant, then render. ──
    let session = endpoint.connect(&target).await.map_err(|e| e.to_string())?;
    let transport = Arc::new(IrohSessionTransport::new(endpoint.clone(), session));
    let controller = Arc::new(ControllerSession::new(
        ControllerSessionConfig::new(target).with_grant(grant),
        transport,
    ));

    let events = controller.connect().await.map_err(|e| e.to_string())?;
    controller
        .attach_renderer(Arc::new(ChannelFrameSink { channel: on_frame }))
        .await
        .map_err(|e| e.to_string())?;

    *lock(&state.session) = Some(ConnectedSession {
        _endpoint: endpoint,
        controller,
        _events: events,
        input_seq: std::sync::atomic::AtomicU64::new(0),
    });
    Ok(())
}

/// Forward the viewer's pointer position to the host for its remote-pointer overlay ("look here").
/// Normalized `0..=65535`. Best-effort + non-blocking (latency-first). Not OS input — a purely visual
/// cursor. No-op unless a viewer session is live.
#[tauri::command]
async fn send_pointer(
    state: State<'_, AppState>,
    x: u16,
    y: u16,
    visible: bool,
) -> Result<(), String> {
    let controller = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = controller {
        c.send_pointer(x, y, visible);
    }
    Ok(())
}

/// End the live viewer session (idempotent).
#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    use ras_core::StopReason;
    let session = lock(&state.session).take();
    if let Some(s) = session {
        s.controller.disconnect(StopReason::UserRequested).await;
    }
    Ok(())
}

/// Ask the host (via the control channel) for a fresh IDR — used by the webview once its decoder is
/// configured (infinite-GOP means the lone startup keyframe may predate it) and on any decoder reset.
#[tauri::command]
async fn request_keyframe(state: State<'_, AppState>) -> Result<(), String> {
    use ras_protocol::KeyframeReason;
    let c = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = c {
        let _ = c.request_keyframe(KeyframeReason::DecoderReset).await;
    }
    Ok(())
}

// ─── Connect role: OS-input control (Phase 3) ────────────────────────────────────────────────────

/// Request the OS-input control lease from the host (Phase 3). The host prompts its local user
/// (Invariant 1); on Allow it replies with a lease and the viewer's subsequent input is injected.
/// No-op unless a viewer session is live. Requesting is not controlling — input flows only once the
/// host has granted the lease (surfaced via `is_controlling`).
#[tauri::command]
async fn request_control(state: State<'_, AppState>) -> Result<(), String> {
    let c = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = c {
        c.request_control(vec![
            "pointer.move".into(),
            "pointer.click".into(),
            "pointer.scroll".into(),
            "keyboard.key".into(),
        ]);
    }
    Ok(())
}

/// Whether this viewer currently holds an OS-input lease (i.e. its input is being injected). The UI
/// polls this to reflect control state and to gate its input capture.
#[tauri::command]
async fn is_controlling(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(lock(&state.session)
        .as_ref()
        .and_then(|s| s.controller.current_lease())
        .is_some())
}

/// Stamp and forward one OS-input action under the held lease. No-op if no lease is held (the host
/// would reject it anyway — this just avoids the round-trip). The host re-checks lease/generation/seq/
/// capability per message (ADR-069): this is a claim, not authority.
fn send_input_action(state: &State<'_, AppState>, action: ras_protocol::InputAction) {
    let guard = lock(&state.session);
    if let Some(s) = guard.as_ref() {
        if let Some((lease_id, generation)) = s.controller.current_lease() {
            let seq = s
                .input_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            s.controller.send_input(ras_protocol::InputEnvelope {
                lease_id,
                generation,
                seq,
                action,
            });
        }
    }
}

/// Move the host's OS pointer to a normalized position (`0..=65535`) on the shared display.
#[tauri::command]
async fn input_pointer_move(state: State<'_, AppState>, nx: u16, ny: u16) -> Result<(), String> {
    send_input_action(
        &state,
        ras_protocol::InputAction::PointerMove {
            display_id: 0,
            nx,
            ny,
            layout_version: 0,
        },
    );
    Ok(())
}

/// Press or release a pointer button (`"left"`/`"right"`/`"middle"`) at a normalized position.
#[tauri::command]
async fn input_pointer_button(
    state: State<'_, AppState>,
    nx: u16,
    ny: u16,
    button: String,
    down: bool,
) -> Result<(), String> {
    let button = match button.as_str() {
        "right" => ras_protocol::PointerButton::Right,
        "middle" => ras_protocol::PointerButton::Middle,
        _ => ras_protocol::PointerButton::Left,
    };
    send_input_action(
        &state,
        ras_protocol::InputAction::PointerButton {
            display_id: 0,
            nx,
            ny,
            layout_version: 0,
            button,
            down,
        },
    );
    Ok(())
}

/// Scroll by notched deltas (clamped `i16`).
#[tauri::command]
async fn input_pointer_wheel(state: State<'_, AppState>, dx: i16, dy: i16) -> Result<(), String> {
    send_input_action(&state, ras_protocol::InputAction::PointerWheel { dx, dy });
    Ok(())
}

/// Press or release a physical key by USB-HID usage (+ modifier bitset: 1 shift, 2 ctrl, 4 alt,
/// 8 cmd). Never a keysym — the host maps HID → OS keycode (Inv 6).
#[tauri::command]
async fn input_key(
    state: State<'_, AppState>,
    hid_usage: u16,
    down: bool,
    modifiers: u8,
) -> Result<(), String> {
    send_input_action(
        &state,
        ras_protocol::InputAction::KeyEvent {
            hid_usage,
            down,
            modifiers,
        },
    );
    Ok(())
}

/// Slave the host's CapsLock/NumLock to the controller's authoritative *state* (not key edges — see
/// ADR-074). Gated host-side on `keyboard.key`, so a pointer-only lease can't flip a lock (Inv 15).
#[tauri::command]
async fn input_set_lock_state(
    state: State<'_, AppState>,
    caps_lock: bool,
    num_lock: bool,
) -> Result<(), String> {
    send_input_action(
        &state,
        ras_protocol::InputAction::SetLockState {
            caps_lock,
            num_lock,
        },
    );
    Ok(())
}

// ─── Share role (agent) ──────────────────────────────────────────────────────────────────────────

/// The sharer side. `session` is `Some` while a share is active; `consent` is the local Allow/Deny
/// gate reached by both the running share task and the `respond_consent` command.
struct ShareState {
    session: Mutex<Option<ShareSession>>,
    consent: Arc<LocalConsent>,
}

/// A running share: the `watch` sender used to tear the whole share down.
struct ShareSession {
    stop: tokio::sync::watch::Sender<bool>,
}

/// The local-consent gate. Implements `ras-core`'s `GrantValidator`: when a viewer requests access it
/// emits a `consent-request` and **blocks the session until the local user answers** (or a timeout
/// denies). No pixels flow before Allow. One viewer at a time, so a single pending slot suffices.
struct LocalConsent {
    app: tauri::AppHandle,
    pending: Mutex<Option<tokio::sync::oneshot::Sender<bool>>>,
    /// A separate pending slot for the Phase-3 **control-lease** consent (Invariant 1): requesting OS
    /// input is a distinct, higher-stakes act than viewing, so it re-prompts on its own channel.
    pending_control: Mutex<Option<tokio::sync::oneshot::Sender<bool>>>,
}

impl LocalConsent {
    fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            pending: Mutex::new(None),
            pending_control: Mutex::new(None),
        }
    }

    /// Deliver the local user's decision to a waiting `prompt`. Extra/late calls are no-ops.
    fn respond(&self, allow: bool) {
        if let Some(tx) = lock(&self.pending).take() {
            let _ = tx.send(allow);
        }
    }

    /// Deliver the local user's decision to a waiting control-consent prompt. Late calls are no-ops.
    fn respond_control(&self, allow: bool) {
        if let Some(tx) = lock(&self.pending_control).take() {
            let _ = tx.send(allow);
        }
    }

    /// Prompt the local user (Invariant 1) and block until they answer, emitting `consent-request`
    /// with the requester's short identity. A 90 s silence **denies** (fail-closed) so a pending
    /// request can't hang the share forever. Returns `true` only on an explicit Allow.
    async fn prompt(&self, peer_short: String) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *lock(&self.pending) = Some(tx);
        let _ = self.app.emit("consent-request", peer_short);
        let allow = matches!(
            tokio::time::timeout(std::time::Duration::from_secs(90), rx).await,
            Ok(Ok(true))
        );
        *lock(&self.pending) = None;
        let _ = self.app.emit("consent-closed", ());
        allow
    }
}

/// Phase-3 control-lease consent (Invariant 1): when a connected viewer requests OS input, prompt the
/// local user on a distinct channel and return the consented subset (Allow ⇒ exactly what was asked,
/// Deny or a 90 s silence ⇒ empty = denied, fail-closed). The host clamps this again to grant ∩ policy.
#[async_trait::async_trait]
impl ras_core::ControlConsent for LocalConsent {
    async fn consent_to_control(
        &self,
        requested: &ras_core::policy::CapabilitySet,
    ) -> ras_core::policy::CapabilitySet {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *lock(&self.pending_control) = Some(tx);
        // Surface the human-readable requested caps so the panel can list what input is being asked for.
        let caps: Vec<String> = requested.iter().cloned().collect();
        let _ = self.app.emit("control-consent-request", caps);
        let allow = matches!(
            tokio::time::timeout(std::time::Duration::from_secs(90), rx).await,
            Ok(Ok(true))
        );
        *lock(&self.pending_control) = None;
        let _ = self.app.emit("control-consent-closed", ());
        if allow {
            requested.clone()
        } else {
            ras_core::policy::CapabilitySet::new()
        }
    }
}

/// Pointer position pushed to the overlay window (normalized 0..=65535).
#[derive(Clone, serde::Serialize)]
struct PointerPayload {
    x: u16,
    y: u16,
    visible: bool,
}

/// A short, log-safe rendering of a peer identity (first 8 hex of the public key). A public identity,
/// not a secret; kept terse for display.
fn short_id(id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    for b in id.iter().take(4) {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    s
}

/// Deliver the local user's Allow/Deny for a pending viewer (Invariant 1).
#[tauri::command]
fn respond_consent(state: State<'_, AppState>, allow: bool) {
    state.share.consent.respond(allow);
}

/// Deliver the local user's Allow/Deny for a pending **control-lease** request (Phase 3, Invariant 1).
#[tauri::command]
fn respond_control_consent(state: State<'_, AppState>, allow: bool) {
    state.share.consent.respond_control(allow);
}

/// Stop the whole share (drop the ticket, stop accepting, end any live viewer). Idempotent.
#[tauri::command]
fn stop_sharing(state: State<'_, AppState>) {
    if let Some(s) = lock(&state.share.session).take() {
        let _ = s.stop.send(true);
    }
}

/// Begin sharing this screen: bind an iroh endpoint, publish a ticket, and accept one viewer at a
/// time behind the local consent gate. Returns immediately; the ticket/status arrive as events.
/// Supported on macOS (hardware) + Linux/Windows (scap + OpenH264), ADR-063.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[tauri::command]
async fn start_sharing(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // Already sharing? No-op (the UI reflects the current ticket).
    if lock(&state.share.session).is_some() {
        return Ok(());
    }
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    *lock(&state.share.session) = Some(ShareSession { stop: stop_tx });
    let consent = state.share.consent.clone();
    tauri::async_runtime::spawn(async move {
        run_share(app, stop_rx, consent).await;
    });
    Ok(())
}

/// On platforms with no capture backend the Share role is unavailable. The Connect role still works.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[tauri::command]
async fn start_sharing(app: tauri::AppHandle, _state: State<'_, AppState>) -> Result<(), String> {
    let _ = app.emit(
        "share-status",
        "Screen sharing isn't available on this platform. You can still Connect to another machine.",
    );
    Err("screen sharing is not available on this platform".into())
}

/// Construct the platform's capture + encoder pair for a share session (ADR-063). macOS uses the
/// zero-copy hardware path; Linux/Windows use scap capture + the OpenH264 software encoder.
#[cfg(target_os = "macos")]
fn make_backends() -> (
    ras_media_macos::MacScreenCapture,
    ras_media_macos::VideoToolboxEncoder,
) {
    (
        ras_media_macos::MacScreenCapture::new(),
        ras_media_macos::VideoToolboxEncoder::new(),
    )
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn make_backends() -> (
    ras_media_scap::ScapCapture,
    ras_media_openh264::OpenH264Encoder,
) {
    (
        ras_media_scap::ScapCapture::new(),
        ras_media_openh264::OpenH264Encoder::new(),
    )
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn run_share(
    app: tauri::AppHandle,
    mut stop: tokio::sync::watch::Receiver<bool>,
    consent: Arc<LocalConsent>,
) {
    use ras_transport_iroh::Endpoint as IrohEndpoint;

    let _ = app.emit("share-active", true);
    let _ = app.emit(
        "share-status",
        "Starting… contacting a relay for a reachable address.",
    );
    let endpoint = match IrohEndpoint::bind().await {
        Ok(e) => Arc::new(e),
        Err(_) => {
            let _ = app.emit("share-status", "Failed to bind a network endpoint.");
            let _ = app.emit("share-active", false);
            return;
        }
    };
    endpoint.online().await;
    let _ = app.emit("share-ticket", endpoint.addr().to_ticket());
    let _ = app.emit("share-status", "Waiting for a viewer to connect…");

    // This host's application identity + grant issuer (Phase 2). Ephemeral per share in the MVP; the
    // issuer's key IS the host id, so the grants it mints verify against the same key the session-phase
    // validator checks (`GrantSessionValidator` uses `ctx.host_id`). A persistent identity + a
    // trusted-controller registry is a later step.
    use ras_core::grant::{LocalHostGrantIssuer, NonceCache, MAX_REQUEST_TTL_MS};
    use ras_core::identity::{KeyStore, SoftwareKeyStore};
    use ras_core::policy::phase3_default_policy;
    let host_ks = match SoftwareKeyStore::generate() {
        Ok(k) => k,
        Err(_) => {
            let _ = app.emit("share-status", "Failed to create a host identity.");
            let _ = app.emit("share-active", false);
            return;
        }
    };
    let host_id = host_ks.public_key();
    let host_endpoint_id = endpoint.id().0;
    let issuer = LocalHostGrantIssuer::new(host_ks, phase3_default_policy(), 1);
    // Shared replay cache for AccessRequest nonces across bootstrap connections (the accept loop
    // handles one connection at a time, so a `&mut` borrow suffices).
    let mut nonces = NonceCache::new(MAX_REQUEST_TTL_MS, 4096);

    loop {
        if *stop.borrow() {
            break;
        }
        let accepted = tokio::select! {
            _ = stop.changed() => { if *stop.borrow() { break } else { continue } },
            a = endpoint.accept() => a,
        };
        match accepted {
            // Route by negotiated ALPN: a bootstrap connection runs consent + issuance; a session
            // connection presents the resulting grant and streams frames.
            Ok(Some(session)) if session.is_bootstrap() => {
                handle_bootstrap(
                    &app,
                    session,
                    host_id,
                    host_endpoint_id,
                    &issuer,
                    &mut nonces,
                    &consent,
                )
                .await;
            }
            Ok(Some(session)) => {
                serve_one(&app, &endpoint, session, host_id, &consent, &mut stop).await;
            }
            Ok(None) => break, // endpoint closed
            Err(_) => continue,
        }
    }

    if let Some(ov) = app.get_webview_window("overlay") {
        let _ = ov.hide();
    }
    let _ = app.emit("share-viewer", false);
    let _ = app.emit("share-active", false);
    let _ = app.emit("share-status", "Sharing stopped.");
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
/// CGWindowIDs of our own windows (overlay + main/indicator) to keep out of the shared capture, so
/// the viewer never sees the remote-pointer overlay we draw for them (a feedback loop) or our local
/// control UI. Fail-safe: any window whose id can't be read is simply not excluded (capture is
/// unchanged), never fatal. macOS-only — the capture backend that consumes these is macOS-only.
#[cfg(target_os = "macos")]
fn host_excluded_windows(app: &tauri::AppHandle) -> Vec<ras_media::WindowId> {
    ["overlay", "main"]
        .iter()
        .filter_map(|label| app.get_webview_window(label))
        .filter_map(|w| w.ns_window().ok())
        .filter_map(|ns| {
            let obj = ns as *mut objc2::runtime::AnyObject;
            if obj.is_null() {
                return None;
            }
            // SAFETY: `obj` is a live NSWindow handed out by Tauri for this window; `windowNumber`
            // takes no arguments and returns the CGWindowID as an NSInteger.
            let number: isize = unsafe { objc2::msg_send![obj, windowNumber] };
            (number > 0).then_some(ras_media::WindowId(number as u64))
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn host_excluded_windows(_app: &tauri::AppHandle) -> Vec<ras_media::WindowId> {
    Vec::new()
}

/// Handle a **bootstrap-ALPN** connection (Phase 2): read the controller's `ClientHello` +
/// signed `AccessRequest`, validate it host-side (signature, endpoint sender-constraint, freshness,
/// replay, capability recognition), get local consent (Invariant 1), and — only on Allow — issue a
/// PASETO grant bound to this controller's endpoint. Every failure sends a content-free `Denied`
/// reason and returns; no session/pixels are involved here.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn handle_bootstrap(
    app: &tauri::AppHandle,
    session: ras_transport_iroh::Session,
    host_id: [u8; 32],
    host_endpoint_id: [u8; 32],
    issuer: &ras_core::grant::LocalHostGrantIssuer<ras_core::identity::SoftwareKeyStore>,
    nonces: &mut ras_core::grant::NonceCache,
    consent: &Arc<LocalConsent>,
) {
    use ras_core::grant::{
        fresh_id, validate_access_request, AccessRequest, SessionGrantIssuer, SessionParams,
        MAX_REQUEST_TTL_MS,
    };
    use ras_core::policy::phase3_default_policy;
    use ras_protocol::{AccessOutcome, BootstrapMsg, ErrorCode};

    // The controller's transport-authenticated endpoint — the identity the grant is bound to.
    let peer_endpoint = session.remote().0;
    let Ok(mut boot) = session.bootstrap().await else {
        return;
    };

    // Small helper: send a content-free denial and stop.
    macro_rules! deny {
        ($boot:expr, $code:expr) => {{
            let _ = $boot
                .send(BootstrapMsg::AccessDecision(AccessOutcome::Denied {
                    code: $code,
                }))
                .await;
            return;
        }};
    }

    // ClientHello → HostHello (advertise our identity + Tier 0).
    match boot.recv().await {
        Ok(BootstrapMsg::ClientHello { .. }) => {}
        _ => return,
    }
    if boot
        .send(BootstrapMsg::HostHello { host_id, tier: 0 })
        .await
        .is_err()
    {
        return;
    }

    // AccessRequest (opaque, signed) → decode + validate.
    let canonical = match boot.recv().await {
        Ok(BootstrapMsg::AccessRequest { canonical }) => canonical,
        _ => return,
    };
    let request = match AccessRequest::decode(&canonical) {
        Ok(r) => r,
        Err(code) => deny!(boot, code),
    };
    let now = now_ms();
    if let Err(code) = validate_access_request(&request, &host_id, &peer_endpoint, now, nonces) {
        deny!(boot, code);
    }

    // Local human consent (Invariant 1) — no grant is minted until the user clicks Allow.
    let _ = app.emit("share-status", "A viewer is requesting access…");
    if !consent.prompt(short_id(&request.controller_id)).await {
        deny!(boot, ErrorCode::ConsentDenied);
    }

    // Issue a sender-constrained grant for the consented (view-only) capabilities.
    let params = SessionParams {
        session_id: fresh_id().unwrap_or([0u8; 16]),
        host_endpoint_id,
        session_generation: 1,
        session_nonce: fresh_id().unwrap_or([0u8; 16]),
        issued_at: now,
        not_before: now,
        expires_at: now + MAX_REQUEST_TTL_MS,
    };
    match issuer
        .issue(&request, &phase3_default_policy(), &params)
        .await
    {
        Ok(grant) => {
            let _ = boot
                .send(BootstrapMsg::AccessDecision(AccessOutcome::Allowed {
                    grant,
                }))
                .await;
        }
        Err(e) => deny!(boot, e.code),
    }
}

async fn serve_one(
    app: &tauri::AppHandle,
    endpoint: &Arc<ras_transport_iroh::Endpoint>,
    session: ras_transport_iroh::Session,
    host_id: [u8; 32],
    consent: &Arc<LocalConsent>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) {
    use ras_core::{
        GrantSessionValidator, HostSession, HostSessionConfig, IrohSessionTransport,
        LifecycleEvent, StopReason,
    };
    use ras_media::MonitorId;

    let _ = app.emit("share-status", "A viewer is connecting…");

    // Phase-3 OS-input backend. Held concretely so we can feed it the shared display's bounds (below).
    // macOS: prompt for PostEvent access up front so that, by the time a viewer asks for control,
    // `input_permitted()` is true; otherwise the host refuses the lease fail-closed.
    #[cfg(target_os = "macos")]
    let input_sink = {
        let s = Arc::new(ras_input_macos::CgEventSink::new());
        let _ = s.request_access();
        s
    };
    // Linux: XTEST over x11rb (ADR-070). No permission prompt — it connects to $DISPLAY as the user
    // and is fail-closed when no X server is reachable (`input_permitted()` false ⇒ lease refused).
    #[cfg(target_os = "linux")]
    let input_sink = Arc::new(ras_input_linux::X11InputSink::new());
    // Windows: SendInput over windows-rs (ADR-071). In-session, no UIAccess (Inv 14).
    #[cfg(target_os = "windows")]
    let input_sink = Arc::new(ras_input_windows::SendInputSink::new());

    let (capture, encoder) = make_backends();
    let transport = Arc::new(IrohSessionTransport::new(endpoint.clone(), session));
    let host = HostSession::new(
        // Exclude our own overlay/indicator windows from the shared feed (privacy + no feedback loop).
        HostSessionConfig::new(MonitorId(0))
            .with_excluded_windows(host_excluded_windows(app))
            .with_host_id(host_id),
        transport,
        capture,
        encoder,
        // The session-phase gate: validate the PASETO grant the controller presents against the
        // endpoint iroh just authenticated (consent already happened in the bootstrap phase).
        Arc::new(GrantSessionValidator),
    )
    // The control-lease consent prompt (Invariant 1) — a second, input-specific Allow/Deny.
    .with_control_consent(consent.clone());
    // On macOS/Linux, feed the OS-input backend so a granted lease can actually inject (elsewhere, no
    // backend ⇒ control requests are refused fail-closed).
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    let host = host.with_input_sink(input_sink.clone());

    // `start()` runs the handshake, then blocks in the consent gate until Allow/Deny. Deny → Err.
    let mut events = match host.start().await {
        Ok(events) => events,
        Err(_) => {
            let _ = app.emit(
                "share-status",
                "Access denied. Waiting for the next viewer…",
            );
            return;
        }
    };

    // Approved: session is Active. Show the indicator + the pointer overlay.
    let _ = app.emit("share-status", "Viewer connected — REMOTE VIEWING ACTIVE.");
    let _ = app.emit("share-viewer", true);
    if let Some(ov) = app.get_webview_window("overlay") {
        let _ = ov.show();
    }

    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() {
                    host.stop(StopReason::UserRequested).await;
                    break;
                }
            }
            ev = events.recv() => match ev {
                Some(LifecycleEvent::RemotePointer { x, y, visible }) => {
                    if let Some(ov) = app.get_webview_window("overlay") {
                        let _ = ov.emit("pointer", PointerPayload { x, y, visible });
                    }
                }
                Some(LifecycleEvent::CaptureGeometry { x, y, width, height }) => {
                    // Place the pointer overlay over exactly the shared display (logical/point
                    // coordinates, which macOS global space and Tauri's Logical* share), so the
                    // normalized remote pointer lands on the right pixels — including on a secondary
                    // monitor. Best-effort: positioning failures leave the default overlay.
                    if let Some(ov) = app.get_webview_window("overlay") {
                        use tauri::{LogicalPosition, LogicalSize};
                        let _ = ov.set_position(LogicalPosition::new(x, y));
                        let _ = ov.set_size(LogicalSize::new(width, height));
                    }
                    // Feed the same bounds to the input backend so normalized input maps to the right
                    // pixels on the shared display (display id 0 in the single-display MVP).
                    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
                    input_sink.set_display_bounds(
                        0,
                        f64::from(x),
                        f64::from(y),
                        f64::from(width),
                        f64::from(height),
                    );
                }
                // Control-lease lifecycle (Phase 3), content-free. Surface it so the sharer's UI can
                // show that the viewer now has (or lost) OS-input control.
                Some(LifecycleEvent::ControlLeaseGranted { .. }) => {
                    let _ = app.emit("share-control", true);
                    let _ = app.emit(
                        "share-status",
                        "Viewer has REMOTE CONTROL of this screen.",
                    );
                }
                Some(LifecycleEvent::ControlLeaseEnded { .. }) => {
                    let _ = app.emit("share-control", false);
                    let _ = app.emit("share-status", "Viewer connected — REMOTE VIEWING ACTIVE.");
                }
                Some(LifecycleEvent::SessionEnded { .. })
                | Some(LifecycleEvent::Revoked { .. })
                | Some(LifecycleEvent::Disconnected { .. })
                | None => break,
                _ => {}
            },
        }
    }

    if let Some(ov) = app.get_webview_window("overlay") {
        let _ = ov.hide();
    }
    let _ = app.emit("share-viewer", false);
    let _ = app.emit(
        "share-status",
        "Viewer disconnected. Waiting for the next viewer…",
    );
}

// ─── Entrypoint ──────────────────────────────────────────────────────────────────────────────────

fn main() {
    // WebKitGTK's DMABUF renderer crashes or paints white artifacts on many Linux
    // GPU/driver/compositor combinations — a well-known Tauri-on-Linux failure, and worse here
    // because we use transparent overlay windows. Force the stable non-DMABUF path before the
    // WebView (and any GTK thread) initializes, unless the user has explicitly chosen a value.
    // Costs a little GPU compositing, never correctness. See issue #1.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    // App entrypoint: a failed event loop is an unrecoverable startup fault, not a request path.
    #[allow(clippy::expect_used)]
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            connect_to_host,
            disconnect,
            send_pointer,
            request_keyframe,
            request_control,
            is_controlling,
            input_pointer_move,
            input_pointer_button,
            input_pointer_wheel,
            input_key,
            input_set_lock_state,
            start_sharing,
            stop_sharing,
            respond_consent,
            respond_control_consent,
        ])
        .setup(|app| {
            let consent = Arc::new(LocalConsent::new(app.handle().clone()));
            app.manage(AppState {
                session: Mutex::new(None),
                share: ShareState {
                    session: Mutex::new(None),
                    consent,
                },
            });

            // The overlay must never steal input from the host user — make it click-through + hidden.
            if let Some(ov) = app.get_webview_window("overlay") {
                let _ = ov.set_ignore_cursor_events(true);
                let _ = ov.hide();
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Casual RAS");
}
