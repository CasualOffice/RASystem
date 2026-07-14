# Casual RAS — Controller (Tauri v2)

The technician / viewer app. It connects to a remote **host** over iroh and renders that machine's
screen, with a **viewer-side annotation** overlay. **View-only** — no clicks or keystrokes are ever
injected into the host's OS; the only interaction is drawing annotations.

## Two ways to view

1. **Connect to a remote host (the real alpha flow).** On the other machine, run `ras-host`; it
   prints a **connection ticket** (`CASUALRAS1:…`). Paste it into the top bar here and press
   **Connect**. The controller dials the host over iroh (`IrohSessionTransport` behind the
   `SessionTransport` seam), and the host's screen renders here. Works on macOS/Linux/Windows — the
   viewer only decodes (WebCodecs), so it is platform-independent.
2. **Local mirror (macOS only, test).** The **Local mirror** button runs a host + controller in this
   one process over the in-memory loopback and shows *this* machine's screen — a one-box glass-to-glass
   test without a second machine.

Encoded H.264 access units arrive on a **binary** Tauri `Channel` as `ras_core::frame_channel` blobs
(24-byte `RAS1` header + Annex-B); a WebCodecs `VideoDecoder` decodes each to a `<canvas>`. No pixels
ever cross JSON IPC. A red **LIVE** banner is always visible while a session renders (Invariant 7).

## Annotations (view-only markup)

Pick a tool from the floating toolbar — **pen / arrow / rectangle / highlighter**, four colors, undo,
clear — and draw over the shared screen. This is **not** remote control: annotations are a drawing
overlay, nothing is injected into the host's OS. When the tool is **🚫 (off)** the overlay ignores
pointer events entirely, so the app is strictly view-only unless you deliberately pick a tool.

> **v1 = viewer-side.** Strokes are local to this window today. Making them visible on the *host's*
> screen (a transparent click-through overlay driven by a typed `ControlMsg::Annotation`) is the next
> step and lands with the host GUI.

## Layout

```
controller/
  src-tauri/            # Rust: Tauri app + connect_to_host / disconnect / start_mirror / request_keyframe
    src/main.rs
    tauri.conf.json     # withGlobalTauri (no bundler), strict-ish CSP, single "main" window
    capabilities/       # core:default for the main window
  ui/                   # static frontend — index.html + main.js (WebCodecs + annotations) + style.css
```

The app is intentionally **outside the root Cargo workspace** (heavy Tauri/WebView deps) — build it
from its own directory.

## Build & run

```sh
cd controller/src-tauri
cargo build            # compile-check (also validates tauri.conf.json + capabilities)
cargo run              # opens the controller window
```

To view a remote host, run `ras-host` on the other machine (`cargo run -p ras-host` from the repo
root, macOS-only for now) and paste the ticket it prints. The **Local mirror** button needs macOS +
**Screen Recording** permission (TCC) for the built binary; the first run prompts.

> A Tauri CLI / bundler is optional — this shell uses a static frontend (`app.withGlobalTauri`), so
> plain `cargo run` is enough. A React/TS + bundler frontend replaces `ui/` when the session UI grows.

## What this is NOT yet

- **Consent is a no-op seam** (`AllowAllValidator`, Phase-1): anyone with the ticket who reaches the
  host is served. Real grant validation + the host **consent window** land with the host GUI.
- **Annotations are viewer-side only** (see above).
- **No host GUI / overlay** yet; the host is an alpha CLI (`ras-host`).
- Frontend is minimal static HTML/JS; the Web Worker + `OffscreenCanvas` renderer and the React/TS
  UI + strict-CSP hardening land later.
