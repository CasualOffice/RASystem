# Phase S — Risk-validation spike (throwaway)

Measures the biggest unvalidated bets before real Phase 1. See `../docs/design/phase-S-design.md`
for the design + go/no-go criteria. **This code is disposable — carry the numbers, not the code.**

Two decoupled probes:
- `iroh-probe/` — Iroh 1.x transport: direct-vs-relay, handshake time, per-frame RTT under load.
- `latency-probe/` — the media half: a **turnkey WebCodecs loopback** (`web/index.html`) + a Windows
  DXGI→H.264 capture skeleton (Rust).

---

> **Platform note (ADR-054):** development leads on **macOS** (this Mac). Run everything below on
> macOS/Linux; the Windows host is a later port. Test the WebCodecs harness in **both Safari (the
> WKWebView engine Tauri uses on macOS) and Chrome** — Safari's result answers a real open question
> (WKWebView WebCodecs H.264 reliability + the reported ~3 s decode bug).

## A. WebCodecs latency harness — RUN THIS FIRST (turnkey, no build)

Open `latency-probe/web/index.html` in **Safari** and **Chrome** on your Mac and click **Start**. It
generates an animated frame, H.264-encodes it (no B-frames, realtime), decodes it with
`VideoDecoder`, renders to canvas, and reports **encode / decode / present / end-to-end** latency —
validating the entire controller-side path, `avcC`-vs-`annexB` handling, and `VideoFrame.close()`
discipline, decoupled from network and capture.

Record for the go/no-go: median & p95 end-to-end latency, decode latency, whether the ~1-frame
compositor penalty appears (toggle **rVFC vs immediate draw**), and any WebView2 quirks.

> Chrome/Edge have full WebCodecs H.264. If `VideoEncoder`/`VideoDecoder` is missing, note the engine
> — that itself is a finding for the controller platform matrix (`docs/12 §4`).

## B. Iroh transport probe

```
# machine 1 (host side):
cargo run -p iroh-probe -- server
#   → prints an ENDPOINT_ID and whether connections come in direct or relayed

# machine 2 (controller side), across each network in the matrix:
cargo run -p iroh-probe -- client <ENDPOINT_ID>
#   → prints connection type, handshake time, and RTT stats (min/median/p95/max)
```

Run the client across the **network matrix** (`docs/08 §3`): same-LAN · different NATs · **symmetric
NAT** · **UDP-blocked / 443-only** · relay-only · Wi-Fi↔hotspot migration. Record for each: did it
connect? direct or relayed? RTT distribution?

> **Iroh 1.x API is young.** `cargo build -p iroh-probe` and reconcile any drift against
> `cargo doc -p iroh --open` — the `// VERIFY:` comments mark the calls most likely to have changed.

## C. Capture skeleton (macOS lead)

`cargo run -p latency-probe` runs the `FrameSource` timing loop. It ships with a **synthetic** source
(std-only, works anywhere, validates the harness) and a **macOS ScreenCaptureKit → VideoToolbox**
source to implement (`src/frame_source.rs`, `#[cfg(target_os = "macos")]`) using the API sequence in
`docs/18`. (The Windows DXGI+MF source is outlined for the later port.) Once it emits real Annex-B
frames, feed them to harness **A** (via a localhost WebSocket — TODO in the file) to measure true
glass-to-glass on your Mac.

---

## What to report back
1. WebCodecs harness: median/p95 e2e + decode latency + compositor-frame observation (+ engine).
2. Iroh probe: connect success + direct/relay + RTT per network profile.
3. (If implemented) capture→encode FPS + latency from the Windows source.

I fold these into a **go / pivot / no-go ADR** (`docs/14`) that gates real Phase 1.
