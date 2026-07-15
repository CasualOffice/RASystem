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
    /// the H.264 level implied by the frame dimensions. This projection lives only at the Tauri/JS
    /// boundary (the wire/in-memory type stays the enum).
    ///
    /// Format is `avc1.PPCCLL` — profile_idc, constraint-set flags, level_idc, each two hex digits.
    /// We emit Main profile (`0x4D`) with constraint byte `0x40` (matches the desktop-encoder
    /// output), and pick the smallest level whose `MaxFS` (macroblocks/frame, Table A-1 of the
    /// H.264 spec) covers `ceil(w/16)·ceil(h/16)`. Level, not bitrate/fps, is what the decoder needs
    /// to size its buffers; dimensions are the load-bearing input.
    #[must_use]
    pub fn webcodecs_string(self, width: u32, height: u32) -> String {
        match self {
            VideoCodec::H264AnnexB => {
                let mbs = width.div_ceil(16) as u64 * height.div_ceil(16) as u64;
                format!("avc1.4D40{:02X}", h264_level_idc_for_mbs(mbs))
            }
        }
    }
}

/// Smallest H.264 `level_idc` whose `MaxFS` (frame size in macroblocks) covers `mbs`. Frame-size
/// bound only (Phase 1 is single-monitor, moderate fps); the DPB/bitrate bounds of higher levels are
/// not the binding constraint here. Saturates at 6.2 (`0x3E`) for anything larger.
#[must_use]
const fn h264_level_idc_for_mbs(mbs: u64) -> u8 {
    // (level_idc, MaxFS) from Table A-1, ascending. level_idc = round(level * 10).
    const LEVELS: [(u8, u64); 12] = [
        (0x0A, 99),     // 1.0
        (0x14, 396),    // 2.0
        (0x15, 792),    // 2.1
        (0x16, 1620),   // 2.2 / 3.0 share 1620; 0x1E chosen below
        (0x1E, 1620),   // 3.0
        (0x1F, 3600),   // 3.1
        (0x20, 5120),   // 3.2
        (0x28, 8192),   // 4.0 / 4.1
        (0x2A, 8704),   // 4.2
        (0x32, 22080),  // 5.0
        (0x33, 36864),  // 5.1 / 5.2
        (0x3E, 139264), // 6.0+ (saturating)
    ];
    let mut i = 0;
    while i < LEVELS.len() {
        if mbs <= LEVELS[i].1 {
            return LEVELS[i].0;
        }
        i += 1;
    }
    0x3E
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

/// Discriminates what a [`PlatformSurface`] points at, so the paired platform encoder can refuse a
/// surface it does not recognise (fail-closed) instead of blindly dereferencing. Additive per
/// platform (ADR-058).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SurfaceKind {
    /// No backing GPU surface (synthetic / test capture). The pointer is null.
    None,
    /// macOS: a borrowed `CVPixelBuffer` (`objc2_core_video::CVPixelBuffer`), IOSurface-backed.
    MacCoreVideoPixelBuffer,
    /// A CPU-resident, top-down **BGRA8888** frame (Linux/Windows software capture → software
    /// encoder). The pointer is a `*const `[`CpuBgraFrame`] describing the borrowed buffer. Used by
    /// the cross-platform OpenH264 encoder, which reads the bytes and converts to I420 (ADR-063).
    CpuBgra,
}

/// Descriptor for a CPU-resident BGRA frame, pointed at by a [`PlatformSurface`] of kind
/// [`SurfaceKind::CpuBgra`]. `data` addresses `height * stride` bytes; each of `height` rows begins a
/// `width * 4`-byte run of **BGRA** (byte order B,G,R,A) within a `stride`-byte row (`stride >=
/// width*4`, for row padding). The buffer is **borrowed** for the lifetime of the producing
/// [`CapturedFrame`]; the consumer (the paired software encoder) must not retain the pointer past the
/// `encode` call. Constructing/holding this is safe; the dereference contract lives with the encoder
/// (mirrors [`PlatformSurface`], ADR-058).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CpuBgraFrame {
    /// Start of the top-left pixel's byte.
    pub data: *const u8,
    /// Total readable length in bytes (`>= (height - 1) * stride + width * 4`).
    pub len: usize,
    /// Bytes per row (`>= width * 4`).
    pub stride: usize,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

/// Opaque, thread-affine, **borrowed** handle to a GPU-resident surface (macOS: IOSurface-backed
/// `CVPixelBuffer`; Windows: D3D11 texture). Produced by a capture backend and consumed by its
/// **paired same-platform** encoder within one `encode` call (ADR-058).
///
/// The pointer is interpreted **only** inside the platform crate that produced it (macOS:
/// `ras-media-macos`); it never crosses to `ras-core`/transport/controller and core never
/// dereferences it. `HostSession` only ever pairs a capture backend with its matching encoder, so
/// the encoder can trust the surface origin — the [`SurfaceKind`] tag is a fail-closed guard, not
/// the primary safety mechanism. Constructing one is safe (storing a pointer is not `unsafe`); the
/// dereference contract lives with the platform-crate consumer of [`PlatformSurface::as_ptr`].
pub struct PlatformSurface<'a> {
    ptr: *const core::ffi::c_void,
    kind: SurfaceKind,
    _marker: core::marker::PhantomData<&'a ()>,
}

