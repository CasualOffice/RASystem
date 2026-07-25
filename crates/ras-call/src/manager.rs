//! Stateful, pure call manager over the [`crate::transition`] FSM (ADR-104, L4).
//!
//! [`CallManager`] is the decision core a call runtime drives: it owns the current [`CallState`], the
//! agreed media kind, and each side's mute state, and turns wire/local events into state changes while
//! enforcing the rules the bare FSM can't (they need memory): **one active call at a time**, **media
//! downgrade** (a video invite may be accepted as voice, never upgraded), and **emergency-stop always
//! ends a call** (Inv 4). It is still pure — no I/O, no async, no clock (the caller owns ring timers) —
//! so the whole thing is unit-testable off-device. The runtime (`ras-core`, L4b) and the app (L6) call
//! these methods and perform the actual effects (send a `ControlMsg`, ring the user, start/stop media).

use crate::{transition, CallEvent, CallMedia, CallState, CallTransition};
use ras_protocol::ErrorCode;

/// A participant's mute state during a call (advisory presentation state — the real security gate is the
/// per-message mic/camera capability check host-side, ADR-103/Inv 15). Default = nothing muted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MuteState {
    /// The participant has muted their microphone (stopped sending mic audio).
    pub audio_muted: bool,
    /// The participant has turned their camera off (stopped sending camera video).
    pub video_muted: bool,
}

/// A call-manager error: an event that isn't valid for the current call state (fail-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallError {
    /// The event is not a legal transition from the current state.
    InvalidTransition,
}

impl CallError {
    /// Stable wire/audit code (content-free).
    #[must_use]
    pub const fn code(self) -> ErrorCode {
        ErrorCode::InvalidMessage
    }
}

/// The manager's response to an inbound [`crate::CallEvent::Ring`] — the point where **one-active-call**
/// is enforced (ADR-104).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteResponse {
    /// The manager was idle: it is now `Ringing`. The runtime must raise the incoming-call surface for
    /// the **local user** to accept/decline (Inv 1) — never auto-answer.
    Ring {
        /// The media kind the caller offered.
        media: CallMedia,
    },
    /// The manager was already in a call: reply `CallBusy` and **do not** ring the user. The manager's
    /// existing call is untouched.
    Busy,
}

/// Narrow an offered media kind to an accepted one: a call may be **downgraded** (video → voice) but
/// never upgraded. The result carries video only if both offer and acceptance want video.
#[must_use]
fn narrow(offered: CallMedia, accepted: CallMedia) -> CallMedia {
    if offered.has_video() && accepted.has_video() {
        CallMedia::Video
    } else {
        CallMedia::Voice
    }
}

/// The stateful call manager. Construct with [`CallManager::new`]; drive with the event methods.
#[derive(Debug, Clone)]
pub struct CallManager {
    state: CallState,
    media: Option<CallMedia>,
    local_mute: MuteState,
    remote_mute: MuteState,
}

