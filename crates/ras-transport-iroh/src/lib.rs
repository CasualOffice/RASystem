//! Casual RAS Iroh/QUIC transport adapter — **interfaces** (Phase 1).
//!
//! Transport authenticates *identity* (which `EndpointId`), never *authorization* (Invariant 9,
//! `docs/09`). It owns the reliability-split channel map: a stalled video path can never block the
//! control channel or a health read (the load-bearing latency invariant). Concrete iroh wiring
//! (endpoint, streams, datagrams, relay) lands in Phase 1 execution behind these types; the
//! `iroh` dependency is added then. Newtypes wrap `[u8; 32]` so downstream crates never depend on
//! `iroh` directly.

use core::time::Duration;
use std::sync::Mutex;

use bytes::{Bytes, BytesMut};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream, VarInt};
use iroh::{
    Endpoint as IrohEndpoint, EndpointAddr as IrohEndpointAddr, EndpointId as IrohEndpointId,
};
use ras_media::{ColorSpace, EncodedFrame, FrameId, StreamConfig, VideoCodec, VideoTransportKind};
use ras_protocol::{BootstrapMsg, ControlMsg, ErrorCode, RasError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// This crate's error alias over the shared taxonomy.
pub type TransportError = ras_protocol::RasError;

/// Transport ALPN — protocol identity + version negotiated in the QUIC/TLS handshake. Peers with a
/// mismatched ALPN cannot connect (fail-closed at the TLS layer, before any app bytes). Bumped only
/// on a breaking transport-wire change (ADR-059).
pub const ALPN: &[u8] = b"casual-ras/1";

/// Bootstrap-phase ALPN (Phase 2) — the `AccessRequest → consent → grant` handshake runs on this,
/// separate from the session [`ALPN`]. Keeping them distinct ALPNs means the host's accept loop
/// routes a connection to the bootstrap handler vs. the session handler by the TLS-negotiated
/// protocol id, and a peer can never present bootstrap traffic on the session path or vice versa.
pub const BOOTSTRAP_ALPN: &[u8] = b"casual-ras/bootstrap/1";

/// Ed25519 public key of a peer (newtype over `iroh::EndpointId`, the 1.x rename of `NodeId`).
/// This is identity — authenticates *who*, never *what they may do*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EndpointId(pub [u8; 32]);

/// Dialable address: an [`EndpointId`] plus optional relay + direct-address hints (newtype over
/// `iroh::EndpointAddr`, so nothing iroh-typed leaks). This is what a **connection ticket** carries:
/// the controller reconstructs one from the host's ticket string and dials it. Identity is
/// authenticated by the QUIC/TLS handshake regardless of which hint path (direct/relay/discovery)
/// actually connects — the address hints only affect *reachability*, never *authority* (Invariant 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointAddr {
    /// The peer's identity.
    pub id: EndpointId,
    /// Known direct socket addresses (hole-punch / same-LAN candidates). May be stale; iroh falls
    /// back to relay + discovery-by-id when they don't reach.
    pub direct_addrs: Vec<std::net::SocketAddr>,
    /// The peer's home relay URL, if known — the always-reachable fallback path across NAT.
    pub relay_url: Option<String>,
}

/// Human-transferable connection-ticket prefix + version. A ticket is `CASUALRAS1:<hex>` where the
/// hex payload is [`EndpointAddr`] in the fixed binary layout below. Bumped only on a breaking
/// ticket-layout change.
const TICKET_PREFIX: &str = "CASUALRAS1:";

impl EndpointAddr {
    /// An identity-only address (no reachability hints): dialing relies purely on iroh's
    /// discovery-by-id + relay. Handy in tests and as a minimal ticket.
    #[must_use]
    pub fn new(id: EndpointId) -> Self {
        Self {
            id,
            direct_addrs: Vec::new(),
            relay_url: None,
        }
    }

    /// Encode as a copy-pasteable **connection ticket** string (`CASUALRAS1:<hex>`). The payload is,
    /// all lengths/ports big-endian:
    /// `id[32] | relay_len:u16 | relay_utf8[relay_len] | addr_count:u8 | (fam:u8 | ip[4|16] | port:u16)*`.
    #[must_use]
    pub fn to_ticket(&self) -> String {
        let mut buf = Vec::with_capacity(48);
        buf.extend_from_slice(&self.id.0);
        let relay = self.relay_url.as_deref().unwrap_or("");
        let relay_len = u16::try_from(relay.len()).unwrap_or(0);
        buf.extend_from_slice(&relay_len.to_be_bytes());
        buf.extend_from_slice(&relay.as_bytes()[..relay_len as usize]);
        let count = u8::try_from(self.direct_addrs.len()).unwrap_or(u8::MAX);
        buf.push(count);
        for sa in self.direct_addrs.iter().take(count as usize) {
            match sa.ip() {
                std::net::IpAddr::V4(v4) => {
                    buf.push(4);
                    buf.extend_from_slice(&v4.octets());
                }
                std::net::IpAddr::V6(v6) => {
                    buf.push(6);
                    buf.extend_from_slice(&v6.octets());
                }
            }
            buf.extend_from_slice(&sa.port().to_be_bytes());
        }
        let mut s = String::with_capacity(TICKET_PREFIX.len() + buf.len() * 2);
        s.push_str(TICKET_PREFIX);
        for b in &buf {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
        }
        s
    }

    /// Parse a **connection ticket** produced by [`to_ticket`](Self::to_ticket). Fail-closed: a wrong
    /// prefix, odd/short hex, an over-long field, or trailing garbage is a typed, content-free error —
    /// never a partial or defaulted address.
    pub fn from_ticket(ticket: &str) -> Result<Self, TransportError> {
        let bad =
            || RasError::recoverable(ErrorCode::InvalidMessage, "malformed connection ticket");
        let hex = ticket.strip_prefix(TICKET_PREFIX).ok_or_else(bad)?;
        if hex.len() % 2 != 0 {
            return Err(bad());
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        let h = hex.as_bytes();
        let mut i = 0;
        while i < h.len() {
            let hi = (h[i] as char).to_digit(16).ok_or_else(bad)?;
            let lo = (h[i + 1] as char).to_digit(16).ok_or_else(bad)?;
            bytes.push(((hi << 4) | lo) as u8);
            i += 2;
        }

        // Cursor-based decode; every read is bounds-checked against the remaining buffer.
        let mut c = 0usize;
        let take = |c: &mut usize, n: usize| -> Result<std::ops::Range<usize>, TransportError> {
            let end = c.checked_add(n).ok_or_else(bad)?;
            if end > bytes.len() {
                return Err(bad());
            }
            let r = *c..end;
            *c = end;
            Ok(r)
        };

        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[take(&mut c, 32)?]);
        let rl = take(&mut c, 2)?;
        let relay_len = u16::from_be_bytes([bytes[rl.start], bytes[rl.start + 1]]);
        let relay_url = if relay_len == 0 {
            None
        } else {
            let r = take(&mut c, relay_len as usize)?;
            Some(String::from_utf8(bytes[r].to_vec()).map_err(|_| bad())?)
        };
        let count = bytes[take(&mut c, 1)?.start];
        let mut direct_addrs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let fam = bytes[take(&mut c, 1)?.start];
            let ip: std::net::IpAddr = match fam {
                4 => {
                    let o = &bytes[take(&mut c, 4)?];
                    std::net::Ipv4Addr::new(o[0], o[1], o[2], o[3]).into()
                }
                6 => {
                    let o = &bytes[take(&mut c, 16)?];
                    let mut a = [0u8; 16];
                    a.copy_from_slice(o);
                    std::net::Ipv6Addr::from(a).into()
                }
                _ => return Err(bad()),
            };
            let p = take(&mut c, 2)?;
            let port = u16::from_be_bytes([bytes[p.start], bytes[p.start + 1]]);
            direct_addrs.push(std::net::SocketAddr::new(ip, port));
        }
        if c != bytes.len() {
            return Err(bad()); // trailing garbage
        }
        Ok(Self {
            id: EndpointId(id),
            direct_addrs,
            relay_url,
        })
    }
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

