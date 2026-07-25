//! Pure 1:1 call state machine for Casual RAS (ADR-103/104).
//!
//! This crate is the **security spine** of calling and nothing else: a deterministic, side-effect-free
//! state machine plus the small value types the signaling/consent/media layers share. No I/O, no async,
//! no clock (the caller supplies time for ring timeouts), no OS capture. It exists so the *lifecycle*
//! of a call — who is ringing, who accepted, when it is torn down — is decided in one auditable place
//! before any microphone or camera byte is ever captured.
//!
//! Invariants it upholds (`CLAUDE.md` §5):
//! - **Inv 1** — an inbound call sits in [`CallState::Ringing`] until the *local* user answers; the FSM
//!   never self-advances a ring to `Active`. Only a [`CallEvent::LocalAccept`] leaves `Ringing`.
//! - **Inv 4** — [`CallEvent::Hangup`] is valid from *every* non-terminal state, so an emergency stop
//!   can always tear a call down immediately, whatever phase it is in.
//! - **Inv 12 (as expanded by ADR-103)** — calling is two-way (mic + camera) but the media itself is
//!   gated by the [`ras_policy::MIC_CAPTURE`]/[`ras_policy::CAMERA_CAPTURE`] capabilities *outside* this
//!   FSM; reaching `Active` is necessary but **not sufficient** to capture — the per-message host gate
//!   still applies (Inv 15). A call is never recorded.
//! - **Inv 8** — every type here is content-free (call ids, media kind, error codes); nothing carries a
//!   name, pixel, or audio sample.
//!
//! The wire signaling (`CallInvite`/`CallAccept`/… as `ras-signal` payloads), the `ras-core` wiring, and
//! the OS mic/camera backends are separate, additive layers built on top of this one.

#![forbid(unsafe_code)]

use ras_protocol::ErrorCode;

/// The media a call carries. Re-exported from `ras-protocol` as the single canonical media-kind type
/// shared by both call planes (the ring signal and the in-session control messages), so the FSM, the
/// wire, and the consent prompt never drift. The lifecycle FSM does not branch on it.
pub use ras_protocol::CallMediaKind as CallMedia;

/// The lifecycle of a single 1:1 call. Direction is implicit in the entry state: an outbound call we
/// place enters [`CallState::Dialing`]; an inbound invite we receive enters [`CallState::Ringing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CallState {
    /// No call in progress.
    Idle,
    /// Outbound: we placed a call and are waiting for the remote to answer.
    Dialing,
    /// Inbound: an invite arrived; waiting for the **local user** to accept/decline (Inv 1).
    Ringing,
    /// Answered by both sides; the media path is negotiating/connecting. No guarantee of media yet.
    Connecting,
    /// Media is flowing. Reaching here does not by itself authorize capture — the mic/camera
    /// capabilities are gated per-message host-side (Inv 15).
    Active,
    /// Terminal: a normal hang-up (either side) or an emergency stop (Inv 4).
    Ended,
    /// Terminal: declined — the callee (or the local user on an inbound ring) said no.
    Declined,
    /// Terminal: the ring window elapsed with no answer.
    Missed,
    /// Terminal: an unrecoverable error tore the call down.
    Failed,
}

impl CallState {
    /// Whether this is a terminal state (no outgoing transitions).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            CallState::Ended | CallState::Declined | CallState::Missed | CallState::Failed
        )
    }

    /// Whether media is (or may be) flowing in this state — the point at which an Inv-7 "in call"
    /// indicator must be shown. True only for [`CallState::Active`].
    #[must_use]
    pub const fn media_live(self) -> bool {
        matches!(self, CallState::Active)
    }

    /// Whether a call is in progress: neither [`CallState::Idle`] (no call) nor terminal. This is the
    /// set of states a hang-up / emergency stop can end (Inv 4).
    #[must_use]
    pub const fn in_progress(self) -> bool {
        matches!(
            self,
            CallState::Dialing | CallState::Ringing | CallState::Connecting | CallState::Active
        )
    }
}