impl<'a> PlatformSurface<'a> {
    /// A surface with no GPU backing (synthetic capture). An encoder that needs real pixels will get
    /// `None` from [`Self::as_ptr`] and must error rather than fabricate — except the synthetic
    /// encoder, which ignores the surface entirely.
    #[must_use]
    pub fn none() -> Self {
        Self {
            ptr: core::ptr::null(),
            kind: SurfaceKind::None,
            _marker: core::marker::PhantomData,
        }
    }

    /// Wrap a borrowed platform-surface pointer with its `kind`. Safe to call: this only *stores* the
    /// pointer. The caller must ensure `ptr` is a valid pointer to the surface type implied by `kind`
    /// and outlives `'a` (the [`CapturedFrame`] that produced it); the dereference — and that
    /// obligation — happens in the platform crate via [`Self::as_ptr`].
    #[must_use]
    pub fn from_ptr(ptr: *const core::ffi::c_void, kind: SurfaceKind) -> Self {
        Self {
            ptr,
            kind,
            _marker: core::marker::PhantomData,
        }
    }

    /// The surface kind (what `ptr` points at).
    #[must_use]
    pub fn kind(&self) -> SurfaceKind {
        self.kind
    }

    /// The raw pointer **iff** the surface matches `expect`, else `None` (fail-closed). The caller (a
    /// platform encoder) casts the returned pointer to the concrete surface type for `expect` and is
    /// responsible for the dereference safety contract in [`Self::from_ptr`].
    #[must_use]
    pub fn as_ptr(&self, expect: SurfaceKind) -> Option<core::ptr::NonNull<core::ffi::c_void>> {
        if self.kind == expect {
            core::ptr::NonNull::new(self.ptr as *mut core::ffi::c_void)
        } else {
            None
        }
    }
}

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

/// The captured display's bounds in the desktop's global coordinate space, **logical units**
/// (points, top-left origin, y-down) — the space macOS global coordinates and Tauri
/// `LogicalPosition`/`LogicalSize` share, so the host UI can size its pointer overlay to cover
/// exactly the display being shared (correct on a secondary monitor, not just the primary). The
/// remote-pointer position is normalized over the frame, so these bounds only need to place + size
/// the overlay, not match pixel resolution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RemoteDisplayBounds {
    /// Global x of the display's top-left, logical units.
    pub x: i32,
    /// Global y of the display's top-left, logical units.
    pub y: i32,
    /// Display width, logical units.
    pub width: u32,
    /// Display height, logical units.
    pub height: u32,
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

    /// The captured display's global bounds (logical units), if the backend can report them, so the
    /// host UI can place its pointer overlay over exactly the shared display. `None` (the default)
    /// means "unknown" — the caller falls back to the primary/whole screen. Valid only while a
    /// capture is active.
    fn captured_bounds(&self) -> Option<RemoteDisplayBounds> {
        None
    }

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

#[cfg(feature = "synthetic")]
pub mod synthetic;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcodecs_string_matches_level_by_dimensions() {
        // 720p → 3600 MBs → level 3.1 (0x1F): the canonical "avc1.4D401F".
        assert_eq!(
            VideoCodec::H264AnnexB.webcodecs_string(1280, 720),
            "avc1.4D401F"
        );
        // 1080p → 8160 MBs → level 4.0 (0x28).
        assert_eq!(
            VideoCodec::H264AnnexB.webcodecs_string(1920, 1080),
            "avc1.4D4028"
        );
        // 4K → 32400 MBs → level 5.1 (0x33).
        assert_eq!(
            VideoCodec::H264AnnexB.webcodecs_string(3840, 2160),
            "avc1.4D4033"
        );
    }

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
    fn platform_surface_is_fail_closed_on_kind_mismatch() {
        // Synthetic surface exposes no pointer to any kind.
        let s = PlatformSurface::none();
        assert_eq!(s.kind(), SurfaceKind::None);
        assert!(s.as_ptr(SurfaceKind::MacCoreVideoPixelBuffer).is_none());

        // A tagged surface hands its pointer back only for the matching kind.
        let x = 0xABu8;
        let ptr = core::ptr::from_ref(&x).cast::<core::ffi::c_void>();
        let s = PlatformSurface::from_ptr(ptr, SurfaceKind::MacCoreVideoPixelBuffer);
        assert!(
            s.as_ptr(SurfaceKind::None).is_none(),
            "wrong kind must not yield the pointer"
        );
        assert_eq!(
            s.as_ptr(SurfaceKind::MacCoreVideoPixelBuffer)
                .map(|p| p.as_ptr().cast_const()),
            Some(ptr),
            "matching kind yields the exact pointer"
        );
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
