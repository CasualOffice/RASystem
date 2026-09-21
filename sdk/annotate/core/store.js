// Casual Annotate — the authoritative stroke store (ADR-107 §4, §7).
//
// The SHARER owns one of these. It is the single renderer in the system, so this is the only place
// annotation state exists — there is no replicated document, no CRDT, and no late-joiner replay,
// because every other participant receives the marks as video (ADR-107 §2).
//
// PURE: no DOM, no transport, no clock beyond what callers pass in. Fully unit-testable.
//
// The invariant this file exists to enforce:
//
//   EVERY REMOVE IS SCOPED TO ITS AUTHOR.
//
// `undo`, `erase` and `clear:"mine"` can only ever affect strokes whose author is the relay-reported
// sender. The store NEVER trusts a stroke id to tell it who owns the stroke — it looks the stroke up
// and compares the stored author. This is the single most important deviation from the RAS code,
// where the host overlay did `annotStrokes.pop()` (`app/ui/overlay.js:68`): with several authors on a
// lossy relay, "pop the last one" deletes someone else's work.

import { LIMITS, OP, CLEAR_SCOPE } from './ops.js';

/**
 * @typedef {object} Stroke
 * @property {string} id
 * @property {string} author   - relay-reported sender. The ONLY ownership record.
 * @property {number} tool     - a `TOOL` tag.
 * @property {Array<[number, number]>} pts - INDEXED, so it may be sparse while a stroke is still
 *   arriving or has lost a message. Render through `densePoints`, never raw.
 * @property {boolean} open    - still receiving `append`s (no `end` yet).
 * @property {number} seq      - insertion order, for `undo` and for oldest-first eviction.
 */

export class StrokeStore {
    constructor() {
        /** @type {Map<string, Stroke>} insertion-ordered, which is what makes eviction + undo cheap. */
        this._strokes = new Map();
        /** @type {Map<string, number>} author → count of currently-open strokes. */
        this._open = new Map();
        this._seq = 0;
    }

    /** Every retained stroke, oldest first. The renderer draws in exactly this order. */
    strokes() {
        return [ ...this._strokes.values() ];
    }

    /**
     * A stroke's points with any holes removed.
     *
     * Holes are transient — a dropped `append` leaves one until the §5 repair fills it — and drawing
     * straight through a hole is the right behaviour: the line is briefly a little coarser, which is
     * far better than a gap or a dropped stroke.
     * @param {Stroke} s
     * @returns {Array<[number, number]>}
     */
    static densePoints(s) {
        const out = [];
        for (let i = 0; i < s.pts.length; i++) if (s.pts[i] !== undefined) out.push(s.pts[i]);
        return out;
    }

    /** @returns {number} retained stroke count. */
    get size() {
        return this._strokes.size;
    }

    /**
     * Apply one DECODED op from `author`.
     *
     * `author` MUST be the relay-reported sender. Passing a value taken from the payload would
     * defeat every ownership check in this file.
     *
     * @param {string} author
     * @param {object} op - already validated by `ops.decode`.
     * @returns {{ changed: boolean, reason?: string, acked?: string }}
     */
    apply(author, op) {
        switch (op.op) {
            case OP.BEGIN: return this._begin(author, op);
            case OP.APPEND: return this._append(author, op);
            case OP.END: return this._end(author, op);
            case OP.UNDO: return this._undo(author);
            case OP.ERASE: return this._erase(author, op);
            case OP.CLEAR: return this._clear(author, op);
            default: return { changed: false, reason: `not-a-store-op:${op.op}` };
        }
    }

    _begin(author, { id, tool }) {
        // Re-`begin` of a live id is a no-op, not a reset: a duplicated message (the §5 stroke
        // repair resends the whole stroke) must not wipe points already accumulated.
        if (this._strokes.has(id)) return { changed: false, reason: 'duplicate-id' };

        const open = this._open.get(author) ?? 0;
        if (open >= LIMITS.MAX_OPEN_STROKES_PER_AUTHOR) {
            return { changed: false, reason: 'too-many-open-strokes' };
        }

        this._strokes.set(id, { id, author, tool, pts: [], open: true, seq: this._seq++ });
        this._open.set(author, open + 1);
        this._evict();
        return { changed: true };
    }

