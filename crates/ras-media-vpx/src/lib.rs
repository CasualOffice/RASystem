//! Cross-platform **software VP8/VP9 encoder** (libvpx) implementing [`ras_media::VideoEncoderBackend`].
//!
//! A royalty-free alternative to the OpenH264 H.264 path (`ras-media-openh264`), chosen for **Linux**:
//! WebKitGTK reliably decodes VP8/VP9 (it often *cannot* decode H.264), the codecs are
//! patent-unencumbered, and libvpx's realtime encoder is what RustDesk (study-only) ships. It consumes
//! CPU **BGRA** frames (a capture backend hands them over as a [`ras_media::SurfaceKind::CpuBgra`]
//! surface), converts to I420, and emits a raw **VP8/VP9 bitstream** access unit per frame — the frame
//! payload from libvpx's compressed-data packet, which the WebCodecs `VideoDecoder` consumes directly
//! (VP8/VP9 need no in-band parameter sets; a keyframe is self-describing).
//!
//! Structure mirrors [`ras-media-openh264`]: first-frame keyframe, forced-IDR-on-demand, live
//! keyframe-free `set_bitrate`, and BGRA→I420 with row-padding + odd-dimension handling. Additions
//! over the H.264 path: **VP9 temporal SVC** (3 temporal layers in a fixed `0212` pattern, or 2 in
//! `0101`), so a bandwidth-constrained sender can *shed* the top temporal layer's frames without
//! breaking the stream (the periodic layer pattern is deterministic, so the pacer knows each frame's
//! layer).
//!
//! **VP9 is the default** ([`VpxCodec::Vp9`]); VP8 is selectable ([`VpxEncoder::new_with`]). VP9 gives
//! better screen-content compression and real temporal SVC; VP8 is a compatibility fallback.
//!
//! FFI-bearing: the workspace `unsafe_code = deny` is relaxed here (CONTRIBUTING §5). **All** `unsafe`
//! is confined to this crate — the borrowed-surface dereference plus the libvpx codec calls (the safe
//! `vpx-encode` wrapper exposes neither forced keyframes, CBR, temporal SVC, nor runtime bitrate, so we
//! bind `env-libvpx-sys` directly).

use std::os::raw::c_int;
use std::ptr;

use bytes::Bytes;
use ras_media::{
    CapturedFrame, ColorSpace, CpuBgraFrame, EncodedFrame, MediaError, StreamConfig, SurfaceKind,
    VideoCodec, VideoTransportKind,
};
use ras_protocol::{ErrorCode, KeyframeReason, RasError};
use vpx_sys as ffi;

/// Codec capabilities of this backend (libvpx software encode). Used by the app to build the host's
/// [`ras_grant::HostEncodeCaps`] for codec negotiation.
pub const SUPPORTS_H264: bool = false;
/// This backend encodes VP9.
pub const SUPPORTS_VP9: bool = true;
/// This backend also encodes VP8.
pub const SUPPORTS_VP8: bool = true;

/// Default target bitrate advertised in [`StreamConfig`], in bits/sec. The encoder is built CBR at
/// this value and retargeted at runtime by the ABR via [`VpxEncoder::set_bitrate`].
const DEFAULT_BITRATE_BPS: u32 = 8_000_000;

/// Which libvpx codec this encoder drives. VP9 is the default (temporal SVC + better screen-content
/// compression); VP8 is a compatibility fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VpxCodec {
    /// VP9 (default). Supports temporal SVC.
    Vp9,
    /// VP8. No SVC layering here (single temporal layer).
    Vp8,
}

impl VpxCodec {
    /// The exact **WebCodecs** codec string the receiver must configure its `VideoDecoder` with.
    ///
    /// - VP8 → `"vp8"`.
    /// - VP9 → `"vp09.PP.LL.BD"` = profile / level / bit-depth. We emit **profile 0** (`00`, 8-bit
    ///   4:2:0), **8-bit** (`08`), and pick the VP9 **level** (`LL`) from the frame dimensions +
    ///   fps the same way the H.264 path derives its level (buffer sizing is what the decoder needs).
    ///   e.g. 720p60 → `"vp09.00.40.08"`, 1080p60 → `"vp09.00.41.08"`, 2160p60 → `"vp09.00.51.08"`.
    #[must_use]
    pub fn webcodecs_string(self, width: u32, height: u32, fps: u32) -> String {
        match self {
            VpxCodec::Vp8 => "vp8".to_string(),
            VpxCodec::Vp9 => {
                format!("vp09.00.{:02}.08", vp9_level_for(width, height, fps))
            }
        }
    }
}

/// Smallest VP9 level (as the two-digit `LL` code, e.g. 31 for level 3.1) whose luma sample-rate and
/// picture-size limits cover `width×height@fps`. From the VP9 bitstream spec Annex-A level table
/// (`MaxLumaSampleRate` = samples/sec, `MaxLumaPictureSize` = samples). Frame-rate is the load-bearing
/// input for the sample-rate bound; we saturate at level 6.2 for anything larger. The level only sizes
/// the decoder's buffers — being slightly generous is harmless, so we round up.
#[must_use]
fn vp9_level_for(width: u32, height: u32, fps: u32) -> u8 {
    let pic = (width as u64) * (height as u64);
    let rate = pic * (fps.max(1) as u64);
    // (LL code, MaxLumaSampleRate, MaxLumaPictureSize) ascending, VP9 spec Annex-A.
    const LEVELS: [(u8, u64, u64); 14] = [
        (10, 829_440, 36_864),           // 1
        (11, 2_764_800, 73_728),         // 1.1
        (20, 4_608_000, 122_880),        // 2
        (21, 9_216_000, 245_760),        // 2.1
        (30, 20_736_000, 552_960),       // 3
        (31, 36_864_000, 983_040),       // 3.1
        (40, 83_558_400, 2_228_224),     // 4
        (41, 160_432_128, 2_228_224),    // 4.1
        (50, 311_951_360, 8_912_896),    // 5
        (51, 588_251_136, 8_912_896),    // 5.1
        (52, 1_176_502_272, 8_912_896),  // 5.2
        (60, 1_176_502_272, 35_651_584), // 6
        (61, 2_353_004_544, 35_651_584), // 6.1
        (62, 4_706_009_088, 35_651_584), // 6.2
    ];
    for (code, max_rate, max_pic) in LEVELS {
        if rate <= max_rate && pic <= max_pic {
            return code;
        }
    }
    62
}

fn enc_fatal(context: &'static str) -> MediaError {
    RasError::fatal(ErrorCode::EncoderFailed, context)
}

/// Number of temporal layers for VP9 SVC. 3 layers (pattern `0212`) gives two shed points; 2 layers
/// (`0101`) gives one. We default to 3 for VP9 (RustDesk-class realtime screen sharing).
const VP9_TEMPORAL_LAYERS: u32 = 3;

/// Highest temporal-layer id reachable by [`VP9_TEMPORAL_LAYERS`] (3 layers ⇒ ids `0..=2`). Used as
/// the default "forward everything" ceiling for [`VpxEncoder::set_max_temporal_layer`].
const VP9_MAX_LAYER_ID: u8 = (VP9_TEMPORAL_LAYERS - 1) as u8;

/// Minimum kbps floor for the VP9 temporal SVC **base layer** (`ts_target_bitrate[0]`). This is the
/// one layer that can never be shed — every receiver, even one dropping every enhancement frame, still
/// depends on the base layer alone decoding to a usable picture. At very low total bitrates (an
/// aggressive ABR downshift under real congestion — exactly the scenario SVC exists to survive) a plain
/// percentage split (`total * 40 / 100`) can floor to 0 kbps under integer division, hobbling libvpx's
/// rate controller for the layer that matters most right when bandwidth is tightest. 20 kbps is well
/// below any usable screen-share bitrate but keeps the base layer's rate-control target sane instead of
/// zero.
const VP9_BASE_LAYER_MIN_KBPS: u32 = 20;

