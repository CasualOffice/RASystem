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
use tokio::sync::mpsc;

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

struct LoopbackControl {
    tx: mpsc::Sender<ControlMsg>,
    rx: mpsc::Receiver<ControlMsg>,
}

#[async_trait]
impl ControlChannelDyn for LoopbackControl {
    async fn send(&mut self, msg: ControlMsg) -> Result<(), CoreError> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| CoreError::fatal(ErrorCode::TransportError, "loopback control closed"))
    }
    async fn recv(&mut self) -> Result<ControlMsg, CoreError> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| CoreError::fatal(ErrorCode::TransportError, "loopback control closed"))
    }
}

struct LoopbackSink {
    tx: mpsc::Sender<EncodedFrame>,
}

impl VideoSinkDyn for LoopbackSink {
    fn send_frame(&self, frame: EncodedFrame) -> SendOutcome {
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
}

#[async_trait]
impl VideoSourceDyn for LoopbackSource {
    async fn next(&mut self) -> Result<VideoEvent, CoreError> {
        self.rx
            .recv()
            .await
            .map(VideoEvent::Frame)
            .ok_or_else(|| CoreError::fatal(ErrorCode::TransportError, "loopback video closed"))
    }
}

/// One end of an in-memory session. Build a wired pair with [`loopback_pair`].
pub struct LoopbackTransport {
    role: Role,
    control: Mutex<Option<LoopbackControl>>,
    video_sink: Mutex<Option<mpsc::Sender<EncodedFrame>>>,
    video_source: Mutex<Option<mpsc::Receiver<EncodedFrame>>>,
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
                .map(|tx| Box::new(LoopbackSink { tx }) as Box<dyn VideoSinkDyn>)
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
                .map(|rx| Box::new(LoopbackSource { rx }) as Box<dyn VideoSourceDyn>)
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

/// Build a wired host/controller transport pair sharing in-memory channels. Returns
/// `(host_transport, controller_transport)`. Video flows host → controller only.
#[must_use]
pub fn loopback_pair() -> (Arc<LoopbackTransport>, Arc<LoopbackTransport>) {
    let (h2c_tx, h2c_rx) = mpsc::channel(64); // host → controller control
    let (c2h_tx, c2h_rx) = mpsc::channel(64); // controller → host control
    let (vid_tx, vid_rx) = mpsc::channel(8); // host → controller video (bounded, droppable)

    let host = LoopbackTransport {
        role: Role::Host,
        control: Mutex::new(Some(LoopbackControl {
            tx: h2c_tx,
            rx: c2h_rx,
        })),
        video_sink: Mutex::new(Some(vid_tx)),
        video_source: Mutex::new(None),
    };
    let controller = LoopbackTransport {
        role: Role::Controller,
        control: Mutex::new(Some(LoopbackControl {
            tx: c2h_tx,
            rx: h2c_rx,
        })),
        video_sink: Mutex::new(None),
        video_source: Mutex::new(Some(vid_rx)),
    };
    (Arc::new(host), Arc::new(controller))
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