/// Which side of a connection we are — decides who *opens* vs *accepts* each stream (the dialer
/// opens, the acceptor accepts, so both name the same stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Accepted the connection (the controlled machine).
    Host,
    /// Dialed the connection (the technician side).
    Controller,
}

/// A bound iroh endpoint — the local half of the transport. Quarantines `iroh` behind our newtypes:
/// nothing iroh-typed escapes this crate's public API.
pub struct Endpoint {
    inner: IrohEndpoint,
}

impl Endpoint {
    /// Bind a new endpoint advertising both the session [`ALPN`] and the [`BOOTSTRAP_ALPN`] (n0
    /// discovery + default relay preset). Advertising both lets one endpoint accept a bootstrap
    /// connection and a session connection and route them by their negotiated ALPN.
    pub async fn bind() -> Result<Self, TransportError> {
        let inner = IrohEndpoint::builder(presets::N0)
            .alpns(vec![ALPN.to_vec(), BOOTSTRAP_ALPN.to_vec()])
            .bind()
            .await
            .map_err(|_| RasError::fatal(ErrorCode::TransportError, "endpoint bind failed"))?;
        Ok(Self { inner })
    }

    /// This endpoint's own authenticated identity.
    #[must_use]
    pub fn id(&self) -> EndpointId {
        EndpointId(*self.inner.id().as_bytes())
    }

    /// The local bound socket address(es) — direct-path hints for a same-network / test dialer.
    #[must_use]
    pub fn bound_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.inner.bound_sockets()
    }

    /// Block until this endpoint has contacted its home relay, so [`addr`](Self::addr) contains a
    /// relay hint that makes the endpoint dialable across NAT. Call before publishing a ticket.
    pub async fn online(&self) {
        self.inner.online().await;
    }

    /// This endpoint's current **dialable** address — identity + home relay + observed direct
    /// addresses — for building a connection ticket ([`EndpointAddr::to_ticket`]). Call
    /// [`online`](Self::online) first so the relay hint is populated.
    #[must_use]
    pub fn addr(&self) -> EndpointAddr {
        let a = self.inner.addr();
        let id = EndpointId(*a.id.as_bytes());
        let direct_addrs: Vec<std::net::SocketAddr> = a.ip_addrs().copied().collect();
        let relay_url = a.relay_urls().next().map(ToString::to_string);
        EndpointAddr {
            id,
            direct_addrs,
            relay_url,
        }
    }

    /// Dial a peer from a full [`EndpointAddr`] (controller role): try its direct-address + relay
    /// hints, and fall back to n0 discovery-by-id when they don't reach. QUIC/TLS authenticates the
    /// peer's identity — never its authorization (Invariant 9).
    pub async fn connect(&self, target: &EndpointAddr) -> Result<Session, TransportError> {
        self.dial(self.full_addr(target)?, ALPN).await
    }

    /// Dial a peer by explicit direct address(es), bypassing discovery — the same-network / loopback
    /// path (and what the hermetic tests use). Relay/NAT-traversal dialing rides [`Self::connect`].
    pub async fn connect_direct(
        &self,
        id: &EndpointId,
        addrs: &[std::net::SocketAddr],
    ) -> Result<Session, TransportError> {
        self.dial(self.direct_addr(id, addrs)?, ALPN).await
    }

    /// Dial a peer on the **bootstrap** ALPN (Phase 2 authorization handshake). Same reachability as
    /// [`Self::connect`] but negotiates [`BOOTSTRAP_ALPN`], so the host routes it to the bootstrap
    /// handler. Identity is authenticated by QUIC/TLS, never authorization (Invariant 9).
    pub async fn connect_bootstrap(
        &self,
        target: &EndpointAddr,
    ) -> Result<Session, TransportError> {
        self.dial(self.full_addr(target)?, BOOTSTRAP_ALPN).await
    }

    /// Dial the bootstrap ALPN by explicit direct address(es) (the hermetic-test / same-network path).
    pub async fn connect_direct_bootstrap(
        &self,
        id: &EndpointId,
        addrs: &[std::net::SocketAddr],
    ) -> Result<Session, TransportError> {
        self.dial(self.direct_addr(id, addrs)?, BOOTSTRAP_ALPN)
            .await
    }

    /// Build an iroh address from a full [`EndpointAddr`] (id + direct + relay hints).
    fn full_addr(&self, target: &EndpointAddr) -> Result<IrohEndpointAddr, TransportError> {
        let peer = iroh_id(&target.id)?;
        let mut addr = target
            .direct_addrs
            .iter()
            .copied()
            .fold(IrohEndpointAddr::new(peer), |a, s| a.with_ip_addr(s));
        if let Some(url) = &target.relay_url {
            if let Ok(relay) = url.parse() {
                addr = addr.with_relay_url(relay);
            }
        }
        Ok(addr)
    }

    /// Build an iroh address from an id + explicit direct socket address(es), bypassing discovery.
    fn direct_addr(
        &self,
        id: &EndpointId,
        addrs: &[std::net::SocketAddr],
    ) -> Result<IrohEndpointAddr, TransportError> {
        let peer = iroh_id(id)?;
        // `TransportAddr` is `#[non_exhaustive]` (not externally constructible); the public
        // `with_ip_addr` builder wraps each socket address into a direct-path hint for us.
        Ok(addrs
            .iter()
            .copied()
            .fold(IrohEndpointAddr::new(peer), |a, s| a.with_ip_addr(s)))
    }

    async fn dial(&self, addr: IrohEndpointAddr, alpn: &[u8]) -> Result<Session, TransportError> {
        let conn = self
            .inner
            .connect(addr, alpn)
            .await
            .map_err(|_| RasError::recoverable(ErrorCode::TransportError, "connect failed"))?;
        Ok(Session {
            conn,
            role: Role::Controller,
        })
    }

    /// Accept the next inbound session (host role). `Ok(None)` once the endpoint is closed.
    pub async fn accept(&self) -> Result<Option<Session>, TransportError> {
        let Some(incoming) = self.inner.accept().await else {
            return Ok(None);
        };
        let conn = incoming
            .await
            .map_err(|_| RasError::recoverable(ErrorCode::TransportError, "accept failed"))?;
        Ok(Some(Session {
            conn,
            role: Role::Host,
        }))
    }

    /// Close the endpoint and all its sessions.
    pub async fn close(&self) {
        self.inner.close().await;
    }
}

