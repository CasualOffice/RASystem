# Casual RAS — Controller (Tauri v2 shell)

The technician-side app. This is the **MVP shell** that proves the controller **video path**
(ADR-021/022, design §S3): encoded H.264 access units arrive on a **binary** Tauri `Channel`, a
WebCodecs `VideoDecoder` decodes each to a `VideoFrame`, and it renders to a `<canvas>`. No pixels
ever cross JSON IPC.

For the MVP the frames come from a **local mirror** — this Mac's own screen, captured and encoded by
`ras-media-macos` in the app's Rust process — so the whole path is runnable **glass-to-glass on one
machine before the iroh transport lands** (step 4 / M2). The webview code is identical whichever
source feeds the channel; the real remote (iroh) source swaps in behind the same `Channel`.

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

- No transport: the feed is a local mirror, not a remote peer (iroh is step 4 / M2).
- No session/auth/consent wiring yet (grant validation, leases, the host consent window) — that is
  the host app + `ras-core::HostSession` integration, next.
- Frontend is minimal static HTML/JS; the React/TS controller UI + strict CSP hardening land later.
