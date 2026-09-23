// Casual Annotate — coordinate mapping and hit-testing (ADR-107 §6).
//
// PURE: takes plain rects and numbers, returns plain numbers. No DOM. The DOM wrapper lives in
// `surface/surface.js`; keeping the maths here is what makes it testable.
//
// ── Why this file is not a two-liner ────────────────────────────────────────────────────────────
//
// A <video> element and the video INSIDE it are different rectangles. Under `object-fit: contain`
// the video is letterboxed — pillar-boxed bars at the sides, or letterbox bars top and bottom —
// and the bars are part of the element but not part of the picture.
//
// Jitsi's own remote control normalizes against the ELEMENT:
//
//     x: (event.pageX - position.left) / area.width()      // area = VideoLayout.getLargeVideoWrapper()
//     — react/features/remote-control/actions.ts:637
//
// which skews whenever the wrapper's aspect ratio differs from the shared screen's. RAS gets this
// right (`videoContentRect()` + `normPt` at `app/ui/main.js:2075`), and since these coordinates end
// up driving marks on somebody's REAL desktop, "close enough" is not good enough. We port ours.

/**
 * @typedef {object} Rect
 * @property {number} left
 * @property {number} top
 * @property {number} width
 * @property {number} height
 */

import { COORD_MAX } from './ops.js';

/** `object-fit` values we can handle. `cover` crops, which is unaddressable — see `contentRect`. */
export const FIT = Object.freeze({ CONTAIN: 'contain', FILL: 'fill', COVER: 'cover' });

/**
 * The rect the picture actually occupies inside its element.
 *
 * @param {Rect} elementRect - the element's client rect.
 * @param {number} srcWidth  - intrinsic width of the shared surface (`video.videoWidth`).
 * @param {number} srcHeight - intrinsic height (`video.videoHeight`).
 * @param {string} fit       - computed `object-fit`.
 * @returns {{ ok: true, rect: Rect } | { ok: false, reason: string }}
 *
 * `cover` returns `{ ok: false }` DELIBERATELY. Under cover the picture is cropped, so part of the
 * shared screen is not on screen at all and cannot be pointed at. Guessing a mapping there puts
 * marks on the wrong pixels of a stranger's desktop — the worst outcome this feature can produce —
 * so the caller must disable drawing and say why (ADR-107 §6.2), never approximate.
 */
export function contentRect(elementRect, srcWidth, srcHeight, fit = FIT.CONTAIN) {
    if (!(srcWidth > 0) || !(srcHeight > 0)) return { ok: false, reason: 'source-not-sized' };
    if (!(elementRect.width > 0) || !(elementRect.height > 0)) return { ok: false, reason: 'element-not-sized' };
    if (fit === FIT.COVER) return { ok: false, reason: 'object-fit-cover-crops' };

    if (fit === FIT.FILL) {
        // The picture is stretched to the element — no bars, so content rect == element rect.
        return { ok: true, rect: { ...elementRect } };
    }

    // contain: scale to fit, centre, and the leftover becomes bars.
    const scale = Math.min(elementRect.width / srcWidth, elementRect.height / srcHeight);
    const w = srcWidth * scale;
    const h = srcHeight * scale;
    return {
        ok: true,
        rect: {
            left: elementRect.left + (elementRect.width - w) / 2,
            top: elementRect.top + (elementRect.height - h) / 2,
            width: w,
            height: h,
        },
    };
}

/**
 * Client point → normalized `0..=COORD_MAX` over the content rect.
 * Clamped, so a drag that leaves the video still produces a valid edge coordinate rather than
 * a point outside the shared screen. Mirrors `normPt` (`app/ui/main.js:2075`).
 * @returns {[number, number]}
 */
export function normalize(clientX, clientY, rect) {
    const nx = clamp01((clientX - rect.left) / rect.width);
    const ny = clamp01((clientY - rect.top) / rect.height);
    return [ Math.round(nx * COORD_MAX), Math.round(ny * COORD_MAX) ];
}

/**
 * Normalized → pixels within a target rect. The inverse of `normalize`, used by both renderers:
 * the sharer's overlay (target = the shared display) and the local echo (target = the content rect).
 * @returns {[number, number]}
 */
export function denormalize(nx, ny, rect) {
    return [ rect.left + (nx / COORD_MAX) * rect.width, rect.top + (ny / COORD_MAX) * rect.height ];
}

function clamp01(n) {
    return n < 0 ? 0 : n > 1 ? 1 : n;
}

// ── hit-testing (the eraser) ────────────────────────────────────────────────────────────────────

/**
 * Does a stroke come within `radius` of the point? Normalized units throughout, so this is
 * resolution-independent and identical on every participant's machine.
 *
 * Used by the ANNOTATOR against its own retained geometry (ADR-107 §4.1), and by the SHARER against
 * the authoritative store. Both sides therefore agree on what an eraser drag touched.
 *
 * @param {Array<[number, number]>} pts
 * @param {number} tool - `TOOL` tag; shapes hit-test on their rendered form, not their two points.
 */
export function strokeHit(pts, tool, x, y, radius) {
    if (!pts || pts.length === 0) return false;
    if (pts.length === 1) return dist(pts[0][0], pts[0][1], x, y) <= radius;

    // Rect (tool 3) is drawn as an outline from two corners — hit its four edges, not its diagonal,
    // and not its interior (an empty rect should not swallow every erase inside it).
    if (tool === 3) {
        const [ ax, ay ] = pts[0];
        const [ bx, by ] = pts[pts.length - 1];
        return segHit(ax, ay, bx, ay, x, y, radius)
            || segHit(bx, ay, bx, by, x, y, radius)
            || segHit(bx, by, ax, by, x, y, radius)
            || segHit(ax, by, ax, ay, x, y, radius);
    }

    // Arrow (tool 2) renders as a single segment first→last; pen/highlighter as a polyline.
    if (tool === 2) {
        const [ ax, ay ] = pts[0];
        const [ bx, by ] = pts[pts.length - 1];
        return segHit(ax, ay, bx, by, x, y, radius);
    }

    for (let i = 1; i < pts.length; i++) {
        if (segHit(pts[i - 1][0], pts[i - 1][1], pts[i][0], pts[i][1], x, y, radius)) return true;
    }
    return false;
}

/** Distance from point to segment, compared against `radius`. */
function segHit(ax, ay, bx, by, px, py, radius) {
    const dx = bx - ax;
    const dy = by - ay;
    const len2 = dx * dx + dy * dy;
    if (len2 === 0) return dist(ax, ay, px, py) <= radius;
    let t = ((px - ax) * dx + (py - ay) * dy) / len2;
    t = t < 0 ? 0 : t > 1 ? 1 : t;
    return dist(ax + t * dx, ay + t * dy, px, py) <= radius;
}

function dist(ax, ay, bx, by) {
    return Math.hypot(ax - bx, ay - by);
}
