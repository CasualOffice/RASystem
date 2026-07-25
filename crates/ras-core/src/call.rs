//! Call driver (ADR-104, L4c) — the thin async-friendly shell that connects the pure [`CallRuntime`]
//! (`ras-call`) to the real world through dependency-injected sinks. It owns the runtime, and every
//! driver method feeds an event to the runtime and then **dispatches** the returned
//! [`ras_call::CallAction`]s to the seams: a control message goes to the session control channel, media
//! start/stop drives the mic/camera pumps, lifecycle events surface to the app/audit. The
//! out-of-session **ring** (`CallInvite`/`CallCancel`) is a signed signal, so it rides a separate
//! [`CallSignalSink`] the driver invokes explicitly (it is not a `ControlMsg`).
//!
//! The driver is transport- and media-agnostic: `ras-core` provides the choreography; the app supplies
//! the concrete seams (iroh control channel, `ras-signal` sender, `ras-mic`/`ras-camera` pumps, the
//! Tauri event bridge). That keeps the whole thing loopback-testable off-device — the seams are trivial
//! doubles in tests. Invariants are inherited verbatim from the runtime: an inbound ring never
//! auto-answers (Inv 1); [`CallDriver::emergency_stop`] always stops media + tells the peer (Inv 4);
//! media never starts before the call is active and the per-message mic/camera gate still applies at the
//! send boundary (Inv 15); every event is content-free (Inv 8).

use ras_call::{CallAction, CallLifecycleEvent, CallMedia, CallRuntime};
use ras_protocol::ControlMsg;
use std::sync::Arc;

/// Puts an in-session call `ControlMsg` on the peer's control channel. The app wires this to the live
/// session's control sender.
pub trait CallControlSink: Send + Sync {
    /// Send one control message (best-effort; the transport owns delivery/ordering).
    fn send(&self, msg: ControlMsg);
}

/// Sends the out-of-session **ring** signal (`CallInvite` / `CallCancel`) — a signed, contacts-only
/// `ras-signal` payload, distinct from the in-session control plane.
pub trait CallSignalSink: Send + Sync {
    /// Ring the peer with an offer of `media`.
    fn send_invite(&self, media: CallMedia);
    /// Withdraw a still-ringing outbound call (we hung up before they answered).
    fn send_cancel(&self);
}

/// Starts/stops this side's call media capture (mic always; camera iff the call is video). The concrete
/// implementation is the `ras-mic` / `ras-camera` pumps; capture is still gated per-message host-side by
/// the `audio.mic.capture` / `video.camera.capture` capabilities at the send boundary (Inv 15).
pub trait CallMediaController: Send + Sync {
    /// Begin capturing + sending call media of the given kind.
    fn start(&self, media: CallMedia);
    /// Stop all call media capture (Inv 4/12 — nothing keeps capturing after a call ends).
    fn stop(&self);
}

/// Surfaces content-free call lifecycle events to the app / audit (Inv 8).
pub trait CallEventSink: Send + Sync {
    /// Emit one lifecycle event.
    fn emit(&self, event: CallLifecycleEvent);
}

/// Owns a [`CallRuntime`] and drives it against the injected seams. Clone-free; hold one per active call
/// slot (one-active-call is enforced inside the runtime).
pub struct CallDriver {
    runtime: CallRuntime,
    control: Arc<dyn CallControlSink>,
    signal: Arc<dyn CallSignalSink>,
    media: Arc<dyn CallMediaController>,
    events: Arc<dyn CallEventSink>,
}

impl CallDriver {
    /// Build a driver over the four seams.
    #[must_use]
    pub fn new(
        control: Arc<dyn CallControlSink>,
        signal: Arc<dyn CallSignalSink>,
        media: Arc<dyn CallMediaController>,
        events: Arc<dyn CallEventSink>,
    ) -> Self {
        Self {
            runtime: CallRuntime::new(),
            control,
            signal,
            media,
            events,
        }
    }

    /// The underlying runtime (state/media/mute queries).
    #[must_use]
    pub fn runtime(&self) -> &CallRuntime {
        &self.runtime
    }

    /// Dispatch the runtime's actions to the seams. `Ring`/`ClearRing` are UI directives already covered
    /// by the `Incoming/OutgoingRinging` + `Ended`/`Declined` lifecycle events, so the app drives its
    /// ring UI from those; the driver forwards only the effectful actions.
    fn dispatch(&self, actions: Vec<CallAction>) {
        for a in actions {
            match a {
                CallAction::Send(msg) => self.control.send(msg),
                CallAction::StartMedia { media } => self.media.start(media),
                CallAction::StopMedia => self.media.stop(),
                CallAction::Emit(ev) => self.events.emit(ev),
                CallAction::Ring { .. } | CallAction::ClearRing => {}
                _ => {}
            }
        }
    }

    // ── Local actions ──────────────────────────────────────────────────────────────────────────

    /// Local user places an outbound call: transition + send the `CallInvite` **signal** (the ring).
    pub fn place_call(&mut self, media: CallMedia) {
        let actions = self.runtime.place_call(media);
        if actions.is_empty() {
            return; // already in a call, etc. — nothing dialed
        }
        self.dispatch(actions);
        self.signal.send_invite(media);
    }

    /// Local user accepts an inbound ring (may downgrade to `accepted`). Sends `CallAccept`; the app then
    /// dials the media session and calls [`Self::media_connected`].
    pub fn local_accept(&mut self, accepted: CallMedia) {
        let actions = self.runtime.local_accept(accepted);
        self.dispatch(actions);
    }

