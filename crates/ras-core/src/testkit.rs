//! In-memory test doubles for the DI seams (design §5.4 "synthetic in-memory impl in tests").
//!
//! [`loopback_pair`] wires a host-side and controller-side [`SessionTransport`] together with plain
//! `tokio` channels — no iroh, no sockets, no OS. Control is a reliable bidi pair; video is a
//! bounded, drop-on-full host→controller path (mirroring the real droppable video semantics).
//! [`CountingFrameSink`] is a content-free [`FrameSink`] that just tallies what it receives, for
//! asserting end-to-end delivery and keyframe plumbing.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use crate::deps::{
    AudioSink, AudioSourceDyn, ControlChannelDyn, DialTarget, FrameSink, PeerIdentity, PushResult,
    SessionTransport, VideoSinkDyn, VideoSourceDyn,
};
use crate::CoreError;
use ras_media::{EncodedAudio, EncodedFrame, StreamConfig};
use ras_protocol::{ControlMsg, ErrorCode};
use ras_transport_iroh::{ConnHealth, EndpointId, LinkState, PathKind, SendOutcome, VideoEvent};

/// The fixed identity the loopback "peer" presents (identity only — never authorization).
const LOOPBACK_PEER: EndpointId = EndpointId([7u8; 32]);

#[derive(Clone, Copy)]
enum Role {
    Host,
    Controller,
}

/// Error a channel returns once the link has been [cut](LoopbackFaults::cut) — models the peer's QUIC
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
    tx: mpsc::Sender<VideoEvent>,
    cut: watch::Receiver<bool>,
}

impl VideoSinkDyn for LoopbackSink {
    fn send_frame(&self, frame: EncodedFrame) -> SendOutcome {
        if *self.cut.borrow() {
            return SendOutcome::DroppedStale; // link severed — nothing to send onto
        }
        // Non-blocking, drop-on-full — the video path is droppable by design. The loopback carries
        // `VideoEvent` (not raw frames) to mirror the real transport, whose source yields both
        // `Frame` and transport-generated `FrameDropped`.
        match self.tx.try_send(VideoEvent::Frame(frame)) {
            Ok(()) => SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => SendOutcome::DroppedCongested,
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::DroppedStale,
        }
    }
}

struct LoopbackSource {
    rx: mpsc::Receiver<VideoEvent>,
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
                    CutPoll::SenderGone => return self.rx.recv().await.ok_or_else(video_closed),
                },
                ev = self.rx.recv() => return ev.ok_or_else(video_closed),
            }
        }
    }
}

/// Host-side audio egress double: pushes encoded packets into the host→controller audio channel.
/// Non-blocking, drop-on-full (audio is droppable like video).
struct LoopbackAudioSink {
    tx: mpsc::Sender<EncodedAudio>,
    cut: watch::Receiver<bool>,
}

impl AudioSink for LoopbackAudioSink {
    fn send_audio(&self, packet: EncodedAudio) {
        if *self.cut.borrow() {
            return; // link severed — nothing to send onto
        }
        let _ = self.tx.try_send(packet); // drop-on-full / closed; loss is not an error
    }
}

/// Controller-side audio ingress double: yields packets from the host→controller audio channel.
struct LoopbackAudioSource {
    rx: mpsc::Receiver<EncodedAudio>,
    cut: watch::Receiver<bool>,
}

fn audio_closed() -> CoreError {
    CoreError::fatal(ErrorCode::TransportError, "loopback audio closed")
}

#[async_trait]
impl AudioSourceDyn for LoopbackAudioSource {
    async fn next(&mut self) -> Result<EncodedAudio, CoreError> {
        if *self.cut.borrow() {
            return Err(severed());
        }
        loop {
            tokio::select! {
                verdict = poll_cut(&mut self.cut) => match verdict {
                    CutPoll::Cut => return Err(severed()),
                    CutPoll::Continue => continue,
                    CutPoll::SenderGone => return self.rx.recv().await.ok_or_else(audio_closed),
                },
                pkt = self.rx.recv() => return pkt.ok_or_else(audio_closed),
            }
        }
    }
}

