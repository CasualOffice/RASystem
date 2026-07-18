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
use ras_audit::AuditEvent;
use ras_media::{EncodedAudio, EncodedFrame, StreamConfig};
use ras_policy::CapabilitySet;
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
    /// Re-establish a dropped connection to the **same peer** for reconnection (ADR-091). After this
    /// returns `Ok`, `control_channel()` / `video_source()` / `audio_source()` yield **fresh** channels
    /// for the new connection. The caller then re-runs the session handshake — which re-presents the
    /// existing grant and re-validates it host-side (no new authorization path). **Default: unsupported**
    /// — a transport that cannot re-dial (the iroh concrete re-dial is an on-device follow-up) reports
    /// this, and the controller terminates on transport loss instead of resuming. Overridden by the
    /// loopback for tests.
    async fn reconnect(&self, target: &DialTarget) -> Result<PeerIdentity, CoreError> {
        let _ = target;
        Err(CoreError::fatal(
            ErrorCode::Internal,
            "reconnect not supported",
        ))
    }
    /// Reliable, ordered control/lifecycle channel.
    async fn control_channel(&self) -> Result<Box<dyn ControlChannelDyn>, CoreError>;
    /// Droppable video egress (host role only).
    async fn video_sink(&self) -> Result<Box<dyn VideoSinkDyn>, CoreError>;
    /// Droppable video ingress (controller role only).
    async fn video_source(&self) -> Result<Box<dyn VideoSourceDyn>, CoreError>;
    /// Droppable audio egress (host role, ADR-077). **Default: unsupported** — a transport that does
    /// not carry an audio plane (the iroh audio sub-stream is a follow-up) simply reports no audio, and
    /// the host stays silent. Overridden by transports that do (the loopback for tests).
    async fn audio_sink(&self) -> Result<Box<dyn AudioSink>, CoreError> {
        Err(CoreError::fatal(
            ErrorCode::Internal,
            "audio transport not supported",
        ))
    }
    /// Droppable audio ingress (controller role, ADR-077). **Default: unsupported** (see [`Self::audio_sink`]).
    async fn audio_source(&self) -> Result<Box<dyn AudioSourceDyn>, CoreError> {
        Err(CoreError::fatal(
            ErrorCode::Internal,
            "audio transport not supported",
        ))
    }
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

/// Droppable audio egress (host role, ADR-077). **Sync + non-blocking** like [`VideoSinkDyn`]:
/// enqueue and return, never await delivery. Only fed after the host has confirmed the session grant
/// carries `audio.listen` (Inv 15) — the transport authenticates identity, not the right to be heard.
pub trait AudioSink: Send + Sync {
    /// Hand one encoded audio packet to the transport. Ordinary loss (a shed packet) is not an error.
    fn send_audio(&self, packet: EncodedAudio);
}

/// Droppable audio ingress on the controller (ADR-077). Mirrors [`VideoSourceDyn`].
#[async_trait]
pub trait AudioSourceDyn: Send + Sync {
    /// Await the next encoded audio packet. `Err` on a terminal transport failure / closed channel.
    async fn next(&mut self) -> Result<EncodedAudio, CoreError>;
}

/// Where received audio goes on the controller (ADR-077). Implemented by the Tauri layer (forwards the
/// Opus packet to a WebCodecs `AudioDecoder`) and by a recording sink in tests. **Sync + non-blocking**
/// like [`FrameSink`]: a slow consumer drops internally, never backpressures ingest.
pub trait AudioOutput: Send + Sync {
    /// Deliver one encoded audio packet. Returns immediately; never awaits.
    fn push(&self, packet: EncodedAudio);
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

// ── Cursor-shape channel (ADR-073): host OS-cursor → controller overlay ───────────────────────────
//
// The host's OS cursor **shape** (arrow, I-beam, resize, hidden…) is sent out-of-band so the controller
// draws it client-side at zero latency instead of relying on the laggy video. This is **display data**
// (a small RGBA bitmap), routed like video frames — through a dedicated sink, **not** the (content-free)
// lifecycle events — and it is never input (outside Inv 6).

/// A host OS-cursor shape to draw on the controller (ADR-073). Top-down RGBA, `width*height*4` bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct CursorShape {
    /// Shape cache key — identical shapes share an id so a repeat can be sent as a cache reference.
    pub id: u32,
    /// Hot-spot x within the image (`< width`).
    pub hotspot_x: u16,
    /// Hot-spot y within the image (`< height`).
    pub hotspot_y: u16,
    /// Image width in pixels (`1..=`[`ras_protocol::MAX_CURSOR_DIM`]).
    pub width: u16,
    /// Image height in pixels (`1..=`[`ras_protocol::MAX_CURSOR_DIM`]).
    pub height: u16,
    /// Top-down RGBA pixels, exactly `width * height * 4` bytes.
    pub rgba: bytes::Bytes,
}

