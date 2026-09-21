# Casual Annotate — SDK design (Jitsi first)

**Status:** design · **ADR:** ADR-107 · **Extracted from:** the annotation slice proven in
`v0.0.4-alpha` (ADR-097 + ADR-100) · **First target:** Jitsi (Electron sharer + browser annotators)

---

## 1. What this is

Take the annotation feature that works in Casual RAS and ship it as an **embeddable SDK** any
meeting product can drop in. Jitsi is target #1, not the only target: the SDK owns the op model, the
coordinate mapping, the sharer overlay and the drawing surface, and takes its **transport as a
plug-in**. Jitsi's own bridge relay is the first transport; `iroh-gossip` is the second (§14).

The product behaviour is RAS's, unchanged: **remote participants draw on the screen-sharer's real
desktop**, not on a canvas floating over a video element.

---

## 2. The load-bearing insight

Because the overlay is a transparent always-on-top window on the **sharer's actual screen**, the
marks are inside the screen capture. They ride the ordinary video path to everyone.

```
annotator A ─┐
annotator B ─┼─► bridge relay ─► SHARER ─► transparent overlay ─► screen capture
annotator C ─┘    (data)          (the ONLY renderer)                   │
                                                                        ▼
                                        ordinary Jitsi video ─► A, B, C, and every other participant
```

This was confirmed on-device: drawing appeared for other participants with no receive-side code at
all. Everything below follows from it:

| Consequence | Why |
|---|---|
| **One renderer, not N** | Only the sharer paints. No per-viewer canvas, no tile-view math on the receive side. |
| **No shared state, no CRDT, no late-joiner replay** | Late joiners just see the video. There is no replicated document to converge. |
| **Lossy, unordered relay is fine** | A single consumer applying idempotent, id-addressed ops. No total order needed. |
| **Browsers work for free** | A browser participant only needs to **send**. It never needs a desktop overlay. |
| **No E2EE gap for the marks themselves** | The drawing reaches viewers as video, under whatever protection the meeting's media already has. |

The cost is one real problem, §11: the annotator sees their own stroke only after a full
draw → relay → composite → encode → decode round trip.

---

## 3. Roles

Two roles, and a participant can hold both:

- **Sharer** (must be Electron/native) — owns the overlay window, is the sole renderer, and is the
  **sole authority**: annotation is off until they turn it on, and they can clear or revoke at any
  moment. Cannot be a browser; a browser has no way to paint on its own desktop.
- **Annotator** (Electron **or browser**) — draws over the shared-screen video element, normalizes to
  the sharer's pixel space, sends ops. Renders a **local echo** only (§11).

A browser-only meeting with a browser sharer degrades to §13.

---

## 4. Op model

RAS sends one message per **completed** stroke (`AnnotateOp::Stroke`, `crates/ras-protocol/src/lib.rs:472`).
That does not survive live multi-party drawing, so the SDK splits a stroke into a begin/append/end
triple. Every op carries an author and a stroke id.

**Colour is not on the wire.** One colour per participant (§7), assigned by the sharer, so the sharer
derives a stroke's colour from its author. This drops a field, and it closes a spoofing vector for
free: **a participant cannot draw in someone else's colour**, because they never get to state one.

```jsonc
// envelope — every op
{ "name": "casual-annotate", "v": 1, "sid": "<session>", "seq": 1234, "t": <ms> }
// NOTE: no "from" field. The author is the relay-reported sender, always (§7).

// annotator → sharer
{ "op": "begin",  "id": "<author>:<n>", "tool": 0|1|2|3 }
{ "op": "append", "id": "<author>:<n>", "pts": [[x,y], ...] }  // <= 64 pts per message
{ "op": "end",    "id": "<author>:<n>" }
{ "op": "undo" }                                  // my most recent live stroke
{ "op": "erase",  "ids": ["<author>:<n>", ...] }  // hit-tested locally by the author (§4.1)
{ "op": "clear",  "scope": "mine" | "all" }       // "all" requires sharer/moderator
{ "op": "cursor", "x": <u16>, "y": <u16> }        // throttled ~20 Hz, lossy, last-wins

// sharer → everyone (broadcast, infrequent)
{ "op": "roster", "colors": { "<author>": "0xRRGGBB", ... }, "names": { "<author>": "<display name>" } }
```

- **Tools** keep the RAS tags exactly — `0=pen, 1=highlighter, 2=arrow, 3=rect` — so the geometry in
  `overlay.js:80` ports untranslated and the future iroh path stays wire-compatible. The eraser is
  **not** a fifth tool tag: it produces `erase` ops, never strokes.
