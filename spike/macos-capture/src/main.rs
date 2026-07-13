//! Casual RAS — Phase-S macOS capture spike (throwaway).
//!
//! Measures the **capture half** of the ScreenCaptureKit → VideoToolbox path that `docs/18 §8`
//! flags as still-unmeasured (the WebCodecs GO covered *decode* only): does SCStream deliver a
//! stable frame cadence, and what does it cost to get pixels out? Reports delivered FPS, frame
//! cadence (wall-clock inter-arrival + SCK presentation-timestamp interval), and the per-frame
//! lock+touch cost of the delivered `CVPixelBuffer`.
//!
//! This is the pull-from-a-push-delegate shape `ScreenCaptureKitBackend` will use. The encode half
//! (VTCompressionSession → Annex-B) is the next spike slice.
//!
//! Bindings: the pure-Rust **objc2** framework crates (no Swift bridge / no build-time SDK codegen),
//! which is also the family the real `ras-media-macos` backend will use.
//!
//! Run (on a Mac with a GUI session + Screen-Recording permission — NOT over SSH/headless):
//!   cargo run -p macos-capture-probe            # 60 fps, 5 s
//!   cargo run -p macos-capture-probe -- 30 8    # <fps> <seconds>

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};

/// `kCVPixelFormatType_32BGRA` — a FourCC packed as an OSType (u32). 4 bytes/pixel, one plane.
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// `kCVReturnSuccess`.
const CV_RETURN_SUCCESS: i32 = 0;

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

/// The instance state our SCK output/delegate object carries.
struct OutputIvars {
    stats: Arc<Stats>,
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
    fn new(stats: Arc<Stats>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { stats });
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
    let output = CaptureOutput::new(stats.clone());
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

    report(&stats, secs);
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
