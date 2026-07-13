//! Casual RAS core: session orchestration and state machines (Phase 1).
//!
//! Ties together identity, grants, policy, control, media, audit, and transport. This crate stays
//! `iroh`-free by depending on `ras-transport-iroh`'s newtypes, not `iroh`. It re-exports the
//! subsystem crates so downstream consumers (and, later, the FFI/SDK layer) have one entry point.
//!
//! Phase 1 is **view-only, no-auth**: the session state machine (§[`transition`]) elides the auth
//! *transition* states but keeps the security-*terminal* states, and the
//! [`GrantValidator`](deps::GrantValidator) auth seam is a no-op
//! ([`AllowAllValidator`](deps::AllowAllValidator), behind the `insecure-no-auth` feature). The DI
//! seams (§5.4), the typed lifecycle events (§5.6), and the host/controller orchestrators
//! (§5.2/§5.3) live in the [`deps`], [`event`], and [`session`] modules. See
//! `docs/design/phase-1-design.md`.
//!
//! The orchestrators bring in `tokio` (task-owning media/control loops) and `async-trait` (the
//! object-safe DI seams) — both design-sanctioned (§5.4 spells them out) and permissively licensed.

pub mod abr;
pub mod deps;
pub mod event;
pub mod frame_channel;
pub mod session;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use ras_audit as audit;
pub use ras_control as control;
pub use ras_grant as grant;
pub use ras_identity as identity;
pub use ras_media as media;
pub use ras_policy as policy;
pub use ras_protocol as protocol;
pub use ras_transport_iroh as transport;

// Ergonomic re-exports so downstream code can reach the Phase-1 surface from the crate root.
pub use abr::LatencyFirstAbr;
#[cfg(feature = "insecure-no-auth")]
pub use deps::AllowAllValidator;
pub use deps::{
    ControlChannelDyn, FrameSink, GrantDecision, GrantValidator, PushResult, SessionAuthContext,
    SessionTransport, VideoSinkDyn, VideoSourceDyn,
};
pub use event::{
    LifecycleEvent, LifecycleStream, QualitySample, SessionId, StopReason, StreamDescriptor,
};
pub use frame_channel::{
    encode_frame_blob, parse_header, FrameHeader, FRAME_HEADER_LEN, FRAME_MAGIC,
};
pub use session::{ControllerSession, ControllerSessionConfig, HostSession, HostSessionConfig};

use ras_protocol::{DecoderFeedback, ErrorCode, KeyframeReason};
use ras_transport_iroh::ConnHealth;

/// This crate's error alias over the shared taxonomy.
pub type CoreError = ras_protocol::RasError;

/// A peer's authenticated identity (alias, not a new type).
pub type PeerIdentity = ras_transport_iroh::EndpointId;
/// A dial target (alias, not a new type).
pub type DialTarget = ras_transport_iroh::EndpointAddr;

/// The runtime version (from `Cargo.toml`).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------------------------
// Session state machine (subset of HLD §10; auth-transition states elided, security-terminal kept)
// ---------------------------------------------------------------------------------------------

/// Phase-1 session lifecycle. Auth transition states are elided; security-terminal states are
/// retained so Phase 2 is additive (no variant is renamed/removed later).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionState {
    /// Freshly created, nothing dialed.
    Created,
    /// Dialing/accepting the session ALPN; QUIC + channel setup in flight. No media.
    SessionConnecting,
    /// Control channel handshaked; the window where Phase-2 `authorize()` runs. No frames yet.
    ControlEstablished,
    /// Channels open, stream configured, frames may flow. Reachable only via the `Authorized`
    /// gate (in Phase 1 the no-op validator emits it immediately).
    Active,
    /// Transport temporarily lost within the reconnect window. Video frozen; controller UI live.
    Suspended,
    /// Terminal: clean end (local stop / peer close / window elapsed).
    Terminated,
    /// Terminal: emergency-stop / mid-session revoke (Invariant 4). Audit-distinct.
    Revoked,
    /// Terminal: authorization refused (Phase 2).
    Rejected,
    /// Terminal: grant/session expiry (Phase 2).
    Expired,
}

impl SessionState {
    /// Whether this is a terminal state (no outgoing transitions).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            SessionState::Terminated
                | SessionState::Revoked
                | SessionState::Rejected
                | SessionState::Expired
        )
    }
}

