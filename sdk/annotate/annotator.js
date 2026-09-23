// Casual Annotate — the annotator controller (ADR-107 §3, §11.2, §18).
//
// Ties the drawing surface to a transport, and owns the two things the surface must not guess:
// which colour we were assigned, and when to let go of the local echo.
//
// The surface is injected as an interface, so this controller is DOM-free and unit-testable. In the
// browser you pass an `AnnotationSurface`; in a test you pass a recorder.

import { hello as helloOp, request as requestOp, decode, OP, TOOL } from './core/ops.js';
import { CAPS, LOCAL_CAPS, PeerProfile } from './core/compat.js';
import { StrokeSender, CursorSender } from './transport/jitsi.js';
import { AckTimer } from './latency/beacon.js';

/** Fallback video-lag estimate, used until measurement replaces it (§11.2). */
const DEFAULT_VIDEO_LAG_MS = 600;

/**
 * How long to wait for the sharer's unprompted `roster` broadcast before concluding annotation is
 * not available at all, rather than leaving "Request to annotate" offered forever.
 *
 * The sharer's own `becomeSharer()`/`startSharing()` broadcasts a roster immediately — but "someone
 * is sharing a desktop track" (which is all `currentSharer()` polling can see) says nothing about
 * whether THEIR client is even running this SDK at all. A vanilla jitsi-meet tab, an old build with
 * no annotate script, or a desktop app that never had the adapter installed all look identical from
 * here: a normal screen share, no roster ever coming. Before this, the toolbar showed a working-
 * looking "Request to annotate" button regardless, and clicking it just sat in "Waiting for
 * approval…" forever — indistinguishable from a slow human, not a feature that was never there.
 */
const AVAILABILITY_TIMEOUT_MS = 8_000;

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
     * @param {number} [opts.availabilityTimeoutMs] - overridable for tests; production callers
     *   should never need to pass this.
     */
    constructor({ transport, sharerId, selfId, surface, onState, availabilityTimeoutMs = AVAILABILITY_TIMEOUT_MS }) {
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
        /** `null` = never asked · 'pending' · 'granted' · 'denied'. Drives the annotator's UI. */
        this.permission = null;
        /**
         * True once the sharer's `SharerController` has actually proven itself alive by broadcasting
         * a `roster` — see `AVAILABILITY_TIMEOUT_MS` above. `null` while still waiting, `false` once
         * the wait has timed out with nothing heard.
         */
        this.sharerAvailable = null;
        this._availabilityTimer = availabilityTimeoutMs > 0 ? setTimeout(() => {
            this._availabilityTimer = null;
            if (this.sharerAvailable === null) {
                this.sharerAvailable = false;
                this._emit();
            }
        }, availabilityTimeoutMs) : null;

        surface.setAuthor?.(selfId);
        this._unsub = transport.onOp((sender, msg) => this._onOp(sender, msg));

        // A synchronous send can throw (e.g. the JVB bridge channel isn't open yet, right after
        // join) — must not abort the constructor, or the caller is left with a half-built object.
        try {
            transport.send(sharerId, helloOp([ ...LOCAL_CAPS ]));
        } catch { /* self-heals: sharerAvailable's own timeout/retry paths still apply */ }
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

    /**
     * Ask the sharer for permission to draw on their screen.
     *
     * Returns nothing useful on purpose: the answer arrives asynchronously as `grant` or `deny`,
     * because a human has to look at a dialog first. Watch `permission` via `onState`.
     */
    requestPermission() {
        this.permission = 'pending';
        this.transport.send(this.sharerId, requestOp());
        this._emit();
    }

    /** True once the sharer's own user has said yes. Gate the drawing UI on this, not on hope. */
    get canDraw() {
        return this.permission === 'granted';
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

        // ANY genuine, decoded op from the sharer — not just the roster it broadcasts unprompted —
        // is proof its `SharerController` is actually alive. Checked here, once, rather than only on
        // `roster`, so a grant/deny/ack arriving through some other ordering still counts; the
        // common case is still the roster, since that is what arrives before anyone has asked.
        if (this.sharerAvailable !== true) {
            this.sharerAvailable = true;
            if (this._availabilityTimer !== null) {
                clearTimeout(this._availabilityTimer);
                this._availabilityTimer = null;
            }
        }

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

        if (d.op.op === OP.GRANT) {
            this.permission = 'granted';
            this._emit();
            return;
        }
        if (d.op.op === OP.DENY || d.op.op === OP.REVOKE) {
            this.permission = d.op.op === OP.DENY ? 'denied' : 'revoked';
            // Put the pen down. Re-rendering the toolbar hides the tools, but the canvas would keep
            // accepting pointer events and emitting ops that the sharer now refuses — the user
            // would appear to be drawing into a void.
            this.surface.setTool?.(null);
            this._emit();
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
            permission: this.permission,
            canDraw: this.canDraw,
            color: this.color,
            caps: [ ...this.sharerProfile.caps ],
            sharerIsLegacy: this.sharerProfile.isLegacy,
            videoLagMs: this.videoLagMs,
            dataRttMs: this.ackTimer.median,
            // `null` = still waiting to hear from the sharer at all · `true` = confirmed alive ·
            // `false` = waited AVAILABILITY_TIMEOUT_MS and heard nothing. Gate the "Request to
            // annotate" button on this, not on "someone is sharing a desktop track" — that alone
            // says nothing about whether their client is running this SDK at all.
            sharerAvailable: this.sharerAvailable,
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
        if (this._availabilityTimer !== null) {
            clearTimeout(this._availabilityTimer);
            this._availabilityTimer = null;
        }
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
