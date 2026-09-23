// Casual Annotate — the op model and its fail-closed codec (ADR-107 §4).
//
// This module is PURE: no DOM, no transport, no I/O. Everything here is either a constant, a
// validator, or a builder, so it can be unit-tested directly and reused unchanged by the browser
// surface, the Electron overlay, and (later) a Rust mirror in `ras-protocol`.
//
// Two rules carry over verbatim from the RAS wire (`crates/ras-protocol/src/lib.rs`) and are the
// reason this file exists at all:
//
//   1. DECODE IS FAIL-CLOSED. An unknown `op` or `tool` tag is REJECTED, never clamped to a default.
//      A hostile or buggy peer must not be able to get *something* rendered by sending garbage.
//   2. EVERY COLLECTION IS BOUNDED. A peer cannot force an unbounded allocation on the sharer.
//
// Colour is deliberately NOT on the wire (ADR-107 §4). The sharer derives a stroke's colour from its
// author, so a participant cannot draw in someone else's colour — they never get to state one.

import { versionOk, PROTOCOL_VERSION } from './compat.js';

export { PROTOCOL_VERSION };

/**
 * Envelope discriminator. Mirrors Jitsi's own convention for this class of feature — remote control
 * tags every endpoint message with `name` (`react/features/remote-control/functions.ts:43`) — so a
 * room carrying both features demultiplexes cleanly.
 */
export const MESSAGE_NAME = 'casual-annotate';

/**
 * Freehand tool tags. These are the RAS values EXACTLY (`AnnotTool` in `ras-protocol`), so the
 * geometry ported from `app/ui/overlay.js:80` needs no translation and the deferred iroh transport
 * stays wire-compatible.
 * @readonly
 */
export const TOOL = Object.freeze({ PEN: 0, HIGHLIGHTER: 1, ARROW: 2, RECT: 3 });

/** The set of valid tool tags, for fail-closed decode. */
const VALID_TOOLS = new Set([ TOOL.PEN, TOOL.HIGHLIGHTER, TOOL.ARROW, TOOL.RECT ]);

/**
 * Coordinates are normalized to `0..=COORD_MAX` over the shared surface's CONTENT rect (not its
 * element rect — see `core/geometry.js`). Same space and same width as the RAS wire.
 */
export const COORD_MAX = 65535;

/**
 * DoS bounds. The first two are RAS's verbatim (`MAX_ANNOT_POINTS`, and the 256 the host overlay
 * retains at `overlay.js:63`); the rest are new, because splitting a stroke into begin/append/end
 * opens surfaces a single completed-stroke message did not have.
 */
export const LIMITS = Object.freeze({
    /** Points in one stroke, total. RAS: `MAX_ANNOT_POINTS`. */
    MAX_STROKE_POINTS: 1024,
    /** Strokes the sharer retains before dropping the oldest. RAS: `MAX_ANNOT_STROKES`. */
    MAX_STROKES: 256,
    /** Points in a single `append` — keeps each relay message small and the ink live. */
    MAX_APPEND_POINTS: 64,
    /** Ids in a single `erase`. */
    MAX_ERASE_IDS: 64,
    /** Un-ended strokes one author may have open at once. */
    MAX_OPEN_STROKES_PER_AUTHOR: 32,
    /** Bytes of an author-supplied stroke id. */
    MAX_ID_BYTES: 128,
    /** Ops per second per author, before the sharer starts dropping. */
    MAX_OPS_PER_SEC_PER_AUTHOR: 40,
});

/** Op tags. @readonly */
export const OP = Object.freeze({
    BEGIN: 'begin',
    APPEND: 'append',
    END: 'end',
    UNDO: 'undo',
    ERASE: 'erase',
    CLEAR: 'clear',
    CURSOR: 'cursor',
    ACK: 'ack',
    ROSTER: 'roster',
    /** Optional capability announcement. Absent ⇒ the peer is old; assume the baseline, never fail. */
    HELLO: 'hello',
    /** Annotator → sharer: "may I draw on your screen?" (ADR-107 §9). */
    REQUEST: 'request',
    /** Sharer → annotator: the answer. Always sent, so a denial is never silence. */
    GRANT: 'grant',
    DENY: 'deny',
    /** Sharer → annotator: permission withdrawn mid-session. */
    REVOKE: 'revoke',
});