impl core::fmt::Debug for CursorShape {
    // Elide the pixels — keep cursor bitmaps out of logs (tidy, and consistent with the content-free
    // event discipline), printing only the shape's dimensions/identity.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CursorShape")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba_len", &self.rgba.len())
            .finish()
    }
}

/// A host cursor change reported by a [`CursorObserver`]. The observer always reports the **full** shape;
/// the host's send side decides whether to transmit it fresh or as a cache reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorFrame {
    /// The cursor now shows this shape.
    Shape(CursorShape),
    /// The OS cursor is currently hidden — draw nothing.
    Hidden,
}

/// Host-side OS-cursor watcher (ADR-073). Implemented by the OS backend (reads the live system cursor)
/// and a synthetic double in tests. Owned by the host cursor task. `Send` (moved into the task), not
/// `Sync`.
#[async_trait]
pub trait CursorObserver: Send {
    /// Await the next cursor change. `None` when the observer ends (capture stopped / backend gone).
    async fn next(&mut self) -> Option<CursorFrame>;
}

/// The cursor-shape dedup window shared by the host send-side cache and the controller [`CursorSink`]
/// (ADR-073). The host sends a `CursorCached { id }` reference (no RGBA) only while it still holds `id`
/// in a FIFO cache of this many distinct shapes; a sink that retains **at least** this many distinct
/// shapes, evicting oldest-first, can therefore always resolve a reference. Making it a shared constant
/// (rather than a private `128` on each side) is what keeps the two caches interoperable.
pub const CURSOR_CACHE_CAP: usize = 128;

/// Where the host's cursor updates go on the controller (ADR-073). Implemented by the app (draws the
/// shape on the pointer overlay) and a recorder in tests. **Sync + non-blocking** like [`FrameSink`]: a
/// slow sink drops internally, never backpressures the control channel.
///
/// # Cache contract (must match the host to render correctly)
/// The host omits the RGBA of a repeated shape and sends [`Self::set_cached`] instead. To resolve those
/// references the sink MUST cache shapes with a policy compatible with the host's send-side cache:
/// - retain **at least** [`CURSOR_CACHE_CAP`] distinct shapes by `id`, evicting **oldest-first (FIFO)**;
/// - a cache **hit does not reorder** (the host uses insertion order, not LRU) — else the two caches
///   diverge and a still-referenced `id` gets evicted early;
/// - under load the sink may drop the *render*, but MUST still record the shape in its cache — dropping
///   a `set_shape` from the cache would strand a later `set_cached(id)` (there is no upstream re-request
///   path; the host only re-sends a full shape once it *also* evicts the id).
///
/// Holding to this, any `set_cached(id)` the host can send is resolvable; violating it shows a stale or
/// blank remote cursor (a render glitch, never a security issue — cursor pixels are display-only).
pub trait CursorSink: Send + Sync {
    /// A fresh cursor shape — draw it and record it in the cache by `shape.id` (see the cache contract).
    fn set_shape(&self, shape: CursorShape);
    /// Reuse a previously-sent shape by `id` (the host sent no RGBA to save bandwidth). Resolvable
    /// whenever the cache contract above is honored.
    fn set_cached(&self, id: u32);
    /// The OS cursor is hidden — draw nothing.
    fn hide(&self);
}

