# Live proof — two participants, a real server, a real bridge

Everything else in this package is tested without a network. This is the leg that needs one: do our
ops actually traverse **JVB's endpoint-message relay** between two participants, and does the sharer
enforce its security model against a real peer?

Run against a **local** Jitsi, not `meet.jit.si` — the public server now gates rooms behind moderator
login, and a local stack also lets you inspect prosody/jicofo when something misbehaves.

## 0. The scripted version — start here

What follows this section used to be the only option: stand up docker-jitsi-meet by hand, then paste
console commands into two browser tabs one at a time. That proved the transport, but never the thing
a user actually does — click a button, see a prompt, click Allow, draw — which is exactly the gap
that let the consent flow go unproven for as long as it did (ADR-107 Decision 8/9/10).

Both suites are **deliberately headless and never open a visible window** — an early version of
`e2e.spec.mjs` used `headless: false` + `--auto-select-desktop-capture-source` to attempt a REAL
`getDisplayMedia()` share, and it surfaced a real, visible browser window (and a real OS permission
popup) directly on whatever machine ran it, which is a genuine surprise for anyone sitting at that
machine and not worth it — see `playwright.config.mjs`'s own comment. Both suites instead simulate
"someone is sharing" at the API boundary (documented in each file), which is enough to prove the
consent/draw PROTOCOL; the separate claim that the overlay's pixels really are inside a live capture
stream is proven headlessly in `test/electron/capture-stream.mjs`.

```bash
bash setup-docker.sh                                                     # HTTP :8000, idempotent, ~5-10 min cold
npx playwright install chromium                                          # once
npm i -D @playwright/test esbuild electron                               # once
npx playwright test --config test/live/playwright.config.mjs test/live/e2e.spec.mjs        # browser ↔ browser
```

`e2e.spec.mjs` drives two real browser contexts through the actual UI: request → the sharer's toast →
Allow → a real pointer-drawn stroke → undo → a forged cross-author erase (refused) → revoke via the
sharer's management panel (not the console) → confirms drawing stops **and** drops the revoked
participant's existing marks (`SharerController.withdraw`'s actual behaviour — the earlier draft of
this test wrongly assumed revoke only blocks future draws).

