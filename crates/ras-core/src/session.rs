//! Host & controller session orchestrators (design §5.2 / §5.3).
//!
//! Each owns the session state machine and drives it over the injected [`SessionTransport`]. The
//! media pump runs on its own task and **never touches the state machine or the control channel** —
//! it only pushes droppable frames, so a stalled video path can never freeze lifecycle, the control
//! channel, or `stop` (the load-bearing latency invariant).
//!
//! Reconciliation vs the design: `HostSession` is **generic** over the capture/encoder backends
//! rather than taking `Arc<dyn ..>` — those traits carry a GAT + a generic `encode`, so they are not
//! object-safe. Monomorphizing them is also the right call for the hot path. Transport, validator,
//! and sinks stay `dyn` (see [`crate::deps`]). Phase-1 target fps is a constant ([`HOST_TARGET_FPS`])
//! because `HostSessionConfig` carries no fps field yet.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::abr::LatencyFirstAbr;
use crate::deps::{
    ControlChannelDyn, DialTarget, FrameSink, GrantValidator, SessionAuthContext, SessionTransport,
};
use crate::event::{
    LifecycleEvent, LifecycleSink, LifecycleStream, QualitySample, SessionId, StopReason,
    StreamDescriptor,
};
use crate::{
    deps::GrantDecision, transition, AdaptiveBitrateController, CoreError, SessionEvent,
    SessionState, Transition,
};
use ras_media::{
    CaptureOptions, ColorSpace, MonitorId, ScreenCaptureBackend, StreamConfig, VideoCodec,
    VideoEncoderBackend, VideoTransportKind,
};
use ras_protocol::{ControlMsg, DecoderFeedback, ErrorCode, KeyframeReason, StreamConfigWire};
use ras_transport_iroh::{DropReason, EndpointAddr, EndpointId, VideoEvent};

/// Phase-1 host capture rate (no fps field on `HostSessionConfig` yet).
pub const HOST_TARGET_FPS: u32 = 30;

/// Bounded depth of the lifecycle event channel.
const LIFECYCLE_DEPTH: usize = 32;

/// How often the host samples connection health → runs ABR → emits a `ConnectionQuality` event.
/// Fast enough for a responsive quality badge and RTT-scale bitrate reactions, cheap enough to be
/// off the frame path entirely.
const STATS_TICK: Duration = Duration::from_millis(250);

/// How often the controller reports content-free decoder feedback (last decoded frame + drops +
/// decode latency) back to the host, feeding the host's ABR (design §2.3). Cold path.
const FEEDBACK_TICK: Duration = Duration::from_millis(200);

/// Bounded grace an emergency stop gives the control loop to flush its final `Bye` to the peer
/// before we stop waiting on it. Small by design: Invariant 4 requires the *local* stop to take
/// effect within 250 ms, and the local media halt is already done (via the stop flag) before we
/// wait here — so this budget only bounds the peer-notification courtesy, never the local halt.
const BYE_FLUSH_GRACE: Duration = Duration::from_millis(50);