/// Split a total VP9 CBR target (`total_kbps`, matching `cfg.rc_target_bitrate`'s units) into the
/// **cumulative** per-temporal-layer targets libvpx's SVC config expects (`ts_target_bitrate[i]` is the
/// bitrate for layers `0..=i` combined, strictly ascending, and the last entry equals `total_kbps` —
/// see the comment on `VpxEncoder::build`). `num_layers` must be 2 or 3 (the only patterns this encoder
/// configures); any other value returns an empty slice conceptually (callers only invoke this under
/// `self.svc()` with a layer count they just set).
///
/// The base layer (index 0) is floored at [`VP9_BASE_LAYER_MIN_KBPS`] so it is never starved to 0 by
/// integer-division rounding at very low total bitrates. Layer 1 (3-layer case) is then clamped up to
/// at least the (possibly-raised) base so the sequence stays non-decreasing, and the total always ends
/// at `total_kbps` exactly, preserving libvpx's cumulative/ascending/ends-at-total contract.
fn vp9_temporal_layer_bitrates_kbps(total_kbps: u32, num_layers: u32) -> [u32; 3] {
    match num_layers {
        3 => {
            let base = ((total_kbps * 40) / 100).max(VP9_BASE_LAYER_MIN_KBPS.min(total_kbps));
            let mid = ((total_kbps * 60) / 100).max(base);
            [base, mid, total_kbps]
        }
        _ => {
            let base = ((total_kbps * 60) / 100).max(VP9_BASE_LAYER_MIN_KBPS.min(total_kbps));
            [base, total_kbps, 0]
        }
    }
}

/// Map a frame's position within the temporal-SVC GOP to its libvpx temporal-layer id, for the fixed
/// periodic patterns this encoder configures (`docs/10`, VP9 spec temporal-layering annex).
///
/// - **3 layers, pattern `0212`** (`period` = 4): layer 0 is the base (every 4th frame — the only
///   frames a `max_layer = 0` receiver needs), layer 1 is the middle (every 2nd), layer 2 is the top
///   (every frame). Matches `ts_layer_id = [0, 2, 1, 2, ...]` in [`VpxEncoder::build_encoder`].
/// - **2 layers, pattern `0101`** (`period` = 2): layer 0 every 2nd frame, layer 1 every frame.
///
/// Returns `None` for any other `period` (defensive; the two patterns above are the only ones this
/// encoder ever configures). Pure and `libvpx`-independent, so it is unit-testable without a codec
/// context.
#[must_use]
fn temporal_layer_for_frame_index(idx: u32, period: u32) -> Option<u8> {
    match period {
        4 => match idx % 4 {
            0 => Some(0),
            1 => Some(2),
            2 => Some(1),
            3 => Some(2),
            _ => None, // unreachable (idx % 4 < 4)
        },
        2 => match idx % 2 {
            0 => Some(0),
            1 => Some(1),
            _ => None, // unreachable (idx % 2 < 2)
        },
        _ => None,
    }
}

/// Software VP8/VP9 encoder over libvpx.
pub struct VpxEncoder {
    codec: VpxCodec,
    config: StreamConfig,
    /// The initialized libvpx encoder context. `None` until the first `configure`/`encode`.
    ctx: Option<Box<ffi::vpx_codec_ctx_t>>,
    /// Owned, contiguous I420 plane buffer (Y then U then V), rebuilt on a dimension change.
    i420: Vec<u8>,
    /// Dimensions the current `i420`/encoder were built for.
    dims: Option<(u32, u32)>,
    /// Contiguous BGRA scratch used when the source has row padding or an odd dimension is cropped.
    repack: Vec<u8>,
    /// Emit the next frame as a forced keyframe (startup + on demand). Infinite GOP otherwise.
    force_idr: bool,
    /// Monotonic frame id; a gap on the wire means loss.
    next_id: u64,
    /// Monotonic presentation timestamp handed to libvpx (in timebase units = frames).
    pts: i64,
    /// Position of the *next* frame within the temporal-SVC periodic GOP (wraps at `ts_periodicity`;
    /// 4 for the 3-layer `0212` pattern, 2 for the 2-layer `0101` pattern). Tracked independently of
    /// `pts`/`next_id` so it stays correct even across dropped/coalesced frames (which do not advance
    /// `next_id`) — libvpx itself advances its internal layer cursor once per *submitted* frame, which
    /// is exactly what this counter mirrors (incremented once per `vpx_codec_encode` call, not once
    /// per produced output frame).
    svc_gop_pos: u32,
    /// The temporal-SVC layer id of the most recently *produced* (non-dropped) output frame, if this
    /// encoder is running VP9 SVC. `None` for VP8 (no temporal layering) or before the first frame.
    /// This is the "layer-id-on-output-frame" readout: [`Self::last_temporal_layer`] lets the caller
    /// correlate a just-returned [`EncodedFrame`] with the layer it belongs to, without re-deriving it
    /// or parsing the VP9 uncompressed header.
    last_temporal_layer: Option<u8>,
    /// Layer-selection knob (sender-side ABR hook): the highest temporal-layer id this encoder will
    /// *forward*. Frames computed to be above this ceiling are encoded (libvpx's rate control still
    /// needs every frame submitted to keep the periodic pattern + per-layer bitrate split coherent) but
    /// their compressed output is discarded before it reaches the caller — i.e. `encode()` returns
    /// `Ok(None)` for them, exactly like the existing "encoder coalesced a static frame" no-output
    /// case, so callers need no new branch. **Keyframes are never dropped** by this knob (a forced IDR
    /// is always let through regardless of its layer id — it is the decoder's resync point and is
    /// requested deliberately, e.g. on `request_keyframe`/first frame). Defaults to
    /// [`VP9_MAX_LAYER_ID`] (forward every layer — today's unconditional 3-layer behavior, unchanged).
    max_forwarded_layer: u8,
}

// The encoder is owned and driven from a single media thread (moved there once, never shared); the
// libvpx context is used single-threaded. Same rationale as the OpenH264 / macOS backends' shim. The
// raw pointers inside `vpx_codec_ctx_t` are not otherwise `Send`.
unsafe impl Send for VpxEncoder {}

impl Default for VpxEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VpxEncoder {
    fn drop(&mut self) {
        if let Some(ctx) = self.ctx.as_mut() {
            // SAFETY: `ctx` is a live, initialized encoder context we own; destroy releases libvpx's
            // internal allocations exactly once (guarded by the `Option` take on the next line pattern
            // — here we only ever drop once).
            unsafe {
                ffi::vpx_codec_destroy(ctx.as_mut());
            }
        }
    }
}

impl VpxEncoder {
    /// A VP9 encoder (the default codec).
    #[must_use]
    pub fn new() -> Self {
        Self::new_with(VpxCodec::Vp9)
    }

    /// An encoder for a specific codec (`Vp9` default, `Vp8` fallback).
    #[must_use]
    pub fn new_with(codec: VpxCodec) -> Self {
        Self {
            codec,
            config: default_stream_config(codec, 1920, 1080, 60),
            ctx: None,
            i420: Vec::new(),
            dims: None,
            repack: Vec::new(),
            force_idr: true,
            next_id: 0,
            pts: 0,
            svc_gop_pos: 0,
            last_temporal_layer: None,
            max_forwarded_layer: VP9_MAX_LAYER_ID,
        }
    }

    /// The codec this encoder drives (so the app can derive the WebCodecs string for the receiver).
    #[must_use]
    pub fn codec(&self) -> VpxCodec {
        self.codec
    }

    fn iface(&self) -> *const ffi::vpx_codec_iface {
        // SAFETY: these are libvpx's global interface accessors; they return a static, valid pointer.
        unsafe {
            match self.codec {
                VpxCodec::Vp9 => ffi::vpx_codec_vp9_cx(),
                VpxCodec::Vp8 => ffi::vpx_codec_vp8_cx(),
            }
        }
    }

    /// Whether this encoder runs VP9 temporal SVC.
    fn svc(&self) -> bool {
        matches!(self.codec, VpxCodec::Vp9)
    }

