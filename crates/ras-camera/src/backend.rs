//! Concrete camera capture over `nokhwa` (AVFoundation / Media Foundation / V4L2), behind the
//! `capture` feature. `nokhwa::Camera` holds non-`Send` platform handles, so — like `ras-mic`'s cpal
//! stream — the camera is created and driven entirely on a dedicated worker thread; each decoded RGB
//! frame is converted to BGRA and published to a **latest-wins** slot (a stalled consumer drops old
//! frames, never builds latency — priority #2). The pull-based `next_frame` takes the latest.

use crate::convert::{rgb8_to_bgra, CameraBuf, CameraFrame};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::Camera;
use ras_media::{
    CameraCaptureBackend, CameraDef, CameraFacing, CameraId, CameraOptions, ColorSpace, MediaError,
    StreamConfig, VideoCodec, VideoTransportKind,
};
use ras_protocol::ErrorCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A raw BGRA frame shared worker→consumer. Deliberately holds **no raw pointer** (unlike
/// [`CameraBuf`], whose self-referential `CpuBgraFrame` pointer is `!Send`) so it crosses the thread
/// boundary safely; the consumer builds the borrowed `CameraBuf` from it in `next_frame`.
struct RawFrame {
    bgra: Vec<u8>,
    width: u32,
    height: u32,
    ts_us: u64,
}

type Slot = Arc<(Mutex<Option<RawFrame>>, Condvar)>;

fn fail(msg: &'static str) -> MediaError {
    MediaError::recoverable(ErrorCode::CaptureFailed, msg)
}

/// A live camera capture. The `nokhwa::Camera` is owned by the worker thread; this struct is `Send`.
pub struct NokhwaCameraCapture {
    slot: Slot,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    latest: Option<CameraBuf>,
    config: StreamConfig,
    started_at: Instant,
}

impl NokhwaCameraCapture {
    /// Build an unstarted capture. Call [`CameraCaptureBackend::start`] to open the device.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: Arc::new((Mutex::new(None), Condvar::new())),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            latest: None,
            config: default_config(1280, 720, 30),
            started_at: Instant::now(),
        }
    }

    fn teardown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Default for NokhwaCameraCapture {
    fn default() -> Self {
        Self::new()
    }
}

fn default_config(width: u32, height: u32, fps: u32) -> StreamConfig {
    StreamConfig {
        codec: VideoCodec::H264AnnexB,
        width,
        height,
        fps,
        target_bitrate_bps: 2_000_000,
        color: ColorSpace::Bt709Limited,
        video_transport: VideoTransportKind::PerFrameStream,
    }
}

impl CameraCaptureBackend for NokhwaCameraCapture {
    type Frame<'a>
        = CameraFrame<'a>
    where
        Self: 'a;

    fn start(&mut self, opts: &CameraOptions) -> Result<StreamConfig, MediaError> {
        self.teardown();
        self.stop = Arc::new(AtomicBool::new(false));
        self.slot = Arc::new((Mutex::new(None), Condvar::new()));

        let index = match &opts.device {
            Some(CameraId(s)) => match s.parse::<u32>() {
                Ok(n) => CameraIndex::Index(n),
                Err(_) => CameraIndex::String(s.clone()),
            },
            None => CameraIndex::Index(0),
        };
        let codec = opts.codec.unwrap_or(VideoCodec::H264AnnexB);
        let config = StreamConfig {
            codec,
            ..default_config(opts.target_width, opts.target_height, opts.target_fps)
        };

        let (tx, rx) = mpsc::channel::<Result<(), MediaError>>();
        let stop = Arc::clone(&self.stop);
        let slot = Arc::clone(&self.slot);
        let worker = std::thread::Builder::new()
            .name("ras-camera".into())
            .spawn(move || run_capture(index, slot, stop, tx))
            .map_err(|_| fail("could not spawn camera thread"))?;

        match rx.recv() {
            Ok(Ok(())) => {
                self.worker = Some(worker);
                self.config = config;
                self.started_at = Instant::now();
                Ok(config)
            }
            Ok(Err(e)) => {
                let _ = worker.join();
                Err(e)
            }
            Err(_) => {
                let _ = worker.join();
                Err(fail("camera init failed"))
            }
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Self::Frame<'_>>, MediaError> {
        let (lock, cvar) = &*self.slot;
        let mut guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = Instant::now() + timeout;
        while guard.is_none() {
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let (g, _res) = cvar
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            guard = g;
        }
        // Take the latest raw frame out of the slot and build the borrowed CameraBuf here (on the
        // consumer thread — its self-referential pointer never crosses a thread boundary).
        let raw = guard.take();
        drop(guard);
        self.latest = raw.and_then(|r| CameraBuf::from_bgra(r.bgra, r.width, r.height, r.ts_us));
        Ok(self.latest.as_ref().map(CameraBuf::frame))
    }

    fn config(&self) -> StreamConfig {
        self.config
    }

    fn enumerate_cameras(&self) -> Vec<CameraDef> {
        match nokhwa::query(ApiBackend::Auto) {
            Ok(infos) => infos
                .into_iter()
                .map(|info| CameraDef {
                    id: CameraId(info.index().to_string()),
                    label: info.human_name(),
                    facing: CameraFacing::Unknown,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn stop(&mut self) {
        self.teardown();
    }
}

impl Drop for NokhwaCameraCapture {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Open the camera and pump RGB→BGRA frames into the latest-wins slot until `stop`. Runs entirely on
/// the worker thread (the non-`Send` `Camera` never leaves it). Never logs a pixel (Inv 8).
fn run_capture(
    index: CameraIndex,
    slot: Slot,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Result<(), MediaError>>,
) {
    let requested =
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
    let mut camera = match Camera::new(index, requested) {
        Ok(c) => c,
        Err(_) => {
            let _ = tx.send(Err(fail("could not open camera")));
            return;
        }
    };
    if camera.open_stream().is_err() {
        let _ = tx.send(Err(fail("could not start camera stream")));
        return;
    }
    let _ = tx.send(Ok(()));

    let mut counter: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        let Ok(buffer) = camera.frame() else {
            // A transient grab error: keep going; a persistent one stalls next_frame (bounded by its
            // timeout), and the session layer can rebuild via start().
            continue;
        };
        let Ok(decoded) = buffer.decode_image::<RgbFormat>() else {
            continue;
        };
        let (w, h) = (decoded.width(), decoded.height());
        let bgra = rgb8_to_bgra(&decoded.into_raw(), w, h);
        counter += 1;
        let (lock, cvar) = &*slot;
        let mut g = lock.lock().unwrap_or_else(|p| p.into_inner());
        *g = Some(RawFrame {
            bgra,
            width: w,
            height: h,
            ts_us: counter,
        }); // latest wins — drop any un-consumed prior frame
        drop(g);
        cvar.notify_one();
    }
    let _ = camera.stop_stream();
}