/** Ops an ANNOTATOR may send to the sharer. Anything else from an annotator is rejected. */
const ANNOTATOR_OPS = new Set([
    OP.BEGIN, OP.APPEND, OP.END, OP.UNDO, OP.ERASE, OP.CLEAR, OP.CURSOR, OP.HELLO, OP.REQUEST,
]);

/** Ops the SHARER may send back. Broadcast, small, infrequent. */
const SHARER_OPS = new Set([ OP.ACK, OP.ROSTER, OP.HELLO, OP.GRANT, OP.DENY, OP.REVOKE ]);

/** `clear` scopes. `all` is honoured only from the sharer or a moderator (enforced by the caller). */
export const CLEAR_SCOPE = Object.freeze({ MINE: 'mine', ALL: 'all' });

// ── helpers ─────────────────────────────────────────────────────────────────────────────────────

/** A coordinate is an integer in `0..=COORD_MAX`. Anything else (NaN, float, negative) is invalid. */
function isCoord(n) {
    return Number.isInteger(n) && n >= 0 && n <= COORD_MAX;
}

/** A point is exactly `[x, y]`, both coordinates. */
function isPoint(p) {
    return Array.isArray(p) && p.length === 2 && isCoord(p[0]) && isCoord(p[1]);
}

/** A stroke id is a short, non-empty string. Its CONTENT is never trusted — see `decode`. */
function isStrokeId(id) {
    return typeof id === 'string' && id.length > 0 && id.length <= LIMITS.MAX_ID_BYTES;
}

/**
 * Mint a stroke id. The author prefix is a convenience for debugging ONLY — the sharer always
 * attributes by the relay-reported sender and never parses an id to decide ownership (ADR-107 §4).
 * @param {string} author
 * @param {number} n
 */
export function strokeId(author, n) {
    return `${author}:${n}`;
}

// ── builders (annotator side) ───────────────────────────────────────────────────────────────────

/** @returns {object} a `begin` op. */
export function begin(id, tool) {
    return { op: OP.BEGIN, id, tool };
}

/**
 * @returns {object} an `append` op carrying at most `MAX_APPEND_POINTS` points.
 *
 * `at` is the index of the FIRST point within the stroke. Carrying the position — rather than
 * implying it from arrival order — is what makes the whole op stream order-independent: a duplicate
 * append rewrites the same slots (idempotent), a reordered one still lands correctly, and a lost one
 * leaves a hole that the §5 repair fills. Without it, `append` would silently depend on the relay
 * preserving order, which it does not promise.
 */
export function append(id, at, pts) {
    return { op: OP.APPEND, id, at, pts };
}

/** @returns {object} an `end` op. */
export function end(id) {
    return { op: OP.END, id };
}

/** @returns {object} an `undo` op — the sender's own most recent stroke. Carries no id by design. */
export function undo() {
    return { op: OP.UNDO };
}

/** @returns {object} an `erase` op. Ids are hit-tested locally by the author; the sharer re-filters. */
export function erase(ids) {
    return { op: OP.ERASE, ids };
}

/** @returns {object} a `clear` op. */
export function clear(scope = CLEAR_SCOPE.MINE) {
    return { op: OP.CLEAR, scope };
}

/** @returns {object} a `cursor` op. Lossy by design — throttled, coalesced, never retried. */
export function cursor(x, y) {
    return { op: OP.CURSOR, x, y };
}

// ── builders (sharer side) ──────────────────────────────────────────────────────────────────────

/**
 * Acknowledge a completed stroke. This is what lets the annotator hold its local echo for a real
 * signal instead of a guessed timer (ADR-107 §11.2) — `t` is the sharer's clock at render time.
 */
export function ack(id, t) {
    return { op: OP.ACK, id, t };
}

/**
 * Broadcast the authoritative colour + name map. The one place the sharer talks back (§7.1).
 * `caps`/`v` ride along so annotators can gate their UI on what this sharer actually honours —
 * both optional, because an old sharer sends neither and must keep working (compat R2/R4).
 */
export function roster(colors, names, caps, v) {
    const op = { op: OP.ROSTER, colors, names };
    if (caps) op.caps = caps;
    if (v !== undefined) op.v = v;
    return op;
}

