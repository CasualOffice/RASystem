//! The concrete [`SessionTransport`] backed by the iroh/QUIC transport.
//!
//! This is the seam that lets the host + controller orchestrators run over a real network peer
//! instead of the in-memory [`crate::testkit`] loopback — with **no change** to the orchestrators or
//! the wire, because both sides program against `Arc<dyn SessionTransport>`. Wrap an already
//! *established* [`ras_transport_iroh::Session`] (dialed by the controller / accepted by the host)
//! together with the [`Endpoint`] that owns it, and hand the `Arc` to
//! [`ControllerSession::new`](crate::ControllerSession::new) /
//! [`HostSession::new`](crate::HostSession::new).
//!
//! Transport authenticates *identity* only, never *authorization* (Invariant 9): [`establish`] just
//! returns the peer identity the QUIC/TLS handshake already authenticated; grant/lease validation
//! stays in `ras-core` behind the [`GrantValidator`](crate::deps::GrantValidator) seam.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::deps::{
    AudioSink, AudioSourceDyn, ControlChannelDyn, DialTarget, PeerIdentity, SessionTransport,
    VideoSinkDyn, VideoSourceDyn,
};
use crate::CoreError;
use ras_media::{EncodedAudio, EncodedFrame};
use ras_protocol::{ControlMsg, ErrorCode};
use ras_transport_iroh::{
    AudioSink as IrohAudioSinkInner, AudioSource as IrohAudioSourceInner, ConnHealth,
    ControlChannel, Endpoint, EndpointAddr, HealthObserver, SendOutcome, Session, VideoEvent,
    VideoSink, VideoSource,
};

/// How [`IrohSessionTransport::reconnect`] re-establishes the wire to the **same peer** after a
/// transport drop (ADR-091). Captured at construction via [`IrohSessionTransport::with_reconnect`];
/// without it, `reconnect` reports unsupported and the session terminates on transport loss (the
/// pre-existing default). The strategy mirrors the role's *first-connect* code exactly — a re-dial is
/// "do what connect/accept did, reusing the same endpoint + target" — so the reconnected connection
/// carries the peer's freshly QUIC/TLS-authenticated `EndpointId` identically to a first connect. The
/// grant re-validation (signature / sender-constraint / expiry) is layered on top by `ras-core`
/// when it re-presents the grant; the transport authenticates identity, never authority (Inv 9).
enum ReconnectStrategy {
    /// Controller side: re-dial the stored target on the session ALPN (`Endpoint::connect`), exactly
    /// as the app's first connect did. iroh re-authenticates the peer identity in the QUIC/TLS
    /// handshake; we still assert it matches the originally-authenticated peer (endpoint continuity)
    /// before accepting the resumed connection, so the grant's sender-constraint still matches.
    ControllerRedial(EndpointAddr),
    /// Host side: re-accept the next inbound session on this endpoint (`Endpoint::accept`), matching
    /// the first-connect accept path. A re-dialer arrives as a fresh inbound connection; we filter to
    /// the session ALPN and the same peer identity (any other inbound is skipped) so an unrelated
    /// dialer can never hijack the resume slot.
    HostReaccept,
}

/// The mutable per-connection state a [`reconnect`](IrohSessionTransport::reconnect) swaps atomically:
/// the live session and its authenticated peer identity. Behind one async mutex so a channel fetch
/// (`control_channel`/`video_source`/…) always reads a coherent, current connection, and a re-dial
/// replaces both together. `session` is an `Option` only so `reconnect` can `take()` and explicitly
/// **close** the stale connection before re-dialing (freeing the old QUIC connection so iroh opens a
/// genuinely fresh one to the same peer, rather than handing back the pooled dead one); it is always
/// `Some` outside that brief window, and the channel-fetch paths treat `None` as a fail-closed error.
struct ConnState {
    session: Option<Session>,
    remote: PeerIdentity,
}

impl ConnState {
    /// Borrow the live session, or a fail-closed error if it is mid-swap (should not happen outside a
    /// concurrent reconnect, which holds the same lock — so in practice always `Some`).
    fn session(&self) -> Result<&Session, CoreError> {
        self.session
            .as_ref()
            .ok_or_else(|| CoreError::fatal(ErrorCode::TransportError, "session mid-reconnect"))
    }
}

