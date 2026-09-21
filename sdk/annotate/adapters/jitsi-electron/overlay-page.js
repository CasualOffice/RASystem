// Casual Annotate — the overlay window's page script (ADR-107 §8).
//
// Runs inside the transparent, click-through, always-on-top window. It owns the authoritative
// `SharerSession` — see the note in `main.js` for why the state lives here rather than in the
// meeting renderer — applies ops as they arrive, and renders continuously.
//
// This surface NEVER receives input. If you reach for `setIgnoreMouseEvents(false)`, re-read
// ADR-100: an interactive overlay is what produced the macOS white screen and the hidden context
// menus that made annotation unusable in the first place.

import { SharerController } from '../../sharer.js';
import { OverlayRenderer } from '../../overlay/renderer.js';

const bridge = window.casualAnnotateOverlay;

// All the sharer logic lives in the controller; this page supplies the two things only it can —
// an `emit` that crosses IPC, and a canvas.
const controller = new SharerController({
    emit: msg => bridge.emit(msg),
    onState: state => bridge.state(state),
});
const session = controller.session;

const canvas = document.getElementById('annotate-overlay');
const renderer = new OverlayRenderer(canvas, session);

/** Ops arriving from the meeting renderer, already off the relay. */
bridge.onOp(({ sender, msg }) => controller.handle(sender, msg));

/** Controls from the meeting UI. */
bridge.onControl((c) => {
    switch (c.type) {
        case 'admit': return controller.setAdmit(c.mode);
        case 'participants': return controller.syncParticipants(c.participants);
        case 'mute': return controller.mute(c.id);
        case 'unmute': return controller.unmute(c.id);
        case 'clear': return controller.clearAll();
        case 'revoke': return controller.revoke();
        default: return; // unknown control: ignore, never throw across the bridge
    }
});


// Expire stale cursors even when nothing else is happening, so a departed pointer never sits on the
// shared screen. The renderer also expires on its own frame; this keeps the UI counter honest.
setInterval(() => controller.tick(), 1000);

// Diagnostic only. The overlay has no DOM worth inspecting — it is one canvas — so this is how an
// automated check, or a developer in devtools, can ask what the session actually holds rather than
// inferring it from pixels. Read-only, and nothing in the SDK depends on it.
Object.defineProperty(window, '__annotateStrokeCount', { get: () => session.store.size });
Object.defineProperty(window, '__annotateCursorCount', { get: () => session.cursors.size });

window.addEventListener('beforeunload', () => renderer.dispose());

bridge.ready();