/**
 * Ask the sharer for permission to draw on their screen.
 *
 * Deliberately carries nothing but the op. The requester's identity and display name come from the
 * relay and the sharer's own roster — a request that carried its own name would let anyone raise a
 * consent prompt that says whatever they like, which is the oldest trick in the consent-dialog book.
 */
export function request() {
    return { op: OP.REQUEST };
}

/** Sharer → annotator: permission granted. */
export function grant() {
    return { op: OP.GRANT };
}

/**
 * Sharer → annotator: refused.
 *
 * Sent explicitly rather than by staying silent, so the requester's UI can say "declined" instead of
 * spinning forever — and so a denial is distinguishable from a dropped message.
 */
export function deny() {
    return { op: OP.DENY };
}

/** Sharer → annotator: permission withdrawn. */
export function revoke() {
    return { op: OP.REVOKE };
}

/**
 * Announce our version and capabilities. Optional in BOTH directions: a peer that never sends one
 * is treated as the baseline, never as an error. This is what lets a years-old mobile build keep
 * drawing against a current sharer (compat R2).
 */
export function hello(caps, v = PROTOCOL_VERSION) {
    return { op: OP.HELLO, caps, hv: v };
}

// ── envelope ────────────────────────────────────────────────────────────────────────────────────

/**
 * Wrap an op for the wire. Note what is NOT here: no `from`. The author is the relay-reported
 * sender, always — a `from` field would be a spoofing surface and nothing more (ADR-107 §7.2).
 * @param {object} op
 * @param {{ sid: string, seq: number, t?: number }} ctx
 */
export function envelope(op, ctx) {
    return {
        name: MESSAGE_NAME,
        v: PROTOCOL_VERSION,
        sid: ctx.sid,
        seq: ctx.seq,
        t: ctx.t ?? Date.now(),
        ...op,
    };
}

/** Cheap pre-filter: is this endpoint message ours at all? */
export function isAnnotateMessage(msg) {
    return !!msg && typeof msg === 'object' && msg.name === MESSAGE_NAME;
}

// ── decode (fail-closed) ────────────────────────────────────────────────────────────────────────

/**
 * Validate and normalize one received message into a trusted op, or reject it.
 *
 * FAIL-CLOSED: every unknown tag, malformed field, or out-of-bounds collection returns
 * `{ ok: false, reason }`. Nothing is clamped into validity, and nothing partially-valid is let
 * through — a message either decodes completely or not at all.
 *
 * @param {unknown} msg - the raw payload off the relay.
 * @param {{ fromSharer?: boolean }} [opts] - `true` when the sender is the sharer, which selects the
 *   sharer op set. Callers determine this from the relay's sender id, never from the payload.
 * @returns {{ ok: true, op: object } | { ok: false, reason: string }}
 */
