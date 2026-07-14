# Casual RAS — desktop app (Tauri v2)

**One app, both roles** (ADR-062). A home screen offers two choices:

- **Share this screen** — let someone view *this* machine. You approve them first (a local Allow/Deny
  prompt), and can stop anytime. Streams your screen over iroh and draws the viewer's remote pointer
  on a transparent overlay. **macOS-only for now** (needs the `ras-media-macos` capture backend).
- **Connect to a screen** — paste a ticket someone shared and view *their* screen, pointing at it with
  a remote cursor. **Works on macOS, Linux, and Windows** — the viewer only decodes (WebCodecs), so it
  is platform-independent.

Nobody installs two apps: the same binary is the agent *and* the controller.

## The flow (two machines)

1. On the machine to be shared, open the app → **Share this screen**. It shows a **connection ticket**
   (`CASUALRAS1:…`) — copy it and send it (chat/email).
2. On the other machine, open the app → **Connect to a screen**, paste the ticket, **Connect**.
3. Back on the sharing machine an **Allow / Deny** prompt appears — click **Allow**. The screen now
   renders on the viewer.
4. The viewer moves their mouse over the video → a **"look here" pointer** (a pulsing ring labelled
   *viewer*) follows it on the **sharing machine's** screen.

**View-only** — no clicks or keystrokes are ever injected into the shared machine's OS. The remote
pointer is a purely visual cursor (ADR-061), outside the input-injection risk Invariants 6/14 govern.

## Security posture

- **Consent is real (Invariant 1).** A connecting viewer is held in the handshake — **no pixels flow**
  — until the local user clicks Allow; Deny or 90 s of silence refuses fail-closed. The app is built
  with `ras-core` `default-features = false`, so the `insecure-no-auth` no-op validator is **not even
  linked**.
- **Always-visible indicator (Invariant 7).** While a viewer is connected the Share panel shows
  `● REMOTE VIEWING ACTIVE`; the Connect side shows a red **LIVE** banner. The UI cannot suppress them.
- **No pixels over JSON IPC.** Encoded H.264 access units ride a **binary** Tauri `Channel`
  (`ras_core::frame_channel` blobs: 24-byte `RAS1` header + Annex-B); a WebCodecs `VideoDecoder`
  decodes each to a `<canvas>`.

## Annotations (viewer-side markup)

A floating toolbar on the Connect view — **pen / arrow / rectangle / highlighter**, four colors, undo,
clear — draws over the shared screen locally (nothing injected). When the tool is **🚫 (off)** the
overlay ignores pointer events, so it stays strictly view-only unless you pick a tool.

## Layout

```
app/
  src-tauri/            # Rust: unified Tauri app
    src/main.rs         # connect_to_host / disconnect / send_pointer / request_keyframe
                        # + start_sharing / stop_sharing / respond_consent (LocalConsent = real consent)
    tauri.conf.json     # withGlobalTauri (no bundler), CSP, main + transparent overlay windows, bundle
    capabilities/       # core:default for main + overlay
  ui/                   # static frontend: index.html (home/share/connect) + main.js + overlay.* + style.css
```

Kept **outside the root Cargo workspace** (heavy Tauri/WebView deps) — build from this directory.

## Build & run

```sh
cd app/src-tauri
cargo run                 # opens the app (compile also validates tauri.conf.json + capabilities)
npx tauri build           # produce installers (.dmg / .AppImage + .deb / NSIS for the current OS)
```

First **Share** on macOS prompts for **Screen Recording** permission (TCC); grant it and relaunch.
There is also a headless `ras-host` CLI in the workspace (`cargo run -p ras-host`) for a no-GUI share.

## What this is NOT yet

- **Sharing is macOS-only** (the Linux PipeWire/VAAPI + Windows DXGI/MF capture backends are the next
  port). Connecting works on all three.
- Authorization is coarse — real local Allow/Deny consent, but **no signed grants/leases, no capability
  scoping, no TPM tiers** yet (the Phase-2 grant model).
- **Single display**, and the overlay is captured into the shared stream (the viewer sees its own
  pointer reflected) — both are known alpha limitations pending hardening.
- Builds are **unsigned** in the alpha (Gatekeeper/SmartScreen warn); EV signing is a hardening step.