/// The host-side audit sink (Inv 10, ADR-088): receives **content-free** security events as they happen
/// and records them into the tamper-evident journal ([`ras_audit::AuditJournal`]) + durable store. Unlike
/// the advisory, **lossy** `LifecycleEvent` stream (bounded, drops-on-full), this is **authoritative and
/// must not drop** — so it is a synchronous, non-awaiting call the orchestrator makes at each security
/// point, *before* the equivalent lifecycle event. The impl owns the journal, the clock (timestamps),
/// and persistence. Default DI is a no-op, so a deployment opts into auditing by wiring one.
pub trait AuditSink: Send + Sync {
    /// Record one security event. Must return promptly and never drop it (append-only completeness).
    fn record(&self, event: AuditEvent);
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

/// Content-free context handed to the validator. Carries the transport-authenticated identity, the
/// opaque access-request/grant bytes from `ControlMsg::AuthEnvelope`, and (Phase 2, additive) the
/// host's own identity + the current time so a real validator can enforce the endpoint/host bindings
/// and expiry. `#[non_exhaustive]` so later fields stay additive.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionAuthContext {
    /// The identity the transport authenticated (the peer's iroh `EndpointId`). **Not** authorization
    /// — the grant's `controller_endpoint_id` must equal this (sender-constraint, Inv 3/9/ADR-040).
    pub peer_identity: PeerIdentity,
    /// Opaque payload from `ControlMsg::AuthEnvelope` — the PASETO session grant on the session ALPN.
    /// Empty in an `insecure-no-auth` build (the no-op validator ignores it).
    pub access_request: bytes::Bytes,
    /// This host's own identity (Ed25519 public key). The grant's `host_id`/`issuer` must match.
    pub host_id: [u8; 32],
    /// Current time (ms since epoch) at the authorize gate — for `not_before`/`expires_at`.
    pub now: u64,
}

/// The validator's verdict.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GrantDecision {
    /// Proceed → orchestrator emits `SessionEvent::Authorized`. Carries the **granted capability set**
    /// (Phase 2) so the session starts knowing its scope for the per-message checks (Inv 15 / ADR-041);
    /// the MVP grants only view-only caps, but the enforcement path exists for Phase-3 input.
    Authorized(CapabilitySet),
    /// Interactive consent pending (Phase 2): hold in `ControlEstablished` until re-driven.
    NeedConsent,
    /// Multi-step challenge/response (Phase 2 replay/nonce).
    Challenge(bytes::Bytes),
    /// Refused → `SessionEvent::Reject { code }`.
    Denied(ErrorCode),
}

// ---------------------------------------------------------------------------------------------
// Phase-3 control-lease consent seam (Invariant 1). Requesting OS **input** is a distinct, higher-
// stakes act than viewing, so it re-prompts the local user before a lease is issued. Object-safe so
// it is injectable as `Arc<dyn ControlConsent>`.
// ---------------------------------------------------------------------------------------------

/// The local-user consent hook for an OS-input control request (Invariant 1 — the local user is the
/// final owner; a controller never self-authorizes). Given the capabilities a controller requested,
/// it returns the **subset** the local user consents to (empty ⇒ denied). The host then clamps that
/// subset again against the session grant and policy (`ras-policy::grantable`) — consent can only
/// *narrow*, never widen. Fail-closed: a timeout or dismissal returns the empty set.
#[async_trait]
pub trait ControlConsent: Send + Sync {
    /// Prompt the local user; return the consented subset of `requested` (empty ⇒ denied).
    async fn consent_to_control(&self, requested: &CapabilitySet) -> CapabilitySet;
}

/// Fail-closed default: with no consent seam wired, **no** OS-input lease is ever granted. A host that
/// wants input must inject a real [`ControlConsent`] (the app's local Allow/Deny prompt).
pub struct DenyAllControl;

#[async_trait]
impl ControlConsent for DenyAllControl {
    async fn consent_to_control(&self, _requested: &CapabilitySet) -> CapabilitySet {
        CapabilitySet::new()
    }
}

