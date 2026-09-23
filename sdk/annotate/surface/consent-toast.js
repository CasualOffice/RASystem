// Casual Annotate — the sharer's consent prompt, browser side (ADR-107 §9).
//
// A corner toast, not a full-screen dimming modal. The earlier full-screen version blocked the
// entire meeting view — video, chat, everything — for a decision that is genuinely lightweight
// ("can this one person draw marks, yes or no"), which is a mismatch industry practice already
// settled: Zoom, Teams and Meet all surface "X wants to…" as a corner notification, not a takeover.
// The security posture does not change — deny-by-default, explicit refusal, no silent timeout — only
// how much of the screen it's allowed to cover while asking.
//
// `askToAllow` deliberately does not go through `SharerController.approve/reject` itself; the caller
// (`standalone/inject.js`) does that, exactly as before. This module only decides what the person
// sees and returns their answer.

const AUTO_DENY_MS = 30_000;
const TOAST_GAP = 10;

/**
 * Open toasts, top to bottom, so two near-simultaneous requests stack instead of exactly overlapping
 * (which — before this — silently hid the earlier one behind the later one while it kept counting
 * down to its own auto-deny underneath, invisible).
 * @type {HTMLElement[]}
 */
const openToasts = [];

function restack() {
    let top = 16;
    for (const wrap of openToasts) {
        wrap.style.top = `${top}px`;
        top += wrap.offsetHeight + TOAST_GAP;
    }
}

const CSS = `
.ca-toast-wrap { position:fixed; top:16px; right:16px; z-index:2147483601;
  font:14px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif; transition:top .15s ease; }
.ca-toast { width:320px; background:#18181b; color:#fff; border-radius:12px;
  box-shadow:0 20px 60px rgba(0,0,0,.55); overflow:hidden;
  animation:ca-toast-in .18s cubic-bezier(.2,.7,.3,1); }
@keyframes ca-toast-in { from { transform:translateX(24px); opacity:0; } to { transform:none; opacity:1; } }
.ca-toast-top { display:flex; gap:10px; padding:14px 14px 10px; align-items:flex-start; }
.ca-toast-avatar { flex-shrink:0; width:32px; height:32px; border-radius:50%; background:#3f3f46;
  display:flex; align-items:center; justify-content:center; font-weight:700; font-size:13px; }
.ca-toast-body { flex:1; min-width:0; }
.ca-toast-title { font-weight:600; margin-bottom:2px; }
.ca-toast-who { font-weight:700; }
.ca-toast-sub { opacity:.65; font-size:12.5px; }
.ca-toast-actions { display:flex; gap:8px; padding:0 14px 14px; }
.ca-toast-btn { flex:1; padding:8px 0; border:0; border-radius:8px; font:inherit; font-weight:600;
  cursor:pointer; }
.ca-toast-btn:focus-visible { outline:2px solid #93c5fd; outline-offset:2px; }
.ca-toast-deny { background:#3f3f46; color:#fff; }
.ca-toast-deny:hover { background:#52525b; }
.ca-toast-allow { background:#2563eb; color:#fff; }
.ca-toast-allow:hover { background:#1d4ed8; }
.ca-toast-timer { height:3px; background:rgba(255,255,255,.12); }
.ca-toast-timer-bar { height:100%; background:#2563eb; width:100%;
  transition:width linear; }
`;

/** Exported for `test/consent-toast.test.js` — the only piece of this file with no DOM in it. */
export function initials(name) {
    const parts = String(name || '?').trim().split(/\s+/).slice(0, 2);
    return parts.map(p => p[0]?.toUpperCase() ?? '').join('') || '?';
}

/**
 * @param {{ name?: string }} req
 * @param {object} [opts]
 * @param {HTMLElement} [opts.parent]
 * @param {number} [opts.timeoutMs] - auto-deny after this long with no answer. A refusal is always
 *   sent explicitly (ADR-107 §9.1) — a silent timeout is indistinguishable from a dropped message and
 *   leaves the requester's UI spinning forever, which is exactly what this exists to avoid.
 * @returns {Promise<boolean>}
 */
