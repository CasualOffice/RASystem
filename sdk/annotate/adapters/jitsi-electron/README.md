# Wiring Casual Annotate into `jitsi-meet-electron`

Four touch points. Nothing in the Jitsi tree is forked or patched — these are additive calls
alongside the ones `@jitsi/electron-sdk` already makes, which is why this survives a Jitsi upgrade.

## Architecture

```
  MEETING RENDERER                 MAIN                    OVERLAY WINDOW
  (Jitsi iframe)                                           (transparent, click-through)

  endpoint message ──► forwardOp ──► relay ──────────────► SharerController
                                                            ├─ SharerSession (the ONE copy of state)
                                                            └─ OverlayRenderer ──► shared display
  transport.send  ◄──── onEmit ◄──── relay ◄───────────────  acks, roster
```

**The session lives in the overlay window, not the renderer.** The alternative — state in the
renderer, overlay as a dumb canvas — means serialising up to 256 strokes × 1024 points through
structured clone on every frame. Ops are tiny and arrive a few times a second, so the relay carries
**ops, not state**, and there is exactly one copy of the truth.

## 1. Main process — `main.ts`

```js
import setupAnnotateMain from '@casualoffice/annotate/adapters/jitsi-electron/main.js';

// Alongside setupRemoteControlMain / setupScreenSharingMain:
const annotate = setupAnnotateMain(jitsiMeetWindow, {
    overlayUrl: `file://${path.join(rootDir, 'build', 'annotate-overlay.html')}`,
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

The overlay window needs its own preload calling `installOverlayBridge()` instead.

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

`onRefused` is not optional in any real integration. Annotation refuses for reasons the user can act
on — see the table below — and a silent refusal means people draw into nothing while the UI implies
it is working.

## 4. Overlay page

Ship `overlay.html` + `overlay-page.js` into the build output at the path you gave `overlayUrl`.

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
