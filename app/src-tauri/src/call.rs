//! 1:1 **voice call** wiring for the app (ADR-104/106, L6a). Ties the `ras-core` [`CallDriver`] to the
//! real world: the ring rides the signed **signal** plane (`CallInvite`/`CallCancel`), the in-session
//! `ControlMsg::Call*` and the Opus mic audio ride a dedicated **call connection** over
//! [`ras_transport_iroh::CALL_ALPN`] (contacts-only, endpoint-authenticated), wrapped in the symmetric
//! [`IrohCallTransport`]. The webview drives it via the `call_*` commands and reacts to the content-free
//! `call-lifecycle` events. Camera video is a follow-up — this ships voice first.
//!
//! Security: a `CallInvite` authorizes nothing (Inv 9) and never auto-answers (Inv 1 — the local user
//! taps Accept). The call connection is contacts-only. Mic capture starts only once the call is Active
//! and is torn down on hangup/emergency-stop (Inv 4/12 — live-only, never recorded). No audio byte is
//! ever logged (Inv 8).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use ras_call::{CallLifecycleEvent, CallMedia};
use ras_core::call::{
    CallControlSink, CallDriver, CallEventSink, CallMediaController, CallSignalSink, CallTransport,
};
use ras_core::identity::{KeyStore, SoftwareKeyStore};
use ras_core::iroh_transport::IrohCallTransport;
use ras_identity::{ContactBook, ContactId};
use ras_media::audio::{
    AudioCaptureBackend, AudioCodec, AudioConfig, AudioEncoderBackend, EncodedAudio,
};
use ras_protocol::ControlMsg;
use ras_signal::SignalPayload;
use ras_transport_iroh::{Endpoint, Session};

use crate::{contacts_of, identity_seed, now_ms, parse_contact_id, AppState, AUDIO_MAGIC};

/// Opus/48 kHz mono voice — the config for a call's mic (matches `ras-mic`).
fn voice_config() -> AudioConfig {
    AudioConfig {
        codec: AudioCodec::Opus,
        sample_rate_hz: 48_000,
        channels: 1,
        frame_duration_us: 20_000,
        target_bitrate_bps: 64_000,
    }
}

fn media_kind(m: CallMedia) -> ras_protocol::CallMediaKind {
    if m.has_video() {
        ras_protocol::CallMediaKind::Video
    } else {
        ras_protocol::CallMediaKind::Voice
    }
}

// ── seams ────────────────────────────────────────────────────────────────────────────────────────

/// Emits content-free `call-lifecycle` events to the webview (Inv 8).
struct EventSink {
    app: AppHandle,
    peer: ContactId,
}
impl CallEventSink for EventSink {
    fn emit(&self, event: CallLifecycleEvent) {
        let (kind, media) = describe(event);
        let _ = self.app.emit(
            "call-lifecycle",
            serde_json::json!({
                "kind": kind,
                "contactId": crate::hex_id(self.peer.as_bytes()),
                "media": media,
            }),
        );
    }
}

fn describe(e: CallLifecycleEvent) -> (&'static str, Option<&'static str>) {
    let m = |media: CallMedia| if media.has_video() { "video" } else { "voice" };
    match e {
        CallLifecycleEvent::OutgoingRinging { media } => ("outgoing_ringing", Some(m(media))),
        CallLifecycleEvent::IncomingRinging { media } => ("incoming_ringing", Some(m(media))),
        CallLifecycleEvent::Connecting => ("connecting", None),
        CallLifecycleEvent::Active { media } => ("active", Some(m(media))),
        CallLifecycleEvent::Ended => ("ended", None),
        CallLifecycleEvent::Declined => ("declined", None),
        CallLifecycleEvent::Missed => ("missed", None),
        CallLifecycleEvent::Failed => ("failed", None),
        CallLifecycleEvent::RemoteMuteChanged { .. } => ("remote_mute_changed", None),
        _ => ("failed", None),
    }
}

/// Queues outbound `ControlMsg::Call*`; the control loop drains it onto the call control channel.
struct ControlSink(mpsc::UnboundedSender<ControlMsg>);
impl CallControlSink for ControlSink {
    fn send(&self, msg: ControlMsg) {
        let _ = self.0.send(msg);
    }
}

