// Casual Annotate — standalone browser loader.
//
// The Electron adapter only exists inside the Electron app. A participant in a plain browser has no
// annotation UI at all, which is the difference between "the SDK works" and "a user can use it".
// This is the browser half: drop it into any jitsi-meet page and it mounts the toolbar, finds who
// is sharing, and wires the real controller.
//
// Load it however suits the deployment — a <script> tag in a self-hosted jitsi-meet, a browser
// extension, or pasted into the console for a demo:
//
//     (0, eval)(await (await fetch('.../casual-annotate.js')).text());
//
// The proper home for this is a toolbar button inside jitsi-meet itself, which needs a fork of the
// web app. Until then this mounts its own layer over the meeting UI.

import { ConferenceTransport } from '../transport/jitsi.js';
import { AnnotatorController } from '../annotator.js';
import { SharerController } from '../sharer.js';
import { AnnotationSurface } from '../surface/surface.js';
import { AnnotatorToolbar } from '../surface/toolbar.js';
import { ADMIT } from '../core/session.js';

const EVENTS = { ENDPOINT_MESSAGE_RECEIVED: 'conference.endpoint_message_received' };

/**
 * Wait for jitsi-meet to have a live conference.
 *
 * Deliberately waits FOREVER rather than timing out. The script is loaded with the page, but the
 * user may sit on the prejoin screen for minutes, or leave and rejoin. A timeout here meant the
 * script gave up before the meeting started and never armed again — the UI simply never appeared,
 * with no error a user could see.
 */
async function waitForConference() {
    for (;;) {
        const room = window.APP?.conference?._room;
        if (room?.myUserId?.()) return room;
        await new Promise(r => setTimeout(r, 500));
    }
}

/** The <video> showing the shared screen, so coordinates map to the sharer's pixels (§6). */
function findShareVideo() {
    // Prefer the large video; fall back to the biggest <video> that is not our own self-view.
    const large = document.getElementById('largeVideo');
    if (large?.videoWidth) return large;
    const vids = [ ...document.querySelectorAll('video') ]
        .filter(v => v.videoWidth > 0)
        .sort((a, b) => (b.videoWidth * b.videoHeight) - (a.videoWidth * a.videoHeight));
    return vids[0] ?? null;
}

/**
 * A consent prompt the sharer cannot miss.
 *
 * Rendered in-page rather than as a native dialog for one decisive reason: the External API's
 * `endpointTextMessageReceived` event has NO callers in jitsi-meet's web app
 * (`notifyEndpointTextMessageReceived` is defined at `API.js:1700` and never invoked; only the
 * mobile middleware implements the equivalent). So a host outside the iframe can send but can never
 * RECEIVE — the request could not reach it, and an accept prompt could never appear there.
 *
 * In-page, we hold the real lib-jitsi-meet conference and do receive. So this is where consent has
 * to live.
 */
function askToAllow({ name }) {
    return new Promise((resolve) => {
        const wrap = document.createElement('div');
        wrap.style.cssText = 'position:fixed;inset:0;z-index:2147483600;display:flex;'
            + 'align-items:center;justify-content:center;background:rgba(0,0,0,.45);'
            + 'font:14px/1.5 system-ui,-apple-system,sans-serif';
        const card = document.createElement('div');
        card.style.cssText = 'background:#18181b;color:#fff;padding:22px 24px;border-radius:12px;'
            + 'max-width:420px;box-shadow:0 20px 60px rgba(0,0,0,.6)';
        const who = String(name || 'A participant').slice(0, 64);
        card.innerHTML = `<div style="font-size:16px;font-weight:600;margin-bottom:8px">
            Annotation request</div>
          <div style="opacity:.85;margin-bottom:6px"><b></b> wants to draw on your shared screen.</div>
          <div style="opacity:.6;font-size:13px;margin-bottom:18px">They can draw marks only —
            they cannot click, type, or control anything.</div>
          <div style="display:flex;gap:8px;justify-content:flex-end">
            <button data-deny style="padding:8px 16px;border:0;border-radius:8px;
              background:#3f3f46;color:#fff;font:inherit;cursor:pointer">Deny</button>
            <button data-allow style="padding:8px 16px;border:0;border-radius:8px;
              background:#2563eb;color:#fff;font:inherit;font-weight:600;cursor:pointer">Allow</button>
          </div>`;
        card.querySelector('b').textContent = who;   // never innerHTML — the name is remote input
        wrap.appendChild(card);
        document.body.appendChild(wrap);

        const done = (v) => { wrap.remove(); resolve(v); };
        card.querySelector('[data-allow]').onclick = () => done(true);
        card.querySelector('[data-deny]').onclick = () => done(false);
    });
}

/** Are WE the one sharing a screen right now? */
function amSharing(room) {
    return (room.getLocalTracks?.() ?? []).some(t => t.videoType === 'desktop');
}