- **Coords** keep RAS's normalized `0..=65535` u16 pair.
- **Every remove op is scoped to the sender.** `undo`, `erase` and `clear: "mine"` can only ever
  affect strokes whose author is the relay-reported sender. The sharer enforces this — it does not
  trust the ids in an `erase` to belong to the sender, it **filters them by author** before applying.
  `annotStrokes.pop()` (`overlay.js:68`) is gone; with several authors on a lossy relay, "pop the last
  one" deletes someone else's work. This is the most important deviation from the RAS code.
- **Bounds carry over verbatim** and are enforced on the sharer: `MAX_ANNOT_POINTS = 1024` per stroke,
  256 retained strokes (oldest dropped). Add: 64 points per `append`, 32 live strokes per author,
  ~40 messages/sec/author, 64 ids per `erase`, and unknown `op`/`tool` tags **rejected, not clamped** —
  fail-closed decode, the same posture as `ras-protocol`'s codec.

### 4.1 Undo and erase need the annotator to remember its own geometry

Both are author-scoped, and the eraser hit-tests — so the **annotator** must keep the geometry of its
own strokes to know which ids a drag crosses. That conflicts with §11, where the local echo eventually fades
out once the shared video catches up.

So: **fade the rendering, keep the record.** The annotator retains a bounded list of its own stroke
ids + point geometry (same 256/1024 bounds) after the visual echo is gone, purely to resolve `undo`
and to hit-test `erase`. It is dropped when the share ends.

The **sharer** needs none of this — it holds every stroke already, so its own eraser hit-tests
directly against the authoritative set and applies without a round trip.

## 5. Transport — and the upstream dead end that reshaped this design

**Verified against `lib-jitsi-meet` and `jitsi-meet`.** The bridge channel is an `RTCDataChannel`
**or** a WebSocket to JVB (`BridgeChannel.ts:23`), and every send is
`JSON.stringify({ colibriClass: 'EndpointMessage', msgPayload, to })` (`BridgeChannel.ts:499`).

### 5.1 The iframe External API can send but cannot receive

This is the single most important fact in this document, and it was found the expensive way — after
the feature had been "verified" several times in pieces and still could not work end to end.

`sendEndpointTextMessage` wraps its payload as `{ name: 'endpoint-text-message', text }`
(`API.js:602`, `constants.js:24`). The matching receive notifier, `notifyEndpointTextMessageReceived`,
is **defined at `API.js:1700` and has zero callers in the web application.** Only the React Native
path implements the equivalent (`mobile/external-api/middleware.ts:232`).

> **So `endpointTextMessageReceived` never fires on web or Electron.** A host outside the iframe can
> send into a conference and can never hear anything back.

That makes an out-of-iframe transport structurally one-way, which in turn makes the whole
ask-and-approve flow impossible: a request reaches nobody, so no prompt can ever appear. The symptom
is a participant who sees "Request to annotate", clicks it, and waits forever while the sharer sees
nothing at all — with no error anywhere.

### 5.2 Consequence: the SDK runs *inside* the meeting page

The fix is not a workaround, it is the correct shape:

```
       jitsi-meet page (served)                    Electron main
  ┌────────────────────────────────┐        ┌───────────────────────┐
  │ casual-annotate.js             │        │                       │
  │  ├─ ConferenceTransport ───────┼── JVB  │                       │
  │  ├─ SharerController + consent │        │                       │
  │  └─ AnnotatorToolbar + surface │ ─ops─► │ overlay window        │
  └────────────────────────────────┘ post   └───────────────────────┘
                                     Message
```

The script is **served by jitsi-meet through its own `body.html` include** (`index.html:214`), so it
runs in the page with the real `lib-jitsi-meet` conference — which does receive. Only the overlay
window, which must be an OS-level always-on-top surface, stays in the Electron main process, and it
is fed ops over `postMessage` → IPC.

This also removes a whole category of problem: there is now **one transport**, the same code in a
browser and inside the desktop app, so the two halves cannot drift apart.

### 5.3 The wire

- **Envelope:** `{ name: 'endpoint-text-message', text: JSON.stringify(op) }`, matching the External
  API's own shape so that a host which *can* receive (mobile, or a future web fix) interoperates
  without a second format. Incoming messages are accepted wrapped **or** bare.
- **Unicast to the sharer**, never broadcast — only the sharer renders, and the capture path does the
  fan-out (§2). Broadcast is reserved for the `roster`, which genuinely needs everyone.
- **Cursor ops are lossy by design** — throttled ~20 Hz, coalesced, never retried.
- **Stroke repair, not a protocol:** on `end`, resend the stroke once. Idempotent by stroke id, so a
  duplicate costs nothing and a dropped `append` self-heals.

## 6. Coordinate space — and where Jitsi's own code gets it wrong

RAS owns its capture, so normalization is exact. Jitsi does not give us that, and its reference
implementation is *worse than the one we already have*:

