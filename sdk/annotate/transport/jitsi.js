// Casual Annotate — Jitsi transport adapter (ADR-107 §5).
//
// Verified against lib-jitsi-meet: the bridge channel is an RTCDataChannel OR a WebSocket to JVB
// (`modules/RTC/BridgeChannel.ts:23`) and every send is
// `JSON.stringify({ colibriClass: 'EndpointMessage', msgPayload, to })` (`BridgeChannel.ts:499`).
//
// Two shapes, because the two integration targets differ:
//
//   ConferenceTransport  — the embedder owns a JitsiConference. Payload is a JSON OBJECT.
//                          `conference.sendMessage(payload, to, true)`. Note `sendEndpointMessage`
//                          is @deprecated in favour of it (`JitsiConference.ts:4715`), though
//                          Jitsi's own remote control still calls the old one.
//
//   ExternalApiTransport — the iframe External API, which is what jitsi-meet-electron uses
//                          (`app/features/conference/components/Conference.tsx:188`). Its
//                          `sendEndpointTextMessage` is STRING-only, so we JSON.stringify.
//
// UNICAST, NOT BROADCAST. Only the sharer renders, so the capture path does the fan-out (§2).
// Broadcasting would multiply bridge traffic by the room size for no benefit. This mirrors the
// direction Jitsi's own remote control sends in (`actions.ts:608`, unicast to the controlled peer).

import { envelope, isAnnotateMessage } from '../core/ops.js';

/** Lazily-ish generated session id. Distinguishes two shares in one room. */
function newSessionId() {
    return `cas-${Math.random().toString(36).slice(2, 10)}`;
}

/**
 * Shared behaviour: sequence numbers, envelope construction, and the listener fan-out. Subclasses
 * supply `_rawSend` and wire up their own receive path.
 * @abstract
 */
class BaseJitsiTransport {
    constructor({ sid } = {}) {
        this.sid = sid ?? newSessionId();
        this._seq = 0;
        /** @type {Set<(sender: string, msg: object) => void>} */
        this._listeners = new Set();
    }

    /** @param {(sender: string, msg: object) => void} fn */
    onOp(fn) {
        this._listeners.add(fn);
        return () => this._listeners.delete(fn);
    }

    /**
     * Send one op to one endpoint.
     * @param {string} to - endpoint id. Required: we never broadcast ops.
     * @param {object} op
     */
    send(to, op) {
        if (!to) throw new Error('casual-annotate: send requires a target endpoint (never broadcast ops)');
        this._rawSend(to, envelope(op, { sid: this.sid, seq: this._seq++ }));
    }

    /**
     * Broadcast — reserved for the sharer's `roster`, which is small and infrequent and genuinely
     * needs to reach everyone (§7.1). Deliberately a separate method so an accidental broadcast of
     * stroke traffic is a visible mistake rather than a default.
     */
    broadcast(op) {
        this._rawSend('', envelope(op, { sid: this.sid, seq: this._seq++ }));
    }

    /** Deliver a received payload to listeners, after the cheap "is it ours" filter. */
    _deliver(sender, payload) {
        if (!sender || !isAnnotateMessage(payload)) return;
        for (const fn of this._listeners) fn(sender, payload);
    }

    _rawSend() {
        throw new Error('not implemented');
    }

    dispose() {
        this._listeners.clear();
    }
}

/**
 * Transport over a `JitsiConference` from lib-jitsi-meet. Use when the embedder owns the conference
 * object (a custom app, or jitsi-meet itself).
 */
export class ConferenceTransport extends BaseJitsiTransport {
    /**
     * @param {object} conference - a JitsiConference.
     * @param {object} events - `JitsiMeetJS.events.conference`, for ENDPOINT_MESSAGE_RECEIVED.
     */
    constructor(conference, events, opts = {}) {
        super(opts);
        this._conf = conference;
        this._evt = events.ENDPOINT_MESSAGE_RECEIVED;

        // lib-jitsi-meet hands us the PARTICIPANT, not a claimed id — which is exactly the
        // attribution guarantee §7.2 depends on. We take the id from there, never from the payload.
        this._onMessage = (participant, payload) => {
            const sender = participant?.getId?.() ?? participant?._id;
            this._deliver(sender, payload);
        };
        this._conf.on(this._evt, this._onMessage);
    }

    _rawSend(to, payload) {
        // Object payload — the bridge channel stringifies it itself (`BridgeChannel.ts:499`).
        this._conf.sendMessage(payload, to, /* sendThroughVideobridge */ true);
    }

    /** Endpoint ids currently in the room, for the roster. */
    participants() {
        return (this._conf.getParticipants?.() ?? []).map(p => ({
            id: p.getId(),
            name: p.getDisplayName?.() ?? '',
            moderator: p.isModerator?.() ?? false,
        }));
    }

    dispose() {
        this._conf.off?.(this._evt, this._onMessage);
        super.dispose();
    }
}

/**
 * Transport over the iframe External API — the jitsi-meet-electron path.
 *
 * `sendEndpointTextMessage` carries a STRING, so ops are JSON-stringified here and parsed on
 * receipt. A malformed or foreign string is dropped silently: this channel is shared with anything
 * else in the room that uses endpoint text messages.
 */