/// The local-user consent hook for a **file push** (ADR-086, per-transfer confirmation, Inv 1). Even a
/// controller holding the `file.push.<target>` capability still needs a live local Allow — file transfer
/// is the danger channel. Given the (already catalogue+capability-authorized) target, leaf filename, and
/// size, return whether the local user permits *this* transfer. Fail-closed: a timeout/dismissal ⇒ false.
#[async_trait]
pub trait FileConsent: Send + Sync {
    /// Prompt the local user for this specific push; `true` ⇒ allowed.
    async fn consent_to_file(&self, target: &str, filename: &str, size: u64) -> bool;
}

/// Fail-closed default: with no seam wired, **no** file push is ever accepted (even a catalogued,
/// capability-granted one). A host that wants file transfer injects a real [`FileConsent`].
pub struct DenyAllFileConsent;

#[async_trait]
impl FileConsent for DenyAllFileConsent {
    async fn consent_to_file(&self, _target: &str, _filename: &str, _size: u64) -> bool {
        false
    }
}

/// Where an accepted file transfer's bytes are written (ADR-090). Implemented by the OS backend and a
/// recorder in tests. The `dest` is **host-resolved** (a validated child of the target's sandbox, never
/// a controller path); the impl **MUST** open it with `O_NOFOLLOW` / `openat` and refuse a symlink — the
/// ADR-086 symlink-follow (TOCTOU) defense that the safe-leaf path string is the precondition for. Sync
/// (a chunk write is fast); one transfer at a time.
pub trait FileWriteSink: Send + Sync {
    /// Open `dest` for a new transfer of `size` bytes. `O_NOFOLLOW` + create-new (never follow/overwrite
    /// a symlink or existing file).
    ///
    /// # Errors
    /// Filesystem/permission failure, or the path resolves to a symlink/existing entry.
    fn open(&self, dest: &std::path::Path, size: u64) -> Result<(), CoreError>;
    /// Append one chunk to the open destination.
    ///
    /// # Errors
    /// Write failure.
    fn write(&self, data: &[u8]) -> Result<(), CoreError>;
    /// Finalize the completed file (fsync + close).
    ///
    /// # Errors
    /// Flush/close failure.
    fn finish(&self) -> Result<(), CoreError>;
    /// Abort and discard the partial file (error / oversize / teardown). Idempotent, never fails.
    fn abort(&self);
}

/// The **real** session-phase authorization gate (Phase 2). Parses `access_request` as the PASETO
/// v4.public session grant and calls [`ras_grant::validate_grant`] against the endpoint the transport
/// just authenticated — enforcing the sender-constraint (ADR-040) at the exact moment the endpoint is
/// proven. Stateless: every input comes from the [`SessionAuthContext`], so a future control-plane
/// validator (different verifying key) is a sibling impl, not a change here.
///
/// This is the local-host validator (issuer == host), so the grant's issuer key == `ctx.host_id`.
pub struct GrantSessionValidator;

#[async_trait]
impl GrantValidator for GrantSessionValidator {
    async fn authorize(&self, ctx: &SessionAuthContext) -> Result<GrantDecision, CoreError> {
        match ras_grant::validate_grant(
            &ctx.access_request,
            &ctx.host_id,
            &ctx.host_id, // MVP: issuer == host, so the verifying key is the host id
            &ctx.peer_identity.0,
            ctx.now,
        ) {
            Ok(grant) => Ok(GrantDecision::Authorized(grant.granted_capabilities)),
            // No oracle beyond the stable code; the session lands on `Rejected`.
            Err(code) => Ok(GrantDecision::Denied(code)),
        }
    }
}

/// PHASE-1 ONLY. Returns `Authorized` (with an empty capability set) unconditionally. Gated behind
/// `insecure-no-auth` so it can never link into an auth build.
#[cfg(feature = "insecure-no-auth")]
pub struct AllowAllValidator;

#[cfg(feature = "insecure-no-auth")]
#[async_trait]
impl GrantValidator for AllowAllValidator {
    async fn authorize(&self, _ctx: &SessionAuthContext) -> Result<GrantDecision, CoreError> {
        Ok(GrantDecision::Authorized(CapabilitySet::new()))
    }
}