/// Sends the out-of-session ring (`CallInvite`/`CallCancel`) over the signed signal plane.
struct SignalSink {
    endpoint: Arc<Endpoint>,
    ks: Arc<dyn KeyStore>,
    peer: ContactId,
}
impl SignalSink {
    fn fire(&self, payload: SignalPayload) {
        let (ep, ks, bytes) = (
            self.endpoint.clone(),
            self.ks.clone(),
            *self.peer.as_bytes(),
        );
        tokio::spawn(async move {
            let Ok(peer) = iroh::EndpointId::from_bytes(&bytes) else {
                return;
            };
            let target = iroh::EndpointAddr::new(peer);
            let _ = ras_signal::net::send_signal(ep.iroh(), target, ks.as_ref(), &payload).await;
        });
    }
}
impl CallSignalSink for SignalSink {
    fn send_invite(&self, media: CallMedia) {
        self.fire(SignalPayload::CallInvite {
            issued_at: now_ms(),
            media: media_kind(media),
        });
    }
    fn send_cancel(&self) {
        self.fire(SignalPayload::CallCancel {
            issued_at: now_ms(),
        });
    }
}

/// Requests the media pumps be started/stopped. The supervisor task owns the actual pumps.
struct MediaCtl(mpsc::UnboundedSender<MediaCmd>);
enum MediaCmd {
    Start(CallMedia),
    Stop,
}
impl CallMediaController for MediaCtl {
    fn start(&self, media: CallMedia) {
        let _ = self.0.send(MediaCmd::Start(media));
    }
    fn stop(&self) {
        let _ = self.0.send(MediaCmd::Stop);
    }
}

// ── active-call state (one at a time) ──────────────────────────────────────────────────────────────

/// The single active call. Held in [`AppState::call`]; one at a time (one-active-call, ADR-104).
pub struct CallCtx {
    driver: Arc<AsyncMutex<CallDriver>>,
    peer: ContactId,
    /// The call transport, once the connection is up.
    transport: Arc<AsyncMutex<Option<Arc<IrohCallTransport>>>>,
    /// The driver's outbound `Call*` queue receiver — taken by the control loop when it starts.
    outbound_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<ControlMsg>>>,
    media: mpsc::UnboundedSender<MediaCmd>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
    mic_stop: Arc<AtomicBool>,
}

impl CallCtx {
    fn push_task(&self, h: JoinHandle<()>) {
        self.tasks.lock().unwrap_or_else(|e| e.into_inner()).push(h);
    }
    fn teardown(&self) {
        self.mic_stop.store(true, Ordering::SeqCst);
        let _ = self.media.send(MediaCmd::Stop);
        for t in self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            t.abort();
        }
    }
}

/// Build a fresh driver + its seams + plumbing for one call to `peer`.
fn build_ctx(app: &AppHandle, state: &AppState, peer: ContactId) -> Result<CallCtx, String> {
    let endpoint = state
        .endpoint
        .clone()
        .ok_or_else(|| "network endpoint unavailable".to_string())?;
    let seed = identity_seed(app).ok_or_else(|| "identity unavailable".to_string())?;
    let ks: Arc<dyn KeyStore> = Arc::new(SoftwareKeyStore::from_seed(seed));

    let (outbound, out_rx) = mpsc::unbounded_channel::<ControlMsg>();
    let (media_tx, media_rx) = mpsc::unbounded_channel::<MediaCmd>();

    let driver = Arc::new(AsyncMutex::new(CallDriver::new(
        Arc::new(ControlSink(outbound)),
        Arc::new(SignalSink { endpoint, ks, peer }),
        Arc::new(MediaCtl(media_tx.clone())),
        Arc::new(EventSink {
            app: app.clone(),
            peer,
        }),
    )));

    let transport: Arc<AsyncMutex<Option<Arc<IrohCallTransport>>>> =
        Arc::new(AsyncMutex::new(None));
    let mic_stop = Arc::new(AtomicBool::new(false));
    let sup = spawn_media_supervisor(
        app.clone(),
        Arc::clone(&transport),
        media_rx,
        Arc::clone(&mic_stop),
    );

    Ok(CallCtx {
        driver,
        peer,
        transport,
        outbound_rx: std::sync::Mutex::new(Some(out_rx)),
        media: media_tx,
        tasks: std::sync::Mutex::new(vec![sup]),
        mic_stop,
    })
}

