//! Casual RAS — Phase-S macOS capture→encode spike (throwaway).
//!
//! Measures the whole host video head that `docs/18 §8` flags as unmeasured (the WebCodecs GO
//! covered *decode/render* only): **capture** (ScreenCaptureKit) → **encode** (VideoToolbox H.264).
//!
//! - Capture: does SCStream deliver a stable cadence, and what does it cost to touch pixels?
//!   Reports delivered FPS, cadence (wall inter-arrival + SCK presentation-timestamp interval), and
//!   per-frame `CVPixelBuffer` lock+touch cost.
//! - Encode: each captured `CVPixelBuffer` is fed to a low-latency `VTCompressionSession`
//!   (realtime, no B-frames, Baseline, ~infinite GOP). Reports per-frame **encode latency**,
//!   encoded FPS, keyframe count, and mean encoded frame size, and writes an Annex-B `.h264`
//!   elementary stream (SPS/PPS + start-code framing) for external playback verification.
//!
//! This is the pull-from-a-push-delegate + encoder shape `ScreenCaptureKitBackend` /
//! `VideoToolboxEncoder` will use.
//!
//! Bindings: the pure-Rust **objc2** framework crates (no Swift bridge / no build-time SDK codegen),
//! which is also the family the real `ras-media-macos` backend will use.
//!
//! Run (on a Mac with a GUI session + Screen-Recording permission — NOT over SSH/headless):
//!   cargo run -p macos-capture-probe            # 60 fps, 5 s → ./capture.h264
//!   cargo run -p macos-capture-probe -- 30 8    # <fps> <seconds>

use std::ffi::{c_char, c_void};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::{
    kCFBooleanFalse, kCFBooleanTrue, CFBoolean, CFNumber, CFNumberType, CFRetained,
};
use objc2_core_media::{
    kCMVideoCodecType_H264, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTProfileLevel_H264_Baseline_AutoLevel, VTCompressionSession, VTEncodeInfoFlags, VTSession,
    VTSessionSetProperty,
};

/// `kCVPixelFormatType_32BGRA` — a FourCC packed as an OSType (u32). 4 bytes/pixel, one plane.
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// `kCVReturnSuccess`.
const CV_RETURN_SUCCESS: i32 = 0;
/// `kVTEncodeInfo_FrameDropped` — bit set in the encoder's info flags when a frame was dropped.
const VT_ENCODE_INFO_FRAME_DROPPED: u32 = 1 << 1;
/// Spike encode bitrate target (8 Mbps) — realistic for a full-screen desktop feed.
const ENCODE_BITRATE: i32 = 8_000_000;
/// Output elementary-stream path (Annex-B), written alongside the working directory.
const OUT_PATH: &str = "capture.h264";
/// H.264 NAL type for an IDR (keyframe) slice.
const NAL_TYPE_IDR: u8 = 5;
/// VideoToolbox emits H.264 in AVCC framing with a 4-byte big-endian length prefix per NAL.
const AVCC_LENGTH_PREFIX: usize = 4;
/// Annex-B NAL start code.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Content-free per-frame telemetry the delegate accumulates (runs on SCK's private queue).
#[derive(Default)]
struct Stats {
    arrivals: Mutex<Vec<Instant>>, // wall-clock arrival of each Screen frame
    pts_secs: Mutex<Vec<f64>>,     // SCK presentation timestamp (seconds), for cadence-from-source
    dims: Mutex<Option<(usize, usize, usize)>>, // (width, height, bytes_per_row) of the first frame
    copy_us: Mutex<Vec<u64>>,      // lock + touch cost per frame (microseconds)
    frames: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn lock_push<T>(m: &Mutex<Vec<T>>, v: T) {
    lock(m).push(v);
}

/// Content-free encoder telemetry, shared with the VideoToolbox output callback via a raw refcon.
#[derive(Default)]
struct EncodeStats {
    file: Mutex<Option<BufWriter<File>>>,
    latency_us: Mutex<Vec<u64>>, // frame-in → callback-out, per delivered frame
    wrote_params: AtomicBool,    // SPS/PPS written once at stream head
    frames_out: AtomicU64,
    keyframes: AtomicU64,
    bytes_out: AtomicU64,
}

impl EncodeStats {
    fn write_all(&self, bytes: &[u8]) {
        if let Some(f) = lock(&self.file).as_mut() {
            let _ = f.write_all(bytes);
        }
        self.bytes_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    }
}

/// A low-latency H.264 `VTCompressionSession`. Feeds captured `CVPixelBuffer`s and streams encoded
/// Annex-B out through the C output callback below. Holds the shared `EncodeStats` (whose pointer
/// the callback dereferences) alive for the session's whole lifetime.
struct Encoder {
    session: CFRetained<VTCompressionSession>,
    stats: Arc<EncodeStats>,
}

impl Encoder {
    fn new(width: i32, height: i32, fps: i32) -> Result<Self, String> {
        let file = File::create(OUT_PATH).map_err(|e| format!("create {OUT_PATH}: {e}"))?;
        let stats = Arc::new(EncodeStats {
            file: Mutex::new(Some(BufWriter::new(file))),
            ..EncodeStats::default()
        });

        // SAFETY: valid dims; NULL specification/attributes; `on_encoded` reads the refcon (a live
        // `Arc<EncodeStats>` pointer held in `self.stats`); out-param written on success.
        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width,
                height,
                kCMVideoCodecType_H264,
                None,
                None,
                None,
                Some(on_encoded),
                Arc::as_ptr(&stats) as *mut c_void,
                NonNull::from(&mut raw),
            )
        };
        let session = NonNull::new(raw)
            .filter(|_| status == 0)
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or_else(|| format!("VTCompressionSessionCreate failed (OSStatus {status})"))?;

