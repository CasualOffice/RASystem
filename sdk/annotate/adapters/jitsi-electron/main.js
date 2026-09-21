// Casual Annotate — Electron MAIN process wiring (ADR-107 §8).
//
// Mirrors `@jitsi/electron-sdk`'s remote-control split: the renderer relays, the privileged side
// does the work. Here that is one transparent always-on-top window on the shared display — and,
// unlike remote control, NO input injection at all. There is no robotjs here and there never should
// be: "display data only" is what keeps this feature droppable into a third-party product.
//
// ── Where the session lives, and why it matters ─────────────────────────────────────────────────
//
// The annotation state lives in the OVERLAY WINDOW, not in the meeting renderer.
//
// The obvious arrangement — session in the renderer, overlay as a dumb canvas — means serialising
// every stroke across a process boundary on every frame: up to 256 strokes of up to 1024 points, at
// 60 Hz, through structured clone. The ops themselves are tiny and arrive a few times a second, so
// forwarding OPS instead of STATE is orders of magnitude cheaper and keeps exactly one copy of the
// truth. Main is therefore a relay in both directions:
//
//     meeting renderer ──(sender, op)──► main ──► overlay window   (applies + renders)
//     meeting renderer ◄──────(ack)──── main ◄──                   (back onto the transport)

import { ipcMain } from 'electron';

import { AnnotationOverlayWindow } from '../../overlay/window.js';

/** IPC channels, namespaced so they cannot collide with the host app's own. */
export const CH = Object.freeze({
    START: 'casual-annotate:start',     // renderer → main   (invoke) show the overlay
    STOP: 'casual-annotate:stop',       // renderer → main   (invoke)
    OP: 'casual-annotate:op',           // renderer → main → overlay: one received op
    CONTROL: 'casual-annotate:control', // renderer → main → overlay: admit/mute/clear
    EMIT: 'casual-annotate:emit',       // overlay  → main → renderer: an op to put on the wire
    STATE: 'casual-annotate:state',     // overlay  → main → renderer: counts, roster, legacy peers
    READY: 'casual-annotate:ready',     // overlay  → main
});

class AnnotateMain {
    constructor(hostWindow, opts = {}) {
        this._host = hostWindow;
        this._overlay = new AnnotationOverlayWindow({
            url: opts.overlayUrl,
            // Windows needs the native addon to locate a display; unused on macOS/Linux. Injected
            // rather than imported so this module never hard-depends on a native build.
            sourceId2Coordinates: opts.sourceId2Coordinates,
        });
        this._ready = false;

        // `handle`, not `on`: a refusal (window share, unresolvable Linux multi-monitor) has to
        // reach the UI. Swallowing it would leave remote participants drawing into nothing while
        // the interface claims annotation is on.
        ipcMain.handle(CH.START, (_e, sourceId) => this._overlay.show(sourceId));
        ipcMain.handle(CH.STOP, () => {
            this._overlay.hide();
            this._ready = false;
            return { ok: true };
        });

        ipcMain.on(CH.READY, () => {
            this._ready = true;
        });
        ipcMain.on(CH.OP, (_e, payload) => this._toOverlay(CH.OP, payload));
        ipcMain.on(CH.CONTROL, (_e, payload) => this._toOverlay(CH.CONTROL, payload));
        ipcMain.on(CH.EMIT, (_e, payload) => this._toHost(CH.EMIT, payload));
        ipcMain.on(CH.STATE, (_e, payload) => this._toHost(CH.STATE, payload));
    }

    _toOverlay(channel, payload) {
        if (!this._ready) return;
        this._overlay.webContents?.send(channel, payload);
    }

    _toHost(channel, payload) {
        if (this._host?.isDestroyed?.()) return;
        this._host.webContents.send(channel, payload);
    }

    dispose() {
        for (const c of [ CH.START, CH.STOP ]) ipcMain.removeHandler(c);
        for (const c of [ CH.READY, CH.OP, CH.CONTROL, CH.EMIT, CH.STATE ]) ipcMain.removeAllListeners(c);
        this._overlay.destroy();
    }
}

/**
 * @param {import('electron').BrowserWindow} jitsiMeetWindow
 * @param {{ overlayUrl: string, sourceId2Coordinates?: (id: string) => ({x:number,y:number}|undefined) }} opts
 */
export default function setupAnnotateMain(jitsiMeetWindow, opts) {
    return new AnnotateMain(jitsiMeetWindow, opts);
}
