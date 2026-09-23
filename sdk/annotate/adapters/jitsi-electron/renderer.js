// Casual Annotate — Electron RENDERER wiring for jitsi-meet-electron (ADR-107 §5, §8).
//
// jitsi-meet-electron loads the meeting in an iframe through `JitsiMeetExternalAPI`
// (`app/features/conference/components/Conference.tsx:188`).
//
// ── The transport, and why it is not the iframe API (ADR-107 Decision 8, Decision 10) ────────────
//
// The obvious choice is the External API's `sendEndpointTextMessage` / `endpointTextMessageReceived`.
// It sends fine. It never receives: `endpointTextMessageReceived` has ZERO callers in jitsi-meet's
// web app (only the mobile middleware implements the equivalent), so a request from an annotator
// could reach this app and be silently dropped forever — the defect recorded in Decision 8/9.
//
// The fix runs on the OTHER side of the iframe boundary: `main.js` injects a real
// `ConferenceTransport` directly into the iframe via Electron's privileged `webFrameMain`
// `executeJavaScript` (`injected-relay.js`), where `lib-jitsi-meet`'s own receive event genuinely
// fires, and bridges it out over `postMessage` — which, unlike direct DOM access, crosses a
// cross-origin iframe boundary just fine. `PostMessageTransport` is the host side of that bridge.
//
// This module is a RELAY, not a brain. The authoritative session lives in the overlay window (see
// `main.js`), so everything here either puts bytes on the wire or takes them off it:
//
//     relayed op   ──► forwardOp ──► overlay   (applies + renders)
//     wire         ◄── onEmit  ◄──────── overlay   (acks, roster)
//
// Install beside the SDK's other renderer helpers:
//
//     import { setupAnnotateRender } from '@casualoffice/annotate/adapters/jitsi-electron/renderer.js';
//     const annotate = setupAnnotateRender(this._api);

import { PostMessageTransport } from '../../transport/jitsi.js';
import { AnnotatorController } from '../../annotator.js';
import { AnnotationSurface } from '../../surface/surface.js';
import { AnnotatorToolbar } from '../../surface/toolbar.js';
import { SharerPanel } from '../../surface/sharer-panel.js';
import { ADMIT } from '../../core/session.js';
import { isAnnotateMessage } from '../../core/ops.js';

/** Refusal reasons, in words a user can act on. Shown verbatim, so they must be true. */
export const REFUSAL = Object.freeze({
    'window-share-not-supported':
        'Annotation needs a whole screen to be shared — a single window is not captured with the overlay.',
    'linux-multi-monitor-unsupported':
        'Annotation cannot tell which monitor is being shared on Linux with more than one display.',
    'display-not-resolved':
        'Could not work out which display is being shared.',
});

/**
 * @param {object} api - the JitsiMeetExternalAPI instance.
 * @param {object} [opts]
 * @param {(state: object) => void} [opts.onSharerState]
 * @param {(message: string, reason: string) => void} [opts.onRefused]
 */
