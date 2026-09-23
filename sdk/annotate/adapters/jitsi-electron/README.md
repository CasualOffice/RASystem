# Wiring Casual Annotate into `jitsi-meet-electron`

Five touch points. Nothing in the Jitsi tree is forked or patched — these are additive calls
alongside the ones `@jitsi/electron-sdk` already makes, which is why this survives a Jitsi upgrade.

## Architecture (ADR-107 §8, Decision 10)

```
  JITSI IFRAME                      MAIN                        MEETING RENDERER    OVERLAY WINDOW
  (cross-origin, real server)                                   (this app's own UI) (transparent)

  injected-relay.js  ◄──executeJavaScript── watches did-frame-finish-load
       │  real ConferenceTransport
       │  (lib-jitsi-meet receives
       │   here — see below)
       ▼
  postMessage ──────────────────────────────────────────────► renderer.js relay ──► overlay
                                                                     │                 SharerController
  postMessage ◄────────────────────────────────────────────── (outbound: acks,        (the ONE copy
                                                                 roster)               of state)
                                                                     │
                                                        onSharerState → native
                                                        dialog.showMessageBox
```

**Why the relay is injected, not just imported.** The obvious transport is the iframe External API's
`sendEndpointTextMessage` / `endpointTextMessageReceived`. It sends fine. It **never receives** —
`endpointTextMessageReceived` has zero callers in jitsi-meet's web app (only the mobile middleware
implements the equivalent), so a request from an annotator could reach this app and be silently
dropped forever. That was a real, live defect (ADR-107 Decision 8/9): permission could end up granted
with no prompt ever having been shown, and a request → prompt → approve → draw cycle had never once
completed.

The fix runs on the *other* side of the iframe boundary. `main.js` injects a small bundled script
(`injected-relay.js`) directly into the Jitsi iframe via Electron's privileged `webContents.mainFrame`
`executeJavaScript` — a capability only a host application has; a real cross-origin page could never
grant itself this. That script holds the real `lib-jitsi-meet` conference and runs a genuine
`ConferenceTransport`, where the receive event actually fires, and bridges it to the host over
`postMessage` (which, unlike direct DOM access, crosses a cross-origin iframe boundary just fine).
`PostMessageTransport` (`transport/jitsi.js`) is the host side of that bridge, used in place of
`ExternalApiTransport`.

**The session still lives in the overlay window, not the renderer** — that part of the original
design was always correct. The alternative — state in the renderer, overlay as a dumb canvas — means
serialising up to 256 strokes × 1024 points through structured clone on every frame. Ops are tiny and
arrive a few times a second, so the relay carries **ops, not state**, and there is exactly one copy of
the truth.

**The native consent dialog is kept, deliberately**, rather than switching desktop to the same in-page
toast the browser deployment uses (`standalone/inject.js`) — a NATIVE dialog on the sharer's own
desktop is harder for a hostile page to imitate than anything drawn inside the meeting UI, and this
app already has the infrastructure for it.

## 1. Main process — `main.ts`

```js
import setupAnnotateMain from '@casualoffice/annotate/adapters/jitsi-electron/main.js';

// Alongside setupRemoteControlMain / setupScreenSharingMain:
const annotate = setupAnnotateMain(jitsiMeetWindow, {
    overlayUrl: `file://${path.join(rootDir, 'build', 'annotate-overlay.html')}`,
    // The esbuild-bundled injected-relay.js IIFE — see "Build" below. Without this, nothing is
    // injected into the iframe and the app is exactly as broken as before Decision 10.
    relayBundlePath: path.join(rootDir, 'build', 'annotate-relay.js'),
    // The configured jitsi-meet server's origin. The relay is injected ONLY into a sub-frame whose
    // URL starts with this — strongly recommended, so an unrelated frame this app happens to host is
    // never a target.
    serverOrigin: 'https://meet.example.com',
    // Windows only: the native addon that maps a sourceId to screen coordinates. Unused elsewhere.
    sourceId2Coordinates: require('@jitsi/electron-sdk/node_addons/sourceId2Coordinates'),
});
```

## 2. Preload — `app/preload/preload.ts`

```js
import { installAnnotateBridge } from '@casualoffice/annotate/adapters/jitsi-electron/preload.js';

