//! VideoToolbox H.264 encoder behind [`ras_media::VideoEncoderBackend`].
//!
//! Low-latency profile (matches `docs/10` + the validated spike): realtime, **no B-frames**
//! (no reorder latency), Baseline AutoLevel, ~infinite GOP with **forced-IDR-on-demand** as the
//! sole keyframe mechanism, CBR retargetable mid-stream. Output is a complete **Annex-B** access
//! unit per frame with **SPS+PPS re-sent in-band on every IDR** (the [`EncodedFrame`] contract —
//! any keyframe is self-contained, no out-of-band `description`).
//!
//! The encode call is synchronous: it submits the frame, forces completion, and drains the one
//! resulting access unit produced on VideoToolbox's callback thread. Pipelined (async) emission is
//! a later latency optimisation; correctness/order first.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::{BufMut, Bytes, BytesMut};
use objc2_core_foundation::{
    kCFBooleanFalse, kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString,
    CFType,
};
use objc2_core_media::{
    kCMVideoCodecType_H264, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Baseline_AutoLevel,
    VTCompressionSession, VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
};
use ras_media::{
    CapturedFrame, EncodedFrame, FrameId, MediaError, StreamConfig, SurfaceKind,
    VideoEncoderBackend,
};
use ras_protocol::{ErrorCode, KeyframeReason, RasError};

use crate::{Sendable, START_CODE};

/// H.264 NAL type for an IDR (keyframe) slice.
const NAL_TYPE_IDR: u8 = 5;
/// VideoToolbox emits AVCC framing with a 4-byte big-endian length prefix per NAL.
const AVCC_LENGTH_PREFIX: usize = 4;
/// `kVTEncodeInfo_FrameDropped`.
const VT_ENCODE_INFO_FRAME_DROPPED: u32 = 1 << 1;
/// `kCMTimeInvalid` — all-zero flags.
const CM_TIME_INVALID: CMTime = CMTime {
    value: 0,
    timescale: 0,
    flags: CMTimeFlags(0),
    epoch: 0,
};

fn enc_fatal(context: &'static str) -> MediaError {
    RasError::fatal(ErrorCode::EncoderFailed, context)
}
fn os_ok(status: i32, context: &'static str) -> Result<(), MediaError> {
    if status == 0 {
        Ok(())
    } else {
        Err(enc_fatal(context))
    }
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One emitted access unit + whether it is an IDR. Frame id / timestamps are attached by `encode`
/// from the input frame (the callback has no access to them).
type EmittedUnit = (Bytes, bool);

/// Shared with the VideoToolbox output callback via a raw refcon.
#[derive(Default)]
struct EncOut {
    units: Mutex<VecDeque<EmittedUnit>>,
    errored: AtomicBool,
}

/// Hardware-preferred H.264 encoder. Zero-copy `CVPixelBuffer` in, Annex-B out.
pub struct VideoToolboxEncoder {
    session: Option<Sendable<CFRetained<VTCompressionSession>>>,
    out: Arc<EncOut>,
    config: StreamConfig,
    next_frame_id: FrameId,
    force_keyframe: bool,
    first: bool,
}

impl VideoToolboxEncoder {
    /// New, unconfigured encoder. [`VideoEncoderBackend::configure`] must run before `encode`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            session: None,
            out: Arc::new(EncOut::default()),
            // VideoToolbox always produces H.264; the negotiated codec (if VP9/VP8) is served by the
            // software libvpx encoder instead, so this backend's placeholder config is H.264.
            config: super::default_stream_config(0, 0, 0, ras_media::VideoCodec::H264AnnexB),
            next_frame_id: 0,
            force_keyframe: false,
            first: true,
        }
    }

    fn session(&self) -> Result<&VTCompressionSession, MediaError> {
        self.session
            .as_ref()
            .map(|s| &***s)
            .ok_or_else(|| enc_fatal("encoder not configured"))
    }

    /// Set one VTSession property (CF value). A `VTCompressionSessionRef` is a `VTSessionRef`.
    fn set_property(&self, key: &CFString, value: Option<&CFType>) -> Result<(), MediaError> {
        let sess = self.session()?;
        // SAFETY: toll-compatible CF pointer cast (VTCompressionSessionRef ⊂ VTSessionRef); key is a
        // framework constant; `value` outlives the call.
        let status = unsafe {
            VTSessionSetProperty(&*(ptr::from_ref(sess) as *const VTSession), key, value)
        };
        os_ok(status, "VTSessionSetProperty failed")
    }

    fn set_i32(&self, key: &CFString, v: i32) -> Result<(), MediaError> {
        // SAFETY: reads a live i32; CFNumberCreate copies it.
        let n = unsafe { CFNumber::new(None, CFNumberType::SInt32Type, ptr::from_ref(&v).cast()) }
            .ok_or_else(|| enc_fatal("CFNumberCreate failed"))?;
        self.set_property(key, Some(n.as_ref()))
    }

    fn set_bool(&self, key: &CFString, v: bool) -> Result<(), MediaError> {
        // Boolean VT keys need the CFBoolean singleton (a CFNumber would be wrong). We set both true
        // and false explicitly (VideoToolbox defaults AllowFrameReordering *on* if left unset).
        // SAFETY: immutable CFBoolean framework singletons.
        let b = unsafe {
            if v {
                kCFBooleanTrue
            } else {
                kCFBooleanFalse
            }
        }
        .ok_or_else(|| enc_fatal("kCFBoolean null"))?;
        self.set_property(key, Some(b.as_ref()))
    }
}

