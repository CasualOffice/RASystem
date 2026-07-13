//! In-memory test doubles for the DI seams (design §5.4 "synthetic in-memory impl in tests").
//!
//! [`loopback_pair`] wires a host-side and controller-side [`SessionTransport`] together with plain
//! `tokio` channels — no iroh, no sockets, no OS. Control is a reliable bidi pair; video is a
//! bounded, drop-on-full host→controller path (mirroring the real droppable video semantics).
//! [`CountingFrameSink`] is a content-free [`FrameSink`] that just tallies what it receives, for
//! asserting end-to-end delivery and keyframe plumbing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use crate::deps::{
    ControlChannelDyn, DialTarget, FrameSink, PeerIdentity, PushResult, SessionTransport,
    VideoSinkDyn, VideoSourceDyn,
};
use crate::CoreError;
use ras_media::{EncodedFrame, StreamConfig};
use ras_protocol::{ControlMsg, ErrorCode};
use ras_transport_iroh::{ConnHealth, EndpointId, LinkState, PathKind, SendOutcome, VideoEvent};

/// The fixed identity the loopback "peer" presents (identity only — never authorization).
const LOOPBACK_PEER: EndpointId = EndpointId([7u8; 32]);

#[derive(Clone, Copy)]
enum Role {
    Host,
    Controller,
}

/// Error a channel returns once the link has been [cut](LoopbackCut::cut) — models the peer's QUIC
/// connection dropping *without* a clean `Bye`, which the orchestrators must treat as transport loss.
fn severed() -> CoreError {
    CoreError::fatal(ErrorCode::TransportError, "loopback severed")
}

fn control_closed() -> CoreError {
    CoreError::fatal(ErrorCode::TransportError, "loopback control closed")
}

/// Whether the shared cut flag is set; on a spurious wakeup we keep waiting, and if the cut
/// *sender* was dropped (plain pair with no fault handle held) a cut can never occur.
enum CutPoll {
    Cut,
    Continue,
    SenderGone,
}

async fn poll_cut(cut: &mut watch::Receiver<bool>) -> CutPoll {
    match cut.changed().await {
        Ok(()) if *cut.borrow() => CutPoll::Cut,
        Ok(()) => CutPoll::Continue,
        Err(_) => CutPoll::SenderGone,
    }
}

struct LoopbackControl {
    tx: mpsc::Sender<ControlMsg>,
    rx: mpsc::Receiver<ControlMsg>,
    cut: watch::Receiver<bool>,
}

#[async_trait]
impl ControlChannelDyn for LoopbackControl {
    async fn send(&mut self, msg: ControlMsg) -> Result<(), CoreError> {
        if *self.cut.borrow() {
            return Err(severed());
        }
        self.tx.send(msg).await.map_err(|_| control_closed())
    }
    async fn recv(&mut self) -> Result<ControlMsg, CoreError> {
        if *self.cut.borrow() {
            return Err(severed());
        }
        loop {
            tokio::select! {
                verdict = poll_cut(&mut self.cut) => match verdict {
                    CutPoll::Cut => return Err(severed()),
                    CutPoll::Continue => continue,
                    CutPoll::SenderGone => return self.rx.recv().await.ok_or_else(control_closed),
                },
                msg = self.rx.recv() => return msg.ok_or_else(control_closed),
            }
        }
    }
}

struct LoopbackSink {
    tx: mpsc::Sender<EncodedFrame>,
    cut: watch::Receiver<bool>,
}

impl VideoSinkDyn for LoopbackSink {
    fn send_frame(&self, frame: EncodedFrame) -> SendOutcome {
        if *self.cut.borrow() {
            return SendOutcome::DroppedStale; // link severed — nothing to send onto
        }
        // Non-blocking, drop-on-full — the video path is droppable by design.
        match self.tx.try_send(frame) {
            Ok(()) => SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => SendOutcome::DroppedCongested,
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::DroppedStale,
        }
    }
}

struct LoopbackSource {
    rx: mpsc::Receiver<EncodedFrame>,
    cut: watch::Receiver<bool>,
}

