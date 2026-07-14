//! Casual RAS controller — Tauri v2 shell (ADR-021/022, S3).
//!
//! Proves the controller **video path**: encoded H.264 access units are pushed to the webview over
//! a **binary** Tauri `Channel` (never JSON — CONTRIBUTING §5), where a WebCodecs `VideoDecoder`
//! decodes to a `VideoFrame` and renders to a canvas. For the MVP shell the frames come from a
//! **local mirror** (this Mac's screen via `ras-media-macos` capture→encode) so the whole path is
//! runnable glass-to-glass on one machine *before* the iroh transport lands (step 4 / M2). The
//! webview code is identical whichever source feeds it — the real remote source swaps in behind the
//! same channel.
//!
//! Each frame crosses the channel as one binary blob via the canonical `ras_core::frame_channel`
//! codec (the 24-byte `RAS1` header + Annex-B access unit) — the same contract the TS side parses,
//! so the header layout lives in exactly one place.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;

/// Handle to a running mirror: flags the capture/encode thread reads.
struct MirrorHandle {
    stop: Arc<AtomicBool>,
    force_keyframe: Arc<AtomicBool>,
}

#[derive(Default)]
struct MirrorState {
    inner: Mutex<Option<MirrorHandle>>,
}

/// Negotiated stream descriptor returned to the webview so it can configure `VideoDecoder`.
#[derive(Serialize)]
struct StreamCfgDto {
    /// WebCodecs codec string, e.g. `"avc1.4D4028"`.
    codec: String,
    width: u32,
    height: u32,
    fps: u32,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Stop any running mirror (idempotent).
#[tauri::command]
fn stop_mirror(state: State<'_, MirrorState>) {
    if let Some(h) = lock(&state.inner).take() {
        h.stop.store(true, Ordering::SeqCst);
    }
}

/// Ask the encoder to emit a fresh IDR on its next frame — used by the webview once its decoder is
/// configured (infinite-GOP means the lone startup keyframe may predate the decoder), and on any
/// decoder reset. Exercises the real forced-IDR path.
#[tauri::command]
fn request_keyframe(state: State<'_, MirrorState>) {
    if let Some(h) = lock(&state.inner).as_ref() {
        h.force_keyframe.store(true, Ordering::SeqCst);
    }
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn start_mirror(
    state: State<'_, MirrorState>,
    on_frame: Channel<InvokeResponseBody>,
) -> Result<StreamCfgDto, String> {
    use std::time::Duration;

    use ras_core::frame_channel::encode_frame_blob;
    use ras_media::{CaptureOptions, MonitorId, ScreenCaptureBackend, VideoEncoderBackend};
    use ras_protocol::KeyframeReason;
    use ras_media_macos::{MacScreenCapture, VideoToolboxEncoder};

    // Stop any prior mirror before starting a new one.
    if let Some(prev) = lock(&state.inner).take() {
        prev.stop.store(true, Ordering::SeqCst);
    }

    let mut capture = MacScreenCapture::new();
    let cfg = capture
        .start(&CaptureOptions {
            monitor: MonitorId(0),
            target_fps: 60,
            excluded_window_ids: vec![],
        })
        .map_err(|e| e.to_string())?;
    let mut encoder = VideoToolboxEncoder::new();
    encoder.configure(&cfg).map_err(|e| e.to_string())?;

    let dto = StreamCfgDto {
        codec: cfg.codec.webcodecs_string(cfg.width, cfg.height),
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let force_keyframe = Arc::new(AtomicBool::new(false));
    *lock(&state.inner) = Some(MirrorHandle {
        stop: stop.clone(),
        force_keyframe: force_keyframe.clone(),
    });

    // Capture→encode pull loop on its own thread; pushes each access unit over the binary channel.
    std::thread::spawn(move || {
        let poll = Duration::from_millis(100);
        while !stop.load(Ordering::SeqCst) {
            if force_keyframe.swap(false, Ordering::SeqCst) {
                encoder.request_keyframe(KeyframeReason::DecoderReset);
            }
            let frame = match capture.next_frame(poll) {
                Ok(Some(f)) => f,
                Ok(None) => continue, // static screen (SCK coalesces); keep polling
                Err(_) => break,      // stream stopped; a real controller would rebuild via start
            };
            match encoder.encode(frame) {
                Ok(Some(ef)) => {
                    // Canonical 24-byte RAS1 header + Annex-B payload (single source of truth).
                    if on_frame
                        .send(InvokeResponseBody::Raw(encode_frame_blob(&ef)))
                        .is_err()
                    {
                        break; // webview closed the channel
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
        capture.stop();
    });

    Ok(dto)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn start_mirror(
    _state: State<'_, MirrorState>,
    _on_frame: Channel<InvokeResponseBody>,
) -> Result<StreamCfgDto, String> {
    Err("the local mirror feed is macOS-only in this build".into())
}

fn main() {
    // App entrypoint: a failed event loop is an unrecoverable startup fault, not a request path.
    #[allow(clippy::expect_used)]
    tauri::Builder::default()
        .manage(MirrorState::default())
        .invoke_handler(tauri::generate_handler![
            start_mirror,
            stop_mirror,
            request_keyframe
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Casual RAS controller");
}
