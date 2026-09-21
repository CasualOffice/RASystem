// Casual Annotate — the annotator's drawing surface (ADR-107 §6, §11.2).
//
// A transparent canvas laid over the shared-screen video. Ported from the RAS viewer's annotation
// module (`app/ui/main.js:1952`), with four changes the multi-party design forces:
//
//   1. No colour picker. The sharer assigns your colour (§7.1) — `setColor` takes what the roster
//      says, and the user never chooses.
//   2. Points stream out in batches while drawing, not once on pen-up, so the ink is live.
//   3. The local echo is held against the sharer's ACK, never a timer (§11.2) — a fixed fade is
//      what makes a mark blink when the round trip is about as long as the fade.
//   4. Completed strokes are RETAINED after their echo fades, because `undo` and the eraser
//      hit-test locally against our own geometry (§4.1).
//
// Requires a DOM. Everything computational it relies on lives in `core/geometry.js`, which does not.

import { TOOL, strokeId } from '../core/ops.js';
import { contentRect, normalize, denormalize, strokeHit, FIT } from '../core/geometry.js';

/** Eraser radius, in normalized units (~1.5% of the shared surface). */
const ERASE_RADIUS = 1000;

/** How long a completed stroke's local echo lingers once the sharer has acked it. */
const ECHO_FADE_MS = 400;

/**
 * @typedef {object} SurfaceOptions
 * @property {HTMLCanvasElement} canvas - overlays the video; must be `pointer-events: none` unless drawing.
 * @property {HTMLVideoElement} video   - the shared-screen video element.
 * @property {(op: object) => void} send
 * @property {(msg: string | null) => void} [onStatus] - surfaced to the user when drawing is disabled.
 */

export class AnnotationSurface {
    /** @param {SurfaceOptions} opts */
    constructor({ canvas, video, send, onStatus }) {
        this.canvas = canvas;
        this.video = video;
        this._send = send;
        this._onStatus = onStatus ?? (() => {});

        this.tool = null;          // null = not drawing; the surface stays click-through
        this.color = '#ff3b30';    // replaced by the roster's assignment
        this._author = 'me';
        this._n = 0;
        this._dpr = 1;

        /** In-flight stroke. */
        this._cur = null;
        /**
         * Our own completed strokes: `{ id, tool, pts, ackedAt }`.
         * Retained AFTER the echo fades — the rendering goes, the record stays, because `undo` and
         * the eraser resolve locally (§4.1).
         * @type {Map<string, {id: string, tool: number, pts: Array<[number,number]>, ackedAt: number|null}>}
         */
        this._mine = new Map();

        this._onDown = this._onDown.bind(this);
        this._onMove = this._onMove.bind(this);
        this._onUp = this._onUp.bind(this);
        this._onResize = this._fit.bind(this);
        this._frame = this._frame.bind(this);

        canvas.addEventListener('pointerdown', this._onDown);
        canvas.addEventListener('pointermove', this._onMove);
        canvas.addEventListener('pointerup', this._onUp);
        canvas.addEventListener('pointercancel', this._onUp);
        window.addEventListener('resize', this._onResize);

        this._fit();
        this._raf = requestAnimationFrame(this._frame);
    }

    /** Identify ourselves, so stroke ids are unique across participants. */
    setAuthor(id) {
        this._author = id;
    }

    /** Apply the colour the sharer assigned us. The user never picks (§7.1). */
    setColor(hex) {
        this.color = hex;
    }

    /**
     * Select a tool, or `null` to stop drawing. While null the canvas ignores pointer events, so the
     * meeting UI underneath stays fully usable — the same default the RAS viewer had.
     */
    setTool(tool) {
        this.tool = tool;
        this.canvas.style.pointerEvents = tool === null ? 'none' : 'auto';
        this.canvas.style.cursor = tool === 'erase' ? 'cell' : tool === null ? '' : 'crosshair';
    }

    /**
     * The video's content rect, or a reason we cannot map coordinates.
     *
     * Refusing is deliberate. Under `object-fit: cover` part of the shared screen is cropped away
     * and cannot be pointed at; guessing would put marks on the wrong pixels of someone's real
     * desktop, which is the worst thing this feature can do (§6.2).
     */
    _rect() {
        const el = this.canvas.getBoundingClientRect();
        const fit = getComputedStyle(this.video).objectFit || FIT.CONTAIN;
        return contentRect(
            { left: el.left, top: el.top, width: el.width, height: el.height },
            this.video.videoWidth, this.video.videoHeight, fit,
        );
    }