        let enc = Encoder { session, stats };
        enc.configure(fps)?;
        Ok(enc)
    }

    /// Low-latency H.264: realtime, no frame reordering (no B-frames), Baseline, ~infinite GOP so a
    /// single IDR sits at the head (recovery keyframes are requested on demand in the real pipeline).
    fn configure(&self, fps: i32) -> Result<(), String> {
        // SAFETY: keys are framework constants; values live across the set call.
        unsafe {
            self.set_bool(kVTCompressionPropertyKey_RealTime, true)?;
            self.set_bool(kVTCompressionPropertyKey_AllowFrameReordering, false)?;
            self.set_property(
                kVTCompressionPropertyKey_ProfileLevel,
                Some(kVTProfileLevel_H264_Baseline_AutoLevel),
            )?;
            self.set_i32(kVTCompressionPropertyKey_MaxKeyFrameInterval, i32::MAX)?;
            self.set_i32(kVTCompressionPropertyKey_ExpectedFrameRate, fps)?;
            self.set_i32(kVTCompressionPropertyKey_AverageBitRate, ENCODE_BITRATE)?;
        }
        Ok(())
    }

    /// SAFETY: `value` must outlive the call; `key` is a framework property constant.
    unsafe fn set_property(
        &self,
        key: &objc2_core_foundation::CFString,
        value: Option<&objc2_core_foundation::CFType>,
    ) -> Result<(), String> {
        // A VTCompressionSessionRef is a VTSessionRef in the C API — cast the CF pointer.
        let session = &*(CFRetained::as_ptr(&self.session).as_ptr() as *const VTSession);
        let status = VTSessionSetProperty(session, key, value);
        if status == 0 {
            Ok(())
        } else {
            Err(format!("VTSessionSetProperty failed (OSStatus {status})"))
        }
    }

    unsafe fn set_bool(
        &self,
        key: &objc2_core_foundation::CFString,
        v: bool,
    ) -> Result<(), String> {
        let b: Option<&CFBoolean> = if v { kCFBooleanTrue } else { kCFBooleanFalse };
        self.set_property(key, b.map(|b| b.as_ref()))
    }

    unsafe fn set_i32(
        &self,
        key: &objc2_core_foundation::CFString,
        v: i32,
    ) -> Result<(), String> {
        let n = CFNumber::new(None, CFNumberType::SInt32Type, &v as *const i32 as *const c_void)
            .ok_or("CFNumberCreate failed")?;
        self.set_property(key, Some(n.as_ref()))
    }

    /// Feed one captured frame. `pts` is SCK's presentation timestamp; the per-frame refcon carries
    /// the enqueue `Instant` so the callback can measure encode latency.
    fn encode(&self, image: &CVImageBuffer, pts: CMTime) {
        let started = Box::into_raw(Box::new(Instant::now())) as *mut c_void;
        let mut flags = VTEncodeInfoFlags(0);
        // Duration unknown → kCMTimeInvalid (all-zero flags).
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        };
        // SAFETY: session + image are live; refcon is a leaked Box the callback reclaims. If the
        // encode call itself errors, VT won't invoke the callback, so we reclaim the Box here.
        let status = unsafe {
            self.session
                .encode_frame(image, pts, invalid, None, started, &mut flags)
        };
        if status != 0 {
            // SAFETY: `started` is the Box we just leaked and VT never took ownership.
            drop(unsafe { Box::from_raw(started as *mut Instant) });
        }
    }

    /// Flush pending frames and tear the session down.
    fn finish(&self) {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        };
        // SAFETY: session is live; completes all frames, then invalidates.
        unsafe {
            let _ = self.session.complete_frames(invalid);
            self.session.invalidate();
        }
        if let Some(mut f) = lock(&self.stats.file).take() {
            let _ = f.flush();
        }
    }
}

