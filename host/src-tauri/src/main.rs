//! Casual RAS **host** — Tauri v2 GUI (M2 view-only + remote pointer + local consent).
//!
//! Two windows:
//! - **main** — the control panel: the connection ticket to share, an always-visible session
//!   indicator (Invariant 7), a **Stop sharing** button, and the **Allow / Deny consent prompt** that
//!   appears when a viewer requests access.
//! - **overlay** — a transparent, click-through, always-on-top window covering the screen; it draws
//!   the connected controller's **remote pointer** ("look here"). Click-through, so it never captures
//!   the host user's own input — purely visual (ADR-061).
//!
//! Consent is real here: a viewer only becomes `Active` after the **local user clicks Allow**
//! (Invariant 1 — the local user is the final owner; a controller never self-authorizes). This app is
//! built with `ras-core`'s `default-features = false`, so the `insecure-no-auth` no-op validator is
//! not even linked. Screen capture is `ras-media-macos`; the stream is served over the real iroh
//! transport (`IrohSessionTransport`). macOS only for now.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::sync::{Arc, Mutex};

use ras_core::{CoreError, GrantDecision, GrantValidator, SessionAuthContext};
use ras_protocol::ErrorCode;
use tauri::{Emitter, Manager};
use tokio::sync::{oneshot, Notify};

/// Pointer position pushed to the overlay window (normalized 0..=65535).
#[derive(Clone, serde::Serialize)]
struct PointerPayload {
    x: u16,
    y: u16,
    visible: bool,
}

/// The local-consent gate. Implements `ras-core`'s [`GrantValidator`]: when a viewer requests access
/// it emits a `consent-request` to the panel and **blocks the session in `ControlEstablished` until
/// the local user answers** (or a timeout denies). No pixels flow before Allow. One viewer at a time,
/// so a single pending slot suffices.
struct LocalConsent {
    app: tauri::AppHandle,
    pending: Mutex<Option<oneshot::Sender<bool>>>,
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
        if let Some(tx) = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = tx.send(allow);
        }
    }
}

#[async_trait::async_trait]
impl GrantValidator for LocalConsent {
    async fn authorize(&self, ctx: &SessionAuthContext) -> Result<GrantDecision, CoreError> {
        let (tx, rx) = oneshot::channel();
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);

        // Ask the local user. The panel shows Allow/Deny with the peer's short identity.
        let _ = self
            .app
            .emit("consent-request", short_id(&ctx.peer_identity.0));

        // Wait for the click; a 90 s silence denies (fail-closed) so a session can't hang forever.
        let decision = match tokio::time::timeout(std::time::Duration::from_secs(90), rx).await {
            Ok(Ok(true)) => GrantDecision::Authorized,
            _ => GrantDecision::Denied(ErrorCode::ConsentDenied),
        };
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let _ = self.app.emit("consent-closed", ());
        Ok(decision)
    }
}

/// App state shared with the Tauri commands.
struct HostState {
    /// "Stop the current viewer now" (the Stop button / emergency stop).
    stop: Arc<Notify>,
    /// The local-consent gate (also reached by the `respond_consent` command).
    consent: Arc<LocalConsent>,
}

/// Stop the current sharing session immediately (Invariant 7 — the stop control is always present).
#[tauri::command]
fn stop_sharing(state: tauri::State<'_, HostState>) {
    state.stop.notify_waiters();
}

/// Deliver the local user's Allow/Deny for a pending viewer (Invariant 1).
#[tauri::command]
fn respond_consent(state: tauri::State<'_, HostState>, allow: bool) {
    state.consent.respond(allow);
}

fn main() {
    // App entrypoint: a failed event loop is an unrecoverable startup fault, not a request path.
    #[allow(clippy::expect_used)]
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![stop_sharing, respond_consent])
        .setup(|app| {
            let handle = app.handle().clone();
            let stop = Arc::new(Notify::new());
            let consent = Arc::new(LocalConsent::new(handle.clone()));
            app.manage(HostState {
                stop: stop.clone(),
                consent: consent.clone(),
            });

            // The overlay must never steal input from the host user — make it click-through + hidden.
            if let Some(ov) = app.get_webview_window("overlay") {
                let _ = ov.set_ignore_cursor_events(true);
                let _ = ov.hide();
            }

            tauri::async_runtime::spawn(async move {
                run_host(handle, stop, consent).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Casual RAS host");
}

/// A short, log-safe rendering of a peer identity (first 8 hex of the public key). It is a public
/// identity, not a secret; kept terse for display.
fn short_id(id: &[u8; 32]) -> String {
    let mut s = String::with_capacity(8);
    for b in id.iter().take(4) {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    s
}

#[cfg(not(target_os = "macos"))]
async fn run_host(app: tauri::AppHandle, _stop: Arc<Notify>, _consent: Arc<LocalConsent>) {
    let _ = app.emit(
        "status",
        "No screen-capture backend for this platform yet (macOS only in the alpha).",
    );
}

#[cfg(target_os = "macos")]
async fn run_host(app: tauri::AppHandle, stop: Arc<Notify>, consent: Arc<LocalConsent>) {
    use ras_transport_iroh::Endpoint;

    let _ = app.emit(
        "status",
        "Starting… contacting a relay for a reachable address.",
    );
    let endpoint = match Endpoint::bind().await {
        Ok(e) => Arc::new(e),
        Err(_) => {
            let _ = app.emit("status", "Failed to bind a network endpoint.");
            return;
        }
    };
    endpoint.online().await;
    let ticket = endpoint.addr().to_ticket();
    let _ = app.emit("ticket", ticket);
    let _ = app.emit("status", "Waiting for a viewer to connect…");

    loop {
        // Accept the next viewer, or re-arm if Stop was pressed while idle.
        let accepted = tokio::select! {
            _ = stop.notified() => continue,
            a = endpoint.accept() => a,
        };
        match accepted {
            Ok(Some(session)) => serve_one(&app, &endpoint, session, &stop, &consent).await,
            Ok(None) => break, // endpoint closed
            Err(_) => continue,
        }
    }
}

#[cfg(target_os = "macos")]
async fn serve_one(
    app: &tauri::AppHandle,
    endpoint: &Arc<ras_transport_iroh::Endpoint>,
    session: ras_transport_iroh::Session,
    stop: &Arc<Notify>,
    consent: &Arc<LocalConsent>,
) {
    use ras_core::{
        HostSession, HostSessionConfig, IrohSessionTransport, LifecycleEvent, StopReason,
    };
    use ras_media::MonitorId;
    use ras_media_macos::{MacScreenCapture, VideoToolboxEncoder};

    let _ = app.emit("status", "A viewer is requesting access…");

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
            let _ = app.emit("status", "Access denied. Waiting for the next viewer…");
            return;
        }
    };

    // Approved: session is Active. Show the indicator + the pointer overlay.
    let _ = app.emit("status", "Viewer connected — REMOTE VIEWING ACTIVE.");
    let _ = app.emit("connected", true);
    if let Some(ov) = app.get_webview_window("overlay") {
        let _ = ov.show();
    }

    loop {
        tokio::select! {
            _ = stop.notified() => {
                host.stop(StopReason::UserRequested).await;
                break;
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
    let _ = app.emit("connected", false);
    let _ = app.emit(
        "status",
        "Viewer disconnected. Waiting for the next viewer…",
    );
}
