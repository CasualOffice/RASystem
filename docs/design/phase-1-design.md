# Phase 1 Design — Transport + Screen Prototype (View-Only, No-Auth)

> Scope: PHASE 1 = transport + screen prototype, **VIEW-ONLY**, **single monitor**, **NO AUTH**
> (identity/pairing/grants are Phase 2). macOS is the development-lead host
> (ScreenCaptureKit → VideoToolbox → CGEvent), platform-abstracted behind backend traits;
> the Tauri v2 controller is cross-platform.
>
> Priority order is **STRICT: Security (1) > Latency (2) > UX (3)**. Where they conflict, the
> higher wins, and that ordering is baked into every seam below.
>
> This is a **design document**: the code blocks are compile-*conceptual* Rust/TS with `todo!()`
> bodies, dependency-light. They are the source for the crate trait skeletons. All bodies are stubs.
>
> **Load-bearing invariants honored throughout** (`CLAUDE.md`):
> - Transport authenticates **identity**, never **authorization** (Invariant 9).
> - A stalled/lost video must **NEVER** freeze the controller's local cursor or its controls
>   (latency beats UX).
> - `VideoFrame.close()` discipline: close every decoded frame within a frame or two.
> - Video is **droppable** and free of head-of-line blocking (per-frame QUIC stream *or*
>   datagram+FEC), on a channel **separate** from the reliable control channel.
> - Annex-B H.264, **B-frames off**. Keep the hot path **allocation-light**. Everything on-device.
> - Errors use the shared `ras_protocol::ErrorCode` taxonomy — no parallel error enums.

---

## 1. Overview & the Phase-1 frame-path data flow

Phase 1 wires one **host** (captures + encodes + sends) to one **controller** (receives +
decodes + renders) over a single Iroh QUIC connection carrying three reliability-split channels.
The host is macOS-lead behind traits; the controller is a Tauri v2 app whose webview decodes with
WebCodecs. No authorization exists yet, but a **no-op auth seam** is present at the exact point
Phase 2 needs it, shaped so filling it in is *additive* (never a breaking `match`/signature change).

### 1.1 Data flow diagram

```
  HOST (macOS lead, trait-abstracted)                 CONTROLLER (Tauri v2, cross-platform)
 ┌──────────────────────────────────────┐           ┌───────────────────────────────────────────┐
 │  ScreenCaptureKit  (SCStream)         │           │  src-tauri (Rust)                           │
 │    │ CVPixelBuffer / IOSurface (0-copy)│          │    ControllerSession (ras-core)             │
 │    ▼                                   │           │      │  VideoSource::recv → EncodedFrame     │
 │  VideoToolbox  (VTCompressionSession)  │           │      │  reorder-by-frame_id, gap→IDR req     │
 │    │  Annex-B, B-frames OFF, in-band   │           │      ▼                                     │
 │    │  SPS/PPS on every IDR             │           │    FrameSink::push (sync, non-blocking)    │
 │    ▼                                   │           │      │  tauri::ipc::Channel(Raw)             │
 │  EncodedFrame { frame_id, captured_at_ │           │      ▼  24-byte header + Annex-B AU        │
 │    us, is_keyframe, data: Bytes, config}│          │  ─────────── IPC boundary ───────────────  │
 │    │                                   │           │    ui/ webview  (main thread: relay only)  │
 │    ▼                                   │           │      │  postMessage(ArrayBuffer) →           │
 │  ras-core HostSession                  │           │      ▼                                     │
 │    │  VideoSink::send_frame (owned)    │           │    Web Worker  (decoder.worker.ts)         │
 │    ▼                                   │           │      │  EncodedVideoChunk (no `description`) │
 │  ras-transport-iroh                    │  QUIC     │      ▼                                     │
 │    VideoSink ═══════════════════════════════════► │    VideoDecoder (WebCodecs, HW-preferred)  │
 │      per-frame uni-stream OR datagram+FEC │ (drop-  │      │  output(VideoFrame)                  │
 │      (DROPPABLE, no cross-frame HOL)   │  pable)   │      ▼  present LATEST/rAF, then close()   │
 │                                        │           │    OffscreenCanvas → WebGL texImage2D      │
 │    ControlChannel ◄════════════════════════════►  │      (decode+present OFF the UI thread)     │
 │      reliable/ordered: Hello,          │ (reliable)│                                             │
 │      StreamConfig, KeyframeRequest,     │           │  ConnHealth (watch) ─► ConnBadge (Direct/  │
 │      DecoderFeedback, Bye              │           │      Relayed, RTT, Stalled)  ── never blocks│
 │    HealthObserver (watch) ─► ABR       │           └───────────────────────────────────────────┘
 └──────────────────────────────────────┘
```

### 1.2 Prose walkthrough

1. **Capture (host).** ScreenCaptureKit's `didOutputSampleBuffer` delegate pushes each
   IOSurface-backed `CVPixelBuffer` into a small SPSC ring. `ras-media`'s `ScreenCaptureBackend`
   exposes this as a **pull, synchronous, blocking-with-timeout** surface (`next_frame`) so the
   pipeline driver owns pacing and can **drop** frames it can't keep up with. No GPU→CPU→GPU bounce.

2. **Encode (host).** `VideoEncoderBackend::encode` consumes the surface by value (frees the pool
   slot immediately) and feeds it straight into VideoToolbox (zero-copy IOSurface import).
   Configured **RealTime, B-frames off (no reordering), CBR, infinite GOP + IDR-on-demand**. Output
   is converted to **Annex-B** with **in-band SPS/PPS on every IDR**, packaged as one canonical
   `EncodedFrame` whose `data` is `bytes::Bytes` (O(1) clone/slice, pooled — allocation-light).

3. **Send (host).** `ras-core` `HostSession` hands the owned `EncodedFrame` to
   `ras-transport-iroh`'s `VideoSink::send_frame`, which fragments+sends on the **droppable** video
   path (per-frame QUIC stream *or* datagram+FEC — chosen behind a trait post-spike) and returns a
   `SendOutcome { Sent | DroppedStale | DroppedCongested }` so the pacer gets a **source-side**
   backpressure signal instead of learning about loss a full RTT later. Loss is normal, silent, and
   never fatal on this path.

4. **Receive & reorder (controller).** `ras-transport-iroh` `VideoSource` reassembles fragments
   (FEC-recovers where possible) and yields `VideoEvent::Frame | FrameDropped`. `ras-core`'s ingest
   task reorders by `frame_id`/`captured_at_us`, and on a gap beyond recovery emits a single
   `KeyframeRequest` up the **reliable** control channel (never freezes; last-good frame stays on
   screen). The reorder/loss logic lives in Rust; JS stays a dumb decode-and-present sink.

5. **Decode & render (controller webview).** `FrameSink::push` (sync, non-blocking) forwards the
   frame over a `tauri::ipc::Channel(Raw)` as **one binary blob**: a fixed **24-byte little-endian
   header** + the Annex-B access unit. The webview **main thread only relays** the ArrayBuffer to a
   Web Worker; the Worker runs the `VideoDecoder` (Annex-B: **no `description`**, `optimizeForLatency`,
   keyframe-first) and presents to an `OffscreenCanvas` via WebGL. Decode+present happen **off the
   UI thread**, so a stalled video can never freeze the local cursor, toolbar, or Stop.