/// Process-global session-id source (content-free, monotonic).
fn next_session_id() -> SessionId {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    SessionId(COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn transport_err(context: &'static str) -> CoreError {
    CoreError::fatal(ErrorCode::TransportError, context)
}

/// Apply an event to the shared state, returning the new state on a valid transition.
fn apply(state: &Mutex<SessionState>, event: SessionEvent) -> Option<SessionState> {
    let mut guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match transition(*guard, event) {
        Transition::To(next) => {
            *guard = next;
            Some(next)
        }
        Transition::Invalid => None,
    }
}

fn config_to_wire(c: &StreamConfig) -> StreamConfigWire {
    StreamConfigWire {
        codec: c.codec.webcodecs_string(c.width, c.height),
        width: c.width,
        height: c.height,
        fps: c.fps,
        target_bitrate_bps: c.target_bitrate_bps,
        color: match c.color {
            ColorSpace::Bt709Full => 1,
            // Bt709Limited and any future variant default to the limited-range tag.
            _ => 0,
        },
        video_transport: match c.video_transport {
            VideoTransportKind::PerFrameStream => 0,
            VideoTransportKind::DatagramFec => 1,
        },
    }
}

fn wire_to_config(w: &StreamConfigWire) -> StreamConfig {
    StreamConfig {
        codec: VideoCodec::H264AnnexB,
        width: w.width,
        height: w.height,
        fps: w.fps,
        target_bitrate_bps: w.target_bitrate_bps,
        color: if w.color == 1 {
            ColorSpace::Bt709Full
        } else {
            ColorSpace::Bt709Limited
        },
        video_transport: if w.video_transport == 1 {
            VideoTransportKind::DatagramFec
        } else {
            VideoTransportKind::PerFrameStream
        },
    }
}

// =============================================================================================
// Host
// =============================================================================================

/// Host-side session configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostSessionConfig {
    /// Single monitor in Phase 1; explicit so multi-monitor is additive.
    pub monitor: MonitorId,
    /// Negotiated ceiling; the encoder is capped to the measured path at runtime.
    pub max_bitrate_bps: u32,
    /// Reconnect window before `Suspended → Terminated`.
    pub reconnect_window: Duration,
}

impl HostSessionConfig {
    /// A reasonable single-monitor default.
    #[must_use]
    pub fn new(monitor: MonitorId) -> Self {
        Self {
            monitor,
            max_bitrate_bps: 8_000_000,
            reconnect_window: Duration::from_secs(10),
        }
    }
}

struct HostInner<C, E> {
    config: HostSessionConfig,
    transport: Arc<dyn SessionTransport>,
    validator: Arc<dyn GrantValidator>,
    backends: Mutex<Option<(C, E)>>,
    state: Mutex<SessionState>,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    /// ABR target the media thread applies via `set_bitrate` when it changes (lock-free hot path).
    target_bitrate: Arc<AtomicU32>,
    /// Frames actually handed to the transport since the last stats tick (delivered-fps signal).
    frames_sent: Arc<AtomicU32>,
    /// Latest decoder feedback from the controller (consumed by the ABR tick). `None` until the
    /// first report arrives.
    last_feedback: Mutex<Option<DecoderFeedback>>,
    /// Count of feedback reports received (observability / tests).
    feedback_count: AtomicU64,
    lifecycle: Mutex<Option<LifecycleSink>>,
    session_id: SessionId,
    media: Mutex<Option<std::thread::JoinHandle<()>>>,
    control_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stats_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Set in `start()`. Lets `stop`/`emergency_stop` ask the control loop to flush a final `Bye`
    /// to the controller (it owns the control channel). Best-effort — a wedged peer must never
    /// delay the local teardown.
    bye_tx: Mutex<Option<mpsc::Sender<ErrorCode>>>,
}

/// Host-side view-only session. Owns capture+encode+transmit on their own thread.
pub struct HostSession<C, E> {
    inner: Arc<HostInner<C, E>>,
}

impl<C, E> HostSession<C, E>
where
    C: ScreenCaptureBackend + Send + 'static,
    E: VideoEncoderBackend + Send + 'static,
{
    /// Build from injected backends. No I/O until [`Self::start`]. `validator` is a no-op in Phase 1
    /// (the seam is present so Phase 2 adds consent without changing this signature).
    #[must_use]
    pub fn new(
        config: HostSessionConfig,
        transport: Arc<dyn SessionTransport>,
        capture: C,
        encoder: E,
        validator: Arc<dyn GrantValidator>,
    ) -> Self {
        Self {
            inner: Arc::new(HostInner {
                config,
                transport,
                validator,
                backends: Mutex::new(Some((capture, encoder))),
                state: Mutex::new(SessionState::Created),
                stop: Arc::new(AtomicBool::new(false)),
                keyframe: Arc::new(AtomicBool::new(false)),
                target_bitrate: Arc::new(AtomicU32::new(0)),
                frames_sent: Arc::new(AtomicU32::new(0)),
                last_feedback: Mutex::new(None),
                feedback_count: AtomicU64::new(0),
                lifecycle: Mutex::new(None),
                session_id: next_session_id(),
                media: Mutex::new(None),
                control_task: Mutex::new(None),
                stats_task: Mutex::new(None),
                bye_tx: Mutex::new(None),
            }),
        }
    }

    /// Current session state.
    #[must_use]
    pub fn state(&self) -> SessionState {
        *self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The ABR's current target bitrate (bits/sec); `0` until the stream is negotiated. Advisory —
    /// exposed for status UIs and tests.
    #[must_use]
    pub fn current_bitrate_bps(&self) -> u32 {
        self.inner.target_bitrate.load(Ordering::Relaxed)
    }

    /// Number of decoder-feedback reports received from the controller. Advisory — for status UIs
    /// and tests.
    #[must_use]
    pub fn feedback_received(&self) -> u64 {
        self.inner.feedback_count.load(Ordering::Relaxed)
    }

    /// Accept a controller, negotiate, and start pushing frames. Returns the lifecycle stream. The
    /// media thread never touches the state machine or the control channel.
    pub async fn start(&self) -> Result<LifecycleStream, CoreError> {
        let inner = &self.inner;
        let (tx, rx) = mpsc::channel(LIFECYCLE_DEPTH);
        let sink = LifecycleSink(tx);
        *inner
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink.clone());

        apply(&inner.state, SessionEvent::Start);
        sink.emit(LifecycleEvent::Connecting);

        // Identity (accept side): the target is unused by the host.
        let target: DialTarget = EndpointAddr::new(EndpointId([0u8; 32]));
        let peer_identity = inner.transport.establish(&target).await?;
        let mut control = inner.transport.control_channel().await?;

        apply(&inner.state, SessionEvent::ControlUp);
        sink.emit(LifecycleEvent::SessionReady {
            session_id: inner.session_id,
        });

        // Handshake + Phase-1 no-op authorization.
        control
            .send(ControlMsg::Hello {
                protocol_version: 1,
            })
            .await?;
        let ctx = SessionAuthContext {
            peer_identity,
            access_request: bytes::Bytes::new(),
        };
        match inner.validator.authorize(&ctx).await? {
            GrantDecision::Authorized => {
                apply(&inner.state, SessionEvent::Authorized);
            }
            GrantDecision::Denied(code) => {
                apply(&inner.state, SessionEvent::Reject { code });
                sink.emit(LifecycleEvent::SessionEnded {
                    reason: StopReason::Error(code),
                });
                return Err(CoreError::fatal(code, "authorization denied"));
            }
            // Phase-1 no-op never returns these; treat as a denial rather than silently hanging.
            GrantDecision::NeedConsent | GrantDecision::Challenge(_) => {
                let code = ErrorCode::ConsentDenied;
                apply(&inner.state, SessionEvent::Reject { code });
                return Err(CoreError::fatal(
                    code,
                    "interactive consent not supported in phase 1",
                ));
            }
        }

        // Start capture, negotiate the stream config, announce it to the controller.
        let opts = CaptureOptions {
            monitor: inner.config.monitor,
            target_fps: HOST_TARGET_FPS,
            excluded_window_ids: vec![],
        };
        let (mut capture, mut encoder) = inner
            .backends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "backends already taken"))?;
        let config = capture
            .start(&opts)
            .map_err(|_| transport_err("capture start failed"))?;
        encoder
            .configure(&config)
            .map_err(|_| transport_err("encoder configure failed"))?;

        control
            .send(ControlMsg::StreamConfig(config_to_wire(&config)))
            .await?;

        apply(&inner.state, SessionEvent::StreamConfigured);
        sink.emit(LifecycleEvent::StreamConfigured {
            descriptor: StreamDescriptor::from_config(&config),
        });

        // Seed the ABR target with the negotiated bitrate before the media thread starts.
        inner
            .target_bitrate
            .store(config.target_bitrate_bps, Ordering::Relaxed);

        // Video egress + the media pump thread.
        let video_sink = inner.transport.video_sink().await?;
        let frame_interval = Duration::from_micros(1_000_000 / u64::from(config.fps.max(1)));
        let signals = MediaSignals {
            stop: inner.stop.clone(),
            keyframe: inner.keyframe.clone(),
            target_bitrate: inner.target_bitrate.clone(),
            frames_sent: inner.frames_sent.clone(),
            opts,
            frame_interval,
        };
        let handle = std::thread::Builder::new()
            .name("ras-host-media".into())
            .spawn(move || {
                media_pump(&mut capture, &mut encoder, video_sink.as_ref(), &signals);
            })
            .map_err(|_| CoreError::fatal(ErrorCode::Internal, "spawn media thread"))?;
        *inner
            .media
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);

        // Control reader: turns inbound KeyframeRequest into a forced IDR; Bye stops the session.
        // The `bye` channel lets a local teardown (graceful or emergency) flush a final Bye out the
        // control channel this loop owns.
        let (bye_tx, bye_rx) = mpsc::channel::<ErrorCode>(1);
        *inner
            .bye_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bye_tx);
        let ctrl_inner = inner.clone();
        let task = tokio::spawn(async move {
            host_control_loop(&mut control, &ctrl_inner, bye_rx).await;
        });
        *inner
            .control_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);

        // Stats/ABR tick: samples health → runs ABR → publishes the new bitrate + emits
        // ConnectionQuality. Off the frame path, so a stalled video never delays a quality update.
        let stats_inner = inner.clone();
        let stats = tokio::spawn(async move {
            host_stats_loop(&stats_inner, config).await;
        });
        *inner
            .stats_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stats);

        Ok(rx)
    }

    /// Cooperative stop. Applies `LocalStop`, tears down, flushes `SessionEnded`. Idempotent.
    /// Signals-and-returns: does not drain video. Unlike [`emergency_stop`](Self::emergency_stop),
    /// this is a *clean* close: it flushes a `Bye{NormalClosure}` so the controller ends promptly on
    /// `PeerClosed → Terminated` rather than mistaking the teardown for transport loss and waiting
    /// out the reconnect window. Peer notification is best-effort and time-bounded.
    pub async fn stop(&self, reason: StopReason) {
        let inner = &self.inner;
        if inner.stop.swap(true, Ordering::SeqCst) {
            return; // already stopped
        }
        apply(&inner.state, SessionEvent::LocalStop);
        if let Some(sink) = inner
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            sink.emit(LifecycleEvent::SessionEnded { reason });
        }
        // Flush a clean Bye and let the control loop exit on its own, bounded so a wedged peer can't
        // stall us; the media pump was already halted by the stop flag above.
        if let Some(tx) = inner
            .bye_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let _ = tx.try_send(ErrorCode::NormalClosure);
        }
        let control_task = inner
            .control_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(t) = control_task {
            let _ = tokio::time::timeout(BYE_FLUSH_GRACE, t).await;
        }
        if let Some(t) = inner
            .stats_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            t.abort();
        }
        if let Some(h) = inner
            .media
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = h.join();
        }
    }

    /// Emergency stop / mid-session revoke (Invariant 4). Overrides everything — grant, lease,
    /// in-flight video — and takes effect **locally and immediately**: setting the stop flag halts
    /// the media pump before its next `send_frame`, so no pixel leaves after this returns. Drives
    /// the audit-distinct `Revoke → Revoked` edge (never the graceful `Terminated`).
    ///
    /// Notifying the controller (`Bye{code}`) is *best-effort and bounded*: a wedged or vanished
    /// peer must never delay the local stop, so we give the control loop a short grace to flush and
    /// otherwise leave it — the controller will also see frames cease and its channel drop. First
    /// caller wins; later calls (and any concurrent graceful `stop`) are no-ops.
    pub async fn emergency_stop(&self, code: ErrorCode) {
        let inner = &self.inner;
        if inner.stop.swap(true, Ordering::SeqCst) {
            return; // already stopping/stopped — first caller wins, revoke can't be downgraded
        }
        // Audit-distinct terminal. Revoke overrides every non-terminal state.
        apply(&inner.state, SessionEvent::Revoke { code });
        if let Some(sink) = inner
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            sink.emit(LifecycleEvent::SessionEnded {
                reason: StopReason::Revoked { code },
            });
        }
        // Ask the control loop to flush a final Bye{code} to the peer (it owns the channel).
        if let Some(tx) = inner
            .bye_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let _ = tx.try_send(code);
        }
        // Bounded flush: let the control loop send its Bye and exit, but never wait on it beyond the
        // grace — the local media halt already happened via the stop flag above. Take the handle out
        // (dropping the MutexGuard) *before* awaiting, so no lock is held across `.await`.
        let control_task = inner
            .control_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(t) = control_task {
            // A timeout simply drops the join future; the stop already stands.
            let _ = tokio::time::timeout(BYE_FLUSH_GRACE, t).await;
        }
        if let Some(t) = inner
            .stats_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            t.abort();
        }
        if let Some(h) = inner
            .media
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = h.join();
        }
    }
}

