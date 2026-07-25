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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use ras_media::{
        CaptureOptions, CapturedFrame, ColorSpace, CpuBgraFrame, MediaError, PlatformSurface,
        RemoteDisplayBounds, StreamConfig, SurfaceKind, VideoCodec, VideoTransportKind,
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

    /// Why the capture thread stopped before delivering a first frame. `start` maps this to a
    /// typed, content-free `MediaError` instead of a bare startup timeout, so a declined portal is
    /// distinguishable from a slow-but-alive one.
    #[derive(Clone, Copy)]
    enum StartupFailure {
        /// Building the OS capturer failed *or panicked* — on Linux this is the
        /// xdg-desktop-portal picker being declined/cancelled or the portal being unavailable
        /// (scap's `LinuxCapturer::new` `expect`s the portal call, so a decline unwinds here).
        Declined,
        /// scap returned a clean `CapturerBuildError` (unsupported / permission not granted).
        Unavailable,
        /// scap delivered only frames in a format we never requested (we ask for
        /// `FrameType::BGRAFrame`; `to_bgra` handles every `Frame` variant except
        /// `Frame::YUVFrame`, which we deliberately drop rather than mis-encode — see `to_bgra`).
        /// Distinguishes "driver/platform ignored our format request" from a genuinely static
        /// screen, which also produces no frame during the startup wait but for a benign reason.
        UnexpectedFormat,
    }

    /// Shared latest-frame slot between the capture thread and the pump.
    struct Shared {
        slot: Mutex<Option<Buf>>,
        cv: Condvar,
        /// Set by the capture thread if it fails to *build* the capturer before any frame. Read by
        /// `start` after the first-frame wait to produce a precise error. `None` = no build failure
        /// observed (either frames are flowing, or startup genuinely timed out).
        startup_failure: Mutex<Option<StartupFailure>>,
        /// Total frames dropped by `to_bgra` because scap delivered a format we didn't request
        /// (currently only `Frame::YUVFrame` — see `to_bgra`). Diagnostic-only: never gates
        /// behavior, just makes an otherwise-silent "we've been dropping every frame" condition
        /// observable and distinguishable from a genuinely static screen (which legitimately
        /// yields zero frames via the `Ok(None)` timeout path). Logged once (rate-limited) by the
        /// capture loop and consulted by `start` to give a precise startup error.
        unexpected_format_drops: AtomicU64,
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
                config: default_stream_config(1920, 1080, 60, None),
                running: None,
                current: None,
            }
        }
    }

    /// The requested display id (`CaptureOptions.monitor.0`) is a real, platform-native id — on
    /// Windows, `Target::Display.id` (`HMONITOR as u32`, the same value `enumerate_displays()`
    /// reports); on Linux it is unused (see the comment on `enumerate_displays` below). `0` always
    /// means "no explicit pick" and resolves to the platform default (primary).
    const NO_DISPLAY_PICK: u32 = 0;

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

    /// This platform's default codec when the caller (codec negotiation) didn't specify one.
    ///   • Linux → VP9 (`ras-media-vpx`): WebKitGTK can't reliably decode our H.264 but decodes VP9.
    ///   • Windows → H.264 (`ras-media-openh264`): WebView2 decodes H.264 natively; no libvpx there.
    #[must_use]
    fn platform_default_codec() -> VideoCodec {
        #[cfg(target_os = "linux")]
        {
            VideoCodec::Vp9
        }
        #[cfg(not(target_os = "linux"))]
        {
            VideoCodec::H264AnnexB
        }
    }

    #[must_use]
    fn default_stream_config(
        width: u32,
        height: u32,
        fps: u32,
        codec: Option<VideoCodec>,
    ) -> StreamConfig {
        StreamConfig {
            // The declared codec MUST match the paired encoder's bytes (see `make_backends` in the
            // app): the capture declares the negotiated codec (codec negotiation), the encoder
            // produces it, they must never disagree. `None` ⇒ this platform's default.
            codec: codec.unwrap_or_else(platform_default_codec),
            width,
            height,
            fps,
            target_bitrate_bps: 8_000_000,
            color: ColorSpace::Bt709Limited,
            video_transport: VideoTransportKind::PerFrameStream,
        }
    }

    /// Name of the `Frame` variant, for diagnostics only (Inv 8: variant tag, never pixel content).
    fn frame_variant_name(frame: &Frame) -> &'static str {
        match frame {
            Frame::BGRA(_) => "BGRA",
            Frame::BGRx(_) => "BGRx",
            Frame::BGR0(_) => "BGR0",
            Frame::RGBx(_) => "RGBx",
            Frame::XBGR(_) => "XBGR",
            Frame::RGB(_) => "RGB",
            Frame::YUVFrame(_) => "YUVFrame",
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
                startup_failure: Mutex::new(None),
                unexpected_format_drops: AtomicU64::new(0),
            });
            let stop = Arc::new(AtomicBool::new(false));
            let fps = opts.target_fps.max(1);
            let target_id = opts.monitor.0;

            // scap's `Options` embeds `Target` (a raw window/monitor handle) which is `!Send` on
            // Windows, so it can't cross the thread boundary even when `None`. Pass only the `Send`
            // `fps`/`target_id` and build `Options` (looking the id back up via `get_all_targets()`)
            // inside `capture_loop`.
            let thread_shared = shared.clone();
            let thread_stop = stop.clone();
            let handle = std::thread::Builder::new()
                .name("ras-scap-capture".into())
                .spawn(move || capture_loop(fps, target_id, thread_shared, thread_stop))
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
                    // Stamp the negotiated codec (codec negotiation) so the declared config matches
                    // the paired encoder's bytes; `None` ⇒ this platform's default.
                    self.config = default_stream_config(buf.w, buf.h, fps, opts.codec);
                    // Keep the first frame available for the next pull.
                    *shared
                        .slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(buf);
                    shared.cv.notify_one();
                    Ok(self.config)
                }
                None => {
                    // Prefer the capture thread's specific reason (declined / unavailable) over a
                    // bare timeout. This is what turns a portal decline from a process abort into a
                    // clean, surfaced error.
                    let reason = *shared
                        .startup_failure
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // No explicit build/panic failure was recorded, but the capture thread may
                    // still have been alive and producing frames the whole time — just never in
                    // the BGRA format we requested. Surface that distinctly from a bare timeout
                    // (which is indistinguishable from a genuinely static screen) rather than
                    // reporting the generic "no frame within the startup window".
                    let reason = reason.or_else(|| {
                        if shared.unexpected_format_drops.load(Ordering::Relaxed) > 0 {
                            Some(StartupFailure::UnexpectedFormat)
                        } else {
                            None
                        }
                    });
                    self.stop();
                    Err(match reason {
                        Some(StartupFailure::Declined) => {
                            cap_fatal("screen sharing was declined or is unavailable")
                        }
                        Some(StartupFailure::Unavailable) => {
                            cap_fatal("screen capture not supported or permission not granted")
                        }
                        Some(StartupFailure::UnexpectedFormat) => cap_fatal(
                            "screen capture delivered only frames in an unexpected format (not BGRA)",
                        ),
                        None => cap_fatal("no frame within the startup window"),
                    })
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

        /// The captured region's bounds, in the SAME pixel space the viewer normalizes coordinates
        /// against (the canvas is sized to `StreamConfig.width/height`). Origin is assumed `(0, 0)` —
        /// correct for the documented single-display MVP scope — because `scap::targets::Display`
        /// carries no logical-position fields on Linux/Windows (unlike macOS's `SCDisplay.frame()`), so
        /// there is no per-monitor origin to report.
        ///
        /// Before this override the trait default (`None`) meant the host's input backend NEVER
        /// received display bounds on Linux/Windows (no `LifecycleEvent::CaptureGeometry`), so
        /// `set_display_bounds` was never called and XTEST/uinput fell back to the X root window's
        /// queried screen size / the compositor's default axis range — which can differ sharply from
        /// the actual captured pixel dimensions under multi-monitor layouts or display scaling. That
        /// mismatch between "what the viewer normalizes against" and "what the host injects into" is
        /// the reported huge cursor-position/scaling bug on Linux. Reporting the real captured pixel
        /// size here closes that gap.
        fn captured_bounds(&self) -> Option<RemoteDisplayBounds> {
            Some(RemoteDisplayBounds {
                x: 0,
                y: 0,
                width: self.config.width,
                height: self.config.height,
            })
        }

        /// Host-local picker query (ADR-081, Inv 1). **Windows**: real per-monitor geometry — `scap`'s
        /// own `Target::Display` gives id/title/`HMONITOR`, `GetMonitorInfoW` gives the virtual-desktop
        /// position, `get_target_dimensions`/`get_scale_factor` give the physical pixel size + DPI
        /// scale (see `enumerate_displays_windows`). **Linux**: intentionally empty — `scap` reports no
        /// targets at all there (the xdg-desktop-portal picks interactively at capture start, not
        /// programmatically), so there is nothing a picker here could actually make take effect; the
        /// app falls back to a single honest "Display 1" entry rather than offering a choice that
        /// silently wouldn't change anything.
        #[cfg(target_os = "windows")]
        fn enumerate_displays(&self) -> Vec<ras_media::MonitorDef> {
            enumerate_displays_windows()
        }

        fn stop(&mut self) {
            if let Some(mut running) = self.running.take() {
                running.stop.store(true, Ordering::SeqCst);
                running.shared.cv.notify_all();
                // Bounded join (task #15): `capturer.get_next_frame()` blocks on scap's internal
                // channel with no timeout/cancellation the capturer thread can observe mid-wait, so we
                // cannot force an instant stop — but we CAN wait a real, bounded amount for the common
                // case (any screen activity within the window) so `capturer.stop_capture()` actually
                // runs and releases the OS capture session (PipeWire stream / portal session / WGC
                // session) BEFORE the next `start()` builds a second one. Detaching unconditionally (as
                // this used to do) let a still-live old session overlap a freshly-built new one on a
                // fast stop→restart — a real resource leak / potential duplicate-portal-prompt on
                // Linux. `JoinHandle` has no `join_timeout`, so poll `is_finished()`.
                if let Some(handle) = running.handle.take() {
                    let deadline = Instant::now() + STOP_JOIN_TIMEOUT;
                    let finished = wait_until_finished(|| handle.is_finished(), deadline);
                    if finished {
                        let _ = handle.join(); // clean: capturer.stop_capture() has already run
                    } else {
                        // Genuinely stalled (e.g. a static screen with no frame in the window, so the
                        // capture thread is still blocked inside get_next_frame and hasn't reached its
                        // stop check). Content-free (Inv 8): no OS error text. We cannot wait forever
                        // without hanging Stop/Share-restart, so detach here as the last resort — the
                        // thread will still tear down cleanly once a frame does arrive.
                        log::warn!(
                            "scap capture: old capture thread did not stop within {:?} (screen likely static) — detaching; it will still tear down once a frame arrives",
                            STOP_JOIN_TIMEOUT
                        );
                    }
                }
            }
            self.current = None;
        }
    }

    /// How long [`ScapCapture::stop`] waits for the capture thread to observe `stop` and call
    /// `capturer.stop_capture()` before giving up and detaching it (task #15). Long enough to cover a
    /// screen with any real activity (a frame every ~1/fps at minimum); short enough that Stop/a
    /// Share-restart stays responsive rather than hanging on a stalled capturer.
    const STOP_JOIN_TIMEOUT: Duration = Duration::from_millis(1500);

    /// Poll `is_finished` until it reports done or `deadline` passes, returning which happened.
    /// Extracted as a pure function (injectable predicate, no real `JoinHandle`) purely so the timeout
    /// logic itself is unit-testable without spawning a real thread.
    fn wait_until_finished(mut is_finished: impl FnMut() -> bool, deadline: Instant) -> bool {
        while !is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        is_finished()
    }

    /// Record why startup failed and wake any `start` waiter so it fails fast (instead of hitting
    /// the full startup timeout) with a precise, content-free error.
    fn record_startup_failure(shared: &Arc<Shared>, reason: StartupFailure) {
        *shared
            .startup_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
        shared.cv.notify_all();
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

    /// Resolve a requested display id (`CaptureOptions.monitor.0`) to a `scap` capture `Target`, by
    /// matching against a fresh `get_all_targets()` call. Must run on the thread that will consume the
    /// result — `Target` is `!Send` on Windows (it embeds a raw `HMONITOR`), so it cannot be looked up
    /// in `start()` and passed in. `NO_DISPLAY_PICK` or an id with no match resolves to `None` (scap's
    /// own platform default, effectively the primary display — never a hard error over a stale pick).
    ///
    /// On Linux `get_all_targets()` always returns empty (the xdg-desktop-portal picks interactively
    /// when the capturer starts, not programmatically — scap's own Linux target-enumeration is a
    /// deliberate no-op, confirmed by inspecting scap's source), so a Linux `target_id` is always
    /// ignored here; `enumerate_displays()` below reflects that same reality by returning no entries,
    /// so the app never offers a picker that could not actually change anything.
    fn resolve_target(target_id: u32) -> Option<scap::Target> {
        if target_id == NO_DISPLAY_PICK {
            return None;
        }
        scap::get_all_targets()
            .into_iter()
            .find(|t| matches!(t, scap::Target::Display(d) if d.id == target_id))
    }

    /// The capture thread: build the capturer, then push each frame into the latest-frame slot
    /// (drop-old — only the newest matters for a low-latency feed).
    fn capture_loop(fps: u32, target_id: u32, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
        // Built here (not passed in) because `Options`/`Target` is `!Send` on Windows.
        let options = Options {
            fps,
            // Composite the OS cursor into the captured frames: the controller sees the host's real
            // cursor in the video — ONE cursor, no soft-cursor overlay. (The out-of-band cursor-shape
            // channel + client-side soft cursor was a needless complication that regressed the
            // experience; keep it simple.)
            show_cursor: true,
            show_highlight: false,
            target: resolve_target(target_id), // `None` = platform default (primary)
            crop_area: None,
            output_type: FrameType::BGRAFrame,
            output_resolution: Resolution::Captured,
            excluded_targets: None,
        };
        // `Capturer::build` can *panic* rather than return `Err` when the OS capturer setup fails —
        // notably on Linux, where scap's `LinuxCapturer::new` `expect`s the xdg-desktop-portal call,
        // so a user declining/cancelling the screen-selection dialog (or an unavailable portal)
        // unwinds here. That unwind happens on *this* thread (`Capturer::build` runs synchronously
        // on the caller), so `catch_unwind` contains it and we can fail closed instead of the panic
        // propagating to the thread boundary and aborting the Share. scap's own `is_supported`/
        // `has_permission` gates are hard-coded `true` on Linux, so they can't pre-empt this — the
        // panic guard is the only reliable defense on the portal path.
        let build =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Capturer::build(options)));
        let mut capturer = match build {
            Ok(Ok(c)) => c,
            Ok(Err(_)) => {
                // Clean build error (unsupported / permission not granted).
                record_startup_failure(&shared, StartupFailure::Unavailable);
                return;
            }
            Err(_) => {
                // Panic during build (declined/cancelled/unavailable portal). The unwind payload is
                // not logged — it could carry OS strings; Inv 8 keeps content out of logs.
                record_startup_failure(&shared, StartupFailure::Declined);
                return;
            }
        };
        // `start_capture` can also panic (e.g. scap's engine `expect`s on start). Contain it on this
        // thread and fail closed rather than aborting the process.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| capturer.start_capture()))
            .is_err()
        {
            record_startup_failure(&shared, StartupFailure::Declined);
            return;
        }
        let start = Instant::now();
        while !stop.load(Ordering::SeqCst) {
            match capturer.get_next_frame() {
                Ok(frame) => {
                    let variant = frame_variant_name(&frame);
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
                    } else {
                        // We asked for `FrameType::BGRAFrame`; scap/the OS delivered something
                        // else (currently only possible for `Frame::YUVFrame`, see `to_bgra`).
                        // Dropping it (rather than mis-encoding) is the correct safety call, but
                        // doing so silently is indistinguishable from a genuinely static screen
                        // (which also yields no frame, via the `Ok(None)` timeout path) — so
                        // count it and log the first occurrence at warn (rate-limited: logging
                        // every dropped frame at up to 60fps would flood the log).
                        let prev = shared
                            .unexpected_format_drops
                            .fetch_add(1, Ordering::Relaxed);
                        if prev == 0 {
                            log::warn!(
                                "scap capture: dropped a frame in unexpected format {variant} \
                                 (requested BGRA) — this and any further occurrences are being \
                                 counted; frames will keep being dropped until the capturer \
                                 delivers the requested format"
                            );
                        }
                        shared.cv.notify_one();
                    }
                }
                Err(_) => break, // channel closed
            }
        }
        // Teardown: scap's `stop_capture` can `expect`/`join` internally (Linux joins the pipewire
        // thread). Contain any unwind so shutdown never aborts the process.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| capturer.stop_capture()));
    }

    /// Real per-display geometry on Windows (ADR-081). scap's own `get_scale_factor`/
    /// `get_target_dimensions` helpers are private to that crate, so this reimplements the same GDI
    /// calls scap uses internally: `GetMonitorInfoW` for the virtual-desktop position + pixel rect
    /// (`dwFlags & MONITORINFOF_PRIMARY` for the primary flag) and `GetDpiForMonitor(MDT_EFFECTIVE_DPI)`
    /// for the HiDPI scale. A monitor a GDI call fails on is dropped rather than reported with made-up
    /// geometry. Primary-first, then top-to-bottom / left-to-right (matches the macOS backend's
    /// convention, ADR-081 §"Primary-first by convention").
    #[cfg(target_os = "windows")]
    fn enumerate_displays_windows() -> Vec<ras_media::MonitorDef> {
        use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
        use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

        const BASE_DPI: u32 = 96;
        // `MONITORINFOF_PRIMARY`, inlined to avoid pulling in the `Win32_UI_WindowsAndMessaging`
        // feature for one documented constant (winuser.h: `#define MONITORINFOF_PRIMARY 0x00000001`).
        const MONITORINFOF_PRIMARY: u32 = 1;

        let mut defs: Vec<ras_media::MonitorDef> = scap::get_all_targets()
            .into_iter()
            .filter_map(|t| match t {
                scap::Target::Display(d) => Some(d),
                scap::Target::Window(_) => None,
            })
            .filter_map(|d| {
                let mut info = MONITORINFO {
                    cbSize: core::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                // SAFETY: `d.raw_handle` is a live `HMONITOR` from a just-fetched enumeration; `info`
                // is a correctly-sized, zeroed out-param buffer per the `GetMonitorInfoW` contract.
                let ok = unsafe { GetMonitorInfoW(d.raw_handle, &mut info) }.0 != 0;
                if !ok {
                    return None;
                }

                let (mut dpi_x, mut dpi_y) = (BASE_DPI, BASE_DPI);
                // SAFETY: same live `HMONITOR`; `dpi_x`/`dpi_y` are valid out-params. A failure leaves
                // them at the `BASE_DPI` (100%) fallback set above.
                let _ = unsafe {
                    GetDpiForMonitor(d.raw_handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y)
                };
                let scale_percent = (dpi_x * 100 / BASE_DPI).clamp(1, u32::from(u16::MAX)) as u16;

                let pixel_width = (info.rcMonitor.right - info.rcMonitor.left).max(0) as u32;
                let pixel_height = (info.rcMonitor.bottom - info.rcMonitor.top).max(0) as u32;
                let logical_width = pixel_width * 100 / u32::from(scale_percent);
                let logical_height = pixel_height * 100 / u32::from(scale_percent);

                Some(ras_media::MonitorDef {
                    id: ras_media::MonitorId(d.id),
                    left: info.rcMonitor.left,
                    top: info.rcMonitor.top,
                    logical_width,
                    logical_height,
                    pixel_width,
                    pixel_height,
                    scale_percent,
                    primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                })
            })
            .collect();
        defs.sort_by_key(|d| (!d.primary, d.top, d.left));
        defs
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn wait_until_finished_returns_true_once_the_predicate_flips() {
            let mut calls = 0u32;
            let deadline = Instant::now() + Duration::from_secs(5);
            let finished = wait_until_finished(
                || {
                    calls += 1;
                    calls >= 3
                },
                deadline,
            );
            assert!(
                finished,
                "should report finished once the predicate flips true"
            );
            assert!(calls >= 3);
        }

        #[test]
        fn wait_until_finished_gives_up_at_the_deadline() {
            let deadline = Instant::now() + Duration::from_millis(60);
            let finished = wait_until_finished(|| false, deadline);
            assert!(
                !finished,
                "a predicate that never flips must time out, never hang"
            );
        }

        #[test]
        fn wait_until_finished_is_immediate_when_already_finished() {
            let deadline = Instant::now() + Duration::from_secs(5);
            let start = Instant::now();
            let finished = wait_until_finished(|| true, deadline);
            assert!(finished);
            assert!(
                start.elapsed() < Duration::from_millis(50),
                "an already-finished predicate must not sleep at all"
            );
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub use imp::{ScapCapture, ScapFrame};
