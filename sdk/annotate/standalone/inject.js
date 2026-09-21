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

/** Wait for jitsi-meet to have a live conference; it is not ready at script-load time. */
async function waitForConference(timeoutMs = 30000) {
    const started = Date.now();
    while (Date.now() - started < timeoutMs) {
        const room = window.APP?.conference?._room;
        if (room?.myUserId?.()) return room;
        await new Promise(r => setTimeout(r, 400));
    }
    throw new Error('casual-annotate: no Jitsi conference on this page');
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

    // Poll rather than rely on one track event: the events that signal a remote desktop track differ
    // across jitsi-meet versions, and a missed event means a permanently dead button.
    const timer = setInterval(() => attach(currentSharer()), 1500);
    attach(currentSharer());

    window.CasualAnnotateSession = {
        transport, toolbar,
        get annotator() { return annotator; },
        get surface() { return surface; },
        get sharerId() { return sharerId; },
        /** Act as the SHARER instead — for a browser that is the one sharing. */
        becomeSharer(opts = {}) {
            const sharer = new SharerController({
                emit: ({ to, op }) => (to ? transport.send(to, op) : transport.broadcast(op)),
                onRequest: async (req) => {
                    const ok = window.confirm(
                        `${req.name} wants to draw on your shared screen.\n\n`
                        + 'They can draw marks only — they cannot click, type or control anything.');
                    ok ? sharer.approve(req.id) : sharer.reject(req.id);
                },
                admit: ADMIT.ALLOWLIST,
                ...opts,
            });
            transport.onOp((s, m) => sharer.handle(s, m));
            sharer.syncParticipants([
                { id: selfId, name: 'Me', moderator: true },
                ...room.getParticipants().map(p => ({
                    id: p.getId(), name: p.getDisplayName(), moderator: false,
                })),
            ]);
            window.CasualAnnotateSession.sharer = sharer;
            return sharer;
        },
        stop() {
            clearInterval(timer);
            detach();
            toolbar.destroy();
            transport.dispose();
        },
    };

    return window.CasualAnnotateSession;
}

// Auto-start when injected directly.
start().catch(e => console.error('[casual-annotate]', e.message));