/// VideoToolbox output callback. Runs on a VT encode thread. Reclaims the per-frame `Instant`,
/// records encode latency, and writes the frame out as Annex-B (SPS/PPS once at head).
///
/// SAFETY: matches `VTCompressionOutputCallback`. `output_ref_con` is the `Arc<EncodeStats>` pointer
/// held alive by `Encoder`; `source_ref_con` is a `Box<Instant>` leaked by `encode`.
unsafe extern "C-unwind" fn on_encoded(
    output_ref_con: *mut c_void,
    source_ref_con: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let started = if source_ref_con.is_null() {
        None
    } else {
        Some(*unsafe { Box::from_raw(source_ref_con as *mut Instant) })
    };
    if output_ref_con.is_null() {
        return;
    }
    let stats = unsafe { &*(output_ref_con as *const EncodeStats) };

    let dropped = flags.0 & VT_ENCODE_INFO_FRAME_DROPPED != 0;
    let Some(sample) = (unsafe { sample.as_ref() }) else {
        return;
    };
    if status != 0 || dropped {
        return;
    }

    // SPS/PPS once at the head of the stream (single-IDR, ~infinite GOP).
    if !stats.wrote_params.swap(true, Ordering::Relaxed) {
        write_parameter_sets(sample, stats);
    }
    write_annexb(sample, stats);

    stats.frames_out.fetch_add(1, Ordering::Relaxed);
    if let Some(t0) = started {
        lock_push(&stats.latency_us, t0.elapsed().as_micros() as u64);
    }
}

/// Emit the H.264 SPS/PPS parameter sets (from the sample's format description) as Annex-B.
fn write_parameter_sets(sample: &CMSampleBuffer, stats: &EncodeStats) {
    // SAFETY: encoded samples carry a video format description holding the H.264 parameter sets.
    let Some(fmt) = (unsafe { sample.format_description() }) else {
        return;
    };
    let mut count: usize = 0;
    // First query the parameter-set count.
    // SAFETY: NULL out-pointers are permitted except the count we request here.
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
        // SAFETY: fmt is retained for this scope; p/size receive an internal pointer + length.
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
        stats.write_all(&START_CODE);
        stats.write_all(nal);
    }
}

