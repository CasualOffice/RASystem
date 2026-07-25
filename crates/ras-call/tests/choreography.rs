//! Two-peer call choreography over the **real** control-message codec (ADR-104, L4b integration).
//!
//! This is the first test that composes the whole in-session call stack end-to-end: each peer's
//! [`CallRuntime`] emits `CallAction::Send(ControlMsg)`, and — instead of asserting on the action in
//! isolation — we push it through `ras_protocol::codec::encode` → `decode` (the same protobuf path the
//! wire uses) to the *other* peer's [`CallRuntime::on_control`]. So it proves L3 (the `ControlMsg::Call*`
//! codec) and L4 (the runtime) actually interoperate across a serialization boundary, not just in
//! memory. Media is modelled by counting `StartMedia`/`StopMedia` directives (no real capture — that's
//! the on-device driver's job).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ras_call::{CallAction, CallLifecycleEvent, CallMedia, CallRuntime};
use ras_protocol::{codec, ControlMsg};

/// A test peer: a runtime plus the media/lifecycle bookkeeping a real driver would perform.
#[derive(Default)]
struct Peer {
    rt: CallRuntime,
    media_running: bool,
    ringing: bool,
    events: Vec<CallLifecycleEvent>,
    /// `ControlMsg`s this peer wants to send — already round-tripped through the wire codec by `pump`.
    outbox: Vec<ControlMsg>,
}

impl Peer {
    /// Perform a batch of actions exactly as a driver would, but serialize every outbound `ControlMsg`
    /// through the real codec (encode → decode) so only wire-faithful messages reach the peer.
    fn perform(&mut self, actions: Vec<CallAction>) {
        for a in actions {
            match a {
                CallAction::Send(msg) => {
                    // The load-bearing step: round-trip through the actual protobuf codec.
                    let bytes = codec::encode(&msg);
                    let decoded =
                        codec::decode(&bytes).expect("Call* message must survive the codec");
                    self.outbox.push(decoded);
                }
                CallAction::StartMedia { .. } => self.media_running = true,
                CallAction::StopMedia => self.media_running = false,
                CallAction::Ring { .. } => self.ringing = true,
                CallAction::ClearRing => self.ringing = false,
                CallAction::Emit(e) => self.events.push(e),
                _ => {}
            }
        }
    }

    /// Run a runtime method and perform its actions (avoids borrowing `self` and `self.rt` at once).
    fn step(&mut self, f: impl FnOnce(&mut CallRuntime) -> Vec<CallAction>) {
        let actions = f(&mut self.rt);
        self.perform(actions);
    }

    /// Drain this peer's outbox (already codec-round-tripped) so the harness can deliver it.
    fn take_outbox(&mut self) -> Vec<ControlMsg> {
        std::mem::take(&mut self.outbox)
    }
}

/// Deliver each control message to `peer.on_control`, performing whatever that produces.
fn deliver(peer: &mut Peer, msgs: Vec<ControlMsg>) {
    for m in msgs {
        let actions = peer.rt.on_control(&m);
        peer.perform(actions);
    }
}

#[test]
fn full_video_call_from_dial_to_hangup_over_the_wire() {
    let mut caller = Peer::default();
    let mut callee = Peer::default();

    // 1. Caller places a video call. (The out-of-session ring signal is the driver's job; here we hand
    //    the invite straight to the callee's on_invite, as the signal layer would.)
    caller.step(|rt| rt.place_call(CallMedia::Video));
    assert!(caller
        .events
        .contains(&CallLifecycleEvent::OutgoingRinging {
            media: CallMedia::Video
        }));

    // 2. Callee receives the invite → rings (never auto-answers, Inv 1).
    callee.step(|rt| rt.on_invite(CallMedia::Video));
    assert!(callee.ringing, "callee must be ringing, not auto-answered");
    assert!(
        !callee.media_running,
        "no media before the local user accepts"
    );

    // 3. Local user accepts → CallAccept crosses the real codec to the caller.
    callee.step(|rt| rt.local_accept(CallMedia::Video));
    assert!(!callee.ringing, "accepting clears the ring");
    deliver(&mut caller, callee.take_outbox());
    assert!(caller.events.contains(&CallLifecycleEvent::Connecting));

    // 4. Both sides' media session comes up → both start media, both go Active.
    caller.step(|rt| rt.media_connected());
    callee.step(|rt| rt.media_connected());
    assert!(
        caller.media_running && callee.media_running,
        "both peers capturing"
    );
    assert!(caller.events.contains(&CallLifecycleEvent::Active {
        media: CallMedia::Video
    }));

    // 5. Callee mutes its camera mid-call → CallMuteState crosses the codec → caller sees it.
    callee.step(|rt| rt.set_local_mute(false, true));
    deliver(&mut caller, callee.take_outbox());
    assert!(caller
        .events
        .contains(&CallLifecycleEvent::RemoteMuteChanged {
            audio_muted: false,
            video_muted: true,
        }));
    assert!(caller.rt.manager().remote_mute().video_muted);

    // 6. Caller hangs up → CallHangup crosses the codec → callee stops media + ends.
    caller.step(|rt| rt.hangup());
    assert!(
        !caller.media_running,
        "hang-up stops the caller's media (Inv 4/12)"
    );
    deliver(&mut callee, caller.take_outbox());
    assert!(
        !callee.media_running,
        "peer hang-up stops the callee's media too"
    );
    assert!(callee.events.contains(&CallLifecycleEvent::Ended));
}

#[test]
fn callee_decline_crosses_the_wire_and_ends_the_callers_dial() {
    let mut caller = Peer::default();
    let mut callee = Peer::default();

    caller.step(|rt| rt.place_call(CallMedia::Voice));
    callee.step(|rt| rt.on_invite(CallMedia::Voice));
    // Decline → CallReject over the codec → caller's dial ends as Declined.
    callee.step(|rt| rt.local_decline());
    deliver(&mut caller, callee.take_outbox());
    assert!(caller.events.contains(&CallLifecycleEvent::Declined));
    assert!(!caller.media_running && !callee.media_running);
}

#[test]
fn emergency_stop_mid_call_tears_down_both_ends_over_the_wire() {
    let mut caller = Peer::default();
    let mut callee = Peer::default();

    // Establish an active call.
    caller.step(|rt| rt.place_call(CallMedia::Video));
    callee.step(|rt| rt.on_invite(CallMedia::Video));
    callee.step(|rt| rt.local_accept(CallMedia::Video));
    deliver(&mut caller, callee.take_outbox());
    caller.step(|rt| rt.media_connected());
    callee.step(|rt| rt.media_connected());
    assert!(caller.media_running && callee.media_running);

    // Caller hits the emergency stop → media down immediately + CallHangup{SessionRevoked} to the peer.
    caller.step(|rt| rt.emergency_stop());
    assert!(
        !caller.media_running,
        "emergency stop halts local media at once (Inv 4)"
    );
    let out = caller.take_outbox();
    assert!(
        out.iter().any(|m| matches!(m, ControlMsg::CallHangup { code } if *code == ras_protocol::ErrorCode::SessionRevoked)),
        "peer is told with a SessionRevoked hangup"
    );
    deliver(&mut callee, out);
    assert!(!callee.media_running, "the peer tears down too");
    assert!(callee.events.contains(&CallLifecycleEvent::Ended));
}
