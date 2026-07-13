# 10 — Media Pipeline Deep-Dive: Capture → Encode → Transport → Decode → Render

> Windows host + Tauri/WebCodecs controller. Grounded July 2026. `[verify]` = confirm on target
> hardware. Priorities: **security → latency → UX**; the whole pipeline is tuned for latency subject
> to never breaking the security invariants.

## 1. End-to-end shape

```
[Host] DXGI capture (GPU texture) ──► HW H.264 encode (D3D11 texture-in, zero-copy)
      ──► Annex-B chunks ──► Iroh (per-frame stream OR datagram+FEC)
[Controller] ──► Tauri Channel(Raw bytes) ──► Web Worker: WebCodecs VideoDecoder
      ──► VideoFrame ──► WebGL/OffscreenCanvas (GPU-resident) ──► present
```

Target budget (`docs/01 §11`): median LAN glass-to-glass < 120 ms; internet overhead < 80 ms beyond
RTT; input-to-visible < 100 ms.

## 2. Capture (Windows) — DXGI Desktop Duplication primary

- **Choose DXGI Desktop Duplication (`IDXGIOutputDuplication`) as primary**, WGC
  (`windows-capture`) as fallback for per-window and hybrid-GPU laptops. Rationale: lowest latency,
  **no capture border ever** (WGC's yellow border cannot be removed on Win10 22H2 at all —
  `E_NOINTERFACE` — and needs consent + manifest capability on Win11), rich **separate cursor
  metadata**, and **dirty/move rects** for bandwidth reduction. This matches RustDesk (`scrap`).
