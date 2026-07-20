//! ScreenCaptureKit capture behind [`ras_media::ScreenCaptureBackend`].
//!
//! SCK is push-based (it calls an `SCStreamOutput` delegate on a private dispatch queue); the trait
//! is pull-based (`next_frame(timeout)`). The adapter bridges the two with a **latest-frame slot**:
//! the delegate stores each arriving frame into a one-slot mailbox (dropping any unconsumed
//! predecessor — video is droppable, and freshest-wins minimises latency), and `next_frame` waits on
//! a condvar up to `timeout`, returning `Ok(None)` on a timeout / static screen. Validated by
//! `spike/macos-capture` (`docs/design/phase-S-design.md §4.1`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{CVImageBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};
use ras_media::{
    CaptureOptions, CaptureTimestampUs, CapturedFrame, MediaError, PlatformSurface,
    RemoteDisplayBounds, ScreenCaptureBackend, StreamConfig, SurfaceKind,
};
use ras_protocol::{ErrorCode, RasError};

use crate::{default_stream_config, Sendable};

/// `kCVPixelFormatType_32BGRA` — a FourCC packed as an OSType (u32).
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A retained CVImageBuffer (== CVPixelBuffer) moved from SCK's queue to the pull thread.
struct SendImage(CFRetained<CVImageBuffer>);
// SAFETY: CoreVideo image/pixel buffers are documented thread-safe for retain/release and read-only
// access; we transfer sole ownership to the pull thread and only ever read the surface there.
unsafe impl Send for SendImage {}

/// One captured frame, owning its retained GPU surface. Owned (no borrow of the backend), matching
/// the synthetic backend's shape.
pub struct MacCapturedFrame {
    image: SendImage,
    captured_at_us: CaptureTimestampUs,
    width: u32,
    height: u32,
}

impl CapturedFrame for MacCapturedFrame {
    fn captured_at_us(&self) -> CaptureTimestampUs {
        self.captured_at_us
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn platform_surface(&self) -> PlatformSurface<'_> {
        // Borrowed pointer to the retained CVPixelBuffer; valid while `self` (and thus the retain)
        // lives — i.e. for the whole `encode` call that consumes the frame (ADR-058).
        let ptr = CFRetained::as_ptr(&self.image.0)
            .as_ptr()
            .cast_const()
            .cast::<c_void>();
        PlatformSurface::from_ptr(ptr, SurfaceKind::MacCoreVideoPixelBuffer)
    }
}

/// One-slot latest-frame mailbox shared between SCK's delegate queue and the pull thread.
#[derive(Default)]
struct Slot {
    frame: Mutex<Option<MacCapturedFrame>>,
    cv: Condvar,
    failed: AtomicBool,
}

struct OutputIvars {
    slot: Arc<Slot>,
}

define_class!(
    // NSObject subclass; SCK calls it on its own queue (not the main thread).
    #[unsafe(super(NSObject))]
    #[name = "RasMacCaptureOutput"]
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
            self.on_frame(sample);
        }
    }

    unsafe impl SCStreamDelegate for CaptureOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop(&self, _stream: &SCStream, _error: &NSError) {
            // Content-free: never log the error object (it can name windows/apps). Mark the stream
            // failed so the pull side surfaces a recoverable error and rebuilds via `start`.
            let slot = &self.ivars().slot;
            slot.failed.store(true, Ordering::Relaxed);
            slot.cv.notify_all();
        }
    }
);

impl CaptureOutput {
    fn new(slot: Arc<Slot>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { slot });
        unsafe { msg_send![super(this), init] }
    }

    /// Runs on SCK's sample-handler queue. Stores the freshest frame (drop-oldest).
    fn on_frame(&self, sample: &CMSampleBuffer) {
        let slot = &self.ivars().slot;
        // SAFETY: valid sample buffer for the callback duration.
        let pts = unsafe { sample.presentation_time_stamp() };
        let captured_at_us = if pts.timescale != 0 {
            (pts.value as i128 * 1_000_000 / i128::from(pts.timescale)) as u64
        } else {
            0
        };
        // SAFETY: a Screen sample carries its frame as a CVImageBuffer (BGRA, one plane) or None.
        let Some(image) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        let width = CVPixelBufferGetWidth(&image) as u32;
        let height = CVPixelBufferGetHeight(&image) as u32;
        let frame = MacCapturedFrame {
            image: SendImage(image),
            captured_at_us,
            width,
            height,
        };
        *lock(&slot.frame) = Some(frame); // freshest-wins; any unconsumed predecessor is dropped
        slot.cv.notify_one();
    }
}