/// A [`SessionTransport`] over one established iroh session. Holds a shared handle to the owning
/// [`Endpoint`] so the connection (and its background tasks) outlive the session — while the same
/// endpoint stays free to accept further controllers (the host serves one viewer at a time but need
/// not rebind between them).
pub struct IrohSessionTransport {
    // Keeps the endpoint alive for the session's lifetime without taking exclusive ownership. Also
    // reused by `reconnect` to re-dial / re-accept on the very same endpoint identity (ADR-091).
    endpoint: Arc<Endpoint>,
    // The live connection state, swapped wholesale on a successful reconnect.
    conn: Mutex<ConnState>,
    // One persistent health observer so the windowed-loss baseline survives across ABR ticks (a fresh
    // observer per tick would reset the window and report the lifetime loss average instead). Behind a
    // *sync* mutex — separate from `conn` — so the non-async `health()` never blocks on I/O and never
    // needs a runtime; a reconnect swaps it to one bound to the fresh connection.
    health: std::sync::Mutex<HealthObserver>,
    // How to re-establish the wire on a drop; `None` ⇒ reconnect unsupported (default).
    reconnect: Option<ReconnectStrategy>,
}

impl IrohSessionTransport {
    /// Wrap an already-established session together with a shared handle to its owning endpoint. The
    /// QUIC/TLS handshake has already authenticated the peer; its identity is captured here for
    /// [`establish`]. Reconnect is **unsupported** unless enabled via [`Self::with_reconnect_controller`]
    /// / [`Self::with_reconnect_host`] — a transport with no re-dial strategy behaves exactly as before
    /// (the session terminates on transport loss instead of resuming).
    #[must_use]
    pub fn new(endpoint: Arc<Endpoint>, session: Session) -> Self {
        let remote = session.remote();
        let health = session.health();
        Self {
            endpoint,
            conn: Mutex::new(ConnState {
                session: Some(session),
                remote,
            }),
            health: std::sync::Mutex::new(health),
            reconnect: None,
        }
    }

    /// Enable ADR-091 session resume for the **controller** role: on a transport drop, re-dial `target`
    /// on the session ALPN (the same address the first connect used). Pass the very
    /// [`EndpointAddr`] the controller originally dialed (id + direct/relay hints) so a NAT-rebind after
    /// an outage still reaches the host. The re-dial re-authenticates the peer identity and is only
    /// accepted if it matches the originally-authenticated peer (endpoint continuity, Inv 3/9).
    #[must_use]
    pub fn with_reconnect_controller(mut self, target: EndpointAddr) -> Self {
        self.reconnect = Some(ReconnectStrategy::ControllerRedial(target));
        self
    }

    /// Enable ADR-091 session resume for the **host** role: on a transport drop, re-accept the next
    /// inbound session-ALPN connection from the *same* peer on this endpoint (the same accept path the
    /// first connect used). Inbound connections from any other peer, or on the bootstrap ALPN, are
    /// skipped so an unrelated dialer cannot claim the resume slot.
    #[must_use]
    pub fn with_reconnect_host(mut self) -> Self {
        self.reconnect = Some(ReconnectStrategy::HostReaccept);
        self
    }
}

#[async_trait]
impl SessionTransport for IrohSessionTransport {
    async fn establish(&self, _target: &DialTarget) -> Result<PeerIdentity, CoreError> {
        // The connection is already established; return the identity the transport authenticated.
        // Identity, not authority (Invariant 9).
        Ok(self.conn.lock().await.remote)
    }