    /// Local user declines an inbound ring.
    pub fn local_decline(&mut self) {
        let actions = self.runtime.local_decline();
        self.dispatch(actions);
    }

    /// The media session came up → start media, go active.
    pub fn media_connected(&mut self) {
        let actions = self.runtime.media_connected();
        self.dispatch(actions);
    }

    /// Local user hangs up.
    pub fn hangup(&mut self) {
        let actions = self.runtime.hangup();
        self.dispatch(actions);
    }

    /// Emergency stop (Inv 4): stop media + tell the peer, whatever phase the call is in.
    pub fn emergency_stop(&mut self) {
        let actions = self.runtime.emergency_stop();
        self.dispatch(actions);
    }

    /// The ring window elapsed with no answer. On an outbound call also withdraw the ring signal.
    pub fn ring_timeout(&mut self) {
        let was_dialing = matches!(self.runtime.manager().state(), ras_call::CallState::Dialing);
        let actions = self.runtime.ring_timeout();
        if actions.is_empty() {
            return;
        }
        self.dispatch(actions);
        if was_dialing {
            self.signal.send_cancel();
        }
    }

    /// Local user toggled mic/camera mid-call; tell the peer.
    pub fn set_local_mute(&mut self, audio_muted: bool, video_muted: bool) {
        let actions = self.runtime.set_local_mute(audio_muted, video_muted);
        self.dispatch(actions);
    }

    // ── Inbound events ─────────────────────────────────────────────────────────────────────────

    /// An inbound `CallInvite` **signal** arrived: ring the local user, or reply busy (one-active-call).
    pub fn on_invite(&mut self, media: CallMedia) {
        let actions = self.runtime.on_invite(media);
        self.dispatch(actions);
    }

    /// An inbound in-session call `ControlMsg` arrived (from the control channel).
    pub fn on_control(&mut self, msg: &ControlMsg) {
        let actions = self.runtime.on_control(msg);
        self.dispatch(actions);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_protocol::ErrorCode;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<ControlMsg>>,
        invites: Mutex<Vec<CallMedia>>,
        cancels: Mutex<u32>,
        media_started: Mutex<Vec<CallMedia>>,
        media_stopped: Mutex<u32>,
        events: Mutex<Vec<CallLifecycleEvent>>,
    }
    impl CallControlSink for Recorder {
        fn send(&self, msg: ControlMsg) {
            self.sent.lock().unwrap().push(msg);
        }
    }
    impl CallSignalSink for Recorder {
        fn send_invite(&self, media: CallMedia) {
            self.invites.lock().unwrap().push(media);
        }
        fn send_cancel(&self) {
            *self.cancels.lock().unwrap() += 1;
        }
    }
    impl CallMediaController for Recorder {
        fn start(&self, media: CallMedia) {
            self.media_started.lock().unwrap().push(media);
        }
        fn stop(&self) {
            *self.media_stopped.lock().unwrap() += 1;
        }
    }
    impl CallEventSink for Recorder {
        fn emit(&self, event: CallLifecycleEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn driver(r: &Arc<Recorder>) -> CallDriver {
        CallDriver::new(r.clone(), r.clone(), r.clone(), r.clone())
    }

    #[test]
    fn outbound_call_rings_via_signal_and_starts_media_on_connect() {
        let r = Arc::new(Recorder::default());
        let mut d = driver(&r);
        d.place_call(CallMedia::Video);
        // The ring goes out as a SIGNAL (not a ControlMsg) …
        assert_eq!(*r.invites.lock().unwrap(), vec![CallMedia::Video]);
        assert!(r.sent.lock().unwrap().is_empty());
        assert!(r
            .events
            .lock()
            .unwrap()
            .contains(&CallLifecycleEvent::OutgoingRinging {
                media: CallMedia::Video
            }));
        // Remote accepts (in-session control), media path up → media starts.
        d.on_control(&ControlMsg::CallAccept {
            media: CallMedia::Video,
        });
        d.media_connected();
        assert_eq!(*r.media_started.lock().unwrap(), vec![CallMedia::Video]);
    }

    #[test]
    fn inbound_ring_requires_local_accept_then_sends_callaccept() {
        let r = Arc::new(Recorder::default());
        let mut d = driver(&r);
        d.on_invite(CallMedia::Voice);
        // No CallAccept, no media before the local user answers (Inv 1).
        assert!(r.sent.lock().unwrap().is_empty());
        assert!(r.media_started.lock().unwrap().is_empty());
        d.local_accept(CallMedia::Voice);
        assert!(r
            .sent
            .lock()
            .unwrap()
            .iter()
            .any(|m| matches!(m, ControlMsg::CallAccept { .. })));
    }

    #[test]
    fn emergency_stop_stops_media_and_sends_revoked_hangup() {
        let r = Arc::new(Recorder::default());
        let mut d = driver(&r);
        d.place_call(CallMedia::Video);
        d.on_control(&ControlMsg::CallAccept {
            media: CallMedia::Video,
        });
        d.media_connected();
        d.emergency_stop();
        assert_eq!(*r.media_stopped.lock().unwrap(), 1);
        assert!(r.sent.lock().unwrap().iter().any(
            |m| matches!(m, ControlMsg::CallHangup { code } if *code == ErrorCode::SessionRevoked)
        ));
    }

    #[test]
    fn outbound_ring_timeout_cancels_the_signal() {
        let r = Arc::new(Recorder::default());
        let mut d = driver(&r);
        d.place_call(CallMedia::Voice);
        d.ring_timeout();
        assert_eq!(
            *r.cancels.lock().unwrap(),
            1,
            "outbound no-answer withdraws the ring"
        );
    }
}
