// Casual Annotate — the in-page wire relay for jitsi-meet-electron (ADR-107 Decision 10).
//
// THIS is the actual fix for the defect recorded in Decision 8/9: `ExternalApiTransport` can send
// into a conference but can never receive, because `endpointTextMessageReceived` has zero callers in
// jitsi-meet's web app. The Electron desktop adapter kept using it anyway, so a request from an
// annotator could reach the app's iframe and be silently dropped forever, and the native consent
// dialog it feeds could never fire.
//
// This script is bundled standalone (IIFE) and injected directly into that iframe by the Electron
// MAIN process, via the privileged `webContents.mainFrame` `executeJavaScript` API
// (`adapters/jitsi-electron/main.js`) — NOT via a `body.html` edit on the jitsi-meet server, which
// `standalone/inject.js`'s browser deployment needs but this does not. That is a deliberate
// deployment win beyond the bug fix: the desktop app works against any jitsi-meet server, not only
// ones an operator has modified.
//
// Running inside the iframe means it holds the real `lib-jitsi-meet` conference, so
// `ConferenceTransport` genuinely receives. This script does nothing else — no session, no consent
// UI, no toolbar. Those already live correctly on the other side of this bridge: the sharer's
// `SharerController` in the overlay window (`overlay-page.js`), and the native consent dialog driven
// by `renderer.js` watching that session's state. This is purely the wire between them and the real
// conference, exactly the one piece that was broken.

import { waitForConference } from '../../standalone/conference.js';
import { ConferenceTransport } from '../../transport/jitsi.js';

const EVENTS = { ENDPOINT_MESSAGE_RECEIVED: 'conference.endpoint_message_received' };
const ROSTER_INTERVAL_MS = 1500;

async function run() {
    // Idempotent: `main.js` re-injects on every `did-frame-finish-load` (a rejoin navigates the
    // frame, which wipes any prior injection along with it) rather than trying to detect exactly
    // when that happens, so a guard here is what keeps a same-document re-injection a no-op instead
    // of a second live relay racing the first.
    if (window.__casualAnnotateRelay) return;
    window.__casualAnnotateRelay = true;

    const room = await waitForConference();
    const transport = new ConferenceTransport(room, EVENTS);

    transport.onOp((sender, msg) => {
        window.parent.postMessage({ __casualAnnotateWire: { type: 'op', sender, msg } }, '*');
    });

    // The host has no way to call into this frame (that is the whole problem this script exists to
    // solve), so it posts outbound sends in instead of us polling for them.
    //
    // `e.source !== window.parent` is not incidental — it is the whole reason this listener is safe
    // to have at all. Without it, ANY script able to reach this window (any other content this page
    // loads, a compromised third party, a future XSS on the operator's own jitsi-meet deployment —
    // this script deliberately runs against an operator's server we do not control, per the header
    // above) could post a `__casualAnnotateEmit` and have it sent on the REAL conference AS THE LOCAL
    // USER via the real `ConferenceTransport` below — forging a `grant` to any id, broadcasting a
    // fake `roster`, or sending `deny`/`revoke` to grief a real annotator, with no consent dialog
    // anywhere in that path. Only the actual embedding parent (the Electron host renderer this
    // script was injected FROM) is trusted.
    window.addEventListener('message', (e) => {
        if (e.source !== window.parent) return;
        const d = e?.data?.__casualAnnotateEmit;
        if (!d) return;
        if (d.to) transport.send(d.to, d.op);
        else transport.broadcast(d.op);
    });

    // No request/response channel exists for the host to ask "who's in the room" on demand, so this
    // pushes a fresh snapshot on an interval — the same polling-over-events choice
    // `standalone/inject.js` already makes, and for the same reason: which conference events fire
    // reliably differs across jitsi-meet versions, but `getParticipants()` always works.
    const pushRoster = () => {
        window.parent.postMessage(
            { __casualAnnotateWire: { type: 'roster', participants: transport.participants() } }, '*',
        );
    };
    setInterval(pushRoster, ROSTER_INTERVAL_MS);
    pushRoster();

    // Also push immediately on a join/leave, on top of the interval above. `renderer.js`'s
    // `syncParticipants()` reconciles the OVERLAY's admitted set against whatever roster snapshot
    // `PostMessageTransport.participants()` last cached — and fires instantly off the React app's own
    // `participantJoined`/`participantLeft` events, not off this interval. A participant who joined
    // (and, after a human clicked Allow, got admitted) inside the last stale window could be reconciled
    // away as "left" — a real, if narrow, race that silently revoked a genuinely-admitted annotator's
    // access with no notification sent to them. `JitsiConference`'s own `USER_JOINED`/`USER_LEFT` are
    // stable, lower-level lib-jitsi-meet API — unlike the React app's UI events, which is why
    // `standalone/inject.js` avoids relying on events elsewhere — so pushing on them here narrows the
    // window without reintroducing that version-skew risk.
    try {
        room.on('conference.userJoined', pushRoster);
        room.on('conference.userLeft', pushRoster);
    } catch { /* not fatal — the interval above still covers it, just less promptly */ }
}

run();
