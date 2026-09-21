// Casual Annotate — the sharer's session (ADR-107 §9, §10).
//
// This is the policy layer that sits between a transport and the store. It is where every decision
// the store cannot make on its own gets made: who is allowed to draw, whether a `clear:"all"` is
// honoured, and how fast one author may send.
//
// PURE: no DOM, no transport. It receives `(sender, rawMessage)` and emits effects the caller
// performs. That keeps the security-relevant logic testable without a browser or a conference.
//
// Authority model (Inv 1 — the local user is the final authority): the SHARER decides. Annotation is
// off until they enable it; they can mute an individual, clear everything, or revoke outright. Any
// participant in the room can *send* — the room is not a trust boundary — so the sharer decides who
// is *honoured*.

import { decode, OP, CLEAR_SCOPE, LIMITS, isAnnotateMessage } from './ops.js';
import { PeerProfile, LOCAL_CAPS, PROTOCOL_VERSION } from './compat.js';
import { StrokeStore } from './store.js';
import { PaletteAssigner } from './palette.js';

/** Who may annotate. The sharer picks one; `NONE` is the default until they opt in. */
export const ADMIT = Object.freeze({
    NONE: 'none',
    EVERYONE: 'everyone',
    MODERATORS: 'moderators',
    ALLOWLIST: 'allowlist',
});

/**
 * A simple token bucket per author. Bounds a chatty or hostile peer without needing a clock of its
 * own — callers pass `now`, so this stays deterministic under test.
 */
class RateLimiter {
    constructor(perSec = LIMITS.MAX_OPS_PER_SEC_PER_AUTHOR) {
        this._perSec = perSec;
        this._buckets = new Map();
    }

    /** @returns {boolean} true if the op is within budget. */
    allow(author, now) {
        let b = this._buckets.get(author);
        if (!b) {
            b = { tokens: this._perSec, last: now };
            this._buckets.set(author, b);
        }
        const elapsed = Math.max(0, now - b.last) / 1000;
        b.tokens = Math.min(this._perSec, b.tokens + elapsed * this._perSec);
        b.last = now;
        if (b.tokens < 1) return false;
        b.tokens -= 1;
        return true;
    }

    forget(author) {
        this._buckets.delete(author);
    }
}

export class SharerSession {
    /**
     * @param {object} [opts]
     * @param {string} [opts.admit] - an `ADMIT` mode. Defaults to `NONE` — annotation is OFF until
     *   the sharer explicitly enables it (ADR-107 §9).
     * @param {(author: string) => boolean} [opts.isModerator]
     */
    constructor(opts = {}) {
        this.store = new StrokeStore();
        this.palette = new PaletteAssigner();
        this._admit = opts.admit ?? ADMIT.NONE;
        this._isModerator = opts.isModerator ?? (() => false);
        this._allow = new Set();
        this._muted = new Set();
        this._limiter = new RateLimiter();
        /** @type {Map<string, {x: number, y: number, t: number}>} author → live cursor. */
        this.cursors = new Map();
        /**
         * @type {Set<string>} authors whose permission request is awaiting a human answer.
         * A pending request grants NOTHING — it only means a prompt is open (Inv 1).
         */
        this.pending = new Set();
        /** @type {Map<string, string>} author → display name, from the conference roster. */
        this._names = new Map();
        /**
         * @type {Map<string, PeerProfile>} author → what that client can do.
         * A participant we have never heard a `hello` from is assumed BASELINE, not broken — that
         * assumption is what keeps an old mobile build drawing (compat R2).
         */
        this._peers = new Map();
    }

    // ── sharer controls ─────────────────────────────────────────────────────────────────────────

    /** Enable annotation. Until this is called, every op is refused. */
    setAdmit(mode) {
        this._admit = mode;
        if (mode === ADMIT.NONE) this.cursors.clear();
    }

    get admit() {
        return this._admit;
    }

    /**
     * Grant one participant permission to draw.
     *
     * Only ever called from a real human decision in the host app. There is deliberately no code
     * path from a `request` op to this method: a request opens a prompt, and nothing else.
     */
    allowParticipant(author) {
        this._allow.add(author);
        this.pending.delete(author);
    }

    /** Refuse a pending request, or withdraw a permission already given. */
    denyParticipant(author) {
        this._allow.delete(author);
        this.pending.delete(author);
        this.cursors.delete(author);
    }

    /** Silence one participant without ending the session for everyone. */
    mute(author) {
        this._muted.add(author);
        this.cursors.delete(author);
    }

    unmute(author) {
        this._muted.delete(author);
    }

    /** Instant stop: clear every mark and refuse further ops (Inv 1). */
    revoke() {
        this._admit = ADMIT.NONE;
        this.store.clearAll();
        this.cursors.clear();
    }

    /** The share ended — markup must never outlive the session (`app/ui/overlay.js:75`). */
    endShare() {
        this.revoke();
    }

    // ── roster ──────────────────────────────────────────────────────────────────────────────────

    /** A participant joined: assign their colour and record the name the cursors will carry. */
    participantJoined(author, displayName) {
        this.palette.colorFor(author);
        if (displayName) this._names.set(author, displayName);
    }