/// Lock-free signals shared between the async orchestrator and the blocking media thread.
struct MediaSignals {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    frames_sent: Arc<AtomicU32>,
    opts: CaptureOptions,
    frame_interval: Duration,
}

/// The media pump. Pure loop: apply ABR bitrate → (force IDR if requested) → capture → encode →
/// droppable send. Rebuilds capture on a recoverable error; exits on the stop flag or a fatal error.
fn media_pump<C, E>(
    capture: &mut C,
    encoder: &mut E,
    sink: &dyn crate::deps::VideoSinkDyn,
    sig: &MediaSignals,
) where
    C: ScreenCaptureBackend,
    E: VideoEncoderBackend,
{
    let mut applied_bitrate = sig.target_bitrate.load(Ordering::Relaxed);
    while !sig.stop.load(Ordering::Relaxed) {
        // Retarget CBR mid-stream when ABR moved the target — keyframe-free (latency-first).
        let want = sig.target_bitrate.load(Ordering::Relaxed);
        if want != 0 && want != applied_bitrate && encoder.set_bitrate(want).is_ok() {
            applied_bitrate = want;
        }
        if sig.keyframe.swap(false, Ordering::Relaxed) {
            encoder.request_keyframe(KeyframeReason::UnrecoverableLoss);
        }
        // The captured frame borrows `capture` (GAT lifetime), so the borrow must end before any
        // rebuild call. We resolve to a rebuild/stop flag inside the match and act after it.
        let mut rebuild = false;
        match capture.next_frame(sig.frame_interval) {
            Ok(Some(frame)) => match encoder.encode(frame) {
                Ok(Some(ef)) => {
                    // Re-check stop between encode and send: an emergency stop set mid-pipeline must
                    // not let this last frame escape to the controller (Invariant 4).
                    if sig.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if sink.send_frame(ef) == ras_transport_iroh::SendOutcome::Sent {
                        sig.frames_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok(None) => {}
                Err(e) if e.recoverable => {}
                Err(_) => break,
            },
            Ok(None) => {}
            // Recoverable capture error (SCK restart / DXGI ACCESS_LOST): rebuild after the borrow.
            Err(e) if e.recoverable => rebuild = true,
            Err(_) => break,
        }
        if rebuild && capture.start(&sig.opts).is_err() {
            break;
        }
        std::thread::sleep(sig.frame_interval);
    }
    capture.stop();
}

/// The stats/ABR tick. Runs off the frame path: sample health → ABR → publish bitrate + optional
/// forced keyframe → emit `ConnectionQuality`. Exits on the stop flag.
async fn host_stats_loop<C, E>(inner: &HostInner<C, E>, config: StreamConfig) {
    let floor = (inner.config.max_bitrate_bps / 16).max(300_000);
    let mut abr = LatencyFirstAbr::new(
        floor,
        inner.config.max_bitrate_bps,
        config.target_bitrate_bps,
    );
    let tick_ms = STATS_TICK.as_millis() as u32;
    while !inner.stop.load(Ordering::Relaxed) {
        tokio::time::sleep(STATS_TICK).await;
        if inner.stop.load(Ordering::Relaxed) {
            break;
        }
        let health = inner.transport.health();
        // Consume the latest controller feedback (drops + last-decoded + any keyframe request) so
        // the ABR reacts to what the decoder actually sees, not just transport stats.
        let feedback = inner
            .last_feedback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let decision = abr.on_tick(&health, feedback);
        inner
            .target_bitrate
            .store(decision.target_bitrate_bps, Ordering::Relaxed);
        if decision.force_keyframe.is_some() {
            inner.keyframe.store(true, Ordering::Relaxed);
        }
        // delivered fps over the tick window (frames_sent is reset each tick).
        let sent = inner.frames_sent.swap(0, Ordering::Relaxed);
        let delivered_fps = u16::try_from(sent * 1000 / tick_ms.max(1)).unwrap_or(u16::MAX);
        let lifecycle = inner
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(sink) = lifecycle {
            sink.emit(LifecycleEvent::ConnectionQuality {
                sample: QualitySample::from_health(&health, delivered_fps),
            });
        }
    }
}

async fn host_control_loop<C, E>(
    control: &mut Box<dyn ControlChannelDyn>,
    inner: &HostInner<C, E>,
    mut bye_rx: mpsc::Receiver<ErrorCode>,
) {
    loop {
        tokio::select! {
            // A local teardown asked us to notify the controller and exit. Emergency revoke uses
            // this to flush `Bye{SessionRevoked}`; the media pump was already halted by the stop
            // flag, so this send is purely the peer-facing courtesy.
            signal = bye_rx.recv() => {
                if let Some(code) = signal {
                    let _ = control.send(ControlMsg::Bye { code }).await;
                }
                break;
            }
            msg = control.recv() => match msg {
            Ok(ControlMsg::KeyframeRequest(_)) => {
                inner.keyframe.store(true, Ordering::Relaxed);
            }
            Ok(ControlMsg::Feedback(fb)) => {
                *inner
                    .last_feedback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fb);
                inner.feedback_count.fetch_add(1, Ordering::Relaxed);
            }
            // Any Bye from the controller is a clean peer close — deliberately code-agnostic. A
            // controller cannot revoke the host, so a controller-claimed `SessionRevoked` is treated
            // as an ordinary close, never a privileged action (Invariants 1/15: never trust the
            // controller's claimed scope). Host-side revoke goes through `emergency_stop`.
            Ok(ControlMsg::Bye { .. }) => {
                inner.stop.store(true, Ordering::SeqCst);
                apply(&inner.state, SessionEvent::PeerClosed);
                if let Some(sink) = inner
                    .lifecycle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                {
                    sink.emit(LifecycleEvent::SessionEnded {
                        reason: StopReason::PeerClosed,
                    });
                }
                break;
            }
            Ok(_) => {}
            Err(_) => {
                // Controller vanished without a Bye. Stop the media thread and end the session.
                if !inner.stop.swap(true, Ordering::SeqCst) {
                    apply(
                        &inner.state,
                        SessionEvent::Fatal {
                            code: ErrorCode::TransportError,
                        },
                    );
                    if let Some(sink) = inner
                        .lifecycle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.emit(LifecycleEvent::SessionEnded {
                            reason: StopReason::Error(ErrorCode::TransportError),
                        });
                    }
                }
                break;
            }
            } // end `match msg`
        } // end tokio::select!
    }
}