/// Owns the mic egress thread + audio ingress task for the call's Active phase.
fn spawn_media_supervisor(
    app: AppHandle,
    transport: Arc<AsyncMutex<Option<Arc<IrohCallTransport>>>>,
    mut rx: mpsc::UnboundedReceiver<MediaCmd>,
    mic_stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut a_ingest: Option<JoinHandle<()>> = None;
        let mut v_ingest: Option<JoinHandle<()>> = None;
        while let Some(cmd) = rx.recv().await {
            match cmd {
                MediaCmd::Start(media) => {
                    let Some(tp) = transport.lock().await.clone() else {
                        continue;
                    };
                    // Audio ingress: peer mic → webview (always).
                    if a_ingest.is_none() {
                        if let Ok(mut src) = tp.audio_source().await {
                            let app2 = app.clone();
                            a_ingest = Some(tokio::spawn(async move {
                                while let Ok(pkt) = src.next().await {
                                    forward_call_audio(&app2, &pkt);
                                }
                            }));
                        }
                    }
                    // Audio egress: our mic → peer (always).
                    if let Ok(sink) = tp.audio_sink().await {
                        let stop = Arc::clone(&mic_stop);
                        stop.store(false, Ordering::SeqCst);
                        std::thread::spawn(move || mic_pump(sink, stop));
                    }
                    // Video (a video call): ingress everywhere (render the peer's camera), egress only
                    // where a camera backend exists (macOS for now).
                    if media.has_video() {
                        if v_ingest.is_none() {
                            if let Ok(mut src) = tp.video_source().await {
                                let app2 = app.clone();
                                v_ingest = Some(tokio::spawn(async move {
                                    forward_call_video(app2, &mut src).await;
                                }));
                            }
                        }
                        start_camera_egress(&app, &tp, Arc::clone(&mic_stop)).await;
                    }
                }
                MediaCmd::Stop => {
                    mic_stop.store(true, Ordering::SeqCst);
                    if let Some(h) = a_ingest.take() {
                        h.abort();
                    }
                    if let Some(h) = v_ingest.take() {
                        h.abort();
                    }
                }
            }
        }
    })
}

/// Camera egress (send our camera): macOS only for now (AVFoundation via `ras-camera`). A no-op on other
/// platforms — a video call there is receive-only for video (voice both ways).
#[cfg(target_os = "macos")]
async fn start_camera_egress(app: &AppHandle, tp: &Arc<IrohCallTransport>, stop: Arc<AtomicBool>) {
    if let Ok(sink) = tp.video_sink().await {
        stop.store(false, Ordering::SeqCst);
        let app = app.clone();
        std::thread::spawn(move || camera_pump(app, sink, stop));
    }
}
#[cfg(not(target_os = "macos"))]
async fn start_camera_egress(
    _app: &AppHandle,
    _tp: &Arc<IrohCallTransport>,
    _stop: Arc<AtomicBool>,
) {
}