6. **Adapt & observe.** `ras-transport-iroh` publishes a `watch` of `ConnHealth`
   (Direct/Relayed, RTT, loss, bandwidth). The host ABR loop caps encoder bitrate to the measured
   path (never outruns the congestion window) and reacts every RTT via `set_bitrate` (keyframe-free),
   reserving IDR for genuine resync. The controller renders a Direct/Relayed/RTT/**Stalled** badge
   from the same snapshot on an independent task — it can honestly show "video stalled" while
   controls stay live.

**Static-screen keep-alive:** the host does **not** re-emit encoded video on a static screen (that
would burn the congestion window on the relayed path and can HOL a fresh frame). It sends a tiny
reliable **control-channel heartbeat** ("alive, last frame N valid"); WebCodecs already holds the
last presented frame on screen. Re-emit is reserved for "new subscriber needs current frame" (an
IDR-on-demand, not a timer).

---

## 2. Canonical cross-crate types

These are the **single** definitions every crate imports. Placement rule:
**hot-path frame types → `ras-media` (producer-owns); wire-negotiated / multi-reader types →
`ras-protocol`; transport-derived stats → `ras-transport-iroh`, re-exported.** No crate re-declares
any of these; core/controller "descriptor"/"info"/"status" variants are explicit **DTO projections**
for the FFI/JS edge only, never independent types.

### 2.1 Frame payload & identifiers (home: `ras-media`)

```rust
use bytes::Bytes;
use ras_protocol::ErrorCode;

/// Monotonic per-stream frame id. Never wraps in a session. Gap ⇒ loss.
/// Crosses to JS as a BigInt (DataView.getBigUint64) — NEVER as a JS `number`
/// (would silently corrupt past 2^53 and trigger spurious IDR storms).
pub type FrameId = u64;

/// Capture time in microseconds on the host **monotonic** clock, sampled at capture.
/// NOT wall-clock; used only for pacing/ordering/jitter — never for authorization.
/// Because B-frames are off, capture order == decode order == presentation order, so this
/// single stamp is also the WebCodecs `EncodedVideoChunk.timestamp` (mapped verbatim).
pub type CaptureTimestampUs = u64;

/// THE ONE encoded access unit. Defined once here; `ras-transport-iroh` and the controller
/// import it and never re-declare it. `data` is `bytes::Bytes` (pooled, O(1) clone/slice,
/// refcounted) — this IS the allocation-light guarantee; there is no separate `FrameBuf`.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Monotonic id; gap = loss (per-stream). Reassembly key + staleness clock.
    pub frame_id: FrameId,
    /// Host monotonic capture time. (Field name is `captured_at_us` everywhere —
    /// not `timestamp_us`/`pts_micros`.)
    pub captured_at_us: CaptureTimestampUs,
    /// True IDR. The controller MUST (re)start decoding on one of these. Intra-refresh /
    /// recovery-point SEI frames are NOT keyframes and this is false for them.
    pub is_keyframe: bool,
    /// Complete Annex-B access unit: start-code (0x000001) delimited NALs. Keyframes carry
    /// SPS+PPS in-band and re-send them every IDR, so a fresh `VideoDecoder` recovers from
    /// any keyframe with no out-of-band `description`. There is deliberately NO avcC path.
    pub data: Bytes,
    /// Config snapshot this frame was encoded under. Carried per-frame because the video path
    /// is droppable/out-of-order — there is no reliable "config changed" message it can depend
    /// on. `Copy`, ~small, so a mid-stream resolution change reacts atomically with its IDR.
    pub config: StreamConfig,
}
```

### 2.2 Stream parameters (home: `ras-media`)

```rust
/// THE ONE stream descriptor. `ras-core::StreamDescriptor` and
/// `controller::VideoStreamInfo` are labeled DTO projections of this for the FFI/JS edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Codec as an ENUM, not a String. The fully-qualified WebCodecs string
    /// ("avc1.4D401F") is DERIVED at the Tauri boundary (see `VideoCodec::webcodecs_string`),
    /// never stored in three crates.
    pub codec: VideoCodec,
    /// Encoded/output dimensions (post-scale, portrait de-rotated).
    pub width: u32,
    pub height: u32,
    /// Target frames/sec (capture may run higher; the pacer decides emit rate).
    pub fps: u32,
    /// Target average bitrate (bits/sec), CBR. Driven by the ABR hook (§3.6).
    pub target_bitrate_bps: u32,
    /// Color space the decoder must assume. ONE enum name `ColorSpace` (no `ColorSpaceHint`).
    pub color: ColorSpace,
    /// Which concrete video transport this session negotiated, so the receiver reassembles
    /// the matching way. Folded onto the config (transport needs it alongside frames).
    pub video_transport: VideoTransportKind,
}

/// H.264 profile we emit. B-frames OFF in every variant (no reorder latency).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoCodec {
    /// H.264 Annex-B, Main profile, CABAC, no B-frames. Default.
    H264AnnexB,
}

impl VideoCodec {
    /// Derive the fully-qualified WebCodecs codec string (e.g. "avc1.4D401F") from the codec
    /// plus dimensions/level. This projection lives ONLY at the Tauri/JS boundary.
    #[must_use]
    pub fn webcodecs_string(self, width: u32, height: u32) -> String { todo!() }
}

/// ONE color-space enum. Limited-range BT.709 is the desktop-encoder default; carried
/// explicitly to avoid crushed/washed colors. (No `Srgb` variant — spurious for desktop H.264.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorSpace {
    Bt709Limited,
    Bt709Full,
}

/// Which droppable video transport a session uses. Negotiated in `StreamConfig`. Both variants
/// are droppable and free of cross-frame head-of-line blocking. Concrete choice is post-spike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoTransportKind {
    /// One uni-directional QUIC stream per frame, reset()-able on staleness.
    PerFrameStream,
    /// App-level fragmentation over datagrams + per-frame Reed-Solomon FEC (block depth = 1 frame).
    DatagramFec,
}
```

### 2.3 Control-plane messages & decoder feedback (home: `ras-protocol`)

Wire-negotiated and protobuf-backed (`proto/casual_ras.proto`, ADR-009). `ras-transport-iroh`
re-exports these; `ras-core` imports them. This is the single home — no duplicate `LossReport`,
no separate `IdrRequest`/`IdrRequestReason`.

```rust
/// Reliable control-channel message set (protobuf oneof once codegen lands).
/// Transport-scoped only — no grant/lease payloads live here (those ride as opaque bytes,
/// see §5.5). Content-free feedback only (counters/timing, never pixels).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ControlMsg {
    /// Session-open handshake: agreed protocol version + feature flags.
    Hello { protocol_version: u32 },
    /// Host → controller: active stream parameters. Reliable so the decoder is always
    /// configured before frames arrive. (Wire projection of `ras_media::StreamConfig`.)
    StreamConfig(StreamConfigWire),
    /// Controller → host: request a fresh IDR (PLI-style). Canonical keyframe request.
    KeyframeRequest(KeyframeRequest),
    /// Controller → host: periodic content-free decoder feedback feeding ABR + resync.
    Feedback(DecoderFeedback),
    /// Phase-2 slot: opaque access-request / consent bytes, empty in Phase 1 (see §5.5).
    /// Present now so the data path to the auth seam exists; carries no meaning in Phase 1.
    AuthEnvelope { payload: Bytes },
    /// Graceful teardown with a stable reason code.
    Bye { code: ErrorCode },
}

/// Canonical keyframe/IDR request (controller → host). Merges the former IdrRequest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeRequest {
    /// Last frame_id the controller has, for host-side coalescing (avoid redundant IDRs).
    pub since_frame: FrameId,
    pub reason: KeyframeReason,
}

/// ONE keyframe-reason enum (superset of the former media/transport enums).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyframeReason {
    /// First frame of a new session / late-join subscriber.
    StreamStart,
    /// Gap in frame_ids beyond FEC recovery.
    UnrecoverableLoss,
    /// WebCodecs decoder went terminal; new decoder needs an IDR.
    DecoderReset,
    /// Resolution/codec/monitor change enacted this frame.
    ConfigChanged,
    /// Optional bounded host safety refresh.
    PeriodicRefresh,
}

/// ONE content-free feedback message (controller → host, reliable). Merges the former
/// media `DecoderFeedback` and transport `LossReport`. Carries only counters/timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderFeedback {
    /// Highest contiguous frame_id successfully decoded.
    pub last_decoded_frame: FrameId,
    /// Frames dropped since last report (metrics + ABR).
    pub frames_dropped: u32,
    /// Controller-measured decode/presentation latency estimate (µs), used as a trend only.
    pub decode_latency_us: u32,
    /// Folds the former idr_request: present when the decoder needs a fresh IDR.
    pub keyframe_request: Option<KeyframeRequest>,
}

/// Wire projection of `ras_media::StreamConfig` for the control channel (protobuf-encoded).
/// Kept structurally identical; separate name only because the codec is serialized as its
/// derived string form on the wire while the in-memory type stays an enum.
#[derive(Debug, Clone)]
pub struct StreamConfigWire { /* codec: String, width, height, fps, bitrate, color, transport */ }
```

### 2.4 Connection health (home: `ras-transport-iroh`, re-exported)

```rust
/// THE ONE connection-health snapshot. Sourced from iroh/Quinn Connection::stats()/rtt() +
/// path events. Consumed by (a) the host ABR loop and (b) the controller status badge —
/// both as projections, never as re-declared structs. Read lock-free via a `watch`; a
/// stalled video path can never block a health read.
///
/// UNIT DISCIPLINE (was a real cross-crate bug): rtt is MICROSECONDS as u32; loss is a
/// FRACTION [0,1] as f32. Everyone converts to ms/permil/pct for DISPLAY only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnHealth {
    /// Direct (hole-punched) vs relayed vs migrating. ~10% of sessions are relay-only.
    pub path: PathKind,
    /// Smoothed round-trip time, microseconds.
    pub rtt_us: u32,
    /// Estimated loss fraction over a recent window, 0.0..=1.0 (drives FEC strength + ABR).
    pub loss_fraction: f32,
    /// Congestion-window-derived deliverable rate, bits/sec. The ABR bitrate CEILING —
    /// the encoder MUST NOT outrun it.
    pub estimated_bandwidth_bps: u32,
    /// Frames dropped at the sink since last snapshot (sender-side pressure signal).
    pub frames_dropped: u32,
    /// Link lifecycle, incl. the Rust-side watchdog `Stalled` (no frame for N ms).
    pub state: LinkState,
}

/// 3 variants. Any controller match MUST handle `Migrating` (map to Relayed for the badge if
/// desired) — a 2-variant match won't compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind { Direct, Relayed, Migrating }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState { Connecting, Live, Stalled, Reconnecting, Closed }
```

### 2.5 Error type (via `ras_protocol::ErrorCode`)

One shared taxonomy; recoverability is load-bearing (it drives the capture-rebuild loop and the
reconnect window), so it is uniform across all crates. Detail is `&'static str` (no secrets — every
field is content-free, honoring the "secrets never touch logs" invariant).

```rust
// ras-protocol — one canonical error struct; all crates alias it so `?` needs no From impls.
#[derive(Debug, Clone)]
pub struct RasError {
    /// Stable machine code from the shared taxonomy (never a parallel enum).
    pub code: ErrorCode,
    /// True ⇒ rebuild-and-continue (SCK restart / DXGI ACCESS_LOST / reconnect window).
    /// False ⇒ fatal stop. Derived consistently, never contradicting `code`.
    pub recoverable: bool,
    /// Operator-facing, content-free. NEVER pixels/paths/tokens/typed-text.
    pub context: &'static str,
}

// Each crate aliases the shared type (keeps names local, avoids conversion boilerplate):
//   ras-media:            pub type MediaError     = ras_protocol::RasError;
//   ras-transport-iroh:   pub type TransportError = ras_protocol::RasError;
//   ras-core:             pub type CoreError      = ras_protocol::RasError;
//   controller (src-tauri): pub type SessionError = ras_protocol::RasError;
```

### 2.6 Identity / dial aliases (home: `ras-transport-iroh`, re-exported)

```rust
/// Ed25519 public key of a peer. Thin newtype over `iroh::EndpointId` (1.x rename of NodeId)
/// so downstream crates never depend on `iroh` directly. This IS identity — authenticates
/// *who*, never *what they may do* (Invariant 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EndpointId(/* iroh::EndpointId */ [u8; 32]);

/// Dialable address: EndpointId + optional relay/direct hints. Newtype over `iroh::EndpointAddr`.
#[derive(Debug, Clone)]
pub struct EndpointAddr { /* iroh::EndpointAddr */ }

// core/controller ALIAS these rather than defining PeerIdentity/DialTarget/HostTarget:
//   pub type PeerIdentity = ras_transport_iroh::EndpointId;
//   pub type DialTarget   = ras_transport_iroh::EndpointAddr;   // (controller: HostTarget = DialTarget)
```

---

## 3. `ras-media` interfaces

Depends on: `ras-protocol` (ErrorCode/RasError, ControlMsg family), `bytes`. **No iroh, no tauri,
no tokio, no `async`** in the frame-producing traits — async lives only at the transport edge that
`ras-core` owns. Capture+encode run pinned on one dedicated high-priority thread with no executor hops.

### 3.1 Capture backend