/// One end of an in-memory session. Build a wired pair with [`loopback_pair`] (or
/// [`loopback_pair_with_faults`] to also get a fault handle).
pub struct LoopbackTransport {
    role: Role,
    control: Mutex<Option<LoopbackControl>>,
    video_sink: Mutex<Option<mpsc::Sender<VideoEvent>>>,
    video_source: Mutex<Option<mpsc::Receiver<VideoEvent>>>,
    audio_sink: Mutex<Option<mpsc::Sender<EncodedAudio>>>,
    audio_source: Mutex<Option<mpsc::Receiver<EncodedAudio>>>,
    /// Shared severed-link flag, cloned into every sink/source this transport hands out.
    cut: watch::Receiver<bool>,
    /// Keeps the cut `Sender` alive for a plain [`loopback_pair`] (so `changed()` never reports the
    /// sender as gone). `None` when a [`LoopbackFaults`] owns the sender instead.
    _cut_keepalive: Option<watch::Sender<bool>>,
}

#[async_trait]
impl SessionTransport for LoopbackTransport {
    async fn establish(&self, _target: &DialTarget) -> Result<PeerIdentity, CoreError> {
        Ok(LOOPBACK_PEER)
    }

    async fn reconnect(&self, _target: &DialTarget) -> Result<PeerIdentity, CoreError> {
        // Model a re-dial: block until the link is healed (the fault handle re-arms the channels and
        // clears the cut), then report the re-authenticated peer. The caller then re-fetches its
        // freshly-armed channels and re-runs the handshake (which re-validates the grant host-side).
        let mut cut = self.cut.clone();
        loop {
            if !*cut.borrow() {
                return Ok(LOOPBACK_PEER);
            }
            if cut.changed().await.is_err() {
                return Err(CoreError::fatal(
                    ErrorCode::TransportError,
                    "loopback closed",
                ));
            }
        }
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

    async fn audio_sink(&self) -> Result<Box<dyn AudioSink>, CoreError> {
        match self.role {
            Role::Host => self
                .audio_sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .map(|tx| {
                    Box::new(LoopbackAudioSink {
                        tx,
                        cut: self.cut.clone(),
                    }) as Box<dyn AudioSink>
                })
                .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "audio sink already taken")),
            Role::Controller => Err(CoreError::fatal(
                ErrorCode::Internal,
                "controller has no audio sink",
            )),
        }
    }

    async fn audio_source(&self) -> Result<Box<dyn AudioSourceDyn>, CoreError> {
        match self.role {
            Role::Controller => self
                .audio_source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .map(|rx| {
                    Box::new(LoopbackAudioSource {
                        rx,
                        cut: self.cut.clone(),
                    }) as Box<dyn AudioSourceDyn>
                })
                .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "audio source already taken")),
            Role::Host => Err(CoreError::fatal(
                ErrorCode::Internal,
                "host has no audio source",
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

/// A fault handle for a wired pair. Two independent injectors for the two failure modes the
/// orchestrators must handle:
/// - [`cut`](Self::cut) severs the link (models the peer's QUIC connection dropping **without** a
///   clean `Bye`) → the transport-loss path (Active → Suspended → reconnect window), which a
///   graceful `Bye` must *not* trigger.
/// - [`inject_video`](Self::inject_video) pushes a [`VideoEvent`] straight into the controller's
///   video source — notably a `FrameDropped`, which the real transport (not the host) generates on
///   loss, to exercise the controller's loss handling (freeze-on-last-good + keyframe recovery).
pub struct LoopbackFaults {
    cut_tx: watch::Sender<bool>,
    video_tx: mpsc::Sender<VideoEvent>,
    // Held so [`heal`](Self::heal) can re-arm both ends after a cut (models a re-dialed connection).
    host: Arc<LoopbackTransport>,
    controller: Arc<LoopbackTransport>,
}

impl LoopbackFaults {
    /// Sever the link. Idempotent.
    pub fn cut(&self) {
        let _ = self.cut_tx.send(true);
    }

    /// **Heal** a severed link — re-arm fresh channels on both transports (modeling a re-dialed
    /// connection whose streams are brand new) and then clear the cut. A [`SessionTransport::reconnect`]
    /// awaiting on either transport wakes once the cut clears, and its subsequent `control_channel()` /
    /// `video_source()` yields the freshly-armed channel. Ordering matters: slots are filled **before**
    /// the cut is cleared, so a woken reconnecter never races an empty slot.
    pub fn heal(&self) {
        let (h2c_tx, h2c_rx) = mpsc::channel(64); // host → controller control
        let (c2h_tx, c2h_rx) = mpsc::channel(64); // controller → host control
        let (vid_tx, vid_rx) = mpsc::channel(8); // host → controller video
        let (aud_tx, aud_rx) = mpsc::channel(16); // host → controller audio
        let cut_rx = self.cut_tx.subscribe();
        *self
            .host
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LoopbackControl {
            tx: h2c_tx,
            rx: c2h_rx,
            cut: cut_rx.clone(),
        });
        *self
            .host
            .video_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(vid_tx);
        *self
            .host
            .audio_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(aud_tx);
        *self
            .controller
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LoopbackControl {
            tx: c2h_tx,
            rx: h2c_rx,
            cut: cut_rx,
        });
        *self
            .controller
            .video_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(vid_rx);
        *self
            .controller
            .audio_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(aud_rx);
        // Clear the cut LAST, so a woken `reconnect()` always finds armed slots.
        let _ = self.cut_tx.send(false);
    }

    /// **Heal the host side only** — models a *silent re-dialer*: the transport re-establishes (so the
    /// host's [`reconnect`](SessionTransport::reconnect) wakes and its `control_channel()` yields a fresh
    /// channel, and its `Hello` send succeeds) but the peer never sends its grant. The host's
    /// post-reconnect handshake `recv()` must be window-bounded, or the host control task hangs forever
    /// (ADR-091 fail-closed). The controller's slots are left empty, so a controller re-dial fails and it
    /// terminates without ever writing.
    ///
    /// Returns `(controller→host sender, host→controller receiver)` — the caller **must keep both alive**
    /// for the duration of the test: dropping the sender would close the channel and turn the intended
    /// *hang* into an immediate error (masking the defect), and dropping the receiver would fail the
    /// host's `Hello` send before it ever reaches the read under test.
    #[must_use]
    pub fn heal_host_only(&self) -> (mpsc::Sender<ControlMsg>, mpsc::Receiver<ControlMsg>) {
        let (h2c_tx, h2c_rx) = mpsc::channel(64); // host → controller (kept open so `Hello` buffers)
        let (c2h_tx, c2h_rx) = mpsc::channel(64); // controller → host (kept open, never written)
        let (vid_tx, _vid_rx) = mpsc::channel(8);
        let (aud_tx, _aud_rx) = mpsc::channel(16);
        let cut_rx = self.cut_tx.subscribe();
        *self
            .host
            .control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LoopbackControl {
            tx: h2c_tx,
            rx: c2h_rx,
            cut: cut_rx,
        });
        *self
            .host
            .video_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(vid_tx);
        *self
            .host
            .audio_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(aud_tx);
        // Controller slots deliberately left empty. Clear the cut LAST so a woken host `reconnect()`
        // finds its armed slot.
        let _ = self.cut_tx.send(false);
        (c2h_tx, h2c_rx)
    }

    /// Inject a video event into the controller's source (e.g. a transport-generated `FrameDropped`).
    pub async fn inject_video(&self, ev: VideoEvent) {
        let _ = self.video_tx.send(ev).await;
    }
}

