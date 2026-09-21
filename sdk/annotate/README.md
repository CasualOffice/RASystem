# `@casualoffice/annotate`

Multi-participant screen annotation you can drop into a meeting product. Remote participants draw on
the **screen-sharer's real desktop**; because those pixels are inside the screen capture, the marks
reach everyone else through the ordinary video path.

Extracted from the annotation slice proven in Casual RAS `v0.0.4-alpha` (ADR-097, ADR-100).
Design: [`docs/design/annotate-sdk-jitsi-design.md`](../../docs/design/annotate-sdk-jitsi-design.md) ·
Decision: ADR-107.

```
annotator A ─┐
annotator B ─┼─► bridge relay ─► SHARER ─► transparent overlay ─► screen capture
annotator C ─┘    (data)         (the ONLY renderer)                    │
                                                                        ▼
                                    ordinary meeting video ─► everyone, including mobile
```

## Why this shape

| Because only the sharer renders… | you don't need |
|---|---|
| there is one copy of the state | a CRDT, or any replication |
| late joiners just watch the video | state sync or replay |
| ops are id-addressed and idempotent | ordered or reliable delivery |
| viewers need no code at all | a client release to see annotations |

That last row is what makes mobile tractable: **seeing annotations requires no client code**, so
every participant — including a years-old mobile build — sees them correctly and always will. Only
*drawing* needs the SDK.

## Layout

| Path | Runs where | Depends on |
|---|---|---|
| `core/` | anywhere | nothing — pure, no DOM, no I/O |
| `surface/` | browser · Electron · mobile webview | DOM |
| `overlay/` | Electron main + a transparent window | Electron |
| `transport/jitsi.js` | wherever the conference lives | lib-jitsi-meet **or** the iframe External API |
| `latency/` | both ends | DOM (measurement harness) |
| `sharer.js` · `annotator.js` | anywhere | nothing — controllers, transport- and DOM-agnostic |
| `adapters/jitsi-electron/` | Electron | `jitsi-meet-electron` ([wiring guide](adapters/jitsi-electron/README.md)) |

No build step, no runtime dependencies, no TypeScript toolchain — plain ES modules with JSDoc, which
is what this repo's `node --check` gate expects.

```bash
npm test          # or: node --test 'test/*.test.js'
```

## The invariants worth knowing before you change anything

1. **Attribution comes from the relay, never the payload.** The envelope has no `from` field. Pass
   the relay-reported sender to `SharerSession.handle` or every ownership check is void.
2. **Colour is not on the wire.** It is assigned per participant by the sharer and derived from the
   author, so nobody can draw in someone else's colour.
3. **Every remove is author-scoped.** `undo` / `erase` / `clear:"mine"` only ever touch the sender's
   own strokes — the store re-filters by author rather than trusting the ids it is handed.
4. **`append` carries its index.** Points are written at a position, never pushed, which makes the
   stream idempotent and order-independent. An append never overwrites a point already present.
5. **Decode is fail-closed on malformed input, but forgiving of version skew.** `kind: 'reject'`
   means malformed; `kind: 'ignore'` means "not ours, or newer than us" — never log the latter as an
   error.
6. **The overlay is always transparent and click-through.** Making it interactive is what caused the
   macOS white screen that ADR-100 was written to fix.

## Quick start — annotator

```js
import { ConferenceTransport, StrokeSender, CursorSender } from '@casualoffice/annotate/transport/jitsi';
import { hello, CAPS, LOCAL_CAPS } from '@casualoffice/annotate/core';

const transport = new ConferenceTransport(conference, JitsiMeetJS.events.conference);
transport.send(sharerId, hello([ ...LOCAL_CAPS ]));   // optional; absence means "baseline"

const strokes = new StrokeSender(transport, sharerId);
const cursors = new CursorSender(transport, sharerId, { hz: 20 });
```

## Quick start — sharer

```js
import { SharerSession, ADMIT } from '@casualoffice/annotate/core';

const session = new SharerSession({ admit: ADMIT.NONE });  // OFF until the user opts in
transport.onOp((sender, msg) => {
    const r = session.handle(sender, msg);      // `sender` MUST come from the relay
    if (r.ack) transport.send(sender, r.ack);   // frees the annotator's local echo
});
```

## Status — honest

| Module | State |
|---|---|
| `core/` (ops, store, palette, geometry, session, compat) | **Implemented and unit-tested** |
| `sharer.js` · `annotator.js` controllers | **Implemented and unit-tested end to end** over a two-endpoint fake bridge |
| `transport/jitsi.js` | **Implemented, tested against a fake conference.** Never run against a live JVB |
| `latency/` estimator + verdict | **Implemented and unit-tested** |
| `adapters/jitsi-electron/` | **Written, never executed** — needs Electron + a jitsi-meet-electron build |
| `latency/` pixel beacon (draw/read) | **Written, never executed** — needs a DOM and a real encoder |
| `surface/` | **Written, never executed** — needs a DOM |
| `overlay/` | **Written, never executed** — needs Electron, and its display resolution is per-platform (§8.1) |

**102 unit tests green** (`npm test`), covering the op codec, the store's author-scoping, palette
assignment, letterbox-aware geometry, the session's security posture, backward compatibility, the
transport's repair/throttle behaviour, and an end-to-end annotator→sharer round trip with acks.

Nothing here has been run inside a real Jitsi meeting yet. The next step is **A2** — one annotator
to one Electron sharer — because it is the phase that can invalidate the design, followed
immediately by **A2.5**, the latency measurement that decides everything downstream (§11, §17).

**Known limits, by construction:**

- A **window share** is not captured with the overlay. Detected and refused, not silently broken.
- **Linux with more than one display** cannot resolve which display is shared — Jitsi's own SDK
  gives up here too. Refused rather than guessed.
- **`object-fit: cover`** (tile view, cropped layouts) makes coordinates unaddressable. Drawing is
  disabled with a reason.
- A **browser sharer** cannot host an overlay at all.
- With **Jitsi E2EE** on, media is end-to-end encrypted but endpoint messages are not, so annotation
  geometry is visible to the bridge when the video is not.