impl Default for CallManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CallManager {
    /// A fresh, idle manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: CallState::Idle,
            media: None,
            local_mute: MuteState::default(),
            remote_mute: MuteState::default(),
        }
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn state(&self) -> CallState {
        self.state
    }

    /// The agreed media kind, once a call is answered (`None` while idle/ringing/terminal-without-answer).
    #[must_use]
    pub fn media(&self) -> Option<CallMedia> {
        self.media
    }

    /// This side's mute state.
    #[must_use]
    pub fn local_mute(&self) -> MuteState {
        self.local_mute
    }

    /// The remote side's last-reported mute state.
    #[must_use]
    pub fn remote_mute(&self) -> MuteState {
        self.remote_mute
    }

    /// Whether media may be captured/sent right now — true only once the call is `Active`. The
    /// per-message mic/camera capability gate still applies on top of this (Inv 15).
    #[must_use]
    pub fn media_live(&self) -> bool {
        self.state.media_live()
    }

    /// Apply an FSM event, updating state; `Invalid` becomes a fail-closed [`CallError`].
    fn apply(&mut self, event: CallEvent) -> Result<(), CallError> {
        match transition(self.state, event) {
            CallTransition::To(next) => {
                self.state = next;
                Ok(())
            }
            CallTransition::Invalid => Err(CallError::InvalidTransition),
        }
    }

    /// Local user places an outbound call offering `media` (`Idle → Dialing`).
    pub fn dial(&mut self, media: CallMedia) -> Result<(), CallError> {
        self.apply(CallEvent::Dial)?;
        self.media = Some(media);
        Ok(())
    }

    /// An inbound invite arrived. Enforces one-active-call: rings only if idle, else replies `Busy`
    /// **without** disturbing the current call.
    pub fn on_invite(&mut self, media: CallMedia) -> InviteResponse {
        if self.state == CallState::Idle && self.apply(CallEvent::Ring).is_ok() {
            self.media = Some(media);
            InviteResponse::Ring { media }
        } else {
            InviteResponse::Busy
        }
    }

    /// Local user accepts an inbound ring, agreeing to `accepted` media (may downgrade the offer;
    /// `Ringing → Connecting`).
    pub fn local_accept(&mut self, accepted: CallMedia) -> Result<(), CallError> {
        self.apply(CallEvent::LocalAccept)?;
        let offered = self.media.unwrap_or(accepted);
        self.media = Some(narrow(offered, accepted));
        Ok(())
    }

    /// Local user declines an inbound ring (`Ringing → Declined`).
    pub fn local_decline(&mut self) -> Result<(), CallError> {
        self.apply(CallEvent::LocalDecline)
    }

    /// The remote accepted our outbound dial, agreeing to `agreed` media (`Dialing → Connecting`). The
    /// agreed kind is bounded by what we offered — the callee can only downgrade.
    pub fn on_remote_accept(&mut self, agreed: CallMedia) -> Result<(), CallError> {
        self.apply(CallEvent::RemoteAccepted)?;
        let offered = self.media.unwrap_or(agreed);
        self.media = Some(narrow(offered, agreed));
        Ok(())
    }

    /// The remote declined our outbound dial (`Dialing → Declined`).
    pub fn on_remote_reject(&mut self) -> Result<(), CallError> {
        self.apply(CallEvent::RemoteDeclined)
    }

    /// The callee is already in a call (`CallBusy`). Treated as a decline of our outbound dial
    /// (`Dialing → Declined`) — the caller ends without ringing anyone.
    pub fn on_busy(&mut self) -> Result<(), CallError> {
        self.apply(CallEvent::RemoteDeclined)
    }

    /// The ring window elapsed with no answer (`Dialing`/`Ringing → Missed`).
    pub fn ring_timeout(&mut self) -> Result<(), CallError> {
        self.apply(CallEvent::RingTimeout)
    }

    /// The media path is up (`Connecting → Active`).
    pub fn media_connected(&mut self) -> Result<(), CallError> {
        self.apply(CallEvent::MediaConnected)
    }

    /// Either side hung up (`in-progress → Ended`).
    pub fn hangup(&mut self) -> Result<(), CallError> {
        self.apply(CallEvent::Hangup)
    }

    /// An unrecoverable failure tore the call down (`in-progress → Failed`).
    pub fn fail(&mut self, code: ErrorCode) -> Result<(), CallError> {
        self.apply(CallEvent::Failed { code })
    }

    /// **Emergency stop (Inv 4).** If a call is in progress, end it immediately and return `true`; if
    /// idle or already terminal, this is a no-op returning `false`. Never errors — a stop must always be
    /// able to end whatever is live. Media capture must be torn down by the caller on a `true` result.
    pub fn emergency_stop(&mut self) -> bool {
        if self.state.in_progress() {
            // Hangup is defined from every in-progress state → Ended, so this cannot fail.
            let _ = self.apply(CallEvent::Hangup);
            true
        } else {
            false
        }
    }

    /// Record this side's mute change. Only meaningful while a call is in progress; ignored otherwise
    /// (a mute toggle with no call is a no-op, not an error).
    pub fn set_local_mute(&mut self, mute: MuteState) {
        if self.state.in_progress() {
            self.local_mute = mute;
        }
    }

    /// Record the remote side's reported mute change (from a `CallMuteState` message).
    pub fn set_remote_mute(&mut self, mute: MuteState) {
        if self.state.in_progress() {
            self.remote_mute = mute;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn outbound_call_dials_answers_and_ends() {
        let mut m = CallManager::new();
        m.dial(CallMedia::Video).unwrap();
        assert_eq!(m.state(), CallState::Dialing);
        m.on_remote_accept(CallMedia::Video).unwrap();
        assert_eq!(m.state(), CallState::Connecting);
        assert_eq!(m.media(), Some(CallMedia::Video));
        m.media_connected().unwrap();
        assert!(m.media_live());
        m.hangup().unwrap();
        assert_eq!(m.state(), CallState::Ended);
        assert!(!m.media_live());
    }

    #[test]
    fn inbound_invite_rings_then_requires_local_accept() {
        let mut m = CallManager::new();
        assert_eq!(
            m.on_invite(CallMedia::Video),
            InviteResponse::Ring {
                media: CallMedia::Video
            }
        );
        assert_eq!(m.state(), CallState::Ringing);
        // The FSM keeps us Ringing until the local user answers (Inv 1) — media isn't live yet.
        assert!(!m.media_live());
        m.local_accept(CallMedia::Video).unwrap();
        m.media_connected().unwrap();
        assert!(m.media_live());
    }

    #[test]
    fn one_active_call_a_second_invite_is_busy_and_does_not_disturb_the_call() {
        let mut m = CallManager::new();
        m.dial(CallMedia::Voice).unwrap();
        m.on_remote_accept(CallMedia::Voice).unwrap();
        m.media_connected().unwrap();
        assert_eq!(m.state(), CallState::Active);
        // A second inbound invite while active → Busy, and the active call is untouched.
        assert_eq!(m.on_invite(CallMedia::Video), InviteResponse::Busy);
        assert_eq!(m.state(), CallState::Active);
    }

    #[test]
    fn media_can_downgrade_but_never_upgrade() {
        // Callee accepts a video invite as voice → agreed = Voice.
        let mut m = CallManager::new();
        m.on_invite(CallMedia::Video);
        m.local_accept(CallMedia::Voice).unwrap();
        assert_eq!(m.media(), Some(CallMedia::Voice));
        // A voice invite "accepted as video" cannot upgrade → stays Voice.
        let mut m2 = CallManager::new();
        m2.on_invite(CallMedia::Voice);
        m2.local_accept(CallMedia::Video).unwrap();
        assert_eq!(m2.media(), Some(CallMedia::Voice));
    }

    #[test]
    fn busy_and_timeout_and_decline_reach_the_right_terminals() {
        let mut a = CallManager::new();
        a.dial(CallMedia::Voice).unwrap();
        a.on_busy().unwrap();
        assert_eq!(a.state(), CallState::Declined);

        let mut b = CallManager::new();
        b.dial(CallMedia::Voice).unwrap();
        b.ring_timeout().unwrap();
        assert_eq!(b.state(), CallState::Missed);

        let mut c = CallManager::new();
        c.on_invite(CallMedia::Voice);
        c.local_decline().unwrap();
        assert_eq!(c.state(), CallState::Declined);
    }

    #[test]
    fn emergency_stop_ends_any_in_progress_call_and_is_noop_when_idle() {
        // Inv 4: emergency stop ends a call from every in-progress phase.
        for setup in [
            |m: &mut CallManager| {
                m.dial(CallMedia::Voice).unwrap();
            },
            |m: &mut CallManager| {
                m.on_invite(CallMedia::Voice);
            },
            |m: &mut CallManager| {
                m.dial(CallMedia::Voice).unwrap();
                m.on_remote_accept(CallMedia::Voice).unwrap();
            },
            |m: &mut CallManager| {
                m.dial(CallMedia::Voice).unwrap();
                m.on_remote_accept(CallMedia::Voice).unwrap();
                m.media_connected().unwrap();
            },
        ] {
            let mut m = CallManager::new();
            setup(&mut m);
            assert!(m.emergency_stop(), "a live call must be endable");
            assert_eq!(m.state(), CallState::Ended);
        }
        // Idle → nothing to stop.
        let mut idle = CallManager::new();
        assert!(!idle.emergency_stop());
        assert_eq!(idle.state(), CallState::Idle);
    }

    #[test]
    fn invalid_events_fail_closed() {
        // Accepting with no ring, hanging up with no call, connecting media before accept — all refused.
        let mut m = CallManager::new();
        assert_eq!(
            m.local_accept(CallMedia::Voice),
            Err(CallError::InvalidTransition)
        );
        assert_eq!(m.hangup(), Err(CallError::InvalidTransition));
        assert_eq!(m.media_connected(), Err(CallError::InvalidTransition));
        m.on_invite(CallMedia::Voice); // Ringing
                                       // A stray media-connected while Ringing must not bypass local accept (Inv 1).
        assert_eq!(m.media_connected(), Err(CallError::InvalidTransition));
        assert_eq!(m.state(), CallState::Ringing);
    }

    #[test]
    fn mute_state_tracks_only_during_a_call() {
        let mut m = CallManager::new();
        let muted = MuteState {
            audio_muted: true,
            video_muted: false,
        };
        // No call → ignored.
        m.set_local_mute(muted);
        assert_eq!(m.local_mute(), MuteState::default());
        // In a call → recorded.
        m.dial(CallMedia::Video).unwrap();
        m.on_remote_accept(CallMedia::Video).unwrap();
        m.media_connected().unwrap();
        m.set_local_mute(muted);
        assert_eq!(m.local_mute(), muted);
        let rmute = MuteState {
            audio_muted: false,
            video_muted: true,
        };
        m.set_remote_mute(rmute);
        assert_eq!(m.remote_mute(), rmute);
    }
}
