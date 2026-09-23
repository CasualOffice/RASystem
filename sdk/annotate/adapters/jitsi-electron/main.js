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
import { readFileSync } from 'node:fs';

import { AnnotationOverlayWindow } from '../../overlay/window.js';
import { CH } from './channels.js';

export { CH };

/**
 * The real, native consent surface (ADR-107 §9, Decision 9). A NATIVE dialog, not drawn inside the
 * meeting UI, where a hostile page could imitate it.
 */
async function defaultShowConsentDialog(hostWindow, { id, name }) {
    const who = String(name || id || 'A participant').slice(0, 64);
    const { response } = await dialog.showMessageBox(hostWindow, {
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
}

/** `new URL(u).origin`, or `null` for a malformed URL — never throws, never falls back to a substring match. */
function safeOrigin(u) {
    try {
        return new URL(u).origin;
    } catch {
        return null;
    }
}

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
        // Messages sent before the overlay page has loaded and signalled CH.READY — a real race on
        // the very first share, since CH.START resolves as soon as the BrowserWindow is shown, not
        // once its page has finished loading. Queued rather than dropped: an `admit` control message
        // lost here left `SharerSession._admit` stuck at ADMIT.NONE, silently rejecting every draw
        // for the rest of the share regardless of any later native-dialog approval.
        this._toOverlayQueue = [];
        this._sourceId = null;
        // Overridable so a test harness can script the answer instead of driving a real OS dialog
        // (ADR-107 Decision 10) — defaults to the real thing in every production wiring.
        this._showConsentDialog = opts.showConsentDialog ?? defaultShowConsentDialog;
        this._relaySource = opts.relayBundlePath ? readFileSync(opts.relayBundlePath, 'utf8') : null;
        this._serverOrigin = opts.serverOrigin ?? null;

        this._watchScreenShareSource();
        this._watchAndInjectRelay();

        // `handle`, not `on`: a refusal (window share, unresolvable Linux multi-monitor) has to
        // reach the UI. Swallowing it would leave remote participants drawing into nothing while
        // the interface claims annotation is on.
        ipcMain.handle(CH.START, (_e, sourceId) => this._overlay.show(sourceId));
        ipcMain.handle(CH.STOP, () => {
            this._overlay.hide();
            // NOT `this._ready = false` here. `AnnotationOverlayWindow.show()` creates its
            // `BrowserWindow` (and loads its page) only once and reuses it on every later share
            // (`overlay/window.js`); `hide()` never reloads it. So CH.READY — sent once, top-level,
            // by that page's own module init (`overlay-page.js`) — fires exactly once for the whole
            // app lifetime. Resetting `_ready` on every stop meant every share after the first had
            // its overlay silently and permanently deaf: `_toOverlay` dropped every op/control
            // forever, with annotation's own UI (the toolbar, the panel) still behaving as if it
            // worked. A completely ordinary flow — stop sharing, share again — broke the feature with
            // no error anywhere, which is exactly what this file's own design principle above (a
            // refusal must reach the UI) exists to prevent.
            return { ok: true };
        });

        ipcMain.on(CH.READY, () => {
            this._ready = true;
            const queued = this._toOverlayQueue.splice(0);
            for (const { channel, payload } of queued) this._toOverlay(channel, payload);
        });
        ipcMain.on(CH.OP, (_e, payload) => this._toOverlay(CH.OP, payload));
        ipcMain.on(CH.CONTROL, (_e, payload) => this._toOverlay(CH.CONTROL, payload));
        ipcMain.on(CH.EMIT, (_e, payload) => this._toHost(CH.EMIT, payload));
        ipcMain.on(CH.STATE, (_e, payload) => this._toHost(CH.STATE, payload));

        // The consent prompt. This is the whole point of the feature's safety story, so it is a
        // NATIVE dialog on the sharer's own desktop — not something drawn inside the meeting UI,
        // where a hostile page could imitate it.
        ipcMain.handle(CH.ASK, async (_e, { id, name }) => this._showConsentDialog(this._host, { id, name }));
    }

    /**
     * Inject the wire relay into the Jitsi iframe (ADR-107 Decision 10).
     *
     * `ExternalApiTransport` can send into the conference but can never receive
     * (`endpointTextMessageReceived` has zero callers in jitsi-meet's web app — Decision 8), so a
     * request from an annotator could never reach this app. The fix is not a workaround on our side
     * of the iframe boundary; it is running real code INSIDE it, where `lib-jitsi-meet`'s own event
     * actually fires. `webContents.mainFrame`'s `executeJavaScript` is how a host application does
     * that — a privileged capability a real cross-origin page could never grant itself.
     *
     * Re-injected on every `did-frame-finish-load` for the matching frame: a rejoin navigates it,
     * which wipes out anything previously injected along with it. `injected-relay.js`'s own
     * `window.__casualAnnotateRelay` guard makes a same-document re-injection (two load events for
     * one live document) a safe no-op rather than a second relay racing the first.
     */
    _watchAndInjectRelay() {
        const wc = this._host?.webContents;
        if (!wc || !this._relaySource) return; // no `relayBundlePath` given: nothing to inject

        const wantedOrigin = this._serverOrigin ? safeOrigin(this._serverOrigin) : null;
        const inject = () => {
            const frames = wc.mainFrame?.framesInSubtree ?? [];
            for (const frame of frames) {
                if (frame === wc.mainFrame) continue; // never the host's own top-level document
                // Match the configured server's ORIGIN exactly, never an unrelated sub-frame this app
                // happens to host. A `startsWith` string check here was a real bypass: a hostile frame
                // at `https://meet.example.com.evil.com` (or `https://meet.example.com@evil.com/…`)
                // "starts with" `https://meet.example.com` too, and would have received the relay.
                if (wantedOrigin && safeOrigin(frame.url) !== wantedOrigin) continue;
                frame.executeJavaScript(this._relaySource).catch(() => {
                    // The frame can navigate away between the check above and this call; a lost race
                    // is harmless; a real failure re-attempts on the next load event regardless.
                });
            }
        };
        this._injectRelay = inject;
        wc.on('did-frame-finish-load', inject);
        wc.on('did-frame-navigate', inject);
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
        if (!session) return;

        // Keyed on the SESSION (`session.__casualAnnotateObservers`), not this instance, because
        // Electron sessions are commonly shared across windows — a `partition`, or the default
        // session used by more than one `BrowserWindow`. A single boolean guard here meant a SECOND
        // `AnnotateMain` on a shared session silently never observed a source id: the wrap only ever
        // happens once, so every instance after the first saw `setDisplayMediaRequestHandler` already
        // wrapped and skipped installing its own observation, with no error anywhere. Every instance
        // registers into the same observer list instead, so all of them still learn the source id.
        (session.__casualAnnotateObservers ??= new Set()).add(this);

        if (session.__casualAnnotateWrapped) return;
        session.__casualAnnotateWrapped = true;

        const original = session.setDisplayMediaRequestHandler.bind(session);
        session.setDisplayMediaRequestHandler = (handler, opts) => original((request, callback) => {
            const observe = (result) => {
                const id = result?.video?.id ?? result?.video?.sourceId ?? null;
                for (const instance of session.__casualAnnotateObservers) {
                    instance._sourceId = id;
                    if (id) instance._toHost(CH.SOURCE, { sourceId: id });
                }
                callback(result);
            };
            return handler(request, observe);
        }, opts);
    }

    /** The source id of the live share, or null. */
    get sourceId() {
        return this._sourceId;
    }

    _toOverlay(channel, payload) {
        if (!this._ready) {
            this._toOverlayQueue.push({ channel, payload });
            return;
        }
        this._overlay.webContents?.send(channel, payload);
    }

    _toHost(channel, payload) {
        if (this._host?.isDestroyed?.()) return;
        this._host.webContents.send(channel, payload);
    }

    dispose() {
        for (const c of [ CH.START, CH.STOP, CH.ASK ]) ipcMain.removeHandler(c);
        for (const c of [ CH.READY, CH.OP, CH.CONTROL, CH.EMIT, CH.STATE ]) ipcMain.removeAllListeners(c);
        if (this._injectRelay) {
            this._host?.webContents?.off('did-frame-finish-load', this._injectRelay);
            this._host?.webContents?.off('did-frame-navigate', this._injectRelay);
        }
        this._host?.webContents?.session?.__casualAnnotateObservers?.delete(this);
        this._overlay.destroy();
    }
}