/// Internal transition inputs (Copy, content-free — runs on the hot control task, no heap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionEvent {
    /// Begin dialing/accepting.
    Start,
    /// Session ALPN connection + control handshake done.
    ControlUp,
    /// Auth gate passed — the only edge toward `Active`. In Phase 1 the no-op validator emits this
    /// immediately after `ControlUp`.
    Authorized,
    /// Codec/monitor/feature negotiation done.
    StreamConfigured,
    /// Transport lost (enters the reconnect window).
    TransportLost,
    /// Transport restored within the window.
    TransportRestored,
    /// Local user stopped the session.
    LocalStop,
    /// Peer closed the session cleanly.
    PeerClosed,
    /// Emergency stop / host or peer revoke (Invariant 4).
    Revoke {
        /// Reason.
        code: ErrorCode,
    },
    /// Authorization refused (Phase 2).
    Reject {
        /// Reason.
        code: ErrorCode,
    },
    /// Grant/session expired (Phase 2).
    Expire {
        /// Reason.
        code: ErrorCode,
    },
    /// Unrecoverable failure.
    Fatal {
        /// Reason.
        code: ErrorCode,
    },
    /// The reconnect window elapsed without restore.
    ReconnectWindowExpired,
}

/// The result of applying an event to a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// A valid transition to the given state.
    To(SessionState),
    /// The event is not valid in this state (ignored / logged).
    Invalid,
}

/// Pure, synchronous, side-effect-free transition function. Deterministic and unit-testable.
/// Orchestrators call this and *then* perform effects — never the reverse. Never blocks on media:
/// `TransportLost` moves `Active → Suspended` immediately so the controller keeps its cursor/
/// controls live.
#[must_use]
pub fn transition(state: SessionState, event: SessionEvent) -> Transition {
    use SessionEvent as E;
    use SessionState as S;
    let next = match (state, event) {
        (S::Created, E::Start) => S::SessionConnecting,
        (S::SessionConnecting, E::ControlUp) => S::ControlEstablished,
        // Active is reachable only through the Authorized gate: the orchestrator emits
        // StreamConfigured *after* authorize() yields Authorized (Phase 1: immediately).
        (S::ControlEstablished, E::Authorized) => S::ControlEstablished,
        (S::ControlEstablished, E::StreamConfigured) => S::Active,
        (S::Active, E::TransportLost) => S::Suspended,
        (S::Suspended, E::TransportRestored) => S::Active,
        (S::Suspended, E::ReconnectWindowExpired) => S::Terminated,
        // Security-terminal edges from any non-terminal state:
        (s, E::Revoke { .. }) if !s.is_terminal() => S::Revoked,
        (s, E::Reject { .. }) if !s.is_terminal() => S::Rejected,
        (s, E::Expire { .. }) if !s.is_terminal() => S::Expired,
        (s, E::LocalStop | E::PeerClosed | E::Fatal { .. }) if !s.is_terminal() => S::Terminated,
        _ => return Transition::Invalid,
    };
    Transition::To(next)
}

// The rich lifecycle event model lives in `event` (§5.6); the DI seams and the auth seam live in
// `deps` (§5.4/§5.5); the orchestrators live in `session` (§5.2/§5.3).

// ---------------------------------------------------------------------------------------------
// Adaptive bitrate (homed here — next to session state + the ConnHealth feed; Q-ABR-HOME)
// ---------------------------------------------------------------------------------------------

/// ABR hook, driven each feedback/stats tick. Pure function of inputs → intents; latency-first
/// (caps bitrate to bandwidth, reacts every RTT keyframe-free, reserves IDR for genuine resync).
pub trait AdaptiveBitrateController: Send {
    /// Compute the next bitrate decision from the latest health + optional decoder feedback.
    fn on_tick(
        &mut self,
        health: &ConnHealth,
        feedback: Option<DecoderFeedback>,
    ) -> BitrateDecision;
}

/// The ABR output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateDecision {
    /// New CBR target (bits/sec), capped to the estimated deliverable rate.
    pub target_bitrate_bps: u32,
    /// Last-resort resync (FEC in transport is the preferred loss response, since IDR spikes
    /// bitrate).
    pub force_keyframe: Option<KeyframeReason>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsystems_are_wired() {
        assert_eq!(protocol::PROTOCOL_VERSION, 1);
        assert!(!version().is_empty());
    }

    #[test]
    fn happy_path_reaches_active_only_via_stream_configured() {
        use SessionEvent as E;
        use SessionState as S;
        let mut s = S::Created;
        for (ev, want) in [
            (E::Start, S::SessionConnecting),
            (E::ControlUp, S::ControlEstablished),
            (E::Authorized, S::ControlEstablished),
            (E::StreamConfigured, S::Active),
            (E::TransportLost, S::Suspended),
            (E::TransportRestored, S::Active),
        ] {
            match transition(s, ev) {
                Transition::To(next) => {
                    assert_eq!(next, want, "from {s:?} on {ev:?}");
                    s = next;
                }
                Transition::Invalid => panic!("unexpected invalid: {s:?} on {ev:?}"),
            }
        }
    }

    #[test]
    fn revoke_is_terminal_and_distinct() {
        let t = transition(
            SessionState::Active,
            SessionEvent::Revoke {
                code: ErrorCode::SessionRevoked,
            },
        );
        assert_eq!(t, Transition::To(SessionState::Revoked));
        assert!(SessionState::Revoked.is_terminal());
        // No transitions out of a terminal state.
        assert_eq!(
            transition(SessionState::Revoked, SessionEvent::Start),
            Transition::Invalid
        );
    }

    #[test]
    fn invalid_events_are_rejected() {
        assert_eq!(
            transition(SessionState::Created, SessionEvent::StreamConfigured),
            Transition::Invalid
        );
        assert_eq!(
            transition(SessionState::Active, SessionEvent::ControlUp),
            Transition::Invalid
        );
    }
}