```rust
/// Zero-copy handle to one captured frame, still GPU-resident. macOS: retained
/// IOSurface-backed CVPixelBuffer; Windows: D3D11 texture. NEVER copies pixels to the CPU.
/// Thread-affine: the surface is only valid on the capture/encode thread and must be released
/// promptly (tiny pool) — expressed by having the encoder consume it BY VALUE within one call.
pub trait CapturedFrame {
    fn captured_at_us(&self) -> CaptureTimestampUs;
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// Opaque platform surface for the paired encoder on the SAME platform. Never crosses a
    /// crate boundary as a raw pointer to another crate.
    fn platform_surface(&self) -> PlatformSurface<'_>;
}

/// Frame source. PULL-based, SYNCHRONOUS, BLOCKING-WITH-TIMEOUT — not async, not callback.
/// Pull lets the pacer DROP frames it can't keep up with (video is droppable, by design).
pub trait ScreenCaptureBackend: Send {
    type Frame<'a>: CapturedFrame where Self: 'a;

    /// Select the single monitor + capture rate; returns the negotiated `StreamConfig`.
    fn start(&mut self, opts: &CaptureOptions) -> Result<StreamConfig, MediaError>;

    /// Block until the next frame or `timeout`.
    ///   Ok(Some) — fresh frame
    ///   Ok(None) — timed out, static screen (pacer decides; see §1.2 keep-alive: NO video re-emit)
    ///   Err(recoverable) — SCK restart / DXGI ACCESS_LOST → caller rebuilds via start()
    ///   Err(fatal)       — unrecoverable
    fn next_frame(&mut self, timeout: core::time::Duration)
        -> Result<Option<Self::Frame<'_>>, MediaError>;

    fn config(&self) -> StreamConfig;
    fn stop(&mut self);
}

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// Single monitor in Phase 1; explicit so multi-monitor is additive, not a redesign.
    pub monitor: MonitorId,
    pub target_fps: u32,
    /// Exclude our own overlay/consent windows (privacy + no capture feedback loop).
    pub excluded_window_ids: Vec<WindowId>,
}
```

### 3.2 Encoder backend

```rust
/// Hardware-preferred H.264 encoder. Zero-copy surface in, Annex-B out. Synchronous
/// single-frame call on the encode thread.
pub trait VideoEncoderBackend: Send {
    /// Build/configure. Applies invariant knobs: RealTime, B-frames OFF (no reordering),
    /// CBR, infinite GOP + forced-IDR-on-demand.
    fn configure(&mut self, config: &StreamConfig) -> Result<(), MediaError>;

    /// Encode exactly one captured frame. Consumes the surface BY VALUE so capture recycles
    /// its pool slot immediately. Any produced frame is a COMPLETE access unit.
    fn encode<F: CapturedFrame>(&mut self, frame: F) -> Result<Option<EncodedFrame>, MediaError>;

    /// Request the NEXT frame be a true IDR. Idempotent within an interval; coalesces.
    /// SOLE keyframe mechanism — no periodic keyframes (infinite GOP).
    fn request_keyframe(&mut self, reason: KeyframeReason);

    /// Retarget CBR bitrate mid-stream WITHOUT reconfigure/keyframe. Driven by ABR every RTT.
    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError>;

    fn config(&self) -> StreamConfig;
}
```

### 3.3 macOS backend shape (ScreenCaptureKit → VideoToolbox)

```rust
/// macOS capture. SCStreamOutput delegate `didOutputSampleBuffer` runs on a private queue;
/// we retain each CVPixelBuffer (IOSurface-backed) into a small SPSC ring, and `next_frame()`
/// pops it — adapting SCK's push-delegate to our pull trait while staying zero-copy.
///   start():      SCStreamConfiguration (one SCDisplay, pixelFormat 420v/BGRA,
///                 minimumFrameInterval = target_fps, small queueDepth, showsCursor per policy)
///   next_frame(): pop ring or time out; map SCK errors to Recoverable/Fatal
pub struct ScreenCaptureKitBackend { /* SCStream, ring, config */ }

/// macOS encode. VTCompressionSession, kCMVideoCodecType_H264. Feeds the CVPixelBuffer straight
/// in (zero-copy IOSurface import). configure() sets:
///   RealTime = true
///   AllowFrameReordering = false            // B-frames OFF → no reorder latency
///   ProfileLevel = H264_Main_AutoLevel
///   AverageBitRate + DataRateLimits         // CBR-ish
///   MaxKeyFrameInterval = very large, MaxKeyFrameIntervalDuration = 0   // infinite GOP, IDR on demand
/// encode(): VTCompressionSessionEncodeFrame; the callback delivers a length-prefixed (avcC-style)
///   CMBlockBuffer → we CONVERT to Annex-B (replace 4-byte NAL length prefixes with 0x00000001)
///   and PREPEND SPS+PPS on keyframes from CMVideoFormatDescription (every IDR self-contained).
/// request_keyframe(): kVTEncodeFrameOptionKey_ForceKeyFrame on the next encode call.
/// set_bitrate():      update kVTCompressionPropertyKey_AverageBitRate live (no keyframe).
pub struct VideoToolboxEncoder { /* VTCompressionSessionRef, sps/pps cache */ }

impl ScreenCaptureBackend for ScreenCaptureKitBackend { /* ... todo!() ... */ }
impl VideoEncoderBackend  for VideoToolboxEncoder     { /* ... todo!() ... */ }
```

> **Windows port (later, identical traits):** `DxgiDuplicationBackend: ScreenCaptureBackend`
> (D3D11 texture surfaces, ACCESS_LOST → recoverable) + `MediaFoundationEncoder: VideoEncoderBackend`
> (D3D11 texture-in, `MF_LOW_LATENCY`, `CODECAPI_AVEncVideoForceKeyFrame`, Annex-B conversion). No
> trait changes.

### 3.4 Synthetic backend (tests, no GPU / no OS permissions)

```rust
/// Deterministic, dependency-free capture+encode for CI. Generates a synthetic pattern
/// (moving bar + frame-id watermark) and a structurally valid Annex-B stream: real start codes,
/// stub SPS/PPS on keyframes, honored length/keyframe-flag — so transport framing, loss handling,
/// reorder-by-id, and keyframe-request plumbing test end-to-end WITHOUT a real decoder. A feature
/// flag can swap in OpenH264 (libloading) for genuinely decodable frames (never x264/GPL).
pub struct SyntheticCaptureBackend { /* size, fps, counter, forced-idr flag */ }
pub struct SyntheticEncoder        { /* honors request_keyframe/set_bitrate, emits EncodedFrame */ }
impl ScreenCaptureBackend for SyntheticCaptureBackend { /* ... */ }
impl VideoEncoderBackend  for SyntheticEncoder        { /* ... */ }
```

### 3.5 Controller-side decode (native-fallback trait only)

The Phase-1 render path is WebCodecs in the webview (§6); Rust just forwards `EncodedFrame.data`.
This trait exists only for the **native-surface fallback** so it is the same interface, not a rewrite.

```rust
/// Native decode fallback (VideoToolbox/MF in Rust). The WebCodecs path does NOT implement this
/// (it lives in JS). First frame after configure()/reset() MUST be a keyframe (caller drops until
/// `is_keyframe`). poll_decoded pull is non-blocking; caller present-and-drops promptly (native
/// mirror of VideoFrame.close() discipline — tiny pool).
pub trait DecoderBackend: Send {
    fn configure(&mut self, config: &StreamConfig) -> Result<(), MediaError>;
    fn decode(&mut self, frame: &EncodedFrame) -> Result<(), MediaError>;
    fn poll_decoded(&mut self) -> Option<DecodedFrame>;
    /// A decode error is terminal; recovery = reset() + next frame MUST be IDR.
    fn reset(&mut self);
}
```

### 3.6 Adaptive bitrate

```rust
/// ABR hook. `ras-core` drives it each feedback/stats tick. Pure function of inputs → intents;
/// unit-testable with the synthetic backend. Latency-first: caps bitrate to bandwidth, reacts
/// every RTT via set_bitrate (keyframe-free), reserves IDR for genuine resync. Consumes the
/// canonical `ConnHealth` directly (no separate TransportStats type).
pub trait AdaptiveBitrateController: Send {
    fn on_tick(&mut self, health: &ConnHealth, feedback: Option<DecoderFeedback>) -> BitrateDecision;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateDecision {
    pub target_bitrate_bps: u32,
    /// IDR-on-loss causes a bitrate spike, so this is the LAST-RESORT resync signal — FEC in
    /// transport is the preferred loss response.
    pub force_keyframe: Option<KeyframeReason>,
}
```

---

## 4. `ras-transport-iroh` interfaces

Depends on: `ras-protocol` (ErrorCode, ControlMsg family), `ras-media` (`EncodedFrame`,
`StreamConfig` — producer-owns), `iroh` 1.x. Exposes **no** grant/capability/lease type — only a
transport-authenticated `EndpointId` + `Alpn` (Invariant 9). Iroh 1.x vocabulary: `EndpointId`/
`EndpointAddr`; streams accept only after first byte.

### 4.1 Endpoint & versioned ALPN