impl Default for VideoToolboxEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoEncoderBackend for VideoToolboxEncoder {
    fn configure(&mut self, config: &StreamConfig) -> Result<(), MediaError> {
        // Build a fresh session for the requested geometry.
        let out = Arc::new(EncOut::default());
        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: valid dims; NULL specification/attributes; `on_encoded` reads the refcon (the live
        // `Arc<EncOut>` held in `self.out`); session written to `raw` on success.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                kCMVideoCodecType_H264,
                None,
                None,
                None,
                Some(on_encoded),
                Arc::as_ptr(&out) as *mut c_void,
                ptr::NonNull::from(&mut raw),
            )
        };
        let session = ptr::NonNull::new(raw)
            .filter(|_| status == 0)
            // SAFETY: `create` returns a +1 VTCompressionSession on success.
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| enc_fatal("VTCompressionSessionCreate failed"))?;

        // SAFETY: single-thread ownership after the backend is moved to the media thread; VT sessions
        // are additionally documented thread-safe.
        self.session = Some(unsafe { Sendable::new(session) });
        self.out = out;
        self.config = *config;
        self.next_frame_id = 0;
        self.force_keyframe = false;
        self.first = true;

        // SAFETY: immutable framework property-key / profile string constants (extern statics).
        let (k_realtime, k_reorder, k_profile, prof_baseline, k_maxgop, k_fps, k_bitrate) = unsafe {
            (
                kVTCompressionPropertyKey_RealTime,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Baseline_AutoLevel,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                kVTCompressionPropertyKey_AverageBitRate,
            )
        };
        self.set_bool(k_realtime, true)?;
        self.set_bool(k_reorder, false)?;
        self.set_property(k_profile, Some(prof_baseline.as_ref()))?;
        self.set_i32(k_maxgop, i32::MAX)?;
        self.set_i32(k_fps, config.fps.max(1) as i32)?;
        self.set_i32(k_bitrate, clamp_bitrate(config.target_bitrate_bps))?;
        Ok(())
    }

    fn encode<F: CapturedFrame>(&mut self, frame: F) -> Result<Option<EncodedFrame>, MediaError> {
        if self.out.errored.load(Ordering::Relaxed) {
            return Err(enc_fatal("encoder faulted"));
        }
        let surface = frame.platform_surface();
        let ptr = surface
            .as_ptr(SurfaceKind::MacCoreVideoPixelBuffer)
            .ok_or_else(|| enc_fatal("captured frame carries no macOS surface"))?;
        // SAFETY: paired same-platform capture (ADR-058); the pointer is a live CVImageBuffer valid
        // for the whole `encode` call (frame owns the retained buffer, consumed by value here).
        let image: &CVImageBuffer = unsafe { &*(ptr.as_ptr() as *const CVImageBuffer) };

        let force = self.first || self.force_keyframe;
        self.first = false;
        self.force_keyframe = false;

        // SAFETY: constructs a CMTime value from a count + timescale; no pointer/UB surface.
        let pts = unsafe { CMTime::new(frame.captured_at_us() as i64, 1_000_000) };
        let props = if force {
            Some(force_keyframe_props()?)
        } else {
            None
        };

        let sess = self.session()?;
        let mut flags = VTEncodeInfoFlags(0);
        // SAFETY: session + image live; NULL source refcon; frame properties (if any) live across
        // the call.
        let status = unsafe {
            sess.encode_frame(
                image,
                pts,
                CM_TIME_INVALID,
                props.as_deref(),
                ptr::null_mut(),
                &mut flags,
            )
        };
        os_ok(status, "VTCompressionSessionEncodeFrame failed")?;
        // Force synchronous emission of the just-submitted frame.
        // SAFETY: session live.
        let _ = unsafe { sess.complete_frames(CM_TIME_INVALID) };

        let Some((data, is_keyframe)) = lock(&self.out.units).pop_front() else {
            return Ok(None);
        };
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;
        Ok(Some(EncodedFrame {
            frame_id,
            captured_at_us: frame.captured_at_us(),
            is_keyframe,
            data,
            config: self.config,
        }))
    }

    fn request_keyframe(&mut self, _reason: KeyframeReason) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        // SAFETY: immutable framework property-key constant.
        let key = unsafe { kVTCompressionPropertyKey_AverageBitRate };
        self.set_i32(key, clamp_bitrate(bitrate_bps))?;
        self.config.target_bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn config(&self) -> StreamConfig {
        self.config
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        if let Some(s) = self.session.as_ref() {
            // SAFETY: session live; invalidate releases encoder resources.
            unsafe { s.invalidate() };
        }
    }
}

