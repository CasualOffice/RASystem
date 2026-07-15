//! Typed lifecycle events surfaced to the embedding app (design §5.6, maps to `docs/05 §4`).
//!
//! Events are **content-free** — enums and numbers only, never pixels/titles/paths (Invariant 8).
//! They ride a bounded channel ([`LifecycleStream`]) so a slow lifecycle consumer can never
//! backpressure the session's hot tasks.

use ras_protocol::ErrorCode;
use ras_transport_iroh::PathKind;

/// Opaque per-session id (content-free, log-safe). Monotonic within a process run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u64);

impl core::fmt::Display for SessionId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "session-{}", self.0)
    }
}

/// Content-free stop reason (log/audit-safe). Phase-2's emergency-stop reason lands in
/// [`StopReason::UserRequested`] first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    /// Local user asked to stop/disconnect.
    UserRequested,
    /// The peer closed the session cleanly.
    PeerClosed,
    /// The reconnect window elapsed without restore.
    Timeout,
    /// Emergency stop / mid-session revoke (Invariant 4). Audit-distinct from a clean close: this
    /// records that control was forcibly withdrawn, not that either side left gracefully.
    Revoked {
        /// Stable revoke reason (typically [`ErrorCode::SessionRevoked`]).
        code: ErrorCode,
    },
    /// Terminated by an error with a stable code.
    Error(ErrorCode),
}

/// DTO projection of [`ras_media::StreamConfig`] for the FFI/JS edge — **not** an independent type.
/// The WebCodecs codec string is derived here (Annex-B ⇒ the decoder needs no `description`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StreamDescriptor {
    /// Fully-qualified WebCodecs string, e.g. `"avc1.4D401F"`.
    pub codec: String,
    /// Output width (px).
    pub width: u32,
    /// Output height (px).
    pub height: u32,
    /// Color space the decoder must assume.
    pub color_space: ras_media::ColorSpace,
}

impl StreamDescriptor {
    /// Project a media [`StreamConfig`](ras_media::StreamConfig) into the DTO, deriving the codec
    /// string at this boundary.
    #[must_use]
    pub fn from_config(config: &ras_media::StreamConfig) -> Self {
        Self {
            codec: config.codec.webcodecs_string(config.width, config.height),
            width: config.width,
            height: config.height,
            color_space: config.color,
        }
    }
}

/// DTO projection of [`ras_transport_iroh::ConnHealth`] for UI. Numbers only (log-safe).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct QualitySample {
    /// Direct / relayed / migrating — a UI `match` must handle `Migrating`.
    pub path: PathKind,
    /// Display projection of `rtt_us`.
    pub rtt_ms: u32,
    /// Display projection of `loss_fraction`.
    pub loss_pct: f32,
    /// Frames actually delivered per second.
    pub delivered_fps: u16,
}

impl QualitySample {
    /// Project a [`ConnHealth`](ras_transport_iroh::ConnHealth) snapshot into the UI DTO.
    #[must_use]
    pub fn from_health(h: &ras_transport_iroh::ConnHealth, delivered_fps: u16) -> Self {
        Self {
            path: h.path,
            rtt_ms: h.rtt_us / 1000,
            loss_pct: h.loss_fraction * 100.0,
            delivered_fps,
        }
    }
}

/// Typed lifecycle event stream item. Content-free.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum LifecycleEvent {
    /// `docs/05 connecting`. State: `SessionConnecting`.
    Connecting,
    /// Controller `session-ready` / host `session-started`. Control channel up.
    SessionReady {
        /// This session's id.
        session_id: SessionId,
    },
    /// `stream-configured`. Carries the DTO the renderer needs to configure the decoder. State:
    /// `Active`.
    StreamConfigured {
        /// Decoder configuration DTO.
        descriptor: StreamDescriptor,
    },
    /// `quality-changed`. Advisory/UI only; never blocks the session.
    ConnectionQuality {
        /// Latest quality projection.
        sample: QualitySample,
    },
    /// `session-suspended`. Transport lost within the reconnect window; controller keeps
    /// cursor + controls live. State: `Suspended`.
    Suspended {
        /// Milliseconds since suspension began.
        since_ms: u64,
    },
    /// Transport restored within the window.
    Resumed,
    /// `disconnected`. Transport gone (window not necessarily elapsed); distinct from
    /// [`LifecycleEvent::SessionEnded`].
    Disconnected {
        /// Reason code.
        code: ErrorCode,
    },
    /// `session-ended`. Terminal; the object is inert afterward.
    SessionEnded {
        /// Why the session ended.
        reason: StopReason,
    },
    /// Emergency-stop / revoke surfaced distinctly for audit (maps to `SessionState::Revoked`).
    Revoked {
        /// Reason code.
        code: ErrorCode,
    },
    /// Host-side: the controller's **remote-pointer** position, for a "look here" overlay. Purely
    /// visual (never OS input). Coordinates are normalized `0..=65535` (left→right / top→bottom).
    RemotePointer {
        /// Horizontal position, `0..=65535`.
        x: u16,
        /// Vertical position, `0..=65535`.
        y: u16,
        /// Whether the pointer is on-screen (`false` → hide the overlay cursor).
        visible: bool,
    },
    /// Host-side: the shared display's global bounds (logical units), emitted once the capture
    /// starts, so the app can place its pointer overlay over exactly the display being shared —
    /// correct on a secondary monitor, not just the primary. Not emitted if the backend can't report
    /// bounds (the app then keeps its default whole-primary overlay).
    CaptureGeometry {
        /// Global x of the display's top-left, logical units.
        x: i32,
        /// Global y of the display's top-left, logical units.
        y: i32,
        /// Display width, logical units.
        width: u32,
        /// Display height, logical units.
        height: u32,
    },
}

/// The lifecycle event stream handed to the embedding app. A bounded receiver: latest-wins-ish, so
/// a slow consumer drops events rather than backpressuring session tasks. (Design §8 Q-STREAM left
/// the concrete type open; Phase 1 pins a bounded `tokio` mpsc receiver.)
pub type LifecycleStream = tokio::sync::mpsc::Receiver<LifecycleEvent>;

/// Sender half held by the orchestrator. Never blocks the caller: on a full/closed channel the
/// event is dropped (lifecycle is advisory; the state machine is the source of truth).
#[derive(Clone)]
pub(crate) struct LifecycleSink(pub(crate) tokio::sync::mpsc::Sender<LifecycleEvent>);

impl LifecycleSink {
    pub(crate) fn emit(&self, ev: LifecycleEvent) {
        // try_send: advisory events must never backpressure the session's hot path.
        let _ = self.0.try_send(ev);
    }
}
