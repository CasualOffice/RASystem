// Casual Annotate — Electron RENDERER wiring for jitsi-meet-electron (ADR-107 §5, §8).
//
// jitsi-meet-electron loads the meeting in an iframe through `JitsiMeetExternalAPI`
// (`app/features/conference/components/Conference.tsx:188`), so this uses the External API
// transport — `sendEndpointTextMessage`, which is string-only.
//
// This module is a RELAY, not a brain. The authoritative session lives in the overlay window (see
// `main.js`), so everything here either puts bytes on the wire or takes them off it:
//
//     endpoint message ──► forwardOp ──► overlay   (applies + renders)
//     wire         ◄── onEmit  ◄──────── overlay   (acks, roster)
//
// Install beside the SDK's other renderer helpers:
//
//     import { setupAnnotateRender } from '@casualoffice/annotate/adapters/jitsi-electron/renderer.js';
//     const annotate = setupAnnotateRender(this._api);

import { ExternalApiTransport } from '../../transport/jitsi.js';
import { AnnotatorController } from '../../annotator.js';
import { AnnotationSurface } from '../../surface/surface.js';
import { AnnotatorToolbar } from '../../surface/toolbar.js';
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

    const transport = new ExternalApiTransport(api);
    let sharing = false;
    let liveSourceId = null;

    // The iframe API never carries the desktop source id — `screensharingDetails` has only
    // `sourceType` (`actions.web.ts:144`). Main observes the real id from the picker and pushes it
    // here, so this is the only place that knows which display to put the overlay on.
    bridge.onSource(({ sourceId }) => {
        liveSourceId = sourceId;
    });

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

    let lastPending = new Set();
    const unsubStateReal = bridge.onState((s) => {
        opts.onSharerState?.(s);

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
        const r = await bridge.start(sourceId);
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
        return { ok: true };
    }

    async function stopSharing() {
        if (!sharing) return;
        sharing = false;
        bridge.control({ type: 'revoke' });
        await bridge.stop();
    }

    // Screen-share lifecycle drives annotation.
    //
    // `e.details` carries only `sourceType`, never the id — an earlier version of this file read
    // `e.details.sourceId` and so ALWAYS fell into the no-source branch, which meant the overlay
    // never appeared and the feature silently did nothing. The id comes from main instead.
    api.on('screenSharingStatusChanged', (e) => {
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

    return {
        transport,
        startSharing,
        stopSharing,
        get isSharing() {
            return sharing;
        },
        mute: id => bridge.control({ type: 'mute', id }),
        unmute: id => bridge.control({ type: 'unmute', id }),
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

            annotator = new AnnotatorController({
                transport,
                sharerId: o.sharerId,
                selfId: o.selfId,
                surface,
                onState: (st) => {
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
            unsubOps?.();
            unsubEmit?.();
            unsubStateReal?.();
            for (const evt of [ 'participantJoined', 'participantLeft', 'displayNameChange' ]) {
                api.removeListener?.(evt, syncParticipants);
            }
            transport.dispose();
        },
    };
}