```rust
/// Versioned ALPN families. Major version folded into the string so a peer on a different major
/// is rejected at accept() by ALPN mismatch, before any bytes are trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alpn {
    /// `casual-ras/bootstrap/{PROTOCOL_VERSION}` — Phase-2 pairing/identity. Stub handler in P1.
    Bootstrap,
    /// `casual-ras/session/{PROTOCOL_VERSION}` — a session. Phase 1 dials this directly, no grant.
    Session,
}
impl Alpn {
    #[must_use] pub fn wire(self) -> Vec<u8> { todo!() }          // e.g. b"casual-ras/session/1"
    #[must_use] pub fn accepted() -> &'static [Alpn] { todo!() }  // for Endpoint::bind; unknown rejected
}

#[derive(Debug, Clone)]
pub enum RelayPolicy { Default /*dev/CI*/, Custom(/*RelayMap*/ ()) /*prod*/, Disabled /*LAN spike*/ }

#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub relay: RelayPolicy,
    pub alpns: Vec<Alpn>,
    // Phase-2 slot: identity key material injected here (P1 uses an ephemeral iroh keypair).
}

/// The single per-process Iroh endpoint (one per host, one per controller).
pub struct Endpoint { /* iroh::Endpoint */ }

impl Endpoint {
    pub async fn bind(cfg: EndpointConfig) -> Result<Self, TransportError> { todo!() }
    #[must_use] pub fn id(&self) -> EndpointId { todo!() }     // iroh 1.x Endpoint::id()
    #[must_use] pub fn addr(&self) -> EndpointAddr { todo!() }

    /// CONTROLLER side. Dial + drive handshake to a `Session`. Sends the first byte itself so
    /// the peer's accept() fires (accept-after-first-byte gotcha).
    /// Err: TransportError (unreachable/timeout) | IdentityMismatch (if expected_remote set).
    pub async fn connect(&self, addr: EndpointAddr, cfg: SessionConfig)
        -> Result<Session, TransportError> { todo!() }

    /// HOST side. Yield the next inbound connection AFTER ALPN negotiation + first-byte handshake.
    pub async fn accept(&self) -> Result<Incoming, TransportError> { todo!() }

    pub async fn close(self) { todo!() }
}

/// Inbound connection whose identity+ALPN are known but channels are not yet opened. The single
/// hook point where Phase-2 identity-pinning runs BEFORE channel setup. In P1, accept() is
/// called unconditionally.
pub struct Incoming { /* iroh::Connecting + negotiated ALPN */ }
impl Incoming {
    #[must_use] pub fn alpn(&self) -> Alpn { todo!() }
    /// Peer's verified identity (transport-authenticated). IDENTITY ONLY (Invariant 9).
    #[must_use] pub fn remote(&self) -> EndpointId { todo!() }
    /// Accept + establish the Session. The `expected_remote` pin (if set) is enforced HERE,
    /// inside accept() — so if iroh 1.x only yields the verified EndpointId later, the check
    /// moves internally with NO API change. [verify against pinned iroh — Phase-S]
    pub async fn accept(self, cfg: SessionConfig) -> Result<Session, TransportError> { todo!() }
    pub async fn reject(self, code: ErrorCode) { todo!() }
}

#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// Phase-2 identity-pinning slot: if Some, reject a peer whose EndpointId differs
    /// (IdentityMismatch). P1 = None. Pins IDENTITY, not authorization — still no grant logic here.
    pub expected_remote: Option<EndpointId>,
    /// Safe app-level datagram payload (bytes), default ~1200 (path-MTU-safe).
    pub datagram_payload: u16,
}
```

### 4.2 Session & the reliability-split channel map

```rust
/// An established, identity-authenticated session over one iroh Connection. Owns the
/// reliability-split channel map: a stalled VideoSource can NEVER block the ControlChannel or
/// PointerChannel (the load-bearing latency invariant).
pub struct Session { /* iroh::Connection + spawned channel tasks */ }

impl Session {
    #[must_use] pub fn remote(&self) -> EndpointId { todo!() }        // identity only
    #[must_use] pub fn control(&self) -> ControlChannel { todo!() }   // reliable/ordered (§4.3)
    #[must_use] pub fn video_sink(&self) -> Option<VideoSink> { todo!() }     // host side (§4.4)
    #[must_use] pub fn video_source(&self) -> Option<VideoSource> { todo!() } // controller side (§4.4)
    #[must_use] pub fn health(&self) -> HealthObserver { todo!() }    // observable (§4.5)
    pub async fn close(self, code: ErrorCode) { todo!() }
    // NOTE: PointerChannel is DEFERRED out of Phase 1 (view-only). See §8 Q-P.
}
```

| Channel  | iroh primitive                        | Reliability                       | Framing                       |
|----------|---------------------------------------|-----------------------------------|-------------------------------|
| Control  | bidi stream (`open_bi`)               | reliable, ordered                 | `u32-BE length \| protobuf`   |
| Video    | per-frame uni stream **or** datagram+FEC (swappable) | **droppable**, no cross-frame HOL | app-level fragment header      |

**Framing rule (stated once):** reliable streams = `u32-BE length | protobuf(ControlMsg)`; datagram
channels = their own fixed fragment header (§4.4). Never mixed.

### 4.3 Control channel

```rust
/// Reliable, ordered control channel over one bidi QUIC stream. Loss-intolerant → never datagrams.
#[derive(Clone)]
pub struct ControlChannel { /* Arc over send/recv halves + writer mutex */ }
impl ControlChannel {
    /// Frames as `u32-BE length | protobuf(ControlMsg)`.
    /// Err: TransportError (broken) | InvalidMessage (> MAX_CONTROL_FRAME).
    pub async fn send(&self, msg: ControlMsg) -> Result<(), TransportError> { todo!() }
    pub async fn recv(&self) -> Result<ControlMsg, TransportError> { todo!() }
}
/// DoS guard on hostile peer input. 1 MiB is ample for config/feedback.
pub const MAX_CONTROL_FRAME: usize = 1 << 20;
```

### 4.4 Droppable video: send / fragment / receive

```rust
/// HOST-side droppable video sender. Non-blocking by design: if the path can't keep up, frames
/// are DROPPED at the sink, never queued unbounded. Returns a SOURCE-SIDE outcome so the pacer
/// learns of drops immediately (not an RTT later via receiver feedback).
pub struct VideoSink { /* Box<dyn VideoTransport sender half> */ }
impl VideoSink {
    /// Fragment (if needed) and send one frame. Returns immediately; does NOT await delivery.
    /// If a prior frame is still in flight and stale, its per-frame stream is reset() / its FEC
    /// block abandoned before this one starts — no HOL. `Err` ONLY on fatal path error
    /// (connection gone); ordinary loss is a non-error `SendOutcome`.
    pub fn send_frame(&self, frame: EncodedFrame) -> Result<SendOutcome, TransportError> { todo!() }
}

/// Source-side send result → feeds the pacer's "drop-to-keyframe at the source" decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome { Sent, DroppedStale, DroppedCongested }

/// CONTROLLER-side droppable video receiver. Reassembles fragments/FEC into whole frames and
/// surfaces loss as a first-class, non-fatal event.
pub struct VideoSource { /* Box<dyn VideoTransport receiver half> */ }
impl VideoSource {
    /// Await the next event. Frames arrive with frame_id/captured_at_us intact; the DECODER owns
    /// reorder-by-frame_id (WebCodecs has no jitter buffer), not the transport.
    pub async fn recv(&self) -> Result<VideoEvent, TransportError> { todo!() }
}

#[derive(Debug)]
pub enum VideoEvent {
    /// A complete Annex-B access unit ready for the decoder.
    Frame(EncodedFrame),
    /// A frame was abandoned. `ras-core` turns a run of these into ONE KeyframeRequest rather
    /// than freezing — last-good frame stays on screen. Controller cursor/controls untouched.
    FrameDropped { frame_id: FrameId, reason: DropReason },
}
#[derive(Debug, Clone, Copy)]
pub enum DropReason { Stale, FecUnrecoverable, StreamReset, MissingFragments }

/// Swappable strategy. Both patterns implement this; the concrete one is chosen at session start
/// from measured path conditions / spike results and pinned into StreamConfig. THIS TRAIT IS THE
/// SEAM that lets the spike change the answer without changing any caller.
pub trait VideoTransport: Send + Sync {
    fn kind(&self) -> VideoTransportKind;
    fn send(&self, frame: &EncodedFrame) -> Result<SendOutcome, TransportError>;
    fn poll_recv(&self)
        -> impl core::future::Future<Output = Result<VideoEvent, TransportError>> + Send;
}

/// App-level fragment header prepended to every video datagram (DatagramFec). Fixed-size, packed,
/// allocation-light. QUIC gives per-datagram integrity; we add reassembly + FEC.
/// wire: [ frame_id:u64 | frag_index:u16 | frag_count:u16 | fec_k:u16 | fec_n:u16 | flags:u8 ]
#[derive(Debug, Clone, Copy)]
pub struct VideoFragHeader {
    pub frame_id: u64,     // reassembly key + staleness/ordering clock
    pub frag_index: u16,   // 0..frag_count (data) then FEC repair shards
    pub frag_count: u16,   // data fragments for this frame
    pub fec_k: u16,        // Reed-Solomon data shards
    pub fec_n: u16,        // total shards (n-k = repair); depth = 1 frame
    pub flags: u8,         // bit0 = keyframe
}
```

**How loss surfaces (mechanism):**
- *PerFrameStream:* a frame's uni-stream that doesn't complete before the next is deemed stale →
  sender `reset()`s it → receiver sees a truncated/reset stream → `FrameDropped { StreamReset | Stale }`.
  No retransmit, no HOL.
- *DatagramFec:* receiver collects fragments keyed by `frame_id`; once `k` of `n` arrive it
  RS-recovers and emits `Frame`. Fewer than `k` within a short deadline → `FrameDropped
  { FecUnrecoverable | MissingFragments }`. FEC not ARQ (ARQ costs a full RTT).
- Either way `ras-core` converts sustained drops into a single `KeyframeRequest`, not a freeze.

### 4.5 Connection-health observable

```rust
/// Read-only, lock-free observable (watch channel). Readers get the LATEST snapshot without
/// awaiting the network — a stalled video path never blocks a health read; ABR polls every tick.
#[derive(Clone)]
pub struct HealthObserver { /* tokio::sync::watch::Receiver<ConnHealth> */ }
impl HealthObserver {
    #[must_use] pub fn snapshot(&self) -> ConnHealth { todo!() }   // never blocks on network
    pub async fn changed(&mut self) -> ConnHealth { todo!() }      // UI reactivity, not hot path
}
```

---

## 5. `ras-core` interfaces

Depends on: `ras-protocol`, `ras-media`, `ras-transport-iroh` (concrete types + the DI seam). Stays
iroh-free by depending on `ras-transport-iroh`'s newtypes, not `iroh`.

### 5.1 Session state machine (subset of HLD §10, with security-terminal states kept)

The Phase-1 machine **elides the auth *transition* states** (`BootstrapConnected`,
`AccessRequested`, `AwaitingConsent`, `GrantIssued`) — a P1 session dials the session ALPN directly.
But it **keeps `Revoked`, `Rejected`, `Expired` as first-class states** (adversarial C2): they cost
nothing in P1, are reachable only via the no-op paths, and mean Phase 2 does **not** have to split a
collapsed `Terminated` (which would break every downstream `match` and every emitted
`LifecycleEvent`). Emergency-stop-as-`Revoked` (Invariant 4) is not deferrable even for view-only —
a revoke must be auditably distinct from a clean `PeerClosed`.

