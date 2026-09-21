// Casual Annotate — the annotator controller (ADR-107 §3, §11.2, §18).
//
// Ties the drawing surface to a transport, and owns the two things the surface must not guess:
// which colour we were assigned, and when to let go of the local echo.
//
// The surface is injected as an interface, so this controller is DOM-free and unit-testable. In the
// browser you pass an `AnnotationSurface`; in a test you pass a recorder.

import { hello as helloOp, decode, OP, TOOL } from './core/ops.js';
import { CAPS, LOCAL_CAPS, PeerProfile } from './core/compat.js';
import { StrokeSender, CursorSender } from './transport/jitsi.js';
import { AckTimer } from './latency/beacon.js';

/** Fallback video-lag estimate, used until measurement replaces it (§11.2). */
const DEFAULT_VIDEO_LAG_MS = 600;

/**
 * @typedef {object} SurfaceLike
 * @property {(hex: string) => void} setColor
 * @property {(tool: number|string|null) => void} setTool
 * @property {(id: string, videoLagMs: number) => void} onAck
 * @property {(author: string) => void} [setAuthor]
 */

export class AnnotatorController {
    /**
     * @param {object} opts
     * @param {import('./transport/jitsi.js').ConferenceTransport} opts.transport
     * @param {string} opts.sharerId - the endpoint that renders. Every op is unicast here.
     * @param {string} opts.selfId
     * @param {SurfaceLike} opts.surface
     * @param {(state: object) => void} [opts.onState]
     */
    constructor({ transport, sharerId, selfId, surface, onState }) {
        this.transport = transport;
        this.sharerId = sharerId;
        this.selfId = selfId;
        this.surface = surface;
        this._onState = onState ?? (() => {});

        this.strokes = new StrokeSender(transport, sharerId);
        this.cursors = new CursorSender(transport, sharerId, { hz: 20 });
        this.ackTimer = new AckTimer();

        /** What the SHARER can honour. Baseline until a roster says otherwise (compat R4). */
        this.sharerProfile = new PeerProfile();
        this.color = '#ff3b30';
        this.videoLagMs = DEFAULT_VIDEO_LAG_MS;

        surface.setAuthor?.(selfId);
        this._unsub = transport.onOp((sender, msg) => this._onOp(sender, msg));

        // Announce ourselves. Optional by design — an old client sends nothing and still works —
        // but sending it is what lets the sharer tailor what it expects from us.
        transport.send(sharerId, helloOp([ ...LOCAL_CAPS ]));
    }

    /**
     * Is a feature usable right now? Gate UI on this rather than offering a button whose ops the
     * sharer would silently drop — a dead button is worse than an absent one (compat R4).
     */
    supports(cap) {
        return this.sharerProfile.supports(cap);
    }

    /** Tools this sharer can actually render, for building the toolbar. */
    availableTools() {
        const tools = [ TOOL.PEN, TOOL.HIGHLIGHTER, TOOL.ARROW, TOOL.RECT ];
        return { tools, erase: this.supports(CAPS.ERASE), cursors: this.supports(CAPS.CURSORS) };
    }

    /** Report pointer position, if the sharer renders cursors at all. */
    pointerMoved(normalizedPt) {
        if (!this.supports(CAPS.CURSORS)) return;
        this.cursors.move(normalizedPt);
    }

    /** Called by the surface when a stroke completes, so we can time its ack. */
    strokeEnded(id) {
        this.ackTimer.sent(id);
    }

    _onOp(sender, msg) {
        // Only the sharer's own messages are treated as sharer messages — otherwise any participant
        // could recolour everyone by forging a roster.
        if (sender !== this.sharerId) return;

        const d = decode(msg, { fromSharer: true });
        if (!d.ok) return;

        if (d.op.op === OP.ROSTER) {
            this.sharerProfile = new PeerProfile({ v: d.op.peerVersion, caps: d.op.caps });
            const mine = d.op.colors?.[this.selfId];
            if (mine) {
                this.color = mine;
                this.surface.setColor(mine);
            }
            this._emit(d.op);
            return;
        }

        if (d.op.op === OP.ACK) {
            const rtt = this.ackTimer.acked(d.op.id);
            if (rtt !== null) this.videoLagMs = estimateVideoLag(rtt, this.ackTimer.median);
            this.surface.onAck(d.op.id, this.videoLagMs);
        }
    }

    _emit(roster) {
        this._onState({
            color: this.color,
            caps: [ ...this.sharerProfile.caps ],
            sharerIsLegacy: this.sharerProfile.isLegacy,
            videoLagMs: this.videoLagMs,
            dataRttMs: this.ackTimer.median,
            participants: roster
                ? Object.entries(roster.colors ?? {}).map(([ id, color ]) => ({
                    id, color, name: roster.names?.[id] ?? id,
                }))
                : undefined,
        });
    }

    dispose() {
        this._unsub?.();
        this.cursors.dispose();
    }
}

/**
 * Estimate how long after the ack the mark shows up in the video.
 *
 * This is deliberately crude and deliberately BIASED LONG. The data RTT is the only thing we can
 * measure without the pixel beacon, and the video path is strictly slower than it: capture tick,
 * encode, jitter buffer, decode. Guessing short makes the echo vanish before the video copy lands,
 * which is the exact blink a fixed 1 s fade produced (§11.2) — so when in doubt, linger.
 *
 * Replace this with a measured value from `LatencyProbe` as soon as A2.5 provides one.
 */
export function estimateVideoLag(lastRtt, medianRtt) {
    const rtt = medianRtt ?? lastRtt ?? 0;
    // One-way data time, plus a 5 fps capture tick, plus an encode/jitter/decode constant.
    return Math.round(rtt / 2 + 200 + 400);
}
