//! The pure call runtime (ADR-104, L4b) — the "brain wiring" that turns call events into the concrete
//! things a call driver must do: send a control message, ring the local user, start/stop media, emit a
//! lifecycle event. It owns a [`CallManager`] and stays **pure** — no async, no I/O, no clock, no
//! media — so the whole integration is unit-testable off-device. `ras-core` / the app drive it: they
//! feed it inbound signals/control + local actions, and *perform* the returned [`CallAction`]s (put a
//! `ControlMsg` on the wire, raise the incoming-call window, spin up the mic/camera pump).
//!
//! Split of planes it respects: the out-of-session **ring** (`CallInvite`/`CallCancel`) is a signed
//! signal the *driver* sends — this runtime handles the **in-session** control plane (`ControlMsg::Call*`)
//! + the lifecycle/media directives. So `place_call` transitions + emits, and the driver sends the
//! invite signal separately; an inbound invite signal is fed in via [`CallRuntime::on_invite`].
//!
//! Invariants: an inbound ring never auto-answers — only [`CallRuntime::local_accept`] leaves `Ringing`
//! (Inv 1); [`CallRuntime::emergency_stop`] always tears media down + sends a `CallHangup` (Inv 4); a
//! control message that isn't valid for the current state is ignored (fail-closed); every action/event
//! is content-free (Inv 8). Media never starts until [`CallState::Active`] and the per-message mic/camera
//! capability gate still applies downstream (Inv 15).

use crate::{CallManager, CallMedia, InviteResponse, MuteState};
use ras_protocol::{ControlMsg, ErrorCode};

/// A content-free call lifecycle event for the app/audit (never a callee label, ringtone, or media byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallLifecycleEvent {
    /// We placed a call and are waiting for the remote to answer.
    OutgoingRinging { media: CallMedia },
    /// An inbound call is ringing for the local user to accept/decline.
    IncomingRinging { media: CallMedia },
    /// Answered by both sides; the media path is coming up.
    Connecting,
    /// Media is live.
    Active { media: CallMedia },
    /// The call ended normally (hang-up / emergency stop).
    Ended,
    /// Declined by the callee (or by us on an inbound ring).
    Declined,
    /// No answer within the ring window.
    Missed,
    /// An unrecoverable failure tore the call down.
    Failed,
    /// The remote peer changed its mute state.
    RemoteMuteChanged {
        audio_muted: bool,
        video_muted: bool,
    },
}

/// A concrete directive the call driver must perform. Pure data — the runtime never performs I/O.
/// (Not `PartialEq`: it carries a [`ControlMsg`], which deliberately isn't comparable.)
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CallAction {
    /// Put this control message on the peer's in-session control channel.
    Send(ControlMsg),
    /// Raise the incoming-call surface for the **local user** to accept/decline (Inv 1). Never auto-answer.
    Ring { media: CallMedia },
    /// Dismiss the incoming-call / outgoing-dialing surface.
    ClearRing,
    /// Begin capturing + sending this call's media (mic always; camera iff `media` is video). The
    /// per-message mic/camera capability gate still applies at the send boundary (Inv 15).
    StartMedia { media: CallMedia },
    /// Stop **all** call media capture (Inv 4/12 — nothing keeps capturing after a call ends).
    StopMedia,
    /// Surface a content-free lifecycle event.
    Emit(CallLifecycleEvent),
}

/// Owns a [`CallManager`] and maps call events → [`CallAction`]s. Construct with [`CallRuntime::new`].
#[derive(Debug, Clone, Default)]
pub struct CallRuntime {
    manager: CallManager,
}

impl CallRuntime {
    /// A fresh, idle runtime.
    #[must_use]
    pub fn new() -> Self {
        Self {
            manager: CallManager::new(),
        }
    }

    /// The underlying manager (state/media/mute queries).
    #[must_use]
    pub fn manager(&self) -> &CallManager {
        &self.manager
    }

    // ── Local actions (this side initiates) ────────────────────────────────────────────────────

    /// Local user places an outbound call. The driver must ALSO send a `CallInvite` **signal** (that
    /// out-of-session ring is not a `ControlMsg`); this only transitions + emits.
    pub fn place_call(&mut self, media: CallMedia) -> Vec<CallAction> {
        if self.manager.dial(media).is_err() {
            return vec![];
        }
        vec![CallAction::Emit(CallLifecycleEvent::OutgoingRinging {
            media,
        })]
    }

