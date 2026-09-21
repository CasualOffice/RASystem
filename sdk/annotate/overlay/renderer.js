// Casual Annotate — the sharer's overlay renderer (ADR-107 §8).
//
// Runs inside the transparent, click-through, always-on-top window created by `overlay/window.js`.
// This is the ONLY renderer of annotation state in the whole system: what it paints goes into the
// screen capture and reaches every participant as video (§2).
//
// Ported from `app/ui/overlay.js`, with the multi-party additions: strokes are keyed by id rather
// than pushed onto an array, and each author gets a named, coloured cursor.
//
// ADR-100's rule is absolute here: this surface NEVER receives input. If you find yourself wanting
// `setIgnoreMouseEvents(false)`, re-read ADR-100 — an interactive overlay is what produced the macOS
// white screen and the hidden context menus.

import { TOOL } from '../core/ops.js';
import { StrokeStore } from '../core/store.js';
import { toHex } from '../core/palette.js';

/** How long a cursor survives without an update before it fades out. */
const CURSOR_TTL_MS = 3000;
const CURSOR_FADE_MS = 500;

export class OverlayRenderer {
    /**
     * @param {HTMLCanvasElement} canvas - fills the shared display.
     * @param {import('../core/session.js').SharerSession} session
     */
    constructor(canvas, session) {
        this.canvas = canvas;
        this.session = session;
        this._dpr = 1;
        this._frame = this._frame.bind(this);
        this._onResize = this._fit.bind(this);
        window.addEventListener('resize', this._onResize);
        this._fit();
        this._raf = requestAnimationFrame(this._frame);
    }

    _fit() {
        this._dpr = window.devicePixelRatio || 1;
        this.canvas.width = Math.round(window.innerWidth * this._dpr);
        this.canvas.height = Math.round(window.innerHeight * this._dpr);
    }

    _frame() {
        this._raf = requestAnimationFrame(this._frame);
        // `alpha: true` is the 2D default, but set explicitly: the overlay must composite over the
        // desktop, never paint an opaque ground. Getting this wrong is a white screen, not a tint.
        const g = this.canvas.getContext('2d', { alpha: true });
        g.clearRect(0, 0, this.canvas.width, this.canvas.height);

        const now = Date.now();
        this.session.expireCursors(now, CURSOR_TTL_MS + CURSOR_FADE_MS);

        for (const s of this.session.store.strokes()) {
            this._drawStroke(g, s, toHex(this.session.colorOf(s.author)));
        }
        for (const [ author, c ] of this.session.cursors) {
            this._drawCursor(g, c, toHex(this.session.colorOf(author)), this.session.nameOf(author), now);
        }
    }

    /** Normalized `0..=65535` spans the whole overlay, which spans the shared display exactly. */
    _x(n) { return (n / 65535) * this.canvas.width; }
    _y(n) { return (n / 65535) * this.canvas.height; }

    _drawStroke(g, s, color) {
        // Holes (a dropped `append`, until the repair lands) are skipped, never drawn as a gap.
        const pts = StrokeStore.densePoints(s);
        if (!pts.length) return;
        const dpr = this._dpr;
        const X = n => this._x(n);
        const Y = n => this._y(n);

        g.strokeStyle = color;
        g.lineJoin = 'round';
        g.lineCap = 'round';
        g.globalAlpha = s.tool === TOOL.HIGHLIGHTER ? 0.35 : 1;
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

    /**
     * A named, coloured pointer per participant.
     *
     * ADR-100 removed the unlabelled "look here" arrow from the RAS overlay because, beside the
     * host's own cursor, it read as a confusing second cursor. The NAME is what makes this different
     * and is not optional decoration — without it we would be reintroducing exactly the artifact
     * ADR-100 deleted.
     */
    _drawCursor(g, c, color, name, now) {
        const age = now - c.t;
        if (age > CURSOR_TTL_MS + CURSOR_FADE_MS) return;
        const alpha = age <= CURSOR_TTL_MS ? 1 : 1 - (age - CURSOR_TTL_MS) / CURSOR_FADE_MS;

        const dpr = this._dpr;
        const x = this._x(c.x);
        const y = this._y(c.y);
        const s = 14 * dpr;

        g.globalAlpha = alpha;

        // Arrow, outlined in white so it stays visible over any desktop content.
        g.beginPath();
        g.moveTo(x, y);
        g.lineTo(x, y + s);
        g.lineTo(x + s * 0.28, y + s * 0.72);
        g.lineTo(x + s * 0.52, y + s * 1.18);
        g.lineTo(x + s * 0.72, y + s * 1.08);
        g.lineTo(x + s * 0.48, y + s * 0.62);
        g.lineTo(x + s * 0.84, y + s * 0.58);
        g.closePath();
        g.fillStyle = color;
        g.strokeStyle = 'rgba(255,255,255,0.9)';
        g.lineWidth = 1.5 * dpr;
        g.fill();
        g.stroke();

        // Name chip.
        const label = String(name ?? '').slice(0, 24);
        if (label) {
            const pad = 5 * dpr;
            g.font = `${12 * dpr}px system-ui, -apple-system, sans-serif`;
            const w = g.measureText(label).width + pad * 2;
            const h = 18 * dpr;
            const lx = x + s * 0.9;
            const ly = y + s * 1.1;
            g.fillStyle = color;
            if (g.roundRect) {
                g.beginPath();
                g.roundRect(lx, ly, w, h, 4 * dpr);
                g.fill();
            } else {
                g.fillRect(lx, ly, w, h);
            }
            g.fillStyle = '#fff';
            g.textBaseline = 'middle';
            g.fillText(label, lx + pad, ly + h / 2);
        }
        g.globalAlpha = 1;
    }

    dispose() {
        cancelAnimationFrame(this._raf);
        window.removeEventListener('resize', this._onResize);
    }
}