```rust
/// Phase-1 session lifecycle. Auth TRANSITION states are elided; security-TERMINAL states are
/// retained so Phase 2 is additive. No variant is renamed/removed later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionState {
    Created,
    /// Dialing/accepting the session ALPN; QUIC + channel setup in flight. No media.
    SessionConnecting,
    /// Control channel handshaked; the window where Phase-2 authorize() runs. No frames yet.
    /// (Distinct from SessionConnecting so the auth gate has a place to sit — adversarial L1.)
    ControlEstablished,
    /// Channels open, stream configured, frames may flow. Steady state. Reachable ONLY via the
    /// `Authorized` event (P1: emitted immediately by the no-op validator; see §5.5).
    Active,
    /// Transport temporarily lost within the reconnect window. Video frozen/blanked; controller
    /// UI stays live.
    Suspended,
    /// Terminal: clean end (local stop / peer close / window elapsed).
    Terminated,
    /// Terminal: emergency-stop / mid-session revoke (Invariant 4). Audit-distinct.
    Revoked,
    /// Terminal: authorization refused (Phase 2). Present now, reachable only via no-op path.
    Rejected,
    /// Terminal: grant/session expiry (Phase 2).
    Expired,
}

/// Internal transition inputs (Copy, content-free — runs on the hot control task, no heap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionEvent {
    Start,
    /// Session ALPN connection + control handshake done.
    ControlUp,
    /// Auth gate passed. In P1 the no-op validator emits this immediately after ControlUp.
    /// This is the ONLY edge into the pre-Active gate → Active is UNREACHABLE without it.
    Authorized,
    /// Codec/monitor/feature negotiation done.
    StreamConfigured,
    TransportLost,
    TransportRestored,
    LocalStop,
    PeerClosed,
    /// Emergency stop / host or peer revoke (Invariant 4).
    Revoke { code: ras_protocol::ErrorCode },
    /// Authorization refused (Phase 2).
    Reject { code: ras_protocol::ErrorCode },
    /// Grant/session expired (Phase 2).
    Expire { code: ras_protocol::ErrorCode },
    Fatal { code: ras_protocol::ErrorCode },
    ReconnectWindowExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition { To(SessionState), Invalid }

/// Pure, synchronous, side-effect-free. Deterministic + unit-testable. Orchestrators call this
/// and THEN perform effects — never the reverse. Never blocks on media: TransportLost moves
/// Active→Suspended immediately so the controller keeps its cursor/controls live.
#[must_use]
pub fn transition(state: SessionState, event: SessionEvent) -> Transition {
    use SessionEvent as E;
    use SessionState as S;
    let next = match (state, event) {
        (S::Created, E::Start)                       => S::SessionConnecting,
        (S::SessionConnecting, E::ControlUp)         => S::ControlEstablished,
        // Active is reachable ONLY through the Authorized gate (adversarial C3):
        (S::ControlEstablished, E::Authorized)       => S::ControlEstablished, // stay until configured
        (S::ControlEstablished, E::StreamConfigured) => S::Active,
        (S::Active, E::TransportLost)                => S::Suspended,
        (S::Suspended, E::TransportRestored)         => S::Active,
        (S::Suspended, E::ReconnectWindowExpired)    => S::Terminated,
        // Security-terminal edges from any non-terminal state:
        (s, E::Revoke { .. }) if !s.is_terminal()    => S::Revoked,
        (s, E::Reject { .. }) if !s.is_terminal()    => S::Rejected,
        (s, E::Expire { .. }) if !s.is_terminal()    => S::Expired,
        (s, E::LocalStop | E::PeerClosed | E::Fatal { .. }) if !s.is_terminal() => S::Terminated,
        _ => return Transition::Invalid,
    };
    Transition::To(next)
}
```

### 5.2 Host orchestrator

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HostSessionConfig {
    /// Single monitor in P1; explicit so multi-monitor is additive.
    pub monitor: MonitorId,
    /// Negotiated ceiling; encoder capped to the measured path at runtime.
    pub max_bitrate_bps: u32,
    /// Reconnect window before Suspended → Terminated.
    pub reconnect_window: core::time::Duration,
}

/// Host-side view-only session. Owns capture+encode+transmit tasks on their own thread(s).
pub struct HostSession { /* opaque */ }
impl HostSession {
    /// Build from injected backends. No I/O until start(). `grant_validator` is a no-op in P1
    /// (see §5.5) — the seam is present so Phase 2 adds consent without changing this signature.
    pub fn new(
        config: HostSessionConfig,
        transport: alloc_arc<dyn crate::deps::SessionTransport>,
        capture:   alloc_arc<dyn ras_media::ScreenCaptureBackend>,
        encoder:   alloc_arc<dyn ras_media::VideoEncoderBackend>,
        grant_validator: alloc_arc<dyn crate::deps::GrantValidator>,
    ) -> Self { todo!() }

    /// Listen for a controller on the session ALPN, negotiate, start pushing frames. Applies
    /// Start. Returns the lifecycle event stream. The spawned media task NEVER touches the state
    /// machine or the control channel — it only pushes droppable frames.
    pub async fn start(&self) -> Result<crate::event::LifecycleStream, CoreError> { todo!() }

    pub fn state(&self) -> SessionState { todo!() }

    /// Cooperative stop. Applies LocalStop, tears down, flushes SessionEnded. Idempotent. P1
    /// stand-in for the Phase-2 emergency-stop path. Signals-and-returns; does NOT drain video —
    /// meets the stop responsiveness target regardless of video state.
    pub async fn stop(&self, reason: StopReason) -> Result<(), CoreError> { todo!() }
}
```

### 5.3 Controller orchestrator

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ControllerSessionConfig {
    /// How to reach the host. P1: EndpointAddr (id + relay hints). Phase 2 replaces with a
    /// validated connection ticket — additive, not a rename.
    pub target: DialTarget,
    /// Local decode/render buffer target (~10–50 ms; WebCodecs has no jitter buffer).
    pub target_buffer: core::time::Duration,
}

/// Controller-side view-only session. Owns receive+reorder+decode-feed. Renderer attached
/// SEPARATELY so ingest runs before/independently of the renderer — a stalled or absent renderer
/// must never block frame ingest.
pub struct ControllerSession { /* opaque */ }
impl ControllerSession {
    pub fn new(
        config: ControllerSessionConfig,
        transport: alloc_arc<dyn crate::deps::SessionTransport>,
    ) -> Self { todo!() }

    /// Dial, handshake control, negotiate. Applies Start → ControlUp → Authorized(no-op) →
    /// StreamConfigured as they occur. Returns the event stream. Does NOT wait for a renderer.
    pub async fn connect(&self) -> Result<crate::event::LifecycleStream, CoreError> { todo!() }

    /// Attach/replace the frame sink. Decoupled from connect() so video can flow (and be dropped)
    /// before the UI canvas exists, and re-attach on reload never stalls ingest. The sink owns
    /// VideoFrame.close() discipline.
    pub async fn attach_renderer(&self, renderer: alloc_arc<dyn crate::deps::FrameSink>)
        -> Result<(), CoreError> { todo!() }

    /// Detach without ending the session. Ingest continues; frames drop at the sink boundary,
    /// never buffered unbounded.
    pub async fn detach_renderer(&self) -> Result<(), CoreError> { todo!() }

    pub fn state(&self) -> SessionState { todo!() }

    /// Ask the host for a fresh IDR (PLI-style). Reliable control channel; never blocks frames.
    pub async fn request_keyframe(&self, reason: KeyframeReason) -> Result<(), CoreError> { todo!() }

    /// Cooperative disconnect. Applies LocalStop, closes streams, emits SessionEnded. Returns
    /// promptly even mid-decode: signals tasks, does NOT await frame draining.
    pub async fn disconnect(&self, reason: StopReason) -> Result<(), CoreError> { todo!() }
}

/// Content-free stop reason (log/audit-safe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    UserRequested,          // Phase-2 emergency-stop reason lands here first
    PeerClosed,
    Timeout,                // reconnect window elapsed
    Error(ras_protocol::ErrorCode),
}
```

> `alloc_arc<dyn T>` denotes `std::sync::Arc<dyn T + Send + Sync>`, spelled out to keep the DI seam
> explicit and object-safe.

### 5.4 DI seams (backends + transport)

```rust
/// The session-level transport ras-core needs. Implemented by ras-transport-iroh (via an adapter
/// — see below) for real runs, and by a synthetic in-memory impl in tests. Reliability-split:
/// control is reliable/ordered; video is a separate droppable path.
#[async_trait::async_trait]
pub trait SessionTransport: Send + Sync {
    /// Establish the session (dial for controller, accept for host) on the session ALPN.
    /// Authenticates IDENTITY only — never authorization (Invariant 9).
    async fn establish(&self, target: &DialTarget) -> Result<PeerIdentity, CoreError>;
    /// Reliable, ordered control/lifecycle channel.
    async fn control_channel(&self) -> Result<Box<dyn ControlChannelDyn>, CoreError>;
    /// Droppable video egress (host). Sync, non-blocking send (see below).
    async fn video_sink(&self) -> Result<Box<dyn VideoSinkDyn>, CoreError>;
    /// Droppable video ingress (controller).
    async fn video_source(&self) -> Result<Box<dyn VideoSourceDyn>, CoreError>;
    /// Non-blocking health snapshot for ConnectionQuality events.
    fn health(&self) -> ras_transport_iroh::ConnHealth;
}

/// Reliable ordered control messages (cold path — async is fine).
#[async_trait::async_trait]
pub trait ControlChannelDyn: Send + Sync {
    async fn send(&mut self, msg: ras_protocol::ControlMsg) -> Result<(), CoreError>;
    async fn recv(&mut self) -> Result<ras_protocol::ControlMsg, CoreError>;
}

/// Droppable per-frame egress. SYNC + NON-BLOCKING (adversarial H2/H3): enqueues into a bounded
/// drop-oldest ring and returns immediately — NEVER awaits delivery (that would reintroduce HOL
/// on the video path from a slow sink). Owned `EncodedFrame`; `Bytes` gives allocation-light.
pub trait VideoSinkDyn: Send + Sync {
    fn send_frame(&self, frame: ras_media::EncodedFrame) -> ras_transport_iroh::SendOutcome;
}

/// Droppable per-frame ingress.
#[async_trait::async_trait]
pub trait VideoSourceDyn: Send + Sync {
    async fn next(&mut self) -> Result<ras_transport_iroh::VideoEvent, CoreError>;
}

/// Where frames go on the controller. Implemented by the Tauri layer (pushes to the WebCodecs
/// worker) and a counting sink in tests. SYNC + NON-BLOCKING push (adversarial H1/H2): the borrowed
/// FrameView is GONE — owned `EncodedFrame` (Bytes, O(1) clone) crosses the boundary. A slow sink
/// drops internally; it must NOT backpressure the transport source.
pub trait FrameSink: Send + Sync {
    /// Configure the render/decode pipeline. First chunk after this MUST be an IDR.
    fn configure(&self, config: &ras_media::StreamConfig) -> Result<(), CoreError>;
    /// Deliver one frame. Returns immediately with a Sent/Dropped status; never awaits.
    fn push(&self, frame: ras_media::EncodedFrame) -> PushResult;
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult { Sent, Dropped }

/// THE structural wiring item (seam-review item 9): the iroh Session must be adapted to the DI
/// traits above. Not a rename — a real bridge.
pub struct IrohSessionAdapter { /* ras_transport_iroh::Session + channel handles */ }
impl SessionTransport for IrohSessionAdapter { /* ... todo!() ... */ }
```