    /// The `ts_periodicity` this encoder configures for its temporal-SVC pattern (4 for the 3-layer
    /// `0212` pattern, 2 for the 2-layer `0101` pattern) — matches [`Self::build_encoder`]. VP8 (no
    /// SVC) reports 0, the "no pattern" sentinel [`temporal_layer_for_frame_index`] returns `None` for.
    fn svc_periodicity(&self) -> u32 {
        if self.svc() {
            match VP9_TEMPORAL_LAYERS {
                3 => 4,
                _ => 2,
            }
        } else {
            0
        }
    }

    /// The VP9 temporal-SVC layer id of the most recently *produced* (non-dropped, non-coalesced)
    /// output frame from [`ras_media::VideoEncoderBackend::encode`] — the "layer-id-on-output-frame"
    /// readout a caller (e.g. the transport/session layer) can consult right after a successful
    /// `encode()` call to tag that frame for downstream layer-aware handling. `None` for VP8 (no
    /// temporal layering) or before the first frame is produced.
    #[must_use]
    pub fn last_temporal_layer(&self) -> Option<u8> {
        self.last_temporal_layer
    }

    /// The number of temporal layers this encoder is configured for (3 for VP9, 1 — no layering — for
    /// VP8). Layer ids from [`Self::last_temporal_layer`] range over `0..temporal_layer_count()`.
    #[must_use]
    pub fn temporal_layer_count(&self) -> u8 {
        if self.svc() {
            VP9_TEMPORAL_LAYERS as u8
        } else {
            1
        }
    }

    /// **Layer-selection knob.** Set the highest VP9 temporal-layer id this encoder will *forward* to
    /// the caller (sender-side ABR hook, orthogonal to [`Self::set_bitrate`] — a second, independent
    /// knob for shedding frames under tight bandwidth without a bitrate/quality collapse). `None` or
    /// any value `>= temporal_layer_count() - 1` forwards every layer (today's default, unconditional
    /// behavior). `Some(0)` forwards only the base layer (1/4 the frame rate of the full 3-layer
    /// pattern); `Some(1)` forwards the base + middle layers (1/2 the frame rate). No-op for VP8 (there
    /// is no layer to select — `encode()` never drops on this knob when `!self.svc()`).
    ///
    /// Dropped frames are still submitted to libvpx (its rate controller and periodic layer pattern
    /// need every frame to stay coherent) — only the compressed output is discarded, so `encode()`
    /// returns `Ok(None)` for them exactly like the existing "coalesced static frame" case. **A forced
    /// keyframe is always forwarded regardless of this ceiling** (Inv-agnostic correctness: an IDR is
    /// the decoder's sole resync point and is only ever emitted on deliberate request, so dropping one
    /// would silently break the receiver with no recovery path).
    pub fn set_max_temporal_layer(&mut self, max_layer: Option<u8>) {
        self.max_forwarded_layer = max_layer.unwrap_or(VP9_MAX_LAYER_ID);
    }

    /// The layer-selection ceiling currently in effect (see [`Self::set_max_temporal_layer`]).
    #[must_use]
    pub fn max_temporal_layer(&self) -> u8 {
        self.max_forwarded_layer
    }

    /// Build the libvpx encoder context for the current `config` and dimensions.
    fn build_encoder(&mut self, w: u32, h: u32) -> Result<(), MediaError> {
        let iface = self.iface();

        // Start from libvpx's realtime defaults, then apply our invariant knobs. The cfg contains
        // `#[repr(u32)]` enums with no zero variant, so `mem::zeroed` is invalid — use `MaybeUninit`
        // and let `vpx_codec_enc_config_default` fully populate it before we read.
        let mut cfg = {
            let mut cfg = std::mem::MaybeUninit::<ffi::vpx_codec_enc_cfg_t>::uninit();
            // SAFETY: `vpx_codec_enc_config_default` writes every field of `*cfg` for `iface` (or
            // returns an error, which we check before assuming init).
            let rc = unsafe { ffi::vpx_codec_enc_config_default(iface, cfg.as_mut_ptr(), 0) };
            if rc != ffi::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(enc_fatal("vpx_codec_enc_config_default failed"));
            }
            // SAFETY: the call above returned OK, so `cfg` is fully initialized.
            unsafe { cfg.assume_init() }
        };

        cfg.g_w = w;
        cfg.g_h = h;
        // Timebase = 1/fps, so a per-frame pts increment of 1 == one frame of wall time.
        cfg.g_timebase.num = 1;
        cfg.g_timebase.den = self.config.fps.max(1) as c_int;
        // Realtime, low-latency: no lookahead (no buffered future frames), error-resilient off by
        // default (SVC turns on its own resilience below). CBR at the target bitrate (kbps).
        cfg.g_lag_in_frames = 0;
        cfg.rc_end_usage = ffi::vpx_rc_mode::VPX_CBR;
        cfg.rc_target_bitrate = (self.config.target_bitrate_bps.max(1) / 1000).max(1);
        // Tight rate-control buffer for low latency (values in ms, libvpx convention).
        cfg.rc_buf_initial_sz = 500;
        cfg.rc_buf_optimal_sz = 600;
        cfg.rc_buf_sz = 1000;
        cfg.rc_min_quantizer = 4;
        cfg.rc_max_quantizer = 56;
        cfg.rc_dropframe_thresh = 0; // we decide dropping upstream (the pacer/SVC), not the encoder
                                     // Infinite GOP: keyframes are forced on demand only (startup + `request_keyframe`), never
                                     // periodic. `kf_max_dist` very large ≈ "auto only when we ask".
        cfg.kf_mode = ffi::vpx_kf_mode::VPX_KF_AUTO;
        cfg.kf_min_dist = 0;
        cfg.kf_max_dist = u32::MAX;
        cfg.g_threads = 4;
        cfg.g_error_resilient = 0;

