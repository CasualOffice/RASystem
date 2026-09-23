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

/**
 * The envelope the iframe External API uses (`modules/API/constants.js:24`).
 *
 * This matters more than it looks. `sendEndpointTextMessage` wraps its payload as
 * `{ name: 'endpoint-text-message', text }` and the API only surfaces INCOMING messages carrying
 * that exact name (`API.js:602`). So a `ConferenceTransport` that puts `{ name: 'casual-annotate' }`
 * straight on the bridge is invisible to an `ExternalApiTransport` peer: the bytes arrive and the
 * External API silently drops them for having the wrong name.
 *
 * Both transports therefore speak this envelope, so a browser participant and the Electron app can
 * actually talk to each other. The cost is our JSON nested inside a string field; the alternative is
 * two halves of the same SDK that cannot interoperate.
 */
const TEXT_MESSAGE_NAME = 'endpoint-text-message';

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

            // Accept both shapes: the External API envelope (what we and the iframe API send) and a
            // bare payload, so a deployment where both ends use this transport still works.
            if (payload?.name === TEXT_MESSAGE_NAME) {
                let inner;
                try {
                    inner = JSON.parse(payload.text);
                } catch {
                    return; // somebody else's endpoint text message
                }
                return this._deliver(sender, inner);
            }
            this._deliver(sender, payload);
        };
        this._conf.on(this._evt, this._onMessage);
    }

    _rawSend(to, payload) {
        // Wrapped in the External API's envelope so an iframe-API peer can see it at all.
        this._conf.sendMessage(
            { name: TEXT_MESSAGE_NAME, text: JSON.stringify(payload) },
            to,
            /* sendThroughVideobridge */ true,
        );
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
 * Transport over the iframe External API.
 *
 * `sendEndpointTextMessage` carries a STRING, so ops are JSON-stringified here and parsed on
 * receipt. A malformed or foreign string is dropped silently: this channel is shared with anything
 * else in the room that uses endpoint text messages.
 *
 * **Not used by the jitsi-meet-electron adapter (ADR-107 Decision 8, Decision 10) — despite the
 * name, this is NOT "the jitsi-meet-electron path."** Its receive half (`onMessage` below) depends on
 * `endpointTextMessageReceived`, which has zero callers in jitsi-meet's web app and never fires on
 * web or Electron. This class can therefore SEND but can never RECEIVE, which makes it useless for
 * anything needing a reply — the whole consent loop included. `PostMessageTransport` (below) is what
 * the Electron adapter actually uses. This class is kept for a host that only ever sends (telemetry,
 * say) or a future context where the iframe API's receive path genuinely works.
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
 * Transport over `postMessage` to an in-page relay injected into a cross-origin iframe — the
 * jitsi-meet-electron path (ADR-107 Decision 10).
 *
 * `jitsi-meet-electron` loads an arbitrary operator's jitsi-meet deployment in a real cross-origin
 * `<iframe>`. The host renderer cannot reach into that frame's DOM (browser same-origin policy), and
 * the iframe's own External API can send into the conference but — per Decision 8 —
 * `endpointTextMessageReceived` never fires, so `ExternalApiTransport` was structurally one-way.
 *
 * The fix is not this class; it is `adapters/jitsi-electron/injected-relay.js`, which the Electron
 * MAIN process injects directly into that frame via `webContents.mainFrame`'s privileged
 * `executeJavaScript` (bypassing the same-origin restriction the way only a host application can).
 * That relay runs a real `ConferenceTransport` where `lib-jitsi-meet`'s own event actually fires, and
 * bridges it to the host over `postMessage` — which crosses a cross-origin iframe boundary just fine,
 * unlike direct DOM access. This class is purely the HOST side of that bridge.
 *
 * No envelope is built here. `BaseJitsiTransport.send/broadcast` would assign a `sid`/`seq` on THIS
 * side and the relay's own `ConferenceTransport` would assign a second one on the far side — two
 * envelopes for one op. So this class does not extend `BaseJitsiTransport`; the relay's
 * `ConferenceTransport` is the only place an envelope is built, exactly once, right before the wire.
 *
 * **`e.source` is checked against the exact window this instance was built for.** A first version of
 * this class accepted a `__casualAnnotateWire` message from *any* sender, trusting the payload shape
 * alone — which quietly threw away the attribution guarantee the rest of this file is built around
 * (§7.2: "the sender comes from the relay, never the payload"). `window.addEventListener('message')`
 * fires for a message from *anything* that can reach this `window` — any other frame this Electron
 * renderer ever hosts, not only the one relay it was constructed with — so without this check, any
 * script capable of posting into the host window could forge `{sender: '<any admitted id>', msg:
 * {...}}` and draw/erase/clear as that participant, or forge a `roster` entry with `moderator: true`
 * and unlock the `clear:'all'` authority check in `core/session.js`, with no real message ever having
 * crossed the actual conference. This one check is what makes the class's own claim true.
 */
export class PostMessageTransport {
    /**
     * @param {Window} targetWindow - the iframe's `contentWindow`, where the relay is injected. Also
     *   the ONLY accepted source of incoming messages — see the class doc above.
     * @param {object} [opts]
     * @param {{addEventListener: Function, removeEventListener: Function}} [opts.host] - where
     *   `message` events are listened for. Defaults to the real `window`; injectable so this class is
     *   unit-testable without a DOM (`test/transport.test.js`), matching the rest of this file's
     *   preference for taking its environment as a parameter rather than reaching for a global.
     */
    constructor(targetWindow, { host, targetOrigin = '*' } = {}) {
        this._target = targetWindow;
        // Defaults to '*' — the same "unrestricted" default `postMessage` itself has — since the
        // caller may not always be able to name an exact origin. Pass the real one when it is known
        // (`renderer.js` derives it from the iframe's own `src`): this is a MUCH lower-stakes gap than
        // the incoming `e.source` check above — a wrong-recipient reading this data is not a new leak
        // (every op here is already visible to anyone in the room via lib-jitsi-meet's own events),
        // whereas an unchecked sender forging one in would have been.
        this._targetOrigin = targetOrigin;
        this._host = host ?? (typeof window !== 'undefined' ? window : undefined);
        if (!this._host) {
            throw new Error('casual-annotate: PostMessageTransport needs `window` or an injected `host`');
        }
        this._roster = [];
        /** @type {Set<(sender: string, msg: object) => void>} */
        this._listeners = new Set();
        this._onMessage = (e) => {
            // The identity check that makes attribution real — see the class doc. A test's fake
            // `targetWindow` may not support `===` the way a real `WindowProxy` does across the
            // `postMessage` boundary; tests pass `e.source` back as the literal object they used, so
            // this still holds without a DOM.
            if (e.source !== this._target) return;
            const d = e?.data?.__casualAnnotateWire;
            if (!d) return;
            if (d.type === 'op') {
                for (const fn of this._listeners) fn(d.sender, d.msg);
            } else if (d.type === 'roster') {
                this._roster = d.participants ?? [];
            }
        };
        this._host.addEventListener('message', this._onMessage);
    }

    /** @param {(sender: string, msg: object) => void} fn */
    onOp(fn) {
        this._listeners.add(fn);
        return () => this._listeners.delete(fn);
    }

    send(to, op) {
        if (!to) throw new Error('casual-annotate: send requires a target endpoint (never broadcast ops)');
        this._target?.postMessage({ __casualAnnotateEmit: { to, op } }, this._targetOrigin);
    }

    broadcast(op) {
        this._target?.postMessage({ __casualAnnotateEmit: { to: '', op } }, this._targetOrigin);
    }

    /**
     * The relay has no request/response channel back to us, so it proactively pushes a fresh roster
     * snapshot every couple of seconds (mirroring the polling `standalone/inject.js` already uses,
     * for the same reason: which conference events fire reliably differs across jitsi-meet versions).
     * This returns whatever snapshot arrived most recently.
     */
    participants() {
        return this._roster;
    }

    dispose() {
        this._host.removeEventListener('message', this._onMessage);
        this._listeners.clear();
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