    /// Local user accepts an inbound ring, agreeing to `accepted` media (may downgrade). Sends
    /// `CallAccept`; the driver then dials the media session and calls [`Self::media_connected`].
    pub fn local_accept(&mut self, accepted: CallMedia) -> Vec<CallAction> {
        if self.manager.local_accept(accepted).is_err() {
            return vec![];
        }
        let media = self.manager.media().unwrap_or(accepted);
        vec![
            CallAction::Send(ControlMsg::CallAccept { media }),
            CallAction::ClearRing,
            CallAction::Emit(CallLifecycleEvent::Connecting),
        ]
    }

    /// Local user declines an inbound ring. Sends `CallReject { ConsentDenied }`.
    pub fn local_decline(&mut self) -> Vec<CallAction> {
        if self.manager.local_decline().is_err() {
            return vec![];
        }
        vec![
            CallAction::Send(ControlMsg::CallReject {
                code: ErrorCode::ConsentDenied,
            }),
            CallAction::ClearRing,
            CallAction::Emit(CallLifecycleEvent::Declined),
        ]
    }

    /// The media session came up (`Connecting → Active`). Starts media capture.
    pub fn media_connected(&mut self) -> Vec<CallAction> {
        if self.manager.media_connected().is_err() {
            return vec![];
        }
        let media = self.manager.media().unwrap_or(CallMedia::Voice);
        vec![
            CallAction::StartMedia { media },
            CallAction::Emit(CallLifecycleEvent::Active { media }),
        ]
    }

    /// Either side hangs up. Sends `CallHangup { NormalClosure }`, stops media, ends.
    pub fn hangup(&mut self) -> Vec<CallAction> {
        if self.manager.hangup().is_err() {
            return vec![];
        }
        vec![
            CallAction::Send(ControlMsg::CallHangup {
                code: ErrorCode::NormalClosure,
            }),
            CallAction::StopMedia,
            CallAction::ClearRing,
            CallAction::Emit(CallLifecycleEvent::Ended),
        ]
    }

    /// **Emergency stop (Inv 4).** Ends any in-progress call: tears media down + tells the peer
    /// (`CallHangup { SessionRevoked }`). No-op (empty) when idle/terminal. Never fails.
    pub fn emergency_stop(&mut self) -> Vec<CallAction> {
        if self.manager.emergency_stop() {
            vec![
                CallAction::StopMedia,
                CallAction::Send(ControlMsg::CallHangup {
                    code: ErrorCode::SessionRevoked,
                }),
                CallAction::ClearRing,
                CallAction::Emit(CallLifecycleEvent::Ended),
            ]
        } else {
            vec![]
        }
    }

    /// The ring window elapsed with no answer. On an *outbound* call the driver should also send a
    /// `CallCancel` signal (out-of-session); this transitions + emits.
    pub fn ring_timeout(&mut self) -> Vec<CallAction> {
        if self.manager.ring_timeout().is_err() {
            return vec![];
        }
        vec![
            CallAction::ClearRing,
            CallAction::Emit(CallLifecycleEvent::Missed),
        ]
    }

    /// Local user toggled mic/camera mid-call. Records it and tells the peer (`CallMuteState`).
    pub fn set_local_mute(&mut self, audio_muted: bool, video_muted: bool) -> Vec<CallAction> {
        if !self.manager.state().in_progress() {
            return vec![];
        }
        self.manager.set_local_mute(MuteState {
            audio_muted,
            video_muted,
        });
        vec![CallAction::Send(ControlMsg::CallMuteState {
            audio_muted,
            video_muted,
        })]
    }

    // ── Inbound events (the remote / signal plane) ─────────────────────────────────────────────

    /// An inbound `CallInvite` **signal** arrived. Rings the local user, or replies `CallBusy` if
    /// already in a call (one-active-call) without disturbing it.
    pub fn on_invite(&mut self, media: CallMedia) -> Vec<CallAction> {
        match self.manager.on_invite(media) {
            InviteResponse::Ring { media } => vec![
                CallAction::Ring { media },
                CallAction::Emit(CallLifecycleEvent::IncomingRinging { media }),
            ],
            InviteResponse::Busy => vec![CallAction::Send(ControlMsg::CallBusy)],
        }
    }

