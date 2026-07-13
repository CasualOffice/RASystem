//! Casual RAS Iroh/QUIC transport adapter — **interfaces** (Phase 1).
//!
//! Transport authenticates *identity* (which `EndpointId`), never *authorization* (Invariant 9,
//! `docs/09`). It owns the reliability-split channel map: a stalled video path can never block the
//! control channel or a health read (the load-bearing latency invariant). Concrete iroh wiring
//! (endpoint, streams, datagrams, relay) lands in Phase 1 execution behind these types; the
//! `iroh` dependency is added then. Newtypes wrap `[u8; 32]` so downstream crates never depend on
//! `iroh` directly.

use ras_media::{EncodedFrame, FrameId};
use ras_protocol::{ControlMsg, ErrorCode};

/// This crate's error alias over the shared taxonomy.
pub type TransportError = ras_protocol::RasError;

/// Ed25519 public key of a peer (newtype over `iroh::EndpointId`, the 1.x rename of `NodeId`).
/// This is identity — authenticates *who*, never *what they may do*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EndpointId(pub [u8; 32]);

/// Dialable address: an [`EndpointId`] plus optional relay/direct hints (newtype over
/// `iroh::EndpointAddr`).
#[derive(Debug, Clone)]
pub struct EndpointAddr {
    /// The peer's identity.
    pub id: EndpointId,
    // relay + direct-address hints added with the iroh wiring.
}

/// Direct (hole-punched) vs relayed vs migrating. A controller `match` must handle `Migrating`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Hole-punched direct path.
    Direct,
    /// Via a relay (~10% of sessions).
    Relayed,
    /// Path change in flight.
    Migrating,
}

/// Link lifecycle, including the Rust-side watchdog `Stalled` (no frame for N ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Establishing.
    Connecting,
    /// Live and delivering.
    Live,
    /// No frames within the watchdog window.
    Stalled,
    /// Reconnecting within the window.
    Reconnecting,
    /// Closed/terminal.
    Closed,
}

/// The one connection-health snapshot, sourced from iroh/Quinn stats + path events. Consumed by
/// the host ABR loop and the controller status badge (both as projections).
///
/// Unit discipline: `rtt_us` is **microseconds** (`u32`); `loss_fraction` is a **fraction**
/// `0.0..=1.0` (`f32`). Convert to ms/percent for display only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnHealth {
    /// Direct / relayed / migrating.
    pub path: PathKind,
    /// Smoothed round-trip time, microseconds.
    pub rtt_us: u32,
    /// Estimated loss fraction over a recent window (drives FEC strength + ABR).
    pub loss_fraction: f32,
    /// Congestion-window-derived deliverable rate, bits/sec — the ABR bitrate ceiling.
    pub estimated_bandwidth_bps: u32,
    /// Frames dropped at the sink since the last snapshot (sender-side pressure signal).
    pub frames_dropped: u32,
    /// Link lifecycle.
    pub state: LinkState,
}

/// DoS guard on hostile control input. 1 MiB is ample for config/feedback.
pub const MAX_CONTROL_FRAME: usize = 1 << 20;

/// Reliable, ordered control channel over one bidi QUIC stream (loss-intolerant → never datagrams).
/// Framed as `u32-BE length | protobuf(ControlMsg)`.
#[derive(Clone)]
pub struct ControlChannel;

impl ControlChannel {
    /// Send one control message.
    pub async fn send(&self, msg: ControlMsg) -> Result<(), TransportError> {
        let _ = msg;
        todo!("length-prefix + protobuf encode over the bidi stream")
    }

    /// Await the next control message.
    pub async fn recv(&self) -> Result<ControlMsg, TransportError> {
        todo!("read length-prefixed protobuf; reject > MAX_CONTROL_FRAME")
    }
}

/// Source-side send result → feeds the pacer's "drop-to-keyframe at the source" decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Frame handed to the transport.
    Sent,
    /// Dropped: a newer frame superseded this stale one.
    DroppedStale,
    /// Dropped: the path is congested.
    DroppedCongested,
}

/// Why a received frame was abandoned (non-fatal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Superseded before it completed.
    Stale,
    /// FEC could not recover it.
    FecUnrecoverable,
    /// Its per-frame stream was reset.
    StreamReset,
    /// Not enough fragments arrived in time.
    MissingFragments,
}

