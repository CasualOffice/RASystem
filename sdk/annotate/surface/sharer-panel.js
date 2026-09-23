// Casual Annotate — the sharer's annotator-management panel (ADR-107 §9.4).
//
// Before this, muting or revoking an individual annotator was reachable only from the JS console —
// `SharerController.mute(id)` / `withdraw(id)`, typed by hand. `SharerController.state()` already
// computes everything a panel needs (`participants: [{id, color, name, muted}]`); it just never had
// a UI. This is that UI: a small, collapsible, always-available list next to the annotation badge.
//
// Framework-free, matching `surface/toolbar.js` — a template-literal stylesheet plus plain DOM. No
// build step, so it works in both the browser (`standalone/inject.js`) and, unmounted from the
// overlay itself (ADR-100: the overlay must never be interactive), the Electron host renderer.

const CSS = `
.ca-panel { position:fixed; right:16px; bottom:96px; z-index:2147483001;
  font:13px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif; color:#fff;
  background:rgba(24,24,27,.94); border-radius:12px; box-shadow:0 8px 28px rgba(0,0,0,.45);
  width:240px; max-height:60vh; overflow:hidden; display:flex; flex-direction:column;
  pointer-events:auto; user-select:none; }
.ca-panel[hidden] { display:none; }
.ca-panel-head { display:flex; align-items:center; gap:8px; padding:10px 12px; cursor:pointer; }
.ca-panel-head:hover { background:rgba(255,255,255,.06); }
.ca-panel-title { flex:1; font-weight:600; }
.ca-panel-count { opacity:.6; font-variant-numeric:tabular-nums; }
.ca-panel-chevron { transition:transform .15s ease; opacity:.7; }
.ca-panel.open .ca-panel-chevron { transform:rotate(180deg); }
.ca-panel-body { display:none; overflow-y:auto; border-top:1px solid rgba(255,255,255,.1); }
.ca-panel.open .ca-panel-body { display:block; }
.ca-panel-empty { padding:14px 12px; opacity:.6; }
.ca-row { display:flex; align-items:center; gap:8px; padding:8px 12px; }
.ca-row:hover { background:rgba(255,255,255,.05); }
.ca-row-swatch { width:10px; height:10px; border-radius:50%; flex-shrink:0; }
.ca-row-name { flex:1; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
.ca-row-name.muted { opacity:.5; text-decoration:line-through; }
.ca-row-name.pending { opacity:.6; font-style:italic; }
.ca-row-btn { border:0; background:transparent; color:#fff; opacity:.7; cursor:pointer;
  padding:4px 6px; border-radius:6px; font:inherit; font-size:12px; }
.ca-row-btn:hover { opacity:1; background:rgba(255,255,255,.1); }
.ca-row-btn:focus-visible { outline:2px solid #93c5fd; outline-offset:1px; }
.ca-row-btn.danger:hover { background:rgba(239,68,68,.25); }
.ca-row-btn:disabled { opacity:.3; cursor:default; }
.ca-row-btn:disabled:hover { background:transparent; }
`;

export class SharerPanel {
    /**
     * @param {object} opts
     * @param {HTMLElement} [opts.parent]
     * @param {(id: string) => void} opts.onMute
     * @param {(id: string) => void} opts.onUnmute
     * @param {(id: string) => void} opts.onRevoke
     */
    constructor({ parent = document.body, onMute, onUnmute, onRevoke } = {}) {
        this._cb = { onMute, onUnmute, onRevoke };
        this._open = false;

        if (!document.getElementById('ca-panel-style')) {
            const st = document.createElement('style');
            st.id = 'ca-panel-style';
            st.textContent = CSS;
            document.head.appendChild(st);
        }

        this.root = document.createElement('div');
        this.root.className = 'ca-panel';
        this.root.hidden = true;

        this.head = document.createElement('div');
        this.head.className = 'ca-panel-head';
        this.head.setAttribute('role', 'button');
        this.head.setAttribute('aria-expanded', 'false');
        this.head.setAttribute('aria-controls', 'ca-panel-body');
        this.head.tabIndex = 0;
        this.head.innerHTML = `<span class="ca-panel-title">Annotators</span>
            <span class="ca-panel-count">0</span>
            <span class="ca-panel-chevron" aria-hidden="true">▾</span>`;
        this._count = this.head.querySelector('.ca-panel-count');
        const toggle = () => this._setOpen(!this._open);
        this.head.onclick = toggle;
        this.head.onkeydown = e => (e.key === 'Enter' || e.key === ' ') && (e.preventDefault(), toggle());

        this.body = document.createElement('div');
        this.body.className = 'ca-panel-body';
        this.body.id = 'ca-panel-body';

        this.root.append(this.head, this.body);
        parent.appendChild(this.root);

        /** @type {Map<string, {row: HTMLElement, sw: HTMLElement, name: HTMLElement,
         *   muteBtn: HTMLElement, revokeBtn: HTMLElement}>} keyed by participant id, so an
         *   unaffected row's DOM nodes are never touched — see `render()`. */
        this._rows = new Map();
    }