/// ScreenCaptureKit capture source. One monitor, BGRA, pull-based with a latest-frame slot.
pub struct MacScreenCapture {
    stream: Option<Sendable<Retained<SCStream>>>,
    output: Option<Sendable<Retained<CaptureOutput>>>,
    queue: Option<Sendable<DispatchRetained<DispatchQueue>>>,
    slot: Arc<Slot>,
    config: Option<StreamConfig>,
    target_fps: u32,
    /// Captured display's global bounds (logical/points), read from `SCDisplay.frame` at `start`.
    bounds: Option<RemoteDisplayBounds>,
}

impl MacScreenCapture {
    /// New, unstarted backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stream: None,
            output: None,
            queue: None,
            slot: Arc::new(Slot::default()),
            config: None,
            target_fps: 60,
            bounds: None,
        }
    }
}

impl Default for MacScreenCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenCaptureBackend for MacScreenCapture {
    type Frame<'a>
        = MacCapturedFrame
    where
        Self: 'a;

    fn start(&mut self, opts: &CaptureOptions) -> Result<StreamConfig, MediaError> {
        self.stop();
        self.target_fps = opts.target_fps.max(1);

        let content = shareable_content()?;
        // SAFETY: `content` is live; `displays()` returns its display array.
        let displays = unsafe { content.displays() };
        let idx = opts.monitor.0 as usize;
        let display = if idx < displays.count() {
            displays.objectAtIndex(idx)
        } else {
            displays
                .firstObject()
                .ok_or_else(|| RasError::fatal(ErrorCode::CaptureFailed, "no display available"))?
        };
        let (dw, dh) = unsafe { (display.width() as u32, display.height() as u32) };

        // The display's global bounds (points, top-left origin) so the host UI can place its pointer
        // overlay over exactly this display — correct on a secondary monitor, not just the primary.
        // SAFETY: `display` is a live SCDisplay; `frame` reads its CoreGraphics rect.
        let frame = unsafe { display.frame() };
        self.bounds = Some(RemoteDisplayBounds {
            x: frame.origin.x as i32,
            y: frame.origin.y as i32,
            width: frame.size.width.max(0.0) as u32,
            height: frame.size.height.max(0.0) as u32,
        });

        // Exclude our own overlay / consent / indicator windows from capture, matched by CGWindowID.
        // Without this the always-on-top overlay we draw the viewer's remote pointer on would be
        // re-captured and streamed straight back to the viewer (a feedback loop), and the local-only
        // indicator/consent surfaces would leak into the shared feed. An empty list excludes nothing.
        let excluded: Retained<NSArray<SCWindow>> = if opts.excluded_window_ids.is_empty() {
            NSArray::new()
        } else {
            // SAFETY: `content` is live; `windows()` returns its current on-screen window array.
            let all = unsafe { content.windows() };
            let mut keep: Vec<Retained<SCWindow>> = Vec::new();
            for i in 0..all.count() {
                let w = all.objectAtIndex(i);
                // SAFETY: `w` is a live `SCWindow`; `windowID` reads its CoreGraphics id.
                let id = unsafe { w.windowID() } as u64;
                if opts.excluded_window_ids.iter().any(|x| x.0 == id) {
                    keep.push(w);
                }
            }
            NSArray::from_retained_slice(&keep)
        };
        // SAFETY: `display` + `excluded` outlive the init call.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            )
        };