/// End-to-end spine test: a host (synthetic capture+encode) and a controller wired through the
/// in-memory loopback transport. Proves the state machine, control handshake, droppable video path,
/// and keyframe-request plumbing all work together with no iroh / no OS / no GPU.
#[cfg(test)]
mod e2e {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::Arc;
    use std::time::Duration;

    use crate::deps::AllowAllValidator;
    use crate::testkit::{loopback_pair, CountingFrameSink};
    use crate::{
        ControllerSession, ControllerSessionConfig, HostSession, HostSessionConfig, LifecycleEvent,
        SessionState, StopReason,
    };
    use ras_media::synthetic::{SyntheticCaptureBackend, SyntheticEncoder};
    use ras_media::MonitorId;
    use ras_protocol::KeyframeReason;
    use ras_transport_iroh::{EndpointAddr, EndpointId};

    /// Poll `cond` up to `tries` × 10 ms; returns whether it became true (never wall-clocks the CI).
    async fn wait_until<F: Fn() -> bool>(cond: F, tries: u32) -> bool {
        for _ in 0..tries {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cond()
    }

    #[tokio::test]
    async fn loopback_streams_frames_and_honors_keyframe_requests() {
        let (host_tp, ctrl_tp) = loopback_pair();

        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(1280, 720),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let target = EndpointAddr {
            id: EndpointId([0u8; 32]),
        };
        let controller = ControllerSession::new(ControllerSessionConfig::new(target), ctrl_tp);

        // Host accepts and starts pushing; controller dials and negotiates the stream.
        let mut host_events = host.start().await.unwrap();
        let _ctrl_events = controller.connect().await.unwrap();

        // Both reach Active purely via the state machine (Authorized gate included).
        assert_eq!(host.state(), SessionState::Active);
        assert_eq!(controller.state(), SessionState::Active);

        // Attach a renderer; frames should start landing, beginning with a keyframe.
        let sink = CountingFrameSink::new();
        controller
            .attach_renderer(Arc::new(sink.clone()))
            .await
            .unwrap();

        assert!(
            wait_until(|| sink.pushed() >= 5, 300).await,
            "expected frames to flow, got {}",
            sink.pushed()
        );
        assert!(
            sink.is_configured(),
            "renderer must be configured before frames"
        );
        assert!(sink.keyframes() >= 1, "the stream must start on a keyframe");

        // A controller-initiated keyframe request must produce another IDR host-side.
        let kf_before = sink.keyframes();
        controller
            .request_keyframe(KeyframeReason::UnrecoverableLoss)
            .await
            .unwrap();
        assert!(
            wait_until(|| sink.keyframes() > kf_before, 300).await,
            "keyframe request did not yield a new IDR (before={kf_before}, after={})",
            sink.keyframes()
        );

        // The stats/ABR tick must raise the bitrate toward the ceiling (loopback health is a clean
        // 50 Mbps / 0 loss path) and emit a content-free ConnectionQuality event.
        assert!(
            wait_until(|| host.current_bitrate_bps() > 6_000_000, 400).await,
            "ABR should raise the bitrate toward the ceiling, got {}",
            host.current_bitrate_bps()
        );
        let mut saw_quality = false;
        while let Ok(ev) = host_events.try_recv() {
            if matches!(ev, LifecycleEvent::ConnectionQuality { .. }) {
                saw_quality = true;
            }
        }
        assert!(saw_quality, "host should emit ConnectionQuality events");

        // Clean teardown → both terminal.
        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
        assert_eq!(controller.state(), SessionState::Terminated);
        assert_eq!(host.state(), SessionState::Terminated);
    }
}