### 5.5 The Phase-2 auth seam (no-op in Phase 1)

This is the single seam where Phase 2 inserts signed access-request validation, local consent,
grant issuance, and endpoint binding. It is shaped (adversarial C1/C3) so that filling it in is
**additive**, not a breaking change:

1. **Data path exists now.** The control channel exchanges `ControlMsg::AuthEnvelope { payload }`
   (empty `Bytes` in P1) during `ControlEstablished`. Its bytes are threaded into
   `SessionAuthContext.access_request` so Phase 2's access-request + nonce + capabilities have a
   route to the validator — the review's "no bytes reach authorize()" gap is closed today.
2. **Consent is modeled as potentially multi-step.** Local consent (Invariant 1) is inherently
   interactive; a one-shot verdict cannot express "prompt the local user and wait." `authorize()`
   may return `NeedConsent`/`Challenge` for a round-trip.
3. **`Active` is gated.** The transition function reaches `Active` only via the `Authorized`
   `SessionEvent`, which the orchestrator emits **only** after `authorize()` yields `Authorized`.
   In P1 the no-op validator yields `Authorized` immediately — but the *edge* exists, so Phase 2 is
   wiring a required input, not adding a branch.
4. **The no-op impl cannot ship in an auth build.** `AllowAllValidator` is gated behind
   `#[cfg(feature = "insecure-no-auth")]`, mutually exclusive at the crate level with any auth
   feature, so a Phase-2 build physically cannot compile it in.

```rust
/// The consent/authorization hook. NO-OP in Phase 1. Invoked by the host orchestrator AFTER
/// transport identity is established (ControlEstablished) but BEFORE Active. Multi-step so it can
/// express interactive local consent.
#[async_trait::async_trait]
pub trait GrantValidator: Send + Sync {
    /// Called once (or iteratively, via Challenge) per session before it may become Active.
    /// Phase 2 drives the AwaitingConsent → GrantIssued transitions elided today.
    async fn authorize(&self, ctx: &SessionAuthContext) -> Result<GrantDecision, CoreError>;
}

/// Content-free context. P1 carries the transport-authenticated identity plus the (empty in P1)
/// opaque access-request bytes. `#[non_exhaustive]` — Phase 2 adds capabilities/nonce additively.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionAuthContext {
    /// Identity iroh authenticated (EndpointId). NOT authorization.
    pub peer_identity: PeerIdentity,
    /// Opaque access-request payload from ControlMsg::AuthEnvelope. Empty in Phase 1.
    pub access_request: bytes::Bytes,
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GrantDecision {
    /// Proceed → orchestrator emits SessionEvent::Authorized. P1 view-only ⇒ empty capability set.
    Authorized,
    /// Interactive consent pending (Phase 2): the host is prompting the local user. The
    /// orchestrator holds in ControlEstablished (no Active) until re-driven.
    NeedConsent,
    /// Multi-step challenge/response (Phase 2 replay/nonce).
    Challenge(bytes::Bytes),
    /// Refused → SessionEvent::Reject{code}. Code from the shared taxonomy
    /// (ConsentDenied / CapabilityDenied / GrantInvalid / ReplayDetected / …).
    Denied(ras_protocol::ErrorCode),
}

/// PHASE-1 ONLY. Returns Authorized unconditionally. Gated so it can never link into an auth build.
#[cfg(feature = "insecure-no-auth")]
pub struct AllowAllValidator;
#[cfg(feature = "insecure-no-auth")]
#[async_trait::async_trait]
impl GrantValidator for AllowAllValidator {
    async fn authorize(&self, _ctx: &SessionAuthContext) -> Result<GrantDecision, CoreError> {
        Ok(GrantDecision::Authorized)
    }
}
```

### 5.6 Event model (typed lifecycle → embedding app)

Maps to the docs/05 §4 SDK event strings. Events are **content-free** (no pixels/titles; only
enums/numbers). Emitted over a bounded, latest-wins-ish stream so a slow lifecycle consumer can
never backpressure the session tasks.

```rust
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum LifecycleEvent {
    /// docs/05 `connecting`. State: SessionConnecting.
    Connecting,
    /// docs/05 controller `session-ready` / host `session-started`. Control channel up.
    SessionReady { session_id: SessionId },
    /// docs/05 `stream-configured`. Carries the DTO the renderer needs to configure the decoder
    /// (Annex-B ⇒ no `description`). State: Active.
    StreamConfigured { descriptor: StreamDescriptor },
    /// docs/05 `quality-changed`. Advisory/UI only (Direct/Relayed badge, RTT). Never blocks.
    ConnectionQuality { sample: QualitySample },
    /// docs/05 `session-suspended`/`disconnected`. Transport lost within the reconnect window.
    /// Controller freezes/blanks video but keeps cursor + controls live. State: Suspended.
    Suspended { since_ms: u64 },
    Resumed,
    /// docs/05 `disconnected`. Transport gone (window not necessarily elapsed). Distinct from
    /// SessionEnded.
    Disconnected { code: ras_protocol::ErrorCode },
    /// docs/05 `session-ended`. Terminal. `reason` from the shared taxonomy. Object inert after.
    SessionEnded { reason: StopReason },
    /// Emergency-stop / revoke surfaced distinctly for audit (maps to SessionState::Revoked).
    Revoked { code: ras_protocol::ErrorCode },
}

/// DTO projection of `ras_media::StreamConfig` for the FFI/JS edge (NOT an independent type).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct StreamDescriptor {
    /// Fully-qualified WebCodecs string derived at this boundary (e.g. "avc1.4D401F").
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub color_space: ras_media::ColorSpace,
    // Annex-B ⇒ the decoder omits `description`; that is implied by the codec family, not a field.
}

/// DTO projection of `ras_transport_iroh::ConnHealth` for UI. Numbers only (log-safe).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct QualitySample {
    pub path: ras_transport_iroh::PathKind,  // MUST handle Migrating
    pub rtt_ms: u32,                          // display projection of rtt_us
    pub loss_pct: f32,                        // display projection of loss_fraction
    pub delivered_fps: u16,
}

/// Concrete choice deferred (§8 Q-STREAM). A bounded mpsc::Receiver or an impl Stream.
pub type LifecycleStream = /* impl Stream<Item = LifecycleEvent> + Send */ ();
```

---

## 6. Controller (Tauri v2)

Layering: `src-tauri/` (Rust: owns the iroh `Endpoint` + `ras-core::ControllerSession`; the webview
never touches iroh) and `ui/` (React webview authored as the future SDK surface). All networking is
in Rust. The frame path and the UI thread are mechanically decoupled so a stalled/lost video can
never freeze the local cursor, toolbar, or Stop.

### 6.1 Rust-side Channel(Raw) frame wiring

**One binary blob per frame** = a fixed 24-byte little-endian header + the Annex-B access unit. A
JSON sidecar would reorder relative to the binary and would corrupt `u64`s past 2^53 — so identifiers
travel **only** in the binary header and are read in JS as `BigInt`.

```
Frame Channel blob layout (little-endian):
  offset 0  : u32  magic          (framing validation)
  offset 4  : u8   flags          (bit0 = keyframe)
  offset 5  : u8   reserved
  offset 6  : u16  reserved
  offset 8  : u64  frame_id
  offset 16 : u64  captured_at_us
  offset 24 : ...  Annex-B access unit bytes
Total header = 24 bytes. (The layout constant is SHARED between frames.rs and decoder.worker.ts.)
```

```rust
/// The header struct; its byte layout is the shared contract with the TS parseHeader.
#[repr(C)]
pub struct FrameHeader {
    pub magic: u32,
    pub flags: u8,          // bit0 = keyframe
    pub _pad0: u8,
    pub _pad1: u16,
    pub frame_id: u64,      // read as BigInt in JS
    pub captured_at_us: u64,
}
pub const FRAME_HEADER_LEN: usize = 24;

