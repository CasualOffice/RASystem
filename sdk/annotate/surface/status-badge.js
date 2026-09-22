// Casual Annotate — the sharer's always-visible "annotation is on" indicator, browser side
// (Invariant 7, ADR-107 §9.4).
//
// This invariant was claimed but never actually built: the design doc and ADR-107 both describe an
// "always-visible indicator … reusing the RAS badge," and grepping the whole SDK tree for
// badge/indicator turns up nothing. This closes that gap for the case where the sharer is a plain
// BROWSER tab: there is no overlay window at all (§13 — a browser cannot host one), so there is no
// capturable surface to draw a badge into, and the only place this can honestly promise "always
// visible" is the sharer's own local view. Remote viewers see nothing extra for a browser sharer,
// which is correct: there is genuinely nothing to put in the outgoing video (unlike Electron, whose
// equivalent lives in `overlay/renderer.js` and IS inside the captured pixels, because that overlay
// genuinely exists).

const CSS = `
.ca-badge { position:fixed; left:16px; bottom:16px; z-index:2147483001;
  font:12.5px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif; color:#fff;
  background:rgba(24,24,27,.94); border-radius:999px; padding:6px 12px 6px 10px;
  display:flex; align-items:center; gap:7px; box-shadow:0 6px 20px rgba(0,0,0,.35);
  pointer-events:none; user-select:none; }
.ca-badge[hidden] { display:none; }
.ca-badge-dot { width:8px; height:8px; border-radius:50%; background:#ef4444;
  animation:ca-badge-pulse 1.6s ease-in-out infinite; flex-shrink:0; }
@keyframes ca-badge-pulse { 0%,100% { opacity:1; } 50% { opacity:.4; } }
.ca-badge-text { white-space:nowrap; }
`;

export class StatusBadge {
    /** @param {object} [opts] @param {HTMLElement} [opts.parent] */
    constructor({ parent = document.body } = {}) {
        if (!document.getElementById('ca-badge-style')) {
            const st = document.createElement('style');
            st.id = 'ca-badge-style';
            st.textContent = CSS;
            document.head.appendChild(st);
        }
        this.root = document.createElement('div');
        this.root.className = 'ca-badge';
        this.root.setAttribute('role', 'status');
        this.root.hidden = true;
        this.root.innerHTML = '<span class="ca-badge-dot" aria-hidden="true"></span>'
            + '<span class="ca-badge-text"></span>';
        this._text = this.root.querySelector('.ca-badge-text');
        parent.appendChild(this.root);
    }

    /**
     * @param {{ on: boolean, count?: number }} state - `on`: is annotation enabled at all right now
     *   (Inv 1 — off by default). `count`: how many participants currently hold permission to draw.
     */
    render({ on, count = 0 }) {
        this.root.hidden = !on;
        if (!on) return;
        this._text.textContent = count === 0
            ? 'Annotation is on — nobody can draw yet'
            : `Annotation is on — ${count} ${count === 1 ? 'person' : 'people'} can draw`;
    }

    destroy() {
        this.root.remove();
    }
}