    async fn reconnect(&self, _target: &DialTarget) -> Result<PeerIdentity, CoreError> {
        // `ras-core` passes a placeholder target here (the concrete transport knows its own peer); the
        // real dial target / accept role is the one captured at construction. Without a strategy this
        // stays the trait's fail-closed default (the session terminates rather than resuming).
        let Some(strategy) = &self.reconnect else {
            return Err(CoreError::fatal(
                ErrorCode::Internal,
                "reconnect not supported",
            ));
        };

        // The identity we must re-authenticate against — continuity across the drop. A reconnected
        // connection that authenticates a *different* endpoint is refused, so the grant's
        // sender-constraint (bound to this exact peer) still matches on the ras-core re-validation
        // (Inv 3/9). This is checked before we swap in the new connection.
        //
        // Take + close the stale session first: the drop that triggered this reconnect leaves the old
        // QUIC connection dead-but-not-yet-evicted, and iroh pools connections by peer id — so without
        // closing it a re-dial can hand back the dead pooled connection (and the host's re-accept would
        // never fire). Closing it frees the pool slot so `connect`/`accept` establish a genuinely fresh
        // connection. Held across the whole reconnect so a channel fetch racing in sees the fail-closed
        // `None` rather than the dead session.
        let (expected, stale) = {
            let mut guard = self.conn.lock().await;
            (guard.remote, guard.session.take())
        };
        if let Some(stale) = stale {
            stale.close(ErrorCode::NormalClosure).await;
        }

        // Re-establish the wire, mirroring the role's first-connect path on the SAME endpoint. The new
        // `Session` yields fresh control/video/audio channels the moment it replaces the old one.
        let fresh = match strategy {
            ReconnectStrategy::ControllerRedial(target) => {
                // Exactly the controller's first connect: dial the session ALPN.
                self.endpoint.connect(target).await?
            }
            ReconnectStrategy::HostReaccept => {
                // Re-accept inbound sessions until the same peer re-dials on the session ALPN; skip any
                // bootstrap-ALPN or unrelated-peer connection (never hijack the resume slot). Bounded by
                // the caller's reconnect-window `timeout` around `reconnect`, so this cannot spin
                // forever.
                loop {
                    match self.endpoint.accept().await? {
                        Some(s) if !s.is_bootstrap() && s.remote() == expected => break s,
                        // Wrong ALPN / wrong peer: drop it and keep waiting for the real re-dialer.
                        Some(_) => continue,
                        // Endpoint closed → cannot resume.
                        None => {
                            return Err(CoreError::fatal(
                                ErrorCode::TransportError,
                                "endpoint closed during reconnect",
                            ))
                        }
                    }
                }
            }
        };

        // Endpoint continuity: the re-established connection MUST authenticate the same peer identity.
        // Refuse otherwise (the ControllerRedial path can in principle reach a rebound-but-different
        // node; fail closed rather than resume to a stranger).
        let remote = fresh.remote();
        if remote != expected {
            return Err(CoreError::fatal(
                ErrorCode::IdentityMismatch,
                "reconnect authenticated a different peer",
            ));
        }

        // Swap the whole connection state atomically: subsequent `control_channel()` / `video_*` /
        // `audio_*` fetches now come off the fresh connection. A new health observer is bound to it
        // (the old one referenced the dead connection's stats).
        let health = fresh.health();
        *self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = health;
        let mut guard = self.conn.lock().await;
        *guard = ConnState {
            session: Some(fresh),
            remote,
        };
        Ok(remote)
    }

    async fn control_channel(&self) -> Result<Box<dyn ControlChannelDyn>, CoreError> {
        let guard = self.conn.lock().await;
        Ok(Box::new(IrohControlChannel(
            guard.session()?.control().await?,
        )))
    }

    async fn video_sink(&self) -> Result<Box<dyn VideoSinkDyn>, CoreError> {
        self.conn
            .lock()
            .await
            .session()?
            .video_sink()
            .map(|s| Box::new(IrohVideoSink(s)) as Box<dyn VideoSinkDyn>)
            .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "no video sink for this role"))
    }

    async fn video_source(&self) -> Result<Box<dyn VideoSourceDyn>, CoreError> {
        self.conn
            .lock()
            .await
            .session()?
            .video_source()
            .map(|s| Box::new(IrohVideoSource(s)) as Box<dyn VideoSourceDyn>)
            .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "no video source for this role"))
    }

    async fn audio_sink(&self) -> Result<Box<dyn AudioSink>, CoreError> {
        // Audio flows host → controller over QUIC datagrams (ADR-077). The host owns the *right* to be
        // heard (the `audio.listen` gate in `ras-core` precedes this fetch); the transport owns the
        // wire. `None` on the controller role.
        self.conn
            .lock()
            .await
            .session()?
            .audio_sink()
            .map(|s| Box::new(IrohAudioSink(s)) as Box<dyn AudioSink>)
            .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "no audio sink for this role"))
    }

    async fn audio_source(&self) -> Result<Box<dyn AudioSourceDyn>, CoreError> {
        self.conn
            .lock()
            .await
            .session()?
            .audio_source()
            .map(|s| Box::new(IrohAudioSource(s)) as Box<dyn AudioSourceDyn>)
            .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "no audio source for this role"))
    }

    fn health(&self) -> ConnHealth {
        // A non-async, non-awaiting read (the latency invariant — a health poll must never stall on
        // I/O). The health observer is behind its own *sync* mutex, contended only by the brief
        // `reconnect` swap, so this takes it directly and derives the snapshot from live QUIC counters.
        self.health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }
}