export function setupAnnotateRender(api, opts = {}) {
    const bridge = window.casualAnnotate;
    if (!bridge) throw new Error('casual-annotate: preload bridge missing — call installAnnotateBridge()');

    // The frame the relay was injected into (`main.js`). Without it there is nothing to transport
    // ops over, and failing loud here beats a silent "nothing ever arrives" (the exact defect this
    // fix exists to close).
    const iframe = api.getIFrame?.();
    if (!iframe?.contentWindow) {
        throw new Error('casual-annotate: JitsiMeetExternalAPI.getIFrame() unavailable — '
            + 'cannot reach the frame the relay is injected into');
    }
    // `iframe.src` is an ordinary HTML attribute — reading it does not cross the cross-origin
    // boundary the way reading `iframe.contentWindow.location` would. Best-effort: a malformed or
    // relative `src` just falls back to PostMessageTransport's own '*' default.
    let targetOrigin;
    try { targetOrigin = new URL(iframe.src, location.href).origin; } catch { /* fall back to '*' */ }
    const transport = new PostMessageTransport(iframe.contentWindow, { targetOrigin });
    let sharing = false;
    let liveSourceId = null;
    let rosterResyncTimer = null;

    // The iframe API never carries the desktop source id — `screensharingDetails` has only
    // `sourceType` (`actions.web.ts:144`). Main observes the real id from the picker and pushes it
    // here, so this is the only place that knows which display to put the overlay on.
    bridge.onSource(({ sourceId }) => {
        liveSourceId = sourceId;
        debug('source', sourceId);
    });

    /**
     * A visible trace of why annotation is or is not running.
     *
     * Every failure in this path is silent by nature — a missing source id, a refused overlay, a
     * share event that never arrived — and all of them look identical from the outside: no button,
     * no dialog, nothing. This makes the difference inspectable instead of guessable.
     */
    const trace = [];
    function debug(event, detail) {
        trace.push({ t: Date.now(), event, detail });
        if (trace.length > 50) trace.shift();
        window.__casualAnnotate = {
            get sharing() { return sharing; },
            get liveSourceId() { return liveSourceId; },
            get attachedTo() { return attachedTo; },
            get selfId() { return selfId; },
            trace,
        };
    }
    debug('init');

    // ── wire → overlay ──────────────────────────────────────────────────────────────────────────
    // The sender id comes from the relay's `senderInfo`, never from the payload — that is the whole
    // attribution guarantee (§7.2), and it is preserved by passing `sender` through untouched.
    const unsubOps = transport.onOp((sender, msg) => {
        if (!sharing || !isAnnotateMessage(msg)) return;
        bridge.forwardOp(sender, msg);
    });


    // ── overlay → wire ──────────────────────────────────────────────────────────────────────────
    const unsubEmit = bridge.onEmit(({ to, op }) => {
        if (to) transport.send(to, op);
        else transport.broadcast(op);
    });

    // The management panel lives in the HOST document, not the iframe and not the overlay: the
    // overlay must never be interactive (ADR-100), and the iframe is a real cross-origin document we
    // cannot reach into — this is our own layer, same reasoning as the browser toolbar (§5.2).
    const panel = new SharerPanel({
        onMute: id => bridge.control({ type: 'mute', id }),
        onUnmute: id => bridge.control({ type: 'unmute', id }),
        onRevoke: id => bridge.control({ type: 'withdraw', id }),
    });

    let lastPending = new Set();
    const unsubStateReal = bridge.onState((s) => {
        opts.onSharerState?.(s);
        panel.render(s);
        if (s?.pending?.length) debug('pending', s.pending);

        // A newly-pending request → native prompt. Tracked so a state update for any other reason
        // does not re-prompt for someone already being asked about.
        for (const p of s?.pending ?? []) {
            if (lastPending.has(p.id)) continue;
            lastPending.add(p.id);
            bridge.ask({ id: p.id, name: p.name }).then(({ allowed }) => {
                bridge.control({ type: allowed ? 'approve' : 'reject', id: p.id });
                lastPending.delete(p.id);
            }).catch(() => lastPending.delete(p.id));
        }
    });

    /** Push the conference roster down to the overlay, which owns colour assignment. */
    const syncParticipants = () => {
        if (!sharing) return;
        bridge.control({ type: 'participants', participants: transport.participants() });
    };
    for (const evt of [ 'participantJoined', 'participantLeft', 'displayNameChange' ]) {
        api.on(evt, syncParticipants);
    }

    /** Begin annotation for a share we are hosting. */
    async function startSharing(sourceId, { admit = ADMIT.ALLOWLIST } = {}) {
        // Ask the overlay FIRST. If it refuses — a window share, or Linux multi-monitor — annotation
        // stays off and the caller gets a reason. Enabling first and discovering the overlay is
        // invisible would mean people drawing into nothing while the UI says it works.
        debug('startSharing', sourceId);
        const r = await bridge.start(sourceId);
        debug('overlay', r);
        if (!r?.ok) {
            const reason = r?.reason ?? 'display-not-resolved';
            opts.onRefused?.(REFUSAL[reason] ?? 'Annotation is not available for this share.', reason);
            return { ok: false, reason };
        }
        sharing = true;
        // ALLOWLIST, not EVERYONE: with EVERYONE anyone in the room could draw the moment a share
        // starts and the consent prompt would be decoration. Each participant is approved
        // individually, by the person whose screen it is.
        bridge.control({ type: 'admit', mode: admit });
        syncParticipants();
        // One-shot + event-triggered broadcasts can each be lost (the roster round-trip here has
        // 4 more hops than the browser path's single XMPP relay: main<->overlay IPC, the injected
        // relay, a postMessage bridge). A periodic resync — same interval as the browser sharer's
        // own poll in standalone/inject.js — self-heals within a couple of ticks, well inside
        // AnnotatorController's 8s availability window, instead of depending on one lucky delivery.
        rosterResyncTimer = setInterval(syncParticipants, 1500);
        return { ok: true };
    }

    async function stopSharing() {
        if (!sharing) return;
        sharing = false;
        if (rosterResyncTimer !== null) { clearInterval(rosterResyncTimer); rosterResyncTimer = null; }
        bridge.control({ type: 'revoke' });
        await bridge.stop();
    }

    // Screen-share lifecycle drives annotation.
    //
    // `e.details` carries only `sourceType`, never the id — an earlier version of this file read
    // `e.details.sourceId` and so ALWAYS fell into the no-source branch, which meant the overlay
    // never appeared and the feature silently did nothing. The id comes from main instead.
    api.on('screenSharingStatusChanged', (e) => {
        debug('screenSharingStatusChanged', { on: e?.on, haveSource: !!liveSourceId });
        if (e?.on) {
            if (liveSourceId) startSharing(liveSourceId);
            else opts.onRefused?.(
                'Annotation could not determine which screen is being shared.', 'no-source-id');
        } else {
            liveSourceId = null;
            stopSharing();
        }
    });

    let annotator = null;
    let surface = null;
    let toolbar = null;
    let attachedTo = null;
    let selfId = null;

    api.on('videoConferenceJoined', (e) => {
        selfId = e?.id ?? selfId;
    });

    /**
     * Show or hide the annotator UI as remote shares come and go.
     *
     * `contentSharingParticipantsChanged` carries the ids of everyone currently sharing content
     * (`subscriber.web.ts:36`). We attach to the first that is not us — annotating your own screen
     * from your own meeting window is meaningless, since you can just draw on your desktop.
     *
     * This is what makes the option APPEAR on its own. Without it the SDK works but nothing in the
     * UI ever offers it, which is indistinguishable from the feature not existing.
     */
    function syncRemoteShare(ids) {
        const remote = (ids ?? []).filter(id => id && id !== selfId);
        const target = remote[0] ?? null;

        if (target === attachedTo) return;

        if (!target) {
            attachedTo = null;
            publicApi.stopAnnotating();
            return;
        }
        attachedTo = target;
        publicApi.annotate({ sharerId: target, selfId: selfId ?? '' });
    }

    api.on('contentSharingParticipantsChanged', (e) => {
        debug('contentSharingParticipantsChanged', e);
        // The payload has been both a bare array and `{ data }` across versions — accept either
        // rather than silently doing nothing on the shape we did not expect.
        const ids = Array.isArray(e) ? e : (e?.data ?? e?.participantIds ?? []);
        syncRemoteShare(ids);
    });

    const publicApi = {
        transport,
        startSharing,
        stopSharing,
        get isSharing() {
            return sharing;
        },
        mute: id => bridge.control({ type: 'mute', id }),
        unmute: id => bridge.control({ type: 'unmute', id }),
        /** Withdraw a permission already granted — the panel's "Remove" button calls this too. */
        withdraw: id => bridge.control({ type: 'withdraw', id }),
        clearAll: () => bridge.control({ type: 'clear' }),
        setAdmit: mode => bridge.control({ type: 'admit', mode }),

        /**
         * Put the annotator UI on screen for somebody else's share.
         *
         * The toolbar and canvas are OUR layer over the Jitsi iframe, not part of it: the meeting is
         * cross-origin, so no control can be added to its real toolbar from here.
         *
         * @param {{sharerId: string, selfId: string, video?: HTMLVideoElement, parent?: HTMLElement}} o
         */
        annotate(o) {
            this.stopAnnotating();

            toolbar = new AnnotatorToolbar({
                parent: o.parent,
                onRequest: () => annotator?.requestPermission(),
                onTool: t => surface?.setTool(t),
                onUndo: () => surface?.undo(),
                onClear: () => surface?.clearMine(),
            });

            surface = new AnnotationSurface({
                canvas: toolbar.canvas,
                // Without a real <video> element we cannot map coordinates onto the sharer's pixels
                // (§6). The surface refuses to draw and says why rather than guessing.
                video: o.video ?? document.querySelector('video') ?? { videoWidth: 0, videoHeight: 0 },
                onStatus: msg => toolbar.setNote(msg),
                send: op => transport.send(o.sharerId, op),
            });
            surface.setTool(null);
            // NOT shown yet — same reasoning as `standalone/inject.js`'s `attach()`: a remote desktop
            // track only proves someone is sharing, not that their client runs this SDK at all. Stay
            // hidden until `AnnotatorController`'s `sharerAvailable` confirms it via their unprompted
            // roster broadcast, rather than offering a button that can hang forever against a sharer
            // with no SharerController on the other end.
            let shown = false;

            annotator = new AnnotatorController({
                transport,
                sharerId: o.sharerId,
                selfId: o.selfId,
                surface,
                onState: (st) => {
                    if (st.sharerAvailable === false) {
                        toolbar.setVisible(false);
                        o.onState?.(st);
                        return;
                    }
                    if (st.sharerAvailable !== true) { o.onState?.(st); return; }
                    if (!shown) { shown = true; toolbar.setVisible(true); }
                    toolbar.render({
                        permission: st.permission,
                        color: st.color,
                        canErase: st.caps?.includes('erase') ?? true,
                    });
                    o.onState?.(st);
                },
            });
            return { annotator, surface, toolbar };
        },

        stopAnnotating() {
            annotator?.dispose();
            surface?.dispose();
            toolbar?.destroy();
            annotator = null;
            surface = null;
            toolbar = null;
        },

        dispose() {
            this.stopAnnotating();
            attachedTo = null;
            if (rosterResyncTimer !== null) { clearInterval(rosterResyncTimer); rosterResyncTimer = null; }
            unsubOps?.();
            unsubEmit?.();
            unsubStateReal?.();
            panel.destroy();
            for (const evt of [ 'participantJoined', 'participantLeft', 'displayNameChange' ]) {
                api.removeListener?.(evt, syncParticipants);
            }
            transport.dispose();
        },
    };

    return publicApi;
}