    /// An inbound in-session call `ControlMsg`. Ignored (empty) if not valid for the current state
    /// (fail-closed). Non-call `ControlMsg`s return empty.
    pub fn on_control(&mut self, msg: &ControlMsg) -> Vec<CallAction> {
        match msg {
            // The remote accepted our outbound dial (with the agreed, possibly downgraded, media). The
            // driver dials the media session next, then calls media_connected() to emit Active + start.
            ControlMsg::CallAccept { media } => {
                if self.manager.on_remote_accept(*media).is_err() {
                    return vec![];
                }
                vec![
                    CallAction::ClearRing,
                    CallAction::Emit(CallLifecycleEvent::Connecting),
                ]
            }
            // The remote declined our outbound dial.
            ControlMsg::CallReject { .. } => {
                if self.manager.on_remote_reject().is_err() {
                    return vec![];
                }
                vec![
                    CallAction::ClearRing,
                    CallAction::Emit(CallLifecycleEvent::Declined),
                ]
            }
            // The callee is already in a call.
            ControlMsg::CallBusy => {
                if self.manager.on_busy().is_err() {
                    return vec![];
                }
                vec![
                    CallAction::ClearRing,
                    CallAction::Emit(CallLifecycleEvent::Declined),
                ]
            }
            // The peer hung up.
            ControlMsg::CallHangup { .. } => {
                if self.manager.hangup().is_err() {
                    return vec![];
                }
                vec![
                    CallAction::StopMedia,
                    CallAction::ClearRing,
                    CallAction::Emit(CallLifecycleEvent::Ended),
                ]
            }
            // The peer changed its mute state.
            ControlMsg::CallMuteState {
                audio_muted,
                video_muted,
            } => {
                if !self.manager.state().in_progress() {
                    return vec![];
                }
                self.manager.set_remote_mute(MuteState {
                    audio_muted: *audio_muted,
                    video_muted: *video_muted,
                });
                vec![CallAction::Emit(CallLifecycleEvent::RemoteMuteChanged {
                    audio_muted: *audio_muted,
                    video_muted: *video_muted,
                })]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallState;

    // CallAction isn't PartialEq (it carries a ControlMsg), so inspect via matchers.
    fn first_send(a: &[CallAction]) -> Option<&ControlMsg> {
        a.iter().find_map(|x| match x {
            CallAction::Send(m) => Some(m),
            _ => None,
        })
    }
    fn emits(a: &[CallAction], want: CallLifecycleEvent) -> bool {
        a.iter()
            .any(|x| matches!(x, CallAction::Emit(e) if *e == want))
    }
    fn has_start_media(a: &[CallAction]) -> bool {
        a.iter().any(|x| matches!(x, CallAction::StartMedia { .. }))
    }
    fn has_stop_media(a: &[CallAction]) -> bool {
        a.iter().any(|x| matches!(x, CallAction::StopMedia))
    }
    fn has_ring(a: &[CallAction]) -> bool {
        a.iter().any(|x| matches!(x, CallAction::Ring { .. }))
    }
    fn has_clear_ring(a: &[CallAction]) -> bool {
        a.iter().any(|x| matches!(x, CallAction::ClearRing))
    }

    #[test]
    fn caller_flow_dials_accepts_starts_media_and_ends() {
        let mut r = CallRuntime::new();
        let a = r.place_call(CallMedia::Video);
        assert!(emits(
            &a,
            CallLifecycleEvent::OutgoingRinging {
                media: CallMedia::Video
            }
        ));
        // Remote accepts (as video).
        let a = r.on_control(&ControlMsg::CallAccept {
            media: CallMedia::Video,
        });
        assert_eq!(r.manager().state(), CallState::Connecting);
        assert!(emits(&a, CallLifecycleEvent::Connecting));
        // Media path up → start media + Active.
        let a = r.media_connected();
        assert!(has_start_media(&a));
        assert!(emits(
            &a,
            CallLifecycleEvent::Active {
                media: CallMedia::Video
            }
        ));
        assert_eq!(r.manager().state(), CallState::Active);
        // Hang up → sends CallHangup(NormalClosure), stops media.
        let a = r.hangup();
        assert!(has_stop_media(&a));
        assert!(matches!(
            first_send(&a),
            Some(ControlMsg::CallHangup {
                code: ErrorCode::NormalClosure
            })
        ));
        assert_eq!(r.manager().state(), CallState::Ended);
    }

    #[test]
    fn callee_flow_rings_requires_accept_then_starts_media() {
        let mut r = CallRuntime::new();
        let a = r.on_invite(CallMedia::Video);
        assert!(has_ring(&a));
        assert_eq!(r.manager().state(), CallState::Ringing);
        // A stray media-connected while Ringing does NOTHING (Inv 1 — no auto-answer).
        assert!(r.media_connected().is_empty());
        assert_eq!(r.manager().state(), CallState::Ringing);
        // Local accept sends CallAccept + Connecting.
        let a = r.local_accept(CallMedia::Video);
        assert!(matches!(
            first_send(&a),
            Some(ControlMsg::CallAccept {
                media: CallMedia::Video
            })
        ));
        assert!(has_start_media(&r.media_connected()));
        assert_eq!(r.manager().state(), CallState::Active);
    }

    #[test]
    fn callee_decline_sends_reject() {
        let mut r = CallRuntime::new();
        r.on_invite(CallMedia::Voice);
        let a = r.local_decline();
        assert!(matches!(
            first_send(&a),
            Some(ControlMsg::CallReject {
                code: ErrorCode::ConsentDenied
            })
        ));
        assert!(has_clear_ring(&a));
        assert_eq!(r.manager().state(), CallState::Declined);
    }

    #[test]
    fn one_active_call_second_invite_replies_busy_and_does_not_ring() {
        let mut r = CallRuntime::new();
        r.place_call(CallMedia::Voice);
        r.on_control(&ControlMsg::CallAccept {
            media: CallMedia::Voice,
        });
        r.media_connected();
        assert_eq!(r.manager().state(), CallState::Active);
        // Second invite → CallBusy, no Ring, call untouched.
        let a = r.on_invite(CallMedia::Video);
        assert!(matches!(first_send(&a), Some(ControlMsg::CallBusy)));
        assert!(!has_ring(&a));
        assert_eq!(r.manager().state(), CallState::Active);
    }

    #[test]
    fn emergency_stop_stops_media_and_tells_peer() {
        let mut r = CallRuntime::new();
        r.place_call(CallMedia::Video);
        r.on_control(&ControlMsg::CallAccept {
            media: CallMedia::Video,
        });
        r.media_connected();
        let a = r.emergency_stop();
        assert!(has_stop_media(&a));
        assert!(matches!(
            first_send(&a),
            Some(ControlMsg::CallHangup {
                code: ErrorCode::SessionRevoked
            })
        ));
        assert_eq!(r.manager().state(), CallState::Ended);
        // Idempotent: a second stop is a no-op.
        assert!(r.emergency_stop().is_empty());
    }

    #[test]
    fn peer_hangup_stops_media_locally() {
        let mut r = CallRuntime::new();
        r.place_call(CallMedia::Voice);
        r.on_control(&ControlMsg::CallAccept {
            media: CallMedia::Voice,
        });
        r.media_connected();
        let a = r.on_control(&ControlMsg::CallHangup {
            code: ErrorCode::NormalClosure,
        });
        assert!(has_stop_media(&a));
        assert!(emits(&a, CallLifecycleEvent::Ended));
        assert_eq!(r.manager().state(), CallState::Ended);
    }

    #[test]
    fn mute_state_round_trips() {
        let mut r = CallRuntime::new();
        r.place_call(CallMedia::Video);
        r.on_control(&ControlMsg::CallAccept {
            media: CallMedia::Video,
        });
        r.media_connected();
        // Local mute → tells the peer.
        let a = r.set_local_mute(true, false);
        assert!(matches!(
            first_send(&a),
            Some(ControlMsg::CallMuteState {
                audio_muted: true,
                video_muted: false
            })
        ));
        assert!(r.manager().local_mute().audio_muted);
        // Remote mute → surfaced as a lifecycle event, recorded.
        let a = r.on_control(&ControlMsg::CallMuteState {
            audio_muted: false,
            video_muted: true,
        });
        assert!(emits(
            &a,
            CallLifecycleEvent::RemoteMuteChanged {
                audio_muted: false,
                video_muted: true
            }
        ));
        assert!(r.manager().remote_mute().video_muted);
    }

    #[test]
    fn video_invite_accepted_as_voice_downgrades() {
        let mut r = CallRuntime::new();
        r.on_invite(CallMedia::Video);
        let a = r.local_accept(CallMedia::Voice);
        // The CallAccept carries the downgraded (voice) media.
        assert!(matches!(
            first_send(&a),
            Some(ControlMsg::CallAccept {
                media: CallMedia::Voice
            })
        ));
        assert_eq!(r.manager().media(), Some(CallMedia::Voice));
    }

    #[test]
    fn stray_control_in_idle_is_ignored() {
        let mut r = CallRuntime::new();
        // No call in progress: any inbound call control is a no-op (fail-closed).
        assert!(r
            .on_control(&ControlMsg::CallHangup {
                code: ErrorCode::NormalClosure
            })
            .is_empty());
        assert!(r
            .on_control(&ControlMsg::CallAccept {
                media: CallMedia::Voice
            })
            .is_empty());
        assert!(r
            .on_control(&ControlMsg::CallMuteState {
                audio_muted: true,
                video_muted: true
            })
            .is_empty());
        assert_eq!(r.manager().state(), CallState::Idle);
    }
}