/// Convert the sample's AVCC (length-prefixed) NAL units to Annex-B start-code framing and write.
fn write_annexb(sample: &CMSampleBuffer, stats: &EncodeStats) {
    // SAFETY: an encoded sample carries its data in a CMBlockBuffer.
    let Some(bb) = (unsafe { sample.data_buffer() }) else {
        return;
    };
    let mut total: usize = 0;
    let mut data: *mut c_char = ptr::null_mut();
    // SAFETY: contiguous access from offset 0; total length + base pointer returned.
    let rc = unsafe { bb.data_pointer(0, ptr::null_mut(), &mut total, &mut data) };
    if rc != 0 || data.is_null() || total == 0 {
        return;
    }
    // SAFETY: `data` points to `total` valid bytes for the lifetime of `bb`.
    let buf = unsafe { std::slice::from_raw_parts(data as *const u8, total) };

    let mut off = 0usize;
    while off + AVCC_LENGTH_PREFIX <= buf.len() {
        let len = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as usize;
        off += AVCC_LENGTH_PREFIX;
        if len == 0 || off + len > buf.len() {
            break;
        }
        let nal = &buf[off..off + len];
        if nal[0] & 0x1F == NAL_TYPE_IDR {
            stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        stats.write_all(&START_CODE);
        stats.write_all(nal);
        off += len;
    }
}

/// The instance state our SCK output/delegate object carries.
struct OutputIvars {
    stats: Arc<Stats>,
    encoder: Arc<Encoder>,
}

define_class!(
    // NSObject subclass; SCK never calls it on the main thread, so no MainThreadOnly marker.
    #[unsafe(super(NSObject))]
    #[name = "RASCaptureOutput"]
    #[ivars = OutputIvars]
    struct CaptureOutput;

    unsafe impl NSObjectProtocol for CaptureOutput {}

    unsafe impl SCStreamOutput for CaptureOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            of_type: SCStreamOutputType,
        ) {
            if of_type.0 != SCStreamOutputType::Screen.0 {
                return;
            }
            self.handle_screen_frame(sample);
        }
    }

    unsafe impl SCStreamDelegate for CaptureOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop(&self, _stream: &SCStream, _error: &NSError) {
            // Content-free: we never log the error object (it can name windows/apps).
        }
    }
);

impl CaptureOutput {
    fn new(stats: Arc<Stats>, encoder: Arc<Encoder>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { stats, encoder });
        unsafe { msg_send![super(this), init] }
    }

    /// Runs on SCK's sample-handler queue. Records cadence + the cost of touching pixels.
    fn handle_screen_frame(&self, sample: &CMSampleBuffer) {
        let stats = &self.ivars().stats;
        let now = Instant::now();
        stats.frames.fetch_add(1, Ordering::Relaxed);
        lock_push(&stats.arrivals, now);

        // SAFETY: valid sample buffer handed to us by SCK for the duration of this callback.
        let pts = unsafe { sample.presentation_time_stamp() };
        if pts.timescale != 0 {
            lock_push(&stats.pts_secs, pts.value as f64 / f64::from(pts.timescale));
        }

        // SAFETY: `image_buffer()` returns the frame's CVPixelBuffer (BGRA, one plane) or None.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };

        // Feed the encoder from the same CVImageBuffer (VideoToolbox reads it asynchronously).
        self.ivars().encoder.encode(&image, pts);

        let pixels: &CVPixelBuffer = &image; // CVImageBuffer and CVPixelBuffer are the same type.

        // Lock read-only, then measure the base-address touch — the real per-frame extraction cost.
        // SAFETY: matched lock/unlock around the read; flags identical on both calls.
        let rc = unsafe { CVPixelBufferLockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) };
        if rc != CV_RETURN_SUCCESS {
            return;
        }
        let (w, h, bpr) = (
            CVPixelBufferGetWidth(pixels),
            CVPixelBufferGetHeight(pixels),
            CVPixelBufferGetBytesPerRow(pixels),
        );
        {
            let mut d = lock(&stats.dims);
            if d.is_none() {
                *d = Some((w, h, bpr));
            }
        }
        let base = CVPixelBufferGetBaseAddress(pixels);
        if !base.is_null() && bpr > 0 && h > 0 {
            let len = bpr.saturating_mul(h);
            let t = Instant::now();
            // SAFETY: base points to `bpr*h` locked bytes; read-only, sparse stride, freed on unlock.
            let px = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
            let mut acc = 0u64;
            let mut i = 0;
            while i < px.len() {
                acc = acc.wrapping_add(u64::from(px[i]));
                i += 4096;
            }
            std::hint::black_box(acc);
            lock_push(&stats.copy_us, t.elapsed().as_micros() as u64);
        }
        // SAFETY: pairs the successful lock above with identical flags.
        unsafe { CVPixelBufferUnlockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) };
    }
}

