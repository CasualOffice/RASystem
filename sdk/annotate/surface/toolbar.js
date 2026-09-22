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
//
// Icons are inline SVG, not emoji: an emoji glyph renders inconsistently across OS/font stacks (a
// different, sometimes illegible shape on Windows vs. macOS vs. Linux), which is a real
// accessibility and brand-consistency problem for UI chrome meant to look identical everywhere.

import { TOOL } from '../core/ops.js';

/** 24x24 viewBox, 1.75 stroke — a consistent weight across the whole set. */
const ICONS = {
    pen: '<path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
    highlighter: '<path d="m9 11-6 6v3h3l6-6"/><path d="m17 3 4 4L11 17l-4-4Z"/>',
    arrow: '<path d="M7 17 17 7"/><path d="M8 7h9v9"/>',
    rect: '<rect x="4" y="6" width="16" height="12" rx="1.5"/>',
    erase: '<path d="m20 20-9-9"/><path d="M8.5 4.5 20 16l-4 4H9l-5.5-5.5a2 2 0 0 1 0-2.8l6-6a2 2 0 0 1 2.8 0Z"/>',
    undo: '<path d="M9 14 4 9l5-5"/><path d="M4 9h11a5 5 0 0 1 0 10h-1"/>',
    clear: '<path d="M4 7h16"/><path d="M10 11v6"/><path d="M14 11v6"/>'
        + '<path d="M6 7l1 12a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-12"/><path d="M9 7V4h6v3"/>',
    spinner: '<circle cx="12" cy="12" r="9" stroke-dasharray="42 14"/>',
};

function svg(name, size = 16) {
    return `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" `
        + `stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" `
        + `aria-hidden="true">${ICONS[name] ?? ''}</svg>`;
}

const TOOLS = [
    { tool: TOOL.PEN, label: 'Pen', icon: 'pen' },
    { tool: TOOL.HIGHLIGHTER, label: 'Highlighter', icon: 'highlighter' },
    { tool: TOOL.ARROW, label: 'Arrow', icon: 'arrow' },
    { tool: TOOL.RECT, label: 'Rectangle', icon: 'rect' },
    { tool: 'erase', label: 'Eraser', icon: 'erase' },
];