export function askToAllow({ name } = {}, { parent = document.body, timeoutMs = AUTO_DENY_MS } = {}) {
    return new Promise((resolve) => {
        if (!document.getElementById('ca-toast-style')) {
            const st = document.createElement('style');
            st.id = 'ca-toast-style';
            st.textContent = CSS;
            document.head.appendChild(st);
        }

        const who = String(name || 'A participant').slice(0, 64);

        const wrap = document.createElement('div');
        wrap.className = 'ca-toast-wrap';

        const card = document.createElement('div');
        card.className = 'ca-toast';
        card.setAttribute('role', 'alertdialog');
        card.setAttribute('aria-live', 'assertive');
        card.setAttribute('aria-label', `${who} wants to draw on your shared screen`);

        const top = document.createElement('div');
        top.className = 'ca-toast-top';
        const avatar = document.createElement('div');
        avatar.className = 'ca-toast-avatar';
        avatar.textContent = initials(who);   // neutral bubble — colour is assigned only on approval
        avatar.setAttribute('aria-hidden', 'true');
        const body = document.createElement('div');
        body.className = 'ca-toast-body';
        const title = document.createElement('div');
        title.className = 'ca-toast-title';
        const whoEl = document.createElement('span');
        whoEl.className = 'ca-toast-who';
        whoEl.textContent = who;              // textContent, never innerHTML — remote input
        title.append(whoEl, document.createTextNode(' wants to annotate'));
        const sub = document.createElement('div');
        sub.className = 'ca-toast-sub';
        // The expiry is stated here, once, rather than announced again as the countdown ticks — the
        // card's `aria-live="assertive"` already announces this whole block on mount, so assistive
        // tech hears the deadline up front. A live region that re-announces every second or so would
        // be actively hostile to a screen-reader user; the visual timer bar is sighted-only by design.
        sub.textContent = 'They can only draw marks — never click, type, or control anything. '
            + `This request expires in ${Math.round(timeoutMs / 1000)} seconds.`;
        body.append(title, sub);
        top.append(avatar, body);

        const actions = document.createElement('div');
        actions.className = 'ca-toast-actions';
        const denyBtn = document.createElement('button');
        denyBtn.type = 'button';
        denyBtn.className = 'ca-toast-btn ca-toast-deny';
        denyBtn.textContent = 'Deny';
        const allowBtn = document.createElement('button');
        allowBtn.type = 'button';
        allowBtn.className = 'ca-toast-btn ca-toast-allow';
        allowBtn.textContent = 'Allow';
        actions.append(denyBtn, allowBtn);

        const timer = document.createElement('div');
        timer.className = 'ca-toast-timer';
        const timerBar = document.createElement('div');
        timerBar.className = 'ca-toast-timer-bar';
        timer.append(timerBar);

        card.append(top, actions, timer);
        wrap.append(card);
        parent.appendChild(wrap);
        openToasts.push(wrap);
        restack();

        let settled = false;
        let timeoutId = null;
        const done = (allowed) => {
            if (settled) return;
            settled = true;
            if (timeoutId !== null) clearTimeout(timeoutId);
            card.removeEventListener('keydown', onKey);
            wrap.remove();
            const i = openToasts.indexOf(wrap);
            if (i !== -1) openToasts.splice(i, 1);
            restack();
            // Return focus to wherever it was before this interrupted the user, rather than letting
            // it silently fall back to <body> — that "focus just vanishes" is disorienting after any
            // modal-ish interruption, and doubly so right after something the user did not initiate.
            if (restoreTo && document.contains(restoreTo) && typeof restoreTo.focus === 'function') {
                restoreTo.focus();
            }
            resolve(allowed);
        };

        // Scoped to the CARD, not `document` — an earlier version listened globally in the capture
        // phase, so pressing Escape ANYWHERE on the page (closing an unrelated menu, backing out of
        // something else entirely) silently denied a request the user may not even have consciously
        // registered yet. Keydown bubbles from wherever focus currently is; since focus starts inside
        // `card` and the only focusable controls in this toast are Allow/Deny, this only ever fires
        // while the user is actually engaged with the toast.
        const onKey = (e) => {
            if (e.key === 'Escape') done(false);
        };
        card.addEventListener('keydown', onKey);

        denyBtn.onclick = () => done(false);
        allowBtn.onclick = () => done(true);

        // Don't steal focus out of an active text field (mid-chat-message being the obvious case): a
        // toast appearing there and grabbing focus means the user's very next keystroke — a space in
        // an ordinary sentence — lands on a focused button instead of their message, silently firing
        // whatever that button does. The toast, the aria-live announcement, and the visible countdown
        // still all happen either way; only the forced focus move is skipped.
        const priorFocus = document.activeElement;
        const isTextEditing = priorFocus && (
            priorFocus.tagName === 'INPUT' || priorFocus.tagName === 'TEXTAREA' || priorFocus.isContentEditable
        );
        const restoreTo = isTextEditing ? priorFocus : (priorFocus && priorFocus !== document.body ? priorFocus : null);
        if (!isTextEditing) {
            // Deny is the default focus target — the safe answer needs no thought, matching the
            // native dialog's `defaultId: 1` convention (`adapters/jitsi-electron/main.js`).
            denyBtn.focus();
        }

        // Visible countdown to an explicit auto-deny — never silence (§9.1's "a refusal is sent
        // explicitly" rule applies here just as much as to a human clicking Deny).
        requestAnimationFrame(() => {
            timerBar.style.transitionDuration = `${timeoutMs}ms`;
            timerBar.style.width = '0%';
        });
        timeoutId = setTimeout(() => done(false), timeoutMs);
    });
}
