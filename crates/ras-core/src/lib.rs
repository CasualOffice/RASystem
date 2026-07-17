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
pub mod iroh_transport;
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
    AudioOutput, AudioSink, AudioSourceDyn, AuditSink, ControlChannelDyn, ControlConsent,
    CursorFrame, CursorObserver, CursorShape, CursorSink, FrameSink, GrantDecision,
    GrantSessionValidator, GrantValidator, PushResult, SessionAuthContext, SessionTransport,
    VideoSinkDyn, VideoSourceDyn,
};
pub use event::{
    LifecycleEvent, LifecycleStream, QualitySample, SessionId, StopReason, StreamDescriptor,
};
pub use frame_channel::{
    encode_frame_blob, parse_header, FrameHeader, FRAME_HEADER_LEN, FRAME_MAGIC,
};
pub use iroh_transport::IrohSessionTransport;
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

    const ALL_STATES: [SessionState; 9] = [
        SessionState::Created,
        SessionState::SessionConnecting,
        SessionState::ControlEstablished,
        SessionState::Active,
        SessionState::Suspended,
        SessionState::Terminated,
        SessionState::Revoked,
        SessionState::Rejected,
        SessionState::Expired,
    ];

    fn all_events() -> [SessionEvent; 13] {
        use SessionEvent as E;
        [
            E::Start,
            E::ControlUp,
            E::Authorized,
            E::StreamConfigured,
            E::TransportLost,
            E::TransportRestored,
            E::LocalStop,
            E::PeerClosed,
            E::Revoke {
                code: ErrorCode::SessionRevoked,
            },
            E::Reject {
                code: ErrorCode::ConsentDenied,
            },
            E::Expire {
                code: ErrorCode::RequestExpired,
            },
            E::Fatal {
                code: ErrorCode::Internal,
            },
            E::ReconnectWindowExpired,
        ]
    }

    /// Invariant 4: emergency stop / revoke overrides *everything* — from every non-terminal state
    /// a `Revoke` lands in the audit-distinct `Revoked` terminal.
    #[test]
    fn emergency_stop_overrides_every_non_terminal_state() {
        for s in ALL_STATES {
            if !s.is_terminal() {
                assert_eq!(
                    transition(
                        s,
                        SessionEvent::Revoke {
                            code: ErrorCode::SessionRevoked
                        }
                    ),
                    Transition::To(SessionState::Revoked),
                    "revoke must win from {s:?}"
                );
            }
        }
    }

    /// A terminal state has no outgoing edges — no event can resurrect or re-terminalize it.
    #[test]
    fn terminal_states_reject_every_event() {
        for s in ALL_STATES {
            if s.is_terminal() {
                for e in all_events() {
                    assert_eq!(
                        transition(s, e),
                        Transition::Invalid,
                        "terminal {s:?} must reject {e:?}"
                    );
                }
            }
        }
    }
}

