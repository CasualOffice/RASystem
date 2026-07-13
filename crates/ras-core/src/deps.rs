//! Dependency-injection seams the orchestrators are built on (design §5.4 / §5.5).
//!
//! These are the *object-safe* boundaries: `ras-core` owns the session logic and drives it over
//! `Arc<dyn ..>` transport + validator + sink so the real iroh/OS/Tauri backends and the in-memory
//! test doubles are interchangeable. The high-frequency capture/encoder traits stay **generic**
//! (they carry a GAT + a generic `encode`, so they are not object-safe and are monomorphized into
//! the media task instead — see [`crate::HostSession`]).
//!
//! Reconciliation note: the design drafted [`GrantValidator`] with RPITIT (`-> impl Future`), which
//! is not object-safe; because `HostSession` takes it as `Arc<dyn GrantValidator>`, it is expressed
//! here with `#[async_trait]` (matching design §5.5's own signature). `AllowAllValidator` stays
//! behind the `insecure-no-auth` feature so it can never link into an auth build.

use async_trait::async_trait;

use crate::CoreError;
use ras_media::{EncodedFrame, StreamConfig};
use ras_protocol::{ControlMsg, ErrorCode};
use ras_transport_iroh::{ConnHealth, SendOutcome, VideoEvent};

/// A dial target / peer address alias (re-exported for signatures here).
pub use ras_transport_iroh::{EndpointAddr as DialTarget, EndpointId as PeerIdentity};

/// The session-level transport `ras-core` needs. Implemented by the iroh adapter for real runs and
/// by the in-memory loopback in tests. Reliability-split: control is reliable/ordered; video is a
/// separate droppable path. Authenticates **identity only**, never authorization (Invariant 9).
#[async_trait]
pub trait SessionTransport: Send + Sync {
    /// Establish the session (dial for controller, accept for host) on the session ALPN.
    async fn establish(&self, target: &DialTarget) -> Result<PeerIdentity, CoreError>;
    /// Reliable, ordered control/lifecycle channel.
    async fn control_channel(&self) -> Result<Box<dyn ControlChannelDyn>, CoreError>;
    /// Droppable video egress (host role only).
    async fn video_sink(&self) -> Result<Box<dyn VideoSinkDyn>, CoreError>;
    /// Droppable video ingress (controller role only).
    async fn video_source(&self) -> Result<Box<dyn VideoSourceDyn>, CoreError>;
    /// Non-blocking health snapshot for `ConnectionQuality` events.
    fn health(&self) -> ConnHealth;
}

/// Reliable, ordered control messages (cold path — async is fine).
#[async_trait]
pub trait ControlChannelDyn: Send + Sync {
    /// Send one control message.
    async fn send(&mut self, msg: ControlMsg) -> Result<(), CoreError>;
    /// Await the next control message. `Err` on a closed channel (peer gone).
    async fn recv(&mut self) -> Result<ControlMsg, CoreError>;
}

/// Droppable per-frame egress. **Sync + non-blocking**: enqueue into a bounded drop-oldest ring and
/// return immediately — never await delivery (that would reintroduce head-of-line blocking on the
/// video path from a slow sink).
pub trait VideoSinkDyn: Send + Sync {
    /// Hand one frame to the transport. Returns a source-side outcome; ordinary loss is not an error.
    fn send_frame(&self, frame: EncodedFrame) -> SendOutcome;
}

/// Droppable per-frame ingress.
#[async_trait]
pub trait VideoSourceDyn: Send + Sync {
    /// Await the next video event (frame or loss). `Err` on a terminal transport failure.
    async fn next(&mut self) -> Result<VideoEvent, CoreError>;
}

/// Where frames go on the controller. Implemented by the Tauri layer (pushes to the WebCodecs
/// worker) and by a counting sink in tests. **Sync + non-blocking** push: a slow sink drops
/// internally; it must not backpressure the transport source.
pub trait FrameSink: Send + Sync {
    /// Configure the render/decode pipeline. The first frame after this must be an IDR.
    fn configure(&self, config: &StreamConfig) -> Result<(), CoreError>;
    /// Deliver one frame. Returns immediately with a `Sent`/`Dropped` status; never awaits.
    fn push(&self, frame: EncodedFrame) -> PushResult;
}

/// Result of a [`FrameSink::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    /// Accepted into the render/decode pipeline.
    Sent,
    /// Dropped at the sink (behind; will resync on the next keyframe).
    Dropped,
}

// ---------------------------------------------------------------------------------------------
// Phase-2 auth seam (no-op in Phase 1). Object-safe (`async_trait`) so it is injectable as
// `Arc<dyn GrantValidator>`. See design §5.5.
// ---------------------------------------------------------------------------------------------

/// The consent/authorization hook. **No-op in Phase 1.** Invoked after transport identity is
/// established (`ControlEstablished`) but before `Active`. Multi-step so it can express interactive
/// local consent (Invariant 1).
#[async_trait]
pub trait GrantValidator: Send + Sync {
    /// Called once (or iteratively, via `Challenge`) per session before it may become `Active`.
    async fn authorize(&self, ctx: &SessionAuthContext) -> Result<GrantDecision, CoreError>;
}

/// Content-free context handed to the validator. Phase 1 carries the transport-authenticated
/// identity plus the (empty in Phase 1) opaque access-request bytes. `#[non_exhaustive]` — Phase 2
/// adds capabilities/nonce additively.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionAuthContext {
    /// The identity the transport authenticated. **Not** authorization.
    pub peer_identity: PeerIdentity,
    /// Opaque access-request payload from `ControlMsg::AuthEnvelope`. Empty in Phase 1.
    pub access_request: bytes::Bytes,
}

/// The validator's verdict.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GrantDecision {
    /// Proceed → orchestrator emits `SessionEvent::Authorized`.
    Authorized,
    /// Interactive consent pending (Phase 2): hold in `ControlEstablished` until re-driven.
    NeedConsent,
    /// Multi-step challenge/response (Phase 2 replay/nonce).
    Challenge(bytes::Bytes),
    /// Refused → `SessionEvent::Reject { code }`.
    Denied(ErrorCode),
}

/// PHASE-1 ONLY. Returns `Authorized` unconditionally. Gated behind `insecure-no-auth` so it can
/// never link into an auth build.
#[cfg(feature = "insecure-no-auth")]
pub struct AllowAllValidator;

#[cfg(feature = "insecure-no-auth")]
#[async_trait]
impl GrantValidator for AllowAllValidator {
    async fn authorize(&self, _ctx: &SessionAuthContext) -> Result<GrantDecision, CoreError> {
        Ok(GrantDecision::Authorized)
    }
}