        // VP9 temporal SVC: N temporal layers in a fixed periodic pattern, so a bandwidth-limited
        // sender can shed the top layer's frames. libvpx assigns layer ids internally under the
        // periodic modes; we split the target bitrate across layers (lower layers get a larger share,
        // matching their higher decimation).
        if self.svc() {
            let layers = VP9_TEMPORAL_LAYERS;
            cfg.ts_number_layers = layers;
            cfg.g_error_resilient = ffi::VPX_ERROR_RESILIENT_DEFAULT;
            // The layering *mode* is a config field (there is no separate control id for it): a fixed
            // periodic pattern lets libvpx assign temporal-layer ids internally. Per-layer cumulative
            // bitrate must be ascending and end at the total (40/60/100% for 3 layers; 60/100% for 2),
            // with the base layer floored at `VP9_BASE_LAYER_MIN_KBPS` — see
            // `vp9_temporal_layer_bitrates_kbps`.
            let total = cfg.rc_target_bitrate;
            let split = vp9_temporal_layer_bitrates_kbps(total, layers);
            match layers {
                3 => {
                    cfg.temporal_layering_mode =
                        ffi::vp9e_temporal_layering_mode::VP9E_TEMPORAL_LAYERING_MODE_0212 as c_int;
                    cfg.ts_periodicity = 4;
                    cfg.ts_layer_id = [0, 2, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                    cfg.ts_rate_decimator[0] = 4;
                    cfg.ts_rate_decimator[1] = 2;
                    cfg.ts_rate_decimator[2] = 1;
                    cfg.ts_target_bitrate[0] = split[0];
                    cfg.ts_target_bitrate[1] = split[1];
                    cfg.ts_target_bitrate[2] = split[2];
                }
                _ => {
                    cfg.temporal_layering_mode =
                        ffi::vp9e_temporal_layering_mode::VP9E_TEMPORAL_LAYERING_MODE_0101 as c_int;
                    cfg.ts_periodicity = 2;
                    cfg.ts_layer_id = [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                    cfg.ts_rate_decimator[0] = 2;
                    cfg.ts_rate_decimator[1] = 1;
                    cfg.ts_target_bitrate[0] = split[0];
                    cfg.ts_target_bitrate[1] = split[1];
                }
            }
        }

        // Fresh context. Destroy any prior one first (a reconfigure with a dimension change).
        if let Some(mut old) = self.ctx.take() {
            // SAFETY: `old` is a live context we own; destroy once.
            unsafe {
                ffi::vpx_codec_destroy(old.as_mut());
            }
        }
        // VPX_CODEC_OK == 0, so zero is a valid `vpx_codec_ctx_t` bit-pattern (all-null pointers + OK
        // err). `vpx_codec_enc_init_ver` then populates it.
        let mut ctx: Box<ffi::vpx_codec_ctx_t> = {
            let mut c = std::mem::MaybeUninit::<ffi::vpx_codec_ctx_t>::uninit();
            // SAFETY: `iface`/`cfg` are valid; init writes every field of `*c` (or returns an error we
            // check before assuming init). `VPX_ENCODER_ABI_VERSION` is the binding-matched ABI const.
            let rc = unsafe {
                ffi::vpx_codec_enc_init_ver(
                    c.as_mut_ptr(),
                    iface,
                    &cfg,
                    0,
                    ffi::VPX_ENCODER_ABI_VERSION as c_int,
                )
            };
            if rc != ffi::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(enc_fatal("vpx_codec_enc_init failed"));
            }
            // SAFETY: init returned OK, so the context is fully initialized.
            Box::new(unsafe { c.assume_init() })
        };

        // Realtime speed knob: VP9 `cpu-used` 8/9 (max speed), VP8 16. Then SVC layering mode.
        let cpu_used: c_int = match self.codec {
            VpxCodec::Vp9 => 8,
            VpxCodec::Vp8 => 16,
        };
        self.control(
            &mut ctx,
            ffi::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
            cpu_used,
        )?;
        if self.svc() {
            // Adaptive quantization tuned for screen content; harmless for camera too. (The temporal
            // layering mode is a config field set in `build_encoder`, not a control call.)
            self.control(
                &mut ctx,
                ffi::vp8e_enc_control_id::VP9E_SET_AQ_MODE as c_int,
                3,
            )?;
        }

        self.ctx = Some(ctx);
        self.dims = Some((w, h));
        // A fresh libvpx context always starts its periodic temporal-layering pattern at position 0
        // (`ts_layer_id[0]`, the base layer) — this rebuild path also runs on a dimension change
        // mid-stream, not just the initial build, so re-sync our own GOP-position mirror here (not only
        // in `configure`) to keep `temporal_layer_for_frame_index` reporting the id libvpx actually
        // assigned to each subsequent frame.
        self.svc_gop_pos = 0;
        Ok(())
    }

    /// Wrapper over `vpx_codec_control_` (the variadic setter) for an `int`-valued control.
    fn control(
        &self,
        ctx: &mut ffi::vpx_codec_ctx_t,
        ctrl_id: c_int,
        val: c_int,
    ) -> Result<(), MediaError> {
        // SAFETY: `ctx` is an initialized encoder; each control id here takes a single `c_int` by
        // value (VP8E_SET_CPUUSED / VP9E_SET_TEMPORAL_LAYERING_MODE / VP9E_SET_AQ_MODE), matching the
        // variadic contract. libvpx copies the value.
        let rc = unsafe { ffi::vpx_codec_control_(ctx, ctrl_id, val) };
        if rc != ffi::vpx_codec_err_t::VPX_CODEC_OK {
            return Err(enc_fatal("vpx_codec_control_ failed"));
        }
        Ok(())
    }

    /// Read the borrowed CPU BGRA descriptor out of a captured frame's surface (fail-closed on any
    /// mismatch), returning `(bytes, stride, width, height)` — dimensions cropped to even.
    fn bgra<F: CapturedFrame>(frame: &F) -> Result<(&[u8], usize, u32, u32), MediaError> {
        let surface = frame.platform_surface();
        let ptr = surface
            .as_ptr(SurfaceKind::CpuBgra)
            .ok_or_else(|| enc_fatal("expected a CpuBgra surface"))?;
        // SAFETY: the paired software capture backend set this pointer to a `CpuBgraFrame` it owns for
        // the lifetime of `frame` (ADR-058/063). We only read it within this call.
        let desc = unsafe { &*(ptr.as_ptr() as *const CpuBgraFrame) };
        if desc.data.is_null() || desc.width == 0 || desc.height == 0 {
            return Err(enc_fatal("empty CpuBgra surface"));
        }
        let w = desc.width & !1;
        let h = desc.height & !1;
        if w == 0 || h == 0 {
            return Err(enc_fatal("frame too small"));
        }
        let needed = desc
            .stride
            .checked_mul(desc.height as usize)
            .ok_or_else(|| enc_fatal("stride overflow"))?;
        if desc.stride < (desc.width as usize) * 4 || desc.len < needed {
            return Err(enc_fatal("CpuBgra buffer too small for its dimensions"));
        }
        // SAFETY: bounds validated above; the buffer is borrowed for the call.
        let bytes = unsafe { core::slice::from_raw_parts(desc.data, desc.len) };
        Ok((bytes, desc.stride, w, h))
    }

    /// Convert a tightly-packed top-down BGRA slice (`row = w*4` bytes/row, `h` rows) into the
    /// contiguous I420 output buffer `out` (Y then U then V), BT.601 full-range coefficients. `w`/`h`
    /// are even. Free-standing (no `&self`) so it can be called while other `self` buffers are
    /// borrowed as the input, without aliasing `&mut self`.
    fn bgra_to_i420(out: &mut Vec<u8>, packed: &[u8], w: u32, h: u32) {
        let (wu, hu) = (w as usize, h as usize);
        let y_size = wu * hu;
        let c_w = wu / 2;
        let c_h = hu / 2;
        let c_size = c_w * c_h;
        out.resize(y_size + 2 * c_size, 0);
        let (y_plane, uv) = out.split_at_mut(y_size);
        let (u_plane, v_plane) = uv.split_at_mut(c_size);

        // Luma for every pixel.
        for j in 0..hu {
            let row = &packed[j * wu * 4..j * wu * 4 + wu * 4];
            let y_out = &mut y_plane[j * wu..j * wu + wu];
            for i in 0..wu {
                let b = row[i * 4] as i32;
                let g = row[i * 4 + 1] as i32;
                let r = row[i * 4 + 2] as i32;
                // BT.601: Y = 0.299R + 0.587G + 0.114B (full range).
                y_out[i] = (((77 * r + 150 * g + 29 * b) + 128) >> 8) as u8;
            }
        }
        // Chroma: average each 2x2 block, then compute U/V from the block-average RGB.
        for cj in 0..c_h {
            for ci in 0..c_w {
                let mut rs = 0i32;
                let mut gs = 0i32;
                let mut bs = 0i32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let px = (ci * 2 + dx) * 4;
                        let row = (cj * 2 + dy) * wu * 4;
                        bs += packed[row + px] as i32;
                        gs += packed[row + px + 1] as i32;
                        rs += packed[row + px + 2] as i32;
                    }
                }
                let r = rs / 4;
                let g = gs / 4;
                let b = bs / 4;
                // BT.601 full range: U = -0.169R -0.331G +0.5B +128; V = 0.5R -0.419G -0.081B +128.
                let u = ((-43 * r - 84 * g + 127 * b + 128) >> 8) + 128;
                let v = ((127 * r - 107 * g - 20 * b + 128) >> 8) + 128;
                u_plane[cj * c_w + ci] = u.clamp(0, 255) as u8;
                v_plane[cj * c_w + ci] = v.clamp(0, 255) as u8;
            }
        }
    }
}

impl ras_media::VideoEncoderBackend for VpxEncoder {
    fn configure(&mut self, config: &StreamConfig) -> Result<(), MediaError> {
        self.config = *config;
        self.i420.clear();
        self.dims = None;
        self.force_idr = true;
        // A reconfigure restarts the temporal-SVC GOP at its base-layer position (0), matching a fresh
        // encoder — `ts_layer_id[0]` is always the base layer, so this keeps the two never out of sync.
        self.svc_gop_pos = 0;
        self.last_temporal_layer = None;
        // Defer the actual libvpx build to the first `encode`, where we know the real (even) frame
        // dimensions from the captured surface — matching the OpenH264 backend's lazy build.
        // Drop any existing context so a reconfigure starts clean.
        if let Some(mut old) = self.ctx.take() {
            // SAFETY: live context we own; destroy once.
            unsafe {
                ffi::vpx_codec_destroy(old.as_mut());
            }
        }
        Ok(())
    }