installJitsiElectronSdk();
installAnnotateBridge();   // exposes window.casualAnnotate
```

The overlay window needs its own preload calling `installOverlayBridge()` instead. Unchanged from
before Decision 10 — the IPC surface between the renderer and the overlay was never the broken part.

## 3. Renderer — `app/features/conference/components/Conference.tsx`

```js
import { setupAnnotateRender } from '@casualoffice/annotate/adapters/jitsi-electron/renderer.js';

this._api = new JitsiMeetExternalAPI(host, options);
setupScreenSharingRender(this._api);
setupRemoteControlRender(this._api);

this._annotate = setupAnnotateRender(this._api, {
    onSharerState: s => this.setState({ annotate: s }),
    onRefused: (message) => this.setState({ annotateError: message }),   // MUST be shown
});
```

Behaviourally the same call as before, but it now constructs a `PostMessageTransport` against
`api.getIFrame().contentWindow` instead of an `ExternalApiTransport` — the one change that actually
matters. It also mounts a small annotator-management panel (mute/unmute/remove) into this app's own
document, next to wherever the app puts its own UI, fed by `SharerController.state()`; before this
the only way to mute or revoke an individual annotator was a JS console call.

`onRefused` is not optional in any real integration. Annotation refuses for reasons the user can act
on — see the table below — and a silent refusal means people draw into nothing while the UI implies
it is working.

## 4. Overlay page

Ship `overlay.html` + `overlay-page.js` into the build output at the path you gave `overlayUrl`.
Unchanged.

## 5. Build — the relay bundle

`injected-relay.js` is `executeJavaScript`'d into the Jitsi iframe verbatim, so it must be a
self-contained IIFE, the same way the overlay page's script is:

```bash
esbuild adapters/jitsi-electron/injected-relay.js --bundle --format=iife --platform=browser \
        --target=chrome120 --outfile=build/annotate-relay.js
```

`test/verify-bundles.mjs` checks its bundle boundary too (no `ipcMain`/`BrowserWindow` — anything that
gets injected into a webpage must never carry Node/Electron internals).

---

## When annotation refuses, and why

| Reason | Cause | What the user should be told |
|---|---|---|
| `window-share-not-supported` | Sharing a single window, not a display | The overlay is not inside a window capture. Share the whole screen. |
| `linux-multi-monitor-unsupported` | Linux, >1 display | `sourceId` → display is unresolvable; Jitsi's own SDK gives up here too |
| `display-not-resolved` | Windows without the native addon, or an unrecognised source id | Annotation is unavailable for this share |
| `no-source-id` | `screenSharingStatusChanged` gave no source | Nothing to place the overlay on |

Refusing is deliberate in every case. The alternative is marks landing on the wrong pixels of
somebody's real desktop, which is the worst thing this feature can do.

## Latency — do this before tuning anything

Jitsi ships screen share at **5 fps** (`SS_DEFAULT_FRAME_RATE`), and that one number also selects
`contentHint: 'detail'` (spatial quality over temporal smoothness — backwards for a moving pen) and
caps the screenshare bitrate. Raising `desktopSharingFrameRate.max` above 5 flips all three.

```js
// config.js, or via the External API's configOverwrite
desktopSharingFrameRate: { min: 15, max: 30 },
```

Then measure rather than guess — `latency/beacon.js` decomposes draw→see into t1 (relay), t2
(capture/encode) and t3 (JVB + jitter buffer + decode), and `LatencyProbe.verdict()` names the
dominant term and the next move. See design doc §11 and §17.

## Compatibility

**Seeing annotations requires no client code.** The marks are in the screen-share video, so mobile,
browsers and old builds all see them correctly with nothing installed. Only *drawing* needs the SDK,
which is why the protocol accepts a version range, never adds a required field, ignores unknown op
tags, and gates UI on advertised capabilities. A client that advertises nothing is assumed baseline.

## Testing this adapter without a full jitsi-meet-electron checkout

`test/electron/e2e-harness/` wires `main.js`/`preload.js`/`injected-relay.js` exactly as above against
a minimal host page that constructs a real `JitsiMeetExternalAPI` iframe — the same integration
contract as jitsi-meet-electron, without the full app's build. `main.js`'s `showConsentDialog` option
lets a test script the native dialog's answer instead of driving a real OS window. See
`test/live/README.md`.
