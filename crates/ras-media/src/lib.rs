//! Casual RAS media pipeline: capture / encode / decode / render **interfaces** (Phase 1).
//!
//! Populated with real backends across Phases 1+ (`docs/10`, `docs/18`). This crate is
//! transport-agnostic: it depends only on `ras-protocol`, never on `ras-transport-iroh`. Frames
//! are `bytes::Bytes` for O(1) clone/slice (the allocation-light guarantee). No `async` in the
//! frame-producing traits — async lives at the transport edge that `ras-core` owns.
//!
//! Canonical types (`docs/design/phase-1-design.md` §2): [`EncodedFrame`] and [`StreamConfig`]
//! are defined here and imported (never re-declared) by transport and the controller.

use bytes::Bytes;

/// This crate's error alias over the shared taxonomy.
pub type MediaError = ras_protocol::RasError;

// Id aliases live in `ras-protocol` (to break the cycle with the control-message set); re-exported
// here so downstream code can say `ras_media::FrameId`.
pub use ras_protocol::{CaptureTimestampUs, FrameId};

/// The one encoded access unit. Defined once here; transport and controller import it.
///
/// `data` is a complete Annex-B access unit (start-code `0x000001` delimited NALs). Keyframes
/// carry SPS+PPS **in-band** and re-send them every IDR, so a fresh `VideoDecoder` recovers from
/// any keyframe with no out-of-band `description`. There is deliberately no avcC path.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Monotonic id; a gap means loss (per-stream). Reassembly key + staleness clock.
    pub frame_id: FrameId,
    /// Host monotonic capture time.
    pub captured_at_us: CaptureTimestampUs,
    /// True IDR. The controller must (re)start decoding on one of these. Intra-refresh /
    /// recovery-point SEI frames are not keyframes and this is `false` for them.
    pub is_keyframe: bool,
    /// Complete Annex-B access unit.
    pub data: Bytes,
    /// Config snapshot this frame was encoded under (carried per-frame because the video path is
    /// droppable/out-of-order, so a resolution change reacts atomically with its IDR).
    pub config: StreamConfig,
}

/// The one stream descriptor. `ras-core`/controller "descriptor"/"info" types are DTO projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Codec as an enum; the WebCodecs string is derived only at the Tauri boundary.
    pub codec: VideoCodec,
    /// Output width (px), post-scale, portrait de-rotated.
    pub width: u32,
    /// Output height (px).
    pub height: u32,
    /// Target frames/sec (capture may run higher; the pacer decides emit rate).
    pub fps: u32,
    /// Target average bitrate (bits/sec), CBR. Driven by the ABR hook (`ras-core`).
    pub target_bitrate_bps: u32,
    /// Color space the decoder must assume.
    pub color: ColorSpace,
    /// Which concrete video transport this session negotiated (so the receiver reassembles right).
    pub video_transport: VideoTransportKind,
}

/// H.264 profile we emit. B-frames are off in every variant (no reorder latency).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoCodec {
    /// H.264 Annex-B, Main profile, CABAC, no B-frames. Default.
    H264AnnexB,
}

impl VideoCodec {
    /// Derive the fully-qualified WebCodecs codec string (e.g. `"avc1.4D401F"`) from the codec plus
    /// dimensions/level. This projection lives only at the Tauri/JS boundary.
    #[must_use]
    pub fn webcodecs_string(self, width: u32, height: u32) -> String {
        let _ = (width, height);
        todo!("derive avc1.PPCCLL from level implied by dimensions")
    }
}

/// The one color-space enum. Limited-range BT.709 is the desktop-encoder default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorSpace {
    /// Limited-range BT.709.
    Bt709Limited,
    /// Full-range BT.709.
    Bt709Full,
}

/// Which droppable video transport a session uses. Both variants are droppable and free of
/// cross-frame head-of-line blocking; the concrete choice is decided post-spike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoTransportKind {
    /// One unidirectional QUIC stream per frame, `reset()`-able on staleness.
    PerFrameStream,
    /// App-level fragmentation over datagrams + per-frame Reed-Solomon FEC (block depth = 1 frame).
    DatagramFec,
}

/// A monitor/display identifier (single monitor in Phase 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorId(pub u32);

/// A window identifier (used to exclude our own overlay/consent windows from capture).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub u64);

/// Opaque, thread-affine handle to a GPU-resident surface (macOS: IOSurface-backed CVPixelBuffer;
/// Windows: D3D11 texture). Only valid on the capture/encode thread; never crosses a crate
/// boundary as a raw pointer.
pub struct PlatformSurface<'a>(core::marker::PhantomData<&'a ()>);

/// Options for a capture session.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// Single monitor in Phase 1; explicit so multi-monitor is additive.
    pub monitor: MonitorId,
    /// Target capture rate.
    pub target_fps: u32,
    /// Exclude our own overlay/consent windows (privacy + no capture feedback loop).
    pub excluded_window_ids: Vec<WindowId>,
}

