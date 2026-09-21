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

import { dialog, ipcMain } from 'electron';

import { AnnotationOverlayWindow } from '../../overlay/window.js';
import { CH } from './channels.js';

export { CH };

class AnnotateMain {
    constructor(hostWindow, opts = {}) {
        this._host = hostWindow;
        this._overlay = new AnnotationOverlayWindow({
            url: opts.overlayUrl,
            preload: opts.overlayPreload,
            // Windows needs the native addon to locate a display; unused on macOS/Linux. Injected
            // rather than imported so this module never hard-depends on a native build.
            sourceId2Coordinates: opts.sourceId2Coordinates,
        });
        this._ready = false;
        this._sourceId = null;

        this._watchScreenShareSource();

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

        // The consent prompt. This is the whole point of the feature's safety story, so it is a
        // NATIVE dialog on the sharer's own desktop — not something drawn inside the meeting UI,
        // where a hostile page could imitate it.
        ipcMain.handle(CH.ASK, async (_e, { id, name }) => {
            const who = String(name || id || 'A participant').slice(0, 64);
            const { response } = await dialog.showMessageBox(this._host, {
                type: 'question',
                buttons: [ 'Allow', 'Deny' ],
                defaultId: 1,          // Deny is the default — the safe answer needs no thought
                cancelId: 1,           // dismissing the dialog denies
                title: 'Annotation request',
                message: `${who} wants to draw on your shared screen.`,
                detail: 'They will be able to draw marks on the screen you are sharing. '
                    + 'They cannot click, type, or control anything.',
                noLink: true,
            });
            return { allowed: response === 0 };
        });
    }

    /**
     * Learn which display is being shared.
     *
     * The iframe API never tells us: `screensharingDetails` carries only `sourceType`
     * (`actions.web.ts:144`), never the source id. The id exists only in the main process, inside
     * the `setDisplayMediaRequestHandler` callback that `@jitsi/electron-sdk` installs.
     *
     * So we wrap that setter before the SDK calls it and observe the source the user picked. We do
     * not change the picker or the result — the original callback is invoked with exactly what it
     * would have received. `setupAnnotateMain` MUST therefore run before `setupScreenSharingMain`.
     */
    _watchScreenShareSource() {
        const session = this._host?.webContents?.session;
        if (!session || session.__casualAnnotateWrapped) return;

        const original = session.setDisplayMediaRequestHandler.bind(session);
        session.setDisplayMediaRequestHandler = (handler, opts) => original((request, callback) => {
            const observe = (result) => {
                const id = result?.video?.id ?? result?.video?.sourceId ?? null;
                this._sourceId = id;
                if (id) this._toHost(CH.SOURCE, { sourceId: id });
                callback(result);
            };
            return handler(request, observe);
        }, opts);
        session.__casualAnnotateWrapped = true;
    }

    /** The source id of the live share, or null. */
    get sourceId() {
        return this._sourceId;
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
        for (const c of [ CH.START, CH.STOP, CH.ASK ]) ipcMain.removeHandler(c);
        for (const c of [ CH.READY, CH.OP, CH.CONTROL, CH.EMIT, CH.STATE ]) ipcMain.removeAllListeners(c);
        this._overlay.destroy();
    }
}

/**
 * @param {import('electron').BrowserWindow} jitsiMeetWindow
 * @param {{ overlayUrl: string, overlayPreload?: string,
 *           sourceId2Coordinates?: (id: string) => ({x:number,y:number}|undefined) }} opts
 */
export default function setupAnnotateMain(jitsiMeetWindow, opts) {
    return new AnnotateMain(jitsiMeetWindow, opts);
}