/// Synchronously fetch the shareable content (SCK's API is completion-handler based).
fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        // SAFETY: SCK hands back a +0 autoreleased content (or a +0 error) for this callback.
        let result = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(c),
            None => {
                let msg = unsafe { err.as_ref() }
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "SCShareableContent unavailable".to_string());
                Err(msg)
            }
        };
        let _ = tx.send(result);
    });
    // SAFETY: passes a live block; SCK invokes it once on an internal queue.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| "timed out waiting for SCShareableContent (permission prompt pending?)".to_string())?
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fps: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let secs: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let content = shareable_content()?;
    // SAFETY: `content` is live; `displays()` returns its display array.
    let displays = unsafe { content.displays() };
    let display = displays.firstObject().ok_or("no display available")?;
    let (dw, dh) = unsafe { (display.width() as usize, display.height() as usize) };
    println!("display: {dw}x{dh}  |  target: {fps} fps for {secs}s  |  format: BGRA");

    // Whole-display filter, exclude nothing.
    let empty: Retained<objc2_foundation::NSArray<objc2_screen_capture_kit::SCWindow>> =
        objc2_foundation::NSArray::new();
    // SAFETY: init consumes a fresh alloc; `display` + `empty` outlive the call.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &empty,
        )
    };

    // SAFETY: fresh config; setters take plain scalars.
    let config = unsafe {
        let c = SCStreamConfiguration::new();
        c.setWidth(dw);
        c.setHeight(dh);
        c.setPixelFormat(PIXEL_FORMAT_BGRA);
        c.setShowsCursor(true);
        c.setMinimumFrameInterval(CMTime::new(1, fps));
        c
    };

    let stats = Arc::new(Stats::default());
    // Shared with SCK's delegate queue: sound because VideoToolbox sessions are thread-safe and the
    // stats are Mutex-guarded; only `CFRetained` lacks the Send/Sync marker.
    #[allow(clippy::arc_with_non_send_sync)]
    let encoder = Arc::new(Encoder::new(dw as i32, dh as i32, fps)?);
    let output = CaptureOutput::new(stats.clone(), encoder.clone());
    let delegate = ProtocolObject::from_ref(&*output);

    // SAFETY: filter/config/delegate all outlive the stream construction call.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(delegate),
        )
    };

    let queue = DispatchQueue::new("com.casualras.capture", None);
    let sc_output = ProtocolObject::from_ref(&*output);
    // SAFETY: registers our output for Screen frames on a dedicated serial queue.
    unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                sc_output,
                SCStreamOutputType::Screen,
                Some(&*queue),
            )
            .map_err(|e| format!("addStreamOutput: {}", e.localizedDescription()))?;
    }

    start_capture_blocking(&stream)?;
    std::thread::sleep(Duration::from_secs(secs));
    stop_capture_blocking(&stream);
    encoder.finish(); // flush pending frames + close the .h264 file

    report(&stats, secs);
    report_encode(&encoder.stats);
    Ok(())
}

/// Start capture and block until SCK reports success/failure (surfaces the TCC permission error).
fn start_capture_blocking(stream: &SCStream) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |err: *mut NSError| {
        let msg = unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(msg);
    });
    // SAFETY: block is live for the call; SCK invokes it once when start settles.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&*handler)) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(None) => Ok(()),
        Ok(Some(msg)) => Err(format!(
            "start_capture failed (Screen-Recording permission granted?): {msg}"
        )),
        Err(_) => Err("timed out starting capture".into()),
    }
}

