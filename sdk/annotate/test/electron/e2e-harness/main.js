// Casual Annotate — hermetic Electron e2e harness, main process (ADR-107 Decision 10).
//
// Wires the REAL production adapter (`adapters/jitsi-electron/main.js`) — not a mock of it — against
// a minimal host window, so the injection/relay/native-dialog pipeline is proven with the actual code
// that ships, without needing a full jitsi-meet-electron checkout and build. `test/live/README.md`
// has the run instructions.

import { app, BrowserWindow, desktopCapturer } from 'electron';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import setupAnnotateMain from '../../../adapters/jitsi-electron/main.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BUILD = process.env.CASUAL_ANNOTATE_BUILD_DIR ?? '/tmp/casual-annotate-build';

// This harness must never put a window on whatever real display it happens to run against BY
// DEFAULT — a lesson learned live while building this suite's browser counterpart, where a
// non-headless Chromium instance surfaced a real, visible window (and a real permission popup)
// directly on the machine's own screen. `show: false` on the host window (below) is not enough on
// its own once the overlay window (`overlay/window.js`) actually shows itself (`showInactive()`, a
// real, if transparent and click-through, native window) — full Electron headless mode keeps
// EVERYTHING off the real display, offscreen, regardless of what any individual window's `show`
// option says. `CASUAL_ANNOTATE_DEMO=1` opts back into a real, visible window — only when a human
// explicitly asked to watch a run, never as this file's own default.
const DEMO = process.env.CASUAL_ANNOTATE_DEMO === '1';
if (!DEMO) {
    app.commandLine.appendSwitch('headless', 'new');
    app.disableHardwareAcceleration();
}
// `JitsiMeetExternalAPI` hardcodes `https://${domain}` with no opt-out (`external_api.js:324`,
// documented in `test/live/README.md`'s "Desktop app as the sharer" section) — this harness
// therefore always talks to a self-signed local HTTPS deployment, never the plain-HTTP one the
// browser-to-browser suite uses. `--ignore-certificate-errors` is what makes that workable for a
// throwaway local test stack; never appropriate outside one.
app.commandLine.appendSwitch('ignore-certificate-errors');
const SERVER = process.env.CASUAL_ANNOTATE_SERVER ?? 'http://localhost:8000';

// Playwright's `_electron.launch()` gives the test driver `electronApp.evaluate((electron) => …)`,
// which runs IN this main process — so the driver reaches this through `app.__e2e`, not IPC. This is
// the seam `main.js`'s `showConsentDialog` option exists for: scripting the native dialog's answer
// instead of automating a real OS window, which Playwright cannot do reliably across platforms.
const consentCalls = [];
app.__e2e = {
    nextConsentAnswer: false,
    get consentCalls() { return consentCalls; },
};

async function scriptedConsentDialog(_hostWindow, req) {
    consentCalls.push(req);
    return { allowed: app.__e2e.nextConsentAnswer };
}

let win;

app.whenReady().then(async () => {
    win = new BrowserWindow({
        width: 1280,
        height: 800,
        // `show: false` outside DEMO mode — this harness has no business putting a window on
        // whatever real display it happens to run against otherwise. `webContents.capturePage()`
        // (used for the pixel-paint assertion in the automated test) still works on a hidden window.
        show: DEMO,
        webPreferences: {
            contextIsolation: true,
            nodeIntegration: false,
            preload: path.join(BUILD, 'harness-preload.cjs'),
        },
    });

    // A minimal REAL display-media handler — the base `setupScreenSharingMain` normally installs
    // before ours wraps it. Uses Electron's own `desktopCapturer`, so a genuine source id flows
    // through exactly the same observation path (`_watchScreenShareSource`) production does.
    win.webContents.session.setDisplayMediaRequestHandler(async (_request, callback) => {
        const sources = await desktopCapturer.getSources({ types: [ 'screen' ] });
        callback(sources[0] ? { video: sources[0] } : {});
    }, { useSystemPicker: false });

    setupAnnotateMain(win, {
        overlayUrl: `file://${path.join(BUILD, 'annotate-overlay.html')}`,
        overlayPreload: path.join(BUILD, 'overlay-preload.cjs'),
        relayBundlePath: path.join(BUILD, 'annotate-relay.js'),
        serverOrigin: SERVER,
        // DEMO mode uses the REAL native dialog (main.js's own default) — the whole point is to
        // watch the actual OS-level popup fire and answer it yourself. The automated test path keeps
        // the scripted seam, since Playwright cannot click a real native dialog reliably.
        ...(DEMO ? {} : { showConsentDialog: scriptedConsentDialog }),
    });

    if (DEMO) win.show();

    await win.loadFile(path.join(HERE, 'host.html'), { query: { server: SERVER } });
});

app.on('window-all-closed', () => app.quit());