/// Inputs to the call FSM. Copy + content-free — runs on the signaling task with no heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallEvent {
    /// Local user places an outbound call (`Idle → Dialing`).
    Dial,
    /// An inbound invite arrived (`Idle → Ringing`).
    Ring,
    /// The remote answered our outbound dial (`Dialing → Connecting`).
    RemoteAccepted,
    /// The **local** user accepted an inbound ring (`Ringing → Connecting`). The only edge out of
    /// `Ringing` toward a live call (Inv 1).
    LocalAccept,
    /// The media path is up (`Connecting → Active`).
    MediaConnected,
    /// The local user declined an inbound ring (`Ringing → Declined`).
    LocalDecline,
    /// The remote declined our outbound dial (`Dialing → Declined`).
    RemoteDeclined,
    /// No answer within the ring window (`Dialing`/`Ringing → Missed`). The caller owns the clock.
    RingTimeout,
    /// Either side ended the call, including via emergency stop (Inv 4). Valid from any non-terminal
    /// state → `Ended`.
    Hangup,
    /// An unrecoverable failure tore the call down (any non-terminal state → `Failed`).
    Failed {
        /// Reason (content-free).
        code: ErrorCode,
    },
}

/// The result of applying an event to a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTransition {
    /// A valid transition to the given state.
    To(CallState),
    /// The event is not valid in this state (ignored / logged content-free).
    Invalid,
}