// =============================================================================================
// Controller
// =============================================================================================

/// Controller-side session configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ControllerSessionConfig {
    /// How to reach the host. Phase 1: an `EndpointAddr`; Phase 2 replaces this with a validated
    /// connection ticket (additive, not a rename).
    pub target: DialTarget,
    /// Local decode/render buffer target (~10–50 ms; WebCodecs has no jitter buffer).
    pub target_buffer: Duration,
    /// How long to stay `Suspended` after a transport loss before giving up (→ `Terminated`). While
    /// suspended the controller freezes/blanks video but keeps its cursor + controls live.
    pub reconnect_window: Duration,
}

impl ControllerSessionConfig {
    /// Dial the given target with a small default buffer and a 10 s reconnect window.
    #[must_use]
    pub fn new(target: DialTarget) -> Self {
        Self {
            target,
            target_buffer: Duration::from_millis(30),
            reconnect_window: Duration::from_secs(10),
        }
    }
}

/// Command sent from the public API into the single control-owning task (avoids splitting the
/// bidi channel: one task both sends and receives via `select!`).
enum ControlCommand {
    Keyframe(KeyframeReason),
    Feedback(DecoderFeedback),
    Bye,
}

struct ControllerInner {
    config: ControllerSessionConfig,
    transport: Arc<dyn SessionTransport>,
    state: Mutex<SessionState>,
    stop: AtomicBool,
    lifecycle: Mutex<Option<LifecycleSink>>,
    renderer: Mutex<Option<Arc<dyn FrameSink>>>,
    stream_config: Mutex<Option<StreamConfig>>,
    command_tx: Mutex<Option<mpsc::Sender<ControlCommand>>>,
    /// Highest frame id delivered to the renderer (reported back as `last_decoded_frame`).
    last_decoded_frame: AtomicU64,
    /// Frames dropped since the last feedback report (reset each report).
    frames_dropped: AtomicU32,
    session_id: SessionId,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// Controller-side view-only session. Owns receive+decode-feed; the renderer attaches separately so
/// ingest runs before/independently of it (a stalled/absent renderer never blocks ingest).
pub struct ControllerSession {
    inner: Arc<ControllerInner>,
}

impl ControllerSession {
    /// Build from an injected transport. No I/O until [`Self::connect`].
    #[must_use]
    pub fn new(config: ControllerSessionConfig, transport: Arc<dyn SessionTransport>) -> Self {
        Self {
            inner: Arc::new(ControllerInner {
                config,
                transport,
                state: Mutex::new(SessionState::Created),
                stop: AtomicBool::new(false),
                lifecycle: Mutex::new(None),
                renderer: Mutex::new(None),
                stream_config: Mutex::new(None),
                command_tx: Mutex::new(None),
                last_decoded_frame: AtomicU64::new(0),
                frames_dropped: AtomicU32::new(0),
                session_id: next_session_id(),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Current session state.
    #[must_use]
    pub fn state(&self) -> SessionState {
        *self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Dial, handshake control, negotiate the stream. Returns the lifecycle stream. Does not wait
    /// for a renderer — video ingest starts immediately and drops frames until one attaches.
    pub async fn connect(&self) -> Result<LifecycleStream, CoreError> {
        let inner = &self.inner;
        let (tx, rx) = mpsc::channel(LIFECYCLE_DEPTH);
        let sink = LifecycleSink(tx);
        *inner
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink.clone());

        apply(&inner.state, SessionEvent::Start);
        sink.emit(LifecycleEvent::Connecting);

        let _peer = inner.transport.establish(&inner.config.target).await?;
        let mut control = inner.transport.control_channel().await?;

        apply(&inner.state, SessionEvent::ControlUp);
        sink.emit(LifecycleEvent::SessionReady {
            session_id: inner.session_id,
        });

        // Handshake: read control until the host announces the stream config.
        let config = loop {
            match control.recv().await? {
                ControlMsg::StreamConfig(wire) => break wire_to_config(&wire),
                ControlMsg::Bye { code } => {
                    return Err(CoreError::fatal(code, "peer closed during handshake"));
                }
                _ => {} // Hello and others: ignore in Phase 1
            }
        };
        *inner
            .stream_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(config);
        // If a renderer is already attached, configure it now.
        if let Some(r) = inner
            .renderer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let _ = r.configure(&config);
        }

        apply(&inner.state, SessionEvent::Authorized);
        apply(&inner.state, SessionEvent::StreamConfigured);
        sink.emit(LifecycleEvent::StreamConfigured {
            descriptor: StreamDescriptor::from_config(&config),
        });

        // Single task owns the control channel: sends commands + receives peer messages via select.
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        *inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cmd_tx);
        let ctrl_inner = inner.clone();
        let ctrl_task =
            tokio::spawn(
                async move { controller_control_loop(control, cmd_rx, &ctrl_inner).await },
            );

        // Video ingest task: pulls frames and pushes to whatever renderer is attached; drops
        // otherwise. Drops leading non-keyframes so the sink always starts on an IDR.
        let vid_inner = inner.clone();
        let vid_task = tokio::spawn(async move { controller_video_loop(&vid_inner).await });

        // Feedback task: periodically report content-free decoder stats to the host (feeds its ABR).
        let fb_inner = inner.clone();
        let fb_task = tokio::spawn(async move { controller_feedback_loop(&fb_inner).await });

        let mut tasks = inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.push(ctrl_task);
        tasks.push(vid_task);
        tasks.push(fb_task);

        Ok(rx)
    }

    /// Attach/replace the frame sink. Decoupled from `connect` so video can flow (and be dropped)
    /// before the canvas exists, and re-attach never stalls ingest.
    pub async fn attach_renderer(&self, renderer: Arc<dyn FrameSink>) -> Result<(), CoreError> {
        let inner = &self.inner;
        if let Some(cfg) = *inner
            .stream_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            renderer.configure(&cfg)?;
        }
        *inner
            .renderer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(renderer);
        // A freshly configured sink needs an IDR: ask the host.
        self.request_keyframe(KeyframeReason::DecoderReset).await
    }

    /// Detach without ending the session; ingest continues and drops frames at the sink boundary.
    pub async fn detach_renderer(&self) -> Result<(), CoreError> {
        *self
            .inner
            .renderer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }

    /// Ask the host for a fresh IDR (PLI-style) over the reliable control channel. Never blocks
    /// frames.
    pub async fn request_keyframe(&self, reason: KeyframeReason) -> Result<(), CoreError> {
        let tx = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match tx {
            Some(tx) => tx
                .send(ControlCommand::Keyframe(reason))
                .await
                .map_err(|_| transport_err("control task gone")),
            None => Err(transport_err("not connected")),
        }
    }

    /// Cooperative disconnect. Applies `LocalStop`, closes tasks, emits `SessionEnded`. Returns
    /// promptly even mid-decode.
    pub async fn disconnect(&self, reason: StopReason) {
        let inner = &self.inner;
        if inner.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        apply(&inner.state, SessionEvent::LocalStop);
        // Best-effort Bye to the host. Clone senders out from under the guard before awaiting.
        let tx = inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.send(ControlCommand::Bye).await;
        }
        let lifecycle = inner
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(sink) = lifecycle {
            sink.emit(LifecycleEvent::SessionEnded { reason });
        }
        for t in inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            t.abort();
        }
    }
}