- **Cursor out-of-band:** DXGI exposes `PointerPosition` + `GetFramePointerShape()` — send the
  cursor as high-rate metadata separate from the encoded desktop, so the remote pointer stays smooth
  without re-encoding. (WGC has no shape API; you'd source position via `GetCursorInfo`.)
- **Build the resilience loop from day one:** DXGI throws `DXGI_ERROR_ACCESS_LOST` on *every*
  transition (resolution/mode change, desktop switch to UAC/lock/CAD, fast-user-switch, driver TDR,
  DWM on/off, fullscreen-exclusive entry) → release + `DuplicateOutput()` again. Static screen →
  `DXGI_ERROR_WAIT_TIMEOUT` (normal; never use timeout 0; repeat last frame to keep the stream
  alive).
- **Correctness caveats:** DXGI hands rotated/portrait desktops **un-rotated** (rotate yourself);
  HDR desktops arrive as `R16G16B16A16_FLOAT` and need tone-mapping (SDR is always
  `B8G8R8A8_UNORM`); **DRM/HDCP content is black on both APIs** (unavoidable); fullscreen-exclusive
  apps bypass DWM → black (borderless/windowed is fine); headless/monitor-off machines need a
  **virtual display driver**; hybrid GPUs must duplicate on the adapter driving the output or you get
  `DXGI_ERROR_UNSUPPORTED` / per-frame cross-GPU copies.
- **Rust:** `windows` (windows-rs) directly, or bootstrap with `scrap` (MIT) / `rusty-duplication`.

## 3. Encode — hardware, low-latency, license-clean

- **Hardware encode, detected at runtime**, with these settings for interactivity:
  - **D3D11 texture-in (zero-copy)** — a GPU→CPU→GPU copy per frame kills the latency budget.
  - **B-frames OFF** — this is the real latency win (removes reorder delay), *not* the profile.
  - **Main profile** (CABAC efficiency without B-frames); Baseline only for max decoder compat.
  - **CBR** rate control (predictable network pacing); **infinite GOP + forced-IDR-on-demand**
    (`CODECAPI_AVEncVideoForceKeyFrame` / `NV_ENC_PIC_FLAG_FORCEIDR`) rather than periodic keyframes.
  - MF low latency = `CODECAPI_AVLowLatencyMode` / `MF_LOW_LATENCY` (note:
    `CODECAPI_AVEncCommonLowLatency` does **not** exist — a common conflation).
- **Path choice:** start on the **Media Foundation MFT** (one vendor-agnostic path; `MFTEnumEx`
  with `MFT_ENUM_FLAG_HARDWARE|ASYNCMFT` to detect). Move to **direct NVENC/AMF/oneVPL** (à la
  RustDesk's FFmpeg-based `hwcodec`, MIT) when we need ultra-low-latency knobs (intra-refresh, LTR).
  `[verify]` no production-grade standalone NVENC Rust crate — the maintained routes are
  FFmpeg-based.
- **Software fallback = OpenH264 via the `openh264` crate's `libloading` feature** (loads Cisco's
  prebuilt royalty-free binary). **Do NOT use x264/libx264** — it's GPL and forces source release;
  fatal for a proprietary SDK. (H.264 itself carries MPEG-LA *patent* considerations — legal review.)

## 4. Transport framing & loss handling

- **Emit Annex-B bitstream** (start-code delimited, SPS/PPS in-band and re-sendable). This is more
  robust to loss than avcC/length-prefixed and avoids keeping an out-of-band `description` in sync
  with the decoder (see §6). It also decodes cleanly in WebCodecs without a `description` field.
- **Loss strategy (prior art: Moonlight/Sunshine):** prefer **per-frame Reed-Solomon FEC**
  (`nanors`, MIT; block depth = 1 frame; effective below ~3–5% loss) over retransmit, and
  **reference-frame-invalidation / intra-refresh over IDR-on-loss** to avoid bitrate spikes. On
  unrecoverable loss: freeze on last-good frame and send an upstream **PLI-style IDR request**.
- Transport primitive selection lives in `docs/09 §5` (per-frame QUIC stream vs datagram+FEC).

## 5. Decode — WebCodecs `VideoDecoder`

- **avcC vs Annex-B is the #1 silent-failure trap:** the presence of `description` selects the
  format. We emit **Annex-B → omit `description`**. Feeding avc bytes without `description` → the
  `output` callback simply never fires.
- **First chunk after `configure()`/`reset()`/`flush()` MUST be a keyframe** (true IDR — a
  recovery-point SEI intra-refresh frame is **not** accepted as a keyframe).
- **Config:** fully-qualified `codec` (e.g. `avc1.4D401F` Main L3.1), `optimizeForLatency:true`
  (hint only), explicit `colorSpace` (most desktop encoders emit **limited-range BT.709**; range
  mismatch = crushed/washed colors), `hardwareAcceleration:"prefer-hardware"` with a
  `prefer-software` retry.
- **Error recovery:** a decode error moves the decoder to terminal `"closed"`; **recovery = create a
  NEW `VideoDecoder` and feed a keyframe.** WebCodecs has no jitter buffer — we own reorder-by-
  timestamp and loss detection (real-time buffer ~10–50 ms).
- **`isConfigSupported()===true` ≠ frames will decode** — always wire the `error` path + fallback.

## 6. Render — keep frames GPU-resident

- `VideoFrame` is a `TexImageSource`: import via **WebGL `texImage2D` / WebGPU
  `copyExternalImageToTexture`**, not a Canvas2D CPU round-trip.
- **Decode + render in a Web Worker with `OffscreenCanvas`** (`VideoFrame` is Transferable) so frame
  delivery never blocks the UI thread — the local cursor and stop button stay responsive during
  video stalls (Invariant: latency beats UX; a stalled stream must not freeze controls).
- **Memory footgun (critical):** **`VideoFrame.close()` on every frame within a frame or two** — MDN
  warns apps "can crash with fewer than 100 active frames"; HW decoders have a tiny output-buffer
  pool and unclosed frames **stall decoding**. Manage input backpressure (`decodeQueueSize`/
  `dequeue`) *and* output backpressure (unclosed frames) separately.

## 7. Latency budget & the structural penalty

- WebCodecs **decode ≈ native** (~3–10 ms HW). The unavoidable browser cost is **~one extra
  compositor frame (~16 ms @60 Hz)** — a browser cannot take the native fullscreen `FLIP_DISCARD`
  swapchain bypass. Native game-streamers (Parsec ~4–8 ms whole-pipeline LAN) win mainly by
  bypassing the compositor.
- **Native-surface fallback (planned v2 / latency-critical / Linux):** decode in Rust
  (Media Foundation/VideoToolbox/`ffmpeg-next`) and direct-present to a D3D11/Metal surface with a
  transparent Tauri webview overlay for UI. See `docs/12 §native-fallback`. **Trigger:** measured
  glass-to-glass exceeds target by ≥1 frame and profiling blames compositor/present.

## 8. Decisions & open validation
- **ADR:** DXGI primary capture; HW encode with B-frames-off/Main/CBR/infinite-GOP+forced-IDR;
  OpenH264 (`libloading`) software fallback (never x264); Annex-B bitstream; FEC over ARQ;
  WebCodecs MVP render, native-surface as planned v2/Linux.
- **Spike must measure:** capture→encode→decode→render per-stage latency on the `docs/08` workloads;
  DXGI ACCESS_LOST recovery time; WebCodecs decode latency in WebView2 (watch the open macOS ~3 s
  H.264 bug for later).

## 9. Sources
learn.microsoft.com: desktop-dup-api, AcquireNextFrame, h-264-video-encoder, CODECAPI_AVLowLatencyMode,
eAVEncCommonRateControlMode, SendInput/virtual-screen · w3.org/TR/webcodecs (+ avc registration) ·
developer.mozilla.org VideoDecoder/VideoFrame/EncodedVideoChunk · developer.chrome.com webcodecs ·
github.com/rustdesk-org/hwcodec · docs.rs/openh264 · x264.org/licensing · moonlight/sunshine wikis ·
github.com/NiiightmareXD/windows-capture.