fn video_closed() -> CoreError {
    CoreError::fatal(ErrorCode::TransportError, "loopback video closed")
}

#[async_trait]
impl VideoSourceDyn for LoopbackSource {
    async fn next(&mut self) -> Result<VideoEvent, CoreError> {
        if *self.cut.borrow() {
            return Err(severed());
        }
        loop {
            tokio::select! {
                verdict = poll_cut(&mut self.cut) => match verdict {
                    CutPoll::Cut => return Err(severed()),
                    CutPoll::Continue => continue,
                    CutPoll::SenderGone => {
                        return self.rx.recv().await.map(VideoEvent::Frame).ok_or_else(video_closed)
                    }
                },
                f = self.rx.recv() => return f.map(VideoEvent::Frame).ok_or_else(video_closed),
            }
        }
    }
}

/// One end of an in-memory session. Build a wired pair with [`loopback_pair`] (or
/// [`loopback_pair_with_cut`] to also get a fault handle).
pub struct LoopbackTransport {
    role: Role,
    control: Mutex<Option<LoopbackControl>>,
    video_sink: Mutex<Option<mpsc::Sender<EncodedFrame>>>,
    video_source: Mutex<Option<mpsc::Receiver<EncodedFrame>>>,
    /// Shared severed-link flag, cloned into every sink/source this transport hands out.
    cut: watch::Receiver<bool>,
    /// Keeps the cut `Sender` alive for a plain [`loopback_pair`] (so `changed()` never reports the
    /// sender as gone). `None` when a [`LoopbackCut`] owns the sender instead.
    _cut_keepalive: Option<watch::Sender<bool>>,
}

#[async_trait]
impl SessionTransport for LoopbackTransport {
    async fn establish(&self, _target: &DialTarget) -> Result<PeerIdentity, CoreError> {
        Ok(LOOPBACK_PEER)
    }

    async fn control_channel(&self) -> Result<Box<dyn ControlChannelDyn>, CoreError> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map(|c| Box::new(c) as Box<dyn ControlChannelDyn>)
            .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "control channel already taken"))
    }

    async fn video_sink(&self) -> Result<Box<dyn VideoSinkDyn>, CoreError> {
        match self.role {
            Role::Host => self
                .video_sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .map(|tx| {
                    Box::new(LoopbackSink {
                        tx,
                        cut: self.cut.clone(),
                    }) as Box<dyn VideoSinkDyn>
                })
                .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "video sink already taken")),
            Role::Controller => Err(CoreError::fatal(
                ErrorCode::Internal,
                "controller has no video sink",
            )),
        }
    }

    async fn video_source(&self) -> Result<Box<dyn VideoSourceDyn>, CoreError> {
        match self.role {
            Role::Controller => self
                .video_source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .map(|rx| {
                    Box::new(LoopbackSource {
                        rx,
                        cut: self.cut.clone(),
                    }) as Box<dyn VideoSourceDyn>
                })
                .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "video source already taken")),
            Role::Host => Err(CoreError::fatal(
                ErrorCode::Internal,
                "host has no video source",
            )),
        }
    }

    fn health(&self) -> ConnHealth {
        ConnHealth {
            path: PathKind::Direct,
            rtt_us: 5_000,
            loss_fraction: 0.0,
            estimated_bandwidth_bps: 50_000_000,
            frames_dropped: 0,
            state: LinkState::Live,
        }
    }
}

/// A fault handle for a wired pair: severs the link to model the peer's QUIC connection dropping
/// **without** a clean `Bye`. After [`cut`](Self::cut), every control `recv`/`send` and video `next`
/// on both ends returns a transport error — the orchestrators' transport-loss path (Active →
/// Suspended → reconnect window), which a graceful `Bye` must *not* trigger.
pub struct LoopbackCut {
    tx: watch::Sender<bool>,
}

impl LoopbackCut {
    /// Sever the link. Idempotent.
    pub fn cut(&self) {
        let _ = self.tx.send(true);
    }
}

