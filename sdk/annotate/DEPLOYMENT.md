# Deploying Casual Annotate

Two pieces, and which you need depends on what you want:

| You want | Deploy |
|---|---|
| Participants can draw on a shared screen | **the in-page script** (below) |
| Marks land on the sharer's **real desktop** | the in-page script **plus** the Electron overlay |

The in-page script is the whole feature for a browser. The Electron half exists only to own an
OS-level always-on-top window, which a browser cannot have.

---

## 1. The in-page script (required)

The SDK runs **inside the jitsi-meet page**, not beside it. That is not a packaging preference — see
[design §5.1](../../docs/design/annotate-sdk-jitsi-design.md): the iframe External API's
`endpointTextMessageReceived` event has **no callers in jitsi-meet's web app**, so anything outside
the page can send into a conference but never receive from it. A host outside the iframe cannot run
the consent flow at all.

### Build

```bash
esbuild standalone/inject.js --bundle --format=iife --platform=browser \
        --target=chrome120 --outfile=casual-annotate.js
```

### Serve it

jitsi-meet includes `body.html` into every page (`index.html:214`). Put the script there:

```html
<!-- body.html -->
<script src="casual-annotate.js"></script>
```

**Self-hosted jitsi-meet:** drop both files into the web root.

**docker-jitsi-meet:** mount them, since the image's root is read-only.

```yaml
# docker-compose.override.yml
services:
  web:
    volumes:
      - ./deploy/casual-annotate.js:/usr/share/jitsi-meet/casual-annotate.js:ro
      - ./deploy/body.html:/usr/share/jitsi-meet/body.html:ro
```

```bash
docker compose up -d web
curl -sk https://your-host/casual-annotate.js | head -c 40   # must be the script, not index.html
```

> A 200 does **not** mean it worked — jitsi-meet serves `index.html` for unknown paths, so a missing
> file returns 200 with the wrong body. Check the content.

That is the whole browser deployment. The script waits for a conference, shows a **Request to
annotate** button when someone is sharing, and shows an accept prompt to whoever is sharing.

---

## 2. The Electron overlay (optional)

Only needed so marks appear on the sharer's actual desktop and therefore inside the screen capture,
which is what lets every other participant — mobile included — see them with no client code
([§2](../../docs/design/annotate-sdk-jitsi-design.md)).

```bash
node adapters/jitsi-electron/install.mjs /path/to/jitsi-meet-electron
cd /path/to/jitsi-meet-electron && npm install && npm start
```

Eight idempotent, sentinel-marked edits plus a symlink. `--dry` previews; `--revert` restores the
checkout exactly (verified as a round trip). Nothing is forked, so a Jitsi upgrade re-applies in
seconds and, if an anchor moved, the script names which one instead of producing a broken build.

**`setupAnnotateMain` must run before `setupScreenSharingMain`.** It wraps
`session.setDisplayMediaRequestHandler` to observe which display the user picked; the SDK installs
its handler through that setter, so wrapping afterwards sees nothing. The installer places it
correctly — this matters only if you wire it by hand.

### Why the source id is obtained that way

The iframe API never exposes it: `screensharingDetails` carries only `sourceType`, never `sourceId`
(`actions.web.ts:144`). The id exists solely inside the main process's display-media callback.

---

## 3. What will not work, by design

| Situation | Behaviour |
|---|---|
| Sharing a **window** instead of a screen | refused — a window capture does not contain the overlay, so marks would land nowhere |
| **Linux with more than one display** | refused — `sourceId` → display is unresolvable, as it is in Jitsi's own SDK |
| Tile view / `object-fit: cover` | drawing disabled — cropped pixels cannot be addressed, and guessing would put marks on the wrong part of someone's desktop |
| A **browser** sharer | no desktop overlay is possible |

Each of these is refused loudly rather than approximated. Surface the reason to the user: a silent
refusal is indistinguishable from a broken feature, which is exactly how this SDK wasted a day.

---

## 4. Latency

Jitsi ships screen share at **5 fps** (`SS_DEFAULT_FRAME_RATE`), and that one number also selects
`contentHint: 'detail'` — spatial quality over temporal smoothness, backwards for a moving pen — and
caps the screenshare bitrate. All three key off `max > 5`:

```js
desktopSharingFrameRate: { min: 15, max: 30 }
```

Raise it only while annotation is active; it costs resolution. Then measure rather than guess —
`latency/beacon.js` splits draw→see into relay / capture-encode / decode and names the next move.

---

## 5. Verifying a deployment

```bash
npm test                # 125 unit tests
npm run verify-bundles  # Electron bundle boundaries (needs esbuild)
npm run smoke           # renders the overlay in real Electron (needs electron)
npm run capture-test    # proves the overlay is inside a live capture stream
```

For a live two-participant check, see [`test/live/`](test/live/README.md) — including the
docker-jitsi-meet HTTPS requirement, which is not optional: `JitsiMeetExternalAPI` hardcodes
`https://${domain}` (`external_api.js:324`), so the Electron app cannot talk to a plain-HTTP
deployment and shows only a black screen.