/// Adapts the iroh [`ControlChannel`] to the object-safe [`ControlChannelDyn`]. The error types are
/// identical (`CoreError` and the transport error are both `ras_protocol::RasError`), so this is a
/// straight forward.
struct IrohControlChannel(ControlChannel);

#[async_trait]
impl ControlChannelDyn for IrohControlChannel {
    async fn send(&mut self, msg: ControlMsg) -> Result<(), CoreError> {
        self.0.send(msg).await
    }
    async fn recv(&mut self) -> Result<ControlMsg, CoreError> {
        self.0.recv().await
    }
}

/// Adapts the iroh [`VideoSink`] to [`VideoSinkDyn`]. Non-blocking, drop-on-pressure.
struct IrohVideoSink(VideoSink);

impl VideoSinkDyn for IrohVideoSink {
    fn send_frame(&self, frame: EncodedFrame) -> SendOutcome {
        // The iroh sink never errs at enqueue (loss is a `SendOutcome`, not an `Err`); map any
        // future fatal-path error conservatively to a stale drop rather than surfacing it here.
        self.0
            .send_frame(frame)
            .unwrap_or(SendOutcome::DroppedStale)
    }
}

/// Adapts the iroh [`VideoSource`] to [`VideoSourceDyn`].
struct IrohVideoSource(VideoSource);

#[async_trait]
impl VideoSourceDyn for IrohVideoSource {
    async fn next(&mut self) -> Result<VideoEvent, CoreError> {
        self.0.recv().await
    }
}

/// Adapts the iroh audio sink to [`AudioSink`]. Non-blocking, drop-on-pressure (audio is droppable).
struct IrohAudioSink(IrohAudioSinkInner);

impl AudioSink for IrohAudioSink {
    fn send_audio(&self, packet: EncodedAudio) {
        // The iroh audio sink never errs at enqueue (loss is a `SendOutcome`, not an `Err`); the
        // `deps::AudioSink` seam is fire-and-forget, so any outcome is simply discarded here.
        let _ = self.0.send_audio(packet);
    }
}

/// Adapts the iroh [`AudioSource`](IrohAudioSourceInner) to [`AudioSourceDyn`].
struct IrohAudioSource(IrohAudioSourceInner);

