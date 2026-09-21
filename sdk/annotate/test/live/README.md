# Live proof — two participants, a real server, a real bridge

Everything else in this package is tested without a network. This is the leg that needs one: do our
ops actually traverse **JVB's endpoint-message relay** between two participants, and does the sharer
enforce its security model against a real peer?

Run against a **local** Jitsi, not `meet.jit.si` — the public server now gates rooms behind moderator
login, and a local stack also lets you inspect prosody/jicofo when something misbehaves.

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

## Result, 2026-09-22

All of the above passed against `docker-jitsi-meet` on localhost: 25/25 points intact, ack and roster
returned, capabilities negotiated, cursor labelled — and both hostile ops refused, with the sharer's
stroke surviving and the attacker's own stroke correctly removed.