fn stop_capture_blocking(stream: &SCStream) {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |_err: *mut NSError| {
        let _ = tx.send(());
    });
    // SAFETY: block is live for the call; SCK invokes it once when stop settles.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&*handler)) };
    let _ = rx.recv_timeout(Duration::from_secs(5));
}

fn report(stats: &Stats, secs: u64) {
    let arrivals = lock(&stats.arrivals).clone();
    let n = arrivals.len();
    if n < 2 {
        println!(
            "\nonly {n} frame(s) captured — expected many. A headless session or missing \
             Screen-Recording permission is the usual cause (capture needs a real GUI login)."
        );
        return;
    }

    // Wall-clock delivered FPS + inter-arrival cadence.
    let wall = arrivals[n - 1].duration_since(arrivals[0]).as_secs_f64();
    let delivered_fps = (n - 1) as f64 / wall.max(f64::EPSILON);
    let mut gaps: Vec<f64> = arrivals
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0)
        .collect();

    println!("\n  ScreenCaptureKit capture — {n} frames over {wall:.2}s (asked {secs}s)");
    println!("  {:-<58}", "");
    println!("  delivered FPS                     {delivered_fps:>8.1}");
    print_pctl("inter-arrival gap ms (wall)", &mut gaps);

    // Cadence from SCK's own presentation timestamps (independent of our scheduling).
    let pts = lock(&stats.pts_secs).clone();
    if pts.len() >= 2 {
        let mut pts_gaps: Vec<f64> = pts.windows(2).map(|w| (w[1] - w[0]) * 1000.0).collect();
        print_pctl("frame interval ms (SCK pts)", &mut pts_gaps);
    }

    // Per-frame pixel-extraction cost.
    let mut copy: Vec<f64> = lock(&stats.copy_us).iter().map(|&u| u as f64).collect();
    if !copy.is_empty() {
        print_pctl("lock+touch us/frame", &mut copy);
    }

    if let Some((w, h, bpr)) = *lock(&stats.dims) {
        println!("  frame                             {w}x{h}, {bpr} bytes/row");
    }
    println!("\n  Record delivered FPS + the pts interval (cadence stability) in phase-S-design.md §4.1.");
}

fn report_encode(stats: &EncodeStats) {
    let frames = stats.frames_out.load(Ordering::Relaxed);
    let keyframes = stats.keyframes.load(Ordering::Relaxed);
    let bytes = stats.bytes_out.load(Ordering::Relaxed);
    let mut lat: Vec<f64> = lock(&stats.latency_us).iter().map(|&u| u as f64).collect();

    println!("\n  VideoToolbox H.264 encode — {frames} frames out, {keyframes} keyframe(s)");
    println!("  {:-<58}", "");
    if lat.is_empty() {
        println!("  no frames encoded — see errors above.");
        return;
    }
    for x in &mut lat {
        *x /= 1000.0; // µs → ms
    }
    print_pctl("encode latency ms/frame", &mut lat);
    let mean_kb = if frames > 0 {
        bytes as f64 / frames as f64 / 1024.0
    } else {
        0.0
    };
    println!("  mean encoded frame size          {mean_kb:>8.1} KB");
    println!("  total encoded                     {:>8.1} KB → {OUT_PATH}", bytes as f64 / 1024.0);
    println!(
        "\n  Verify playback: ffplay {OUT_PATH}  (or: ffmpeg -i {OUT_PATH} -c copy out.mp4)"
    );
    println!("  Record encode latency (med/p95) in phase-S-design.md §4.1.");
}

/// Print median / p95 / max of `xs` (mutates: sorts in place).
fn print_pctl(label: &str, xs: &mut [f64]) {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |p: f64| xs[(((xs.len() - 1) as f64) * p).round() as usize];
    println!(
        "  {label:<33} med {:>7.2}  p95 {:>7.2}  max {:>7.2}",
        at(0.50),
        at(0.95),
        xs[xs.len() - 1]
    );
}
