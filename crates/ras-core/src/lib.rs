//! Casual RAS core: session orchestration and state machines (Phase 1).
//!
//! Ties together identity, grants, policy, control, media, audit, and transport. This crate stays
//! `iroh`-free by depending on `ras-transport-iroh`'s newtypes, not `iroh`. It re-exports the
//! subsystem crates so downstream consumers (and, later, the FFI/SDK layer) have one entry point.
//!
//! Phase 1 is **view-only, no-auth**: the session state machine (§[`transition`]) elides the auth
//! *transition* states but keeps the security-*terminal* states, and the [`GrantValidator`] auth
//! seam is a no-op ([`AllowAllValidator`], behind the `insecure-no-auth` feature). See
//! `docs/design/phase-1-design.md`.

pub use ras_audit as audit;
pub use ras_control as control;
pub use ras_grant as grant;
pub use ras_identity as identity;
pub use ras_media as media;
pub use ras_policy as policy;
pub use ras_protocol as protocol;
pub use ras_transport_iroh as transport;

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

/// Typed lifecycle events emitted to the embedding app (Phase-1 subset of `docs/05`).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum LifecycleEvent {
    /// Session is connecting.
    Connecting,
    /// Session became active; frames may flow.
    SessionReady,
    /// Stream (re)configured (projection of `ras_media::StreamConfig`).
    StreamConfigured(ras_media::StreamConfig),
    /// Connection quality update.
    ConnectionQuality(ConnHealth),
    /// Transport lost; within the reconnect window.
    Suspended,
    /// Transport restored.
    Reconnected,
    /// Session ended (clean or terminal) with a reason code.
    SessionEnded {
        /// Reason.
        code: ErrorCode,
    },
}

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

// ---------------------------------------------------------------------------------------------
// Phase-2 auth seam (no-op in Phase 1). Shaped so filling it in is additive, not breaking.
// ---------------------------------------------------------------------------------------------

/// The consent/authorization hook. No-op in Phase 1. Invoked after transport identity is
/// established (`ControlEstablished`) but before `Active`. Multi-step so it can express interactive
/// local consent.
///
/// Uses an explicit `-> impl Future + Send` (RPITIT) rather than `async fn` to avoid the
/// `async_fn_in_trait` auto-trait-leakage lint on a public trait.
pub trait GrantValidator: Send + Sync {
    /// Called once (or iteratively via `Challenge`) per session before it may become `Active`.
    fn authorize(
        &self,
        ctx: &SessionAuthContext,
    ) -> impl core::future::Future<Output = Result<GrantDecision, CoreError>> + Send;
}

/// Content-free context handed to the validator. Phase 1 carries the transport-authenticated
/// identity plus the (empty in Phase 1) opaque access-request bytes. `#[non_exhaustive]` — Phase 2
/// adds capabilities/nonce additively.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionAuthContext {
    /// The identity iroh authenticated. Not authorization.
    pub peer_identity: PeerIdentity,
    /// Opaque access-request payload from `ControlMsg::AuthEnvelope`. Empty in Phase 1.
    pub access_request: bytes::Bytes,
}

/// The validator's verdict.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GrantDecision {
    /// Proceed → orchestrator emits `SessionEvent::Authorized`.
    Authorized,
    /// Interactive consent pending (Phase 2): hold in `ControlEstablished` until re-driven.
    NeedConsent,
    /// Multi-step challenge/response (Phase 2 replay/nonce).
    Challenge(bytes::Bytes),
    /// Refused → `SessionEvent::Reject { code }`.
    Denied(ErrorCode),
}

/// PHASE-1 ONLY. Returns `Authorized` unconditionally. Gated behind `insecure-no-auth` so it can
/// never link into an auth build.
#[cfg(feature = "insecure-no-auth")]
pub struct AllowAllValidator;

#[cfg(feature = "insecure-no-auth")]
impl GrantValidator for AllowAllValidator {
    // Implemented as a plain `async fn` (allowed in a trait impl; satisfies the trait's
    // `-> impl Future + Send` declaration without tripping `manual_async_fn`).
    async fn authorize(&self, _ctx: &SessionAuthContext) -> Result<GrantDecision, CoreError> {
        Ok(GrantDecision::Authorized)
    }
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
