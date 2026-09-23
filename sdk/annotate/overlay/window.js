// Casual Annotate — the Electron overlay window (ADR-107 §8, §8.1).
//
// Runs in the ELECTRON MAIN PROCESS. This mirrors the architecture `@jitsi/electron-sdk` already
// uses for remote control (`remotecontrol/{renderer,main}.js`): the renderer relays messages, and
// the main process owns the privileged work — here, a transparent always-on-top window positioned
// on the display being shared. We copy that shape, and none of its input injection: this SDK has no
// `robotjs` and no OS input path at all, which is what keeps its threat model small.
//
// The two hard parts are both platform problems, and both are documented below rather than
// discovered later.

import { BrowserWindow, screen } from 'electron';

/**
 * Resolve the display behind a `desktopCapturer` source id.
 *
 * Ported from `@jitsi/electron-sdk` `RemoteControlMain._getDisplay()` (`remotecontrol/main.js:323`),
 * whose per-platform mess is not incidental — it is what this actually takes:
 *
 *   Windows — needs the native `sourceId2Coordinates` addon, and must use ITS x/y, because
 *             Electron's "don't seem to respect the scale factors of the other displays".
 *   macOS   — the source id is `screen:<displayId>:0`; match `displayId` against `getAllDisplays()`.
 *   Linux   — UNRESOLVABLE with more than one display. Jitsi's own SDK returns `undefined` here,
 *             and so do we: painting on the wrong monitor is worse than not painting.
 *
 * @param {string} sourceId
 * @param {(id: string) => ({x: number, y: number} | undefined)} [sourceId2Coordinates]
 * @returns {{ bounds: {x:number,y:number,width:number,height:number}, scaleFactor: number } | undefined}
 */
export function displayForSourceId(sourceId, sourceId2Coordinates) {
    const displays = screen.getAllDisplays();
    if (displays.length === 0) return undefined;
    if (displays.length === 1) return normalizeDisplay(displays[0]);

    const parsed = String(sourceId).replace('screen:', '');

    if (process.platform === 'win32') {
        if (!sourceId2Coordinates) return undefined;
        const coords = sourceId2Coordinates(parsed);
        if (!coords) return undefined;
        // +1 so the point is inside the display rather than on its boundary.
        const d = screen.getDisplayNearestPoint({ x: coords.x + 1, y: coords.y + 1 });
        if (!d) return undefined;
        return {
            // The addon's origin, not Electron's — see the note above.
            bounds: { x: coords.x, y: coords.y, width: d.bounds.width, height: d.bounds.height },
            scaleFactor: d.scaleFactor,
        };
    }

    if (process.platform === 'darwin') {
        let id = Number(parsed);
        if (Number.isNaN(id)) {
            const parts = parsed.split(':');
            if (parts.length <= 1) return undefined;
            id = Number(parts[0]);
        }
        const d = displays.find(x => x.id === id);
        return d ? normalizeDisplay(d) : undefined;
    }

    // Linux, multi-monitor: genuinely unresolvable. The caller must disable annotation and say why.
    return undefined;
}

/**
 * macOS reports `scaleFactor === 2` while `bounds` ALREADY accounts for it
 * (`remotecontrol/main.js:105-111`). Applying it again puts the overlay on a quarter of the screen.
 */
function normalizeDisplay(d) {
    return {
        bounds: { ...d.bounds },
        scaleFactor: process.platform === 'darwin' ? 1 : (d.scaleFactor || 1),
    };
}

/** Is this source a whole display? A window share is never captured with the overlay (§12). */
export function isDisplaySource(sourceId) {
    return String(sourceId).startsWith('screen:');
}

export class AnnotationOverlayWindow {
    /**
     * @param {object} opts
     * @param {string} opts.url - the page hosting `overlay/renderer.js`.
     * @param {string} [opts.preload] - absolute path to the overlay preload. WITHOUT it the page
     *   has no `window.casualAnnotateOverlay`, so it receives no ops and renders nothing — the
     *   window appears to work and is simply always empty.
     * @param {(id: string) => ({x:number,y:number}|undefined)} [opts.sourceId2Coordinates]
     */
    constructor({ url, preload, sourceId2Coordinates } = {}) {
        this._url = url;
        this._preload = preload;
        this._s2c = sourceId2Coordinates;
        this._win = null;
        this._sourceId = null;
        this._onMetrics = () => this._reposition();
    }

    /**
     * Show the overlay over the display identified by `sourceId`.
     * @returns {{ ok: true } | { ok: false, reason: string }} — a refusal the UI must surface, since
     *   the alternative is annotation appearing to work while landing nowhere.
     */
    show(sourceId) {
        if (!isDisplaySource(sourceId)) {
            return { ok: false, reason: 'window-share-not-supported' };
        }
        const display = displayForSourceId(sourceId, this._s2c);
        if (!display) {
            return {
                ok: false,
                reason: process.platform === 'linux' ? 'linux-multi-monitor-unsupported' : 'display-not-resolved',
            };
        }

        this._sourceId = sourceId;
        if (!this._win) this._win = this._create();
        this._win.setBounds(display.bounds);
        this._win.showInactive();   // never steal focus from what the user is presenting
        screen.on('display-metrics-changed', this._onMetrics);
        return { ok: true };
    }

    _create() {
        const win = new BrowserWindow({
            transparent: true,
            frame: false,
            hasShadow: false,
            resizable: false,
            movable: false,
            minimizable: false,
            maximizable: false,
            fullscreenable: false,
            skipTaskbar: true,
            focusable: false,
            show: false,
            webPreferences: {
                contextIsolation: true,
                nodeIntegration: false,
                sandbox: true,
                ...(this._preload ? { preload: this._preload } : {}),
            },
        });

        // ADR-100, the non-negotiable part. `forward: true` keeps hover events flowing to the apps
        // underneath so the desktop behaves exactly as if the overlay were not there.
        win.setIgnoreMouseEvents(true, { forward: true });
        // Above full-screen apps, and present on every space.
        win.setAlwaysOnTop(true, 'screen-saver');
        win.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });

        if (this._url) win.loadURL(this._url);
        return win;
    }

    /** The overlay page's `webContents`, for the main-process relay. Null before `show`. */
    get webContents() {
        return this._win?.webContents ?? null;
    }

    /** Displays got rearranged mid-share — re-resolve rather than keep stale bounds. */
    _reposition() {
        if (!this._win || !this._sourceId) return;
        const d = displayForSourceId(this._sourceId, this._s2c);
        if (d) this._win.setBounds(d.bounds);
    }

    hide() {
        screen.removeListener('display-metrics-changed', this._onMetrics);
        this._win?.hide();
    }

    destroy() {
        screen.removeListener('display-metrics-changed', this._onMetrics);
        this._win?.destroy();
        this._win = null;
        this._sourceId = null;
    }
}
