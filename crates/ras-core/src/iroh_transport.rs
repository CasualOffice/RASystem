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

use crate::deps::{
    ControlChannelDyn, DialTarget, PeerIdentity, SessionTransport, VideoSinkDyn, VideoSourceDyn,
};
use crate::CoreError;
use ras_media::EncodedFrame;
use ras_protocol::{ControlMsg, ErrorCode};
use ras_transport_iroh::{
    ConnHealth, ControlChannel, Endpoint, HealthObserver, SendOutcome, Session, VideoEvent,
    VideoSink, VideoSource,
};

/// A [`SessionTransport`] over one established iroh session. Holds a shared handle to the owning
/// [`Endpoint`] so the connection (and its background tasks) outlive the session — while the same
/// endpoint stays free to accept further controllers (the host serves one viewer at a time but need
/// not rebind between them).
pub struct IrohSessionTransport {
    // Keeps the endpoint alive for the session's lifetime without taking exclusive ownership.
    _endpoint: Arc<Endpoint>,
    session: Session,
    remote: PeerIdentity,
    // One persistent health observer so the windowed-loss baseline survives across ABR ticks (a
    // fresh observer per tick would reset the window and report the lifetime loss average instead).
    health: HealthObserver,
}

impl IrohSessionTransport {
    /// Wrap an already-established session together with a shared handle to its owning endpoint. The
    /// QUIC/TLS handshake has already authenticated the peer; its identity is captured here for
    /// [`establish`].
    #[must_use]
    pub fn new(endpoint: Arc<Endpoint>, session: Session) -> Self {
        let remote = session.remote();
        let health = session.health();
        Self {
            _endpoint: endpoint,
            session,
            remote,
            health,
        }
    }
}

#[async_trait]
impl SessionTransport for IrohSessionTransport {
    async fn establish(&self, _target: &DialTarget) -> Result<PeerIdentity, CoreError> {
        // The connection is already established; return the identity the transport authenticated.
        // Identity, not authority (Invariant 9).
        Ok(self.remote)
    }

    async fn control_channel(&self) -> Result<Box<dyn ControlChannelDyn>, CoreError> {
        Ok(Box::new(IrohControlChannel(self.session.control().await?)))
    }

    async fn video_sink(&self) -> Result<Box<dyn VideoSinkDyn>, CoreError> {
        self.session
            .video_sink()
            .map(|s| Box::new(IrohVideoSink(s)) as Box<dyn VideoSinkDyn>)
            .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "no video sink for this role"))
    }

    async fn video_source(&self) -> Result<Box<dyn VideoSourceDyn>, CoreError> {
        self.session
            .video_source()
            .map(|s| Box::new(IrohVideoSource(s)) as Box<dyn VideoSourceDyn>)
            .ok_or_else(|| CoreError::fatal(ErrorCode::Internal, "no video source for this role"))
    }

    fn health(&self) -> ConnHealth {
        self.health.snapshot()
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
}