    /** Can the user draw right now? Sets the status message as a side effect. */
    canDraw() {
        const r = this._rect();
        if (!r.ok) {
            this._onStatus(
                r.reason === 'object-fit-cover-crops'
                    ? 'Annotation needs the full shared screen in view — switch off cropped/tile view.'
                    : 'Waiting for the shared screen…',
            );
            return false;
        }
        this._onStatus(null);
        return true;
    }

    // ── drawing ─────────────────────────────────────────────────────────────────────────────────

    _onDown(e) {
        if (this.tool === null || !this.canDraw()) return;
        this.canvas.setPointerCapture(e.pointerId);

        const r = this._rect();
        const pt = normalize(e.clientX, e.clientY, r.rect);

        if (this.tool === 'erase') {
            this._erasing = true;
            this._eraseAt(pt);
            return;
        }

        const id = strokeId(this._author, this._n++);
        this._cur = { id, tool: this.tool, pts: [ pt ], sent: 0 };
        this._send({ op: 'begin', id, tool: this.tool });
    }

    _onMove(e) {
        if (!this._cur && !this._erasing) return;
        const r = this._rect();
        if (!r.ok) return;
        const pt = normalize(e.clientX, e.clientY, r.rect);

        if (this._erasing) return this._eraseAt(pt);

        // Freehand accumulates; the two-point shapes track their second corner.
        if (this._cur.tool === TOOL.PEN || this._cur.tool === TOOL.HIGHLIGHTER) {
            this._cur.pts.push(pt);
        } else {
            this._cur.pts[1] = pt;
        }
        this._flush(false);
    }

    _onUp() {
        if (this._erasing) {
            this._erasing = false;
            return;
        }
        if (!this._cur) return;

        this._flush(true);
        const { id, tool, pts } = this._cur;
        this._send({ op: 'end', id });
        // Retain the geometry: the echo will fade, but undo and the eraser need it (§4.1).
        this._mine.set(id, { id, tool, pts, ackedAt: null });
        this._cur = null;
    }

    /**
     * Send whatever points have not gone out yet, in batches.
     * Each `append` carries its start index, so the stream is idempotent and order-independent.
     */
    _flush(final) {
        const c = this._cur;
        if (!c) return;
        const BATCH = 24;
        while (c.pts.length - c.sent >= BATCH || (final && c.pts.length > c.sent)) {
            const at = c.sent;
            const slice = c.pts.slice(at, at + BATCH);
            this._send({ op: 'append', id: c.id, at, pts: slice });
            c.sent += slice.length;
            if (!final && c.pts.length - c.sent < BATCH) break;
        }
        // A shape's second point keeps moving, so re-send it rather than leaving a stale corner.
        if (final && (c.tool === TOOL.ARROW || c.tool === TOOL.RECT) && c.pts.length === 2) {
            this._send({ op: 'append', id: c.id, at: 0, pts: c.pts });
        }
    }

    /** Erase our own strokes under the pointer. Hit-tested locally; the sharer re-filters anyway. */
    _eraseAt([ x, y ]) {
        const hits = [];
        for (const s of this._mine.values()) {
            if (strokeHit(s.pts, s.tool, x, y, ERASE_RADIUS)) hits.push(s.id);
        }
        if (!hits.length) return;
        for (const id of hits) this._mine.delete(id);
        this._send({ op: 'erase', ids: hits.slice(0, 64) });
    }

    /** Undo our own most recent stroke. */
    undo() {
        let last = null;
        for (const s of this._mine.values()) last = s;
        if (last) this._mine.delete(last.id);
        this._send({ op: 'undo' });
    }

    /** Clear our own marks. */
    clearMine() {
        this._mine.clear();
        this._send({ op: 'clear', scope: 'mine' });
    }

    /**
     * The sharer acked a stroke — it is now on their screen and heading into the video.
     *
     * This is what replaces a guessed timer (§11.2): we start the hand-off from a real signal, then
     * hold for the estimated video lag before fading, and never fade while the pointer is down.
     * @param {string} id
     * @param {number} videoLagMs - current estimate; see `latency/beacon.js`.
     */
    onAck(id, videoLagMs = 600) {
        const s = this._mine.get(id);
        if (s) s.ackedAt = performance.now() + videoLagMs;
    }