/// Shared builder: wires both transports around one cut receiver and one video channel. Returns the
/// pair plus a spare clone of the video sender (the caller either drops it — plain pair — or hands
/// it to a [`LoopbackFaults`] for injection). The caller likewise owns the cut `Sender`.
fn build_pair(
    cut_rx: watch::Receiver<bool>,
    keepalive: Option<watch::Sender<bool>>,
) -> (
    Arc<LoopbackTransport>,
    Arc<LoopbackTransport>,
    mpsc::Sender<VideoEvent>,
) {
    let (h2c_tx, h2c_rx) = mpsc::channel(64); // host → controller control
    let (c2h_tx, c2h_rx) = mpsc::channel(64); // controller → host control
    let (vid_tx, vid_rx) = mpsc::channel(8); // host → controller video (bounded, droppable)
    let (aud_tx, aud_rx) = mpsc::channel(16); // host → controller audio (bounded, droppable)

    let host = LoopbackTransport {
        role: Role::Host,
        control: Mutex::new(Some(LoopbackControl {
            tx: h2c_tx,
            rx: c2h_rx,
            cut: cut_rx.clone(),
        })),
        video_sink: Mutex::new(Some(vid_tx.clone())),
        video_source: Mutex::new(None),
        audio_sink: Mutex::new(Some(aud_tx)),
        audio_source: Mutex::new(None),
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
        audio_sink: Mutex::new(None),
        audio_source: Mutex::new(Some(aud_rx)),
        cut: cut_rx,
        _cut_keepalive: None,
    };
    (Arc::new(host), Arc::new(controller), vid_tx)
}

/// Build a wired host/controller transport pair sharing in-memory channels. Returns
/// `(host_transport, controller_transport)`. Video flows host → controller only. The link can never
/// be severed (no fault handle); use [`loopback_pair_with_faults`] for that.
#[must_use]
pub fn loopback_pair() -> (Arc<LoopbackTransport>, Arc<LoopbackTransport>) {
    let (cut_tx, cut_rx) = watch::channel(false);
    // Stash the sender on the host transport so `changed()` never reports it as gone.
    let (host, controller, _spare_video_tx) = build_pair(cut_rx, Some(cut_tx));
    (host, controller)
}