/// Pure, synchronous, side-effect-free transition. Deterministic and unit-testable. Callers apply this
/// and *then* perform effects (ring, capture, tear down) — never the reverse. Fail-closed: any pairing
/// not explicitly allowed is [`CallTransition::Invalid`], so an unexpected event can never advance a
/// call (e.g. a stray `MediaConnected` while still `Ringing` does nothing — consent is not bypassed).
#[must_use]
pub fn transition(state: CallState, event: CallEvent) -> CallTransition {
    use CallEvent as E;
    use CallState as S;
    let next = match (state, event) {
        // Placing / receiving a call.
        (S::Idle, E::Dial) => S::Dialing,
        (S::Idle, E::Ring) => S::Ringing,
        // Answering.
        (S::Dialing, E::RemoteAccepted) => S::Connecting,
        (S::Ringing, E::LocalAccept) => S::Connecting,
        // Media coming up.
        (S::Connecting, E::MediaConnected) => S::Active,
        // Declines.
        (S::Ringing, E::LocalDecline) => S::Declined,
        (S::Dialing, E::RemoteDeclined) => S::Declined,
        // No answer.
        (S::Dialing | S::Ringing, E::RingTimeout) => S::Missed,
        // Emergency stop / hang-up is always available while a call is IN PROGRESS (Inv 4). `Idle` has
        // no call to end, so it is excluded (fail-closed: only a real call can be torn down).
        (s, E::Hangup) if s.in_progress() => S::Ended,
        // Failure from any in-progress state.
        (s, E::Failed { .. }) if s.in_progress() => S::Failed,
        _ => return CallTransition::Invalid,
    };
    CallTransition::To(next)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn step(state: CallState, event: CallEvent) -> CallState {
        match transition(state, event) {
            CallTransition::To(s) => s,
            CallTransition::Invalid => panic!("unexpected invalid: {state:?} on {event:?}"),
        }
    }

    #[test]
    fn outbound_happy_path() {
        let mut s = CallState::Idle;
        for (ev, want) in [
            (CallEvent::Dial, CallState::Dialing),
            (CallEvent::RemoteAccepted, CallState::Connecting),
            (CallEvent::MediaConnected, CallState::Active),
        ] {
            s = step(s, ev);
            assert_eq!(s, want);
        }
        assert!(s.media_live());
        // Hang up from Active is terminal.
        s = step(s, CallEvent::Hangup);
        assert_eq!(s, CallState::Ended);
        assert!(s.is_terminal());
    }

    #[test]
    fn inbound_requires_local_accept_to_leave_ringing() {
        let ringing = step(CallState::Idle, CallEvent::Ring);
        assert_eq!(ringing, CallState::Ringing);
        // Inv 1: the FSM never self-advances a ring — only LocalAccept leaves Ringing toward a call.
        // A stray "media connected" or "remote accepted" must NOT bypass local consent.
        assert_eq!(
            transition(ringing, CallEvent::MediaConnected),
            CallTransition::Invalid
        );
        assert_eq!(
            transition(ringing, CallEvent::RemoteAccepted),
            CallTransition::Invalid
        );
        // Local accept advances; media then comes up.
        let connecting = step(ringing, CallEvent::LocalAccept);
        assert_eq!(connecting, CallState::Connecting);
        assert_eq!(
            step(connecting, CallEvent::MediaConnected),
            CallState::Active
        );
    }

    #[test]
    fn inbound_decline_and_timeout() {
        assert_eq!(
            step(CallState::Ringing, CallEvent::LocalDecline),
            CallState::Declined
        );
        assert_eq!(
            step(CallState::Ringing, CallEvent::RingTimeout),
            CallState::Missed
        );
        assert_eq!(
            step(CallState::Dialing, CallEvent::RemoteDeclined),
            CallState::Declined
        );
        assert_eq!(
            step(CallState::Dialing, CallEvent::RingTimeout),
            CallState::Missed
        );
    }

    #[test]
    fn hangup_is_valid_from_every_non_terminal_state() {
        // Inv 4: emergency stop / hang-up must always be able to end a call, whatever phase it is in.
        for s in [
            CallState::Dialing,
            CallState::Ringing,
            CallState::Connecting,
            CallState::Active,
        ] {
            assert_eq!(
                transition(s, CallEvent::Hangup),
                CallTransition::To(CallState::Ended)
            );
        }
        // But not from Idle (no call) or a terminal state.
        assert_eq!(
            transition(CallState::Idle, CallEvent::Hangup),
            CallTransition::Invalid
        );
        assert_eq!(
            transition(CallState::Ended, CallEvent::Hangup),
            CallTransition::Invalid
        );
    }

    #[test]
    fn failure_from_any_non_terminal_state() {
        let code = ErrorCode::TransportError;
        for s in [
            CallState::Dialing,
            CallState::Ringing,
            CallState::Connecting,
            CallState::Active,
        ] {
            assert_eq!(
                transition(s, CallEvent::Failed { code }),
                CallTransition::To(CallState::Failed)
            );
        }
        assert_eq!(
            transition(CallState::Idle, CallEvent::Failed { code }),
            CallTransition::Invalid
        );
    }

    #[test]
    fn terminal_states_reject_all_events() {
        let code = ErrorCode::Internal;
        for s in [
            CallState::Ended,
            CallState::Declined,
            CallState::Missed,
            CallState::Failed,
        ] {
            for ev in [
                CallEvent::Dial,
                CallEvent::Ring,
                CallEvent::RemoteAccepted,
                CallEvent::LocalAccept,
                CallEvent::MediaConnected,
                CallEvent::LocalDecline,
                CallEvent::RemoteDeclined,
                CallEvent::RingTimeout,
                CallEvent::Hangup,
                CallEvent::Failed { code },
            ] {
                assert_eq!(
                    transition(s, ev),
                    CallTransition::Invalid,
                    "{s:?} on {ev:?}"
                );
            }
            assert!(s.is_terminal());
            assert!(!s.media_live());
        }
    }

    #[test]
    fn media_kind_video_implies_camera() {
        assert!(CallMedia::Video.has_video());
        assert!(!CallMedia::Voice.has_video());
    }

    #[test]
    fn only_active_is_media_live() {
        for s in [
            CallState::Idle,
            CallState::Dialing,
            CallState::Ringing,
            CallState::Connecting,
        ] {
            assert!(
                !s.media_live(),
                "{s:?} must not be media-live before Active"
            );
        }
        assert!(CallState::Active.media_live());
    }
}