    // ── rendering ───────────────────────────────────────────────────────────────────────────────

    _fit() {
        this._dpr = window.devicePixelRatio || 1;
        this.canvas.width = Math.max(1, Math.round(this.canvas.clientWidth * this._dpr));
        this.canvas.height = Math.max(1, Math.round(this.canvas.clientHeight * this._dpr));
    }

    _frame() {
        this._raf = requestAnimationFrame(this._frame);
        const g = this.canvas.getContext('2d');
        g.clearRect(0, 0, this.canvas.width, this.canvas.height);

        const r = this._rect();
        if (!r.ok) return;
        const box = this.canvas.getBoundingClientRect();
        const now = performance.now();

        for (const s of this._mine.values()) {
            const a = this._echoAlpha(s, now);
            if (a > 0) this._draw(g, s, r.rect, box, a);
        }
        if (this._cur) this._draw(g, this._cur, r.rect, box, 1);
    }

    /**
     * Local-echo opacity. Full strength until the ack plus the video lag, then a short cross-fade.
     * A lingering ghost reads as ink; a gap reads as a bug — so we err long (§11.2).
     */
    _echoAlpha(s, now) {
        if (s.ackedAt === null) return 1;
        const since = now - s.ackedAt;
        if (since <= 0) return 1;
        if (since >= ECHO_FADE_MS) return 0;
        return 1 - since / ECHO_FADE_MS;
    }

    /** Tool geometry, ported from `app/ui/overlay.js:80`. Normalized → canvas backing-store pixels. */
    _draw(g, s, rect, box, alpha) {
        const pts = s.pts;
        if (!pts || !pts.length) return;
        const dpr = this._dpr;
        const X = n => (rect.left - box.left + (n / 65535) * rect.width) * dpr;
        const Y = n => (rect.top - box.top + (n / 65535) * rect.height) * dpr;

        g.strokeStyle = this.color;
        g.lineJoin = 'round';
        g.lineCap = 'round';
        g.globalAlpha = (s.tool === TOOL.HIGHLIGHTER ? 0.35 : 1) * alpha;
        g.lineWidth = (s.tool === TOOL.HIGHLIGHTER ? 18 : 3) * dpr;

        const a = pts[0];
        const b = pts[pts.length - 1];
        g.beginPath();
        if (s.tool === TOOL.RECT) {
            g.strokeRect(X(a[0]), Y(a[1]), X(b[0]) - X(a[0]), Y(b[1]) - Y(a[1]));
        } else if (s.tool === TOOL.ARROW) {
            g.moveTo(X(a[0]), Y(a[1]));
            g.lineTo(X(b[0]), Y(b[1]));
            g.stroke();
            const ang = Math.atan2(Y(b[1]) - Y(a[1]), X(b[0]) - X(a[0]));
            const head = 16 * dpr;
            g.beginPath();
            g.moveTo(X(b[0]), Y(b[1]));
            g.lineTo(X(b[0]) - head * Math.cos(ang - Math.PI / 6), Y(b[1]) - head * Math.sin(ang - Math.PI / 6));
            g.moveTo(X(b[0]), Y(b[1]));
            g.lineTo(X(b[0]) - head * Math.cos(ang + Math.PI / 6), Y(b[1]) - head * Math.sin(ang + Math.PI / 6));
            g.stroke();
        } else {
            g.moveTo(X(pts[0][0]), Y(pts[0][1]));
            for (let i = 1; i < pts.length; i++) g.lineTo(X(pts[i][0]), Y(pts[i][1]));
            g.stroke();
        }
        g.globalAlpha = 1;
    }

    dispose() {
        cancelAnimationFrame(this._raf);
        this.canvas.removeEventListener('pointerdown', this._onDown);
        this.canvas.removeEventListener('pointermove', this._onMove);
        this.canvas.removeEventListener('pointerup', this._onUp);
        this.canvas.removeEventListener('pointercancel', this._onUp);
        window.removeEventListener('resize', this._onResize);
        this._mine.clear();
    }
}

export { denormalize };