    /** A participant left: free their colour slot and drop their cursor. Strokes remain (§12). */
    participantLeft(author) {
        this.palette.release(author);
        this._names.delete(author);
        this.cursors.delete(author);
        this._muted.delete(author);
        this._peers.delete(author);
        this._allow.delete(author);
        this.pending.delete(author);
        this._limiter.forget(author);
    }

    /**
     * The `roster` op payload to broadcast after any membership change (§7.1).
     * Carries our capabilities so annotators can gate their UI on what this sharer honours, rather
     * than offering a button whose ops we would silently drop (compat R4).
     */
    rosterOp() {
        const names = {};
        for (const [ a, n ] of this._names) names[a] = n;
        return { colors: this.palette.colorMap(), names, caps: [ ...LOCAL_CAPS ], v: PROTOCOL_VERSION };
    }

    /** What we know about one client. Unknown ⇒ baseline, never a failure. */
    profileOf(author) {
        let p = this._peers.get(author);
        if (!p) {
            p = new PeerProfile();
            this._peers.set(author, p);
        }
        return p;
    }

    /** True if any connected client predates this build — the sharer may want to say so in the UI. */
    hasLegacyPeers() {
        for (const p of this._peers.values()) if (p.isLegacy) return true;
        return false;
    }

    /** Display name for a cursor label, falling back to the opaque id rather than to nothing. */
    nameOf(author) {
        return this._names.get(author) ?? author;
    }

    /** Colour for a stroke or cursor. Derived from the AUTHOR — never from the wire (§4). */
    colorOf(author) {
        return this.palette.colorFor(author);
    }

    // ── admission ───────────────────────────────────────────────────────────────────────────────

    /** @returns {boolean} may this participant's ops be honoured at all? */
    admits(author) {
        if (this._muted.has(author)) return false;
        switch (this._admit) {
            case ADMIT.EVERYONE: return true;
            case ADMIT.MODERATORS: return this._isModerator(author);
            case ADMIT.ALLOWLIST: return this._allow.has(author);
            case ADMIT.NONE:
            default: return false;
        }
    }

    // ── the one entry point ─────────────────────────────────────────────────────────────────────

    /**
     * Handle one raw endpoint message.
     *
     * @param {string} sender - the RELAY-REPORTED sender. Never a value from the payload: passing
     *   anything else here defeats every ownership and admission check in this class.
     * @param {unknown} raw
     * @param {number} [now]
     * @returns {{ accepted: boolean, reason?: string, changed?: boolean, ack?: object }}
     */
    handle(sender, raw, now = Date.now()) {
        if (!isAnnotateMessage(raw)) return no('not-ours');
        if (!this._limiter.allow(sender, now)) return no('rate-limited');

        // Admission is checked AFTER decode, not before, for one reason: a permission REQUEST
        // necessarily arrives from someone not yet admitted. Gating every message on admission
        // first would silently drop the very op whose purpose is to ask for admission.
        const d = decode(raw);
        if (!d.ok) return { accepted: false, reason: d.reason, kind: d.kind };
        const op = d.op;

        // Everything except `hello` and `request` requires admission.
        if (op.op !== OP.HELLO && op.op !== OP.REQUEST && !this.admits(sender)) return no('not-admitted');

        // A capability announcement changes nothing about the canvas — it records what this client
        // can do so we never send it something it would drop on the floor.
        if (op.op === OP.HELLO) {
            this._peers.set(sender, new PeerProfile({ v: op.hv, caps: op.caps }));
            return { accepted: true, changed: false, reason: 'hello' };
        }

        // A permission request is NOT a grant. It records that someone is asking and hands the
        // decision up to the host app, which must put it in front of the person whose screen this
        // is. Nothing here can widen what the requester may do (Inv 1).
        if (op.op === OP.REQUEST) {
            if (this.admits(sender)) return { accepted: true, changed: false, reason: 'already-allowed' };
            this.pending.add(sender);
            return { accepted: true, changed: false, reason: 'request', requestFrom: sender };
        }

        // Cursors are transient presentation state, not stored markup.
        if (op.op === OP.CURSOR) {
            this.cursors.set(sender, { x: op.x, y: op.y, t: now });
            return { accepted: true, changed: true };
        }

        // `clear: "all"` is the one op whose authority the store cannot judge. Wiping everyone's
        // work is a sharer/moderator action; from anyone else it degrades to clearing their own
        // rather than being silently dropped, so the button still does something honest.
        if (op.op === OP.CLEAR && op.scope === CLEAR_SCOPE.ALL && !this._isModerator(sender)) {
            const r = this.store.apply(sender, { op: OP.CLEAR, scope: CLEAR_SCOPE.MINE });
            return { accepted: true, changed: r.changed, reason: 'downgraded-to-mine' };
        }

        const r = this.store.apply(sender, op);
        const out = { accepted: true, changed: r.changed, reason: r.reason };
        // An `end` produces the ack the annotator holds its local echo against (§11.2).
        if (r.acked) out.ack = { id: r.acked, t: now };
        return out;
    }

    /** Drop cursors nobody has moved recently, so a stale arrow never sits on the screen. */
    expireCursors(now = Date.now(), ttlMs = 3000) {
        let changed = false;
        for (const [ author, c ] of this.cursors) {
            if (now - c.t > ttlMs) {
                this.cursors.delete(author);
                changed = true;
            }
        }
        return changed;
    }
}

function no(reason) {
    return { accepted: false, reason };
}