export class ExternalApiTransport extends BaseJitsiTransport {
    /** @param {object} api - a JitsiMeetExternalAPI instance. */
    constructor(api, opts = {}) {
        super(opts);
        this._api = api;

        this._onMessage = (e) => {
            // `{ senderInfo: { id, displayName }, eventData: { text } }`
            const sender = e?.senderInfo?.id;
            let payload;
            try {
                payload = JSON.parse(e?.eventData?.text ?? e?.data?.text ?? '');
            } catch {
                return; // not JSON — somebody else's traffic.
            }
            this._deliver(sender, payload);
        };
        this._api.on('endpointTextMessageReceived', this._onMessage);
    }

    _rawSend(to, payload) {
        this._api.executeCommand('sendEndpointTextMessage', to, JSON.stringify(payload));
    }

    participants() {
        return (this._api.getParticipantsInfo?.() ?? []).map(p => ({
            id: p.participantId,
            name: p.displayName ?? p.formattedDisplayName ?? '',
            moderator: p.role === 'moderator',
        }));
    }

    dispose() {
        this._api.removeListener?.('endpointTextMessageReceived', this._onMessage);
        super.dispose();
    }
}

/**
 * Stroke sender with the §5 repair: points are batched into small `append`s while drawing, and the
 * completed stroke is resent ONCE on `end`.
 *
 * The resend is idempotent — the store keys on stroke id and a replayed `begin` does not reset an
 * existing stroke (`core/store.js`) — so a duplicate costs nothing while a dropped mid-stroke
 * `append` self-heals. That is the whole reliability story: no acks-per-append, no retransmit
 * windows, no ordering requirement.
 */
export class StrokeSender {
    /**
     * @param {BaseJitsiTransport} transport
     * @param {string} sharerId
     * @param {object} [opts]
     * @param {number} [opts.batchPoints] - points per `append`.
     */
    constructor(transport, sharerId, opts = {}) {
        this._t = transport;
        this._to = sharerId;
        this._batch = Math.max(1, Math.min(opts.batchPoints ?? 24, 64));
        this._pending = new Map(); // id → all points, for the repair resend
    }

    begin(id, tool) {
        this._pending.set(id, []);
        this._t.send(this._to, { op: 'begin', id, tool });
    }

    /** Buffer points and flush whenever a batch is full. Returns true if anything was sent. */
    points(id, pts) {
        const all = this._pending.get(id);
        if (!all) return false;
        let sent = false;
        for (const p of pts) {
            all.push(p);
            if (all.length % this._batch === 0) {
                const at = all.length - this._batch;
                this._t.send(this._to, { op: 'append', id, at, pts: all.slice(at) });
                sent = true;
            }
        }
        return sent;
    }

    /** Flush the tail, end the stroke, then resend the whole thing once as the repair. */
    end(id) {
        const all = this._pending.get(id);
        if (!all) return;
        const tail = all.length % this._batch;
        if (tail > 0) {
            const at = all.length - tail;
            this._t.send(this._to, { op: 'append', id, at, pts: all.slice(at) });
        }
        this._t.send(this._to, { op: 'end', id });
        this._pending.delete(id);
    }

    /**
     * The repair: replay a completed stroke in full. Call once after `end`, ideally on the next
     * tick so it does not compete with the live traffic it is insuring.
     * @param {{id: string, tool: number, pts: Array<[number,number]>}} stroke
     */
    repair(stroke) {
        const { id, tool, pts } = stroke;
        this._t.send(this._to, { op: 'begin', id, tool });
        for (let i = 0; i < pts.length; i += this._batch) {
            this._t.send(this._to, { op: 'append', id, at: i, pts: pts.slice(i, i + this._batch) });
        }
        this._t.send(this._to, { op: 'end', id });
    }
}

/**
 * Cursor sender: throttled, coalesced, never retried (§5). Losing a cursor update is free — the
 * next one supersedes it — so this deliberately has no reliability machinery at all.
 */
export class CursorSender {
    constructor(transport, sharerId, { hz = 20 } = {}) {
        this._t = transport;
        this._to = sharerId;
        this._minGap = 1000 / hz;
        this._last = -Infinity; // the first move always sends, whatever the caller's clock origin
        this._queued = null;
        this._timer = null;
    }

    /** @param {[number, number]} pt normalized */
    move(pt, now = Date.now()) {
        this._queued = pt;
        const since = now - this._last;
        if (since >= this._minGap) return this._flush(now);
        if (this._timer === null) {
            this._timer = setTimeout(() => {
                this._timer = null;
                this._flush(Date.now());
            }, this._minGap - since);
        }
    }

    _flush(now) {
        if (!this._queued) return;
        const [ x, y ] = this._queued;
        this._queued = null;
        this._last = now;
        this._t.send(this._to, { op: 'cursor', x, y });
    }

    dispose() {
        if (this._timer !== null) clearTimeout(this._timer);
        this._timer = null;
    }
}