/// A video receive event. Loss is first-class and non-fatal.
#[derive(Debug)]
pub enum VideoEvent {
    /// A complete Annex-B access unit ready for the decoder.
    Frame(EncodedFrame),
    /// A frame was abandoned. `ras-core` turns a run of these into one keyframe request rather than
    /// freezing — last-good frame stays on screen; controller cursor/controls untouched.
    FrameDropped {
        /// Which frame was lost.
        frame_id: FrameId,
        /// Why.
        reason: DropReason,
    },
}

/// Host-side droppable video sender. Non-blocking: if the path can't keep up, frames are dropped at
/// the sink, never queued unbounded.
pub struct VideoSink;

impl VideoSink {
    /// Fragment (if needed) and send one frame. Returns immediately; does not await delivery.
    /// `Err` only on fatal path error (connection gone); ordinary loss is a non-error
    /// [`SendOutcome`].
    pub fn send_frame(&self, frame: EncodedFrame) -> Result<SendOutcome, TransportError> {
        let _ = frame;
        todo!("reset stale in-flight frame; fragment; send via the negotiated VideoTransport")
    }
}

/// Controller-side droppable video receiver. Reassembles fragments/FEC into whole frames and
/// surfaces loss as a first-class, non-fatal event. The decoder (not the transport) owns
/// reorder-by-`frame_id`.
pub struct VideoSource;

impl VideoSource {
    /// Await the next video event.
    pub async fn recv(&self) -> Result<VideoEvent, TransportError> {
        todo!("reassemble per the negotiated VideoTransport")
    }
}

/// Swappable video-transport strategy. Both patterns implement this; the concrete one is chosen at
/// session start from measured path conditions / spike results and pinned into `StreamConfig`.
/// This trait is the seam that lets the spike change the answer without changing any caller.
pub trait VideoTransport: Send + Sync {
    /// Which pattern this is.
    fn kind(&self) -> ras_media::VideoTransportKind;
    /// Send one frame (source-side, non-blocking).
    fn send(&self, frame: &EncodedFrame) -> Result<SendOutcome, TransportError>;
    /// Await the next received video event.
    fn poll_recv(
        &self,
    ) -> impl core::future::Future<Output = Result<VideoEvent, TransportError>> + Send;
}

/// App-level fragment header prepended to every video datagram (DatagramFec). Fixed-size, packed.
/// Wire: `[frame_id:u64 | frag_index:u16 | frag_count:u16 | fec_k:u16 | fec_n:u16 | flags:u8]`.
#[derive(Debug, Clone, Copy)]
pub struct VideoFragHeader {
    /// Reassembly key + staleness/ordering clock.
    pub frame_id: u64,
    /// `0..frag_count` (data), then FEC repair shards.
    pub frag_index: u16,
    /// Data fragments for this frame.
    pub frag_count: u16,
    /// Reed-Solomon data shards.
    pub fec_k: u16,
    /// Total shards (`n - k` = repair); depth = 1 frame.
    pub fec_n: u16,
    /// bit0 = keyframe.
    pub flags: u8,
}

/// An established, identity-authenticated session over one iroh connection. Owns the
/// reliability-split channel map. The pointer channel is deferred out of Phase 1 (view-only).
pub struct Session;

impl Session {
    /// The remote peer's authenticated identity (not authorization).
    #[must_use]
    pub fn remote(&self) -> EndpointId {
        todo!("iroh Connection remote EndpointId")
    }
    /// The reliable control channel.
    #[must_use]
    pub fn control(&self) -> ControlChannel {
        todo!()
    }
    /// Host-side video sink (present on the host role).
    #[must_use]
    pub fn video_sink(&self) -> Option<VideoSink> {
        todo!()
    }
    /// Controller-side video source (present on the controller role).
    #[must_use]
    pub fn video_source(&self) -> Option<VideoSource> {
        todo!()
    }
    /// Lock-free health observable.
    #[must_use]
    pub fn health(&self) -> HealthObserver {
        todo!()
    }
    /// Close the session with a reason code.
    pub async fn close(self, code: ErrorCode) {
        let _ = code;
        todo!()
    }
}

/// Read-only, lock-free connection-health observable (a `watch` receiver). A stalled video path
/// never blocks a health read.
#[derive(Clone)]
pub struct HealthObserver;

impl HealthObserver {
    /// The latest snapshot; never blocks on the network.
    #[must_use]
    pub fn snapshot(&self) -> ConnHealth {
        todo!()
    }
    /// Await the next health change (UI reactivity, not the hot path).
    pub async fn changed(&mut self) -> ConnHealth {
        todo!()
    }
}