    /**
     * Write points at their stated indices.
     *
     * Two properties fall out of indexing rather than pushing, and both matter:
     *
     *   IDEMPOTENT — an append NEVER overwrites a point that is already present. So a duplicate
     *   (the §5 repair resends the whole stroke) is a no-op, and a repair after `end` can still
     *   FILL A HOLE left by a dropped message without being able to rewrite history.
     *
     *   ORDER-INDEPENDENT — appends may arrive in any order. The relay does not promise ordering,
     *   so a push-based append would have been a latent corruption bug on a busy bridge.
     */
    _append(author, { id, at, pts }) {
        const s = this._strokes.get(id);
        if (!s) return { changed: false, reason: 'unknown-stroke' };
        // Ownership: an author may only extend their OWN stroke.
        if (s.author !== author) return { changed: false, reason: 'not-your-stroke' };

        let changed = false;
        for (let i = 0; i < pts.length; i++) {
            const idx = at + i;
            // Bound the stroke. Points past the cap are DROPPED, not rejected — the stroke stays
            // valid and renderable, it just stops growing. RAS does the same (`MAX_ANNOT_POINTS`).
            if (idx >= LIMITS.MAX_STROKE_POINTS) break;
            if (s.pts[idx] === undefined) {
                s.pts[idx] = pts[i];
                changed = true;
            }
        }
        return changed ? { changed: true } : { changed: false, reason: 'no-new-points' };
    }

    _end(author, { id }) {
        const s = this._strokes.get(id);
        if (!s) return { changed: false, reason: 'unknown-stroke' };
        if (s.author !== author) return { changed: false, reason: 'not-your-stroke' };
        if (!s.open) return { changed: false, reason: 'already-ended' };

        s.open = false;
        this._open.set(author, Math.max(0, (this._open.get(author) ?? 1) - 1));
        // The ack is what frees the annotator's local echo from guessing (ADR-107 §11.2).
        return { changed: true, acked: id };
    }

    /** Remove the author's own most recent stroke. Never anyone else's, whatever arrived last. */
    _undo(author) {
        let victim = null;
        for (const s of this._strokes.values()) {
            if (s.author === author && (victim === null || s.seq > victim.seq)) victim = s;
        }
        if (!victim) return { changed: false, reason: 'nothing-to-undo' };
        this._drop(victim);
        return { changed: true };
    }

    /**
     * Remove the ids the author hit-tested locally — after RE-FILTERING them by author here.
     * The annotator computed this list against its own record, but the store does not take its word
     * for it: an `erase` carrying someone else's ids silently removes nothing of theirs.
     */
    _erase(author, { ids }) {
        let changed = false;
        for (const id of ids) {
            const s = this._strokes.get(id);
            if (s && s.author === author) {
                this._drop(s);
                changed = true;
            }
        }
        return changed ? { changed: true } : { changed: false, reason: 'nothing-erased' };
    }

    /**
     * `mine` drops the sender's strokes. `all` drops everyone's — and is the ONE op whose authority
     * this store cannot determine on its own, so the caller must gate it (sharer/moderator) before
     * calling. See `session.js`.
     */
    _clear(author, { scope }) {
        if (scope === CLEAR_SCOPE.ALL) {
            if (this._strokes.size === 0) return { changed: false, reason: 'already-empty' };
            this._strokes.clear();
            this._open.clear();
            return { changed: true };
        }
        let changed = false;
        for (const s of [ ...this._strokes.values() ]) {
            if (s.author === author) {
                this._drop(s);
                changed = true;
            }
        }
        return changed ? { changed: true } : { changed: false, reason: 'nothing-to-clear' };
    }

    /** Drop every stroke by one author — used when a participant leaves, if policy says to. */
    dropAuthor(author) {
        let changed = false;
        for (const s of [ ...this._strokes.values() ]) {
            if (s.author === author) {
                this._drop(s);
                changed = true;
            }
        }
        return changed;
    }

    /** Drop everything. Called when the share ends, so markup never outlives the session. */
    clearAll() {
        this._strokes.clear();
        this._open.clear();
    }

    _drop(s) {
        this._strokes.delete(s.id);
        if (s.open) this._open.set(s.author, Math.max(0, (this._open.get(s.author) ?? 1) - 1));
    }

    /** Bound retained strokes, oldest dropped first — RAS's `MAX_ANNOT_STROKES` discipline. */
    _evict() {
        while (this._strokes.size > LIMITS.MAX_STROKES) {
            const oldest = this._strokes.values().next().value;
            if (!oldest) break;
            this._drop(oldest);
        }
    }
}