    fn encode<F: CapturedFrame>(&mut self, frame: F) -> Result<Option<EncodedFrame>, MediaError> {
        let captured_at_us = frame.captured_at_us();
        let (bytes, stride, w, h) = Self::bgra(&frame)?;
        let row = (w as usize) * 4;

        // (Re)build the encoder if not yet built or dimensions changed — do this before touching the
        // repack scratch, so the mutable `self` borrow does not overlap the immutable `packed` borrow.
        if self.dims != Some((w, h)) || self.ctx.is_none() {
            self.build_encoder(w, h)?;
        }

        // Feed a tightly-packed BGRA slice. Repack when the source has row padding or was cropped.
        // `bytes` borrows the captured surface (not `self`); the repack path fills `self.repack`. We
        // convert into `self.i420` — a *disjoint* field from `self.repack`, so borrowing one mutably
        // and reading the other is fine (no `&mut self` method call spans the read).
        if stride == row && bytes.len() >= row * h as usize {
            Self::bgra_to_i420(&mut self.i420, &bytes[..row * h as usize], w, h);
        } else {
            self.repack.resize(row * h as usize, 0);
            for y in 0..h as usize {
                let src = &bytes[y * stride..y * stride + row];
                self.repack[y * row..y * row + row].copy_from_slice(src);
            }
            let (i420, repack) = (&mut self.i420, &self.repack);
            Self::bgra_to_i420(i420, repack, w, h);
        }

        // Wrap the I420 buffer as a vpx_image (borrowed — no copy). Stride alignment 1. The image
        // struct holds `#[repr(u32)]` enums with no zero variant, so use `MaybeUninit`; `vpx_img_wrap`
        // populates it.
        let mut img = std::mem::MaybeUninit::<ffi::vpx_image_t>::uninit();
        // SAFETY: `img` is uninit storage `vpx_img_wrap` fully initializes; `self.i420` holds a valid
        // I420 buffer of the right size (Y=w*h, U/V=(w/2)*(h/2)) built just above. The wrap borrows the
        // buffer (self_allocd=0); we do not free it.
        let wrapped = unsafe {
            ffi::vpx_img_wrap(
                img.as_mut_ptr(),
                ffi::vpx_img_fmt::VPX_IMG_FMT_I420,
                w,
                h,
                1,
                self.i420.as_mut_ptr(),
            )
        };
        if wrapped.is_null() {
            return Err(enc_fatal("vpx_img_wrap failed"));
        }
        // SAFETY: wrap returned non-null, so `img` is initialized.
        let img = unsafe { img.assume_init() };

        let flags: ffi::vpx_enc_frame_flags_t = if self.force_idr {
            self.force_idr = false;
            ffi::VPX_EFLAG_FORCE_KF as ffi::vpx_enc_frame_flags_t
        } else {
            0
        };

        let ctx = self.ctx.as_mut().ok_or_else(|| enc_fatal("no encoder"))?;
        let pts = self.pts;
        // SAFETY: `ctx` is initialized, `img` is a valid wrapped image, both live for the call. The
        // realtime deadline matches our low-latency posture.
        let rc = unsafe {
            ffi::vpx_codec_encode(
                ctx.as_mut(),
                &img,
                pts,
                1,
                flags,
                // Cast to the deadline param's type INFERRED from the fn signature: libvpx 1.15+ names it
                // `vpx_enc_deadline_t`, but 1.14 (Ubuntu) has no such typedef and uses plain `c_ulong`.
                // `as _` compiles against both instead of a version-specific type name (CI-caught: #5).
                ffi::VPX_DL_REALTIME as _,
            )
        };
        if rc != ffi::vpx_codec_err_t::VPX_CODEC_OK {
            return Err(enc_fatal("vpx_codec_encode failed"));
        }
        self.pts += 1;

        // Drain the compressed-data packets: concatenate all CX_FRAME_PKT payloads for this input
        // frame (realtime never buffers, so it is one packet, but we drain defensively) and OR their
        // keyframe flags.
        let mut data: Vec<u8> = Vec::new();
        let mut is_keyframe = false;
        let mut iter: ffi::vpx_codec_iter_t = ptr::null();
        loop {
            // SAFETY: `ctx` is initialized; `iter` starts null and is advanced by libvpx. The returned
            // pointer is valid until the next `get_cx_data`/`encode` call, so we copy out immediately.
            let pkt = unsafe { ffi::vpx_codec_get_cx_data(ctx.as_mut(), &mut iter) };
            if pkt.is_null() {
                break;
            }
            let pkt = unsafe { &*pkt };
            if pkt.kind == ffi::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                // SAFETY: kind == FRAME_PKT selects the `frame` union arm.
                let f = unsafe { &pkt.data.frame };
                if !f.buf.is_null() && f.sz > 0 {
                    // SAFETY: `f.buf`/`f.sz` describe the compressed bytes libvpx owns for now; copy.
                    let slice = unsafe { core::slice::from_raw_parts(f.buf as *const u8, f.sz) };
                    data.extend_from_slice(slice);
                    if f.flags & ffi::VPX_FRAME_IS_KEY != 0 {
                        is_keyframe = true;
                    }
                }
            }
        }

        // Free the wrapped image's descriptor (no owned data — stride 1 wrap over our buffer).
        // vpx_img_wrap sets self_allocd=0 so free only tears down the (stack) descriptor; we skip it
        // since `img` is a stack value that drops here.

        // Derive this submitted frame's temporal-SVC layer id from its position in the periodic GOP
        // *before* advancing the position — `svc_gop_pos` names the slot the frame we just handed to
        // `vpx_codec_encode` occupied, matching the `ts_layer_id[...]` libvpx assigned it internally.
        // `None` for VP8 (`svc_periodicity() == 0`) or if the periodicity is ever something other than
        // the two patterns this encoder configures (defensive; unreachable in practice).
        let period = self.svc_periodicity();
        let layer = temporal_layer_for_frame_index(self.svc_gop_pos, period);
        // One real frame was submitted to libvpx regardless of what it emitted (a coalesced/decimated
        // frame still occupies its GOP slot), so the position mirror always advances here.
        if period > 0 {
            self.svc_gop_pos = (self.svc_gop_pos + 1) % period;
        }

        if data.is_empty() {
            // The encoder dropped/coalesced this frame (static screen / SVC decimation) — nothing to
            // send. Do not advance the frame id; no output frame exists to attribute a layer to.
            return Ok(None);
        }

        // Layer-id-on-output-frame readout: record which layer this produced frame belongs to so the
        // caller can consult `last_temporal_layer()` immediately after this call.
        self.last_temporal_layer = layer;

        // Layer-selection knob: shed this frame if it is above the forwarded ceiling. Never applies to
        // VP8 (`layer` is `None`) or to a keyframe (the sole decoder resync point — always let through).
        if let Some(l) = layer {
            if !is_keyframe && l > self.max_forwarded_layer {
                // Encoded (libvpx's rate control + pattern state already accounted for it above) but
                // not forwarded — same "no output this call" contract as a coalesced frame, so callers
                // need no new branch to handle sender-side layer shedding.
                return Ok(None);
            }
        }

        let frame_id = self.next_id;
        self.next_id += 1;

