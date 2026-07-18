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
    AudioOutput, AuditSink, ControlChannelDyn, ControlConsent, CursorFrame, CursorObserver,
    CursorShape, CursorSink, DenyAllControl, DenyAllFileConsent, DialTarget, FileConsent,
    FileWriteSink, FrameSink, GrantValidator, SessionAuthContext, SessionTransport,
};
use crate::event::{
    LifecycleEvent, LifecycleSink, LifecycleStream, QualitySample, SessionId, StopReason,
    StreamDescriptor,
};
use crate::{
    deps::GrantDecision, transition, AdaptiveBitrateController, CoreError, SessionEvent,
    SessionState, Transition,
};
use ras_control::{ClipboardSink, LeaseManager, OsInputSink, LEASE_DEFAULT_TTL_MS};
use ras_media::{
    AudioCaptureBackend, AudioCodec, AudioConfig, AudioEncoderBackend, CaptureOptions, ColorSpace,
    MonitorId, ScreenCaptureBackend, StreamConfig, VideoCodec, VideoEncoderBackend,
    VideoTransportKind, WindowId,
};
use ras_policy::file::{authorize_file_push, DropCatalogue, FilePushError, FilePushRequest};
use ras_policy::{clipboard_push_allowed, CapabilitySet, ClipboardDirection, AUDIO_LISTEN};
use ras_protocol::{
    ControlMsg, DecoderFeedback, ErrorCode, InputEnvelope, KeyframeReason, StreamConfigWire,
};
use ras_transport_iroh::{DropReason, EndpointAddr, EndpointId, VideoEvent};

/// The grant window the host seeds the [`LeaseManager`] with (ms). MVP is **attended-only**: the real
/// grant `expires_at`/`session_generation` are not threaded through the authorize decision yet (there
/// is no mid-session grant-expiry enforcement — Phase-2 Q-GEN-STORE), so the operative bound on a
/// lease is its own ≤120 s TTL. This ceiling only prevents an absurd clamp; leases are far shorter.
const LEASE_GRANT_WINDOW_MS: u64 = 60 * 60 * 1000;

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

/// Wall-clock time in ms since the Unix epoch, for the authorization gate (`not_before`/`expires_at`).
/// The single point where ambient time enters the orchestrator; a pre-epoch clock saturates to 0
/// (fail-closed: everything looks "not yet valid" rather than silently valid).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
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
    /// The host's own windows (overlay / consent / indicator) to exclude from capture, by platform
    /// window id, so they never re-enter the shared feed (privacy + no capture-feedback loop). The
    /// UI supplies these; empty means capture the whole display.
    pub excluded_window_ids: Vec<WindowId>,
    /// This host's own identity (Ed25519 public key). Handed to the [`GrantValidator`] so it can check
    /// the presented grant's `host_id`/`issuer` match. `[0u8; 32]` for the `insecure-no-auth` path
    /// (the no-op validator ignores it).
    pub host_id: [u8; 32],
}

impl HostSessionConfig {
    /// A reasonable single-monitor default.
    #[must_use]
    pub fn new(monitor: MonitorId) -> Self {
        Self {
            monitor,
            max_bitrate_bps: 8_000_000,
            reconnect_window: Duration::from_secs(10),
            excluded_window_ids: Vec::new(),
            host_id: [0u8; 32],
        }
    }

    /// Set the host-owned windows to exclude from capture (overlay / consent / indicator).
    #[must_use]
    pub fn with_excluded_windows(mut self, ids: Vec<WindowId>) -> Self {
        self.excluded_window_ids = ids;
        self
    }