/// Like [`loopback_pair`] but also returns a [`LoopbackFaults`] that can sever the link or inject
/// video events mid-session. Returns `(host, controller, faults)`.
#[must_use]
pub fn loopback_pair_with_faults() -> (
    Arc<LoopbackTransport>,
    Arc<LoopbackTransport>,
    LoopbackFaults,
) {
    let (cut_tx, cut_rx) = watch::channel(false);
    let (host, controller, video_tx) = build_pair(cut_rx, None);
    let faults = LoopbackFaults {
        cut_tx,
        video_tx,
        host: host.clone(),
        controller: controller.clone(),
    };
    (host, controller, faults)
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
    /// Dimensions of the most recent `configure` call — i.e. what the decoder is actually set up for
    /// (distinct from `last_width`/`last_height`, which track pushed *frame* dims). A mid-stream
    /// resolution change must update these, or the decoder keeps decoding at the old size.
    configured_width: AtomicU32,
    configured_height: AtomicU32,
    pushed: AtomicU64,
    keyframes: AtomicU64,
    last_frame_id: AtomicU64,
    last_width: AtomicU32,
    last_height: AtomicU32,
    /// Set if a frame arrived whose config dimensions differ from the previous frame's but which was
    /// **not** a keyframe — i.e. a resolution change reached the decoder without an accompanying IDR
    /// (a black-screen / torn-frame bug). Must stay false.
    resize_without_keyframe: AtomicBool,
    /// Whether the very first frame this sink received was a keyframe (the configure contract: a fresh
    /// decoder's first frame must be an IDR). Meaningful only once `pushed() >= 1`.
    first_was_keyframe: AtomicBool,
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
    /// The width of the most recent `configure` call (what the decoder is actually set up for).
    #[must_use]
    pub fn configured_width(&self) -> u32 {
        self.inner.configured_width.load(Ordering::Relaxed)
    }
    /// The height of the most recent `configure` call.
    #[must_use]
    pub fn configured_height(&self) -> u32 {
        self.inner.configured_height.load(Ordering::Relaxed)
    }
    /// The config width of the most recently pushed frame.
    #[must_use]
    pub fn last_width(&self) -> u32 {
        self.inner.last_width.load(Ordering::Relaxed)
    }
    /// The config height of the most recently pushed frame.
    #[must_use]
    pub fn last_height(&self) -> u32 {
        self.inner.last_height.load(Ordering::Relaxed)
    }
    /// Whether any resolution change reached the decoder without an accompanying keyframe (must be
    /// false — every dimension change must arrive on an IDR, or the decoder shows a black/torn frame).
    #[must_use]
    pub fn saw_resize_without_keyframe(&self) -> bool {
        self.inner.resize_without_keyframe.load(Ordering::Relaxed)
    }
    /// Whether the first frame this sink received was a keyframe (call only after `pushed() >= 1`).
    #[must_use]
    pub fn first_frame_was_keyframe(&self) -> bool {
        self.inner.first_was_keyframe.load(Ordering::Relaxed)
    }
}

impl FrameSink for CountingFrameSink {
    fn configure(&self, config: &StreamConfig) -> Result<(), CoreError> {
        self.inner.configured.store(true, Ordering::Relaxed);
        self.inner
            .configured_width
            .store(config.width, Ordering::Relaxed);
        self.inner
            .configured_height
            .store(config.height, Ordering::Relaxed);
        Ok(())
    }
    fn push(&self, frame: EncodedFrame) -> PushResult {
        let prev = self.inner.pushed.fetch_add(1, Ordering::Relaxed);
        if prev == 0 {
            self.inner
                .first_was_keyframe
                .store(frame.is_keyframe, Ordering::Relaxed);
        }
        if frame.is_keyframe {
            self.inner.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        // Track config dimensions and flag any dimension change that arrives on a non-keyframe.
        let (w, h) = (frame.config.width, frame.config.height);
        let pw = self.inner.last_width.swap(w, Ordering::Relaxed);
        let ph = self.inner.last_height.swap(h, Ordering::Relaxed);
        if (pw != 0 || ph != 0) && (pw != w || ph != h) && !frame.is_keyframe {
            self.inner
                .resize_without_keyframe
                .store(true, Ordering::Relaxed);
        }
        self.inner
            .last_frame_id
            .store(frame.frame_id, Ordering::Relaxed);
        PushResult::Sent
    }
}