**Jitsi remote control normalizes against the large-video *wrapper element*** —
`x: (event.pageX - position.left) / area.width()` where `area = VideoLayout.getLargeVideoWrapper()`
(`actions.ts:637`, `functions.ts:88`). That is the **element** rect, not the **video content** rect,
so any letterboxing (a 16:9 wrapper showing a 16:10 share, or any aspect mismatch) skews the mapping.

RAS already does this correctly: `normPt` (`app/ui/main.js:2075`) and `drawHostStroke`
(`main.js:2022`) both map through `videoContentRect()`, letterbox-aware, into `0..=65535`. **Port ours,
not theirs.** Same convention as Jitsi (normalized fraction of the shared surface), better mapping.

Three more rules follow:

1. **Bind to large video, not tile view.** Jitsi's remote control only operates on the large-video
   wrapper, and for good reason — a tile is too small and may be cropped. Annotation binds to the
   pinned/large screen-share.
2. **`object-fit: cover` means refuse, not guess.** Cropped pixels are unaddressable. Read the
   computed `object-fit`; if the share is cropped, disable drawing with a visible reason rather than
   send coordinates that land somewhere else on a stranger's real desktop. Silently-wrong marks on
   someone's screen is the worst outcome available here.
3. **The sharer maps into the shared source, not the desktop** — `0..=65535` spans the shared
   display's bounds (§8), so a secondary-monitor share lands correctly.

## 7. Multi-participant — one colour per person

The requirement is N annotators, each identifiable, with N visible named cursors, N > 2.

### 7.1 Colour is the identity, so it must be assigned — not hashed

The obvious implementation is a deterministic hash of the endpoint id into a palette. **That is a bug
here**, and it is worth stating plainly: hashing collides, and the moment two participants land on the
same colour, "erase my colour" stops being meaningful to a human watching the screen — and one user's
eraser visually appears to eat another's work.

So the **sharer assigns colours** — it is already the sole renderer, the authority, and the holder of
the conference roster, so it is the only place a collision-free assignment can be made. It hands out
the next free palette slot on join, releases it on leave, and broadcasts the map as a `roster` op.
This is the one place the sharer talks *back* to annotators, and it is small and infrequent.

- **Internally, the key is the author id, never the colour.** Colour is how a human reads identity;
  `undo`/`erase`/`clear` are resolved by author (§4). Even under palette exhaustion, removes stay
  correct.
- **Palette exhaustion** (more participants than slots): reuse colours and lean on the name labels.
  The removes remain exact; only the visual shorthand degrades.
- **Palette choice matters more than usual** — these marks land on arbitrary screen content, not a
  white canvas. High-contrast, distinguishable under highlighter alpha (0.35, `overlay.js:89`), and
  distinguishable for the common colour-vision deficiencies.
- **The colour picker goes away.** The RAS toolbar's `.swatch` handling (`main.js`) is deleted: your
  colour is assigned, not chosen. Strictly simpler UI, and it is what makes the colour mean something.

### 7.2 Attribution