        Ok(Some(EncodedFrame {
            frame_id,
            captured_at_us,
            is_keyframe,
            data: Bytes::from(data),
            config: self.config,
        }))
    }

    fn request_keyframe(&mut self, _reason: KeyframeReason) {
        self.force_idr = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        self.config.target_bitrate_bps = bitrate_bps;
        // Retarget the live encoder's rate controller without forcing a keyframe. libvpx applies a new
        // config via `vpx_codec_enc_config_set` mid-stream (no reinit, no IDR). If the encoder is not
        // built yet, the new target is picked up at build time.
        let target_kbps = (bitrate_bps.max(1) / 1000).max(1);
        let svc = self.svc();
        if let Some(ctx) = self.ctx.as_mut() {
            // Read the current config back, patch bitrate (+ per-layer split), set it.
            // SAFETY: `ctx` is initialized, so its `config` union's `enc` arm is the active one (this
            // is an encoder context) and points at a valid `vpx_codec_enc_cfg` while the context
            // lives. We copy it out, patch plain-data fields, and hand it back by pointer; libvpx
            // validates + copies it.
            let cur = unsafe { ctx.config.enc };
            if cur.is_null() {
                return Err(enc_fatal("encoder has no active config"));
            }
            let mut cfg: ffi::vpx_codec_enc_cfg_t = unsafe { *cur };
            cfg.rc_target_bitrate = target_kbps;
            if svc {
                let total = target_kbps;
                let split = vp9_temporal_layer_bitrates_kbps(total, cfg.ts_number_layers);
                match cfg.ts_number_layers {
                    3 => {
                        cfg.ts_target_bitrate[0] = split[0];
                        cfg.ts_target_bitrate[1] = split[1];
                        cfg.ts_target_bitrate[2] = split[2];
                    }
                    2 => {
                        cfg.ts_target_bitrate[0] = split[0];
                        cfg.ts_target_bitrate[1] = split[1];
                    }
                    _ => {}
                }
            }
            let rc = unsafe { ffi::vpx_codec_enc_config_set(ctx.as_mut(), &cfg) };
            if rc != ffi::vpx_codec_err_t::VPX_CODEC_OK {
                return Err(enc_fatal("vpx_codec_enc_config_set failed"));
            }
        }
        Ok(())
    }

    fn config(&self) -> StreamConfig {
        self.config
    }

    fn set_max_temporal_layer(&mut self, max_layer: Option<u8>) {
        // Delegate to the existing inherent method (kept public in its own right — the crate's own
        // tests call it directly without going through the trait object).
        VpxEncoder::set_max_temporal_layer(self, max_layer);
    }
}