/// Command the webview calls ONCE to open the frame stream. Returns immediately; frames flow
/// asynchronously on `channel`. Deny-by-default capability gates this to the session window only.
///
/// ALLOCATION-LIGHT (adversarial H1 — no per-frame Vec+memcpy): the header is written into 24
/// bytes of headroom reserved in the encoder's pooled BytesMut, so the whole frame ships as ONE
/// `Bytes` with zero copy. If `InvokeResponseBody::Raw` requires a contiguous owned buffer, the
/// pump reuses `Vec`s from a small free-list rather than allocating per frame.
#[tauri::command]
async fn open_frame_stream(
    state: tauri::State<'_, AppState>,
    channel: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>,
) -> Result<StreamDescriptorDto, ErrDto> {
    // Wire the ras-core FrameSink (a WebviewChannelSurface, §6.4) to this channel.
    // The FrameSink::push is SYNC + non-blocking; on Channel-send failure (webview gone) the
    // pump ends. NEVER JSON-encodes bytes (a plain Vec<u8> becomes a giant number-array).
    todo!()
}
```

**Backpressure — self-imposed, because a Tauri Channel has none:**
1. **Rust ingest:** `ras-core`'s inbound frame ring is a **bounded** drop-oldest ring (depth ~2–3).
   On full it drops the oldest non-keyframe and sets a `needs_keyframe` flag rather than blocking —
   allocation-light, memory-bounded. The transport is already per-frame-droppable, so this is the
   same policy one hop later.
2. **Worker egress:** the decoder input queue is watched via `decodeQueueSize`; when behind, discard
   non-keyframes until the next keyframe.

A late frame is worthless; we never add a jitter buffer here (that would trade latency for UX, which
the priority order forbids at this layer).

### 6.2 Command surface & Channel surface

| Command             | Rust signature                                                                 | Notes |
|---------------------|--------------------------------------------------------------------------------|-------|
| `connect_host`      | `async fn(state, target: HostTargetDto) -> Result<StreamDescriptorDto, ErrDto>` | dials + negotiates; Phase 2 takes an auth/grant arg here |
| `open_frame_stream` | `async fn(state, channel: Channel<InvokeResponseBody>) -> Result<StreamDescriptorDto, ErrDto>` | §6.1; the one binary pump |
| `open_status_stream`| `async fn(state, channel: Channel<ConnStatusDto>) -> Result<(), ErrDto>`         | pushes ConnHealth projections (§6.5) |
| `request_keyframe`  | `async fn(state) -> Result<(), ErrDto>`                                          | worker → main → here on decoder reset |
| `stop_session`      | `async fn(state) -> Result<(), ErrDto>`                                          | must return promptly even if the frame path is wedged (Invariant 4) |

`ErrDto` carries `ras_protocol::ErrorCode::as_str()` — the UI branches on stable codes, never on
strings. **Frames use a `Channel(Raw)`, never the Tauri event system** (JSON number-arrays, can
reorder, not throughput-safe). Events are only for coarse low-rate lifecycle notices.

### 6.3 TS worker / OffscreenCanvas / WebCodecs

```ts
// ui/src/renderer/RemoteSurface.ts — MAIN thread; does almost nothing (relay only).
export interface RemoteSurface {
  /** Create the Channel, create the Worker, transfer the OffscreenCanvas. Call once per session. */
  start(canvas: HTMLCanvasElement, info: VideoStreamInfo): Promise<void>;
  /** Idempotent; safe from the Stop handler even mid-stall. */
  stop(): void;
}

// ui/src/worker/decoder.worker.ts — ALL decode + present here, OFF the UI thread.
interface DecoderWorkerInit { canvas: OffscreenCanvas; info: VideoStreamInfo; }
// messages IN : {type:"init",...} | {type:"frame", buf:ArrayBuffer} | {type:"stop"}
// messages OUT: {type:"needKeyframe"} | {type:"stats",decodeMs,dropped} | {type:"fatal",code}

// --- header parse (24 bytes, BigInt for u64 — never Number) ---
function parseHeader(buf: ArrayBuffer) {
  const dv = new DataView(buf);
  return {
    keyframe: (dv.getUint8(4) & 1) === 1,
    frameId:  dv.getBigUint64(8, true),          // BigInt: gap/equality only, never arithmetic→Number
    captured: dv.getBigUint64(16, true),
  };
}

// --- decoder config, built ONCE from VideoStreamInfo ---
const config: VideoDecoderConfig = {
  codec: info.codec,                         // fully-qualified, e.g. "avc1.4D401F"
  // NO `description` field ⇒ selects Annex-B. Its presence selects avcC and the output callback
  // silently never fires (the #1 WebCodecs trap).
  optimizeForLatency: true,
  hardwareAcceleration: "prefer-hardware",   // with a prefer-software retry on configure failure
  // colorSpace from info (BT.709 limited-range default; wrong range = washed colors)
};

// --- input side: drop-to-keyframe backpressure ---
function onFrame(buf: ArrayBuffer) {
  const { keyframe, captured } = parseHeader(buf);
  const chunkData = new Uint8Array(buf, 24);          // header is 24 bytes
  if (decoder.decodeQueueSize > MAX_QUEUE && !keyframe) { dropped++; return; }
  if (needKeyframe && !keyframe) return;              // after reset/loss, wait for IDR
  const chunk = new EncodedVideoChunk({
    type: keyframe ? "key" : "delta",
    timestamp: Number(captured),                       // µs; monotonic (no B-frames)
    data: chunkData,
  });
  try { decoder.decode(chunk); } catch { recreateDecoder(); }
  if (keyframe) needKeyframe = false;
}

// --- output side: present LATEST per rAF, close EVERY frame promptly (adversarial M1) ---
let pending: VideoFrame | null = null;
const decoder = new VideoDecoder({
  output: (frame: VideoFrame) => {
    // Close any previously-stashed unpresented frame IMMEDIATELY (tiny decoder pool; MDN warns
    // apps can crash past ~a few active frames, and unclosed frames STALL decoding).
    if (pending) pending.close();
    pending = frame;                                   // present only the newest per compositor tick
  },
  error: (e) => { postMessage({ type: "fatal", code: String(e) }); recreateDecoder(); },
});
function presentLoop() {                               // requestAnimationFrame loop
  if (pending) {
    gl.texImage2D(/* … */, pending);                   // GPU-resident: VideoFrame is a TexImageSource
    draw();
    pending.close();                                   // *** VideoFrame.close() DISCIPLINE ***
    pending = null;
  }
  requestAnimationFrame(presentLoop);
}

// --- recovery: a decode error → terminal "closed"; rebuild + feed an IDR ---
function recreateDecoder() {
  try { decoder.close(); } catch {}
  if (pending) { pending.close(); pending = null; }
  needKeyframe = true;
  postMessage({ type: "needKeyframe" });               // main → invoke("request_keyframe")
  buildDecoder(config);
}
```

**Why worker + OffscreenCanvas:** the main thread's only frame-path job is to relay opaque
ArrayBuffers and `invoke("request_keyframe")` when the worker asks. A stalled/lost/never-arriving
video cannot block the local cursor, toolbar, or Stop — it just stops painting the canvas. This is
the direct mechanical realization of the load-bearing invariant.

### 6.4 Native-surface fallback seam

```rust
/// The controller's presentation backend. The session/app layer is written entirely against this
/// trait; it never knows whether frames land in a webview VideoDecoder or a native Metal/D3D11
/// surface. Swapping is a construction-time choice, no session churn. Speaks ONLY the canonical
/// EncodedFrame + StreamConfig.
pub trait RendererSurface: Send {
    fn configure(&mut self, config: &ras_media::StreamConfig) -> Result<(), SessionError>;
    /// Submit one encoded frame. MUST be non-blocking + drop-tolerant (a late frame is worthless).
    fn submit(&mut self, frame: ras_media::EncodedFrame);
    /// The surface requests a fresh IDR (decoder reset/lost sync). App forwards to request_keyframe().
    fn poll_keyframe_request(&mut self) -> bool;
    fn shutdown(&mut self);
}

/// Phase-1 impl: pushes frames onto the Tauri frame Channel (§6.1). "Decode" happens in the webview.
pub struct WebviewChannelSurface { /* frame Channel sender, keyframe flag */ }
impl RendererSurface for WebviewChannelSurface { /* ... todo!() ... */ }