/// Generative coverage of the state machine over the *whole* state×event space (the exhaustive
/// tests above enumerate states; these fuzz the pairing and the code-carrying events).
#[cfg(test)]
mod state_proptests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use proptest::prelude::*;

    fn arb_state() -> impl Strategy<Value = SessionState> {
        prop_oneof![
            Just(SessionState::Created),
            Just(SessionState::SessionConnecting),
            Just(SessionState::ControlEstablished),
            Just(SessionState::Active),
            Just(SessionState::Suspended),
            Just(SessionState::Terminated),
            Just(SessionState::Revoked),
            Just(SessionState::Rejected),
            Just(SessionState::Expired),
        ]
    }

    fn arb_code() -> impl Strategy<Value = ErrorCode> {
        prop_oneof![
            Just(ErrorCode::SessionRevoked),
            Just(ErrorCode::ConsentDenied),
            Just(ErrorCode::RequestExpired),
            Just(ErrorCode::Internal),
            Just(ErrorCode::TransportError),
        ]
    }

    fn arb_event() -> impl Strategy<Value = SessionEvent> {
        prop_oneof![
            Just(SessionEvent::Start),
            Just(SessionEvent::ControlUp),
            Just(SessionEvent::Authorized),
            Just(SessionEvent::StreamConfigured),
            Just(SessionEvent::TransportLost),
            Just(SessionEvent::TransportRestored),
            Just(SessionEvent::LocalStop),
            Just(SessionEvent::PeerClosed),
            Just(SessionEvent::ReconnectWindowExpired),
            arb_code().prop_map(|code| SessionEvent::Revoke { code }),
            arb_code().prop_map(|code| SessionEvent::Reject { code }),
            arb_code().prop_map(|code| SessionEvent::Expire { code }),
            arb_code().prop_map(|code| SessionEvent::Fatal { code }),
        ]
    }

    proptest! {
        /// The transition function is total — it never panics for any (state, event).
        #[test]
        fn transition_is_total(s in arb_state(), e in arb_event()) {
            let _ = transition(s, e);
        }

        /// Terminal states are absorbing: no event produces an outgoing transition.
        #[test]
        fn terminal_is_absorbing(s in arb_state(), e in arb_event()) {
            if s.is_terminal() {
                prop_assert_eq!(transition(s, e), Transition::Invalid);
            }
        }

        /// A resulting state is never a fresh `Created` — the machine only moves forward.
        #[test]
        fn never_transitions_back_to_created(s in arb_state(), e in arb_event()) {
            if let Transition::To(next) = transition(s, e) {
                prop_assert_ne!(next, SessionState::Created);
            }
        }

        /// Invariant 4: from any non-terminal state, a revoke always wins (→ Revoked).
        #[test]
        fn revoke_always_wins_from_non_terminal(s in arb_state(), code in arb_code()) {
            if !s.is_terminal() {
                prop_assert_eq!(
                    transition(s, SessionEvent::Revoke { code }),
                    Transition::To(SessionState::Revoked)
                );
            }
        }
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
    use crate::testkit::{loopback_pair, loopback_pair_with_faults, CountingFrameSink};
    use crate::{
        ControllerSession, ControllerSessionConfig, HostSession, HostSessionConfig, LifecycleEvent,
        LifecycleStream, SessionState, StopReason,
    };
    use ras_media::synthetic::{
        SyntheticAudioCapture, SyntheticAudioEncoder, SyntheticCaptureBackend, SyntheticEncoder,
    };
    use ras_media::MonitorId;
    use ras_protocol::KeyframeReason;
    use ras_transport_iroh::{DropReason, EndpointAddr, EndpointId, VideoEvent};

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
        let target = EndpointAddr::new(EndpointId([0u8; 32]));
        let controller = ControllerSession::new(ControllerSessionConfig::new(target), ctrl_tp);

        // Host accepts and starts pushing; controller dials and negotiates the stream.
        // Phase 2: the host now reads the controller's AuthEnvelope mid-handshake, so both sides must
        // make progress together (as over real iroh) — the pre-wired loopback no longer lets one run
        // to completion first.
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let mut host_events = host_r.unwrap();
        let _ctrl_events = ctrl_r.unwrap();

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

        // The controller must report decoder feedback back to the host (feeds its ABR).
        assert!(
            wait_until(|| host.feedback_received() > 0, 300).await,
            "host should receive decoder feedback from the controller"
        );

        // Clean teardown → both terminal.
        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
        assert_eq!(controller.state(), SessionState::Terminated);
        assert_eq!(host.state(), SessionState::Terminated);
    }

    #[tokio::test]
    async fn controller_pointer_reaches_host_as_a_lifecycle_event() {
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(640, 480),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );

        // Phase 2: the host now reads the controller's AuthEnvelope mid-handshake, so both sides must
        // make progress together (as over real iroh) — the pre-wired loopback no longer lets one run
        // to completion first.
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let mut host_events = host_r.unwrap();
        let _ctrl_events = ctrl_r.unwrap();
        assert_eq!(host.state(), SessionState::Active);

        // The controller points at ~3/4 across, 1/4 down — a "look here" gesture. Best-effort send.
        controller.send_pointer(49151, 16384, true);

        // The host must surface it as a RemotePointer lifecycle event (for its overlay). Poll the
        // stream briefly; the event rides the reliable control channel.
        let mut seen = None;
        for _ in 0..200 {
            while let Ok(ev) = host_events.try_recv() {
                if let LifecycleEvent::RemotePointer { x, y, visible } = ev {
                    seen = Some((x, y, visible));
                }
            }
            if seen.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            seen,
            Some((49151, 16384, true)),
            "host should receive the controller's pointer position"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    /// End-to-end Phase-3 input: a controller requests the lease, the host consents + issues it, an
    /// authorized pointer move reaches the OS sink, an un-granted key is rejected at the gate (Inv 15),
    /// and an emergency stop flushes held keys + stales further input (Inv 4).
    #[tokio::test]
    async fn controller_input_flows_through_the_lease_gate_to_the_sink() {
        use crate::deps::{ControlConsent, GrantDecision, GrantValidator, SessionAuthContext};
        use ras_control::OsInputSink;
        use ras_policy::CapabilitySet;
        use ras_protocol::{InputAction, InputEnvelope, PointerButton};
        use std::sync::atomic::{AtomicU32, Ordering as O};

        #[derive(Default)]
        struct RecordingSink {
            calls: std::sync::Mutex<Vec<&'static str>>,
            released: AtomicU32,
        }
        impl RecordingSink {
            fn calls(&self) -> Vec<&'static str> {
                self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
            }
        }
        impl OsInputSink for RecordingSink {
            fn pointer_move(&self, _d: u32, _x: f32, _y: f32) -> Result<(), crate::CoreError> {
                self.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push("move");
                Ok(())
            }
            fn pointer_button(
                &self,
                _d: u32,
                _x: f32,
                _y: f32,
                _b: PointerButton,
                _down: bool,
            ) -> Result<(), crate::CoreError> {
                self.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push("button");
                Ok(())
            }
            fn pointer_wheel(&self, _dx: i16, _dy: i16) -> Result<(), crate::CoreError> {
                self.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push("wheel");
                Ok(())
            }
            fn key(&self, _u: u16, _down: bool, _m: u8) -> Result<(), crate::CoreError> {
                self.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push("key");
                Ok(())
            }
            fn text(&self, _s: &str) -> Result<(), crate::CoreError> {
                self.calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push("text");
                Ok(())
            }
            fn release_all(&self) -> Result<(), crate::CoreError> {
                self.released.fetch_add(1, O::Relaxed);
                Ok(())
            }
            fn input_permitted(&self) -> bool {
                true
            }
        }

        struct AllowControl;
        #[async_trait::async_trait]
        impl ControlConsent for AllowControl {
            async fn consent_to_control(&self, requested: &CapabilitySet) -> CapabilitySet {
                requested.clone() // the local user consents to exactly what was asked
            }
        }

        // Authorize with the full Phase-3 policy so the seeded lease manager's grant ceiling includes
        // control.request + the input caps.
        struct Phase3Validator;
        #[async_trait::async_trait]
        impl GrantValidator for Phase3Validator {
            async fn authorize(
                &self,
                _ctx: &SessionAuthContext,
            ) -> Result<GrantDecision, crate::CoreError> {
                Ok(GrantDecision::Authorized(
                    ras_policy::phase3_default_policy(),
                ))
            }
        }

        let sink = Arc::new(RecordingSink::default());
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(640, 480),
            SyntheticEncoder::new(),
            Arc::new(Phase3Validator),
        )
        .with_input_sink(sink.clone())
        .with_control_consent(Arc::new(AllowControl));
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );

        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let mut host_events = host_r.unwrap();
        let _ctrl_events = ctrl_r.unwrap();
        assert_eq!(host.state(), SessionState::Active);

        // Request a lease for pointer only (not keyboard).
        controller.request_control(vec!["pointer.move".into(), "pointer.click".into()]);
        assert!(
            wait_until(|| controller.current_lease().is_some(), 500).await,
            "controller should receive a ControlGranted lease"
        );
        let (lease_id, generation) = controller.current_lease().unwrap();

        // An authorized pointer move reaches the sink.
        controller.send_input(InputEnvelope {
            lease_id,
            generation,
            seq: 1,
            action: InputAction::PointerMove {
                display_id: 0,
                nx: 100,
                ny: 200,
                layout_version: 0,
            },
        });
        assert!(
            wait_until(|| sink.calls().contains(&"move"), 500).await,
            "an authorized pointer move must reach the OS sink"
        );

        // A key event is OUTSIDE the granted caps → rejected at the gate; never reaches the sink.
        controller.send_input(InputEnvelope {
            lease_id,
            generation,
            seq: 2,
            action: InputAction::KeyEvent {
                hid_usage: 0x04,
                down: true,
                modifiers: 0,
            },
        });
        let mut saw_reject = false;
        for _ in 0..50 {
            while let Ok(ev) = host_events.try_recv() {
                if let LifecycleEvent::InputRejected {
                    code: ras_protocol::ErrorCode::CapabilityDenied,
                } = ev
                {
                    saw_reject = true;
                }
            }
            if saw_reject {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(saw_reject, "an un-granted key must be rejected (Inv 15)");
        assert!(
            !sink.calls().contains(&"key"),
            "a rejected key must never reach the OS sink"
        );

        // Emergency stop flushes held keys (release_all) and stales any further input (Inv 4).
        host.emergency_stop(ras_protocol::ErrorCode::SessionRevoked)
            .await;
        assert!(
            sink.released.load(O::Relaxed) >= 1,
            "emergency stop must flush held keys via release_all"
        );

        // No input reaches the sink after an emergency stop, even under the (now stale) lease.
        let before = sink.calls().len();
        controller.send_input(InputEnvelope {
            lease_id,
            generation,
            seq: 3,
            action: InputAction::PointerMove {
                display_id: 0,
                nx: 1,
                ny: 1,
                layout_version: 0,
            },
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            sink.calls().len(),
            before,
            "no input may reach the OS after an emergency stop (Inv 4)"
        );

        controller.disconnect(StopReason::UserRequested).await;
    }

    // ── Clipboard push (ADR-076) ────────────────────────────────────────────────────────────────
    // A test double for the OS clipboard: it records what was *set*. (Holding the content is exactly
    // what the real OS clipboard does — Inv 8 is about production logs, not this simulated sink.)
    struct RecordingClipboard {
        set: std::sync::Mutex<Vec<String>>,
    }
    impl RecordingClipboard {
        fn new() -> Self {
            Self {
                set: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn seen(&self) -> Vec<String> {
            self.set.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }
    impl ras_control::ClipboardSink for RecordingClipboard {
        fn set_text(&self, text: &str) -> Result<(), crate::CoreError> {
            self.set
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(text.to_string());
            Ok(())
        }
    }

    /// A validator that authorizes a fixed capability set (so a test can grant `clipboard.write`,
    /// which the default policies withhold).
    struct FixedCaps(ras_policy::CapabilitySet);
    #[async_trait::async_trait]
    impl crate::deps::GrantValidator for FixedCaps {
        async fn authorize(
            &self,
            _ctx: &crate::deps::SessionAuthContext,
        ) -> Result<crate::deps::GrantDecision, crate::CoreError> {
            Ok(crate::deps::GrantDecision::Authorized(self.0.clone()))
        }
    }

    async fn drain_for_clipboard_outcome(events: &mut LifecycleStream) -> Option<LifecycleEvent> {
        for _ in 0..50 {
            while let Ok(ev) = events.try_recv() {
                if matches!(
                    ev,
                    LifecycleEvent::ClipboardApplied { .. }
                        | LifecycleEvent::ClipboardRejected { .. }
                ) {
                    return Some(ev);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }

    fn caps(items: &[&str]) -> ras_policy::CapabilitySet {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// With `clipboard.write` granted and a backend wired, a controller push sets the host clipboard
    /// (never pastes) and reports `ClipboardApplied` with the byte length only (Inv 8).
    #[tokio::test]
    async fn clipboard_push_reaches_the_os_sink_when_granted() {
        let clip = Arc::new(RecordingClipboard::new());
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(320, 240),
            SyntheticEncoder::new(),
            Arc::new(FixedCaps(caps(&[
                "screen.view",
                ras_policy::CLIPBOARD_WRITE,
            ]))),
        )
        .with_clipboard_sink(clip.clone());
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let mut host_events = host_r.unwrap();
        let _c = ctrl_r.unwrap();
        assert_eq!(host.state(), SessionState::Active);

        let payload = "clipboard from the controller 📋";
        controller.send_clipboard_text(payload.to_string());

        match drain_for_clipboard_outcome(&mut host_events).await {
            Some(LifecycleEvent::ClipboardApplied { len }) => {
                assert_eq!(len, payload.len(), "event carries byte length, not content");
            }
            other => panic!("expected ClipboardApplied, got {other:?}"),
        }
        assert_eq!(
            clip.seen(),
            vec![payload.to_string()],
            "the granted push must reach the OS clipboard sink exactly once"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    /// Without `clipboard.write` in the grant, the same push is refused host-side (Inv 15) and never
    /// touches the sink — even though a backend is wired.
    #[tokio::test]
    async fn clipboard_push_refused_without_the_write_capability() {
        let clip = Arc::new(RecordingClipboard::new());
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(320, 240),
            SyntheticEncoder::new(),
            // view-only grant: clipboard.write withheld.
            Arc::new(FixedCaps(caps(&["screen.view"]))),
        )
        .with_clipboard_sink(clip.clone());
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let mut host_events = host_r.unwrap();
        let _c = ctrl_r.unwrap();

        controller.send_clipboard_text("should be blocked".to_string());

        match drain_for_clipboard_outcome(&mut host_events).await {
            Some(LifecycleEvent::ClipboardRejected { code }) => {
                assert_eq!(code, ras_protocol::ErrorCode::CapabilityDenied);
            }
            other => panic!("expected ClipboardRejected, got {other:?}"),
        }
        assert!(
            clip.seen().is_empty(),
            "an un-granted clipboard push must never reach the OS sink (Inv 15)"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    /// The reverse direction (ADR-076, `clipboard.read`): the host pushes its clipboard to the
    /// controller, which **sets** its OS clipboard (never pastes). Granted → the controller's sink
    /// receives it; withheld → nothing crosses the wire (Inv 15).
    #[tokio::test]
    async fn clipboard_read_direction_host_to_controller() {
        async fn run(caps_set: &[&str]) -> (Arc<RecordingClipboard>, Option<LifecycleEvent>) {
            let clip = Arc::new(RecordingClipboard::new());
            let (host_tp, ctrl_tp) = loopback_pair();
            let host = HostSession::new(
                HostSessionConfig::new(MonitorId(0)),
                host_tp,
                SyntheticCaptureBackend::new(320, 240),
                SyntheticEncoder::new(),
                Arc::new(FixedCaps(caps(caps_set))),
            );
            let controller = ControllerSession::new(
                ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
                ctrl_tp,
            );
            controller.attach_clipboard_sink(clip.clone());
            let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
            host_r.unwrap();
            let mut ctrl_events = ctrl_r.unwrap();

            host.send_clipboard_text("clipboard from the host 📋".to_string());
            // Give the outbound message time to cross the loopback (or be dropped by the gate).
            let ev = tokio::time::timeout(
                Duration::from_millis(300),
                drain_for_clipboard_outcome(&mut ctrl_events),
            )
            .await
            .ok()
            .flatten();
            controller.disconnect(StopReason::UserRequested).await;
            host.stop(StopReason::UserRequested).await;
            (clip, ev)
        }

        // Granted: the controller's OS clipboard sink receives the host's text; a ClipboardApplied event.
        let (clip, ev) = run(&["screen.view", ras_policy::CLIPBOARD_READ]).await;
        assert_eq!(clip.seen(), vec!["clipboard from the host 📋".to_string()]);
        assert!(
            matches!(ev, Some(LifecycleEvent::ClipboardApplied { .. })),
            "granted read direction should apply + report, got {ev:?}"
        );

        // Withheld: the host gate drops the send — nothing reaches the controller's sink (Inv 15).
        let (clip, _ev) = run(&["screen.view"]).await;
        assert!(
            clip.seen().is_empty(),
            "without clipboard.read the host must not send its clipboard (Inv 15)"
        );
    }

    // ── Audio plane, host→controller (ADR-077) ──────────────────────────────────────────────────
    /// Controller-side [`AudioOutput`] that tallies packets delivered through the transport — proves a
    /// true end-to-end host→controller flow, not just a host-local send.
    struct RecordingAudio {
        count: std::sync::atomic::AtomicU64,
    }
    impl crate::deps::AudioOutput for RecordingAudio {
        fn push(&self, _packet: ras_media::EncodedAudio) {
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn audio_host(
        caps_set: &[&str],
    ) -> (
        Arc<RecordingAudio>,
        HostSession<SyntheticCaptureBackend, SyntheticEncoder>,
        ControllerSession,
    ) {
        let rec = Arc::new(RecordingAudio {
            count: std::sync::atomic::AtomicU64::new(0),
        });
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(320, 240),
            SyntheticEncoder::new(),
            Arc::new(FixedCaps(caps(caps_set))),
        )
        .with_audio(
            Box::new(SyntheticAudioCapture::new()),
            Box::new(SyntheticAudioEncoder::new()),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        controller.attach_audio_output(rec.clone());
        (rec, host, controller)
    }

    /// With `audio.listen` granted, the host audio pump captures→encodes→sends through the transport
    /// audio plane and the controller's output receives the packets (end-to-end).
    #[tokio::test]
    async fn audio_streams_when_audio_listen_is_granted() {
        let (rec, host, controller) = audio_host(&["screen.view", ras_policy::AUDIO_LISTEN]);
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        host_r.unwrap();
        ctrl_r.unwrap();
        assert_eq!(host.state(), SessionState::Active);

        assert!(
            wait_until(
                || rec.count.load(std::sync::atomic::Ordering::Relaxed) > 0,
                500
            )
            .await,
            "audio packets should reach the controller output when audio.listen is granted"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    /// The output-audio stream's start + stop are audited (Inv 10, ADR-088) — only when it actually runs
    /// (`audio.listen` granted). The recorded chain verifies.
    #[tokio::test]
    async fn audio_start_stop_is_audited() {
        let audit = Arc::new(RecordingAudit::new([0x33; 16]));
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(320, 240),
            SyntheticEncoder::new(),
            Arc::new(FixedCaps(caps(&["screen.view", ras_policy::AUDIO_LISTEN]))),
        )
        .with_audio(
            Box::new(SyntheticAudioCapture::new()),
            Box::new(SyntheticAudioEncoder::new()),
        )
        .with_audit_sink(audit.clone());
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        host_r.unwrap();
        ctrl_r.unwrap();

        assert!(
            wait_until(
                || audit
                    .events()
                    .contains(&crate::audit::AuditEvent::AudioStarted),
                200
            )
            .await,
            "AudioStarted must be audited when audio.listen is granted, got {:?}",
            audit.events()
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
        let events = audit.events();
        assert!(
            events.contains(&crate::audit::AuditEvent::AudioStopped),
            "AudioStopped must be audited on teardown, got {events:?}"
        );
        assert!(audit.chain_ok(), "the recorded audit chain must verify");
    }

    /// Without `audio.listen`, the pump never starts (Inv 15) — the controller receives nothing even
    /// though the audio backends and transport plane are both wired.
    #[tokio::test]
    async fn audio_is_silent_when_audio_listen_is_withheld() {
        let (rec, host, controller) = audio_host(&["screen.view"]); // no audio.listen
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        host_r.unwrap();
        ctrl_r.unwrap();

        // Give the pump ample time to (not) run.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            rec.count.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "no audio may be captured/sent without the audio.listen capability (Inv 15)"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    // ── Cursor-shape channel, host→controller (ADR-073) ──────────────────────────────────────────
    /// A cursor observer that replays a fixed script, yielding to the runtime between updates so the
    /// control loop drains each one (deterministic ordering without sleeps).
    struct ScriptedCursor {
        frames: std::collections::VecDeque<crate::CursorFrame>,
    }
    #[async_trait::async_trait]
    impl crate::CursorObserver for ScriptedCursor {
        async fn next(&mut self) -> Option<crate::CursorFrame> {
            tokio::task::yield_now().await;
            self.frames.pop_front()
        }
    }

    /// A cursor sink that records the update sequence it receives (as compact strings, so the
    /// fresh-vs-cached distinction is asserted).
    #[derive(Default)]
    struct RecordingCursor {
        events: std::sync::Mutex<Vec<String>>,
    }
    impl crate::CursorSink for RecordingCursor {
        fn set_shape(&self, shape: crate::CursorShape) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("shape:{}", shape.id));
        }
        fn set_cached(&self, id: u32) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(format!("cached:{id}"));
        }
        fn hide(&self) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("hidden".into());
        }
    }

    fn cursor_shape_frame(id: u32) -> crate::CursorFrame {
        crate::CursorFrame::Shape(crate::CursorShape {
            id,
            hotspot_x: 0,
            hotspot_y: 0,
            width: 2,
            height: 2,
            rgba: bytes::Bytes::from_static(&[0u8; 16]), // 2*2*4
        })
    }

    /// The host cursor observer streams shapes to the controller's cursor sink, and a **repeated** id
    /// is sent as a cache reference (`CursorCached`), not re-transmitted — the host-side dedup (ADR-073).
    #[tokio::test]
    async fn cursor_shapes_stream_to_the_controller_with_dedup() {
        let script = std::collections::VecDeque::from(vec![
            cursor_shape_frame(1),
            cursor_shape_frame(1), // repeat → CursorCached
            cursor_shape_frame(2),
            crate::CursorFrame::Hidden,
        ]);
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(320, 240),
            SyntheticEncoder::new(),
            Arc::new(FixedCaps(caps(&["screen.view"]))),
        )
        .with_cursor_observer(Box::new(ScriptedCursor { frames: script }));
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let rec = Arc::new(RecordingCursor::default());
        controller.attach_cursor_sink(rec.clone());

        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        host_r.unwrap();
        ctrl_r.unwrap();

        assert!(
            wait_until(
                || rec
                    .events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len()
                    >= 4,
                500
            )
            .await,
            "controller should receive all four cursor updates, got {:?}",
            rec.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        );
        assert_eq!(
            *rec.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["shape:1", "cached:1", "shape:2", "hidden"],
            "a repeated shape id must arrive as a cache reference, not a re-sent shape (ADR-073)"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    /// The host emits the HiDPI display descriptor (ADR-081) at capture start, carrying the shared
    /// display's logical + pixel dimensions and scale — what the controller needs to render crisply.
    #[tokio::test]
    async fn host_emits_capture_display_hidpi_metadata() {
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(1280, 720),
            SyntheticEncoder::new(),
            Arc::new(FixedCaps(caps(&["screen.view"]))),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let mut host_events = host_r.unwrap();
        ctrl_r.unwrap();

        // Drain host lifecycle events for the HiDPI descriptor (emitted once at capture start).
        let mut found = None;
        for _ in 0..50 {
            match tokio::time::timeout(Duration::from_millis(50), host_events.recv()).await {
                Ok(Some(LifecycleEvent::CaptureDisplay {
                    logical_width,
                    logical_height,
                    pixel_width,
                    pixel_height,
                    scale_percent,
                    primary,
                    ..
                })) => {
                    found = Some((
                        logical_width,
                        logical_height,
                        pixel_width,
                        pixel_height,
                        scale_percent,
                        primary,
                    ));
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert_eq!(
            found,
            Some((1280, 720, 1280, 720, 100, true)),
            "host should emit CaptureDisplay HiDPI metadata for the shared display"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    /// In-session chat (ADR-082) flows **both** directions: the controller's message surfaces on the
    /// host's lifecycle stream and the host's on the controller's. Content survives; each side receives
    /// only the *other* peer's text.
    #[tokio::test]
    async fn chat_flows_both_directions() {
        async fn wait_for_chat(events: &mut LifecycleStream) -> Option<String> {
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
                    Ok(Some(LifecycleEvent::ChatMessage { text })) => {
                        return Some(text.reveal().to_string())
                    }
                    Ok(Some(_)) => continue,
                    _ => return None,
                }
            }
            None
        }

        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(320, 240),
            SyntheticEncoder::new(),
            Arc::new(FixedCaps(caps(&["screen.view"]))),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let mut host_events = host_r.unwrap();
        let mut ctrl_events = ctrl_r.unwrap();
        assert!(wait_until(|| controller.state() == SessionState::Active, 200).await);

        // Controller → host.
        controller.send_chat("click the button, top-right".to_string());
        assert_eq!(
            wait_for_chat(&mut host_events).await.as_deref(),
            Some("click the button, top-right"),
            "the host should receive the controller's chat"
        );

        // Host → controller.
        host.send_chat("done, thanks!".to_string());
        assert_eq!(
            wait_for_chat(&mut ctrl_events).await.as_deref(),
            Some("done, thanks!"),
            "the controller should receive the host's chat"
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    // ── Audit journal wiring (ADR-088 / Inv 10) ──────────────────────────────────────────────────
    /// An audit sink backed by a real hash-chained journal. Uses a monotonic counter for timestamps
    /// (deterministic), so the recorded chain is reproducible and verifiable.
    struct RecordingAudit {
        journal: std::sync::Mutex<crate::audit::AuditJournal>,
        clock: std::sync::atomic::AtomicU64,
    }
    impl RecordingAudit {
        fn new(session_id: [u8; 16]) -> Self {
            Self {
                journal: std::sync::Mutex::new(crate::audit::AuditJournal::new(session_id)),
                clock: std::sync::atomic::AtomicU64::new(0),
            }
        }
        fn events(&self) -> Vec<crate::audit::AuditEvent> {
            self.journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entries()
                .iter()
                .map(|e| e.event)
                .collect()
        }
        fn chain_ok(&self) -> bool {
            self.journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .verify()
                .is_ok()
        }
    }
    impl crate::AuditSink for RecordingAudit {
        fn record(&self, event: crate::audit::AuditEvent) {
            let t = self
                .clock
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .append(event, t);
        }
    }

    /// The host records content-free security events into a tamper-evident, hash-chained journal
    /// (Inv 10): a `SessionStarted` at authorization, then `EmergencyStop` + `SessionEnded` on revoke,
    /// and the chain verifies.
    #[tokio::test]
    async fn host_records_content_free_audit_events() {
        use crate::audit::AuditEvent;
        use ras_protocol::ErrorCode;
        let audit = Arc::new(RecordingAudit::new([0x11; 16]));
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(320, 240),
            SyntheticEncoder::new(),
            Arc::new(FixedCaps(caps(&["screen.view"]))),
        )
        .with_audit_sink(audit.clone());
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        host_r.unwrap();
        ctrl_r.unwrap();

        // SessionStarted is recorded once the session is authorized + streaming.
        assert!(
            wait_until(|| audit.events().contains(&AuditEvent::SessionStarted), 200).await,
            "SessionStarted must be recorded, got {:?}",
            audit.events()
        );

        // Emergency stop → EmergencyStop + SessionEnded, both losslessly recorded.
        host.emergency_stop(ErrorCode::SessionRevoked).await;
        controller.disconnect(StopReason::UserRequested).await;

        let events = audit.events();
        assert_eq!(events.first(), Some(&AuditEvent::SessionStarted));
        assert!(
            events.contains(&AuditEvent::EmergencyStop {
                code: ErrorCode::SessionRevoked
            }),
            "emergency stop must be audited, got {events:?}"
        );
        assert!(events.contains(&AuditEvent::SessionEnded {
            code: ErrorCode::SessionRevoked
        }));
        assert!(
            audit.chain_ok(),
            "the recorded audit hash-chain must verify (tamper-evident)"
        );
    }

    #[tokio::test]
    async fn controller_suspends_then_terminates_when_transport_drops() {
        let (host_tp, ctrl_tp, faults) = loopback_pair_with_faults();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(640, 480),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let mut cfg = ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32])));
        cfg.reconnect_window = Duration::from_millis(120); // short so the test is fast
        let controller = ControllerSession::new(cfg, ctrl_tp);

        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let _host_events = host_r.unwrap();
        let mut ctrl_events = ctrl_r.unwrap();
        assert!(
            wait_until(|| controller.state() == SessionState::Active, 300).await,
            "controller should reach Active, got {:?}",
            controller.state()
        );

        // Abrupt transport loss: the peer's connection drops with NO clean Bye (QUIC conn death).
        faults.cut();

        // The controller must freeze (Suspended) but keep its UI live, then terminate once the
        // reconnect window elapses (Phase 1 has no re-dial yet).
        assert!(
            wait_until(|| controller.state() == SessionState::Suspended, 300).await
                || controller.state() == SessionState::Terminated,
            "controller should suspend on transport loss, got {:?}",
            controller.state()
        );
        assert!(
            wait_until(|| controller.state() == SessionState::Terminated, 300).await,
            "controller should terminate after the reconnect window, got {:?}",
            controller.state()
        );

        let mut saw_suspended = false;
        let mut saw_timeout_end = false;
        while let Ok(ev) = ctrl_events.try_recv() {
            match ev {
                LifecycleEvent::Suspended { .. } => saw_suspended = true,
                LifecycleEvent::SessionEnded {
                    reason: StopReason::Timeout,
                } => saw_timeout_end = true,
                _ => {}
            }
        }
        assert!(
            saw_suspended,
            "controller must surface Suspended (video frozen, controls live)"
        );
        assert!(
            saw_timeout_end,
            "controller must end with Timeout after the window"
        );
    }

    /// A *graceful* host stop flushes a clean `Bye{NormalClosure}`, so the controller ends promptly
    /// on `PeerClosed → Terminated` — it must NOT mistake an intentional close for transport loss
    /// (no `Suspended`, no waiting out the reconnect window). The complement of the `cut` test above.
    #[tokio::test]
    async fn graceful_host_stop_ends_controller_cleanly() {
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(640, 480),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let mut cfg = ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32])));
        // Deliberately long: if the controller wrongly suspended, it would still be Suspended (not
        // Terminated) for the whole test window, failing the prompt-termination assertion below.
        cfg.reconnect_window = Duration::from_secs(30);
        let controller = ControllerSession::new(cfg, ctrl_tp);

        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let _host_events = host_r.unwrap();
        let mut ctrl_events = ctrl_r.unwrap();
        assert!(wait_until(|| controller.state() == SessionState::Active, 300).await);

        host.stop(StopReason::UserRequested).await;

        assert!(
            wait_until(|| controller.state() == SessionState::Terminated, 300).await,
            "controller should terminate promptly on a clean Bye, got {:?}",
            controller.state()
        );
        let mut saw_suspended = false;
        let mut ended_clean = false;
        while let Ok(ev) = ctrl_events.try_recv() {
            match ev {
                LifecycleEvent::Suspended { .. } => saw_suspended = true,
                LifecycleEvent::SessionEnded {
                    reason: StopReason::PeerClosed,
                } => ended_clean = true,
                _ => {}
            }
        }
        assert!(
            !saw_suspended,
            "a clean host stop must not suspend the controller"
        );
        assert!(
            ended_clean,
            "controller must end with PeerClosed on a clean Bye, not Timeout/Revoked"
        );
    }

    /// Loss handling (docs/10 §4): an *unrecoverable* drop makes the controller request a fresh IDR
    /// (and freeze on the last good frame until it arrives). The synthetic encoder only emits an IDR
    /// on the first frame and on request, so a *new* keyframe at the sink is an unambiguous signal
    /// that recovery fired end-to-end (drop event → keyframe request → host IDR → sink). The
    /// stale-vs-unrecoverable discrimination itself is covered exhaustively by
    /// `session::loss_tests` (host-side keyframe coalescing makes a count-based negative unreliable).
    #[tokio::test]
    async fn unrecoverable_drop_drives_end_to_end_keyframe_recovery() {
        let (host_tp, ctrl_tp, faults) = loopback_pair_with_faults();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(1280, 720),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let _host_events = host_r.unwrap();
        let _ctrl_events = ctrl_r.unwrap();
        let sink = CountingFrameSink::new();
        controller
            .attach_renderer(Arc::new(sink.clone()))
            .await
            .unwrap();

        // Let the stream start on its first IDR and any startup keyframe requests settle.
        assert!(wait_until(|| sink.keyframes() >= 1, 300).await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let kf_baseline = sink.keyframes();

        // A transport-generated unrecoverable gap → the controller must request a fresh IDR.
        faults
            .inject_video(VideoEvent::FrameDropped {
                frame_id: 9_999,
                reason: DropReason::FecUnrecoverable,
            })
            .await;

        assert!(
            wait_until(|| sink.keyframes() > kf_baseline, 300).await,
            "an unrecoverable drop must drive a fresh IDR (baseline={kf_baseline}, now={})",
            sink.keyframes()
        );

        controller.disconnect(StopReason::UserRequested).await;
        host.stop(StopReason::UserRequested).await;
    }

    /// Invariant 4: an emergency stop halts the host locally and immediately, on the audit-distinct
    /// `Revoked` path, well inside the 250 ms budget — and no frame is sent afterward.
    #[tokio::test]
    async fn emergency_stop_halts_host_within_budget_on_revoked_path() {
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(1280, 720),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );

        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let _host_events = host_r.unwrap();
        let _ctrl_events = ctrl_r.unwrap();
        let sink = CountingFrameSink::new();
        controller
            .attach_renderer(Arc::new(sink.clone()))
            .await
            .unwrap();
        assert!(
            wait_until(|| sink.pushed() >= 5, 300).await,
            "expected frames to flow before the stop, got {}",
            sink.pushed()
        );

        // Fire the emergency stop and measure how long it takes to return (local halt budget).
        let t0 = std::time::Instant::now();
        host.emergency_stop(ras_protocol::ErrorCode::SessionRevoked)
            .await;
        let elapsed = t0.elapsed();

        assert_eq!(
            host.state(),
            SessionState::Revoked,
            "emergency stop must drive the audit-distinct Revoked state, not Terminated"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "emergency stop must take effect within 250 ms locally, took {elapsed:?}"
        );

        // No frames escape after the stop: once the flow has settled, the delivered count is frozen.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let a = sink.pushed();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let b = sink.pushed();
        assert_eq!(
            a, b,
            "frames kept flowing after the emergency stop (a={a}, b={b})"
        );
    }

    /// The revoke can never be downgraded: a graceful stop after an emergency stop is a no-op, and
    /// the state stays `Revoked` (first caller wins, terminal is absorbing).
    #[tokio::test]
    async fn emergency_stop_is_idempotent_and_cannot_be_downgraded() {
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(640, 480),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let _h = host_r.unwrap();
        let _c = ctrl_r.unwrap();
        assert!(wait_until(|| host.state() == SessionState::Active, 300).await);

        host.emergency_stop(ras_protocol::ErrorCode::SessionRevoked)
            .await;
        assert_eq!(host.state(), SessionState::Revoked);

        // A late graceful stop and a second emergency stop are both no-ops.
        host.stop(StopReason::UserRequested).await;
        assert_eq!(
            host.state(),
            SessionState::Revoked,
            "graceful stop downgraded a revoke"
        );
        host.emergency_stop(ras_protocol::ErrorCode::Internal).await;
        assert_eq!(host.state(), SessionState::Revoked);
    }

    /// A host emergency stop reaches the controller as a *revoke* (audit-distinct from a clean peer
    /// close), carried by the existing `Bye{SessionRevoked}` wire message.
    #[tokio::test]
    async fn revoke_propagates_to_controller_as_revoked() {
        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(1280, 720),
            SyntheticEncoder::new(),
            Arc::new(AllowAllValidator),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let (host_r, ctrl_r) = tokio::join!(host.start(), controller.connect());
        let _host_events = host_r.unwrap();
        let mut ctrl_events = ctrl_r.unwrap();
        assert!(wait_until(|| controller.state() == SessionState::Active, 300).await);

        host.emergency_stop(ras_protocol::ErrorCode::SessionRevoked)
            .await;

        assert!(
            wait_until(|| controller.state() == SessionState::Revoked, 300).await,
            "controller should end Revoked, got {:?}",
            controller.state()
        );
        let mut saw_revoked_end = false;
        while let Ok(ev) = ctrl_events.try_recv() {
            if let LifecycleEvent::SessionEnded {
                reason: StopReason::Revoked { code },
            } = ev
            {
                assert_eq!(code, ras_protocol::ErrorCode::SessionRevoked);
                saw_revoked_end = true;
            }
        }
        assert!(
            saw_revoked_end,
            "controller must surface a Revoked SessionEnded, not a plain PeerClosed"
        );
    }

    /// Invariants 2 & 9: authorization is a host-enforced gate, and it fails **closed**. A denying
    /// validator must block the session on the `Rejected` terminal *before* any capture/encode starts
    /// (the media pump spawns only after the authorize gate passes) — no frame can ever leave.
    #[tokio::test]
    async fn denying_validator_blocks_the_session_before_capture() {
        use crate::deps::{GrantDecision, GrantValidator, SessionAuthContext};

        struct DenyValidator;
        #[async_trait::async_trait]
        impl GrantValidator for DenyValidator {
            async fn authorize(
                &self,
                _ctx: &SessionAuthContext,
            ) -> Result<GrantDecision, crate::CoreError> {
                Ok(GrantDecision::Denied(
                    ras_protocol::ErrorCode::ConsentDenied,
                ))
            }
        }

        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(1280, 720),
            SyntheticEncoder::new(),
            Arc::new(DenyValidator),
        );
        // A controller must present its AuthEnvelope for the host to reach the authorize gate; it
        // connects concurrently and is torn down when the host denies.
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let ctrl = tokio::spawn(async move { controller.connect().await });

        let err = host.start().await.unwrap_err();
        assert_eq!(err.code, ras_protocol::ErrorCode::ConsentDenied);
        assert_eq!(
            host.state(),
            SessionState::Rejected,
            "a denied session must land on the Rejected terminal, never Active"
        );
        let _ = ctrl.await;
    }

    /// Fail-closed for a decision Phase 1 can't service: an interactive `NeedConsent` must be treated
    /// as a denial (Rejected + error), never silently proceed to `Active` or hang. Guards against a
    /// future `GrantDecision` variant accidentally falling through the authorize gate.
    #[tokio::test]
    async fn unsupported_consent_decision_fails_closed() {
        use crate::deps::{GrantDecision, GrantValidator, SessionAuthContext};

        struct NeedsConsentValidator;
        #[async_trait::async_trait]
        impl GrantValidator for NeedsConsentValidator {
            async fn authorize(
                &self,
                _ctx: &SessionAuthContext,
            ) -> Result<GrantDecision, crate::CoreError> {
                Ok(GrantDecision::NeedConsent)
            }
        }

        let (host_tp, ctrl_tp) = loopback_pair();
        let host = HostSession::new(
            HostSessionConfig::new(MonitorId(0)),
            host_tp,
            SyntheticCaptureBackend::new(640, 480),
            SyntheticEncoder::new(),
            Arc::new(NeedsConsentValidator),
        );
        let controller = ControllerSession::new(
            ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
            ctrl_tp,
        );
        let ctrl = tokio::spawn(async move { controller.connect().await });

        let err = host.start().await.unwrap_err();
        assert_eq!(err.code, ras_protocol::ErrorCode::ConsentDenied);
        assert_ne!(
            host.state(),
            SessionState::Active,
            "an unserviceable consent decision must never reach Active"
        );
        let _ = ctrl.await;
    }

    /// The real session-phase gate (`GrantSessionValidator`) authorizes a valid PASETO grant and
    /// **fails closed** on the two attacks the sender-constraint defends against: a grant replayed
    /// from a different endpoint, and a tampered token. Exercises the `ras-grant` ↔ `ras-core` seam
    /// directly (no transport) so the authorization logic is verified independent of the orchestrator.
    #[tokio::test]
    async fn grant_session_validator_authorizes_valid_grant_and_denies_forgeries() {
        use crate::deps::{
            GrantDecision, GrantSessionValidator, GrantValidator, SessionAuthContext,
        };
        use ras_grant::{AccessRequest, LocalHostGrantIssuer, SessionGrantIssuer, SessionParams};
        use ras_identity::{KeyStore, SoftwareKeyStore};

        let host = SoftwareKeyStore::from_seed([1u8; 32]);
        let host_id = host.public_key();
        let controller = SoftwareKeyStore::generate().unwrap();
        let ctrl_ep = [2u8; 32];
        let now = 1_000_000u64;

        // Controller signs a request bound to this host + its endpoint, then the host issues a grant.
        let req = AccessRequest::signed(
            &controller,
            [1u8; 16],
            ras_protocol::PROTOCOL_VERSION,
            host_id,
            "Tech".to_string(),
            ctrl_ep,
            ["screen.view".to_string()].into_iter().collect(),
            "help".to_string(),
            now,
            now + 60_000,
            [9u8; 16],
        )
        .unwrap();
        let issuer = LocalHostGrantIssuer::new(
            SoftwareKeyStore::from_seed([1u8; 32]),
            ras_policy::phase2_default_policy(),
            1,
        );
        let session = SessionParams {
            session_id: [5u8; 16],
            host_endpoint_id: [3u8; 32],
            session_generation: 1,
            session_nonce: [6u8; 16],
            issued_at: now,
            not_before: now,
            expires_at: now + 60_000,
        };
        let token = issuer
            .issue(
                &req,
                &["screen.view".to_string()].into_iter().collect(),
                &session,
            )
            .await
            .unwrap();

        let validator = GrantSessionValidator;
        let ctx = |ep: [u8; 32], grant: bytes::Bytes| SessionAuthContext {
            peer_identity: EndpointId(ep),
            access_request: grant,
            host_id,
            now: now + 1_000,
        };

        // Valid grant from the bound endpoint → Authorized with exactly the granted caps.
        match validator
            .authorize(&ctx(ctrl_ep, token.clone()))
            .await
            .unwrap()
        {
            GrantDecision::Authorized(caps) => {
                assert_eq!(caps, ["screen.view".to_string()].into_iter().collect());
            }
            other => panic!("expected Authorized, got {other:?}"),
        }

        // Same grant presented from a different endpoint → sender-constraint fails closed.
        assert_eq!(
            validator
                .authorize(&ctx([0xEE; 32], token.clone()))
                .await
                .unwrap(),
            GrantDecision::Denied(ras_protocol::ErrorCode::IdentityMismatch)
        );

        // Tampered token → GrantInvalid.
        let mut bad = token.to_vec();
        bad[14] ^= 0x01;
        assert_eq!(
            validator
                .authorize(&ctx(ctrl_ep, bytes::Bytes::from(bad)))
                .await
                .unwrap(),
            GrantDecision::Denied(ras_protocol::ErrorCode::GrantInvalid)
        );
    }
}