async fn controller_control_loop(
    mut control: Box<dyn ControlChannelDyn>,
    mut cmd_rx: mpsc::Receiver<ControlCommand>,
    inner: &ControllerInner,
) {
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(ControlCommand::Keyframe(reason)) => {
                    let msg = ControlMsg::KeyframeRequest(ras_protocol::KeyframeRequest {
                        since_frame: 0,
                        reason,
                    });
                    if control.send(msg).await.is_err() { break; }
                }
                Some(ControlCommand::Feedback(fb)) => {
                    if control.send(ControlMsg::Feedback(fb)).await.is_err() { break; }
                }
                Some(ControlCommand::Bye) => {
                    // A controller leaving is a clean peer close, never a revoke — a controller
                    // cannot revoke the host (Invariants 1/13). Use the benign closure code.
                    let _ = control.send(ControlMsg::Bye { code: ErrorCode::NormalClosure }).await;
                    break;
                }
                None => break,
            },
            msg = control.recv() => match msg {
                Ok(ControlMsg::Bye { code }) => {
                    inner.stop.store(true, Ordering::SeqCst);
                    // A revoke Bye is the host emergency-stopping us: take the audit-distinct
                    // `Revoke → Revoked` edge, not the graceful `PeerClosed → Terminated`. The
                    // controller can never resume from this (Invariant 13); resume authority is the
                    // local user's alone.
                    let revoked = code == ErrorCode::SessionRevoked;
                    let (event, reason) = if revoked {
                        (SessionEvent::Revoke { code }, StopReason::Revoked { code })
                    } else {
                        (SessionEvent::PeerClosed, StopReason::PeerClosed)
                    };
                    apply(&inner.state, event);
                    if let Some(sink) = inner.lifecycle.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
                        sink.emit(LifecycleEvent::Disconnected { code });
                        sink.emit(LifecycleEvent::SessionEnded { reason });
                    }
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    // The reliable channel died WITHOUT a clean Bye ⇒ transport loss, not a peer
                    // close. Freeze video but keep the UI live: Active → Suspended, then honor the
                    // reconnect window before giving up. (Re-dial itself is deferred to the iroh
                    // transport; the state/UX contract is exercised now.)
                    if inner.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if apply(&inner.state, SessionEvent::TransportLost).is_none() {
                        break; // not in a suspendable state (e.g. still connecting) — just end
                    }
                    if let Some(sink) = inner
                        .lifecycle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.emit(LifecycleEvent::Suspended { since_ms: 0 });
                    }
                    tokio::time::sleep(inner.config.reconnect_window).await;
                    if inner.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    // Still suspended (Phase 1 has no re-dial) → window elapsed → Terminated.
                    if apply(&inner.state, SessionEvent::ReconnectWindowExpired).is_some() {
                        inner.stop.store(true, Ordering::SeqCst);
                        if let Some(sink) = inner
                            .lifecycle
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone()
                        {
                            sink.emit(LifecycleEvent::SessionEnded {
                                reason: StopReason::Timeout,
                            });
                        }
                    }
                    break;
                }
            },
        }
    }
}

