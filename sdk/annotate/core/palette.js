// Casual Annotate — colour assignment (ADR-107 §7.1).
//
// One colour per participant, ASSIGNED by the sharer — never hashed from the endpoint id.
//
// Why this is not a hash: a hash collides. The moment two participants land on the same colour,
// "erase my colour" stops meaning anything to a human watching the screen, and one user's eraser
// visually appears to eat another's work. The sharer is already the sole renderer, the authority and
// the roster holder, so it is the only place a collision-free assignment can be made.
//
// Correctness never depends on the colour being unique: every remove is keyed on AUTHOR ID
// (`core/store.js`), so even under palette exhaustion removes stay exact. Only the visual shorthand
// degrades. Colour is how a human reads identity, not how the system resolves it.

/**
 * The palette. These land on ARBITRARY screen content — someone's IDE, a spreadsheet, a photo — not
 * a white canvas, so they are chosen for:
 *   - high contrast against both light and dark backgrounds,
 *   - legibility at 0.35 alpha (the highlighter path, `app/ui/overlay.js:89`),
 *   - separability under the common colour-vision deficiencies (no red/green-only distinctions).
 *
 * Ordered so the first few assignments are maximally distinct from each other.
 */
export const PALETTE = Object.freeze([
    0xFF3B30, // red      — the RAS default (`app/ui/main.js` first swatch)
    0x1E90FF, // blue
    0xFFD60A, // amber
    0x30D158, // green
    0xBF5AF2, // violet
    0xFF9F0A, // orange
    0x64D2FF, // cyan
    0xFF375F, // pink
    0xAC8E68, // tan
    0x8E8E93, // grey
]);

/** `0xRRGGBB` → `"#rrggbb"`. Mirrors `colorHex` in `app/ui/overlay.js:64`. */
export function toHex(n) {
    return '#' + ((n & 0xffffff) >>> 0).toString(16).padStart(6, '0');
}

/**
 * Collision-free colour assignment over a small palette, with graceful degradation past its end.
 *
 * Slots are released on leave and REUSED, so a long meeting with churn keeps handing out distinct
 * colours instead of marching off the end of the palette.
 */
export class PaletteAssigner {
    constructor(palette = PALETTE) {
        this._palette = palette;
        /** @type {Map<string, number>} author → palette index. */
        this._assigned = new Map();
        /** @type {Set<number>} indices currently in use. */
        this._used = new Set();
        /** Round-robin cursor for the exhausted case, so reuse spreads instead of piling on slot 0. */
        this._overflow = 0;
    }

    /**
     * Colour for `author`, assigning one on first sight. Stable for as long as they stay.
     * @returns {number} `0xRRGGBB`
     */
    colorFor(author) {
        const existing = this._assigned.get(author);
        if (existing !== undefined) return this._palette[existing];

        let idx = -1;
        for (let i = 0; i < this._palette.length; i++) {
            if (!this._used.has(i)) { idx = i; break; }
        }
        if (idx === -1) {
            // Exhausted: reuse round-robin. Removes stay exact (they key on author id); only the
            // visual shorthand degrades, and the name label still disambiguates.
            idx = this._overflow % this._palette.length;
            this._overflow++;
        } else {
            this._used.add(idx);
        }
        this._assigned.set(author, idx);
        return this._palette[idx];
    }

    /** Release an author's slot so it can be handed to the next joiner. */
    release(author) {
        const idx = this._assigned.get(author);
        if (idx === undefined) return;
        this._assigned.delete(author);
        // Only free the slot if nobody else landed on it via the overflow path.
        for (const other of this._assigned.values()) if (other === idx) return;
        this._used.delete(idx);
    }

    /** The `roster` op's colour map: `{ author: "0xRRGGBB" }` for everyone currently assigned. */
    colorMap() {
        const out = {};
        for (const [ author, idx ] of this._assigned) out[author] = toHex(this._palette[idx]);
        return out;
    }

    /** True once every palette slot is taken — the point where colours start repeating. */
    get exhausted() {
        return this._used.size >= this._palette.length;
    }
}