/// Convert our identity newtype into an iroh `EndpointId`, rejecting a malformed key.
fn iroh_id(id: &EndpointId) -> Result<IrohEndpointId, TransportError> {
    IrohEndpointId::from_bytes(&id.0)
        .map_err(|_| RasError::fatal(ErrorCode::IdentityMismatch, "invalid peer identity"))
}

/// Reliable, ordered control channel over one bidi QUIC stream (loss-intolerant → never datagrams).
/// Delegates to the [`FramedControlChannel`] codec over the iroh `RecvStream`/`SendStream`; the iroh
/// stream types stay quarantined inside.
pub struct ControlChannel {
    framed: FramedControlChannel<RecvStream, SendStream>,
}

impl ControlChannel {
    /// Send one control message.
    pub async fn send(&mut self, msg: ControlMsg) -> Result<(), TransportError> {
        self.framed.send(&msg).await
    }

    /// Await the next control message.
    pub async fn recv(&mut self) -> Result<ControlMsg, TransportError> {
        self.framed.recv().await
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

/// Reliable, ordered **bootstrap** channel over one bidi QUIC stream, carrying `BootstrapMsg` (the
/// Phase-2 `AccessRequest → consent → grant` handshake). The iroh stream types stay quarantined
/// inside. Runs the same DoS-safe framing as [`ControlChannel`], but with the bootstrap codec.
pub struct BootstrapChannel {
    framed: FramedBootstrapChannel<RecvStream, SendStream>,
}

impl BootstrapChannel {
    /// Send one bootstrap message.
    pub async fn send(&mut self, msg: BootstrapMsg) -> Result<(), TransportError> {
        self.framed.send(&msg).await
    }

    /// Await the next bootstrap message.
    pub async fn recv(&mut self) -> Result<BootstrapMsg, TransportError> {
        self.framed.recv().await
    }
}

/// The [`FramedControlChannel`] analogue for `BootstrapMsg`: runs the `ras-protocol` bootstrap framing
/// codec (`u32-BE length | protobuf(BootstrapMsg)`) over any async byte streams, so it is testable
/// over an in-memory duplex and wires onto iroh's `(RecvStream, SendStream)` unchanged. The read side
/// buffers across reads; the codec's `MAX_CONTROL_FRAME` guard rejects an oversized prefix before the
/// body is read. Payloads (AccessRequest, PASETO grant) stay opaque here (Invariant 9).
pub struct FramedBootstrapChannel<R, W> {
    reader: R,
    writer: W,
    read_buf: BytesMut,
}

impl<R, W> FramedBootstrapChannel<R, W>
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

    /// Frame and send one bootstrap message, flushing so the peer observes it promptly.
    pub async fn send(&mut self, msg: &BootstrapMsg) -> Result<(), TransportError> {
        let framed = ras_protocol::codec::frame_bootstrap(msg);
        self.writer.write_all(&framed).await.map_err(|_| {
            RasError::recoverable(ErrorCode::TransportError, "bootstrap write failed")
        })?;
        self.writer.flush().await.map_err(|_| {
            RasError::recoverable(ErrorCode::TransportError, "bootstrap flush failed")
        })?;
        Ok(())
    }

    /// Await the next complete bootstrap message. Incremental reads; the `MAX_CONTROL_FRAME` guard
    /// fires on the length prefix before an oversized body is read. A clean peer close (EOF, empty
    /// buffer) and a truncated frame both surface as a typed error.
    pub async fn recv(&mut self) -> Result<BootstrapMsg, TransportError> {
        loop {
            if let Some(msg) = ras_protocol::codec::try_read_bootstrap_frame(&mut self.read_buf)? {
                return Ok(msg);
            }
            let mut chunk = [0u8; 4096];
            let n = self.reader.read(&mut chunk).await.map_err(|_| {
                RasError::recoverable(ErrorCode::TransportError, "bootstrap read failed")
            })?;
            if n == 0 {
                return Err(RasError::recoverable(
                    ErrorCode::TransportError,
                    "bootstrap channel closed",
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

/// Largest video access unit we will read off one uni stream (DoS bound on hostile input): a
/// high-resolution IDR is well under this. `read_to_end` aborts a stream that exceeds it — the frame
/// is dropped, the connection survives.
pub const MAX_VIDEO_FRAME: usize = 8 * 1024 * 1024;

/// Fixed 44-byte header prepended to each per-frame uni stream (`PerFrameStream` transport, ADR-060).
/// Carries everything needed to reconstruct an [`EncodedFrame`] *except* its bytes, which are the
/// remainder of the stream (delimited by the QUIC FIN). The [`StreamConfig`] travels **per frame**
/// because the video path is droppable/out-of-order: a resolution change must arrive atomically with
/// the IDR it applies to. All fields little-endian.
///
/// Wire: `magic:u32 | version:u8 | flags:u8 | codec:u8 | color:u8 | video_transport:u8 |
/// reserved[3] | width:u32 | height:u32 | fps:u32 | target_bitrate_bps:u32 | frame_id:u64 |
/// captured_at_us:u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VideoFrameHeader {
    is_keyframe: bool,
    config: StreamConfig,
    frame_id: u64,
    captured_at_us: u64,
}

impl VideoFrameHeader {
    /// `"RVF1"` big-endian ASCII tag; a mismatch means desync (drop the frame, not the connection).
    const MAGIC: u32 = u32::from_be_bytes(*b"RVF1");
    const VERSION: u8 = 1;
    const LEN: usize = 44;
    const FLAG_KEYFRAME: u8 = 0b0000_0001;

    fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&Self::MAGIC.to_le_bytes());
        b[4] = Self::VERSION;
        b[5] = if self.is_keyframe {
            Self::FLAG_KEYFRAME
        } else {
            0
        };
        // Known variants map to their discriminant; an unknown future (`#[non_exhaustive]`) variant
        // maps to 0xFF, which `decode` rejects — fail-closed, never silently mis-tagged.
        b[6] = match self.config.codec {
            VideoCodec::H264AnnexB => 0,
            _ => 0xFF,
        };
        b[7] = match self.config.color {
            ColorSpace::Bt709Limited => 0,
            ColorSpace::Bt709Full => 1,
            _ => 0xFF,
        };
        b[8] = match self.config.video_transport {
            VideoTransportKind::PerFrameStream => 0,
            VideoTransportKind::DatagramFec => 1,
        };
        // b[9..12] reserved (zero)
        b[12..16].copy_from_slice(&self.config.width.to_le_bytes());
        b[16..20].copy_from_slice(&self.config.height.to_le_bytes());
        b[20..24].copy_from_slice(&self.config.fps.to_le_bytes());
        b[24..28].copy_from_slice(&self.config.target_bitrate_bps.to_le_bytes());
        b[28..36].copy_from_slice(&self.frame_id.to_le_bytes());
        b[36..44].copy_from_slice(&self.captured_at_us.to_le_bytes());
        b
    }

    /// Parse a header from the front of `buf`. `None` on any malformed field (short, bad magic,
    /// unknown version, or an out-of-range enum) — the caller drops that frame. Fail-closed: an
    /// unrecognized enum discriminant is rejected, never defaulted.
    fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::LEN {
            return None;
        }
        // Infallible: the length is checked above, so every slice below is exactly in range.
        let u32le = |o: usize| {
            let mut a = [0u8; 4];
            a.copy_from_slice(&buf[o..o + 4]);
            u32::from_le_bytes(a)
        };
        let u64le = |o: usize| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&buf[o..o + 8]);
            u64::from_le_bytes(a)
        };
        if u32le(0) != Self::MAGIC || buf[4] != Self::VERSION {
            return None;
        }
        let codec = match buf[6] {
            0 => VideoCodec::H264AnnexB,
            _ => return None,
        };
        let color = match buf[7] {
            0 => ColorSpace::Bt709Limited,
            1 => ColorSpace::Bt709Full,
            _ => return None,
        };
        let video_transport = match buf[8] {
            0 => VideoTransportKind::PerFrameStream,
            1 => VideoTransportKind::DatagramFec,
            _ => return None,
        };
        Some(Self {
            is_keyframe: buf[5] & Self::FLAG_KEYFRAME != 0,
            config: StreamConfig {
                codec,
                width: u32le(12),
                height: u32le(16),
                fps: u32le(20),
                target_bitrate_bps: u32le(24),
                color,
                video_transport,
            },
            frame_id: u64le(28),
            captured_at_us: u64le(36),
        })
    }
}

/// Host-side droppable video sender (`PerFrameStream`). Non-blocking: [`send_frame`](Self::send_frame)
/// hands the frame to a bounded channel drained by a background task that opens **one unidirectional
/// QUIC stream per frame**. Separate streams never head-of-line-block each other, so a lost/stalled
/// frame cannot stall a later one or the control channel (the latency invariant). If the path can't
/// keep up the channel fills and frames are dropped at the source — never queued unbounded.
pub struct VideoSink {
    tx: mpsc::Sender<EncodedFrame>,
}

impl VideoSink {
    /// Bounded channel depth: a few frames of slack absorbs jitter without letting a slow path build
    /// latency. Deeper would trade latency for a smoother-but-staler stream — the wrong trade here.
    const QUEUE_DEPTH: usize = 4;