/// Shared builder: wires both transports around one cut receiver. The caller decides who owns the
/// cut `Sender` (a keepalive on the host transport for the plain pair, or a [`LoopbackCut`]).
fn build_pair(
    cut_rx: watch::Receiver<bool>,
    keepalive: Option<watch::Sender<bool>>,
) -> (Arc<LoopbackTransport>, Arc<LoopbackTransport>) {
    let (h2c_tx, h2c_rx) = mpsc::channel(64); // host → controller control
    let (c2h_tx, c2h_rx) = mpsc::channel(64); // controller → host control
    let (vid_tx, vid_rx) = mpsc::channel(8); // host → controller video (bounded, droppable)

    let host = LoopbackTransport {
        role: Role::Host,
        control: Mutex::new(Some(LoopbackControl {
            tx: h2c_tx,
            rx: c2h_rx,
            cut: cut_rx.clone(),
        })),
        video_sink: Mutex::new(Some(vid_tx)),
        video_source: Mutex::new(None),
        cut: cut_rx.clone(),
        _cut_keepalive: keepalive,
    };
    let controller = LoopbackTransport {
        role: Role::Controller,
        control: Mutex::new(Some(LoopbackControl {
            tx: c2h_tx,
            rx: h2c_rx,
            cut: cut_rx.clone(),
        })),
        video_sink: Mutex::new(None),
        video_source: Mutex::new(Some(vid_rx)),
        cut: cut_rx,
        _cut_keepalive: None,
    };
    (Arc::new(host), Arc::new(controller))
}

/// Build a wired host/controller transport pair sharing in-memory channels. Returns
/// `(host_transport, controller_transport)`. Video flows host → controller only. The link can never
/// be severed (no fault handle); use [`loopback_pair_with_cut`] for that.
#[must_use]
pub fn loopback_pair() -> (Arc<LoopbackTransport>, Arc<LoopbackTransport>) {
    let (cut_tx, cut_rx) = watch::channel(false);
    // Stash the sender on the host transport so `changed()` never reports it as gone.
    build_pair(cut_rx, Some(cut_tx))
}

/// Like [`loopback_pair`] but also returns a [`LoopbackCut`] that can sever the link mid-session to
/// exercise the abrupt-transport-loss path. Returns `(host, controller, cut)`.
#[must_use]
pub fn loopback_pair_with_cut() -> (Arc<LoopbackTransport>, Arc<LoopbackTransport>, LoopbackCut) {
    let (cut_tx, cut_rx) = watch::channel(false);
    let (host, controller) = build_pair(cut_rx, None);
    (host, controller, LoopbackCut { tx: cut_tx })
}

/// A content-free [`FrameSink`] that tallies delivery. Cheap to clone (shares its counters), so a
/// test can hold one handle while another is injected into the session.
#[derive(Clone, Default)]
pub struct CountingFrameSink {
    inner: Arc<CountingState>,
}

#[derive(Default)]
struct CountingState {
    configured: AtomicBool,
    pushed: AtomicU64,
    keyframes: AtomicU64,
    last_frame_id: AtomicU64,
}

impl CountingFrameSink {
    /// A fresh sink with zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Total frames pushed (not dropped).
    #[must_use]
    pub fn pushed(&self) -> u64 {
        self.inner.pushed.load(Ordering::Relaxed)
    }
    /// Total keyframes pushed.
    #[must_use]
    pub fn keyframes(&self) -> u64 {
        self.inner.keyframes.load(Ordering::Relaxed)
    }
    /// The id of the most recently pushed frame.
    #[must_use]
    pub fn last_frame_id(&self) -> u64 {
        self.inner.last_frame_id.load(Ordering::Relaxed)
    }
    /// Whether `configure` has been called.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.inner.configured.load(Ordering::Relaxed)
    }
}

impl FrameSink for CountingFrameSink {
    fn configure(&self, _config: &StreamConfig) -> Result<(), CoreError> {
        self.inner.configured.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn push(&self, frame: EncodedFrame) -> PushResult {
        self.inner.pushed.fetch_add(1, Ordering::Relaxed);
        if frame.is_keyframe {
            self.inner.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        self.inner
            .last_frame_id
            .store(frame.frame_id, Ordering::Relaxed);
        PushResult::Sent
    }
}