/// Planned v2 impl (macOS/Linux/latency-critical): decode in Rust (VideoToolbox / MediaFoundation)
/// and direct-present to Metal/D3D11/Vulkan behind a transparent Tauri overlay webview for UI.
/// Same trait ⇒ app layer untouched.
pub struct NativeSurface { /* platform decoder + swapchain */ }
```

**Runtime probe & trigger:** (1) **Capability gate** — at startup the webview runs
`'VideoDecoder' in self && (await VideoDecoder.isConfigSupported({codec, /*Annex-B*/})).supported`;
if false (old WebKitGTK / WKWebView lacking WebCodecs) construct `NativeSurface`. macOS: also require
Ventura 13.3+ and treat WKWebView WebCodecs as *probe, don't assume*; Linux WebKitGTK is fragile →
native from the start on unsupported versions. (2) **Latency trigger** — the worker reports
`decodeMs` and a Rust watchdog measures glass-to-glass; if it exceeds target by ≥1 compositor frame
attributable to compositor/present, move that platform to `NativeSurface`. Windows MVP stays on
WebCodecs.

### 6.5 Connection-state UI (never blocks)

`ras-transport-iroh`/`ras-core` sample `Connection::stats()`/`rtt()` + iroh path events on their own
task and publish a `watch::Receiver<ConnHealth>` (latest-wins). `open_status_stream` forwards each
change onto a Status Channel; the JS side keeps only the last message (latest-wins on the UI side
too). The `LinkState::Stalled` transition is computed in **Rust** (watchdog: no frame for N ms), so
"video stalled" and the frozen frame are the same event, surfaced with zero UI-thread work. Because
the status stream and the frame stream are **independent Channels on independent Rust tasks**, a
wedged frame pump does not stop status updates. `PathKind::Migrating` must be handled in any UI match
(map to Relayed for the badge).

### 6.6 Security config (priority #1)

- **Pin `tauri >= 2.11.1`** in `src-tauri/Cargo.toml` (GHSA-7gmj-67g7-phm9 Origin-Confusion:
  `<=2.11.0` lets a remote page invoke local-only IPC; also GHSA-57fm-592m-34r7 iframe origin
  bypass). Single most important line. `cargo-deny` in CI; track advisories.
- **Deny-by-default capabilities** — `src-tauri/capabilities/session.json` grants **only**
  `connect_host`, `open_frame_stream`, `open_status_stream`, `request_keyframe`, `stop_session`,
  scoped to the session window. No `fs`, `shell`, `http`, `dialog`, or broad core permissions.
- **Isolation Pattern enabled** (`tauri.conf.json → app.security.pattern = "isolation"`): AES-GCM
  IPC interception validates every invoke in an isolated frame before it reaches Rust. Input is
  re-validated in Rust — never trust the webview.
- **Strict CSP** (`tauri.conf.json`): `default-src 'self'`; no `unsafe-eval`, no `unsafe-inline`
  scripts, no remote script origins; `worker-src 'self'`; `connect-src` limited to `ipc:`/`tauri:`;
  `img-src`/`media-src` narrowly `'self'`. All assets bundled; nothing loaded remotely.
- **Remote feed to `<canvas>`/WebGL only, never the DOM.** The H.264 stream is **hostile input**;
  decoding exercises the platform decoder inside the sandboxed worker, and a decode failure is
  contained to `recreateDecoder()` — no shell/system calls are exposed.
- **Stop responsiveness (Invariant 4, adversarial L3):** Tauri commands and the frame Channel share
  the IPC transport. Benchmark command latency under max frame throughput; if commands can be
  starved behind video bytes, move the frame stream to the localhost-WebSocket fallback (which must
  still fit the deny-by-default `connect-src`) so `stop_session` is never queued behind video.
- Config lives in `src-tauri/tauri.conf.json`, `src-tauri/capabilities/session.json`,
  `src-tauri/Cargo.toml`. No Phase-2 auth is designed here, but nothing above blocks it — grants are
  validated in `ras-core` **before** `connect_host` returns Ok and **before any Channel opens**, so
  the webview is never handed a live frame stream on an unauthorized session.

---

## 7. What stays STUBBED until the Phase-S go/no-go

Phase S (`docs/17`, `docs/design/phase-S-design.md`) is a throwaway spike that converts the biggest
unvalidated bets to measured numbers. Carry the **numbers + the go/no-go ADR** into Phase 1, never
the spike code. These stubs stay `todo!()` until the spike decides them — and the table names the
decision that flips each.

> **Status — the spine is built (spike-independent).** The parts of Phase 1 that do **not** depend on
> the spike are implemented and verified green (build/clippy/test/deny): the canonical cross-crate
> types (§2), the pure state machine (§5.1), the DI seams (§5.4), the event model (§5.6), the no-op
> auth seam (§5.5), and the **host + controller orchestrators** (§5.2/§5.3) — exercised end-to-end by
> a `SyntheticCaptureBackend`+`SyntheticEncoder` (§3.4) over an in-memory `LoopbackTransport`
> (`crates/ras-core/src/testkit.rs`). One `#[tokio::test]` drives host→controller streaming, the
> keyframe-request round-trip, and clean terminal teardown with no iroh / OS / GPU. Reconciliations
> made during execution: (a) `HostSession` is **generic** over capture/encoder (those traits aren't
> object-safe — GAT + generic `encode`), `dyn` kept for transport/validator/sink; (b) `GrantValidator`
> and the other DI seams use `#[async_trait]` for object safety (design §5.5's own signature), so the
> earlier RPITIT `GrantValidator` was replaced; (c) `ras-core` now depends on `tokio` + `async-trait`
> (both design-sanctioned, permissive). The table below is what **remains** stubbed behind the traits.
>
> **Also landed (spike-independent):** a concrete `LatencyFirstAbr` (`ras-core::abr`) wired into the
> host as a 250 ms stats/ABR tick — samples `transport.health()`, retargets CBR via the media
> thread's `set_bitrate` (keyframe-free), and emits `LifecycleEvent::ConnectionQuality`; and the
> controller frame-Channel codec (`ras-core::frame_channel`) — the exact 24-byte LE header contract
> of §6.1 (`FRAME_MAGIC`/`FRAME_HEADER_LEN` shared with the future TS `decoder.worker.ts`), with
> `u64` ids kept BigInt-safe. The ABR *control law* stays tunable by the spike numbers (table below);
> the trait/DTO shapes are fixed.

| Stub (left `todo!()`)                                     | Spike bet / target that decides it                                    | Which way the decision flips the stub |
|----------------------------------------------------------|-----------------------------------------------------------------------|---------------------------------------|
| `VideoTransport` concrete impl (`PerFrameStream` vs `DatagramFec`) + `VideoFragHeader` FEC shaping | Transport latency overhead < 80 ms beyond RTT; per-frame RTT (`spike/iroh-probe`) | Picks which impl is built first and whether `VideoFragHeader`/FEC ships in P1 at all. `EncodedFrame.data: Bytes` supports both — only the impl behind the trait changes. |
| `SendOutcome` staleness/congestion thresholds in `VideoSink` | Per-frame RTT + loss on direct vs relayed (`spike/iroh-probe`)        | Sets the drop thresholds; if relay loss is high, `DatagramFec` + stronger FEC becomes default. |
| `WebviewChannelSurface` as the primary render path vs `NativeSurface` | Glass-to-glass < 120 ms LAN via WebCodecs in WebView2 (`spike/latency-probe/web`) | If WebCodecs misses the target, `NativeSurface` becomes the P1 primary on that platform (trait already abstracts it). |
| Frame Channel transport (`Channel(Raw)` vs localhost-WebSocket) | WebView2 large-payload IPC + Stop-starvation benchmark (`docs/12`, adversarial L3) | If IPC starves commands / is too slow on Windows, the frame stream moves to a localhost WebSocket; the 24-byte header + `EncodedFrame` are unchanged. |
| `ScreenCaptureKitBackend` / `VideoToolboxEncoder` bodies (and the DXGI/MF Windows port) | DXGI/SCK capture → HW H.264, 30 FPS, ACCESS_LOST/SCK-restart recovery (`spike/latency-probe`) | Confirms the recoverable-error taxonomy and the zero-copy import path; the traits don't change, only the bodies. |
| `RelayPolicy::Custom` (self-hosted relay map)             | Iroh direct/relay session setup > 95% on supported nets (`spike/iroh-probe`) | If default relays suffice for dev but not prod, wires the self-hosted map; `Default` stays for CI. |
| `AdaptiveBitrateController` concrete impl                 | Bandwidth-estimate quality from `ConnHealth` (derived from the RTT/loss probe) | Sets the control law; the trait + `BitrateDecision` are fixed regardless. |
| `LifecycleStream` concrete type (`impl Stream` vs mpsc vs callback) | SDK-extraction shape (`docs/05 §6`) — not a spike-blocker but pick before SDK split | Flips only the return type; events are unchanged. |

Everything **not** in this table (canonical types §2, the state machine §5.1, the auth seam §5.5,
the channel map §4.2, the header layout §6.1, the security config §6.6) is **decided now** and does
not wait on the spike.

---

## 8. Open questions

1. **Q-FRAME-HOME (confirm before writing skeletons).** `EncodedFrame`/`StreamConfig` are
   producer-owned in `ras-media`, which makes `ras-transport-iroh` depend on `ras-media`. Accept
   that dependency direction? The only alternative is hoisting both into `ras-protocol`. Pick one
   now — the skeletons can't be written until this is fixed. (Recommendation: `ras-media`-owned.)
2. **Q-STREAM (transport pattern).** `PerFrameStream` vs `DatagramFec` is deferred to Phase S. If
   the spike shows datagram+FEC wins on relay but per-frame-stream wins direct, do we allow
   switching mid-session on a `Direct↔Relayed` migration (requires a renegotiation `StreamConfig` +
   decoder reset)? The enum/config already permit re-sending `StreamConfig`; confirm we want the
   adaptive switch vs pin-at-start.
3. **Q-FEC-CONTROL.** `fec_k`/`fec_n` live in the wire header but nothing yet chooses them from
   `ConnHealth.loss_fraction`. That logic belongs in the `DatagramFec` impl — should `ras-core` be
   able to override (force stronger FEC on a known-lossy path)?
4. **Q-DGRAM-MTU.** `SessionConfig.datagram_payload` defaults to ~1200, but iroh's
   `max_datagram_size()` is path-dependent and can shrink after migration. Clamp once at session
   start, or re-clamp on each `Migrating→settled` transition? (Under-shoot is safe but wasteful;
   over-shoot silently drops.)
5. **Q-REORDER-OWNER.** Confirmed split: **Rust `ras-core` owns reorder-by-`frame_id` + loss
   detection** and emits `DecoderFeedback`/`KeyframeRequest`; JS stays a dumb decode-and-present
   sink. Confirm this vs doing gap detection in the Worker.
6. **Q-KEEPALIVE.** Confirmed: **no video re-emit on static screen** — a reliable control-channel
   heartbeat instead; WebCodecs holds last-good. Confirm the heartbeat cadence and that "new
   subscriber needs current frame" is handled as an IDR-on-demand, not a timer.
7. **Q-ABR-HOME.** `AdaptiveBitrateController` is a trait in `ras-media`; the concrete impl could
   live in `ras-media` (next to the encoder it drives) or `ras-core` (next to session state + the
   `ConnHealth` feed). Trait placement allows either; pick before implementation.
8. **Q-KEYFRAME-RATE-LIMIT.** Repeated `KeyframeRequest` on a lossy relayed path can cause IDR
   storms (bitrate spikes). Should the host apply reference-frame-invalidation / intra-refresh
   instead of honoring every request, with the controller debouncing? (`docs/10 §4` favors RFI over
   IDR-on-loss.)
9. **Q-INCOMING-IDENTITY (`[verify]`, Phase-S blocker).** The whole "reject before opening channels"
   + Phase-2 pinning story assumes `Incoming::remote()` yields the TLS-verified `EndpointId` before
   `Incoming::accept()` given accept-after-first-byte. If false, the pin check moves *inside*
   `accept()` (already designed to allow this) — verify against the pinned `iroh =1.0.x`.
10. **Q-STATS-MAPPING (`[verify]`, Phase-S blocker).** `ConnHealth.estimated_bandwidth_bps` and
    `loss_fraction` must be derived from Quinn/iroh `stats()` fields whose exact names/semantics
    (and CUBIC-vs-BBR "budget" meaning) need validation against the pinned iroh version.
11. **Q-POINTER-SCOPE (decide now).** The transport design specified a `PointerChannel`/
    `PointerUpdate` (normalized virtual cursor) that neither `ras-core` nor the controller wires up.
    **Recommendation: DEFER the entire pointer seam out of Phase 1** (view-only = transport + screen
    only) and omit `PointerChannel` from `Session` — as reflected in §4.2. If instead a remote-cursor
    overlay is wanted in P1, add `SessionTransport::pointer()`, a core→controller `PointerUpdate`
    stream, and a `Channel<PointerUpdate>` to the webview cursor layer. All four layers must agree;
    right now only transport had it.
12. **Q-CONSENT-SHAPE (Phase-2 forward-check).** The auth seam (§5.5) now carries opaque
    `AuthEnvelope` bytes and a multi-step `GrantDecision` (`NeedConsent`/`Challenge`). Confirm this
    is expressive enough for Invariants 1–3 (signed access request bound to host+controller+endpoint,
    nonce/replay, requested capabilities) so Phase 2 fills fields, never restructures the machine.
13. **Q-STOP-TRANSPORT (`[verify]`, Invariant 4).** Benchmark `stop_session` command latency under
    max frame throughput. If frame-Channel bytes can starve command dispatch on shared IPC, move the
    frame stream to the localhost-WebSocket fallback so Stop is never behind video (≤250 ms target).
