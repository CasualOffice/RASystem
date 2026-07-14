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
use ras_core::{CoreError, ControllerSession, FrameSink, LifecycleStream, PushResult};
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
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
#[tauri::command]
async fn connect_to_host(
    state: State<'_, AppState>,
    ticket: String,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    use ras_core::transport::EndpointAddr;
    use ras_core::{ControllerSessionConfig, IrohSessionTransport};

    // Tear down any prior viewer session first.
    let _ = disconnect(state.clone()).await;

    let target = EndpointAddr::from_ticket(ticket.trim()).map_err(|e| e.to_string())?;
    let endpoint = Arc::new(Endpoint::bind().await.map_err(|e| e.to_string())?);
    // Dial the host: tries direct addrs + relay from the ticket, falls back to discovery-by-id.
    let session = endpoint.connect(&target).await.map_err(|e| e.to_string())?;
    let transport = Arc::new(IrohSessionTransport::new(endpoint.clone(), session));
    let controller = Arc::new(ControllerSession::new(
        ControllerSessionConfig::new(target),
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
}

impl LocalConsent {
    fn new(app: tauri::AppHandle) -> Self {
        Self {
            app,
            pending: Mutex::new(None),
        }
    }

    /// Deliver the local user's decision to a waiting `authorize`. Extra/late calls are no-ops.
    fn respond(&self, allow: bool) {
        if let Some(tx) = lock(&self.pending).take() {
            let _ = tx.send(allow);
        }
    }
}

#[async_trait::async_trait]
impl ras_core::GrantValidator for LocalConsent {
    async fn authorize(
        &self,
        ctx: &ras_core::SessionAuthContext,
    ) -> Result<ras_core::GrantDecision, CoreError> {
        use ras_core::GrantDecision;
        use ras_protocol::ErrorCode;

        let (tx, rx) = tokio::sync::oneshot::channel();
        *lock(&self.pending) = Some(tx);

        // Ask the local user. The panel shows Allow/Deny with the peer's short identity.
        let _ = self
            .app
            .emit("consent-request", short_id(&ctx.peer_identity.0));

        // Wait for the click; a 90 s silence denies (fail-closed) so a session can't hang forever.
        let decision = match tokio::time::timeout(std::time::Duration::from_secs(90), rx).await {
            Ok(Ok(true)) => GrantDecision::Authorized,
            _ => GrantDecision::Denied(ErrorCode::ConsentDenied),
        };
        *lock(&self.pending) = None;
        let _ = self.app.emit("consent-closed", ());
        Ok(decision)
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

/// Stop the whole share (drop the ticket, stop accepting, end any live viewer). Idempotent.
#[tauri::command]
fn stop_sharing(state: State<'_, AppState>) {
    if let Some(s) = lock(&state.share.session).take() {
        let _ = s.stop.send(true);
    }
}

/// Begin sharing this screen: bind an iroh endpoint, publish a ticket, and accept one viewer at a
/// time behind the local consent gate. Returns immediately; the ticket/status arrive as events.
#[cfg(target_os = "macos")]
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

/// On non-macOS the Share role isn't wired yet (no capture backend). The Connect role still works.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
async fn start_sharing(app: tauri::AppHandle, _state: State<'_, AppState>) -> Result<(), String> {
    let _ = app.emit(
        "share-status",
        "Screen sharing isn't available on this platform yet (macOS only in the alpha). You can still Connect to another machine.",
    );
    Err("screen sharing is macOS-only in the alpha".into())
}

#[cfg(target_os = "macos")]
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

    loop {
        if *stop.borrow() {
            break;
        }
        let accepted = tokio::select! {
            _ = stop.changed() => { if *stop.borrow() { break } else { continue } },
            a = endpoint.accept() => a,
        };
        match accepted {
            Ok(Some(session)) => serve_one(&app, &endpoint, session, &mut stop, &consent).await,
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

#[cfg(target_os = "macos")]
async fn serve_one(
    app: &tauri::AppHandle,
    endpoint: &Arc<ras_transport_iroh::Endpoint>,
    session: ras_transport_iroh::Session,
    stop: &mut tokio::sync::watch::Receiver<bool>,
    consent: &Arc<LocalConsent>,
) {
    use ras_core::{
        HostSession, HostSessionConfig, IrohSessionTransport, LifecycleEvent, StopReason,
    };
    use ras_media::MonitorId;
    use ras_media_macos::{MacScreenCapture, VideoToolboxEncoder};

    let _ = app.emit("share-status", "A viewer is requesting access…");

    let transport = Arc::new(IrohSessionTransport::new(endpoint.clone(), session));
    let host = HostSession::new(
        HostSessionConfig::new(MonitorId(0)),
        transport,
        MacScreenCapture::new(),
        VideoToolboxEncoder::new(),
        // Real consent: no frame flows until the local user clicks Allow (Invariant 1).
        consent.clone(),
    );

    // `start()` runs the handshake, then blocks in the consent gate until Allow/Deny. Deny → Err.
    let mut events = match host.start().await {
        Ok(events) => events,
        Err(_) => {
            let _ = app.emit("share-status", "Access denied. Waiting for the next viewer…");
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
    let _ = app.emit("share-status", "Viewer disconnected. Waiting for the next viewer…");
}

// ─── Entrypoint ──────────────────────────────────────────────────────────────────────────────────

fn main() {
    // App entrypoint: a failed event loop is an unrecoverable startup fault, not a request path.
    #[allow(clippy::expect_used)]
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            connect_to_host,
            disconnect,
            send_pointer,
            request_keyframe,
            start_sharing,
            stop_sharing,
            respond_consent,
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