/// A one-entry `{ ForceKeyFrame: true }` frame-properties dictionary.
fn force_keyframe_props() -> Result<CFRetained<CFDictionary>, MediaError> {
    // SAFETY: immutable framework constants — the option-key string, the CFBoolean singleton, and the
    // two standard CF callback structs (addresses of extern statics).
    let (fk_key, val_true, kcb, vcb) = unsafe {
        (
            kVTEncodeFrameOptionKey_ForceKeyFrame,
            kCFBooleanTrue,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    };
    let val_true = val_true.ok_or_else(|| enc_fatal("kCFBooleanTrue null"))?;
    let key: *const c_void = (fk_key as *const CFString).cast();
    let val: *const c_void = (val_true as *const objc2_core_foundation::CFBoolean).cast();
    let mut keys = [key];
    let mut vals = [val];
    // SAFETY: 1-entry parallel arrays; standard CF type callbacks (retain/release/equal) applied.
    let dict =
        unsafe { CFDictionary::new(None, keys.as_mut_ptr(), vals.as_mut_ptr(), 1, kcb, vcb) };
    dict.ok_or_else(|| enc_fatal("CFDictionaryCreate failed"))
}

/// Clamp an ABR target to a sane VideoToolbox range and `i32`.
fn clamp_bitrate(bps: u32) -> i32 {
    bps.clamp(100_000, 50_000_000) as i32
}

/// VideoToolbox output callback. Runs on a VT encode thread. Converts the sample's AVCC NAL units to
/// Annex-B, prepends SPS/PPS on every IDR, and enqueues the access unit.
///
/// SAFETY: matches `VTCompressionOutputCallback`. `output_ref_con` is the `Arc<EncOut>` pointer held
/// alive by the encoder; `source_ref_con` is unused (NULL).
unsafe extern "C-unwind" fn on_encoded(
    output_ref_con: *mut c_void,
    _source_ref_con: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if output_ref_con.is_null() {
        return;
    }
    let out = unsafe { &*(output_ref_con as *const EncOut) };

    let dropped = flags.0 & VT_ENCODE_INFO_FRAME_DROPPED != 0;
    let Some(sample) = (unsafe { sample.as_ref() }) else {
        if status != 0 {
            out.errored.store(true, Ordering::Relaxed);
        }
        return;
    };
    if status != 0 {
        out.errored.store(true, Ordering::Relaxed);
        return;
    }
    if dropped {
        return;
    }

    if let Some(unit) = build_access_unit(sample) {
        lock(&out.units).push_back(unit);
    }
}

/// Build one Annex-B access unit from an encoded sample: SPS/PPS (from the format description) on an
/// IDR, followed by the sample's slice NALs converted from AVCC to start-code framing.
fn build_access_unit(sample: &CMSampleBuffer) -> Option<EmittedUnit> {
    // SAFETY: an encoded sample carries its slice data in a CMBlockBuffer.
    let bb = unsafe { sample.data_buffer() }?;
    let mut total: usize = 0;
    let mut data: *mut c_char = ptr::null_mut();
    // SAFETY: contiguous access from offset 0; total length + base pointer returned.
    let rc = unsafe { bb.data_pointer(0, ptr::null_mut(), &mut total, &mut data) };
    if rc != 0 || data.is_null() || total == 0 {
        return None;
    }
    // SAFETY: `data` points to `total` valid bytes for the lifetime of `bb`.
    let buf = unsafe { std::slice::from_raw_parts(data as *const u8, total) };

    let is_keyframe = scan_has_idr(buf);
    let mut au = BytesMut::with_capacity(total + 64);
    if is_keyframe {
        append_parameter_sets(sample, &mut au);
    }
    let mut off = 0usize;
    while off + AVCC_LENGTH_PREFIX <= buf.len() {
        let len = be_u32(&buf[off..]) as usize;
        off += AVCC_LENGTH_PREFIX;
        if len == 0 || off + len > buf.len() {
            break;
        }
        au.put_slice(&START_CODE);
        au.put_slice(&buf[off..off + len]);
        off += len;
    }
    Some((au.freeze(), is_keyframe))
}

/// Is any NAL in this AVCC buffer an IDR slice (type 5)?
fn scan_has_idr(buf: &[u8]) -> bool {
    let mut off = 0usize;
    while off + AVCC_LENGTH_PREFIX <= buf.len() {
        let len = be_u32(&buf[off..]) as usize;
        off += AVCC_LENGTH_PREFIX;
        if len == 0 || off + len > buf.len() {
            break;
        }
        if buf[off] & 0x1F == NAL_TYPE_IDR {
            return true;
        }
        off += len;
    }
    false
}

/// Append the H.264 SPS/PPS parameter sets (from the sample's format description) as Annex-B.
fn append_parameter_sets(sample: &CMSampleBuffer, au: &mut BytesMut) {
    // SAFETY: encoded samples carry a video format description holding the parameter sets.
    let Some(fmt) = (unsafe { sample.format_description() }) else {
        return;
    };
    let mut count: usize = 0;
    // SAFETY: query the parameter-set count (NULL pointer/size out-params are permitted).
    let rc = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            &fmt,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            ptr::null_mut(),
        )
    };
    if rc != 0 {
        return;
    }
    for i in 0..count {
        let mut p: *const u8 = ptr::null();
        let mut size: usize = 0;
        // SAFETY: fmt retained for this scope; p/size receive an internal pointer + length.
        let rc = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                &fmt,
                i,
                &mut p,
                &mut size,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != 0 || p.is_null() || size == 0 {
            continue;
        }
        // SAFETY: p points to `size` bytes owned by fmt, valid while fmt is retained.
        let nal = unsafe { std::slice::from_raw_parts(p, size) };
        au.put_slice(&START_CODE);
        au.put_slice(nal);
    }
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