    /// Spawn the per-frame-stream writer task bound to `conn` and return the sender half. Must be
    /// called from within a Tokio runtime (it is — the media pump is async).
    fn spawn(conn: Connection) -> Self {
        let (tx, mut rx) = mpsc::channel::<EncodedFrame>(Self::QUEUE_DEPTH);
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                // One uni stream per frame: open, write header + Annex-B AU, FIN. `write_all` awaits
                // flow-control, so a slow receiver backpressures here → the channel fills → the next
                // `send_frame` drops (congestion). A per-frame error means the connection is gone.
                let mut stream = match conn.open_uni().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let header = VideoFrameHeader {
                    is_keyframe: frame.is_keyframe,
                    config: frame.config,
                    frame_id: frame.frame_id,
                    captured_at_us: frame.captured_at_us,
                }
                .encode();
                if stream.write_all(&header).await.is_err()
                    || stream.write_all(&frame.data).await.is_err()
                    || stream.finish().is_err()
                {
                    break;
                }
            }
        });
        Self { tx }
    }

    /// Hand one frame to the transport. Returns immediately; does not await delivery. Ordinary loss
    /// (a full or closed queue) is a non-error [`SendOutcome`], not an `Err`.
    #[allow(clippy::unnecessary_wraps)] // signature parity with the DatagramFec sink (may fail-fast)
    pub fn send_frame(&self, frame: EncodedFrame) -> Result<SendOutcome, TransportError> {
        Ok(match self.tx.try_send(frame) {
            Ok(()) => SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => SendOutcome::DroppedCongested,
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::DroppedStale,
        })
    }
}

/// Controller-side droppable video receiver (`PerFrameStream`). Accepts one uni stream per frame,
/// reads it to the FIN, and reconstructs the [`EncodedFrame`]. Loss is first-class and non-fatal: a
/// `frame_id` gap (the host dropped that frame at the source under congestion) surfaces as a
/// [`VideoEvent::FrameDropped`] *before* the next frame, so `ras-core` can coalesce a run of drops
/// into one keyframe request instead of freezing. The decoder owns final reorder-by-`frame_id`.
pub struct VideoSource {
    conn: Connection,
    /// The next in-order `frame_id` we expect; `None` until the first frame establishes the base.
    next_expected: Option<u64>,
    /// A frame read ahead of a detected gap, returned on the call after the synthesized drop event.
    pending: Option<EncodedFrame>,
}

impl VideoSource {
    fn new(conn: Connection) -> Self {
        Self {
            conn,
            next_expected: None,
            pending: None,
        }
    }

