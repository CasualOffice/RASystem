//! Casual RAS **host** — Tauri v2 GUI (M2 view-only + remote pointer).
//!
//! Two windows:
//! - **main** — the control panel: shows the connection ticket to share, an always-visible session
//!   indicator (Invariant 7), and a **Stop sharing** button.
//! - **overlay** — a transparent, click-through, always-on-top window covering the screen; it draws
//!   the connected controller's **remote pointer** ("look here") on top of everything, so the host
//!   user sees where the viewer is pointing. It never captures input (click-through), so it cannot
//!   interfere with the host user's own use of the machine.
//!
//! The screen is captured with `ras-media-macos` and served to one controller at a time over the
//! real iroh transport (`IrohSessionTransport`). Alpha honesty: consent is still the Phase-1
//! `AllowAllValidator` no-op seam (anyone with the ticket who reaches this endpoint is served) — real
//! approve/deny consent lands next. Stop is always available. macOS only for now.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use tauri::{Emitter, Manager};

/// Pointer position pushed to the overlay window (normalized 0..=65535).
#[derive(Clone, serde::Serialize)]
struct PointerPayload {
    x: u16,
    y: u16,
    visible: bool,
}

/// Shared "stop the current viewer now" signal (the Stop button / an emergency stop).
#[derive(Default)]
struct HostState {
    stop: std::sync::Arc<tokio::sync::Notify>,
}

/// Stop the current sharing session immediately (Invariant 7 — the stop control is always present).
/// Tears down the active viewer; the host keeps listening for the next one.
#[tauri::command]
fn stop_sharing(state: tauri::State<'_, HostState>) {
    state.stop.notify_waiters();
}

fn main() {
    // App entrypoint: a failed event loop is an unrecoverable startup fault, not a request path.
    #[allow(clippy::expect_used)]
    tauri::Builder::default()
        .manage(HostState::default())
        .invoke_handler(tauri::generate_handler![stop_sharing])
        .setup(|app| {
            // The overlay must never steal input from the host user — make it click-through.
            if let Some(ov) = app.get_webview_window("overlay") {
                let _ = ov.set_ignore_cursor_events(true);
                let _ = ov.hide();
            }
            let handle = app.handle().clone();
            let stop = app.state::<HostState>().stop.clone();
            tauri::async_runtime::spawn(async move {
                run_host(handle, stop).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Casual RAS host");
}

#[cfg(not(target_os = "macos"))]
async fn run_host(app: tauri::AppHandle, _stop: std::sync::Arc<tokio::sync::Notify>) {
    let _ = app.emit(
        "status",
        "No screen-capture backend for this platform yet (macOS only in the alpha).",
    );
}

#[cfg(target_os = "macos")]
async fn run_host(app: tauri::AppHandle, stop: std::sync::Arc<tokio::sync::Notify>) {
    use ras_transport_iroh::Endpoint;
    use std::sync::Arc;

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
        // Accept the next viewer, or stop if the app asked us to (stop-while-idle just re-arms).
        let accepted = tokio::select! {
            _ = stop.notified() => continue,
            a = endpoint.accept() => a,
        };
        match accepted {
            Ok(Some(session)) => serve_one(&app, &endpoint, session, &stop).await,
            Ok(None) => break, // endpoint closed
            Err(_) => continue,
        }
    }
}

#[cfg(target_os = "macos")]
async fn serve_one(
    app: &tauri::AppHandle,
    endpoint: &std::sync::Arc<ras_transport_iroh::Endpoint>,
    session: ras_transport_iroh::Session,
    stop: &std::sync::Arc<tokio::sync::Notify>,
) {
    use std::sync::Arc;

    use ras_core::{
        AllowAllValidator, HostSession, HostSessionConfig, IrohSessionTransport, LifecycleEvent,
        StopReason,
    };
    use ras_media::MonitorId;
    use ras_media_macos::{MacScreenCapture, VideoToolboxEncoder};

    let _ = app.emit("status", "Viewer connected — REMOTE VIEWING ACTIVE.");
    let _ = app.emit("connected", true);
    if let Some(ov) = app.get_webview_window("overlay") {
        let _ = ov.show();
    }

    let transport = Arc::new(IrohSessionTransport::new(endpoint.clone(), session));
    let host = HostSession::new(
        HostSessionConfig::new(MonitorId(0)),
        transport,
        MacScreenCapture::new(),
        VideoToolboxEncoder::new(),
        Arc::new(AllowAllValidator),
    );

    let mut events = match host.start().await {
        Ok(events) => events,
        Err(_) => {
            let _ = app.emit(
                "status",
                "Session failed to start (screen-recording permission?).",
            );
            return;
        }
    };

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
