//! Cross-platform CPU screen capture implementing [`ras_media::ScreenCaptureBackend`] (ADR-063).
//!
//! Wraps the permissive **`scap`** crate, which selects **PipeWire + xdg-desktop-portal** on Linux,
//! **Windows.Graphics.Capture** on Windows, and ScreenCaptureKit on macOS. It delivers CPU **BGRA**
//! frames, which are handed to the [`ras_media_openh264`](../ras_media_openh264/index.html) software
//! encoder via a [`ras_media::SurfaceKind::CpuBgra`] surface. macOS production still uses the
//! zero-copy `ras-media-macos` backend; `scap` builds here only so this adapter is compile-checked
//! locally.
//!
//! `scap`'s pull API blocks per frame, so a dedicated thread drains it into a single latest-frame
//! slot; [`ScapCapture::next_frame`] waits on that slot with a timeout — returning `Ok(None)` on a
//! static screen exactly like the macOS push→pull adapter, so the media pump never stalls.
//!
//! On non-scap targets the crate is empty (keeps `cargo build --workspace` green everywhere).

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use ras_media::{
        CaptureOptions, CapturedFrame, ColorSpace, CpuBgraFrame, MediaError, PlatformSurface,
        StreamConfig, SurfaceKind, VideoCodec, VideoTransportKind,
    };
    use ras_protocol::{ErrorCode, RasError};
    use scap::capturer::{Capturer, Options, Resolution};
    use scap::frame::{Frame, FrameType};

    fn cap_fatal(context: &'static str) -> MediaError {
        RasError::fatal(ErrorCode::CaptureFailed, context)
    }

    /// One captured BGRA frame plus the borrowed-surface descriptor pointing into its own buffer.
    struct Buf {
        /// Owns the BGRA allocation that `desc.data` points into. Read only through that raw pointer
        /// (in the encoder), so the compiler can't see the use — keep it alive, don't drop it.
        #[allow(dead_code)]
        data: Vec<u8>,
        desc: CpuBgraFrame,
        w: u32,
        h: u32,
        ts_us: u64,
    }

    // `desc.data` points into `data`'s heap allocation, which is stable across a `Buf` move. A `Buf`
    // is only ever *moved* between the capture thread and the pump (through the mutex slot), never
    // shared, so the self-referential pointer remains valid and access stays single-threaded.
    unsafe impl Send for Buf {}

    impl Buf {
        /// Build from a tightly-packed BGRA byte buffer (`stride = width*4`, byte order B,G,R,A/X).
        fn new(data: Vec<u8>, w: u32, h: u32, ts_us: u64) -> Self {
            let ptr = data.as_ptr();
            let len = data.len();
            Buf {
                desc: CpuBgraFrame {
                    data: ptr,
                    len,
                    stride: (w as usize) * 4,
                    width: w,
                    height: h,
                },
                data,
                w,
                h,
                ts_us,
            }
        }
    }

    /// Shared latest-frame slot between the capture thread and the pump.
    struct Shared {
        slot: Mutex<Option<Buf>>,
        cv: Condvar,
    }

    struct Running {
        shared: Arc<Shared>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    /// scap-backed screen capture.
    pub struct ScapCapture {
        config: StreamConfig,
        running: Option<Running>,
        /// The frame handed out by the most recent `next_frame` (kept alive for its borrow).
        current: Option<Buf>,
    }

    impl Default for ScapCapture {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ScapCapture {
        #[must_use]
        pub fn new() -> Self {
            Self {
                config: default_stream_config(1920, 1080, 60),
                running: None,
                current: None,
            }
        }
    }

    /// A borrowed captured frame; exposes its BGRA buffer as a `CpuBgra` surface.
    pub struct ScapFrame<'a> {
        buf: &'a Buf,
    }

    impl CapturedFrame for ScapFrame<'_> {
        fn captured_at_us(&self) -> u64 {
            self.buf.ts_us
        }
        fn width(&self) -> u32 {
            self.buf.w
        }
        fn height(&self) -> u32 {
            self.buf.h
        }
        fn platform_surface(&self) -> PlatformSurface<'_> {
            PlatformSurface::from_ptr(
                core::ptr::from_ref(&self.buf.desc).cast(),
                SurfaceKind::CpuBgra,
            )
        }
    }

    #[must_use]
    fn default_stream_config(width: u32, height: u32, fps: u32) -> StreamConfig {
        StreamConfig {
            codec: VideoCodec::H264AnnexB,
            width,
            height,
            fps,
            target_bitrate_bps: 8_000_000,
            color: ColorSpace::Bt709Limited,
            video_transport: VideoTransportKind::PerFrameStream,
        }
    }

    /// Normalize a scap frame to a tightly-packed 4-byte **BGRA** buffer (byte order B,G,R,A). The
    /// encoder reads only B,G,R, so any `B,G,R,*` layout is used directly; RGB-order layouts are
    /// byte-swapped. Returns `(data, width, height)`.
    fn to_bgra(frame: Frame) -> Option<(Vec<u8>, u32, u32)> {
        // Swap R/B for RGB-order 4-byte inputs into a fresh BGRA buffer.
        fn swap_rb_4(src: &[u8], w: usize, h: usize, r_at: usize, b_at: usize) -> Vec<u8> {
            let mut out = vec![0u8; w * h * 4];
            let n = (src.len() / 4).min(w * h);
            for i in 0..n {
                let s = i * 4;
                out[s] = src[s + b_at]; // B
                out[s + 1] = src[s + 1]; // G
                out[s + 2] = src[s + r_at]; // R
                out[s + 3] = 255;
            }
            out
        }
        match frame {
            // B,G,R,{A,X,0}: already usable as BGRA (the 4th byte is ignored downstream).
            Frame::BGRA(f) => Some((f.data, f.width as u32, f.height as u32)),
            Frame::BGRx(f) => Some((f.data, f.width as u32, f.height as u32)),
            Frame::BGR0(f) => Some((f.data, f.width as u32, f.height as u32)),
            // R,G,B,X / X,R,G,B style: swap R and B.
            Frame::RGBx(f) => {
                let (w, h) = (f.width as usize, f.height as usize);
                Some((
                    swap_rb_4(&f.data, w, h, 0, 2),
                    f.width as u32,
                    f.height as u32,
                ))
            }
            Frame::XBGR(f) => {
                // bytes: X,B,G,R -> read from offsets 1..4.
                let (w, h) = (f.width as usize, f.height as usize);
                let mut out = vec![0u8; w * h * 4];
                let n = (f.data.len() / 4).min(w * h);
                for i in 0..n {
                    let s = i * 4;
                    out[s] = f.data[s + 1]; // B
                    out[s + 1] = f.data[s + 2]; // G
                    out[s + 2] = f.data[s + 3]; // R
                    out[s + 3] = 255;
                }
                Some((out, f.width as u32, f.height as u32))
            }
            // 3-byte RGB: expand to BGRA.
            Frame::RGB(f) => {
                let (w, h) = (f.width as usize, f.height as usize);
                let mut out = vec![0u8; w * h * 4];
                let n = (f.data.len() / 3).min(w * h);
                for i in 0..n {
                    out[i * 4] = f.data[i * 3 + 2]; // B
                    out[i * 4 + 1] = f.data[i * 3 + 1]; // G
                    out[i * 4 + 2] = f.data[i * 3]; // R
                    out[i * 4 + 3] = 255;
                }
                Some((out, f.width as u32, f.height as u32))
            }
            // YUV isn't requested (we ask for BGRA); drop it rather than mis-encode.
            Frame::YUVFrame(_) => None,
        }
    }

    impl ras_media::ScreenCaptureBackend for ScapCapture {
        type Frame<'a> = ScapFrame<'a>;

        fn start(&mut self, opts: &CaptureOptions) -> Result<StreamConfig, MediaError> {
            self.stop();
            if !scap::is_supported() {
                return Err(cap_fatal("screen capture not supported on this system"));
            }

            let shared = Arc::new(Shared {
                slot: Mutex::new(None),
                cv: Condvar::new(),
            });
            let stop = Arc::new(AtomicBool::new(false));
            let fps = opts.target_fps.max(1);

            // scap's `Options` embeds `Target` (a raw window/monitor handle) which is `!Send` on
            // Windows, so it can't cross the thread boundary even when `None`. Pass only the `Send`
            // `fps` and build `Options` for the primary display inside `capture_loop`.
            let thread_shared = shared.clone();
            let thread_stop = stop.clone();
            let handle = std::thread::Builder::new()
                .name("ras-scap-capture".into())
                .spawn(move || capture_loop(fps, thread_shared, thread_stop))
                .map_err(|_| cap_fatal("failed to spawn capture thread"))?;

            self.running = Some(Running {
                shared: shared.clone(),
                stop,
                handle: Some(handle),
            });

            // Block for the first frame to learn the real dimensions (portal picker may prompt here).
            let first = wait_for_frame(&shared, Duration::from_secs(30));
            match first {
                Some(buf) => {
                    self.config = default_stream_config(buf.w, buf.h, fps);
                    // Keep the first frame available for the next pull.
                    *shared
                        .slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(buf);
                    shared.cv.notify_one();
                    Ok(self.config)
                }
                None => {
                    self.stop();
                    Err(cap_fatal(
                        "no frame within the startup window (capture declined?)",
                    ))
                }
            }
        }

        fn next_frame(&mut self, timeout: Duration) -> Result<Option<Self::Frame<'_>>, MediaError> {
            let shared = self
                .running
                .as_ref()
                .map(|r| r.shared.clone())
                .ok_or_else(|| cap_fatal("capture not started"))?;
            let buf = wait_for_frame(&shared, timeout);
            match buf {
                Some(b) => {
                    self.current = Some(b);
                    // Reborrow the stored buffer for the returned frame.
                    let buf_ref = self
                        .current
                        .as_ref()
                        .ok_or_else(|| cap_fatal("frame vanished"))?;
                    Ok(Some(ScapFrame { buf: buf_ref }))
                }
                None => Ok(None), // static screen / timed out
            }
        }

        fn config(&self) -> StreamConfig {
            self.config
        }

        fn stop(&mut self) {
            if let Some(mut running) = self.running.take() {
                running.stop.store(true, Ordering::SeqCst);
                running.shared.cv.notify_all();
                // Detach: scap's blocking pull can't be interrupted mid-wait, so we don't join. The
                // thread exits when its next frame arrives (or the channel closes) and sees `stop`.
                let _ = running.handle.take();
            }
            self.current = None;
        }
    }

    /// Wait up to `timeout` for a frame to appear in the slot, and take it.
    fn wait_for_frame(shared: &Arc<Shared>, timeout: Duration) -> Option<Buf> {
        let mut slot = shared
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            let (guard, _res) = shared
                .cv
                .wait_timeout_while(slot, timeout, |s| s.is_none())
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot = guard;
        }
        slot.take()
    }

    /// The capture thread: build the capturer, then push each frame into the latest-frame slot
    /// (drop-old — only the newest matters for a low-latency feed).
    fn capture_loop(fps: u32, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
        // Built here (not passed in) because `Options`/`Target` is `!Send` on Windows.
        let options = Options {
            fps,
            show_cursor: true,
            show_highlight: false,
            target: None, // primary display
            crop_area: None,
            output_type: FrameType::BGRAFrame,
            output_resolution: Resolution::Captured,
            excluded_targets: None,
        };
        let mut capturer = match Capturer::build(options) {
            Ok(c) => c,
            Err(_) => {
                // Wake any startup waiter so `start` fails fast instead of hitting the full timeout.
                shared.cv.notify_all();
                return;
            }
        };
        capturer.start_capture();
        let start = Instant::now();
        while !stop.load(Ordering::SeqCst) {
            match capturer.get_next_frame() {
                Ok(frame) => {
                    if let Some((data, w, h)) = to_bgra(frame) {
                        if w == 0 || h == 0 {
                            continue;
                        }
                        let ts_us = start.elapsed().as_micros() as u64;
                        let buf = Buf::new(data, w, h, ts_us);
                        let mut slot = shared
                            .slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        *slot = Some(buf); // drop-old
                        drop(slot);
                        shared.cv.notify_one();
                    }
                }
                Err(_) => break, // channel closed
            }
        }
        capturer.stop_capture();
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub use imp::{ScapCapture, ScapFrame};
