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
            // bitrate must be ascending and end at the total (40/60/100% for 3 layers; 60/100% for 2).
            let total = cfg.rc_target_bitrate;
            match layers {
                3 => {
                    cfg.temporal_layering_mode =
                        ffi::vp9e_temporal_layering_mode::VP9E_TEMPORAL_LAYERING_MODE_0212 as c_int;
                    cfg.ts_periodicity = 4;
                    cfg.ts_layer_id = [0, 2, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                    cfg.ts_rate_decimator[0] = 4;
                    cfg.ts_rate_decimator[1] = 2;
                    cfg.ts_rate_decimator[2] = 1;
                    cfg.ts_target_bitrate[0] = (total * 40) / 100;
                    cfg.ts_target_bitrate[1] = (total * 60) / 100;
                    cfg.ts_target_bitrate[2] = total;
                }
                _ => {
                    cfg.temporal_layering_mode =
                        ffi::vp9e_temporal_layering_mode::VP9E_TEMPORAL_LAYERING_MODE_0101 as c_int;
                    cfg.ts_periodicity = 2;
                    cfg.ts_layer_id = [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                    cfg.ts_rate_decimator[0] = 2;
                    cfg.ts_rate_decimator[1] = 1;
                    cfg.ts_target_bitrate[0] = (total * 60) / 100;
                    cfg.ts_target_bitrate[1] = total;
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
                ffi::VPX_DL_REALTIME as ffi::vpx_enc_deadline_t,
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

        if data.is_empty() {
            // The encoder dropped/coalesced this frame (static screen / SVC decimation) — nothing to
            // send. Do not advance the frame id.
            return Ok(None);
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
                match cfg.ts_number_layers {
                    3 => {
                        cfg.ts_target_bitrate[0] = (total * 40) / 100;
                        cfg.ts_target_bitrate[1] = (total * 60) / 100;
                        cfg.ts_target_bitrate[2] = total;
                    }
                    2 => {
                        cfg.ts_target_bitrate[0] = (total * 60) / 100;
                        cfg.ts_target_bitrate[1] = total;
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

    #[test]
    fn webcodecs_strings() {
        assert_eq!(VpxCodec::Vp8.webcodecs_string(1920, 1080, 60), "vp8");
        // 1080p60 → VP9 profile 0, level 4.1 (below), 8-bit.
        let s = VpxCodec::Vp9.webcodecs_string(1920, 1080, 60);
        assert!(s.starts_with("vp09.00."), "got {s}");
        assert!(s.ends_with(".08"), "8-bit: got {s}");
    }
}
