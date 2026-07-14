//! Casual RAS controller — Tauri v2 shell (ADR-021/022, S3).
//!
//! Proves the controller **video path** *through the real session spine*: a `ras_core::HostSession`
//! (real `ras-media-macos` capture→encode) and a `ras_core::ControllerSession` are connected by the
//! in-memory **loopback transport**, so frames actually traverse handshake → authorize-gate
//! (`AllowAllValidator`, Phase-1 no-op seam) → grant → media pump → teardown, and keyframe requests
//! ride the control channel — exactly the path the loopback e2e tests exercise, but with the real
//! macOS backends and a live WebCodecs renderer. This is a **local mirror** (host + controller in one
//! process) so it runs glass-to-glass on one machine *before* the iroh transport lands (step 4 / M2);
//! the loopback transport swaps for the concrete iroh one behind the same `SessionTransport` seam.
//!
//! Frames reach the webview as `ras_core::frame_channel` blobs (24-byte `RAS1` header + Annex-B) over
//! a **binary** Tauri `Channel`; the negotiated stream config rides the same channel once as an
//! `RCFG` message. No pixels ever cross JSON IPC.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::sync::{Arc, Mutex};

use ras_core::frame_channel::encode_frame_blob;
use ras_core::{CoreError, ControllerSession, FrameSink, LifecycleStream, PushResult};
use ras_core::transport::Endpoint;
use ras_media::{EncodedFrame, StreamConfig};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;

/// Framing magic for the one-shot stream-config message (`"RCFG"` big-endian, sent little-endian).
/// Distinguishes the JSON config blob from the `RAS1` frame blobs on the same channel.
const CONFIG_MAGIC: u32 = u32::from_be_bytes(*b"RCFG");

#[derive(Default)]
struct AppState {
    /// A live **remote** controller session (the real alpha flow — dial a host's ticket over iroh).
    /// Platform-independent: the viewing side only decodes, so this works on macOS/Linux/Windows.
    session: Mutex<Option<ConnectedSession>>,
    /// The macOS-only **local self-mirror** demo (host + controller in one process over loopback),
    /// kept for on-device glass-to-glass testing without a second machine.
    #[cfg(target_os = "macos")]
    mirror: Mutex<Option<mac::Handles>>,
}

/// A connected remote session: the controller + the iroh endpoint that must outlive it.
struct ConnectedSession {
    // Endpoint kept alive for the session's lifetime.
    _endpoint: Arc<Endpoint>,
    controller: Arc<ControllerSession>,
    // Lifecycle events drained by the core tasks; held so the receiver isn't dropped.
    _events: LifecycleStream,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A [`FrameSink`] that forwards the decoded-stream config + each encoded access unit to the webview
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

/// Dial a host's **connection ticket** over iroh and render its screen. This is the real alpha flow:
/// a `ControllerSession` runs over `IrohSessionTransport` to a remote `ras-host`. Works on any
/// platform (the viewer only decodes).
#[tauri::command]
async fn connect_to_host(
    state: State<'_, AppState>,
    ticket: String,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    use ras_core::transport::EndpointAddr;
    use ras_core::{ControllerSessionConfig, IrohSessionTransport};

    // Tear down any prior remote session first.
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

/// End the live remote session (idempotent).
#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    use ras_core::StopReason;
    let session = lock(&state.session).take();
    if let Some(s) = session {
        s.controller.disconnect(StopReason::UserRequested).await;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::Arc;

    use ras_core::{ControllerSession, HostSession, LifecycleStream};
    use ras_media_macos::{MacScreenCapture, VideoToolboxEncoder};

    /// Live self-mirror handles, kept alive for the duration of a mirror (dropping tears it down).
    pub struct Handles {
        pub host: HostSession<MacScreenCapture, VideoToolboxEncoder>,
        pub controller: Arc<ControllerSession>,
        // Lifecycle event streams: kept alive so the sessions keep running (events are drained by the
        // core tasks; we only need to not drop the receivers).
        pub _host_events: LifecycleStream,
        pub _ctrl_events: LifecycleStream,
    }
}

/// Ask the encoder (via the control channel) to emit a fresh IDR — used by the webview once its
/// decoder is configured (infinite-GOP means the lone startup keyframe may predate the decoder) and
/// on any decoder reset. Works for whichever session is live (remote connect, or the mac mirror).
#[tauri::command]
async fn request_keyframe(state: State<'_, AppState>) -> Result<(), String> {
    use ras_protocol::KeyframeReason;

    let remote = lock(&state.session).as_ref().map(|s| s.controller.clone());
    if let Some(c) = remote {
        let _ = c.request_keyframe(KeyframeReason::DecoderReset).await;
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let mirror = lock(&state.mirror).as_ref().map(|h| h.controller.clone());
        if let Some(c) = mirror {
            let _ = c.request_keyframe(KeyframeReason::DecoderReset).await;
        }
    }
    Ok(())
}

/// Stop the macOS local self-mirror (idempotent). The remote flow uses [`disconnect`].
#[tauri::command]
async fn stop_mirror(state: State<'_, AppState>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use ras_core::StopReason;
        let handles = lock(&state.mirror).take();
        if let Some(h) = handles {
            h.controller.disconnect(StopReason::UserRequested).await;
            h.host.stop(StopReason::UserRequested).await;
        }
    }
    let _ = &state; // silence unused on non-macOS
    Ok(())
}

#[cfg(target_os = "macos")]
#[tauri::command]
async fn start_mirror(
    state: State<'_, AppState>,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    use ras_core::testkit::loopback_pair;
    use ras_core::transport::{EndpointAddr, EndpointId};
    use ras_core::{
        AllowAllValidator, ControllerSession, ControllerSessionConfig, HostSession,
        HostSessionConfig,
    };
    use ras_media::MonitorId;
    use ras_media_macos::{MacScreenCapture, VideoToolboxEncoder};

    // Tear down any prior mirror first.
    stop_mirror(state.clone()).await?;

    // Host and controller wired over the in-memory loopback transport (a one-machine demo; the real
    // two-machine flow is `connect_to_host` over iroh).
    let (host_tp, ctrl_tp) = loopback_pair();
    let host = HostSession::new(
        HostSessionConfig::new(MonitorId(0)),
        host_tp,
        MacScreenCapture::new(),
        VideoToolboxEncoder::new(),
        Arc::new(AllowAllValidator),
    );
    let target = EndpointAddr::new(EndpointId([0u8; 32]));
    let controller = Arc::new(ControllerSession::new(
        ControllerSessionConfig::new(target),
        ctrl_tp,
    ));

    // Host accepts + starts capturing; controller dials + negotiates the stream.
    let host_events = host.start().await.map_err(|e| e.to_string())?;
    let ctrl_events = controller.connect().await.map_err(|e| e.to_string())?;

    // Attach the renderer that forwards config + frames to the webview.
    controller
        .attach_renderer(Arc::new(ChannelFrameSink { channel: on_frame }))
        .await
        .map_err(|e| e.to_string())?;

    *lock(&state.mirror) = Some(mac::Handles {
        host,
        controller,
        _host_events: host_events,
        _ctrl_events: ctrl_events,
    });
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
async fn start_mirror(
    _state: State<'_, AppState>,
    _on_frame: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    Err("the local self-mirror is macOS-only; use Connect with a host ticket instead".into())
}

fn main() {
    // App entrypoint: a failed event loop is an unrecoverable startup fault, not a request path.
    #[allow(clippy::expect_used)]
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            connect_to_host,
            disconnect,
            start_mirror,
            stop_mirror,
            request_keyframe
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Casual RAS controller");
}