const CSS = `
.ca-root { position:fixed; inset:0; pointer-events:none; z-index:2147483000;
  font:13px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif; }
.ca-canvas { position:absolute; inset:0; width:100%; height:100%; pointer-events:none; }
.ca-bar { position:absolute; left:50%; transform:translateX(-50%); bottom:96px;
  display:flex; align-items:center; gap:4px; padding:6px; border-radius:12px;
  background:rgba(24,24,27,.94); color:#fff; box-shadow:0 8px 28px rgba(0,0,0,.45);
  pointer-events:auto; user-select:none;
  animation:ca-bar-in .18s cubic-bezier(.2,.7,.3,1); }
@keyframes ca-bar-in { from { transform:translate(-50%,8px); opacity:0; } to { transform:translate(-50%,0); opacity:1; } }
.ca-group { display:flex; align-items:center; gap:2px; padding:2px; border-radius:9px;
  background:rgba(255,255,255,.05); }
.ca-btn { display:flex; align-items:center; gap:6px; border:0; border-radius:7px; padding:8px 10px;
  background:transparent; color:#fff; font:inherit; font-weight:500; cursor:pointer; line-height:0; }
.ca-btn:hover { background:rgba(255,255,255,.12); }
.ca-btn:focus-visible { outline:2px solid #93c5fd; outline-offset:1px; }
.ca-btn.on { background:#2563eb; }
.ca-btn.on:hover { background:#1d4ed8; }
.ca-btn:disabled { opacity:.45; cursor:default; }
.ca-btn:disabled:hover { background:transparent; }
.ca-primary { background:#2563eb; font-weight:600; padding:9px 16px; }
.ca-primary:hover { background:#1d4ed8; }
.ca-primary:disabled:hover { background:#2563eb; }
.ca-spin { animation:ca-spin 1s linear infinite; }
@keyframes ca-spin { to { transform:rotate(360deg); } }
.ca-swatch { width:14px; height:14px; border-radius:50%; border:2px solid rgba(255,255,255,.85);
  flex-shrink:0; }
.ca-note { opacity:.75; padding:0 8px; max-width:280px; line-height:1.3; }
.ca-sep { width:1px; align-self:stretch; background:rgba(255,255,255,.14); margin:0 2px; }
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
        this.bar.setAttribute('role', 'toolbar');
        this.bar.setAttribute('aria-label', 'Annotation');

        this.root.append(this.canvas, this.bar);
        parent.appendChild(this.root);

        // Hidden until there is actually a shared screen to annotate. Offering "Request to
        // annotate" when nobody is sharing asks for permission to draw on nothing.
        this.setVisible(false);
    }

    /**
     * Rebuild the bar for the current permission state.
     *
     * A full rebuild (`this.bar.textContent = ''`, then reconstruct) is simple, but a keyboard user
     * who just pressed a tool button — the ONLY way this method gets called with a materially
     * different tool selection — had their now-detached button silently drop focus back to
     * `<body>` every single time, breaking Tab-order navigation right after the one interaction most
     * worth supporting well. `data-ca-key` tags each interactive control with a stable identity
     * across rebuilds so focus can be restored onto its replacement rather than lost.
     */
    render({ permission, color = '#ff3b30', canErase = true } = {}) {
        const focusedKey = this.bar.contains(document.activeElement)
            ? document.activeElement.dataset.caKey : null;

        this.bar.textContent = '';

        if (permission !== 'granted') {
            const pending = permission === 'pending';
            const btn = el('button', 'ca-btn ca-primary');
            btn.type = 'button';
            btn.dataset.caKey = 'request';
            btn.disabled = pending;
            btn.setAttribute('aria-busy', String(pending));
            btn.innerHTML = pending
                ? `<span class="ca-spin">${svg('spinner', 15)}</span><span>Waiting for approval…</span>`
                : '<span>Request to annotate</span>';
            btn.onclick = () => this._cb.onRequest?.();
            this.bar.append(btn);

            const note = {
                denied: 'Your request was declined.',
                revoked: 'Annotation access was withdrawn.',
            }[permission];
            if (note) this.bar.append(el('span', 'ca-note', note));
            this._restoreFocus(focusedKey);
            return;
        }

        const sw = el('span', 'ca-swatch');
        sw.style.background = color;
        sw.title = 'Your colour, assigned by the person sharing';
        sw.setAttribute('aria-hidden', 'true');
        this.bar.append(sw, sep());

        const group = el('span', 'ca-group');
        group.setAttribute('role', 'group');
        group.setAttribute('aria-label', 'Drawing tools');
        for (const t of TOOLS) {
            if (t.tool === 'erase' && !canErase) continue;
            const on = this._tool === t.tool;
            const b = el('button', `ca-btn${on ? ' on' : ''}`);
            b.type = 'button';
            b.dataset.caKey = `tool:${t.tool}`;
            b.title = t.label;
            b.setAttribute('aria-label', t.label);
            b.setAttribute('aria-pressed', String(on));
            b.innerHTML = svg(t.icon);
            b.onclick = () => {
                // Clicking the active tool turns drawing off — otherwise there is no way back to a
                // click-through canvas without a separate "off" button.
                this._tool = this._tool === t.tool ? null : t.tool;
                this._cb.onTool?.(this._tool);
                this.render({ permission, color, canErase });
            };
            group.append(b);
        }
        this.bar.append(group, sep());

        const undo = el('button', 'ca-btn');
        undo.type = 'button';
        undo.dataset.caKey = 'undo';
        undo.title = 'Undo';
        undo.setAttribute('aria-label', 'Undo my last stroke');
        undo.innerHTML = svg('undo');
        undo.onclick = () => this._cb.onUndo?.();

        const clear = el('button', 'ca-btn');
        clear.type = 'button';
        clear.dataset.caKey = 'clear';
        clear.title = 'Clear my marks';
        clear.setAttribute('aria-label', 'Clear my marks');
        clear.innerHTML = svg('clear');
        clear.onclick = () => this._cb.onClear?.();

        this.bar.append(undo, clear);
        this._restoreFocus(focusedKey);
    }

    /** Focus the rebuilt control with the same `data-ca-key` the previously-focused one had, if any. */
    _restoreFocus(key) {
        if (!key) return;
        this.bar.querySelector(`[data-ca-key="${key}"]`)?.focus();
    }

    /**
     * Show or hide the whole UI.
     *
     * The toolbar exists only while someone is sharing a screen. With no share there is nothing to
     * draw on, so the correct amount of UI is none — not a button that would request permission to
     * annotate a screen that does not exist.
     */
    setVisible(on) {
        this.root.style.display = on ? '' : 'none';
        this._visible = !!on;
    }

    get visible() {
        return this._visible;
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

function sep() {
    const s = el('span', 'ca-sep');
    s.setAttribute('aria-hidden', 'true');
    return s;
}