/// Camera capture → VP9 encode → send loop (blocking; own thread). macOS only. Each encoded frame goes
/// two places: to the peer (`sink`) and to our own webview as a `call-selfvideo` self-view (reusing the
/// same encoded bytes — the camera is opened once, never twice). Never logs a pixel (Inv 8).
#[cfg(target_os = "macos")]
fn camera_pump(app: AppHandle, sink: Box<dyn ras_core::deps::VideoSinkDyn>, stop: Arc<AtomicBool>) {
    use ras_media::{CameraCaptureBackend, CameraOptions, VideoCodec, VideoEncoderBackend};
    let mut cam = ras_camera::NokhwaCameraCapture::new();
    // Declare VP9 so the negotiated StreamConfig (and thus the RCFG the webview reads) matches the VP9
    // bytes the encoder emits — otherwise the peer's decoder would be configured for the wrong codec.
    let cfg = match cam.start(&CameraOptions {
        target_width: 640,
        target_height: 480,
        target_fps: 24,
        codec: Some(VideoCodec::Vp9),
        ..CameraOptions::default()
    }) {
        Ok(c) => c,
        Err(_) => return,
    };
    // Software VP9 encoder (CpuBgra-native; WebCodecs decodes vp09). openh264 is Linux/Windows-only in
    // this app; macOS ships libvpx already (ras-media-vpx), and VP9 is universally WebCodecs-decodable.
    let mut enc = ras_media_vpx::VpxEncoder::new();
    if enc.configure(&cfg).is_err() {
        return;
    }
    let mut self_configured = false;
    while !stop.load(Ordering::SeqCst) {
        match cam.next_frame(std::time::Duration::from_millis(100)) {
            Ok(Some(frame)) => {
                if let Ok(Some(pkt)) = enc.encode(frame) {
                    // Self-view first (borrows), then hand the frame to the peer sink (moves).
                    emit_video_frame(&app, "call-selfvideo", &pkt, &mut self_configured);
                    sink.send_frame(pkt);
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    cam.stop();
}

/// Emit one encoded video frame to the webview over `event` as session-format blobs: a one-shot `RCFG`
/// config blob (on the first frame, so the JS decoder configures for the exact codec/size) then the
/// `RAS1` frame blob. Shared by the peer-camera render (`call-video`) and the local self-view
/// (`call-selfvideo`). Content-free at the log layer — never logs a pixel (Inv 8).
fn emit_video_frame(
    app: &AppHandle,
    event: &str,
    frame: &ras_media::EncodedFrame,
    configured: &mut bool,
) {
    if !*configured {
        let c = &frame.config;
        let codec = c.codec.webcodecs_string(c.width, c.height);
        let json = serde_json::json!({
            "codec": codec, "width": c.width, "height": c.height, "fps": c.fps,
        })
        .to_string();
        let mut blob = Vec::with_capacity(4 + json.len());
        blob.extend_from_slice(&crate::CONFIG_MAGIC.to_le_bytes());
        blob.extend_from_slice(json.as_bytes());
        let _ = app.emit(event, blob);
        *configured = true;
    }
    let _ = app.emit(event, ras_core::frame_channel::encode_frame_blob(frame));
}

/// Video ingress: read the peer's camera frames and forward them to the webview as `call-video` blobs —
/// one `RCFG` config blob (from the first frame's config) then `RAS1` frame blobs (the session format).
async fn forward_call_video(app: AppHandle, src: &mut Box<dyn ras_core::deps::VideoSourceDyn>) {
    let mut configured = false;
    while let Ok(ev) = src.next().await {
        let ras_transport_iroh::VideoEvent::Frame(frame) = ev else {
            continue; // a FrameDropped marker — nothing to render
        };
        emit_video_frame(&app, "call-video", &frame, &mut configured);
    }
}

/// The mic capture → Opus encode → send loop (blocking; runs on its own thread). Never logs a sample.
fn mic_pump(sink: Box<dyn ras_core::deps::AudioSink>, stop: Arc<AtomicBool>) {
    let mut cap = ras_mic::CpalMicCapture::new();
    let cfg = match cap.start(&voice_config()) {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut enc = ras_audio_opus::OpusEncoder::new();
    if enc.configure(&cfg).is_err() {
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match cap.next_chunk(std::time::Duration::from_millis(100)) {
            Ok(Some(chunk)) => {
                if let Ok(Some(pkt)) = enc.encode(chunk) {
                    sink.send_audio(pkt);
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    cap.stop();
}

/// Forward one received Opus packet to the webview as a self-describing RAU1 blob (reusing the session
/// audio format), over the `call-audio` event as raw bytes. The webview decodes with WebCodecs.
fn forward_call_audio(app: &AppHandle, pkt: &EncodedAudio) {
    let cfg = pkt.config;
    let opus = &pkt.data;
    let mut blob = Vec::with_capacity(4 + 4 + 1 + 8 + opus.len());
    blob.extend_from_slice(&AUDIO_MAGIC.to_le_bytes());
    blob.extend_from_slice(&cfg.sample_rate_hz.to_le_bytes());
    blob.push(cfg.channels);
    blob.extend_from_slice(&pkt.seq.to_le_bytes());
    blob.extend_from_slice(opus);
    let _ = app.emit("call-audio", blob);
}

/// Run the bidi control loop over the established transport: drain the driver's outbound `Call*` and
/// feed inbound `Call*` back into the driver. Ends on channel close / peer hangup.
async fn run_control_loop(
    driver: Arc<AsyncMutex<CallDriver>>,
    transport: Arc<IrohCallTransport>,
    mut outbound: mpsc::UnboundedReceiver<ControlMsg>,
) {
    let mut chan = match transport.control_channel().await {
        Ok(c) => c,
        Err(_) => return,
    };
    loop {
        tokio::select! {
            out = outbound.recv() => match out {
                Some(msg) => { let _ = chan.send(msg).await; }
                None => break,
            },
            inbound = chan.recv() => match inbound {
                Ok(msg) => {
                    let ends = matches!(msg, ControlMsg::CallHangup { .. } | ControlMsg::CallReject { .. });
                    driver.lock().await.on_control(&msg);
                    if ends { break; }
                }
                Err(_) => break,
            },
        }
    }
}

// ── commands ─────────────────────────────────────────────────────────────────────────────────────

/// Place an outbound call to a saved contact. Rings via the signal plane and dials the call connection.
#[tauri::command]
pub async fn call_place(
    app: AppHandle,
    state: State<'_, AppState>,
    contact_id: String,
    video: bool,
) -> Result<(), String> {
    let bytes = parse_contact_id(&contact_id)?;
    let peer = ContactId::from_bytes(bytes);
    if !contacts_of(&state)?.is_active_contact(&peer) {
        return Err("not a saved contact (or blocked)".into());
    }
    let driver = {
        let mut slot = state.call.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Err("already in a call".into());
        }
        let ctx = build_ctx(&app, &state, peer)?;
        let d = ctx.driver.clone();
        *slot = Some(ctx);
        d
    };
    // place_call: transitions Dialing + fires the CallInvite signal.
    let media = if video {
        CallMedia::Video
    } else {
        CallMedia::Voice
    };
    driver.lock().await.place_call(media);
    // Dial the call connection; when it's up, run the control loop (which delivers the callee's
    // CallAccept/Reject) and, on active, start media.
    dial_and_serve(app, peer);
    Ok(())
}

/// Accept the incoming ring. Opens the call control channel + starts media once active.
#[tauri::command]
pub async fn call_accept(
    app: AppHandle,
    state: State<'_, AppState>,
    video: bool,
) -> Result<(), String> {
    let accepted = if video {
        CallMedia::Video
    } else {
        CallMedia::Voice
    };
    // Extract handles under the sync lock (NO await while it's held), then release it.
    let (driver, transport, rx) = {
        let slot = state.call.lock().unwrap_or_else(|e| e.into_inner());
        let ctx = slot.as_ref().ok_or("no incoming call")?;
        let rx = ctx
            .outbound_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        (ctx.driver.clone(), Arc::clone(&ctx.transport), rx)
    };
    let Some(rx) = rx else {
        return Err("call already answered".into());
    };
    // The inbound connection must have arrived (it comes with/just after the invite).
    let Some(tp) = transport.lock().await.clone() else {
        return Err("call connection not established yet — try again".into());
    };
    // Start the bidi control loop, then accept (queues CallAccept onto it) and bring media up.
    let loop_task = tokio::spawn(run_control_loop(driver.clone(), tp, rx));
    {
        let slot = state.call.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ctx) = slot.as_ref() {
            ctx.push_task(loop_task);
        }
    }
    {
        let mut d = driver.lock().await;
        d.local_accept(accepted);
        d.media_connected();
    }
    let _ = app.emit("call-audio-active", ());
    Ok(())
}

/// Decline the incoming ring.
#[tauri::command]
pub async fn call_decline(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if let Some(driver) = current_driver(&state) {
        driver.lock().await.local_decline();
    }
    end_call(&app, &state);
    Ok(())
}

/// Hang up the active call.
#[tauri::command]
pub async fn call_hangup(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if let Some(driver) = current_driver(&state) {
        driver.lock().await.hangup();
    }
    end_call(&app, &state);
    Ok(())
}

/// Toggle mic/camera mute mid-call.
#[tauri::command]
pub async fn call_set_mute(
    state: State<'_, AppState>,
    audio_muted: bool,
    video_muted: bool,
) -> Result<(), String> {
    if let Some(driver) = current_driver(&state) {
        driver.lock().await.set_local_mute(audio_muted, video_muted);
    }
    Ok(())
}

// ── inbound (from the accept loop / signal handler) ────────────────────────────────────────────────

/// A `CallInvite` signal arrived from a contact → ring the local user (never auto-answer, Inv 1).
pub async fn handle_call_invite(app: &AppHandle, state: &AppState, peer: ContactId, video: bool) {
    if !contacts_of_ok(state, &peer) {
        return;
    }
    {
        if state
            .call
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return; // one active call — don't ring a second (the caller learns via CallBusy later)
        }
    }
    let Ok(ctx) = build_ctx(app, state, peer) else {
        return;
    };
    let driver = ctx.driver.clone();
    *state.call.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
    let media = if video {
        CallMedia::Video
    } else {
        CallMedia::Voice
    };
    driver.lock().await.on_invite(media);
}

/// A `CallCancel` signal arrived (the caller withdrew before we answered) → end the ring.
pub async fn handle_call_cancel(app: &AppHandle, state: &AppState, peer: ContactId) {
    let matches = state
        .call
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|c| c.peer == peer)
        .unwrap_or(false);
    if matches {
        end_call(app, state);
    }
}

/// An inbound call **connection** arrived (the caller dialed CALL_ALPN) → wrap it and stash it on the
/// (ringing) call context, so `call_accept` can open the control channel + start media. Contacts-only +
/// matching the ringing peer; otherwise the session is dropped (connection closes).
pub async fn handle_inbound_call(state: &AppState, session: Session) {
    let Some(endpoint) = state.endpoint.clone() else {
        return;
    };
    let peer = ContactId::from_bytes(session.remote().0);
    let tp_slot = {
        let slot = state.call.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(ctx) if ctx.peer == peer => Some(Arc::clone(&ctx.transport)),
            _ => None,
        }
    };
    if let Some(tp_slot) = tp_slot {
        *tp_slot.lock().await = Some(Arc::new(IrohCallTransport::new(endpoint, session)));
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────────

fn contacts_of_ok(state: &AppState, peer: &ContactId) -> bool {
    state
        .contacts
        .as_ref()
        .map(|b| b.is_active_contact(peer))
        .unwrap_or(false)
}

fn current_driver(state: &AppState) -> Option<Arc<AsyncMutex<CallDriver>>> {
    state
        .call
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|c| c.driver.clone())
}

fn end_call(app: &AppHandle, state: &AppState) {
    if let Some(ctx) = state.call.lock().unwrap_or_else(|e| e.into_inner()).take() {
        ctx.teardown();
    }
    let _ = app.emit("call-audio-inactive", ());
}

/// Caller side: dial CALL_ALPN, install the transport into the active call, run the control loop (which
/// delivers CallAccept/Reject), and on accept start media. Fire-and-forget; failures end the call.
fn dial_and_serve(app: AppHandle, peer: ContactId) {
    tokio::spawn(async move {
        let state = app.state::<AppState>();
        let Some(endpoint) = state.endpoint.clone() else {
            return;
        };
        let target = ras_core::transport::EndpointAddr::new(ras_core::transport::EndpointId(
            *peer.as_bytes(),
        ));
        let dialed = tokio::time::timeout(
            std::time::Duration::from_secs(45),
            endpoint.connect_call(&target),
        )
        .await;
        let session = match dialed {
            Ok(Ok(s)) => s,
            _ => return, // unreachable / timed out — the UI's ring timeout ends it
        };
        // Install the transport + start the control loop over it.
        let (driver, transport_slot, rx) = {
            let slot = state.call.lock().unwrap_or_else(|e| e.into_inner());
            match slot.as_ref() {
                Some(ctx) if ctx.peer == peer => (
                    ctx.driver.clone(),
                    Arc::clone(&ctx.transport),
                    ctx.outbound_rx
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take(),
                ),
                _ => return, // call was cancelled while dialing
            }
        };
        let Some(rx) = rx else { return };
        let tp = Arc::new(IrohCallTransport::new(endpoint, session));
        *transport_slot.lock().await = Some(Arc::clone(&tp));
        run_control_loop(driver, tp, rx).await;
    });
}
