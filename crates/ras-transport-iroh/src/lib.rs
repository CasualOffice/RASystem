//! Casual RAS Iroh/QUIC transport adapter — **interfaces** (Phase 1).
//!
//! Transport authenticates *identity* (which `EndpointId`), never *authorization* (Invariant 9,
//! `docs/09`). It owns the reliability-split channel map: a stalled video path can never block the
//! control channel or a health read (the load-bearing latency invariant). Concrete iroh wiring
//! (endpoint, streams, datagrams, relay) lands in Phase 1 execution behind these types; the
//! `iroh` dependency is added then. Newtypes wrap `[u8; 32]` so downstream crates never depend on
//! `iroh` directly.

use bytes::BytesMut;
use ras_media::{EncodedFrame, FrameId};
use ras_protocol::{ControlMsg, ErrorCode, RasError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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

/// DoS guard on hostile control input. Re-exported from `ras-protocol` (its single home) so the
/// codec's framing guard and this crate's limit can never drift apart.
pub use ras_protocol::MAX_CONTROL_FRAME;

/// Reliable, ordered control channel over one bidi QUIC stream (loss-intolerant → never datagrams).
/// The iroh-specific handle; its send/recv delegate to a [`FramedControlChannel`] over the iroh
/// `SendStream`/`RecvStream` once the endpoint is wired. Kept as a distinct type so the iroh
/// dependency stays quarantined here.
#[derive(Clone)]
pub struct ControlChannel;

impl ControlChannel {
    /// Send one control message.
    pub async fn send(&self, msg: ControlMsg) -> Result<(), TransportError> {
        let _ = msg;
        todo!("delegate to FramedControlChannel over the iroh bidi stream")
    }

    /// Await the next control message.
    pub async fn recv(&self) -> Result<ControlMsg, TransportError> {
        todo!("delegate to FramedControlChannel over the iroh bidi stream")
    }
}

/// Reliable, ordered control channel that runs the `ras-protocol` framing codec
/// (`u32-BE length | protobuf(ControlMsg)`) over **any** async byte streams. That is exactly the
/// shape of iroh's `(RecvStream, SendStream)` pair, so wiring iroh is `FramedControlChannel::new(recv,
/// send)` — and it is fully testable today over an in-memory duplex.
///
/// The read side buffers across calls so a frame split across multiple reads reassembles, and the
/// codec's [`MAX_CONTROL_FRAME`] guard rejects an oversized length prefix **before** the body is
/// buffered (DoS-safe). This channel carries no grant/lease payloads — those ride opaque in
/// [`ControlMsg::AuthEnvelope`] (Invariant 9).
pub struct FramedControlChannel<R, W> {
    reader: R,
    writer: W,
    read_buf: BytesMut,
}

impl<R, W> FramedControlChannel<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    /// Build over a read half and a write half (e.g. iroh `RecvStream` + `SendStream`).
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader,
            writer,
            read_buf: BytesMut::with_capacity(4096),
        }
    }

    /// Frame and send one control message, flushing so the peer observes it promptly.
    pub async fn send(&mut self, msg: &ControlMsg) -> Result<(), TransportError> {
        let framed = ras_protocol::codec::frame(msg);
        self.writer.write_all(&framed).await.map_err(|_| {
            RasError::recoverable(ErrorCode::TransportError, "control write failed")
        })?;
        self.writer.flush().await.map_err(|_| {
            RasError::recoverable(ErrorCode::TransportError, "control flush failed")
        })?;
        Ok(())
    }

    /// Await the next complete control message. Reads incrementally into the buffer; the codec's
    /// `MAX_CONTROL_FRAME` guard fires on the length prefix before an oversized body is read. A clean
    /// peer close (EOF with an empty buffer) and a truncated frame both surface as a typed error.
    pub async fn recv(&mut self) -> Result<ControlMsg, TransportError> {
        loop {
            if let Some(msg) = ras_protocol::codec::try_read_frame(&mut self.read_buf)? {
                return Ok(msg);
            }
            let mut chunk = [0u8; 4096];
            let n = self.reader.read(&mut chunk).await.map_err(|_| {
                RasError::recoverable(ErrorCode::TransportError, "control read failed")
            })?;
            if n == 0 {
                return Err(RasError::recoverable(
                    ErrorCode::TransportError,
                    "control channel closed",
                ));
            }
            self.read_buf.extend_from_slice(&chunk[..n]);
        }
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

#[cfg(test)]
mod framed_control_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use tokio::io::{split, AsyncWriteExt};

    /// Build a wired pair of `FramedControlChannel`s over one in-memory duplex — models iroh's two
    /// crossed streams (what A writes, B reads).
    fn pair() -> (
        FramedControlChannel<impl AsyncRead + Unpin + Send, impl AsyncWrite + Unpin + Send>,
        FramedControlChannel<impl AsyncRead + Unpin + Send, impl AsyncWrite + Unpin + Send>,
    ) {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, aw) = split(a);
        let (br, bw) = split(b);
        (
            FramedControlChannel::new(ar, aw),
            FramedControlChannel::new(br, bw),
        )
    }

    #[tokio::test]
    async fn round_trips_a_message_both_directions() {
        let (mut a, mut b) = pair();
        a.send(&ControlMsg::Hello {
            protocol_version: 1,
        })
        .await
        .unwrap();
        let got = b.recv().await.unwrap();
        assert!(matches!(
            got,
            ControlMsg::Hello {
                protocol_version: 1
            }
        ));

        // Reverse direction on the same pair.
        b.send(&ControlMsg::Bye {
            code: ErrorCode::SessionRevoked,
        })
        .await
        .unwrap();
        let got = a.recv().await.unwrap();
        assert!(matches!(
            got,
            ControlMsg::Bye {
                code: ErrorCode::SessionRevoked
            }
        ));
    }

    #[tokio::test]
    async fn reassembles_multiple_back_to_back_frames() {
        let (mut a, mut b) = pair();
        for v in [10u32, 20, 30] {
            a.send(&ControlMsg::Hello {
                protocol_version: v,
            })
            .await
            .unwrap();
        }
        for v in [10u32, 20, 30] {
            match b.recv().await.unwrap() {
                ControlMsg::Hello { protocol_version } => assert_eq!(protocol_version, v),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn reassembles_a_frame_split_across_reads() {
        // Write a valid frame's bytes in two chunks with a gap, so recv must loop and reassemble.
        let (a, b) = tokio::io::duplex(8192);
        let (ar, aw) = split(a);
        let (_br, mut bw) = split(b);
        let mut chan = FramedControlChannel::new(ar, aw);

        let framed = ras_protocol::codec::frame(&ControlMsg::KeyframeRequest(
            ras_protocol::KeyframeRequest {
                since_frame: 7,
                reason: ras_protocol::KeyframeReason::DecoderReset,
            },
        ));
        let (head, tail) = framed.split_at(3); // split mid-header
        let head = head.to_vec();
        let tail = tail.to_vec();
        tokio::spawn(async move {
            bw.write_all(&head).await.unwrap();
            tokio::task::yield_now().await;
            bw.write_all(&tail).await.unwrap();
        });
        match chan.recv().await.unwrap() {
            ControlMsg::KeyframeRequest(k) => assert_eq!(k.since_frame, 7),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected() {
        let (a, b) = tokio::io::duplex(64);
        let (ar, aw) = split(a);
        let (_br, mut bw) = split(b);
        let mut chan = FramedControlChannel::new(ar, aw);
        // A length prefix beyond MAX_CONTROL_FRAME, no body — the DoS guard must fire.
        let oversized = u32::try_from(MAX_CONTROL_FRAME + 1).unwrap();
        bw.write_all(&oversized.to_be_bytes()).await.unwrap();
        let err = chan.recv().await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[tokio::test]
    async fn peer_close_surfaces_as_error() {
        let (a, b) = tokio::io::duplex(64);
        drop(b); // peer gone
        let (ar, aw) = split(a);
        let mut chan = FramedControlChannel::new(ar, aw);
        let err = chan.recv().await.unwrap_err();
        assert_eq!(err.code, ErrorCode::TransportError);
    }

    // -----------------------------------------------------------------------------------------
    // Adversarial coverage of the reader — the code that will parse untrusted bytes off iroh's
    // streams. A tiny deterministic PRNG (xorshift64) keeps these reproducible without a fuzz dep.
    // -----------------------------------------------------------------------------------------

    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        /// A value in `0..n` (n > 0).
        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
        fn byte(&mut self) -> u8 {
            (self.next_u64() & 0xff) as u8
        }
    }

    /// A representative valid message set for round-trip fuzzing.
    fn sample_messages() -> Vec<ControlMsg> {
        use ras_protocol::{DecoderFeedback, KeyframeReason, KeyframeRequest};
        vec![
            ControlMsg::Hello {
                protocol_version: 1,
            },
            ControlMsg::KeyframeRequest(KeyframeRequest {
                since_frame: 42,
                reason: KeyframeReason::UnrecoverableLoss,
            }),
            ControlMsg::Feedback(DecoderFeedback {
                last_decoded_frame: 1 << 40, // > 2^32, exercises the u64 path
                frames_dropped: 3,
                decode_latency_us: 900,
                keyframe_request: None,
            }),
            ControlMsg::Bye {
                code: ErrorCode::NormalClosure,
            },
        ]
    }

    /// Feeding arbitrary bytes to the reader never panics and never hangs: every case ends in a
    /// decoded frame or a typed error, and EOF guarantees termination.
    #[tokio::test]
    async fn adversarial_byte_streams_never_panic() {
        for seed in 1..=256u64 {
            let mut rng = Rng::new(seed);
            let len = rng.below(600);
            let blob: Vec<u8> = (0..len).map(|_| rng.byte()).collect();

            let (a, b) = tokio::io::duplex(8192);
            let (ar, aw) = split(a);
            let (br, mut bw) = split(b);
            drop(br);
            let mut reader = FramedControlChannel::new(ar, aw);

            bw.write_all(&blob).await.unwrap();
            bw.shutdown().await.unwrap(); // EOF → recv must terminate

            // Drain to completion: a valid-looking frame may decode, but EOF forces a terminal error.
            let mut guard = 0;
            loop {
                guard += 1;
                assert!(guard < 10_000, "reader failed to terminate on seed {seed}");
                match reader.recv().await {
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }
    }

    /// A stream of valid frames reassembles correctly no matter how the bytes are chunked — down to
    /// one byte at a time and up to several frames per write.
    #[tokio::test]
    async fn framed_messages_survive_adversarial_chunking() {
        let msgs = sample_messages();
        for seed in 1..=64u64 {
            let mut wire = Vec::new();
            for m in &msgs {
                wire.extend_from_slice(&ras_protocol::codec::frame(m));
            }

            let (a, b) = tokio::io::duplex(64 * 1024);
            let (ar, aw) = split(a);
            let (br, mut bw) = split(b);
            drop(br);
            let mut reader = FramedControlChannel::new(ar, aw);

            let writer = tokio::spawn(async move {
                let mut rng = Rng::new(seed ^ 0xA5A5);
                let mut off = 0;
                while off < wire.len() {
                    let step = 1 + rng.below(9); // 1..=9 bytes per write
                    let end = (off + step).min(wire.len());
                    bw.write_all(&wire[off..end]).await.unwrap();
                    off = end;
                    tokio::task::yield_now().await;
                }
                bw.shutdown().await.unwrap();
            });

            for expected in &msgs {
                let got = reader
                    .recv()
                    .await
                    .expect("a framed message must decode under any chunking");
                assert_eq!(
                    ras_protocol::codec::frame(&got),
                    ras_protocol::codec::frame(expected),
                    "message mismatch under chunking (seed {seed})"
                );
            }
            writer.await.unwrap();
        }
    }

    /// An oversized length prefix doesn't just error once — it leaves the stream permanently refused
    /// (the guard never consumes the bad prefix), so every later `recv` re-errors without blocking on
    /// a body or resyncing. This is the "a garbage length kills the connection" posture; the caller
    /// drops the session rather than trying to recover a desynced attacker-controlled stream.
    #[tokio::test]
    async fn oversized_prefix_leaves_stream_permanently_refused() {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, aw) = split(a);
        let (br, mut bw) = split(b);
        drop(br);
        let mut reader = FramedControlChannel::new(ar, aw);

        let oversized = u32::try_from(MAX_CONTROL_FRAME + 1).unwrap();
        bw.write_all(&oversized.to_be_bytes()).await.unwrap();
        // Deliberately do NOT close the stream — an attacker keeps it open after the bad prefix.

        for _ in 0..3 {
            let err = reader.recv().await.unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidMessage);
        }
    }
}