    _setOpen(on) {
        this._open = on;
        this.root.classList.toggle('open', on);
        this.head.setAttribute('aria-expanded', String(on));
    }

    /**
     * Redraw from a `SharerController.state()` snapshot. Never called with stroke geometry — that
     * would be the expensive mistake (`sharer.js` never returns it in the first place).
     *
     * A diff against the previous render, not a clear-and-rebuild. `SharerController` pushes a new
     * state on EVERY change it makes — including every incoming `cursor` op, which arrives at up to
     * ~20 Hz per active annotator (`core/session.js`'s rate limit). Rebuilding the whole list that
     * often tore out and recreated a button mid-click and threw keyboard focus back to `<body>` on
     * every single pointer move any annotator made, which is a real, constant hazard for anyone
     * trying to actually use Mute/Remove while someone is drawing.
     *
     * @param {{participants: Array<{id: string, color: string, name: string,
     *   granted: boolean, muted: boolean}>}} state
     */
    render(state) {
        const participants = state?.participants ?? [];
        this.root.hidden = participants.length === 0;
        this._count.textContent = String(participants.length);

        const seen = new Set();
        let prevRow = null;
        for (const p of participants) {
            seen.add(p.id);
            let entry = this._rows.get(p.id);
            if (!entry) entry = this._createRow(p.id);
            this._updateRow(entry, p);
            // Keep DOM order matching `participants`' order without recreating any node — `after()`
            // is a no-op if the row is already in the right place.
            if (prevRow) prevRow.after(entry.row); else this.body.prepend(entry.row);
            prevRow = entry.row;
        }
        for (const [ id, entry ] of this._rows) {
            if (seen.has(id)) continue;
            entry.row.remove();
            this._rows.delete(id);
        }
    }

    _createRow(id) {
        const row = document.createElement('div');
        row.className = 'ca-row';

        const sw = document.createElement('span');
        sw.className = 'ca-row-swatch';
        sw.setAttribute('aria-hidden', 'true');

        const name = document.createElement('span');
        name.className = 'ca-row-name';

        const muteBtn = document.createElement('button');
        muteBtn.className = 'ca-row-btn';
        muteBtn.type = 'button';

        const revokeBtn = document.createElement('button');
        revokeBtn.className = 'ca-row-btn danger';
        revokeBtn.type = 'button';
        revokeBtn.textContent = 'Remove';
        revokeBtn.onclick = () => this._cb.onRevoke?.(id);

        row.append(sw, name, muteBtn, revokeBtn);
        const entry = { row, sw, name, muteBtn, revokeBtn };
        this._rows.set(id, entry);
        return entry;
    }

    /** Update one row's content/attributes in place — the DOM nodes themselves never change. */
    _updateRow(entry, p) {
        entry.sw.style.background = p.color;

        // Three real states, not two: never granted (nothing to mute/unmute — the "Mute" toggle was
        // previously shown, and did nothing, for someone who had simply never been given access at
        // all), granted-and-active, granted-and-muted.
        const label = !p.granted ? `${p.name} — not yet allowed to draw`
            : p.muted ? `${p.name} (muted)` : p.name;
        entry.name.className = `ca-row-name${!p.granted ? ' pending' : p.muted ? ' muted' : ''}`;
        entry.name.textContent = String(p.name ?? p.id);   // textContent, never innerHTML — remote input
        entry.name.title = label;

        entry.muteBtn.hidden = !p.granted;
        entry.muteBtn.textContent = p.muted ? 'Unmute' : 'Mute';
        entry.muteBtn.setAttribute('aria-label', `${p.muted ? 'Unmute' : 'Mute'} ${p.name}`);
        entry.muteBtn.onclick = () => (p.muted ? this._cb.onUnmute : this._cb.onMute)?.(p.id);

        entry.revokeBtn.disabled = !p.granted;
        entry.revokeBtn.setAttribute('aria-label',
            p.granted ? `Remove ${p.name}'s annotation access` : `${p.name} has not been granted access`);
    }

    destroy() {
        this.root.remove();
    }
}
