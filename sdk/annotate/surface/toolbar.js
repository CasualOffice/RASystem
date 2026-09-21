// Casual Annotate — the annotator's UI (ADR-107 §9).
//
// A small floating panel plus a drawing canvas, injected over whatever is showing the shared
// screen. Two states, and the boundary between them is the consent decision:
//
//   BEFORE permission — one button: "Request to annotate". Nothing else is offered, because
//   nothing else would work, and a toolbar full of dead buttons is worse than no toolbar.
//
//   AFTER permission  — tools, and the canvas starts accepting pointer events.
//
// ── Why this is not a Jitsi toolbar button ──────────────────────────────────────────────────────
//
// In the Electron app the meeting is a CROSS-ORIGIN IFRAME. The External API exposes commands and
// events, but no way to add a control inside that UI, and the iframe's DOM is unreachable. So the
// annotator's UI has to be our own layer drawn over the iframe by the host app. A *browser*
// participant would need a jitsi-meet fork to get a button in the real toolbar — a separate track,
// noted in the design doc rather than pretended away.

import { TOOL } from '../core/ops.js';

const TOOLS = [
    { tool: TOOL.PEN, label: 'Pen', glyph: '✏️' },
    { tool: TOOL.HIGHLIGHTER, label: 'Highlighter', glyph: '🖍️' },
    { tool: TOOL.ARROW, label: 'Arrow', glyph: '↗' },
    { tool: TOOL.RECT, label: 'Rectangle', glyph: '▭' },
    { tool: 'erase', label: 'Eraser', glyph: '🧽' },
];

const CSS = `
.ca-root { position:fixed; inset:0; pointer-events:none; z-index:2147483000;
  font:13px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif; }
.ca-canvas { position:absolute; inset:0; width:100%; height:100%; pointer-events:none; }
.ca-bar { position:absolute; left:50%; transform:translateX(-50%); bottom:96px;
  display:flex; align-items:center; gap:6px; padding:8px 10px; border-radius:10px;
  background:rgba(24,24,27,.94); color:#fff; box-shadow:0 8px 28px rgba(0,0,0,.45);
  pointer-events:auto; user-select:none; }
.ca-btn { display:flex; align-items:center; gap:6px; border:0; border-radius:7px; padding:7px 11px;
  background:#3f3f46; color:#fff; font:inherit; cursor:pointer; }
.ca-btn:hover { background:#52525b; }
.ca-btn.on { background:#2563eb; }
.ca-btn:disabled { opacity:.45; cursor:default; }
.ca-primary { background:#2563eb; font-weight:600; }
.ca-primary:hover { background:#1d4ed8; }
.ca-swatch { width:14px; height:14px; border-radius:50%; border:2px solid rgba(255,255,255,.85); }
.ca-note { opacity:.75; padding:0 4px; max-width:320px; }
.ca-sep { width:1px; align-self:stretch; background:rgba(255,255,255,.18); margin:0 2px; }
`;

export class AnnotatorToolbar {
    /**
     * @param {object} opts
     * @param {HTMLElement} [opts.parent]
     * @param {() => void} opts.onRequest
     * @param {(tool: number|string|null) => void} opts.onTool
     * @param {() => void} opts.onUndo
     * @param {() => void} opts.onClear
     */
    constructor({ parent = document.body, onRequest, onTool, onUndo, onClear }) {
        this._cb = { onRequest, onTool, onUndo, onClear };
        this._tool = null;

        if (!document.getElementById('ca-style')) {
            const st = document.createElement('style');
            st.id = 'ca-style';
            st.textContent = CSS;
            document.head.appendChild(st);
        }

        this.root = document.createElement('div');
        this.root.className = 'ca-root';

        // The canvas the surface draws on. Sits under the bar and ignores pointer events until a
        // tool is picked, so the meeting UI beneath stays fully usable.
        this.canvas = document.createElement('canvas');
        this.canvas.className = 'ca-canvas';

        this.bar = document.createElement('div');
        this.bar.className = 'ca-bar';

        this.root.append(this.canvas, this.bar);
        parent.appendChild(this.root);

        this.render({ permission: null });
    }

    /** Rebuild the bar for the current permission state. */
    render({ permission, color = '#ff3b30', canErase = true } = {}) {
        this.bar.textContent = '';

        if (permission !== 'granted') {
            const btn = el('button', 'ca-btn ca-primary', 'Request to annotate');
            btn.disabled = permission === 'pending';
            btn.textContent = permission === 'pending' ? 'Waiting for approval…' : 'Request to annotate';
            btn.onclick = () => this._cb.onRequest?.();
            this.bar.append(btn);

            const note = {
                pending: 'The person sharing has been asked.',
                denied: 'Your request was declined.',
                revoked: 'Annotation access was withdrawn.',
            }[permission];
            if (note) this.bar.append(el('span', 'ca-note', note));
            return;
        }

        const sw = el('span', 'ca-swatch');
        sw.style.background = color;
        sw.title = 'Your colour, assigned by the person sharing';
        this.bar.append(sw, el('span', 'ca-sep'));

        for (const t of TOOLS) {
            if (t.tool === 'erase' && !canErase) continue;
            const b = el('button', `ca-btn${this._tool === t.tool ? ' on' : ''}`, `${t.glyph} ${t.label}`);
            b.onclick = () => {
                // Clicking the active tool turns drawing off — otherwise there is no way back to a
                // click-through canvas without a separate "off" button.
                this._tool = this._tool === t.tool ? null : t.tool;
                this._cb.onTool?.(this._tool);
                this.render({ permission, color, canErase });
            };
            this.bar.append(b);
        }

        this.bar.append(el('span', 'ca-sep'));
        const undo = el('button', 'ca-btn', 'Undo');
        undo.onclick = () => this._cb.onUndo?.();
        const clear = el('button', 'ca-btn', 'Clear mine');
        clear.onclick = () => this._cb.onClear?.();
        this.bar.append(undo, clear);
    }

    /** A transient message — a refusal reason, or why drawing is unavailable right now. */
    setNote(text) {
        let n = this.bar.querySelector('.ca-note');
        if (!text) return n?.remove();
        if (!n) {
            n = el('span', 'ca-note');
            this.bar.append(n);
        }
        n.textContent = text;
    }

    destroy() {
        this.root.remove();
    }
}

function el(tag, cls, text) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text) n.textContent = text;
    return n;
}