export async function start(options = {}) {
    const room = await waitForConference();
    const selfId = room.myUserId();
    const transport = new ConferenceTransport(room, EVENTS, { sid: options.sid });

    let annotator = null;
    let surface = null;
    let sharerId = null;

    const toolbar = new AnnotatorToolbar({
        onRequest: () => annotator?.requestPermission(),
        onTool: t => surface?.setTool(t),
        onUndo: () => surface?.undo(),
        onClear: () => surface?.clearMine(),
    });

    /** Attach to whoever is sharing; detach when they stop. */
    function attach(id) {
        if (id === sharerId) return;
        detach();
        if (!id) {
            // Nobody is sharing: no toolbar at all. Previously this rendered the request button
            // with a "waiting for a share" note, which offered to ask permission to draw on
            // nothing — visible, confusing, and pointing at a screen that did not exist.
            toolbar.setVisible(false);
            return;
        }
        sharerId = id;

        surface = new AnnotationSurface({
            canvas: toolbar.canvas,
            video: findShareVideo() ?? { videoWidth: 0, videoHeight: 0 },
            onStatus: msg => toolbar.setNote(msg),
            send: op => transport.send(id, op),
        });
        surface.setTool(null);

        annotator = new AnnotatorController({
            transport, sharerId: id, selfId, surface,
            onState: st => toolbar.render({
                permission: st.permission,
                color: st.color,
                canErase: st.caps?.includes('erase') ?? true,
            }),
        });

        toolbar.render({ permission: null });
        toolbar.setNote(`${displayName(id)} is sharing.`);
        toolbar.setVisible(true);
    }

    function detach() {
        annotator?.dispose();
        surface?.dispose();
        annotator = null;
        surface = null;
        sharerId = null;
    }

    const displayName = id =>
        room.getParticipants().find(p => p.getId() === id)?.getDisplayName() ?? id;

    /** Who is sharing a desktop track right now, other than us? */
    function currentSharer() {
        for (const p of room.getParticipants()) {
            const tracks = room.getParticipantById?.(p.getId())?.getTracks?.() ?? [];
            for (const t of tracks) {
                if (t.getType?.() === 'video' && t.videoType === 'desktop') return p.getId();
            }
        }
        return null;
    }

    // ── the sharer half, in the same page ───────────────────────────────────────────────────────
    // Whoever is sharing runs a SharerController here, so requests reach a session that can
    // actually receive them and a prompt appears for the person whose screen it is.
    let sharer = null;

    function becomeSharer() {
        if (sharer) return sharer;
        sharer = new SharerController({
            emit: ({ to, op }) => (to ? transport.send(to, op) : transport.broadcast(op)),
            onRequest: async (req) => {
                const allowed = await askToAllow(req);
                allowed ? sharer.approve(req.id) : sharer.reject(req.id);
                notifyHost({ type: 'consent', id: req.id, allowed });
            },
            onState: st => notifyHost({ type: 'state', state: st }),
            admit: ADMIT.ALLOWLIST,
        });
        transport.onOp((s, m) => {
            const r = sharer.handle(s, m);
            // The overlay lives in the Electron main process when we are inside the desktop app;
            // forward every op so it can render. In a plain browser there is no host and this is a
            // no-op (a browser sharer has no desktop overlay — §13).
            notifyHost({ type: 'op', sender: s, msg: m });
            return r;
        });
        syncSharerRoster();
        return sharer;
    }

    function syncSharerRoster() {
        sharer?.syncParticipants([
            { id: selfId, name: room.getLocalDisplayName?.() ?? 'Me', moderator: true },
            ...room.getParticipants().map(p => ({
                id: p.getId(), name: p.getDisplayName(), moderator: false,
            })),
        ]);
    }

    /** Talk to the Electron shell, if we are inside it. Harmless in a plain browser. */
    function notifyHost(msg) {
        try {
            if (window.parent && window.parent !== window) {
                window.parent.postMessage({ __casualAnnotate: msg }, '*');
            }
        } catch { /* cross-origin parent that does not want us; nothing to do */ }
    }

    function syncSharerState() {
        if (amSharing(room)) {
            becomeSharer();
            syncSharerRoster();
        } else if (sharer) {
            sharer.revoke();
            sharer = null;
        }
    }

    // Poll rather than rely on one track event: the events that signal a remote desktop track differ
    // across jitsi-meet versions, and a missed event means a permanently dead button.
    const timer = setInterval(() => {
        attach(currentSharer());
        syncSharerState();
    }, 1500);
    attach(currentSharer());
    syncSharerState();

    window.CasualAnnotateSession = {
        transport, toolbar,
        get annotator() { return annotator; },
        get surface() { return surface; },
        get sharerId() { return sharerId; },
        get sharer() { return sharer; },
        becomeSharer,
        stop() {
            clearInterval(timer);
            detach();
            toolbar.destroy();
            transport.dispose();
        },
    };

    return window.CasualAnnotateSession;
}

// Auto-start when the page loads it. Re-arms if the user leaves and rejoins, so a second meeting
// in the same tab still gets the UI.
async function run() {
    for (;;) {
        try {
            const session = await start();
            // Wait until this conference ends, then arm again for the next one.
            while (window.APP?.conference?._room?.myUserId?.()) {
                await new Promise(r => setTimeout(r, 2000));
            }
            session.stop();
        } catch (e) {
            console.error('[casual-annotate]', e?.message ?? e);
            await new Promise(r => setTimeout(r, 2000));
        }
    }
}
run();
