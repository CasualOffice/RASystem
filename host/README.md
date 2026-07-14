# Casual RAS — Host (Tauri v2)

The shared-machine app. It captures **this** machine's screen and serves it to one remote controller
over iroh, and shows the controller's **remote pointer** ("look here") on screen. **View-only** — the
viewer can point but cannot click or type on this machine.

## Windows

- **Control panel** — shows the **connection ticket** to share (with a Copy button), an always-visible
  session indicator (`● REMOTE VIEWING ACTIVE` while a viewer is connected — Invariant 7), a
  **Stop sharing** button (always present), and the **Allow / Deny consent prompt** that pops up when a
  viewer requests access.
- **Overlay** — a transparent, click-through, always-on-top window covering the screen. It draws the
  connected viewer's pointer where they hover on the shared screen. Being click-through, it never
  intercepts your own mouse/keyboard — it is purely visual (ADR-061), so it cannot control anything.

## Consent (Invariant 1)

When a viewer connects, **no pixels flow until you click Allow.** The host implements a real
`GrantValidator` that blocks the session in the handshake and shows an Allow/Deny prompt with the
viewer's short identity; Deny (or 90 s of silence) refuses the session, fail-closed. This app is
built with `ras-core`'s `default-features = false`, so the `insecure-no-auth` no-op validator that the
CLI/test paths use is **not even linked** — the product host cannot accidentally skip consent.

## How it works

Screen capture/encode is `ras-media-macos`; the stream is served over the real iroh transport
(`IrohSessionTransport`, the same `ras-core` spine the tests exercise). The controller's pointer
arrives as `ControlMsg::Pointer` and is surfaced as a `RemotePointer` lifecycle event, which the app
forwards to the overlay window to draw. One viewer at a time; when a viewer leaves, the host keeps
listening.

## Build & run (macOS)

```sh
cd host/src-tauri
cargo build     # compile-check (also validates tauri.conf.json + capabilities)
cargo run       # opens the control panel; copy the ticket into a controller on another machine
```

First run prompts for **Screen Recording** permission (TCC). Kept **out of the root Cargo workspace**
(heavy Tauri deps) — build from this directory.

## What this is NOT yet

- Consent is **local Allow/Deny** (real), but authorization is still coarse — no capability scoping,
  no signed grants/leases, no TPM-backed identity tiers yet (those are the Phase-2 grant model).
- **Single display**, and the overlay is captured into the shared stream (the viewer sees its own
  pointer reflected) — both are known alpha limitations; excluding the overlay from capture + proper
  multi-monitor mapping come with hardening.
- **macOS only** so far; the Linux/Windows capture backends are the next port.