/// Zero-copy handle to one captured frame, still GPU-resident. Consumed by value by the paired
/// encoder within one call so the capture pool slot is recycled promptly.
pub trait CapturedFrame {
    /// Capture time on the host monotonic clock.
    fn captured_at_us(&self) -> CaptureTimestampUs;
    /// Frame width (px).
    fn width(&self) -> u32;
    /// Frame height (px).
    fn height(&self) -> u32;
    /// Opaque platform surface for the paired same-platform encoder.
    fn platform_surface(&self) -> PlatformSurface<'_>;
}

/// Frame source. Pull-based, synchronous, blocking-with-timeout — not async, not callback. Pull
/// lets the pacer drop frames it can't keep up with (video is droppable by design).
pub trait ScreenCaptureBackend: Send {
    /// The backend's captured-frame type (borrows the backend's surface pool).
    type Frame<'a>: CapturedFrame
    where
        Self: 'a;

    /// Select the monitor + capture rate; returns the negotiated [`StreamConfig`].
    fn start(&mut self, opts: &CaptureOptions) -> Result<StreamConfig, MediaError>;

    /// Block until the next frame or `timeout`. `Ok(None)` = timed out / static screen (the pacer
    /// decides; there is no video re-emit — a control heartbeat covers liveness). A recoverable
    /// `Err` (SCK restart / DXGI `ACCESS_LOST`) means the caller rebuilds via [`Self::start`].
    fn next_frame(
        &mut self,
        timeout: core::time::Duration,
    ) -> Result<Option<Self::Frame<'_>>, MediaError>;

    /// The currently negotiated config.
    fn config(&self) -> StreamConfig;

    /// Stop capture and release resources.
    fn stop(&mut self);
}

/// Hardware-preferred H.264 encoder. Zero-copy surface in, Annex-B out. Synchronous single-frame
/// call on the encode thread.
pub trait VideoEncoderBackend: Send {
    /// Build/configure. Applies the invariant knobs: RealTime, B-frames off, CBR, infinite GOP +
    /// forced-IDR-on-demand.
    fn configure(&mut self, config: &StreamConfig) -> Result<(), MediaError>;

    /// Encode exactly one captured frame. Consumes the surface by value so capture recycles its
    /// pool slot immediately. Any produced frame is a complete access unit.
    fn encode<F: CapturedFrame>(&mut self, frame: F) -> Result<Option<EncodedFrame>, MediaError>;

    /// Request the next frame be a true IDR. Idempotent within an interval; the sole keyframe
    /// mechanism (infinite GOP, no periodic keyframes).
    fn request_keyframe(&mut self, reason: ras_protocol::KeyframeReason);

    /// Retarget CBR bitrate mid-stream without a reconfigure/keyframe (driven by ABR each RTT).
    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError>;

    /// The currently negotiated config.
    fn config(&self) -> StreamConfig;
}

/// A decoded, presentable frame (native-fallback path only).
pub struct DecodedFrame {
    /// Width (px).
    pub width: u32,
    /// Height (px).
    pub height: u32,
}

/// Native decode fallback (VideoToolbox / Media Foundation in Rust). The WebCodecs path does not
/// implement this (it lives in JS); this exists so the native-surface fallback is the same
/// interface, not a rewrite. The first frame after `configure`/`reset` must be a keyframe.
pub trait DecoderBackend: Send {
    /// Configure for a stream.
    fn configure(&mut self, config: &StreamConfig) -> Result<(), MediaError>;
    /// Submit one access unit for decoding.
    fn decode(&mut self, frame: &EncodedFrame) -> Result<(), MediaError>;
    /// Non-blocking pull of a decoded frame; caller presents and drops promptly (tiny pool).
    fn poll_decoded(&mut self) -> Option<DecodedFrame>;
    /// A decode error is terminal; recovery = `reset()` then a keyframe.
    fn reset(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> StreamConfig {
        StreamConfig {
            codec: VideoCodec::H264AnnexB,
            width: 1920,
            height: 1080,
            fps: 60,
            target_bitrate_bps: 6_000_000,
            color: ColorSpace::Bt709Limited,
            video_transport: VideoTransportKind::PerFrameStream,
        }
    }

    #[test]
    fn encoded_frame_composes_with_config() {
        let f = EncodedFrame {
            frame_id: 7,
            captured_at_us: 123_456,
            is_keyframe: true,
            data: Bytes::from_static(&[0, 0, 1, 0x67]),
            config: sample_config(),
        };
        assert!(f.is_keyframe);
        assert_eq!(f.config.width, 1920);
        // Bytes clone is O(1) refcount, not a copy.
        let g = f.clone();
        assert_eq!(g.frame_id, 7);
    }
}