Author = the Jitsi endpoint id, taken from the **relay-reported sender** — never from a field inside
the payload (which is why §4's envelope has no `from`). A participant cannot draw as someone else, in
someone else's colour, or erase someone else's work.

### 7.3 Named cursors

Each annotator's live pointer renders on the sharer's overlay as a coloured arrow **with that
participant's display name** beside it, in their colour — idle fade-out (~3 s), removed on leave. The
name comes from the sharer's roster, not from the cursor op, for the same attribution reason.

**This deliberately reverses part of ADR-100.** ADR-100 removed the look-here pointer from the RAS
overlay because a second arrow next to the host's own baked-in cursor read as a confusing "multi
cursor" artifact (reported on Linux). That reasoning was for **1:1**, where one unlabelled arrow was
indistinguishable from the host's real cursor. Here it inverts: several *named, coloured* pointers are
precisely the feature, and the name + colour are exactly what stop them reading as the system cursor.
ADR-100's other rule is **kept absolutely**: the overlay is always transparent and click-through and is
never made interactive — that is what caused the Mac white screen.

## 8. The sharer's overlay (Electron)

**Architecture: copy `@jitsi/electron-sdk`'s remote-control shape.** It already solves this exact
class of problem — a renderer relays messages from the Jitsi iframe over a context-isolated bridge,
and the **main process** owns the privileged work and the display resolution
(`remotecontrol/renderer.js`, `remotecontrol/main.js`). We do the same, minus all input injection:
renderer relays annotation ops → main owns the overlay window. (Their main process pulls in
**robotjs** to inject input. We need none of that — §10.)

The canvas is a direct port of `app/ui/overlay.js`: the rAF loop, `alpha: true`, the DPR fit, and all
four tool geometries (`overlay.js:80`) transfer unchanged.

```js
new BrowserWindow({
  transparent: true, frame: false, hasShadow: false, resizable: false,
  skipTaskbar: true, focusable: false,
  webPreferences: { contextIsolation: true, nodeIntegration: false },
})
win.setIgnoreMouseEvents(true, { forward: true })   // ADR-100: click-through, always
win.setAlwaysOnTop(true, "screen-saver")            // above full-screen apps
win.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true })
win.setBounds(displayForSourceId(sourceId).bounds)  // the SHARED display, not the primary
```

Store changes from `overlay.js:62`: `annotStrokes` becomes a `Map<strokeId, stroke>` plus a
`Map<authorId, cursor>`; `pop()` becomes tombstone-by-id; the 256 bound and the clear-on-share-end
(`overlay.js:75`) stay.

### 8.1 `sourceId` → display: solved, but ugly and platform-split

`@jitsi/electron-sdk`'s `RemoteControlMain._getDisplay()` (`remotecontrol/main.js:323`) is the
battle-tested version of this, and it is worth reusing rather than rediscovering:

| Platform | How | Status |
|---|---|---|
| **Windows** | Native addon `node_addons/sourceId2Coordinates` → `screen.getDisplayNearestPoint({x+1, y+1})`, and it must use the **addon's** x/y because Electron's "don't seem to respect the scale factors of the other displays" | works, needs the addon |
| **macOS** | `sourceId` is `screen:<displayId>:0` → parse `displayId`, match `screen.getAllDisplays()` by `id` | works |
| **Linux** | **returns `undefined` when there is more than one display** | **unsupported multi-monitor** |

Two traps their code documents and ours must copy:
- **macOS `scaleFactor` must be forced to 1** — `display.scaleFactor` is always 2 there, but
  `display.bounds` already accounts for it (`main.js:105-111`). Double-applying it puts the overlay on
  a quarter of the screen.
- **Re-resolve on `display-metrics-changed`** (`main.js:312`) — monitors get rearranged mid-share.

**Platform notes.** macOS already needs Screen Recording permission for the share itself, and a
transparent always-on-top window composites into a display capture. On Windows a layered always-on-top
window is captured in display mode. **Linux/Wayland is the weak one** — an always-on-top click-through
surface is compositor-dependent, and per the table above Jitsi's own SDK gives up on multi-monitor
there. Needs on-device verification, the same caveat this repo already carries for its Linux host path.

## 9. Consent — ask and approve (Inv 1)

Annotation paints on someone's real desktop, so the person whose screen it is decides. Every other
guarantee in this document is downstream of that one.

### 9.1 The flow

```
annotator                     sharer (in-page)                person at the keyboard
    │  request ───────────────────►│
    │                              │  pending.add(id) ──────────► prompt
    │                              │◄─────────────────────────── Allow / Deny
    │◄────────── grant | deny ─────│
```

- **`request` carries nothing** — not even a display name. A request that named itself would let
  anyone make the consent prompt say whatever they liked. The prompt is labelled from the sharer's
  own roster, and the name is inserted as **text, never HTML**.
- **A refusal is sent explicitly.** Silence is indistinguishable from a dropped message, and leaves
  the requester's UI spinning forever.
- **Deny is the default** and dismissing the prompt denies.

### 9.2 A grant is impossible without a request

This was a real defect, found in a live run: permission ended up **granted with no prompt ever
shown**. That is worse than having no consent feature, because the interface claims a gate that is
not there.

The cause was that "only called after a human decision" was a *comment*. It is now a *guard*:

| Method | Behaviour |
|---|---|
| `allowParticipant(id)` | **refuses** unless that id has a request outstanding; returns `false` |
| `preAuthorize(id)` | the only way to admit someone who never asked — named so it cannot be reached by accident |
| `approve(id)` | answers a prompt; returns `false` and emits nothing if there was no request |

So a stray call, a replayed state update, or a second session object in the same page cannot produce
a grant. A withdrawn permission cannot be restored by a replayed approval either: the original
request is spent.

Three tests pin this, and one of them is named after the defect.

### 9.3 Revocation puts the pen down

Withdrawing permission removes the participant's marks **and** resets their tool to `null`, so the
canvas stops accepting pointer events. Re-rendering the toolbar alone would leave the user drawing
into a void — emitting ops the sharer now refuses, with no feedback.

### 9.4 The rest of the sharer's authority

Off by default per share · an always-visible indicator on the overlay · instant clear + revoke ·
per-participant mute · an allow-list. Any participant can *send*; the sharer decides who is
*honoured*. Annotation carries **no capability** (ADR-097): it is geometry and a colour, with no OS
input, screen-write or filesystem path — which is what keeps it safe to drop into a third-party
product. The moment anything here gains an input path, that reasoning is void.

## 10. Security posture

- **The bridge sees the ops.** In a standard Jitsi meeting, endpoint messages are relayed by JVB in
  the clear — the same trust the meeting already places in the bridge for media. **But note honestly:**
  with Jitsi E2EE enabled, media is end-to-end encrypted and endpoint messages are **not**. Annotation
  geometry would then be visible to the bridge when the video is not. Document it; don't paper over it.
  (This is precisely what the iroh transport in §14 fixes.)
- **Any room participant can send ops.** The sharer enforces an allow-list (everyone / moderators /
  named participants), per-sender rate limits, and the §4 bounds. Deny-by-default on unknown tags.
- **Attribution is from the relay, not the payload** (§7) — a participant cannot draw as someone else.
- **No OS input path exists anywhere in this SDK.** That is the invariant that keeps the threat model
  small, and it should be stated in the public API docs as a guarantee.

---

## 11. Latency — the actual problem, and where it comes from

Observed on-device: **~1 s** between drawing and seeing it. That is the single biggest threat to the
feature, so it gets measured, not hand-waved. Most of it is **Jitsi configuration**, and it is fixable.

### 11.1 Jitsi ships screen share at 5 fps, and that cascades

**Verified in the source.** `SS_DEFAULT_FRAME_RATE = 5` (`ScreenObtainer.ts:125`), and `config.js:396`
documents the default as `desktopSharingFrameRate: { min: 5, max: 5 }`. That one number sets three
things against us at once:

| At the default `max: 5` | Source | Effect on ink |
|---|---|---|
| A frame every **200 ms** | — | Up to 200 ms of pure capture-tick wait, and the stroke arrives in 200 ms **steps** — it reads as choppy, not merely late |
| `contentHint = 'detail'` | `ScreenObtainer.ts:422` — `max > 5 ? 'motion' : 'detail'` | Encoder optimizes **spatial detail over temporal smoothness** — precisely backwards for moving ink |
| Simulcast lower layers disabled, bitrate capped | `TraceablePeerConnection.ts:2281` — `capScreenshareBitrate = spatialScalability && maxFps <= 5` | Tuned for static slides, not for a moving pen |

**So the first thing to try is raising `desktopSharingFrameRate.max` above 5 while annotation is
active** — because it is `> 5` that flips `contentHint` to `'motion'` automatically, it is the same
comparison that re-enables the simulcast layers, and it shortens the capture tick. One lever, three
wins. `config.js:397` warns higher fps costs resolution; for annotation that is the right trade, and
it argues for raising it **only while annotation is on** and restoring it afterwards.

The residual after that — encode, network, jitter buffer, decode — is real and irreducible. Measure
it before tuning anything else.

### 11.2 The echo must not fade on a timer

A fixed ~1 s cross-fade — which is what an earlier draft of this document specified — is actively
wrong when the round trip is *also* about 1 s: the local mark disappears at almost exactly the moment
the video version arrives, so it blinks, or vanishes and comes back. **That is the artifact, and the
timer causes it.**

Correct policy:

1. **Hold the local echo at full strength until the sharer `ack`s the stroke** over the data channel —
   a data round trip (~RTT), far shorter than the video path, and it is a real signal rather than a guess.
2. **Then keep holding for the estimated video lag**, and only then cross-fade over ~400 ms.
3. **Never fade while the pointer is still down.**
4. Prefer a lingering ghost to a gap. A mark that overstays reads as ink; a mark that vanishes reads
   as a bug.

**Estimating the video lag honestly.** Seed it from measured data RTT + `1/fps` + an encode/jitter
constant, and let it be tuned. The accurate version — deferred, but cheap and worth doing — is a
**pixel beacon**: the sharer's overlay draws a tiny low-visibility counter in one corner; annotators
sample the incoming video with `drawImage` into a small canvas and read it back, which yields true
end-to-end video latency instead of an estimate. That turns §11 from a guess into a measurement.

### 11.3 Who actually suffers the delay

| Role | Sees ink after | Fix |
|---|---|---|
| **Annotator** (drawing) | immediately | local echo (§11.2) |
| **Sharer** | data RTT only — no video round trip at all | already fine |
| **Every other participant** | the **full** video round trip | **nothing, in this design** |

Passive viewers are the irreducible case, and it is worth being explicit that §11.1 is the *only*
lever for them.

**The option that fixes it for everyone, and its price.** Broadcast ops instead of unicasting to the
sharer, and have each viewer *also* render a local low-latency preview over the large-video element,
fading as the video catches up — the sharer's desktop stays the authoritative record. That buys
instant ink for all participants, but it pulls in the whole §13 machinery: broadcast fan-out,
per-viewer content-rect mapping, and the tile-view / `object-fit: cover` refusals. **Deferred, not
rejected** — revisit once §11.1 is measured, because raising the frame rate may well make it
unnecessary.

## 12. Failure modes to design for

| Case | Behaviour |
|---|---|
| **Sharer shares a *window*, not a display** | The overlay is **not** in the capture — the feature silently does nothing. Detect the source kind (`sourceId` prefix `window:` vs `screen:`) and tell the sharer annotation needs a display share. **Saying so is the honest v1**; clipping the overlay to the window's bounds would still not get it into the capture. |
| **Linux with more than one display** | `sourceId` → display is **unresolvable** — `@jitsi/electron-sdk` returns `undefined` here too (§8.1). Disable annotation with a clear reason rather than paint on the wrong monitor. |
| Sharer is a browser | No overlay is possible. Degrade to §13 or disable with a clear reason. |
| `object-fit: cover` crop, or tile view | Drawing surface refuses to draw (§6). |
| Sharer changes monitor mid-share, or displays get rearranged | Re-resolve on `display-metrics-changed` (§8.1), re-bind overlay bounds, clear marks — the old coordinates no longer mean anything. |
| Annotator leaves | Drop their cursor; leave their strokes (they are part of the discussion) until cleared. |
| Relay drops an `append` | Self-heals on the `end` resend (§5). |
| Two annotators on the same pixel | Both render; colour + label disambiguate. No locking. |

## 13. Fallback: video-element rendering

For browser sharers and window shares, the same op stream can be rendered on **every participant's**
video element instead of the sharer's desktop — `main.js:2022` (`drawHostStroke`) is already exactly
that renderer, content-rect-aware. It is strictly worse (needs broadcast instead of unicast, needs
late-joiner state sync, and marks are per-viewer rather than in the shared image), so it is a
**fallback, not the model**. Build it second, once the primary path is proven.

---

## 14. Deferred: the iroh-gossip transport

Once the Jitsi path is proven, the second transport adapter is `iroh-gossip` (already in the tree at
`0.101`, ADR-094) — a side-channel mesh alongside the call, carrying **signed** ops.

Why it's worth doing: it removes the §10 bridge-visibility gap, works when the meeting product has no
usable data channel, and makes the SDK genuinely product-independent. Its constraints are already
recorded in ADR-094 and they suit this op model well — best-effort, unordered and ~4 KiB/message are
all fine for id-addressed, tombstone-undo ops (§4), and every payload is signed and verified because
`delivered_from` is the forwarding neighbour, not the author. The open problem is **topic bootstrap**:
`TopicId` is a bearer secret, so deriving it from a meeting the SDK doesn't own needs its own design.

---

## 15. What we reuse from `v0.0.4-alpha`

| Reused | Source |
|---|---|
| Four tool geometries (pen/highlighter/arrow/rect), DPR + alpha canvas discipline | `app/ui/overlay.js:80` |
| Content-rect normalization, letterbox-aware, `0..=65535` | `app/ui/main.js:2075`, `overlay.js:83` |
| Local drawing surface, tool bar, undo/clear UX | `app/ui/main.js:1952` |
| *(the `.swatch` colour picker is **dropped** — colour is assigned per user, §7.1)* | — |
| Video-element renderer (for the §13 fallback) | `app/ui/main.js:2022` |
| Transparent + click-through + always-on-top discipline, always-visible badge | ADR-100, `overlay.js:19` |
| Bounds + fail-closed decode posture (`MAX_ANNOT_POINTS`, 256 strokes) | `crates/ras-protocol/src/lib.rs:545` |
| "Display data ⇒ no capability" reasoning | ADR-097 |

**Changed:** stroke splitting (§4), colour off the wire + author-scoped `undo`/`erase`/`clear` (§4),
sharer-assigned one-colour-per-user replacing the colour picker (§7.1), named cursors (§7.3),
transport (§5), Electron overlay window (§8).

### 15.1 What we reuse from Jitsi (all **Apache-2.0** — clean against `deny.toml`)

Verified: `jitsi-meet`, `lib-jitsi-meet` and `jitsi-meet-electron` (2026.8.0, Electron ^43.3.0) are all
Apache-2.0, so nothing here trips this repo's GPL/AGPL denial (ADR-051).

| Reused | Source |
|---|---|
| Endpoint-message envelope convention (`{ name, ...op }`, unicast) | `react/features/remote-control/functions.ts:43` |
| Renderer-relays → main-process-owns-the-privileged-work architecture | `@jitsi/electron-sdk` `remotecontrol/{renderer,main}.js` |
| `sourceId` → display resolution, per platform, with the macOS scale-factor trap | `remotecontrol/main.js:105`, `:323` |
| `display-metrics-changed` re-resolution | `remotecontrol/main.js:312` |
| Native consent gate before a privileged session starts (`requestConsent`) | `remotecontrol/main.js` (`setupRemoteControlMain` options) |

**Not reused:** `robotjs` and the entire input-injection path — we need no OS input, which is what
keeps §10's threat model small.

## 16. Shape and phasing

**v1 is TypeScript, not Rust.** The data path is a browser/Electron data channel and the renderer is a
canvas — a Rust core and an N-API addon would be pure overhead on the critical path. Rust re-enters
with the iroh transport (§14). The op schema is defined once and mirrored into `ras-protocol` so the
two paths stay wire-compatible.

```
sdk/annotate/
  core/        op schema, codec + bounds, stroke store, author-scoped removes, palette    (no I/O)
  surface/     drawing canvas + toolbar + normalization + local echo + own-stroke record   (browser + Electron)
  overlay/     Electron transparent click-through window + renderer + badge                (sharer)
  transport/
    jitsi.ts   endpoint-message adapter (lib-jitsi-meet + External API)
    iroh.ts    deferred (§14)
  adapters/
    jitsi-electron/   wiring for jitsi-meet-electron
```

| Phase | Deliverable | Proves |
|---|---|---|
| **A1** | `core` + `surface`, local-only, no transport | Geometry + normalization port cleanly |
| **A2** | `transport/jitsi` + `overlay`, one annotator → one Electron sharer | The capture-path insight (§2) end to end |
| **A2.5** | **Measure draw→see latency; raise `desktopSharingFrameRate.max` above 5 and re-measure** | §11.1 — whether the delay is config or architecture. **Do this before building A3.** |
| **A3** | Multi-participant: sharer-assigned colours + `roster`, N **named** cursors, author-scoped `undo`/`erase`/`clear` | The actual requirement |
| **A4** | Consent, badge, allow-list, rate limits, bounds, window-share detection | §9/§10/§12 — shippable |
| **A5** | Browser annotator → Electron sharer | "browser also works" |
| **A6** | §13 fallback; then §14 iroh transport | Product-independence |

A2 is the one that can invalidate the design. Build it before A1 is polished — and run **A2.5**
immediately after, because if the ~1 s delay survives a raised frame rate, the echo policy (§11.2) and
the deferred viewer-side preview (§11.3) stop being refinements and become the main work.

---

## 17. If the delay survives: offloading the screen share

If §11.1 does not bring draw→see down far enough, the next move is to stop sending the shared screen
through JVB and carry it ourselves. That is a sound instinct — we already own a latency-first screen
pipeline (ScreenCaptureKit + VideoToolbox, per-frame QUIC uni-streams, ABR, forced-IDR resync) — but
it is also **the largest scope increase available in this project**, so it needs a decision rule, not
an inclination.

### 17.1 First decompose the measurement — it decides everything

"~1 s" is three different numbers, and each one points at a different fix. A2.5 must split them:

| Term | What it is | How to measure |
|---|---|---|
| **t1** | draw → op reaches the sharer | sharer `ack` timestamp (already in §11.2) |
| **t2** | sharer renders → the mark is in an encoded outgoing frame | pixel beacon (§11.2) read from the sharer's **local preview** track |
| **t3** | outgoing frame → a viewer displays it | pixel beacon read at the viewer, minus t2 |

- **t2 dominates** → it is capture/encode. Fixable with config (§11.1) or our own capture. **Cheap.**
- **t3 dominates** → it is JVB relay + jitter buffer + decode. **Only bypassing that path helps**, and
  that is §17.3.
- **t1 dominates** → the relay is the problem, not the video, and the answer is §14, not a media server.

Do not choose an offload shape before this split exists. The expensive option only pays if t3 wins.

### 17.2 Two cheap levers to exhaust first

1. **`desktopSharingFrameRate.max > 5`** (§11.1) — attacks t2, flips three settings at once.
2. **`receiver.playoutDelayHint = 0`** on the screen-share receiver — attacks **t3**, the receiver's
   jitter buffer. **Verified unused: `playoutDelayHint` appears nowhere in `lib-jitsi-meet`**, though
   the `playout-delay` RTP header extension shows up in its SDP, and `findReceiverForTrack`
   (`TraceablePeerConnection.ts:2365`) already hands you the receiver. This is a ~2-line experiment
   against the exact term that a media server would cost months to attack.

### 17.3 The three offload shapes, honestly priced

| Shape | What it is | Fixes | Cost |
|---|---|---|---|
| **B1 · Own capture, Jitsi transport** | Produce the screen track ourselves in Electron and `replaceTrack` it into the Jitsi peer connection | **t2 only** | Low. No server. Viewers unchanged. |
| **B2 · Gateway re-injection** | Sharer → our server over QUIC; a headless client publishes it into the Jitsi room | t2 | **Negative overall — adds a hop and still leaves every viewer behind JVB.** Do not build this. |
| **B3 · Our own path end to end** | Screen + annotation ride our transport to every participant; Jitsi keeps people and audio | t2 **and** t3 | **Very high** — see below |

**B2 is the trap.** It looks like "offload the screen share to a separate server," but viewers still
receive through JVB, so t3 is untouched and a server hop is added. It makes the measured problem
worse. Named here so it does not get built by accident.

**B3 is the real version, and it is a different product.** It means our stack carries the share to
*every* participant — which is an SFU, or a mesh. And note where that lands: RAS screen sharing is
**1 host → 1 controller**, so making it N-way is precisely the "iroh doesn't do groups" problem this
whole thread started from. `iroh-gossip` (§14) does **not** solve it — gossip is a signalling overlay
for small, best-effort messages (~4 KiB, unordered), emphatically not a video fanout. B3 needs real
media fanout that does not exist in this repo yet.

That is not an argument against B3 — "Casual RAS attaches to your meeting: we carry the screen, Jitsi
carries the people" is a coherent and differentiated product, and it is where the SDK thesis
eventually points. It is an argument against reaching for it as a **latency fix**, before t1/t2/t3 are
split and the two ~2-line levers in §17.2 are tried.

### 17.4 Decision rule

> Run A2.5. Split t1/t2/t3. Try §17.2's two levers.
> If t2 dominated and the levers fixed it → done, ship the Jitsi-native SDK.
> If t2 dominated and they did not → **B1**.
> If t3 dominates → stop, and decide **B3 as a product**, on its own merits and its own roadmap slot.
> Never **B2**.

---

## 18. Mobile, and why the media path stays on Jitsi by default

### 18.1 The release-cadence asymmetry

The web app and the Electron app ship when we decide. A mobile app ships when a store review allows,
and then reaches users when *they* update — which, for a long tail, is never. **The deployed
population is permanently mixed**, so any design that assumes a synchronised upgrade is a design that
breaks mobile.

**What rescues this is §2, again: viewing annotations needs no client code at all.** Marks render on
the sharer's desktop and travel inside the screen-share video, so every participant — a two-year-old
mobile build, an untested browser, someone on a gateway — sees annotations correctly and always
will. There is no forward-compatibility surface in the common case because there is no code in it.

Only **drawing** needs our code. That narrows compatibility to exactly one direction: *a current
sharer must keep understanding ops from an old annotator.*

### 18.2 The four rules (implemented in `core/compat.js`)

| | Rule | Why |
|---|---|---|
| **R1** | Accept a **version range**, never a single version | An equality check cuts off every client that has not updated |
| **R2** | **Never add a required field.** New information is optional, and its **absence means the old behaviour** | An old client omits everything new; absence must not be an error |
| **R3** | Unknown **op tags are ignored**, not rejected — but malformed **known** ops are still hard rejections | Forward compatibility and fail-closed security are different concerns and must not be conflated |
| **R4** | **Advertise capabilities, gate UI on them** | Otherwise a new client offers a button whose ops the sharer silently drops |

`decode` therefore returns `kind: 'ignore' | 'reject'`. *Ignore* means "not ours, or newer than us" —
benign, never logged as an error, never counted toward an abuse threshold. *Reject* means malformed
or genuinely below the version floor. Conflating the two is what turns a version skew into an
apparent attack.

Capability exchange is `hello` (annotator → sharer) and `caps` on `roster` (sharer → everyone).
**Both are optional in both directions**: a peer that never sends one is assumed `BASELINE_CAPS`,
which is exactly what a client written before capability exchange existed emits.

### 18.3 Mobile's role

| Capability | Mobile | Why |
|---|---|---|
| **See annotations** | ✅ always, no code | It is in the video (§2) |
| **Draw / cursor** | ✅ with SDK code | `surface` is portable; coordinates are normalized |
| **Host the overlay (be the annotated sharer)** | ❌ | Needs a transparent always-on-top desktop window (§8) |
| **Offloaded media path** | ❌ | No native QUIC stack in the mobile app |

### 18.4 So the media path stays on Jitsi — configurable, negotiated, never assumed

This settles §17: **B0 (Jitsi's own media path) is the default and stays the default.** Offload is
*configurable*, and a client uses it only if it advertises the capability; anything that does not —
mobile, browsers, old builds — silently continues on the JVB path with no degradation and no code
change. That means:

- **Offload can never be a requirement**, because a meeting will always contain a client that cannot
  do it. It is an optimisation for the clients that can.
- **A mixed meeting must work.** If the sharer offloads for some participants, the JVB path must
  still carry everyone else — which means the sharer is *publishing twice*, and that cost is a
  reason to be sceptical of offload rather than an argument for it.
- **B3 is correspondingly harder than §17 priced it.** It is not "replace the media path"; it is
  "run a second media path alongside the one you must keep anyway".

The practical consequence for the roadmap: **§11.1's frame-rate lever is worth far more than it
looked**, because it improves latency for *every* client — mobile included — with a config change and
no release cycle at all.