        // SAFETY: fresh config; setters take plain scalars / a CMTime.
        let config = unsafe {
            let c = SCStreamConfiguration::new();
            c.setWidth(dw as usize);
            c.setHeight(dh as usize);
            c.setPixelFormat(PIXEL_FORMAT_BGRA);
            // Do NOT composite the OS cursor into captured frames (ADR-073): the live cursor shape is
            // streamed out-of-band over the dedicated cursor-shape channel (`ras-cursor-macos`'s
            // `CursorObserver` → `ras-core` → controller) and drawn client-side at zero latency. Baking
            // it into the video too would double-draw it and lag behind the out-of-band shape.
            c.setShowsCursor(false);
            c.setMinimumFrameInterval(CMTime::new(1, self.target_fps as i32));
            c.setQueueDepth(3); // small; freshest-wins slot discards backlog anyway
            c
        };

        // Reset the mailbox for the new session.
        *lock(&self.slot.frame) = None;
        self.slot.failed.store(false, Ordering::Relaxed);

        let output = CaptureOutput::new(self.slot.clone());
        let delegate = ProtocolObject::from_ref(&*output);
        // SAFETY: filter/config/delegate outlive the construction call.
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
                    Some(&queue),
                )
                .map_err(|_e| {
                    RasError::fatal(ErrorCode::CaptureFailed, "addStreamOutput failed")
                })?;
        }

        start_capture_blocking(&stream)?;

        // SAFETY (all three): single-thread ownership after the backend is moved to the media thread.
        self.stream = Some(unsafe { Sendable::new(stream) });
        self.output = Some(unsafe { Sendable::new(output) });
        self.queue = Some(unsafe { Sendable::new(queue) });
        let cfg = default_stream_config(dw, dh, self.target_fps);
        self.config = Some(cfg);
        Ok(cfg)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Self::Frame<'_>>, MediaError> {
        if self.slot.failed.load(Ordering::Relaxed) {
            return Err(RasError::recoverable(
                ErrorCode::CaptureFailed,
                "capture stream stopped; restart",
            ));
        }
        let guard = lock(&self.slot.frame);
        let mut guard = if guard.is_some() {
            guard
        } else {
            self.slot
                .cv
                .wait_timeout(guard, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0
        };
        if self.slot.failed.load(Ordering::Relaxed) {
            return Err(RasError::recoverable(
                ErrorCode::CaptureFailed,
                "capture stream stopped; restart",
            ));
        }
        Ok(guard.take())
    }

    fn config(&self) -> StreamConfig {
        self.config
            .unwrap_or_else(|| default_stream_config(0, 0, self.target_fps))
    }

    fn captured_bounds(&self) -> Option<RemoteDisplayBounds> {
        self.bounds
    }

    fn stop(&mut self) {
        if let Some(s) = self.stream.as_ref() {
            stop_capture_blocking(s);
        }
        self.stream = None;
        self.output = None;
        self.queue = None;
        self.bounds = None;
        *lock(&self.slot.frame) = None;
    }
}

/// Synchronously fetch shareable content (SCK's API is completion-handler based).
fn shareable_content() -> Result<Retained<SCShareableContent>, MediaError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, _err: *mut NSError| {
            // SAFETY: SCK hands back a +0 autoreleased content (or NULL + a +0 error) for this callback.
            let _ = tx.send(unsafe { Retained::retain(content) });
        },
    );
    // SAFETY: passes a live block; SCK invokes it once on an internal queue.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    rx.recv_timeout(Duration::from_secs(10))
        .ok()
        .flatten()
        .ok_or_else(|| RasError::fatal(ErrorCode::CaptureFailed, "SCShareableContent unavailable"))
}

/// Start capture and block until SCK reports success/failure (surfaces the TCC permission error).
fn start_capture_blocking(stream: &SCStream) -> Result<(), MediaError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |err: *mut NSError| {
        let _ = tx.send(!err.is_null());
    });
    // SAFETY: block is live for the call; SCK invokes it once when start settles.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(false) => Ok(()),
        Ok(true) => Err(RasError::fatal(
            ErrorCode::CaptureFailed,
            "start_capture failed (Screen-Recording permission?)",
        )),
        Err(_) => Err(RasError::fatal(
            ErrorCode::CaptureFailed,
            "start_capture timed out",
        )),
    }
}

fn stop_capture_blocking(stream: &SCStream) {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |_err: *mut NSError| {
        let _ = tx.send(());
    });
    // SAFETY: block is live for the call; SCK invokes it once when stop settles.
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
    let _ = rx.recv_timeout(Duration::from_secs(5));
}
