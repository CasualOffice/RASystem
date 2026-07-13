# 12 — Controller Deep-Dive: Tauri v2 + React + WebCodecs

> Grounded July 2026. `[verify]` = benchmark on the pinned Tauri version and target webview.
> Priorities: **security → latency → UX**.

## 1. Architecture

The controller is a **Tauri v2** app: a **Rust backend** (holds the single Iroh `Endpoint`, session
core crates, decode-feedback logic) and a **React/TypeScript webview** for the session UI (video
canvas, participant cursors, toolbar, control-request dialogs). Networking lives in Rust; the
webview never touches Iroh directly. The same pattern is reused for the **host consent UI** so both
apps share one UI stack.

The controller UI components are written from the start as the **future React controller SDK**
(`RemoteSessionView`, `ParticipantCursorLayer`, `ControlRequestDialog`, hooks — see `docs/05`), so
SDK extraction is drawing a boundary, not a rewrite (strategy S1).

## 2. Security (this is a controller of *remote machines* — treat it as high-value)

- **Pin Tauri ≥ 2.11.1 (non-negotiable).** Advisory **GHSA-7gmj-67g7-phm9** (Origin Confusion): a
  flawed `is_local_url()` on Windows/Android let a **remote page invoke local-only IPC commands** in
  `>=2.0, <=2.11.0`. Also **GHSA-57fm-592m-34r7** (iframes bypass origin checks). Track advisories.
- **Deny-by-default Capabilities/Permissions** (`src-tauri/capabilities/`), scoped per window. Never
  expose broad commands to any window that renders remote content.
- **Enable the Isolation Pattern** (AES-GCM IPC interception/validation) and a **strict CSP** in
  `tauri.conf.json`: locked `default-src`, no `unsafe-eval`, no remote scripts; open
  `media-src`/`connect-src` only for our custom protocol / localhost as narrowly as possible.
- **Render the remote feed only to `<canvas>` (it's data, not code) — never into DOM/HTML.**
  Validate all IPC input in Rust; never pass remote bytes to shell/system calls. Decoding an
  attacker-controlled H.264 stream exercises the platform decoder — **treat the stream as hostile
  input**.

## 3. Getting encoded frames into the webview

- **Use `tauri::ipc::Channel` with `InvokeResponseBody::Raw(Vec<u8>)`** for the encoded-chunk
  stream. Channel guarantees ordering (events do not). **`Raw` sends true binary** (arrives as
  `ArrayBuffer`); returning a plain `Vec<u8>` would serialize to a JSON number-array (huge/slow) —
  always wrap in `Raw`/`Response`.
- **Channel has no backpressure** and no published throughput limits — **we throttle ourselves:
  when behind, drop to the next keyframe.** Manage both decode-input backpressure
  (`decodeQueueSize`/`dequeue`) and output backpressure (close every `VideoFrame` — see `docs/10`).
- **Decode + render in a Web Worker + `OffscreenCanvas`** so IPC delivery never blocks the UI
  thread.
- **Do NOT push frames through the Tauri event system** (JSON strings; can reorder; not for
  high-throughput).
- **Windows/WebView2 IPC caveat `[verify]`:** a maintainer-relayed, unexplained regression showed
  large-payload IPC far slower on Windows than macOS. **Benchmark WebView2 frame-stream throughput
  early**; if it's a bottleneck, evaluate a **localhost WebSocket** for the frame stream (WebView2
  handles WS natively) as an alternative to Channel. Encoded chunks are small, so Channel is likely
  fine — but measure.

## 4. Webview coverage — where WebCodecs is safe

| Engine | Platform | WebCodecs H.264 | Verdict |
|---|---|---|---|
| **WebView2** (Chromium, evergreen) | Windows | Yes, HW-accelerated | **SAFE — primary MVP target** |
| **WKWebView** (Safari 16.4+ engine) | macOS Ventura 13.3+ | Video-only WebCodecs present | **Runtime-probe; assume, don't guarantee** `[verify]` |
| **WebKitGTK** | Linux | Conditional (GStreamer plugin; HW-accel only ≥2.48) | **Fragile → plan native surface** |

- **Windows is the safe primary target** — ship WebCodecs here first.
- **macOS:** gate behind `isConfigSupported()`; require Ventura 13.3+; embedded-WKWebView WebCodecs
  parity with Safari is *very likely but not authoritatively confirmed* — **probe at runtime**. Watch
  the open ~3 s H.264 decode bug (#899) and a pre-Safari-27 B-frame ordering bug (encoding with **no
  B-frames** mitigates both — and we already do, per `docs/10`).
- **Linux:** WebKitGTK version divergence (Ubuntu 22.04 ships too old for WebCodecs) and NVIDIA
  HW-accel breakage make it fragile — **plan the native-surface path here from the start.**
- **Universal gate:** `'VideoDecoder' in window && (await
  VideoDecoder.isConfigSupported({codec:'avc1...'})).supported`, else native fallback.

## 5. Native-surface fallback (planned v2 / latency-critical / Linux)

When the WebCodecs path can't meet the latency SLA (structural ~1 compositor frame, `docs/10 §7`),
switch that platform to: **decode in Rust (Media Foundation/VideoToolbox/`ffmpeg-next`) and
direct-present to a native D3D11/Metal/Vulkan surface, with a transparent Tauri webview overlaid for
UI/cursors.** This buys back the compositor frame + upload cost at the price of per-OS compositing
code. **Trigger:** measured glass-to-glass exceeds target by ≥1 frame and profiling attributes it to
compositor/present. Keep WebCodecs for the Windows MVP; treat native-surface as the planned v2.

## 6. Caveats summary
- Pin Tauri ≥ 2.11.1 (Origin-Confusion CVE) — the single most important line here.
- Channel `Raw` for binary; never plain `Vec<u8>`; never the event system.
- No Channel backpressure — self-throttle to keyframe.
- Close every `VideoFrame` (see `docs/10 §6`).
- WebView2 large-payload IPC regression — benchmark; WS fallback available.
- macOS WKWebView WebCodecs unconfirmed — runtime-probe; Linux fragile — native surface.

## 7. Decisions & open validation
- **ADR:** Tauri v2 controller (pinned ≥2.11.1, deny-by-default capabilities, Isolation + strict
  CSP, canvas-only remote feed); WebCodecs render for the Windows MVP; native-surface as planned
  v2/Linux.
- **Spike must measure:** WebView2 Channel throughput for the frame stream; WebCodecs decode latency
  in WebView2; the native-surface trigger threshold.

## 8. Sources
v2.tauri.app (IPC, Channel, capabilities, CSP, isolation) · docs.rs/tauri (ipc::Channel, Response,
InvokeResponseBody) · github.com/tauri-apps security advisories GHSA-7gmj-67g7-phm9,
GHSA-57fm-592m-34r7 · w3.org/TR/webcodecs · developer.mozilla.org VideoDecoder/VideoFrame ·
caniuse.com/webcodecs · webkit.org (Safari 16.4 features) · webkitgtk.org (2.44, 2.48 releases).