/**
 * @param {import('electron').BrowserWindow} jitsiMeetWindow
 * @param {object} opts
 * @param {string} opts.overlayUrl
 * @param {string} [opts.overlayPreload]
 * @param {(id: string) => ({x:number,y:number}|undefined)} [opts.sourceId2Coordinates]
 * @param {string} [opts.relayBundlePath] - absolute path to the esbuild-bundled
 *   `injected-relay.js` IIFE. Without it, nothing is injected and the Electron adapter is as broken
 *   as before Decision 10 — see `adapters/jitsi-electron/README.md`.
 * @param {string} [opts.serverOrigin] - the configured jitsi-meet server URL prefix (e.g.
 *   `https://meet.example.com`). The relay is injected ONLY into a sub-frame whose URL starts with
 *   this, so a malicious or unrelated frame this app happens to host is never a target. Strongly
 *   recommended; omitting it injects into every non-main sub-frame.
 * @param {(hostWindow, req: {id: string, name: string}) => Promise<{allowed: boolean}>}
 *   [opts.showConsentDialog] - overrides the native `dialog.showMessageBox` prompt. Test-only seam —
 *   a real integration should never need this.
 */
export default function setupAnnotateMain(jitsiMeetWindow, opts) {
    return new AnnotateMain(jitsiMeetWindow, opts);
}