#[async_trait]
impl AudioSourceDyn for IrohAudioSource {
    async fn next(&mut self) -> Result<EncodedAudio, CoreError> {
        self.0.recv().await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::sync::Arc;
    use std::time::Duration;

    use super::IrohSessionTransport;
    use crate::deps::AllowAllValidator;
    use crate::testkit::CountingFrameSink;
    use crate::{
        ControllerSession, ControllerSessionConfig, HostSession, HostSessionConfig, SessionState,
        StopReason,
    };
    use ras_media::synthetic::{SyntheticCaptureBackend, SyntheticEncoder};
    use ras_media::MonitorId;
    use ras_protocol::KeyframeReason;
    use ras_transport_iroh::{Endpoint, EndpointAddr};

    /// Rewrite a freshly-bound endpoint's unspecified sockets (`0.0.0.0` / `[::]`) to loopback so a
    /// same-host dial is hermetic (no discovery, no relay, no egress).
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

    async fn wait_until<F: Fn() -> bool>(cond: F, tries: u32) -> bool {
        for _ in 0..tries {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cond()
    }

    /// The full `ras-core` spine — handshake → authorize gate → grant → media pump → keyframe
    /// requests → teardown — driven end-to-end over **two real iroh endpoints on loopback**, with
    /// the synthetic capture/encode doubles. This is the loopback e2e test's exact shape but with the
    /// loopback transport swapped for [`IrohSessionTransport`] on both sides, proving the swap is
    /// transparent to the orchestrators (the M2 promise). Hermetic: direct-address dial, no
    /// discovery/relay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spine_runs_over_real_iroh_transport() {
        // Establish one real iroh connection: host accepts, controller dials by direct address.
        let host_ep = Arc::new(Endpoint::bind().await.unwrap());
        let ctrl_ep = Arc::new(Endpoint::bind().await.unwrap());
        let host_id = host_ep.id();
        let host_addrs = to_loopback(&host_ep.bound_addrs());

        let accept_ep = host_ep.clone();
        let accept = tokio::spawn(async move {
            accept_ep
                .accept()
                .await
                .unwrap()
                .expect("an inbound session")
        });
        let ctrl_session = ctrl_ep.connect_direct(&host_id, &host_addrs).await.unwrap();
        let host_session = accept.await.unwrap();

        // Wrap each established side in the adapter and hand it to the orchestrators as `dyn`.
        let host_tp = Arc::new(IrohSessionTransport::new(host_ep, host_session));
        let ctrl_tp = Arc::new(IrohSessionTransport::new(ctrl_ep, ctrl_session));

        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(1280, 720),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(host_id)),
            ctrl_tp,
        );

        // Drive both sides concurrently: over real QUIC the host's `control_channel()` accepts the
        // bidi stream the controller opens, so `start()` and `connect()` must make progress together
        // (unlike the pre-wired loopback, which lets one complete before the other begins). This
        // models the real deployment, where host and controller run simultaneously on two machines.
        let (host_events, ctrl_events) = tokio::join!(host.start(), controller.connect());
        let _host_events = host_events.unwrap();
        let _ctrl_events = ctrl_events.unwrap();

        // Both reach Active through the real handshake + authorize gate over iroh.
        assert_eq!(host.state(), SessionState::Active);
        assert_eq!(controller.state(), SessionState::Active);

        // Frames flow over the per-frame uni-stream video path, starting on a keyframe.
        let sink = CountingFrameSink::new();
        controller
            .attach_renderer(Arc::new(sink.clone()))
            .await
            .unwrap();
        assert!(
            wait_until(|| sink.pushed() >= 5, 500).await,
            "expected frames over iroh, got {}",
            sink.pushed()
        );
        assert!(sink.is_configured(), "renderer configured before frames");
        assert!(sink.keyframes() >= 1, "stream must start on a keyframe");

        // A controller keyframe request rides the real control stream and yields a fresh IDR.
        let kf_before = sink.keyframes();
        controller
            .request_keyframe(KeyframeReason::UnrecoverableLoss)
            .await
            .unwrap();
        assert!(
            wait_until(|| sink.keyframes() > kf_before, 500).await,
            "keyframe request over iroh did not yield a new IDR (before={kf_before}, after={})",
            sink.keyframes()
        );