    /// Await the next video event (a decoded frame or a synthesized loss). `Err` only on a terminal
    /// transport failure (the connection is gone); a malformed or oversized single frame is skipped,
    /// not fatal.
    pub async fn recv(&mut self) -> Result<VideoEvent, TransportError> {
        loop {
            // Deliver a frame stashed behind a just-reported gap before reading more.
            if let Some(frame) = self.pending.take() {
                self.next_expected = Some(frame.frame_id.wrapping_add(1));
                return Ok(VideoEvent::Frame(frame));
            }

            // Accept the next per-frame stream. A connection-level error here is terminal.
            let mut stream = self.conn.accept_uni().await.map_err(|_| {
                RasError::recoverable(ErrorCode::TransportError, "video stream ended")
            })?;

            // Read the whole access unit (bounded). A frame-level read/parse failure drops just this
            // frame — loop to the next stream rather than tearing down the session.
            let Ok(buf) = stream.read_to_end(MAX_VIDEO_FRAME).await else {
                continue;
            };
            let Some(header) = VideoFrameHeader::decode(&buf) else {
                continue;
            };
            let frame = EncodedFrame {
                frame_id: header.frame_id,
                captured_at_us: header.captured_at_us,
                is_keyframe: header.is_keyframe,
                data: Bytes::copy_from_slice(&buf[VideoFrameHeader::LEN..]),
                config: header.config,
            };

            match self.next_expected {
                // A gap: the source dropped frame(s) under congestion. Report one loss for the first
                // missing id, stash this frame, and return it next call.
                Some(expected) if frame.frame_id > expected => {
                    self.pending = Some(frame);
                    return Ok(VideoEvent::FrameDropped {
                        frame_id: expected,
                        reason: DropReason::MissingFragments,
                    });
                }
                // A stale/reordered frame at or behind the watermark: drop it, keep reading.
                Some(expected) if frame.frame_id < expected => continue,
                // In order (or the first frame): deliver and advance.
                _ => {
                    self.next_expected = Some(frame.frame_id.wrapping_add(1));
                    return Ok(VideoEvent::Frame(frame));
                }
            }
        }
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
pub struct Session {
    conn: Connection,
    role: Role,
}

impl Session {
    /// The remote peer's authenticated identity (not authorization — Invariant 9).
    #[must_use]
    pub fn remote(&self) -> EndpointId {
        EndpointId(*self.conn.remote_id().as_bytes())
    }

    /// Whether this connection negotiated the [`BOOTSTRAP_ALPN`] (Phase-2 authorization handshake)
    /// rather than the session [`ALPN`]. The host's accept loop routes on this. Fail-closed: an
    /// absent/unrecognized ALPN is treated as **not** bootstrap (never mis-routed into the
    /// consent/issuance path).
    #[must_use]
    pub fn is_bootstrap(&self) -> bool {
        self.conn.alpn() == BOOTSTRAP_ALPN
    }

    /// The reliable **bootstrap** channel over one bidi QUIC stream, carrying `BootstrapMsg`. Here the
    /// **controller opens** the stream and speaks first (`ClientHello` → `AccessRequest`) and the
    /// **host accepts** — the mirror of the session control channel (ADR-059), because on the bootstrap
    /// ALPN it is the *controller* that speaks first, so making it the opener is what unblocks the
    /// host's accept with no dead wait.
    pub async fn bootstrap(&self) -> Result<BootstrapChannel, TransportError> {
        let (send, recv) = match self.role {
            Role::Controller => self.conn.open_bi().await,
            Role::Host => self.conn.accept_bi().await,
        }
        .map_err(|_| RasError::recoverable(ErrorCode::TransportError, "bootstrap stream failed"))?;
        Ok(BootstrapChannel {
            framed: FramedBootstrapChannel::new(recv, send),
        })
    }

    /// The reliable control channel over one bidi QUIC stream. The **host opens** it and the
    /// **controller accepts** it (ADR-059). This is deliberate: QUIC only surfaces a freshly-opened
    /// stream to the *acceptor* once the *opener* first writes, and in the Casual RAS handshake the
    /// **host speaks first** (`Hello` → `StreamConfig`). Making the host the opener means its first
    /// write is what unblocks the controller's accept — the two rendezvous with no dead wait. (The
    /// host likewise opens every per-frame video uni-stream, so it is the uniform stream opener; the
    /// controller only *dials the connection*.)
    pub async fn control(&self) -> Result<ControlChannel, TransportError> {
        let (send, recv) = match self.role {
            Role::Host => self.conn.open_bi().await,
            Role::Controller => self.conn.accept_bi().await,
        }
        .map_err(|_| RasError::recoverable(ErrorCode::TransportError, "control stream failed"))?;
        Ok(ControlChannel {
            framed: FramedControlChannel::new(recv, send),
        })
    }

    /// Host-side video sink (present on the host role only; video flows host → controller). Spawns
    /// the per-frame-stream writer task bound to this connection. `None` on the controller side.
    #[must_use]
    pub fn video_sink(&self) -> Option<VideoSink> {
        match self.role {
            Role::Host => Some(VideoSink::spawn(self.conn.clone())),
            Role::Controller => None,
        }
    }
    /// Controller-side video source (present on the controller role only). `None` on the host side.
    #[must_use]
    pub fn video_source(&self) -> Option<VideoSource> {
        match self.role {
            Role::Controller => Some(VideoSource::new(self.conn.clone())),
            Role::Host => None,
        }
    }
    /// Connection-health observable, computed on demand from this connection's live QUIC stats.
    ///
    /// Hold onto the returned observer across samples: each read reports loss over the interval
    /// since the previous read (a windowed rate), so a fresh observer per read would lose that
    /// baseline and fall back to the lifetime average.
    #[must_use]
    pub fn health(&self) -> HealthObserver {
        HealthObserver {
            conn: self.conn.clone(),
            prev_loss: Mutex::new(None),
        }
    }

    /// Close the session with a reason code (carried as the QUIC application close code).
    pub async fn close(self, code: ErrorCode) {
        self.conn
            .close(VarInt::from_u32(code as u32), code.as_str().as_bytes());
    }
}

/// Read-only connection-health observable over one iroh [`Connection`]. Each read derives a fresh
/// [`ConnHealth`] from the connection's live QUIC stats; it reads in-memory counters only and never
/// awaits network I/O, so a stalled video path can never block a health read (the latency invariant).
///
/// It is *lightly* stateful: it remembers the previous loss counters so each read reports loss over
/// the interval since the last read, not the connection's lifetime average (so the ABR recovers the
/// bitrate once a loss burst passes). The state is one small baseline behind a `Mutex`; a read still
/// never blocks on I/O.
pub struct HealthObserver {
    conn: Connection,
    prev_loss: Mutex<Option<LossSample>>,
}

/// Cumulative datagram counters captured at a health sample, used to window the loss rate.
#[derive(Clone, Copy)]
struct LossSample {
    sent: u64,
    lost: u64,
}

/// Loss fraction over the interval between two cumulative samples (or the lifetime ratio for the
/// first sample, when there is no prior baseline). Pure — unit-tested without a live connection.
/// An idle interval (no datagrams sent) reports `0.0`: no traffic, no observed loss.
fn windowed_loss(prev: Option<LossSample>, cur: LossSample) -> f32 {
    let frac = match prev {
        Some(p) => {
            let d_sent = cur.sent.saturating_sub(p.sent);
            let d_lost = cur.lost.saturating_sub(p.lost);
            if d_sent > 0 {
                d_lost as f64 / d_sent as f64
            } else {
                0.0
            }
        }
        None if cur.sent > 0 => cur.lost as f64 / cur.sent as f64,
        None => 0.0,
    };
    (frac as f32).clamp(0.0, 1.0)
}

impl HealthObserver {
    /// How often [`changed`](Self::changed) resamples. Health drives UI badges + the ABR loop, not
    /// the hot frame path, so a coarse poll is right — a tighter loop would burn CPU for no benefit.
    const POLL_INTERVAL: Duration = Duration::from_millis(500);

    /// The current health snapshot. Non-blocking: reads in-memory QUIC counters, never the network.
    /// Advances the loss window (this read becomes the baseline for the next).
    #[must_use]
    pub fn snapshot(&self) -> ConnHealth {
        map_health(&self.conn, &self.prev_loss)
    }

    /// Await the next sampled health value (UI reactivity + ABR, not the hot path). iroh exposes no
    /// edge-triggered health signal, so this is a fixed-interval resample rather than a true
    /// change-notify — honest about being a sampler, not a watch.
    pub async fn changed(&mut self) -> ConnHealth {
        tokio::time::sleep(Self::POLL_INTERVAL).await;
        self.snapshot()
    }
}

/// Derive a [`ConnHealth`] from one connection's live QUIC stats. Sourced honestly:
/// - `rtt_us` / `estimated_bandwidth_bps` / `path` come from the **selected** network path's
///   [`PathStats`] (`rtt`, congestion window, relay-vs-direct); bandwidth is the BDP estimate
///   `cwnd·8 / rtt` (bits/sec), saturating.
/// - `loss_fraction` is the lost-vs-sent datagram ratio **over the interval since the last read**
///   (windowed via `prev_loss`), so a burst of loss no longer depresses the estimate for the rest of
///   the session — the ABR can raise the bitrate again once the link recovers.
/// - `frames_dropped` is `0` here: host-side sink drops surface as a [`SendOutcome`] at send time,
///   not through this connection-level view.
/// - `state` is [`LinkState::Live`] while the connection is up; the watchdog `Stalled`/`Reconnecting`
///   transitions are a `ras-core` timing concern, not a transport-stats one.
fn map_health(conn: &Connection, prev_loss: &Mutex<Option<LossSample>>) -> ConnHealth {
    let paths = conn.paths();
    // The path currently carrying application data (fall back to the first known path).
    let selected = paths
        .iter()
        .find(iroh::endpoint::Path::is_selected)
        .or_else(|| paths.iter().next());

    let (rtt_us, cwnd_bytes, is_relay) = match selected {
        Some(p) => (
            u32::try_from(p.rtt().as_micros()).unwrap_or(u32::MAX),
            p.stats().cwnd,
            p.is_relay(),
        ),
        None => (0, 0, false),
    };

    // BDP estimate: cwnd (bytes) · 8 / rtt (seconds) = bits/sec. Guard rtt=0 (loopback reports
    // near-zero) with a 1 µs floor; saturate the u64 math into the u32 field.
    let rtt_floor_us = u64::from(rtt_us).max(1);
    let bw_bps = cwnd_bytes
        .saturating_mul(8)
        .saturating_mul(1_000_000)
        .checked_div(rtt_floor_us)
        .unwrap_or(0);
    let estimated_bandwidth_bps = u32::try_from(bw_bps).unwrap_or(u32::MAX);

    let cs = conn.stats();
    let cur = LossSample {
        sent: cs.udp_tx.datagrams,
        lost: cs.lost_packets,
    };
    let loss_fraction = {
        let mut prev = prev_loss
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let frac = windowed_loss(*prev, cur);
        *prev = Some(cur);
        frac
    };

    ConnHealth {
        path: if is_relay {
            PathKind::Relayed
        } else {
            PathKind::Direct
        },
        rtt_us,
        loss_fraction,
        estimated_bandwidth_bps,
        frames_dropped: 0,
        state: LinkState::Live,
    }
}

#[cfg(test)]
mod loss_window_tests {
    use super::{windowed_loss, LossSample};

    fn s(sent: u64, lost: u64) -> LossSample {
        LossSample { sent, lost }
    }

    #[test]
    fn first_sample_uses_lifetime_ratio() {
        assert_eq!(windowed_loss(None, s(0, 0)), 0.0); // nothing sent yet
        assert!((windowed_loss(None, s(100, 10)) - 0.10).abs() < 1e-6);
    }

    #[test]
    fn reports_loss_over_the_interval_not_the_lifetime() {
        // 5% loss confined to this interval (50 of 1000 new datagrams), despite a clean history.
        let prev = s(9_000, 0);
        let cur = s(10_000, 50);
        assert!((windowed_loss(Some(prev), cur) - 0.05).abs() < 1e-6);
    }

    #[test]
    fn recovers_after_a_burst_passes() {
        // History carries a big cumulative loss (100/1000 = 10% lifetime), but the latest interval
        // is clean — the windowed estimate must read ~0 so the ABR can raise the bitrate again.
        let prev = s(1_000, 100);
        let cur = s(2_000, 100); // 1000 new datagrams, 0 new losses
        assert_eq!(windowed_loss(Some(prev), cur), 0.0);
    }

    #[test]
    fn idle_interval_reports_no_loss() {
        // No datagrams sent since the last read → no traffic, no observed loss (avoid divide-by-zero
        // and avoid carrying a stale rate forward).
        let prev = s(5_000, 50);
        assert_eq!(windowed_loss(Some(prev), s(5_000, 50)), 0.0);
    }

    #[test]
    fn saturates_into_zero_one() {
        // Defensive: counters are monotonic, but never emit a nonsensical fraction.
        assert_eq!(windowed_loss(Some(s(100, 100)), s(50, 40)), 0.0); // sent went backwards → 0
        assert_eq!(windowed_loss(Some(s(0, 0)), s(10, 10)), 1.0); // total loss this interval
    }
}

#[cfg(test)]
mod ticket_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn ticket_round_trips_id_relay_and_addrs() {
        let addr = EndpointAddr {
            id: EndpointId([0xAB; 32]),
            direct_addrs: vec![
                "192.168.1.5:41641".parse::<SocketAddr>().unwrap(),
                "[2001:db8::1]:5000".parse::<SocketAddr>().unwrap(),
            ],
            relay_url: Some("https://relay.example.com./".to_string()),
        };
        let ticket = addr.to_ticket();
        assert!(ticket.starts_with(TICKET_PREFIX));
        assert_eq!(EndpointAddr::from_ticket(&ticket).unwrap(), addr);
    }

    #[test]
    fn ticket_round_trips_identity_only() {
        let addr = EndpointAddr::new(EndpointId([7u8; 32]));
        assert_eq!(EndpointAddr::from_ticket(&addr.to_ticket()).unwrap(), addr);
    }

    #[test]
    fn from_ticket_is_fail_closed() {
        let good = EndpointAddr::new(EndpointId([1u8; 32])).to_ticket();
        // Wrong prefix.
        assert!(EndpointAddr::from_ticket("NOPE:00").is_err());
        // Odd-length hex.
        assert!(EndpointAddr::from_ticket(&format!("{good}0")).is_err());
        // Non-hex char.
        assert!(EndpointAddr::from_ticket(&format!("{TICKET_PREFIX}zz")).is_err());
        // Truncated (only the id, missing relay_len/count).
        let short = &good[..TICKET_PREFIX.len() + 10];
        assert!(EndpointAddr::from_ticket(short).is_err());
        // Trailing garbage.
        assert!(EndpointAddr::from_ticket(&format!("{good}ffff")).is_err());
    }
}

#[cfg(test)]
mod video_header_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use ras_media::{ColorSpace, StreamConfig, VideoCodec, VideoTransportKind};