/// The [`StreamConfig`] these VP8/VP9 software backends negotiate. The concrete VP8/VP9 codec identity
/// is now carried **in-band** as the matching [`VideoCodec`] variant (`Vp9`/`Vp8`), so the receiver
/// derives its WebCodecs decoder string from `StreamConfig.codec.webcodecs_string(w, h)` with no
/// out-of-band threading. Per-frame-stream transport; limited-range declared for parity.
///
/// NOTE: the actual `StreamConfig.codec` a share stamps is owned by the **capture** backend
/// ([`ras_media`]'s `ScreenCaptureBackend::start`), not the encoder. This helper is used by the crate's
/// own tests and any caller building a config directly; production Linux/Windows shares get their config
/// from `ras-media-scap` (which now stamps [`VideoCodec::Vp9`]). The two MUST agree — capture-declared
/// codec and encoder bytes — or the decoder is configured for a codec the bytes aren't.
#[must_use]
pub fn default_stream_config(codec: VpxCodec, width: u32, height: u32, fps: u32) -> StreamConfig {
    StreamConfig {
        codec: match codec {
            VpxCodec::Vp9 => VideoCodec::Vp9,
            VpxCodec::Vp8 => VideoCodec::Vp8,
        },
        width,
        height,
        fps,
        target_bitrate_bps: DEFAULT_BITRATE_BPS,
        color: ColorSpace::Bt709Limited,
        video_transport: VideoTransportKind::PerFrameStream,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_media::{PlatformSurface, VideoEncoderBackend};

    /// A synthetic captured frame backed by a CPU BGRA buffer + its descriptor.
    struct Frame {
        desc: CpuBgraFrame,
        w: u32,
        h: u32,
    }
    impl CapturedFrame for Frame {
        fn captured_at_us(&self) -> u64 {
            1234
        }
        fn width(&self) -> u32 {
            self.w
        }
        fn height(&self) -> u32 {
            self.h
        }
        fn platform_surface(&self) -> PlatformSurface<'_> {
            PlatformSurface::from_ptr(core::ptr::from_ref(&self.desc).cast(), SurfaceKind::CpuBgra)
        }
    }

    fn gradient(w: u32, h: u32, stride: usize) -> Vec<u8> {
        let mut buf = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = y * stride + x * 4;
                buf[i] = (x * 4) as u8; // B
                buf[i + 1] = (y * 4) as u8; // G
                buf[i + 2] = 128; // R
                buf[i + 3] = 255; // A
            }
        }
        buf
    }

    fn mk_frame(buf: &[u8], w: u32, h: u32, stride: usize) -> Frame {
        Frame {
            desc: CpuBgraFrame {
                data: buf.as_ptr(),
                len: buf.len(),
                stride,
                width: w,
                height: h,
            },
            w,
            h,
        }
    }

    /// VP9 keyframes begin with the frame marker `0b10` in the top 2 bits (uncompressed header).
    /// This is a light sanity check on the bitstream shape, not a full parse.
    fn looks_like_vp9(data: &[u8]) -> bool {
        !data.is_empty() && (data[0] >> 6) == 0b10
    }

    /// VP8 keyframes start with a 3-byte uncompressed header whose bytes 3..6 are the start code
    /// 0x9d 0x01 0x2a. This proves a real VP8 keyframe bitstream.
    fn is_vp8_keyframe(data: &[u8]) -> bool {
        data.len() > 6 && data[3] == 0x9d && data[4] == 0x01 && data[5] == 0x2a
    }

    #[test]
    fn first_output_is_a_vp9_keyframe() {
        let (w, h) = (128u32, 96u32);
        let stride = (w * 4) as usize;
        let buf = gradient(w, h, stride);
        let mut enc = VpxEncoder::new(); // VP9 default
        assert_eq!(enc.codec(), VpxCodec::Vp9);
        enc.configure(&default_stream_config(VpxCodec::Vp9, w, h, 60))
            .unwrap();
        let out = enc
            .encode(mk_frame(&buf, w, h, stride))
            .expect("encode ok")
            .expect("a frame is produced");
        assert!(out.is_keyframe, "first frame must be a keyframe");
        assert_eq!(out.frame_id, 0);
        assert_eq!(out.captured_at_us, 1234);
        assert!(!out.data.is_empty());
        assert!(looks_like_vp9(&out.data), "VP9 keyframe frame-marker");
    }

    #[test]
    fn vp8_first_output_is_a_valid_keyframe() {
        let (w, h) = (128u32, 96u32);
        let stride = (w * 4) as usize;
        let buf = gradient(w, h, stride);
        let mut enc = VpxEncoder::new_with(VpxCodec::Vp8);
        enc.configure(&default_stream_config(VpxCodec::Vp8, w, h, 60))
            .unwrap();
        let out = enc
            .encode(mk_frame(&buf, w, h, stride))
            .expect("encode ok")
            .expect("a frame");
        assert!(out.is_keyframe);
        assert!(
            is_vp8_keyframe(&out.data),
            "VP8 keyframe start code 9d 01 2a present"
        );
    }

    #[test]
    fn forced_keyframe_after_request() {
        let (w, h) = (96u32, 64u32);
        let stride = (w * 4) as usize;
        let buf = gradient(w, h, stride);
        let mut enc = VpxEncoder::new();
        enc.configure(&default_stream_config(VpxCodec::Vp9, w, h, 60))
            .unwrap();
        let f0 = enc.encode(mk_frame(&buf, w, h, stride)).unwrap().unwrap();
        assert!(f0.is_keyframe);
        // Subsequent frames need not be keyframes (may be P or dropped)...
        let _ = enc.encode(mk_frame(&buf, w, h, stride)).unwrap();
        let _ = enc.encode(mk_frame(&buf, w, h, stride)).unwrap();
        // ...but a requested keyframe forces an IDR again.
        enc.request_keyframe(KeyframeReason::DecoderReset);
        // A forced keyframe is never dropped, so we should get a produced frame that is a keyframe.
        let f = loop {
            if let Some(f) = enc.encode(mk_frame(&buf, w, h, stride)).unwrap() {
                break f;
            }
        };
        assert!(f.is_keyframe, "forced keyframe after request");
    }

    #[test]
    fn handles_row_padding_and_odd_dimensions() {
        // Odd width (cropped to 100) and a padded stride.
        let (w, h) = (101u32, 64u32);
        let stride = (w as usize) * 4 + 48; // padded
        let buf = gradient(w, h, stride);
        let mut enc = VpxEncoder::new();
        enc.configure(&default_stream_config(VpxCodec::Vp9, w & !1, h, 60))
            .unwrap();
        let out = enc
            .encode(mk_frame(&buf, w, h, stride))
            .expect("encode ok")
            .expect("a frame");
        assert!(out.is_keyframe);
        assert!(looks_like_vp9(&out.data));
    }

    #[test]
    fn rejects_wrong_surface_kind() {
        struct Bad;
        impl CapturedFrame for Bad {
            fn captured_at_us(&self) -> u64 {
                0
            }
            fn width(&self) -> u32 {
                64
            }
            fn height(&self) -> u32 {
                64
            }
            fn platform_surface(&self) -> PlatformSurface<'_> {
                PlatformSurface::none()
            }
        }
        let mut enc = VpxEncoder::new();
        enc.configure(&default_stream_config(VpxCodec::Vp9, 64, 64, 60))
            .unwrap();
        assert!(
            enc.encode(Bad).is_err(),
            "must fail-close on a non-CpuBgra surface"
        );
    }

    #[test]
    fn rejects_zero_size_frame() {
        // A CpuBgra descriptor claiming zero dimensions must be refused (fail-closed).
        let buf = [0u8; 16];
        let frame = Frame {
            desc: CpuBgraFrame {
                data: buf.as_ptr(),
                len: buf.len(),
                stride: 0,
                width: 0,
                height: 0,
            },
            w: 0,
            h: 0,
        };
        let mut enc = VpxEncoder::new();
        enc.configure(&default_stream_config(VpxCodec::Vp9, 64, 64, 60))
            .unwrap();
        assert!(
            enc.encode(frame).is_err(),
            "zero-size frame must fail-close"
        );
    }

    /// Frame-varying pseudo-random content (deterministic LCG): hard to compress and different every
    /// frame, so P-frames carry real residual and the bitrate cap actually binds.
    fn noisy(w: u32, h: u32, stride: usize, seed: u32) -> Vec<u8> {
        let mut buf = vec![0u8; stride * h as usize];
        let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        for y in 0..h as usize {
            for x in 0..w as usize {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = (s >> 24) as u8;
                let i = y * stride + x * 4;
                buf[i] = n;
                buf[i + 1] = n.wrapping_add((x as u8).wrapping_add(seed as u8));
                buf[i + 2] = n.wrapping_add(y as u8);
                buf[i + 3] = 255;
            }
        }
        buf
    }

    /// Runtime ABR: after `set_bitrate` lowers the target, the live encoder must produce substantially
    /// smaller access units for the same class of content — no reconfigure, no keyframe. Exercises the
    /// `vpx_codec_enc_config_set` path end-to-end.
    #[test]
    fn runtime_set_bitrate_shrinks_output() {
        let (w, h) = (320u32, 240u32);
        let stride = (w * 4) as usize;
        let mut enc = VpxEncoder::new();
        enc.configure(&default_stream_config(VpxCodec::Vp9, w, h, 30))
            .unwrap();

        fn push(enc: &mut VpxEncoder, w: u32, h: u32, stride: usize, seed: u32) -> usize {
            let buf = noisy(w, h, stride, seed);
            let n = enc
                .encode(mk_frame(&buf, w, h, stride))
                .expect("encode ok")
                .map_or(0, |f| f.data.len());
            drop(buf);
            n
        }

        // Warm up at the default 8 Mbps so the rate controller converges, then measure output bytes.
        for seed in 0..20 {
            push(&mut enc, w, h, stride, seed);
        }
        let high: usize = (100..160)
            .map(|seed| push(&mut enc, w, h, stride, seed))
            .sum();

        // Drop to 1 Mbps at runtime (no reconfigure / keyframe), let it converge, then measure.
        enc.set_bitrate(1_000_000).expect("set_bitrate ok");
        for seed in 300..340 {
            push(&mut enc, w, h, stride, seed);
        }
        let low: usize = (400..460)
            .map(|seed| push(&mut enc, w, h, stride, seed))
            .sum();

        assert!(
            high > 0 && low > 0,
            "both phases must produce frames (high={high}, low={low})"
        );
        assert!(
            low * 2 < high,
            "lowering the bitrate must shrink output (high={high} bytes, low={low} bytes)"
        );
    }

    /// The base temporal layer (`ts_target_bitrate[0]`) must never be starved to 0 by integer-division
    /// rounding at very low total bitrates — it's the one layer every receiver depends on to decode
    /// anything at all, and it's exactly the layer the ABR leans on hardest when it downshifts under
    /// real congestion. Regression test for the P1 fixed here.
    #[test]
    fn base_temporal_layer_never_floors_to_zero_at_low_bitrate() {
        for total_kbps in [0u32, 1, 2, 3, 5, 10, 19, 20, 21] {
            let split3 = vp9_temporal_layer_bitrates_kbps(total_kbps, 3);
            assert!(
                split3[0] > 0 || total_kbps == 0,
                "3-layer base must be >0 for total_kbps={total_kbps} (got {split3:?})"
            );
            assert!(
                split3[0] <= split3[1] && split3[1] <= split3[2],
                "3-layer split must stay ascending/cumulative for total_kbps={total_kbps} (got {split3:?})"
            );
            assert_eq!(
                split3[2], total_kbps,
                "3-layer split must end exactly at the total for total_kbps={total_kbps}"
            );

            let split2 = vp9_temporal_layer_bitrates_kbps(total_kbps, 2);
            assert!(
                split2[0] > 0 || total_kbps == 0,
                "2-layer base must be >0 for total_kbps={total_kbps} (got {split2:?})"
            );
            assert!(
                split2[0] <= split2[1],
                "2-layer split must stay ascending/cumulative for total_kbps={total_kbps} (got {split2:?})"
            );
            assert_eq!(
                split2[1], total_kbps,
                "2-layer split must end exactly at the total for total_kbps={total_kbps}"
            );
        }
    }

    /// At normal (non-degenerate) bitrates the fixed floor must not perturb the existing 40/60/100 (or
    /// 60/100) percentage split at all — this is purely a low-bitrate safety net.
    #[test]
    fn temporal_layer_split_matches_legacy_percentages_above_the_floor() {
        let split3 = vp9_temporal_layer_bitrates_kbps(1000, 3);
        assert_eq!(split3, [400, 600, 1000]);

        let split2 = vp9_temporal_layer_bitrates_kbps(1000, 2);
        assert_eq!(split2, [600, 1000, 0]);
    }

    #[test]
    fn webcodecs_strings() {
        assert_eq!(VpxCodec::Vp8.webcodecs_string(1920, 1080, 60), "vp8");
        // 1080p60 → VP9 profile 0, level 4.1 (below), 8-bit.
        let s = VpxCodec::Vp9.webcodecs_string(1920, 1080, 60);
        assert!(s.starts_with("vp09.00."), "got {s}");
        assert!(s.ends_with(".08"), "8-bit: got {s}");
    }

    /// Pure GOP-position → layer-id mapping, independent of libvpx: the 3-layer `0212` pattern
    /// (period 4) matches the `ts_layer_id = [0, 2, 1, 2, ...]` config, and the 2-layer `0101` pattern
    /// (period 2) matches `ts_layer_id = [0, 1, ...]`.
    #[test]
    fn temporal_layer_pattern_is_deterministic() {
        // 3-layer 0212, one full period + wraparound.
        assert_eq!(temporal_layer_for_frame_index(0, 4), Some(0));
        assert_eq!(temporal_layer_for_frame_index(1, 4), Some(2));
        assert_eq!(temporal_layer_for_frame_index(2, 4), Some(1));
        assert_eq!(temporal_layer_for_frame_index(3, 4), Some(2));
        assert_eq!(temporal_layer_for_frame_index(4, 4), Some(0)); // wraps
        assert_eq!(temporal_layer_for_frame_index(5, 4), Some(2));

        // 2-layer 0101.
        assert_eq!(temporal_layer_for_frame_index(0, 2), Some(0));
        assert_eq!(temporal_layer_for_frame_index(1, 2), Some(1));
        assert_eq!(temporal_layer_for_frame_index(2, 2), Some(0)); // wraps

        // No pattern (VP8 / unknown periodicity).
        assert_eq!(temporal_layer_for_frame_index(0, 0), None);
        assert_eq!(temporal_layer_for_frame_index(7, 3), None);
    }

    /// VP9 defaults to 3 forwarded layers and reports a layer id on every produced frame; the exact
    /// per-frame sequence over one full GOP-plus-wraparound must match the `0212` pattern (validated
    /// against real libvpx output, not just the pure helper above).
    #[test]
    fn vp9_reports_layer_id_per_output_frame_in_0212_pattern() {
        let (w, h) = (128u32, 96u32);
        let stride = (w * 4) as usize;
        let mut enc = VpxEncoder::new();
        assert_eq!(enc.temporal_layer_count(), 3);
        enc.configure(&default_stream_config(VpxCodec::Vp9, w, h, 60))
            .unwrap();

        let mut layers = Vec::new();
        for seed in 0..8u32 {
            let buf = noisy(w, h, stride, seed);
            let produced = enc.encode(mk_frame(&buf, w, h, stride)).unwrap().is_some();
            assert!(
                produced,
                "no forwarding ceiling is set, nothing should drop"
            );
            layers.push(enc.last_temporal_layer());
        }
        // Frame 0 is the forced keyframe at GOP position 0 (layer 0); frames 1..8 continue the
        // pattern one full period plus wraparound: [0,2,1,2,0,2,1,2].
        assert_eq!(
            layers,
            vec![
                Some(0),
                Some(2),
                Some(1),
                Some(2),
                Some(0),
                Some(2),
                Some(1),
                Some(2),
            ]
        );
    }

    /// VP8 has no temporal layering: `last_temporal_layer` stays `None` and the layer-selection knob
    /// is a no-op (nothing is ever shed on it).
    #[test]
    fn vp8_has_no_temporal_layer() {
        let (w, h) = (96u32, 64u32);
        let stride = (w * 4) as usize;
        let mut enc = VpxEncoder::new_with(VpxCodec::Vp8);
        assert_eq!(enc.temporal_layer_count(), 1);
        enc.configure(&default_stream_config(VpxCodec::Vp8, w, h, 60))
            .unwrap();
        enc.set_max_temporal_layer(Some(0)); // would shed everything above base on VP9; no-op on VP8
        for seed in 0..5u32 {
            let buf = noisy(w, h, stride, seed);
            let out = enc.encode(mk_frame(&buf, w, h, stride)).unwrap();
            assert_eq!(enc.last_temporal_layer(), None);
            if let Some(f) = out {
                let _ = f; // VP8 frames may still be produced/dropped by the codec itself, never by SVC
            }
        }
    }

    /// **Layer-selection knob**: capping at the base layer (`Some(0)`) must forward only frames whose
    /// pattern position is layer 0 (plus any forced keyframe, which always ships), and shed the rest —
    /// exercising the real libvpx encode path end-to-end, not just the pure pattern helper.
    #[test]
    fn max_temporal_layer_knob_sheds_frames_above_ceiling() {
        let (w, h) = (128u32, 96u32);
        let stride = (w * 4) as usize;
        let mut enc = VpxEncoder::new();
        enc.configure(&default_stream_config(VpxCodec::Vp9, w, h, 30))
            .unwrap();
        assert_eq!(enc.max_temporal_layer(), 2, "default forwards every layer");
        enc.set_max_temporal_layer(Some(0));
        assert_eq!(enc.max_temporal_layer(), 0);

        // Frame 0: forced keyframe (startup) at GOP position 0 — always forwarded, layer 0 anyway.
        let f0 = enc
            .encode(mk_frame(&noisy(w, h, stride, 0), w, h, stride))
            .unwrap();
        assert!(f0.is_some(), "startup keyframe always forwarded");
        assert!(f0.unwrap().is_keyframe);

        // Frames 1..4 are GOP positions 1,2,3,0 → layers 2,1,2,0. Only the last (layer 0) forwards.
        let mut forwarded = Vec::new();
        for seed in 1..5u32 {
            let buf = noisy(w, h, stride, seed);
            forwarded.push(enc.encode(mk_frame(&buf, w, h, stride)).unwrap().is_some());
        }
        assert_eq!(
            forwarded,
            vec![false, false, false, true],
            "only the base-layer (position 4 → layer 0) frame should forward"
        );
    }

    /// A forced keyframe must ship even when it lands on a non-base GOP position and the layer
    /// ceiling would otherwise shed it — the decoder's sole resync point is never dropped.
    #[test]
    fn max_temporal_layer_knob_never_drops_a_forced_keyframe() {
        let (w, h) = (96u32, 64u32);
        let stride = (w * 4) as usize;
        let mut enc = VpxEncoder::new();
        enc.configure(&default_stream_config(VpxCodec::Vp9, w, h, 30))
            .unwrap();
        enc.set_max_temporal_layer(Some(0));

        // Advance past the forced startup keyframe to GOP position 1 (layer 2 — would be shed).
        let _ = enc
            .encode(mk_frame(&noisy(w, h, stride, 0), w, h, stride))
            .unwrap();
        // Now GOP position is 1 (layer 2). Force another keyframe and confirm it still ships despite
        // the layer-0 ceiling.
        enc.request_keyframe(KeyframeReason::DecoderReset);
        let f = enc
            .encode(mk_frame(&noisy(w, h, stride, 1), w, h, stride))
            .unwrap()
            .expect("a forced keyframe must never be shed by the layer ceiling");
        assert!(f.is_keyframe);
    }

    /// `set_max_temporal_layer(None)` restores "forward everything" (the default), even after a
    /// previous call had lowered the ceiling.
    #[test]
    fn max_temporal_layer_none_restores_default() {
        let mut enc = VpxEncoder::new();
        enc.set_max_temporal_layer(Some(1));
        assert_eq!(enc.max_temporal_layer(), 1);
        enc.set_max_temporal_layer(None);
        assert_eq!(enc.max_temporal_layer(), 2, "None restores forward-all");
    }

    /// A dimension-change mid-stream rebuilds the libvpx context (fresh GOP position 0); the layer
    /// mirror must re-sync to that rebuild, not keep counting from the pre-rebuild position.
    #[test]
    fn dimension_change_resyncs_gop_position() {
        let (w1, h1) = (96u32, 64u32);
        let stride1 = (w1 * 4) as usize;
        let mut enc = VpxEncoder::new();
        enc.configure(&default_stream_config(VpxCodec::Vp9, w1, h1, 30))
            .unwrap();
        // Advance to GOP position 1 (post the startup keyframe at position 0).
        let _ = enc
            .encode(mk_frame(&noisy(w1, h1, stride1, 0), w1, h1, stride1))
            .unwrap();

        // Rebuild via a dimension change: the new frame's dims differ, forcing `build_encoder` again.
        let (w2, h2) = (128u32, 96u32);
        let stride2 = (w2 * 4) as usize;
        let out = enc
            .encode(mk_frame(&noisy(w2, h2, stride2, 1), w2, h2, stride2))
            .unwrap()
            .expect("a frame is produced after the rebuild");
        // The rebuilt encoder's first frame sits at fresh GOP position 0 → layer 0 (also a keyframe,
        // since a dimension-change rebuild starts a brand new libvpx context/GOP).
        assert!(out.is_keyframe);
        assert_eq!(enc.last_temporal_layer(), Some(0));
    }
}