        // Clean teardown → both terminal.
        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
        assert_eq!(controller.state(), SessionState::Terminated);
        assert_eq!(host.state(), SessionState::Terminated);
    }

    /// A grant validator that authorizes with a fixed capability set — used to grant `audio.listen`
    /// (unlike `AllowAllValidator`, which authorizes but grants nothing).
    struct GrantsCaps(ras_policy::CapabilitySet);
    #[async_trait::async_trait]
    impl crate::deps::GrantValidator for GrantsCaps {
        async fn authorize(
            &self,
            _ctx: &crate::deps::SessionAuthContext,
        ) -> Result<crate::deps::GrantDecision, crate::CoreError> {
            Ok(crate::deps::GrantDecision::Authorized(self.0.clone()))
        }
    }

    /// Controller-side audio output that tallies packets delivered through the transport.
    struct RecordingAudioOut(std::sync::atomic::AtomicU64);
    impl crate::deps::AudioOutput for RecordingAudioOut {
        fn push(&self, _packet: ras_media::EncodedAudio) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// An audio encoder double that emits a small, **MTU-safe** packet per chunk. Real Opus packets are
    /// ≈240 B (well under the datagram MTU); the `synthetic` passthrough emits raw ~3.8 KB PCM frames,
    /// which exceed it — so we use this to exercise the datagram plane at a realistic packet size.
    struct SmallPacketEncoder {
        config: ras_media::AudioConfig,
        seq: u64,
    }
    impl ras_media::AudioEncoderBackend for SmallPacketEncoder {
        fn configure(
            &mut self,
            config: &ras_media::AudioConfig,
        ) -> Result<(), ras_media::MediaError> {
            self.config = *config;
            Ok(())
        }
        fn encode(
            &mut self,
            chunk: ras_media::CapturedAudio,
        ) -> Result<Option<ras_media::EncodedAudio>, ras_media::MediaError> {
            let seq = self.seq;
            self.seq += 1;
            Ok(Some(ras_media::EncodedAudio {
                seq,
                captured_at_us: chunk.captured_at_us,
                data: bytes::Bytes::from_static(&[0u8; 200]),
                config: self.config,
            }))
        }
        fn set_bitrate(&mut self, _bitrate_bps: u32) -> Result<(), ras_media::MediaError> {
            Ok(())
        }
        fn config(&self) -> ras_media::AudioConfig {
            self.config
        }
    }

    /// The full audio path — host pump (Inv-15 `audio.listen` gate) → real iroh **QUIC datagrams** →
    /// controller ingest → attached `AudioOutput` — end-to-end over two real iroh endpoints on
    /// loopback, with the synthetic audio doubles. Combines the orchestrator gate (proven on loopback
    /// in `lib.rs`) with the real datagram plane (proven at the transport layer in the iroh crate).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audio_flows_over_real_iroh_when_granted() {
        let host_ep = Arc::new(Endpoint::bind().await.unwrap());
        let ctrl_ep = Arc::new(Endpoint::bind().await.unwrap());
        let host_id = host_ep.id();
        let host_addrs = to_loopback(&host_ep.bound_addrs());

        let accept_ep = host_ep.clone();
        let accept = tokio::spawn(async move {
            accept_ep
                .accept()
                .await
                .unwrap()
                .expect("an inbound session")
        });
        let ctrl_session = ctrl_ep.connect_direct(&host_id, &host_addrs).await.unwrap();
        let host_session = accept.await.unwrap();

        let host_tp = Arc::new(IrohSessionTransport::new(host_ep, host_session));
        let ctrl_tp = Arc::new(IrohSessionTransport::new(ctrl_ep, ctrl_session));

        // Grant screen.view + audio.listen so the host audio pump is allowed to run (Inv 15).
        let caps: ras_policy::CapabilitySet = ["screen.view", ras_policy::AUDIO_LISTEN]
            .into_iter()
            .map(String::from)
            .collect();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(320, 240),
            SyntheticEncoder::new(),
            Arc::new(GrantsCaps(caps)),
        )
        .with_audio(
            Box::new(ras_media::synthetic::SyntheticAudioCapture::new()),
            Box::new(SmallPacketEncoder {
                config: ras_media::AudioConfig {
                    codec: ras_media::AudioCodec::Opus,
                    sample_rate_hz: 48_000,
                    channels: 2,
                    frame_duration_us: 20_000,
                    target_bitrate_bps: 96_000,
                },
                seq: 0,
            }),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(host_id)),
            ctrl_tp,
        );
        let rec = Arc::new(RecordingAudioOut(std::sync::atomic::AtomicU64::new(0)));
        controller.attach_audio_output(rec.clone());

        let (host_events, ctrl_events) = tokio::join!(host.start(), controller.connect());
        host_events.unwrap();
        ctrl_events.unwrap();
        assert_eq!(host.state(), SessionState::Active);

        assert!(
            wait_until(
                || rec.0.load(std::sync::atomic::Ordering::Relaxed) > 0,
                500
            )
            .await,
            "audio packets should reach the controller output over real iroh datagrams when granted"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    /// Regression guard: a transport built **without** a reconnect strategy keeps the trait's
    /// fail-closed default — `reconnect` returns `Internal("reconnect not supported")`, so `ras-core`
    /// terminates on transport loss rather than silently trying (and failing) to resume. This is the
    /// exact behavior that existed before this override was added; the two `with_reconnect_*` builders
    /// are what opt a live deployment into ADR-091 resume.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_is_unsupported_without_a_strategy() {
        use crate::deps::{DialTarget, SessionTransport};
        use ras_protocol::ErrorCode;
        use ras_transport_iroh::EndpointId;

        let host_ep = Arc::new(Endpoint::bind().await.unwrap());
        let ctrl_ep = Arc::new(Endpoint::bind().await.unwrap());
        let host_id = host_ep.id();
        let host_addrs = to_loopback(&host_ep.bound_addrs());

        let accept_ep = host_ep.clone();
        let accept = tokio::spawn(async move {
            accept_ep
                .accept()
                .await
                .unwrap()
                .expect("an inbound session")
        });
        let ctrl_session = ctrl_ep.connect_direct(&host_id, &host_addrs).await.unwrap();
        let _host_session = accept.await.unwrap();

        // No `.with_reconnect_*` → the default fail-closed reconnect.
        let ctrl_tp = IrohSessionTransport::new(ctrl_ep, ctrl_session);
        let placeholder: DialTarget = EndpointAddr::new(EndpointId([0u8; 32]));
        let err = ctrl_tp.reconnect(&placeholder).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);

        host_ep.close().await;
    }

    /// Endpoint-continuity (Inv 3/9): a controller with a reconnect strategy whose target is a
    /// **different** endpoint than the one it originally authenticated must **refuse** the resume with
    /// `IdentityMismatch`, never swap in a connection to a stranger — the transport half of the
    /// sender-constraint the re-presented grant relies on. Here the transport authenticated `host_a`
    /// but is pointed at a fresh `host_b` for the re-dial, so the re-established connection's peer id
    /// won't match and reconnect is refused (and the old session stays taken/closed — fail-closed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_refuses_a_different_peer() {
        use crate::deps::{DialTarget, SessionTransport};
        use ras_protocol::ErrorCode;
        use ras_transport_iroh::EndpointId;

        // host_a: the peer the controller actually authenticated on the first connect.
        let host_a = Arc::new(Endpoint::bind().await.unwrap());
        let ctrl_ep = Arc::new(Endpoint::bind().await.unwrap());
        let host_a_id = host_a.id();
        let host_a_addrs = to_loopback(&host_a.bound_addrs());

        let accept_ep = host_a.clone();
        let accept = tokio::spawn(async move {
            accept_ep
                .accept()
                .await
                .unwrap()
                .expect("an inbound session")
        });
        let ctrl_session = ctrl_ep
            .connect_direct(&host_a_id, &host_a_addrs)
            .await
            .unwrap();
        let _host_session = accept.await.unwrap();

        // host_b: a DIFFERENT endpoint, accepting so the re-dial actually connects to *someone*.
        let host_b = Arc::new(Endpoint::bind().await.unwrap());
        let host_b_id = host_b.id();
        let host_b_addrs = to_loopback(&host_b.bound_addrs());
        assert_ne!(host_a_id, host_b_id);
        let host_b_accept = host_b.clone();
        let _b_accept = tokio::spawn(async move { host_b_accept.accept().await.ok().flatten() });

        // Wire the controller's reconnect to host_b, not the host_a it authenticated.
        let ctrl_tp = IrohSessionTransport::new(ctrl_ep, ctrl_session).with_reconnect_controller(
            EndpointAddr {
                id: host_b_id,
                direct_addrs: host_b_addrs,
                relay_url: None,
            },
        );

        // The re-dial reaches host_b, whose authenticated id ≠ the original host_a → refused.
        let placeholder: DialTarget = EndpointAddr::new(EndpointId([0u8; 32]));
        let err = ctrl_tp.reconnect(&placeholder).await.unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::IdentityMismatch,
            "resuming to a different peer must be refused (sender-constraint)"
        );

        host_a.close().await;
        host_b.close().await;
    }
}