    fn sample() -> VideoFrameHeader {
        VideoFrameHeader {
            is_keyframe: true,
            config: StreamConfig {
                codec: VideoCodec::H264AnnexB,
                width: 2560,
                height: 1440,
                fps: 60,
                target_bitrate_bps: 12_000_000,
                color: ColorSpace::Bt709Full,
                video_transport: VideoTransportKind::PerFrameStream,
            },
            frame_id: 1 << 40, // exercises the u64 path (> 2^32)
            captured_at_us: 123_456_789,
        }
    }

    #[test]
    fn header_round_trips_every_field() {
        let h = sample();
        let decoded = VideoFrameHeader::decode(&h.encode()).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn decode_is_fail_closed_on_corruption() {
        let good = sample().encode();
        assert_eq!(good.len(), VideoFrameHeader::LEN);
        // Truncated.
        assert!(VideoFrameHeader::decode(&good[..VideoFrameHeader::LEN - 1]).is_none());
        // Bad magic.
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert!(VideoFrameHeader::decode(&bad_magic).is_none());
        // Unknown version.
        let mut bad_ver = good;
        bad_ver[4] = 0xEE;
        assert!(VideoFrameHeader::decode(&bad_ver).is_none());
        // Out-of-range enum discriminant (color byte) — rejected, never defaulted.
        let mut bad_color = good;
        bad_color[7] = 0x7F;
        assert!(VideoFrameHeader::decode(&bad_color).is_none());
    }
}

#[cfg(test)]
mod iroh_session_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_media::{ColorSpace, EncodedFrame, StreamConfig, VideoCodec, VideoTransportKind};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    /// A freshly-bound endpoint reports its sockets as the *unspecified* address (`0.0.0.0` /
    /// `[::]`) — the wildcard it listens on, not a dialable peer address. For a hermetic same-host
    /// dial we rewrite each to its loopback counterpart, preserving the port. No discovery, no
    /// relay, no network egress: the whole exchange stays on the loopback interface.
    fn to_loopback(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
        addrs
            .iter()
            .map(|a| match a.ip() {
                IpAddr::V4(ip) if ip.is_unspecified() => {
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), a.port())
                }
                IpAddr::V6(ip) if ip.is_unspecified() => {
                    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), a.port())
                }
                _ => *a,
            })
            .collect()
    }

    /// End-to-end over a **real iroh connection** on loopback: the host binds and accepts, the
    /// controller dials it by direct address, and a `ControlMsg` round-trips both ways over the
    /// framed control stream. Also asserts the identity guarantee (Invariant 9): each side sees the
    /// *other's* authenticated `EndpointId` as the connection's remote — the transport proves *who*,
    /// and nothing here grants any authority.
    #[tokio::test]
    async fn control_round_trips_over_a_real_iroh_connection() {
        let host = Endpoint::bind().await.unwrap();
        let controller = Endpoint::bind().await.unwrap();
        let host_id = host.id();
        let controller_id = controller.id();
        let host_addrs = to_loopback(&host.bound_addrs());

        // Host side: accept the connection, **open** the control stream, and speak first (`Hello`) —
        // the same order as the real handshake. Its first write is what unblocks the controller's
        // `accept_bi`. Then read the controller's reply.
        let host_task = tokio::spawn(async move {
            let session = host.accept().await.unwrap().expect("an inbound session");
            assert!(
                !session.is_bootstrap(),
                "a session-ALPN connection must not be routed to the bootstrap handler"
            );
            let remote = session.remote();
            let mut control = session.control().await.unwrap();
            control
                .send(ControlMsg::Hello {
                    protocol_version: 1,
                })
                .await
                .unwrap();
            let reply = control.recv().await.unwrap();
            assert!(matches!(
                reply,
                ControlMsg::Bye {
                    code: ErrorCode::NormalClosure
                }
            ));
            // Keep the endpoint alive until the controller has finished.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            host.close().await;
            remote
        });

        let session = controller
            .connect_direct(&host_id, &host_addrs)
            .await
            .unwrap();
        // The dialer authenticated the host's identity, not any authority (Invariant 9).
        assert_eq!(session.remote(), host_id);
        // Controller **accepts** the host-opened control stream, reads the host's `Hello`, replies.
        let mut control = session.control().await.unwrap();
        let got = control.recv().await.unwrap();
        assert!(matches!(
            got,
            ControlMsg::Hello {
                protocol_version: 1
            }
        ));
        control
            .send(ControlMsg::Bye {
                code: ErrorCode::NormalClosure,
            })
            .await
            .unwrap();

        let host_saw = host_task.await.unwrap();
        // The acceptor authenticated the controller's identity in turn.
        assert_eq!(host_saw, controller_id);
        controller.close().await;
    }

    /// End-to-end over a **real iroh connection** on the **bootstrap** ALPN (Phase 2): the controller
    /// dials `connect_direct_bootstrap`, the host accepts and confirms `is_bootstrap()` (routing by
    /// negotiated ALPN), and a full `BootstrapMsg` exchange round-trips — the **controller speaks
    /// first** (`ClientHello` → `AccessRequest`), the host replies (`HostHello` → `AccessDecision`).
    /// The AccessRequest and grant ride as **opaque bytes** (Invariant 9): the transport moves them,
    /// never interprets them.
    #[tokio::test]
    async fn bootstrap_handshake_round_trips_over_a_real_iroh_connection() {
        use ras_protocol::AccessOutcome;

        let host = Endpoint::bind().await.unwrap();
        let controller = Endpoint::bind().await.unwrap();
        let host_id = host.id();
        let host_addrs = to_loopback(&host.bound_addrs());

        let host_task = tokio::spawn(async move {
            let session = host.accept().await.unwrap().expect("an inbound session");
            assert!(
                session.is_bootstrap(),
                "host must route a bootstrap-ALPN connection to the bootstrap handler"
            );
            let mut boot = session.bootstrap().await.unwrap();
            // Read the controller's ClientHello, then its (opaque) AccessRequest.
            assert!(matches!(
                boot.recv().await.unwrap(),
                BootstrapMsg::ClientHello {
                    protocol_version: 1
                }
            ));
            match boot.recv().await.unwrap() {
                BootstrapMsg::AccessRequest { canonical } => {
                    assert_eq!(&canonical[..], b"signed-access-request");
                }
                other => panic!("expected AccessRequest, got {other:?}"),
            }
            // Reply: HostHello, then an Allow decision carrying an opaque PASETO grant.
            boot.send(BootstrapMsg::HostHello {
                host_id: [7u8; 32],
                tier: 0,
            })
            .await
            .unwrap();
            boot.send(BootstrapMsg::AccessDecision(AccessOutcome::Allowed {
                grant: Bytes::from_static(b"v4.public.opaque-grant"),
            }))
            .await
            .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            host.close().await;
        });

        let session = controller
            .connect_direct_bootstrap(&host_id, &host_addrs)
            .await
            .unwrap();
        assert_eq!(session.remote(), host_id);
        let mut boot = session.bootstrap().await.unwrap();
        boot.send(BootstrapMsg::ClientHello {
            protocol_version: 1,
        })
        .await
        .unwrap();
        boot.send(BootstrapMsg::AccessRequest {
            canonical: Bytes::from_static(b"signed-access-request"),
        })
        .await
        .unwrap();
        assert!(matches!(
            boot.recv().await.unwrap(),
            BootstrapMsg::HostHello { tier: 0, .. }
        ));
        match boot.recv().await.unwrap() {
            BootstrapMsg::AccessDecision(AccessOutcome::Allowed { grant }) => {
                assert_eq!(&grant[..], b"v4.public.opaque-grant");
            }
            other => panic!("expected an Allowed decision, got {other:?}"),
        }

        host_task.await.unwrap();
        controller.close().await;
    }

    /// A tiny valid `EncodedFrame` for the streaming test. The payload stands in for an Annex-B AU
    /// (the transport is codec-agnostic — it moves opaque bytes).
    fn test_frame(id: u64, keyframe: bool, payload: &[u8]) -> EncodedFrame {
        EncodedFrame {
            frame_id: id,
            captured_at_us: id.wrapping_mul(1000),
            is_keyframe: keyframe,
            data: Bytes::copy_from_slice(payload),
            config: StreamConfig {
                codec: VideoCodec::H264AnnexB,
                width: 1920,
                height: 1080,
                fps: 60,
                target_bitrate_bps: 8_000_000,
                color: ColorSpace::Bt709Limited,
                video_transport: VideoTransportKind::PerFrameStream,
            },
        }
    }

    /// End-to-end video over **real per-frame uni streams** on loopback: the host opens one uni
    /// stream per frame, the controller reconstructs each `EncodedFrame` faithfully (bytes, keyframe
    /// flag, and the per-frame `StreamConfig`). Driven in lockstep (send one, receive one) so stream
    /// ordering is deterministic. Then exercises loss handling: a `frame_id` gap surfaces as exactly
    /// one synthesized `FrameDropped` for the first missing id, *before* the next frame — the signal
    /// `ras-core` coalesces into a keyframe request instead of freezing. Also asserts the role split
    /// (only the host has a sink; only the controller a source).
    #[tokio::test]
    async fn video_streams_frame_by_frame_over_iroh_with_gap_detection() {
        let host = Endpoint::bind().await.unwrap();
        let controller = Endpoint::bind().await.unwrap();
        let host_id = host.id();
        let host_addrs = to_loopback(&host.bound_addrs());

        // Host accepts, takes its video sink, and hands the endpoint + sink back (kept alive so the
        // connection — and the sink's writer task — outlive the exchange).
        let host_task = tokio::spawn(async move {
            let session = host.accept().await.unwrap().expect("an inbound session");
            assert!(
                session.video_source().is_none(),
                "the host has no video source"
            );
            let sink = session.video_sink().expect("the host has a video sink");
            (host, sink)
        });

        let session = controller
            .connect_direct(&host_id, &host_addrs)
            .await
            .unwrap();
        assert!(
            session.video_sink().is_none(),
            "the controller has no video sink"
        );
        let mut source = session
            .video_source()
            .expect("the controller has a video source");
        let (host_ep, sink) = host_task.await.unwrap();

        // Three in-order frames round-trip 1:1, config and keyframe flag intact.
        for id in 0..3u64 {
            let sent = sink
                .send_frame(test_frame(id, id == 0, b"annexb-au"))
                .unwrap();
            assert_eq!(sent, SendOutcome::Sent);
            match source.recv().await.unwrap() {
                VideoEvent::Frame(f) => {
                    assert_eq!(f.frame_id, id);
                    assert_eq!(f.is_keyframe, id == 0);
                    assert_eq!(&f.data[..], b"annexb-au");
                    assert_eq!(f.config.width, 1920);
                    assert_eq!(f.config.video_transport, VideoTransportKind::PerFrameStream);
                }
                other => panic!("expected a frame, got {other:?}"),
            }
        }

        // Skip ids 3 and 4 (as if the host dropped them under congestion): send 5. The source reports
        // one drop for the first missing id (3), then delivers frame 5.
        sink.send_frame(test_frame(5, false, b"annexb-au")).unwrap();
        match source.recv().await.unwrap() {
            VideoEvent::FrameDropped { frame_id, reason } => {
                assert_eq!(frame_id, 3);
                assert_eq!(reason, DropReason::MissingFragments);
            }
            other => panic!("expected a synthesized drop, got {other:?}"),
        }
        match source.recv().await.unwrap() {
            VideoEvent::Frame(f) => assert_eq!(f.frame_id, 5),
            other => panic!("expected frame 5 after the gap, got {other:?}"),
        }

        // Health reads off the live connection: a real, direct, low-loss loopback path.
        let health = session.health().snapshot();
        assert_eq!(health.state, LinkState::Live);
        assert_eq!(health.path, PathKind::Direct);
        assert!(
            health.loss_fraction < 0.5,
            "loopback loss should be low, got {}",
            health.loss_fraction
        );

        drop(sink);
        host_ep.close().await;
        controller.close().await;
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

    /// The bootstrap channel frames `BootstrapMsg` over the same duplex shape, both directions.
    #[tokio::test]
    async fn bootstrap_channel_round_trips_both_directions() {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, aw) = split(a);
        let (br, bw) = split(b);
        let mut a = FramedBootstrapChannel::new(ar, aw);
        let mut b = FramedBootstrapChannel::new(br, bw);

        a.send(&BootstrapMsg::ClientHello {
            protocol_version: 1,
        })
        .await
        .unwrap();
        assert!(matches!(
            b.recv().await.unwrap(),
            BootstrapMsg::ClientHello {
                protocol_version: 1
            }
        ));

        b.send(&BootstrapMsg::AccessRequest {
            canonical: Bytes::from_static(b"opaque-request"),
        })
        .await
        .unwrap();
        match a.recv().await.unwrap() {
            BootstrapMsg::AccessRequest { canonical } => {
                assert_eq!(&canonical[..], b"opaque-request")
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