**`electron.spec.mjs` needs a SECOND, HTTPS docker-jitsi-meet** — `JitsiMeetExternalAPI` hardcodes
`https://${domain}` with no opt-out (`external_api.js:324`), so the Electron path this test exercises
can never talk to the plain-HTTP stack above. `setup-docker.sh` deliberately only sets up the HTTP
case (`DISABLE_HTTPS=1`, the browser suite's requirement) — do NOT point it at this second checkout;
configure it directly instead, as its own independent compose project so it cannot collide with the
HTTP stack's containers, network, or ports:

```bash
git clone --depth 1 https://github.com/jitsi/docker-jitsi-meet.git /tmp/casual-annotate-jitsi-https
cd /tmp/casual-annotate-jitsi-https && cp env.example .env && ./gen-passwords.sh

# In .env: PUBLIC_URL=https://localhost:8444, HTTPS_PORT=8444, DISABLE_HTTPS left UNSET (or =0),
# HTTP_PORT=8001, ENABLE_AUTH=0, ENABLE_GUESTS=1, ENABLE_LOBBY=0, ENABLE_PREJOIN_PAGE=0,
# JVB_ADVERTISE_IPS=127.0.0.1, XMPP_BOSH_URL_BASE=http://xmpp.meet.jitsi:5280 (the same gotcha as
# the HTTP stack — see below), and JVB_PORT=10001 / JVB_COLIBRI_PORT=8081 / JICOFO_REST_PORT=8889 to
# avoid colliding with the HTTP stack's own default ports. CONFIG should point at its OWN directory
# (e.g. ~/.jitsi-meet-cfg-https), pre-created with the same subdirectories `setup-docker.sh` creates
# (see its own comment on why: a missing one gets auto-created as root by `docker compose up`, and
# prosody then refuses to start at all).

docker compose -p annotate-https up -d   # a distinct project name — self-signed cert is automatic

node test/electron/e2e-harness/build.mjs
CASUAL_ANNOTATE_HTTPS_PORT=8444 \
    npx playwright test --config test/live/playwright.config.mjs test/live/electron.spec.mjs
```

`electron.spec.mjs` runs a real Electron process as the sharer (`test/electron/e2e-harness/`, which
wires the actual `adapters/jitsi-electron/main.js` — not a mock — without needing a full
jitsi-meet-electron checkout), and asserts the specific thing Decision 10 fixed: that a request
reaches the native `dialog.showMessageBox` seam at all (previously provably unreachable), and that the
resulting stroke is in the overlay window's own rendered pixels (a real `capturePage()` check, not
just the session's stroke count).

Both suites pass reliably run individually; back-to-back in a resource-constrained environment they
can need the one retry `playwright.config.mjs` configures (real local-Jitsi XMPP-join timing under
contention — re-running an apparently-failed suite alone immediately after has always passed clean).

Everything below this section is the original manual runbook — still useful for interactively
inspecting `prosody`/`jicofo`/`jvb` state or reproducing something the scripted suite doesn't cover,
but the scripted version is what should be run to answer "does this actually work."

## 1. A local Jitsi

```bash
git clone --depth 1 https://github.com/jitsi/docker-jitsi-meet.git
cd docker-jitsi-meet && cp env.example .env && ./gen-passwords.sh
```

Set in `.env`:

```
PUBLIC_URL=http://localhost:8000
HTTP_PORT=8000
DISABLE_HTTPS=1
ENABLE_AUTH=0
ENABLE_GUESTS=1
ENABLE_LOBBY=0
ENABLE_PREJOIN_PAGE=0
JVB_ADVERTISE_IPS=127.0.0.1
```

```bash
docker compose up -d
```

### The one gotcha, and it will bite you

`docker-jitsi-meet`'s config template does `trimPrefix "https://"` and then hardcodes `wss://`:

```
{{ $PUBLIC_URL_DOMAIN := .Env.PUBLIC_URL | trimPrefix "https://" | trimSuffix "/" }}
```

So an `http://` PUBLIC_URL produces `wss://http://localhost:8000/xmpp-websocket`, the websocket never
opens, and the client shows **"You have been disconnected"** with no useful clue. Upstream assumes
TLS. Either terminate TLS properly, or patch the generated config after each start:

```bash
docker compose exec -T web sh -c \
  "sed -i 's|wss://http://localhost:8000/|ws://localhost:8000/|g; \
           s|https://http://localhost:8000/|http://localhost:8000/|g' /run/web/config/config.js"
```

Verify: `curl -s http://localhost:8000/config.js | grep -E 'websocket|bosh'` — both must be
`ws://`/`http://` on `localhost:8000`.

## 2. Serve the SDK bundle

```bash
esbuild test/live/browser-entry.js --bundle --format=iife --platform=browser \
  --target=chrome120 --outfile=/tmp/annotate-browser.js
cd /tmp && python3 -c "
import http.server, socketserver
class H(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header('Access-Control-Allow-Origin','*'); super().end_headers()
socketserver.TCPServer.allow_reuse_address = True
socketserver.TCPServer(('127.0.0.1', 8099), H).serve_forever()"
```

## 3. Two participants

Open `http://localhost:8000/AnnotateProof` in two tabs. In **each** console:

```js
(0, eval)(await (await fetch('http://127.0.0.1:8099/annotate-browser.js')).text());
```

**Sharer tab:**

```js
const A = window.CasualAnnotate, room = window.APP.conference._room;
const t = new A.ConferenceTransport(room, { ENDPOINT_MESSAGE_RECEIVED: 'conference.endpoint_message_received' });
const sharer = new A.SharerController({
    emit: ({ to, op }) => (to ? t.send(to, op) : t.broadcast(op)),
    admit: A.ADMIT.EVERYONE,
});
t.onOp((sender, msg) => sharer.handle(sender, msg));
sharer.syncParticipants([
    { id: room.myUserId(), name: 'Sharer', moderator: true },
    ...room.getParticipants().map(p => ({ id: p.getId(), name: p.getDisplayName(), moderator: false })),
]);
```

> **Note:** with `ENABLE_AUTH=0` every participant is a moderator, so pass `moderator: false`
> explicitly for the annotator or the `clear: "all"` authority check is trivially satisfied and
> proves nothing.

**Annotator tab:**

```js
const A = window.CasualAnnotate, room = window.APP.conference._room;
const t = new A.ConferenceTransport(room, { ENDPOINT_MESSAGE_RECEIVED: 'conference.endpoint_message_received' });
const sharerId = room.getParticipants()[0].getId();
t.onOp((s, m) => (window.__in ??= []).push(m.op));
t.send(sharerId, A.ops.hello([ ...A.compat.LOCAL_CAPS ]));

const strokes = new A.StrokeSender(t, sharerId, { batchPoints: 8 });
const pts = Array.from({ length: 25 }, (_, i) => [ i * 2500, 20000 ]);
strokes.begin('live:1', A.ops.TOOL.PEN);
strokes.points('live:1', pts);
strokes.end('live:1');
```

## 4. What must be true

| In the sharer tab | Expected |
|---|---|
| `__sharer.session.store.size` | `1` |
| `StrokeStore.densePoints(...)` length | every point sent — none lost across the bridge |
| stroke `author` | the annotator's endpoint id, from the relay |
| `session.cursors` | labelled with the display name from the roster |
| `session.profileOf(id).caps` | all five — `hello` negotiated |

| In the annotator tab | Expected |
|---|---|
| `__in` | contains `roster` **and** `ack` |
| roster colours | distinct per participant |

**The security assertions — the point of doing this live:**

```js
sharer.handle(annotatorId, { name:'casual-annotate', v:1, sid:'x', op:'erase', ids:['sharer:victim'] });
// → { changed: false, reason: 'nothing-erased' }  — ids are re-filtered by author

sharer.handle(annotatorId, { name:'casual-annotate', v:1, sid:'x', op:'clear', scope:'all' });
// → { reason: 'downgraded-to-mine' }  — removes only their own work
```

## Desktop app as the sharer

Web-to-web proves the transport. It does **not** prove the product, because the desktop app is the
half that owns the overlay. Two upstream facts make that harder than it sounds:

1. **`JitsiMeetExternalAPI` hardcodes `https://${domain}`** (`external_api.js:324`) with no SSL
   opt-out, so the Electron app **cannot** talk to a plain-HTTP deployment at all. A local test has
   to be HTTPS, self-signed cert and all — the app shows only a black screen otherwise.
2. **`Conference.tsx:143` rewrites `jitsi-meet://` to `https://`**, so passing a protocol URL on the
   command line forces HTTPS regardless of `--defaultServerURL`.

So: serve the stack on `https://localhost:8443` (`PUBLIC_URL=https://localhost:8443`,
`DISABLE_HTTPS=0`) and launch both ends with certificate errors ignored.

```bash
# desktop (sharer)
npx electron ./build/main.js --remote-debugging-port=9223     --ignore-certificate-errors --defaultServerURL https://localhost:8443

# web (annotator) — a SEPARATE Chrome with a throwaway profile, so your own browser is untouched
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"     --remote-debugging-port=9224 --ignore-certificate-errors     --user-data-dir=/tmp/chrome-annotate --no-first-run     "https://localhost:8443/AnnotateProof#config.prejoinConfig.enabled=false"
```

Drive the Electron app into a room over its own IPC rather than by typing at it — keystroke
automation lands on the wrong window and is not worth debugging:

```js
// in the LAUNCHER window's renderer (a `page` target, file://…/index.html)
window.jitsiElectronApp.ipc.send('open-meeting-window',
    { room: 'AnnotateProof', serverURL: 'https://localhost:8443' });
```

> **CDP note:** Electron hosts the Jitsi app in an **iframe** target, not a `page`. A driver that
> filters on `type === 'page'` will report "no target" while the app is plainly running.

## Result, 2026-09-22

**Web ↔ web** (transport): 25/25 points intact, ack and roster returned, capabilities negotiated,
cursor labelled — and both hostile ops refused, with the sharer's stroke surviving and the
attacker's own stroke correctly removed.

**Web → desktop** (the real shape — Electron as sharer, browser as annotator): a 31-point stroke
arrived complete (`[2000,30000]` … `[62000,27206]`), rendered in the annotator's **assigned** colour
`#1e90ff` derived from the author rather than the wire, `end` received, cursor labelled
`WebAnnotator` from the roster, all five capabilities negotiated, ack returned to the browser.

## Result, 2026-09-22 (scripted, UI-driven — §0) — the first full cycle ever observed

Everything above proved the transport and the store. It never proved the actual product: a person
clicks a button, sees a prompt, clicks Allow, draws — and, per the design doc's own status line going
into this run, that exact cycle "has never been observed working end to end, in either shape." This
run is that observation, for both shapes, driven through the real UI rather than the console:

- **Browser → browser** (`e2e.spec.mjs`): request → the sharer's real toast appears → Allow → a real
  pointer-dragged stroke lands with correct author/colour attribution → Undo removes it → a forged
  cross-author `erase` from an unadmitted sender is refused (`not-admitted`) with the victim stroke
  surviving → revoke through the sharer's management panel (not the console) drops the withdrawn
  participant's existing marks **and** stops further drawing.
- **Browser → Electron desktop** (`electron.spec.mjs`, via `test/electron/e2e-harness/`, wiring the
  real production `adapters/jitsi-electron/main.js`/`renderer.js`/`injected-relay.js` — no
  jitsi-meet-electron checkout): the request reaches the **native `dialog.showMessageBox` seam**, and
  the resulting stroke lands in the **overlay window's own rendered pixels** (7,489 non-transparent
  pixels captured via `webContents.capturePage()`, the same technique `test/electron/smoke.mjs`
  already used) — not merely the session's stroke count. This is the specific thing Decision 8/9
  found broken and Decision 10 fixed; before this run it had never been observed working.

Also found live, and fixed, while building this proof (beyond the transport bug Decision 10 already
names): the harness's own preload bundled `adapters/jitsi-electron/preload.js` raw instead of calling
`installAnnotateBridge()`, so `window.casualAnnotate` was never actually installed — a bug in the
TEST, not the SDK. And a real, adversarial multi-agent review of the whole Decision-10 change (23
confirmed findings — see ADR-107's Status line for the two most serious) was run and fixed before this
proof, so what is verified here is the code as it stands after that pass, not before it.

**Not yet re-verified:** `adapters/jitsi-electron/install.mjs`'s Decision-10-era edits (the relay
build target, `relayBundlePath`/`serverOrigin`) against a real jitsi-meet-electron checkout — this
proof deliberately used the hermetic harness instead, to avoid needing one.