    /// Set this host's identity (the grant validator checks it against the presented grant).
    #[must_use]
    pub fn with_host_id(mut self, host_id: [u8; 32]) -> Self {
        self.host_id = host_id;
        self
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
    /// Set in `start()`. Proactive host→controller `ControlMsg`s (cursor shapes, chat) that the control
    /// loop forwards over the wire it owns. Fed by the cursor task and [`HostSession::send_chat`].
    outbound_tx: Mutex<Option<mpsc::Sender<ControlMsg>>>,
    /// Phase-3 OS-input backend (`ras-input-macos` in the app). `None` ⇒ this host cannot inject, so a
    /// control request is refused. Injected via [`HostSession::with_input_sink`].
    input_sink: Mutex<Option<Arc<dyn OsInputSink>>>,
    /// Phase-3 control-lease consent (Invariant 1). Defaults to [`DenyAllControl`] (fail-closed — no
    /// lease without a real local prompt). Injected via [`HostSession::with_control_consent`].
    control_consent: Mutex<Arc<dyn ControlConsent>>,
    /// The single OS-input lease + generation state (Inv 5/15, ADR-069). Seeded at `Active` from the
    /// authorized capability set; `None` before authorization.
    lease: Mutex<Option<LeaseManager>>,
    /// The authenticated peer endpoint, captured at `Active` — the lease holder identity (informational).
    peer_endpoint: Mutex<[u8; 32]>,
    /// The session's granted capability set, captured at `Active`. The authority for grant-level checks
    /// that are **not** lease-gated — e.g. clipboard push (`clipboard.write`/`read`, ADR-076, Inv 15).
    /// Empty before authorization (fail-closed).
    granted_caps: Mutex<CapabilitySet>,
    /// Phase-3 OS-clipboard write seam (ADR-076). `None` ⇒ this host cannot set its clipboard, so an
    /// inbound clipboard push is refused fail-closed. Injected via [`HostSession::with_clipboard_sink`].
    clipboard_sink: Mutex<Option<Arc<dyn ClipboardSink>>>,
    /// Audio pipeline (ADR-077): output-audio capture + encoder + egress sink. `None` ⇒ no audio.
    /// The pump only starts if this is wired **and** the grant carries `audio.listen` (Inv 15).
    /// Injected via [`HostSession::with_audio`].
    audio: Mutex<Option<AudioBackends>>,
    /// The audio pump thread handle (mirrors `media`); joined on teardown.
    audio_task: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Cursor-shape observer (ADR-073): watches the host OS cursor. `None` ⇒ the cursor channel is
    /// silent (the controller keeps its own generic pointer). Injected via
    /// [`HostSession::with_cursor_observer`]. Display data, never input (outside Inv 6).
    cursor: Mutex<Option<Box<dyn CursorObserver>>>,
    /// The cursor task handle (a tokio task); aborted on teardown.
    cursor_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Audit sink (Inv 10, ADR-088): records content-free security events into the tamper-evident
    /// journal, losslessly (unlike the advisory lifecycle stream). `None` ⇒ no auditing. Injected via
    /// [`HostSession::with_audit_sink`].
    audit: Mutex<Option<Arc<dyn AuditSink>>>,
    /// The vendor's file drop-target catalogue (ADR-086). `None` ⇒ no target exists, so every file offer
    /// is refused. Injected via [`HostSession::with_file_catalogue`].
    file_catalogue: Mutex<Option<DropCatalogue>>,
    /// Per-transfer local consent for a file push (Inv 1). Defaults to [`DenyAllFileConsent`] (fail-closed
    /// — no transfer without a real local Allow). Injected via [`HostSession::with_file_consent`].
    file_consent: Mutex<Arc<dyn FileConsent>>,
    /// Where accepted file bytes are written (ADR-090). `None` ⇒ this host cannot receive files, so an
    /// offer is refused. Injected via [`HostSession::with_file_write_sink`]. `O_NOFOLLOW` is the impl's.
    file_write_sink: Mutex<Option<Arc<dyn FileWriteSink>>>,
    /// The in-progress accepted transfer (one at a time), or `None`. Tracks bytes received vs the offered
    /// size so an over-run is aborted (no oversized/partial file).
    active_transfer: Mutex<Option<ActiveTransfer>>,
}

/// State of the one in-progress file transfer (ADR-090). The destination is already open on the
/// injected [`FileWriteSink`]; this only tracks progress against the offered size.
struct ActiveTransfer {
    received: u64,
    declared_size: u64,
}

/// The host's audio capture + encoder, moved into the audio pump thread when it starts. The egress
/// sink comes from the transport (`audio_sink()`), like video — the transport owns the wire path.
struct AudioBackends {
    capture: Box<dyn AudioCaptureBackend>,
    encoder: Box<dyn AudioEncoderBackend>,
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
                outbound_tx: Mutex::new(None),
                input_sink: Mutex::new(None),
                control_consent: Mutex::new(Arc::new(DenyAllControl)),
                lease: Mutex::new(None),
                peer_endpoint: Mutex::new([0u8; 32]),
                granted_caps: Mutex::new(CapabilitySet::new()),
                clipboard_sink: Mutex::new(None),
                audio: Mutex::new(None),
                audio_task: Mutex::new(None),
                cursor: Mutex::new(None),
                cursor_task: Mutex::new(None),
                audit: Mutex::new(None),
                file_catalogue: Mutex::new(None),
                file_consent: Mutex::new(Arc::new(DenyAllFileConsent)),
                file_write_sink: Mutex::new(None),
                active_transfer: Mutex::new(None),
            }),
        }
    }

    /// Inject the Phase-3 OS-input backend (e.g. `ras-input-macos`). Without one, this host refuses
    /// every control request (it cannot inject). Additive; call before [`Self::start`].
    #[must_use]
    pub fn with_input_sink(self, sink: Arc<dyn OsInputSink>) -> Self {
        *self
            .inner
            .input_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
        self
    }

    /// Inject the Phase-3 OS-clipboard write backend (ADR-076). Without one, an inbound clipboard push
    /// is refused fail-closed. The backend sets the OS clipboard and never pastes. Additive; call
    /// before [`Self::start`].
    #[must_use]
    pub fn with_clipboard_sink(self, sink: Arc<dyn ClipboardSink>) -> Self {
        *self
            .inner
            .clipboard_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
        self
    }

    /// Inject the audio pipeline (ADR-077): output-audio `capture` + `encoder`. The pump only runs if
    /// the session grant carries `audio.listen` (Inv 15) **and** the transport provides an audio sink
    /// (`audio_sink()`); otherwise no audio is captured or sent. Additive; call before [`Self::start`].
    #[must_use]
    pub fn with_audio(
        self,
        capture: Box<dyn AudioCaptureBackend>,
        encoder: Box<dyn AudioEncoderBackend>,
    ) -> Self {
        *self
            .inner
            .audio
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(AudioBackends { capture, encoder });
        self
    }

    /// Inject the cursor-shape observer (ADR-073): the host's OS cursor is streamed to the controller
    /// so it draws the real shape client-side at zero latency. Without one, the cursor channel stays
    /// silent (the controller keeps its generic pointer). Display data, never input. Additive; call
    /// before [`Self::start`].
    #[must_use]
    pub fn with_cursor_observer(self, observer: Box<dyn CursorObserver>) -> Self {
        *self
            .inner
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(observer);
        self
    }

    /// Inject the audit sink (Inv 10, ADR-088): the host records content-free security events into it
    /// losslessly as they happen. Without one, no auditing. Additive; call before [`Self::start`].
    #[must_use]
    pub fn with_audit_sink(self, audit: Arc<dyn AuditSink>) -> Self {
        *self
            .inner
            .audit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(audit);
        self
    }

    /// Inject the file drop-target catalogue (ADR-086). Without one, every file offer is refused (no
    /// target exists). Additive; call before [`Self::start`].
    #[must_use]
    pub fn with_file_catalogue(self, catalogue: DropCatalogue) -> Self {
        *self
            .inner
            .file_catalogue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(catalogue);
        self
    }

    /// Inject the per-transfer file-push consent prompt (Inv 1). Without one, the fail-closed
    /// [`DenyAllFileConsent`] refuses every push. Additive; call before [`Self::start`].
    #[must_use]
    pub fn with_file_consent(self, consent: Arc<dyn FileConsent>) -> Self {
        *self
            .inner
            .file_consent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = consent;
        self
    }

    /// Inject the file write backend (ADR-090): where an accepted transfer's bytes land (`O_NOFOLLOW`).
    /// Without one, every offer is refused (the host cannot receive). Additive; call before
    /// [`Self::start`].
    #[must_use]
    pub fn with_file_write_sink(self, sink: Arc<dyn FileWriteSink>) -> Self {
        *self
            .inner
            .file_write_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
        self
    }

    /// Inject the Phase-3 control-lease consent prompt (Invariant 1). Without one, control requests
    /// are denied by the fail-closed [`DenyAllControl`]. Additive; call before [`Self::start`].
    #[must_use]
    pub fn with_control_consent(self, consent: Arc<dyn ControlConsent>) -> Self {
        *self
            .inner
            .control_consent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = consent;
        self
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

        // Handshake: host speaks first (ADR-059), then reads the controller's AuthEnvelope (the
        // session grant) as the first control message before authorizing. `Bye` here is a clean
        // controller-side abort; anything else (or empty) leaves `access_request` empty and the real
        // validator will deny — the no-op validator ignores it.
        control
            .send(ControlMsg::Hello {
                protocol_version: 1,
            })
            .await?;
        let access_request = match control.recv().await? {
            ControlMsg::AuthEnvelope { payload } => payload,
            ControlMsg::Bye { code } => {
                apply(&inner.state, SessionEvent::Reject { code });
                return Err(CoreError::fatal(
                    code,
                    "controller aborted before authorization",
                ));
            }
            _ => bytes::Bytes::new(),
        };
        let ctx = SessionAuthContext {
            peer_identity,
            access_request,
            host_id: inner.config.host_id,
            now: now_ms(),
        };
        // The holder identity for any control lease this session issues (informational — the
        // generation + transport authentication are what bind input). Captured before `ctx` moves it.
        let holder = ctx.peer_identity.0;
        match inner.validator.authorize(&ctx).await? {
            GrantDecision::Authorized(caps) => {
                // The granted capability set is the ceiling for any OS-input lease (Inv 15). Seed the
                // per-message gate with it; a lease can only ever be a subset (Phase 3). Also retain
                // the full set for grant-level, non-lease checks (clipboard push, ADR-076).
                apply(&inner.state, SessionEvent::Authorized);
                // The local user allowed this connection (Inv 1) — the first auditable decision (Inv 10),
                // before `SessionStarted` (recorded once the stream is up).
                record_audit(inner, ras_audit::AuditEvent::ConsentGranted);
                *inner
                    .peer_endpoint
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = holder;
                *inner
                    .granted_caps
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = caps.clone();
                *inner
                    .lease
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LeaseManager::new(
                    caps,
                    now_ms().saturating_add(LEASE_GRANT_WINDOW_MS),
                    0,
                ));
            }
            GrantDecision::Denied(code) => {
                // A refused connection attempt is security-relevant — audit it (Inv 10), content-free.
                record_audit(inner, ras_audit::AuditEvent::ConsentDenied);
                apply(&inner.state, SessionEvent::Reject { code });
                sink.emit(LifecycleEvent::SessionEnded {
                    reason: StopReason::Error(code),
                });
                return Err(CoreError::fatal(code, "authorization denied"));
            }
            // Phase-1 no-op never returns these; treat as a denial rather than silently hanging.
            GrantDecision::NeedConsent | GrantDecision::Challenge(_) => {
                let code = ErrorCode::ConsentDenied;
                record_audit(inner, ras_audit::AuditEvent::ConsentDenied);
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
            excluded_window_ids: inner.config.excluded_window_ids.clone(),
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
        // Read the shared display's bounds + HiDPI descriptor while we still hold `capture` (it moves
        // into the media thread below), so the app can place its pointer overlay over exactly this
        // display and render at the right size/scale.
        let bounds = capture.captured_bounds();
        let display = capture.captured_display();
        encoder
            .configure(&config)
            .map_err(|_| transport_err("encoder configure failed"))?;

        control
            .send(ControlMsg::StreamConfig(config_to_wire(&config)))
            .await?;

        apply(&inner.state, SessionEvent::StreamConfigured);
        // Authorized + streaming: the first audit event of the session (Inv 10). Recorded losslessly at
        // the source, distinct from the advisory lifecycle stream.
        record_audit(inner, ras_audit::AuditEvent::SessionStarted);
        sink.emit(LifecycleEvent::StreamConfigured {
            descriptor: StreamDescriptor::from_config(&config),
        });
        if let Some(b) = bounds {
            sink.emit(LifecycleEvent::CaptureGeometry {
                x: b.x,
                y: b.y,
                width: b.width,
                height: b.height,
            });
        }
        if let Some(d) = display {
            sink.emit(LifecycleEvent::CaptureDisplay {
                id: d.id.0,
                logical_width: d.logical_width,
                logical_height: d.logical_height,
                pixel_width: d.pixel_width,
                pixel_height: d.pixel_height,
                scale_percent: d.scale_percent,
                primary: d.primary,
            });
        }

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

        // Audio pump (ADR-077): host→controller output audio, gated on `audio.listen` (Inv 15 — the
        // host streams sound only if the grant permits it). Starts only if audio backends are wired,
        // the capability is granted, AND the transport carries an audio plane; otherwise the session
        // is silent. Runs on its own thread, stopped by the shared `stop` flag and joined on teardown.
        let audio_allowed = inner
            .granted_caps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(AUDIO_LISTEN);
        if audio_allowed {
            if let Ok(audio_sink) = inner.transport.audio_sink().await {
                maybe_start_audio(inner, audio_sink);
            }
        }

        // Control reader: turns inbound KeyframeRequest into a forced IDR; Bye stops the session.
        // The `bye` channel lets a local teardown (graceful or emergency) flush a final Bye out the
        // control channel this loop owns.
        let (bye_tx, bye_rx) = mpsc::channel::<ErrorCode>(1);
        *inner
            .bye_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bye_tx);

        // Host outbound-control channel: proactive host→controller `ControlMsg`s that the control loop
        // (which owns the wire) forwards. Fed by the cursor task (ADR-073) and by `send_chat` (ADR-082).
        // Bounded + drop-newest at the source: these are advisory and must never backpressure control.
        let (outbound_tx, outbound_rx) = mpsc::channel::<ControlMsg>(OUTBOUND_QUEUE_DEPTH);
        *inner
            .outbound_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outbound_tx.clone());
        // Cursor-shape task (ADR-073): if an observer is wired, watch the OS cursor and push ready
        // `ControlMsg`s (fresh shape / cache reference) into the outbound channel. Silent if no observer.
        maybe_start_cursor(inner, outbound_tx);

        let ctrl_inner = inner.clone();
        let task = tokio::spawn(async move {
            host_control_loop(&mut control, &ctrl_inner, bye_rx, outbound_rx).await;
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
        // Release any held keys/buttons and revoke the lease on a clean teardown too (Inv 4 cleanup).
        release_input(inner);
        apply(&inner.state, SessionEvent::LocalStop);
        record_audit(
            inner,
            ras_audit::AuditEvent::SessionEnded {
                code: ErrorCode::NormalClosure,
            },
        );
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
        join_audio(inner);
        abort_cursor(inner);
        abort_file_transfer(inner);
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
        // Invariant 4: revoke the lease + flush held keys before anything else — a post-stop input
        // event is now stale at the gate (generation bumped) and every held key is released.
        release_input(inner);
        // Audit-distinct terminal. Revoke overrides every non-terminal state.
        apply(&inner.state, SessionEvent::Revoke { code });
        record_audit(inner, ras_audit::AuditEvent::EmergencyStop { code });
        record_audit(inner, ras_audit::AuditEvent::SessionEnded { code });
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
        join_audio(inner);
        abort_cursor(inner);
        abort_file_transfer(inner);
    }

    /// Send an in-session chat message to the connected controller (ADR-082). Base session comms (no
    /// capability). The text is a secret and is never logged (Inv 8); an oversized message
    /// (> `MAX_CHAT_BYTES`) is dropped rather than sent. Best-effort — no-op before `start` / after
    /// teardown, or if the advisory outbound queue is full.
    pub fn send_chat(&self, text: String) {
        if text.len() > ras_protocol::MAX_CHAT_BYTES {
            return;
        }
        let tx = self
            .inner
            .outbound_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ControlMsg::ChatMessage {
                text: ras_protocol::Redacted(text),
            });
        }
    }

    /// Push the host's clipboard text to the controller (ADR-076, host→controller direction). Gated
    /// host-side on `clipboard.read` against the session grant (Inv 15 — the controller must have been
    /// authorized to read the host's clipboard); refused fail-closed otherwise, and the controller only
    /// **sets** its OS clipboard, never pastes (no-auto-paste rule). The text is a secret and is never
    /// logged (Inv 8); an oversized push (> `MAX_CLIPBOARD_BYTES`) or an ungranted one is dropped, not
    /// sent. Best-effort; audited.
    pub fn send_clipboard_text(&self, text: String) {
        let inner = &self.inner;
        if text.len() > ras_protocol::MAX_CLIPBOARD_BYTES {
            return;
        }
        let granted = inner
            .granted_caps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !clipboard_push_allowed(ClipboardDirection::HostToController, &granted) {
            record_audit(
                inner,
                ras_audit::AuditEvent::ClipboardRejected {
                    code: ErrorCode::CapabilityDenied,
                },
            );
            return;
        }
        let tx = inner
            .outbound_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let len = text.len();
            if tx
                .try_send(ControlMsg::ClipboardText {
                    text: ras_protocol::Redacted(text),
                })
                .is_ok()
            {
                record_audit(
                    inner,
                    ras_audit::AuditEvent::ClipboardApplied {
                        len: u32::try_from(len).unwrap_or(u32::MAX),
                    },
                );
            }
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

/// Default requested audio config: Opus, 48 kHz stereo, 20 ms frames (ADR-077). The capture backend
/// may negotiate its own; the encoder is configured with whatever `start` returns.
fn default_audio_config() -> AudioConfig {
    AudioConfig {
        codec: AudioCodec::Opus,
        sample_rate_hz: 48_000,
        channels: 2,
        frame_duration_us: 20_000,
        target_bitrate_bps: 96_000,
    }
}

/// Start the audio pump given the transport's egress `sink` (the caller has already checked the
/// `audio.listen` gate, Inv 15). Consumes the injected backends (moves them into the thread). No-op if
/// no audio backends are wired. The thread stops on the shared `stop` flag and is joined on teardown.
fn maybe_start_audio<C, E>(inner: &HostInner<C, E>, sink: Box<dyn crate::deps::AudioSink>) {
    let backends = inner
        .audio
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(AudioBackends {
        mut capture,
        mut encoder,
    }) = backends
    else {
        return;
    };
    let stop = inner.stop.clone();
    let requested = default_audio_config();
    let handle = std::thread::Builder::new()
        .name("ras-host-audio".into())
        .spawn(move || {
            audio_pump(
                capture.as_mut(),
                encoder.as_mut(),
                sink.as_ref(),
                &stop,
                &requested,
            );
        });
    if let Ok(h) = handle {
        // The output-audio stream is now live (`audio.listen` was granted + gated) — audit it (Inv 10).
        record_audit(inner, ras_audit::AuditEvent::AudioStarted);
        *inner
            .audio_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(h);
    }
}

/// The audio capture→encode→egress loop (mirrors [`media_pump`], simpler: no ABR/keyframes). Starts
/// capture, configures the encoder to the negotiated config, then pumps until `stop`. Re-checks `stop`
/// between encode and send so a teardown never lets one last packet escape (Invariant 4, audio path).
fn audio_pump(
    capture: &mut dyn AudioCaptureBackend,
    encoder: &mut dyn AudioEncoderBackend,
    sink: &dyn crate::deps::AudioSink,
    stop: &AtomicBool,
    requested: &AudioConfig,
) {
    let config = match capture.start(requested) {
        Ok(c) => c,
        Err(_) => return,
    };
    if encoder.configure(&config).is_err() {
        capture.stop();
        return;
    }
    // Poll cadence: long enough to check `stop` regularly, short enough to keep latency low on silence.
    let timeout = Duration::from_millis(100);
    while !stop.load(Ordering::Relaxed) {
        match capture.next_chunk(timeout) {
            Ok(Some(chunk)) => match encoder.encode(chunk) {
                Ok(Some(pkt)) => {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    sink.send_audio(pkt);
                }
                Ok(None) => {}
                Err(e) if e.recoverable => {}
                Err(_) => break,
            },
            Ok(None) => {}
            Err(e) if e.recoverable => {
                if capture.start(requested).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    capture.stop();
}

/// Join the audio pump thread if one is running (teardown; mirrors the `media` join). Idempotent.
fn join_audio<C, E>(inner: &HostInner<C, E>) {
    if let Some(h) = inner
        .audio_task
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        let _ = h.join();
        // The pump was running and is now stopped — audit the end of the audio stream (Inv 10).
        record_audit(inner, ras_audit::AuditEvent::AudioStopped);
    }
}

/// Abort the cursor task if one is running (teardown, ADR-073). Idempotent. Aborting is fine — the
/// cursor channel is advisory display data with no cleanup obligation (unlike input, which must
/// release held keys); the control loop that consumed it has already been torn down.
fn abort_cursor<C, E>(inner: &HostInner<C, E>) {
    if let Some(h) = inner
        .cursor_task
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        h.abort();
    }
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

/// Bounded depth of the host outbound-control channel (cursor shapes, chat). A little slack absorbs
/// bursts; under sustained pressure the newest message is dropped (these are advisory, not control).
const OUTBOUND_QUEUE_DEPTH: usize = 8;
/// Cap on distinct cursor ids the host remembers as "already sent" (so it can send `CursorCached`).
/// Real sessions cycle through a handful of shapes; this bounds memory against a pathological observer.
const CURSOR_CACHE_CAP: usize = 128;

/// The host's send-side record of which shape ids it has already transmitted in full (so a repeat can
/// go as a `CursorCached` reference). Insertion-ordered with a hard cap; the oldest id is evicted when
/// full (a later reoccurrence just re-sends the full shape — correct, only costs bandwidth).
struct CursorCache {
    seen: std::collections::HashSet<u32>,
    order: std::collections::VecDeque<u32>,
}

impl CursorCache {
    fn new() -> Self {
        Self {
            seen: std::collections::HashSet::new(),
            order: std::collections::VecDeque::new(),
        }
    }
    fn contains(&self, id: u32) -> bool {
        self.seen.contains(&id)
    }
    fn insert(&mut self, id: u32) {
        if self.seen.insert(id) {
            self.order.push_back(id);
            if self.order.len() > CURSOR_CACHE_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }
}

/// Validate a cursor shape against the wire bounds *before* sending — the exact checks the receiver's
/// codec enforces on decode, so a shape that would be rejected on the wire is never transmitted.
fn cursor_shape_is_valid(s: &CursorShape) -> bool {
    let (w, h) = (u32::from(s.width), u32::from(s.height));
    w >= 1
        && h >= 1
        && w <= ras_protocol::MAX_CURSOR_DIM
        && h <= ras_protocol::MAX_CURSOR_DIM
        && u32::from(s.hotspot_x) < w
        && u32::from(s.hotspot_y) < h
        && s.rgba.len() == (w as usize) * (h as usize) * 4
}

/// Start the cursor task iff an observer is wired (ADR-073). The task feeds the shared host
/// outbound-control channel (`tx`); no-op if there is no observer.
fn maybe_start_cursor<C, E>(inner: &HostInner<C, E>, tx: mpsc::Sender<ControlMsg>) {
    let Some(observer) = inner
        .cursor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let stop = inner.stop.clone();
    let handle = tokio::spawn(async move {
        cursor_pump(observer, tx, stop).await;
    });
    *inner
        .cursor_task
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
}

/// The cursor watch→dedup→enqueue loop (ADR-073). Reads the observer, maps each change to a wire
/// message — a fresh [`ControlMsg::CursorShape`] the first time an id is seen, else a
/// [`ControlMsg::CursorCached`] reference — validating bounds fail-closed, and enqueues it for the
/// control loop. An id is recorded as "sent" **only after** a successful enqueue, so a dropped fresh
/// shape is re-sent (never referenced as cached before the controller has it). Exits on `stop`, on the
/// observer ending, or when the control loop drops the receiver.
async fn cursor_pump(
    mut observer: Box<dyn CursorObserver>,
    tx: mpsc::Sender<ControlMsg>,
    stop: Arc<AtomicBool>,
) {
    let mut cache = CursorCache::new();
    while !stop.load(Ordering::Relaxed) {
        let Some(frame) = observer.next().await else {
            break;
        };
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match frame {
            CursorFrame::Hidden => {
                if tx.try_send(ControlMsg::CursorHidden).is_err() && tx.is_closed() {
                    break;
                }
            }
            CursorFrame::Shape(shape) => {
                if !cursor_shape_is_valid(&shape) {
                    continue; // malformed — never send garbage the receiver would reject
                }
                let id = shape.id;
                let known = cache.contains(id);
                let msg = if known {
                    ControlMsg::CursorCached { id }
                } else {
                    ControlMsg::CursorShape {
                        id,
                        hotspot_x: shape.hotspot_x,
                        hotspot_y: shape.hotspot_y,
                        width: shape.width,
                        height: shape.height,
                        rgba: shape.rgba,
                    }
                };
                match tx.try_send(msg) {
                    Ok(()) => {
                        if !known {
                            cache.insert(id); // recorded only once the full shape is truly enqueued
                        }
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {} // advisory — drop this update
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        }
    }
}

async fn host_control_loop<C, E>(
    control: &mut Box<dyn ControlChannelDyn>,
    inner: &HostInner<C, E>,
    mut bye_rx: mpsc::Receiver<ErrorCode>,
    mut outbound_rx: mpsc::Receiver<ControlMsg>,
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
            // A proactive host→controller message (cursor shape ADR-073, chat ADR-082) is ready to
            // send; forward it over the reliable control channel this loop owns. Never blocks the
            // session — the source already dropped-newest under pressure. `None` = all senders gone
            // (teardown) → exit.
            out = outbound_rx.recv() => match out {
                Some(msg) => {
                    if control.send(msg).await.is_err() { break; }
                }
                None => break,
            },
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
            // Remote-pointer position from the controller: surface it as a lifecycle event for the
            // host app's "look here" overlay. Purely visual — never OS input (Invariants 6/14 untouched).
            Ok(ControlMsg::Pointer(p)) => {
                if let Some(sink) = inner
                    .lifecycle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                {
                    sink.emit(LifecycleEvent::RemotePointer {
                        x: p.x,
                        y: p.y,
                        visible: p.visible,
                    });
                }
            }
            // Controller requests the OS-input control lease (Phase 3). Refuse fail-closed if this
            // host has no input backend or lacks OS permission; else prompt the local user (Inv 1),
            // clamp to grant ∩ policy ∩ consent, and issue. Never trust the requested caps as
            // authority (Inv 15) — `LeaseManager::issue` re-clamps against the seeded grant caps.
            Ok(ControlMsg::ControlRequest { capabilities }) => {
                let requested: CapabilitySet = capabilities.into_iter().collect();
                let decision = host_handle_control_request(inner, &requested).await;
                match decision {
                    Ok(lease) => {
                        let _ = control
                            .send(ControlMsg::ControlGranted {
                                lease_id: lease.lease_id.0,
                                generation: lease.generation,
                                capabilities: lease.capabilities.iter().cloned().collect(),
                                expires_at: lease.expires_at,
                                signature: bytes::Bytes::new(),
                            })
                            .await;
                        record_audit(
                            inner,
                            ras_audit::AuditEvent::ControlLeaseGranted {
                                generation: lease.generation,
                            },
                        );
                        emit_lifecycle(
                            inner,
                            LifecycleEvent::ControlLeaseGranted {
                                generation: lease.generation,
                            },
                        );
                    }
                    Err(code) => {
                        let _ = control.send(ControlMsg::ControlRevoked { code }).await;
                        record_audit(inner, ras_audit::AuditEvent::ControlLeaseRevoked { code });
                        emit_lifecycle(inner, LifecycleEvent::ControlLeaseEnded { code });
                    }
                }
            }
            // One OS-input event. The per-message gate (Inv 15 / ADR-041) runs against the host's own
            // lease state; only a fully-authorized action reaches the sink. A rejection is surfaced as
            // a content-free lifecycle event (never the coordinate/key/text — Inv 8).
            Ok(ControlMsg::Input(env)) => {
                if let Err(code) = host_handle_input(inner, &env) {
                    record_audit(inner, ras_audit::AuditEvent::InputRejected { code });
                    emit_lifecycle(inner, LifecycleEvent::InputRejected { code });
                }
            }
            // Controller → host clipboard-text push (ADR-076). Gated host-side on `clipboard.write`
            // against the session grant (Inv 15 — never the peer's claim), then the OS clipboard is
            // **set, never pasted** (no-auto-paste rule). Outcome is a content-free lifecycle event.
            Ok(ControlMsg::ClipboardText { text }) => {
                host_handle_clipboard(inner, &text);
            }
            // Chat from the controller (ADR-082): surface it for the host UI. Base session comms —
            // content-bearing but never logged (the payload stays wrapped in `Redacted`, Inv 8).
            Ok(ControlMsg::ChatMessage { text }) => {
                emit_lifecycle(inner, LifecycleEvent::ChatMessage { text });
            }
            // File push to a catalogued drop target (ADR-086, the danger channel). Authorize
            // (catalogue + `file.push.<target>` cap + safe-leaf filename + size cap) then get per-transfer
            // local consent (Inv 1); reply FileAccept or FileReject, audited. Never a controller path.
            Ok(ControlMsg::FileOffer {
                target,
                filename,
                size,
            }) => {
                let reply = host_handle_file_offer(inner, target, filename, size).await;
                if control.send(reply).await.is_err() {
                    break;
                }
            }
            // A chunk / completion of an accepted transfer (ADR-090). Written to the host-resolved dest
            // via the O_NOFOLLOW sink; an over-run or short transfer is aborted (no oversized/partial
            // file). A stray chunk with no active transfer is ignored.
            Ok(ControlMsg::FileChunk { data }) => host_handle_file_chunk(inner, &data),
            Ok(ControlMsg::FileComplete) => host_handle_file_complete(inner),
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

/// Emit a lifecycle event if a sink is attached (advisory — never backpressures the control loop).
fn emit_lifecycle<C, E>(inner: &HostInner<C, E>, ev: LifecycleEvent) {
    if let Some(sink) = inner
        .lifecycle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        sink.emit(ev);
    }
}

/// Record a content-free security event to the audit journal (Inv 10, ADR-088), losslessly. No-op if
/// no audit sink is wired. Synchronous — the sink must not drop (unlike the advisory lifecycle stream).
fn record_audit<C, E>(inner: &HostInner<C, E>, event: ras_audit::AuditEvent) {
    if let Some(sink) = inner
        .audit
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        sink.record(event);
    }
}

/// Handle a `ControlRequest`: fail-closed if this host cannot inject, else local consent (Inv 1) then
/// issue a lease clamped to grant ∩ policy ∩ consent. Consent is awaited **outside** any lock.
async fn host_handle_control_request<C, E>(
    inner: &HostInner<C, E>,
    requested: &CapabilitySet,
) -> Result<ras_control::ControlLease, ErrorCode> {
    // A session that is stopping (emergency or graceful) must never mint a fresh lease.
    if inner.stop.load(Ordering::SeqCst) {
        return Err(ErrorCode::SessionRevoked);
    }
    // Fail-closed: no backend, or the OS won't permit injection ⇒ no lease.
    let sink = inner
        .input_sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if !sink.is_some_and(|s| s.input_permitted()) {
        return Err(ErrorCode::CapabilityDenied);
    }
    // Local consent (Invariant 1). Clone the Arc out of the lock, then await with no lock held.
    let consent = inner
        .control_consent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let consented = consent.consent_to_control(requested).await;
    if consented.is_empty() {
        return Err(ErrorCode::ConsentDenied);
    }
    // Re-check after the (possibly long, up to 90 s) consent await: an emergency stop or teardown that
    // landed *during* the prompt must abort issuance, or `issue` would resurrect a lease (bump the
    // generation + install an active lease) after `revoke_all` had already cleared it (Inv 4).
    if inner.stop.load(Ordering::SeqCst) {
        return Err(ErrorCode::SessionRevoked);
    }
    let holder = *inner
        .peer_endpoint
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_ms();
    let mut guard = inner
        .lease
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match guard.as_mut() {
        Some(lm) => lm
            .issue(holder, requested, &consented, now, LEASE_DEFAULT_TTL_MS)
            .map_err(ras_control::ControlError::code),
        None => Err(ErrorCode::Internal),
    }
}

/// Run one inbound `Input` event through the per-message gate (Inv 15) and, only if fully authorized,
/// dispatch it to the OS sink. The lease lock is released before dispatch. Returns the reason code on
/// any rejection (content-free — the caller never logs the payload, Inv 8).
fn host_handle_input<C, E>(inner: &HostInner<C, E>, env: &InputEnvelope) -> Result<(), ErrorCode> {
    let now = now_ms();
    // Authorize under the lease lock (sync); `map(|_| ())` drops the borrowed action so the guard can
    // release before we dispatch.
    let verdict = {
        let mut guard = inner
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.as_mut() {
            Some(lm) => lm
                .authorize_input(env, now)
                .map(|_| ())
                .map_err(ras_control::ControlError::code),
            None => Err(ErrorCode::LeaseInvalid),
        }
    };
    verdict?;
    // Invariant 4 closes the authorize→dispatch gap: an emergency stop that lands *after* this event
    // was authorized (advancing the gate's `last_seq`) but *before* it is injected must still override
    // it. `emergency_stop`/`stop` set `stop` before `release_input` runs, so re-checking it here drops
    // the already-authorized in-flight event rather than injecting one frame past the stop.
    if inner.stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let sink = inner
        .input_sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    match sink {
        Some(s) => ras_control::dispatch(s.as_ref(), &env.action).map_err(|e| e.code),
        None => Err(ErrorCode::InputFailed),
    }
}

/// Map a file-push authorization failure to a stable wire reason code. Content-free.
fn file_error_code(e: FilePushError) -> ErrorCode {
    match e {
        // A withheld per-target capability or a disallowed extension is a capability/policy refusal.
        FilePushError::CapabilityDenied | FilePushError::ExtensionDenied => {
            ErrorCode::CapabilityDenied
        }
        // Unknown target / unsafe filename / oversized are malformed-request refusals.
        _ => ErrorCode::InvalidMessage,
    }
}

/// Audit + surface a file-push refusal and return the reply. Content-free.
fn file_reject<C, E>(inner: &HostInner<C, E>, code: ErrorCode) -> ControlMsg {
    record_audit(inner, ras_audit::AuditEvent::FilePushRejected { code });
    emit_lifecycle(inner, LifecycleEvent::FileTransferRejected { code });
    ControlMsg::FileReject { code }
}

/// Handle a controller `FileOffer` (ADR-086/090, the danger channel). Authorizes host-side against the
/// vendor catalogue + the session grant (`file.push.<target>`, Inv 15 — never the peer's claim) + the
/// safe-leaf filename validator (defeats the traversal/zip-slip CVE class) + the size cap; then gets
/// **per-transfer local consent** (Inv 1); then opens the **host-resolved** destination on the write sink
/// (`O_NOFOLLOW`, the symlink-follow defense) and arms the transfer. Returns the reply (`FileAccept` /
/// `FileReject`), audited content-free. The filename/path never leave the host. Consent is awaited
/// **outside** any lock.
async fn host_handle_file_offer<C, E>(
    inner: &HostInner<C, E>,
    target: String,
    filename: String,
    size: u64,
) -> ControlMsg {
    // ⓪ one transfer at a time. A second offer while one is in flight is an out-of-sequence protocol
    // violation — refuse it fail-closed *before* authorize/consent, so it can neither prompt a wasted
    // consent nor overwrite the active-transfer state and orphan the first partial file on disk.
    if inner
        .active_transfer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
    {
        return file_reject(inner, ErrorCode::InvalidMessage);
    }
    // ① authorize → the host-resolved destination path, or a reject code. No catalogue ⇒ no target.
    let resolved = {
        let cat = inner
            .file_catalogue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let granted = inner
            .granted_caps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match cat.as_ref() {
            Some(catalogue) => authorize_file_push(
                catalogue,
                &granted,
                &FilePushRequest {
                    target: target.clone(),
                    filename: filename.clone(),
                    size,
                },
            )
            .map_err(file_error_code),
            None => Err(ErrorCode::InvalidMessage),
        }
    };
    let dest = match resolved {
        Ok(d) => d,
        Err(code) => return file_reject(inner, code),
    };
    // ② per-transfer local consent (Inv 1) — awaited outside any lock.
    let consent = inner
        .file_consent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if !consent.consent_to_file(&target, &filename, size).await {
        return file_reject(inner, ErrorCode::ConsentDenied);
    }
    // ③ open the host-resolved destination on the write backend (O_NOFOLLOW). No backend ⇒ can't receive.
    let Some(sink) = inner
        .file_write_sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    else {
        return file_reject(inner, ErrorCode::InputFailed);
    };
    if sink.open(&dest, size).is_err() {
        return file_reject(inner, ErrorCode::InputFailed);
    }
    // Armed: record the active transfer, audit the authorization, accept.
    *inner
        .active_transfer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ActiveTransfer {
        received: 0,
        declared_size: size,
    });
    record_audit(inner, ras_audit::AuditEvent::FilePushAccepted);
    emit_lifecycle(inner, LifecycleEvent::FileTransferAccepted);
    ControlMsg::FileAccept
}

/// Handle one `FileChunk` of an accepted transfer (ADR-090). Writes it to the open sink and tracks the
/// running total; a chunk that would exceed the offered size (or a write failure) **aborts** the
/// transfer (no oversized/partial file). A chunk with no active transfer is ignored (a stray/late chunk).
fn host_handle_file_chunk<C, E>(inner: &HostInner<C, E>, data: &[u8]) {
    // Clone the sink Arc first (consistent lock order: write_sink → active_transfer), then work state.
    let sink = inner
        .file_write_sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut guard = inner
        .active_transfer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(t) = guard.as_mut() else {
        return; // no active transfer — ignore a stray chunk
    };
    let new_total = t.received.saturating_add(data.len() as u64);
    if new_total > t.declared_size {
        *guard = None;
        drop(guard);
        if let Some(s) = &sink {
            s.abort();
        }
        let _ = file_reject(inner, ErrorCode::InvalidMessage);
        return;
    }
    match &sink {
        Some(s) if s.write(data).is_ok() => t.received = new_total,
        _ => {
            *guard = None;
            drop(guard);
            if let Some(s) = &sink {
                s.abort();
            }
            let _ = file_reject(inner, ErrorCode::InputFailed);
        }
    }
}

/// Handle `FileComplete` (ADR-090): finalize the write iff the received total equals the offered size,
/// else abort (no truncated file). A complete with no active transfer is ignored.
fn host_handle_file_complete<C, E>(inner: &HostInner<C, E>) {
    let sink = inner
        .file_write_sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(t) = inner
        .active_transfer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return;
    };
    if t.received == t.declared_size {
        if let Some(s) = &sink {
            let _ = s.finish();
        }
    } else if let Some(s) = &sink {
        // Short transfer (fewer bytes than offered): discard rather than keep a truncated file.
        s.abort();
        let _ = file_reject(inner, ErrorCode::InvalidMessage);
    }
}

/// Abort any in-progress file transfer on teardown (ADR-090) — discard the partial file. Idempotent.
fn abort_file_transfer<C, E>(inner: &HostInner<C, E>) {
    let had = inner
        .active_transfer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .is_some();
    if had {
        if let Some(s) = inner
            .file_write_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            s.abort();
        }
    }
}

/// Apply a controller→host clipboard-text push (ADR-076). Authorizes host-side against the session
/// grant (`clipboard.write`, Inv 15 — never the peer's claim); on success sets the OS clipboard via the
/// injected [`ClipboardSink`], which **sets, never pastes** (the no-auto-paste rule). Emits a
/// content-free lifecycle outcome (never the clipboard text — Inv 8). Fail-closed: capability withheld
/// or no backend wired ⇒ `CapabilityDenied`.
fn host_handle_clipboard<C, E>(inner: &HostInner<C, E>, text: &ras_protocol::Redacted) {
    let granted = inner
        .granted_caps
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if !clipboard_push_allowed(ClipboardDirection::ControllerToHost, &granted) {
        emit_lifecycle(
            inner,
            LifecycleEvent::ClipboardRejected {
                code: ErrorCode::CapabilityDenied,
            },
        );
        return;
    }
    let sink = inner
        .clipboard_sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let outcome = match sink {
        // `reveal()` here is the one sanctioned use — the OS write, not a log.
        Some(s) => match s.set_text(text.reveal()) {
            Ok(()) => LifecycleEvent::ClipboardApplied {
                len: text.reveal().len(),
            },
            Err(e) => LifecycleEvent::ClipboardRejected { code: e.code },
        },
        None => LifecycleEvent::ClipboardRejected {
            code: ErrorCode::CapabilityDenied,
        },
    };
    record_audit(
        inner,
        match &outcome {
            LifecycleEvent::ClipboardApplied { len } => ras_audit::AuditEvent::ClipboardApplied {
                len: u32::try_from(*len).unwrap_or(u32::MAX),
            },
            _ => ras_audit::AuditEvent::ClipboardRejected {
                code: clipboard_reject_code(&outcome),
            },
        },
    );
    emit_lifecycle(inner, outcome);
}

/// The reject code carried by a `ClipboardRejected` lifecycle event (defensive default otherwise).
fn clipboard_reject_code(ev: &LifecycleEvent) -> ErrorCode {
    match ev {
        LifecycleEvent::ClipboardRejected { code } => *code,
        _ => ErrorCode::Internal,
    }
}

/// Revoke any active lease and flush the OS key-state (Invariant 4 key-state cleanup). Called on
/// emergency stop and graceful teardown, before the media halt is reported to the peer. Idempotent,
/// never blocks, never fails.
fn release_input<C, E>(inner: &HostInner<C, E>) {
    if let Some(lm) = inner
        .lease
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        lm.revoke_all();
    }
    if let Some(sink) = inner
        .input_sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        let _ = sink.release_all();
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
    /// The PASETO session grant (obtained out-of-band in the bootstrap phase) to present to the host
    /// in `ControlMsg::AuthEnvelope`. Empty on the `insecure-no-auth` path (the host's no-op validator
    /// ignores it); a real host denies an empty/invalid grant.
    pub grant: bytes::Bytes,
}

impl ControllerSessionConfig {
    /// Dial the given target with a small default buffer and a 10 s reconnect window.
    #[must_use]
    pub fn new(target: DialTarget) -> Self {
        Self {
            target,
            target_buffer: Duration::from_millis(30),
            reconnect_window: Duration::from_secs(10),
            grant: bytes::Bytes::new(),
        }
    }

    /// Attach the session grant to present to the host on the session ALPN.
    #[must_use]
    pub fn with_grant(mut self, grant: bytes::Bytes) -> Self {
        self.grant = grant;
        self
    }
}

/// Command sent from the public API into the single control-owning task (avoids splitting the
/// bidi channel: one task both sends and receives via `select!`).
enum ControlCommand {
    Keyframe(KeyframeReason),
    Feedback(DecoderFeedback),
    /// Remote-pointer position to forward to the host (best-effort; dropped if the task is behind).
    Pointer(ras_protocol::PointerUpdate),
    /// Request the OS-input control lease from the host (Phase 3).
    ControlRequest(Vec<String>),
    /// One OS-input event to forward to the host (Phase 3), under a held lease.
    Input(InputEnvelope),
    /// Push clipboard text to the host (ADR-076). Gated host-side on `clipboard.write`; the host sets
    /// its clipboard and never pastes.
    ClipboardText(String),
    /// Send an in-session chat message to the host (ADR-082). Base session comms; never logged (Inv 8).
    Chat(String),
    /// Offer a file push to a catalogued drop target (ADR-086): `(target, filename, size)`.
    FileOffer(String, String, u64),
    /// One chunk of an accepted file transfer's bytes (ADR-090).
    FileChunk(bytes::Bytes),
    /// The accepted file transfer's bytes are all sent (ADR-090).
    FileComplete,
    Bye,
}

struct ControllerInner {
    config: ControllerSessionConfig,
    transport: Arc<dyn SessionTransport>,
    state: Mutex<SessionState>,
    stop: AtomicBool,
    lifecycle: Mutex<Option<LifecycleSink>>,
    renderer: Mutex<Option<Arc<dyn FrameSink>>>,
    /// Where inbound audio packets go (ADR-077). Attached like the renderer; absent → audio is
    /// dropped at the ingest boundary (a stalled/absent output never blocks control or video).
    audio_output: Mutex<Option<Arc<dyn AudioOutput>>>,
    /// Where inbound host cursor-shape updates go (ADR-073). Attached like the renderer; absent →
    /// cursor updates are dropped (the app keeps its generic pointer). Display data, never input.
    cursor_sink: Mutex<Option<Arc<dyn CursorSink>>>,
    /// Where an inbound host→controller clipboard push is applied (ADR-076, `clipboard.read`). The host
    /// gated it on `clipboard.read`; this sink only **sets** the OS clipboard, never pastes. Absent →
    /// the push is dropped.
    clipboard_sink: Mutex<Option<Arc<dyn ClipboardSink>>>,
    stream_config: Mutex<Option<StreamConfig>>,
    command_tx: Mutex<Option<mpsc::Sender<ControlCommand>>>,
    /// Highest frame id delivered to the renderer (reported back as `last_decoded_frame`).
    last_decoded_frame: AtomicU64,
    /// Frames dropped since the last feedback report (reset each report).
    frames_dropped: AtomicU32,
    session_id: SessionId,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The OS-input control lease the host granted us, if any: `(lease_id, generation)`. Set on
    /// `ControlGranted`, cleared on `ControlRevoked` (Phase 3). The controller echoes these on each
    /// `Input` — they are claims the host re-checks, never authority (ADR-069).
    lease: Mutex<Option<([u8; 16], u32)>>,
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
                audio_output: Mutex::new(None),
                cursor_sink: Mutex::new(None),
                clipboard_sink: Mutex::new(None),
                stream_config: Mutex::new(None),
                command_tx: Mutex::new(None),
                last_decoded_frame: AtomicU64::new(0),
                frames_dropped: AtomicU32::new(0),
                session_id: next_session_id(),
                tasks: Mutex::new(Vec::new()),
                lease: Mutex::new(None),
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

        // Present the session grant first (the host reads it as its first control message and gates
        // authorization on it). Empty on the insecure path; the host's no-op validator ignores it.
        control
            .send(ControlMsg::AuthEnvelope {
                payload: inner.config.grant.clone(),
            })
            .await?;

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

        // Audio ingest task (ADR-077): pulls encoded audio packets and pushes to whatever output is
        // attached; drops otherwise. Returns immediately on transports without an audio plane.
        let aud_inner = inner.clone();
        let aud_task = tokio::spawn(async move { controller_audio_loop(&aud_inner).await });

        let mut tasks = inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.push(ctrl_task);
        tasks.push(vid_task);
        tasks.push(fb_task);
        tasks.push(aud_task);

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

    /// Attach/replace the audio output (ADR-077). Decoupled from `connect` like the renderer, so audio
    /// can flow (and be dropped) before playback exists. No keyframe concept — Opus packets are each
    /// independently decodable, so a freshly attached output plays from the next packet.
    pub fn attach_audio_output(&self, output: Arc<dyn AudioOutput>) {
        *self
            .inner
            .audio_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(output);
    }

    /// Detach the audio output; ingest continues and drops packets at the boundary.
    pub fn detach_audio_output(&self) {
        *self
            .inner
            .audio_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Attach/replace the cursor sink (ADR-073): where the host's OS cursor-shape updates are drawn
    /// (the pointer overlay). Decoupled from `connect` like the renderer; absent → updates are dropped
    /// and the app keeps its generic pointer.
    pub fn attach_cursor_sink(&self, sink: Arc<dyn CursorSink>) {
        *self
            .inner
            .cursor_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    /// Detach the cursor sink; cursor updates are then dropped at the boundary.
    pub fn detach_cursor_sink(&self) {
        *self
            .inner
            .cursor_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Attach/replace the clipboard sink (ADR-076): where an inbound host→controller clipboard push is
    /// applied to the controller's OS clipboard (**set, never pasted**). Absent → pushes are dropped.
    /// The host has already gated the push on `clipboard.read` (Inv 15).
    pub fn attach_clipboard_sink(&self, sink: Arc<dyn ClipboardSink>) {
        *self
            .inner
            .clipboard_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
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

    /// Forward the controller's pointer position to the host for its **remote-pointer** overlay
    /// ("look here"). Best-effort and **non-blocking** (latency-first): a high-frequency update is
    /// dropped rather than queued if the control task is briefly behind. This is a purely visual
    /// pointer — **not OS input** (no click, no keyboard reaches the host), so it is outside the
    /// input-injection invariants. `x`/`y` are normalized `0..=65535` (left→right / top→bottom).
    pub fn send_pointer(&self, x: u16, y: u16, visible: bool) {
        let tx = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ControlCommand::Pointer(ras_protocol::PointerUpdate {
                x,
                y,
                visible,
            }));
        }
    }

    /// Request the OS-input control lease from the host (Phase 3). The host prompts its local user and
    /// replies with `ControlGranted` (→ [`Self::current_lease`]) or `ControlRevoked`. Best-effort.
    pub fn request_control(&self, capabilities: Vec<String>) {
        let tx = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ControlCommand::ControlRequest(capabilities));
        }
    }

    /// Forward one OS-input event to the host under the held lease (Phase 3). The caller stamps the
    /// envelope's `lease_id`/`generation`/`seq`; the host re-checks all of them (ADR-069). Best-effort.
    pub fn send_input(&self, env: InputEnvelope) {
        let tx = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ControlCommand::Input(env));
        }
    }

    /// Push clipboard text to the host (ADR-076) — an explicit user action ("Send clipboard"). The
    /// host gates it on `clipboard.write` (Inv 15) and, if allowed, **sets its OS clipboard without
    /// pasting** (the no-auto-paste rule). Best-effort; the text is a secret and is never logged (Inv 8).
    pub fn send_clipboard_text(&self, text: String) {
        let tx = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ControlCommand::ClipboardText(text));
        }
    }

    /// Send an in-session chat message to the host (ADR-082). Base session comms (no capability). The
    /// text is a secret and is never logged (Inv 8); an oversized message (> `MAX_CHAT_BYTES`) is
    /// dropped rather than sent (the wire would refuse it). Best-effort.
    pub fn send_chat(&self, text: String) {
        if text.len() > ras_protocol::MAX_CHAT_BYTES {
            return;
        }
        let tx = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ControlCommand::Chat(text));
        }
    }

    /// Offer a file push to a catalogued host drop target (ADR-086). Sends only the target name, a leaf
    /// `filename`, and the `size` — **never a path**; the host authorizes + gets consent and replies with
    /// a `FileTransferAccepted`/`Rejected` lifecycle event. Oversized names are dropped, not sent (the
    /// host would refuse them). Best-effort. (Byte streaming after an accept is a follow-up.)
    pub fn send_file_offer(&self, target: String, filename: String, size: u64) {
        if target.len() > ras_protocol::MAX_FILE_TARGET
            || filename.len() > ras_protocol::MAX_FILE_NAME
        {
            return;
        }
        let tx = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(tx) = tx {
            let _ = tx.try_send(ControlCommand::FileOffer(target, filename, size));
        }
    }

    /// Send one chunk of an **accepted** file transfer (ADR-090) — call only after a
    /// `FileTransferAccepted` lifecycle event. Bytes over [`ras_protocol::MAX_FILE_CHUNK`] are dropped
    /// (split them). Best-effort; the host aborts the transfer if the total exceeds the offered size.
    pub fn send_file_chunk(&self, data: bytes::Bytes) {
        if data.len() > ras_protocol::MAX_FILE_CHUNK {
            return;
        }
        if let Some(tx) = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let _ = tx.try_send(ControlCommand::FileChunk(data));
        }
    }

    /// Signal that all chunks of the accepted transfer have been sent (ADR-090); the host finalizes the
    /// write iff the received total equals the offered size.
    pub fn send_file_complete(&self) {
        if let Some(tx) = self
            .inner
            .command_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let _ = tx.try_send(ControlCommand::FileComplete);
        }
    }

    /// The OS-input lease the host has granted this controller, if any: `(lease_id, generation)`.
    #[must_use]
    pub fn current_lease(&self) -> Option<([u8; 16], u32)> {
        *self
            .inner
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
                Some(ControlCommand::Pointer(p)) => {
                    if control.send(ControlMsg::Pointer(p)).await.is_err() { break; }
                }
                Some(ControlCommand::ControlRequest(capabilities)) => {
                    if control.send(ControlMsg::ControlRequest { capabilities }).await.is_err() { break; }
                }
                Some(ControlCommand::Input(env)) => {
                    if control.send(ControlMsg::Input(env)).await.is_err() { break; }
                }
                Some(ControlCommand::ClipboardText(text)) => {
                    let msg = ControlMsg::ClipboardText { text: ras_protocol::Redacted(text) };
                    if control.send(msg).await.is_err() { break; }
                }
                Some(ControlCommand::Chat(text)) => {
                    let msg = ControlMsg::ChatMessage { text: ras_protocol::Redacted(text) };
                    if control.send(msg).await.is_err() { break; }
                }
                Some(ControlCommand::FileOffer(target, filename, size)) => {
                    let msg = ControlMsg::FileOffer { target, filename, size };
                    if control.send(msg).await.is_err() { break; }
                }
                Some(ControlCommand::FileChunk(data)) => {
                    if control.send(ControlMsg::FileChunk { data }).await.is_err() { break; }
                }
                Some(ControlCommand::FileComplete) => {
                    if control.send(ControlMsg::FileComplete).await.is_err() { break; }
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
                // Host granted us the OS-input lease (Phase 3): remember it so `send_input` can stamp
                // envelopes. These are claims the host re-checks per message (ADR-069), never authority.
                Ok(ControlMsg::ControlGranted { lease_id, generation, .. }) => {
                    *inner
                        .lease
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((lease_id, generation));
                }
                // Host revoked / refused the lease: drop it (further input will be rejected host-side).
                Ok(ControlMsg::ControlRevoked { .. }) => {
                    *inner
                        .lease
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                }
                // Host's verdict on a file offer (ADR-086): surface it for the controller UI. Content-free.
                Ok(ControlMsg::FileAccept) => {
                    if let Some(sink) = inner
                        .lifecycle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.emit(LifecycleEvent::FileTransferAccepted);
                    }
                }
                Ok(ControlMsg::FileReject { code }) => {
                    if let Some(sink) = inner
                        .lifecycle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.emit(LifecycleEvent::FileTransferRejected { code });
                    }
                }
                // Host cursor-shape updates (ADR-073): forward to the attached cursor sink (the pointer
                // overlay). Display data, not input. The `id`-cached form reuses a previously-drawn
                // shape; an absent sink simply drops the update.
                Ok(ControlMsg::CursorShape { id, hotspot_x, hotspot_y, width, height, rgba }) => {
                    if let Some(sink) = inner
                        .cursor_sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.set_shape(CursorShape { id, hotspot_x, hotspot_y, width, height, rgba });
                    }
                }
                Ok(ControlMsg::CursorCached { id }) => {
                    if let Some(sink) = inner
                        .cursor_sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.set_cached(id);
                    }
                }
                Ok(ControlMsg::CursorHidden) => {
                    if let Some(sink) = inner
                        .cursor_sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.hide();
                    }
                }
                // Chat from the host (ADR-082): surface it for the controller UI. Content-bearing but
                // never logged (the payload stays wrapped in `Redacted`, Inv 8).
                Ok(ControlMsg::ChatMessage { text }) => {
                    if let Some(sink) = inner
                        .lifecycle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.emit(LifecycleEvent::ChatMessage { text });
                    }
                }
                // Host → controller clipboard push (ADR-076, `clipboard.read`). The host already gated it
                // on `clipboard.read`; apply it to the controller's OS clipboard — **set, never pasted**
                // (no-auto-paste rule) — and surface a content-free outcome. Absent sink → dropped.
                Ok(ControlMsg::ClipboardText { text }) => {
                    let sink = inner
                        .clipboard_sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    let outcome = match sink {
                        // `reveal()` at the OS-write boundary is the one sanctioned use, never a log.
                        Some(s) => match s.set_text(text.reveal()) {
                            Ok(()) => LifecycleEvent::ClipboardApplied {
                                len: text.reveal().len(),
                            },
                            Err(e) => LifecycleEvent::ClipboardRejected { code: e.code },
                        },
                        None => LifecycleEvent::ClipboardRejected {
                            code: ErrorCode::CapabilityDenied,
                        },
                    };
                    if let Some(sink) = inner
                        .lifecycle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                    {
                        sink.emit(outcome);
                    }
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

/// Pull inbound audio packets and hand them to the attached output (ADR-077). Mirrors the video loop:
/// a terminal transport error ends the loop; an absent output drops the packet. Returns immediately on
/// transports without an audio plane (`audio_source()` reports unsupported).
async fn controller_audio_loop(inner: &ControllerInner) {
    let mut source = match inner.transport.audio_source().await {
        Ok(s) => s,
        Err(_) => return, // no audio plane on this transport — silent, by design
    };
    while !inner.stop.load(Ordering::Relaxed) {
        match source.next().await {
            Ok(packet) => {
                if let Some(out) = inner
                    .audio_output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                {
                    out.push(packet);
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
