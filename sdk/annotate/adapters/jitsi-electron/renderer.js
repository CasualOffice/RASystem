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

    const unsubState = bridge.onState(s => opts.onSharerState?.(s));

    /** Push the conference roster down to the overlay, which owns colour assignment. */
    const syncParticipants = () => {
        if (!sharing) return;
        bridge.control({ type: 'participants', participants: transport.participants() });
    };
    for (const evt of [ 'participantJoined', 'participantLeft', 'displayNameChange' ]) {
        api.on(evt, syncParticipants);
    }

    /** Begin annotation for a share we are hosting. */
    async function startSharing(sourceId, { admit = ADMIT.EVERYONE } = {}) {
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

    // Screen-share lifecycle drives annotation. The source id comes from the Electron picker the
    // Jitsi SDK already installs; without one we cannot place the overlay, so we do not pretend to.
    api.on('screenSharingStatusChanged', (e) => {
        if (e?.on) {
            const sourceId = e?.details?.sourceId;
            if (sourceId) startSharing(sourceId);
            else opts.onRefused?.('Annotation needs a screen-share source id.', 'no-source-id');
        } else {
            stopSharing();
        }
    });

    let annotator = null;
    let surface = null;

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
         * Start annotating somebody ELSE's share.
         * @param {{sharerId: string, selfId: string, canvas: HTMLCanvasElement, video: HTMLVideoElement}} o
         */
        annotate(o) {
            this.stopAnnotating();
            surface = new AnnotationSurface({
                canvas: o.canvas,
                video: o.video,
                onStatus: o.onStatus,
                send: op => transport.send(o.sharerId, op),
            });
            annotator = new AnnotatorController({
                transport,
                sharerId: o.sharerId,
                selfId: o.selfId,
                surface,
                onState: o.onState,
            });
            return { annotator, surface };
        },

        stopAnnotating() {
            annotator?.dispose();
            surface?.dispose();
            annotator = null;
            surface = null;
        },

        dispose() {
            this.stopAnnotating();
            unsubOps?.();
            unsubEmit?.();
            unsubState?.();
            for (const evt of [ 'participantJoined', 'participantLeft', 'displayNameChange' ]) {
                api.removeListener?.(evt, syncParticipants);
            }
            transport.dispose();
        },
    };
}
