# Casual RAS — Controller (Tauri v2 shell)

The technician-side app. This is the **MVP shell** that proves the controller **video path**
(ADR-021/022, design §S3): encoded H.264 access units arrive on a **binary** Tauri `Channel`, a
WebCodecs `VideoDecoder` decodes each to a `VideoFrame`, and it renders to a `<canvas>`. No pixels
ever cross JSON IPC.

For the MVP the frames come from a **local mirror** — this Mac's own screen, captured and encoded by
`ras-media-macos` — but they flow through the **real `ras-core` session spine**: a `HostSession` and
a `ControllerSession` are connected by the in-memory **loopback transport** (host + controller in one
process), so each frame actually traverses handshake → authorize-gate (`AllowAllValidator`, the
Phase-1 no-op seam) → grant → media pump → teardown, and the webview's keyframe requests ride the
control channel. This is the same path the loopback e2e tests exercise, now with the real macOS
backends and a live WebCodecs renderer. It runs **glass-to-glass on one machine before the iroh
transport lands** (step 4 / M2); the loopback transport swaps for the concrete iroh one behind the
same `SessionTransport` seam — no controller/webview change.

## Layout

```
controller/
  src-tauri/            # Rust: Tauri app + the start/stop/request_keyframe commands + mirror feed
    src/main.rs
    tauri.conf.json     # withGlobalTauri (no bundler), strict-ish CSP, single "main" window
    capabilities/       # core:default for the main window
  ui/                   # static frontend (no bundler) — index.html + main.js (WebCodecs) + style.css
```

The app is intentionally **outside the root Cargo workspace** (heavy Tauri/WebView deps that the
core CI gates don't need) — build it from its own directory.

## Build & run (macOS, dev-lead)

Prereqs: a GUI login session + **Screen Recording** permission for the built binary (TCC); the first
run prompts. Not runnable headless/over SSH (capture needs the window server).

```sh
cd controller/src-tauri
cargo build            # compile-check (also validates tauri.conf.json + capabilities at build time)
cargo run              # opens the window; the webview auto-starts the local mirror
```

`cargo run` opens the controller window; `main.js` calls `start_mirror`, configures the decoder from
the returned descriptor, requests an IDR (infinite-GOP ⇒ the lone startup keyframe may predate the
decoder), and renders your screen back into the window. The HUD shows render fps / received /
decoded / loss gaps. A red **LIVE** banner is always visible (Invariant 7 — the session indicator is
not suppressible by the UI).

> A Tauri CLI is optional. This shell uses a static frontend (`app.withGlobalTauri`), so plain
> `cargo run` is enough — no `pnpm`/`vite` build step. A React/TS + bundler frontend replaces `ui/`
> when the session UI grows.

## What this is NOT yet

- No real transport: the two sessions share an in-process loopback, not a remote peer (iroh is
  step 4 / M2). Because it's one process, this is a mirror, not a two-party session.
- Auth/consent is a **no-op seam**: `AllowAllValidator` authorizes unconditionally (Phase-1,
  `insecure-no-auth`). Real grant validation, leases, and the host **consent window** land with the
  host app — the seam is present so that adds no signature churn.
- Frontend is minimal static HTML/JS; the Web Worker + `OffscreenCanvas` renderer and the React/TS
  controller UI + strict-CSP hardening land later.