/// What the controller does on a dropped-frame notification (design §10 / docs/10 §4 loss handling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LossAction {
    /// Benign: a newer frame superseded this one, so decoding continues uninterrupted.
    Ignore,
    /// A real gap the decoder can't bridge — freeze on the last good frame and request a fresh IDR.
    RecoverWithKeyframe,
}

/// Pure decision: only an unrecoverable gap warrants freeze-on-last-good + an IDR request; a stale
/// (superseded) drop is benign. Exhaustive match — a new [`DropReason`] variant is a compile error
/// here, never a silent default, so recovery policy can't drift as the transport grows.
fn loss_action(reason: DropReason) -> LossAction {
    match reason {
        DropReason::Stale => LossAction::Ignore,
        DropReason::FecUnrecoverable | DropReason::StreamReset | DropReason::MissingFragments => {
            LossAction::RecoverWithKeyframe
        }
    }
}

async fn controller_video_loop(inner: &ControllerInner) {
    let mut source = match inner.transport.video_source().await {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut seen_keyframe = false;
    while !inner.stop.load(Ordering::Relaxed) {
        match source.next().await {
            Ok(VideoEvent::Frame(ef)) => {
                if !seen_keyframe {
                    if !ef.is_keyframe {
                        continue; // wait for the first IDR before feeding the sink
                    }
                    seen_keyframe = true;
                }
                let fid = ef.frame_id;
                if let Some(r) = inner
                    .renderer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                {
                    let _ = r.push(ef);
                }
                inner.last_decoded_frame.store(fid, Ordering::Relaxed);
            }
            Ok(VideoEvent::FrameDropped { reason, .. }) => {
                inner.frames_dropped.fetch_add(1, Ordering::Relaxed);
                // A merely *stale* (superseded) frame needs no recovery — the newer frame decodes
                // fine, and requesting an IDR would spam the host for nothing. A real gap means the
                // decoder can no longer use subsequent P-frames: freeze on the last good frame (stop
                // feeding the renderer via the keyframe re-gate below) and request one fresh IDR;
                // resume only once it arrives (docs/10 §4 freeze-on-last-good).
                if loss_action(reason) == LossAction::RecoverWithKeyframe {
                    seen_keyframe = false; // re-gate: gate out P-frames until the requested IDR
                                           // Clone the sender out from under the std mutex before awaiting (guards !Send).
                    let tx = inner
                        .command_tx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if let Some(tx) = tx {
                        let _ = tx
                            .send(ControlCommand::Keyframe(KeyframeReason::UnrecoverableLoss))
                            .await;
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// Periodically report content-free decoder feedback to the host (cold path; feeds the host ABR).
async fn controller_feedback_loop(inner: &ControllerInner) {
    while !inner.stop.load(Ordering::Relaxed) {
        tokio::time::sleep(FEEDBACK_TICK).await;
        if inner.stop.load(Ordering::Relaxed) {
            break;
        }
        let fb = DecoderFeedback {
            last_decoded_frame: inner.last_decoded_frame.load(Ordering::Relaxed),
            frames_dropped: inner.frames_dropped.swap(0, Ordering::Relaxed),
            // Real decode latency comes from the WebCodecs worker later; 0 (unknown) in Phase 1.
            decode_latency_us: 0,
            // The immediate PLI path (KeyframeRequest on loss) handles resync; periodic feedback is
            // for ABR, so it does not itself request a keyframe.
            keyframe_request: None,
        };
        let tx = inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match tx {
            Some(tx) => {
                if tx.send(ControlCommand::Feedback(fb)).await.is_err() {
                    break;
                }
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod loss_tests {
    use super::{loss_action, LossAction};
    use ras_transport_iroh::DropReason;

    /// The recovery policy is exhaustive and correct: only a real gap freezes-and-recovers; a stale
    /// (superseded) drop is benign. The exhaustive `match` in `loss_action` makes a new `DropReason`
    /// a compile error, so this stays in lock-step with the transport.
    #[test]
    fn only_unrecoverable_gaps_request_recovery() {
        assert_eq!(loss_action(DropReason::Stale), LossAction::Ignore);
        assert_eq!(
            loss_action(DropReason::FecUnrecoverable),
            LossAction::RecoverWithKeyframe
        );
        assert_eq!(
            loss_action(DropReason::StreamReset),
            LossAction::RecoverWithKeyframe
        );
        assert_eq!(
            loss_action(DropReason::MissingFragments),
            LossAction::RecoverWithKeyframe
        );
    }
}