export function decode(msg, opts = {}) {
    if (!isAnnotateMessage(msg)) return skip('not-an-annotate-message');

    // A VERSION RANGE, not an equality check. A peer below the floor is a real incompatibility; a
    // peer ABOVE our ceiling is simply newer than us, which is not a fault and must not be logged
    // or counted as one (compat R1).
    const ver = versionOk(msg.v);
    if (!ver.ok) return ver.kind === 'ignore' ? skip(ver.reason) : fail(ver.reason);

    if (typeof msg.sid !== 'string' || !msg.sid) return fail('bad-sid');

    const allowed = opts.fromSharer ? SHARER_OPS : ANNOTATOR_OPS;
    // An unrecognized op tag is IGNORED, not rejected — it is most likely a newer peer's feature,
    // and refusing to render something we do not understand is the correct, safe degradation
    // (compat R3). Malformed KNOWN ops below are still hard rejections: that is the security
    // property, and it is a different thing from version skew.
    if (typeof msg.op !== 'string' || !allowed.has(msg.op)) return skip(`unknown-op:${String(msg.op)}`);

    switch (msg.op) {
        case OP.BEGIN: {
            if (!isStrokeId(msg.id)) return fail('bad-id');
            // Fail-closed on the tool tag: an unknown tool is rejected, NOT defaulted to pen.
            if (!VALID_TOOLS.has(msg.tool)) return fail(`unknown-tool:${String(msg.tool)}`);
            return okOp({ op: OP.BEGIN, id: msg.id, tool: msg.tool });
        }
        case OP.APPEND: {
            if (!isStrokeId(msg.id)) return fail('bad-id');
            if (!Number.isInteger(msg.at) || msg.at < 0 || msg.at >= LIMITS.MAX_STROKE_POINTS) {
                return fail('bad-index');
            }
            if (!Array.isArray(msg.pts) || msg.pts.length === 0) return fail('empty-append');
            if (msg.pts.length > LIMITS.MAX_APPEND_POINTS) return fail('append-too-long');
            if (!msg.pts.every(isPoint)) return fail('bad-point');
            // Copy the points — never retain caller/JSON-parser-owned arrays in the store.
            return okOp({ op: OP.APPEND, id: msg.id, at: msg.at, pts: msg.pts.map(p => [ p[0], p[1] ]) });
        }
        case OP.END: {
            if (!isStrokeId(msg.id)) return fail('bad-id');
            return okOp({ op: OP.END, id: msg.id });
        }
        case OP.UNDO:
            return okOp({ op: OP.UNDO });
        case OP.REQUEST:
            return okOp({ op: OP.REQUEST });
        case OP.GRANT:
            return okOp({ op: OP.GRANT });
        case OP.DENY:
            return okOp({ op: OP.DENY });
        case OP.REVOKE:
            return okOp({ op: OP.REVOKE });
        case OP.ERASE: {
            if (!Array.isArray(msg.ids) || msg.ids.length === 0) return fail('empty-erase');
            if (msg.ids.length > LIMITS.MAX_ERASE_IDS) return fail('erase-too-long');
            if (!msg.ids.every(isStrokeId)) return fail('bad-id');
            return okOp({ op: OP.ERASE, ids: msg.ids.slice() });
        }
        case OP.CLEAR: {
            if (msg.scope !== CLEAR_SCOPE.MINE && msg.scope !== CLEAR_SCOPE.ALL) {
                return fail(`unknown-scope:${String(msg.scope)}`);
            }
            return okOp({ op: OP.CLEAR, scope: msg.scope });
        }
        case OP.CURSOR: {
            if (!isCoord(msg.x) || !isCoord(msg.y)) return fail('bad-coord');
            return okOp({ op: OP.CURSOR, x: msg.x, y: msg.y });
        }
        case OP.ACK: {
            if (!isStrokeId(msg.id)) return fail('bad-id');
            if (!Number.isFinite(msg.t)) return fail('bad-time');
            return okOp({ op: OP.ACK, id: msg.id, t: msg.t });
        }
        case OP.ROSTER: {
            if (!isPlainObject(msg.colors) || !isPlainObject(msg.names)) return fail('bad-roster');
            const out = { op: OP.ROSTER, colors: { ...msg.colors }, names: { ...msg.names } };
            // Optional, and absent from every old sender — so a missing/garbage `caps` degrades to
            // "unknown, assume baseline" rather than failing the whole roster (compat R2).
            if (Array.isArray(msg.caps)) out.caps = msg.caps.filter(c => typeof c === 'string');
            if (Number.isInteger(msg.v)) out.peerVersion = msg.v;
            return okOp(out);
        }
        case OP.HELLO: {
            // Deliberately permissive: every field is optional and anything malformed degrades to
            // the baseline. A `hello` must never be the reason a peer cannot draw.
            return okOp({
                op: OP.HELLO,
                caps: Array.isArray(msg.caps) ? msg.caps.filter(c => typeof c === 'string') : [],
                hv: Number.isInteger(msg.hv) ? msg.hv : MIN_VERSION_FALLBACK,
            });
        }
        default:
            // Unreachable — `allowed` already gated it. Kept so a future op added to the tag sets
            // without a case here degrades safely instead of silently falling through.
            return skip(`unhandled-op:${msg.op}`);
    }
}

function isPlainObject(v) {
    return !!v && typeof v === 'object' && !Array.isArray(v);
}

/** The version assumed for a peer that did not state one. */
const MIN_VERSION_FALLBACK = 1;

/**
 * A hard rejection: malformed, out of bounds, or genuinely too old. Worth surfacing — it means
 * something is wrong, not merely different.
 */
function fail(reason) {
    return { ok: false, kind: 'reject', reason };
}

/**
 * A benign skip: not ours, or from a peer newer than us. Nothing is wrong; we just cannot use it.
 * Callers must not log these as errors or count them toward any abuse threshold.
 */
function skip(reason) {
    return { ok: false, kind: 'ignore', reason };
}

function okOp(op) {
    return { ok: true, op };
}
