//! Cross-platform microphone capture for 1:1 calls (ADR-103, L5), behind [`ras_media::AudioCaptureBackend`].
//!
//! Wraps `cpal` (CoreAudio / WASAPI / ALSA). cpal's `Stream` is **not `Send`**, so it is created and
//! held entirely on a dedicated thread; the audio callback pushes interleaved samples into a bounded
//! shared [`framer::Framer`], and the pull-based [`AudioCaptureBackend::next_chunk`] drains fixed-size
//! frames from it. Bounded = capture latency never grows without limit (priority #2). The framing /
//! sample-conversion core ([`framer`]) is pure and unit-tested off-device; the live device path needs
//! real hardware + an OS microphone-permission grant (on-device follow-up).
//!
//! Empty off the three desktop OSes so the workspace stays green everywhere.

pub mod framer;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub use backend_impl::CpalMicCapture;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod backend_impl {
    use crate::framer::{f32_to_i16, u16_to_i16, Framer};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use ras_media::audio::{AudioCaptureBackend, AudioCodec, AudioConfig, CapturedAudio};
    use ras_media::MediaError;
    use ras_protocol::ErrorCode;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// Opus operates at 48 kHz; we request it explicitly so the encoder needs no resampling.
    const SAMPLE_RATE_HZ: u32 = 48_000;
    /// Buffer at most this many frames before dropping the oldest (bounded latency).
    const MAX_BUFFERED_FRAMES: usize = 8;
    /// The config reported before a device is opened (mono voice at Opus's native 48 kHz, 20 ms).
    const DEFAULT_CONFIG: AudioConfig = AudioConfig {
        codec: AudioCodec::Opus,
        sample_rate_hz: SAMPLE_RATE_HZ,
        channels: 1,
        frame_duration_us: 20_000,
        target_bitrate_bps: 64_000,
    };

    type Shared = Arc<(Mutex<Framer>, Condvar)>;

    fn fail(msg: &'static str) -> MediaError {
        MediaError::recoverable(ErrorCode::CaptureFailed, msg)
    }

    /// A live microphone capture. The cpal stream is owned by `_worker`; this struct is `Send` (the
    /// non-`Send` `Stream` never leaves that thread).
    pub struct CpalMicCapture {
        shared: Shared,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        config: AudioConfig,
        started_at: Instant,
    }

    impl CpalMicCapture {
        /// Build an unstarted capture. Call [`AudioCaptureBackend::start`] to open the device.
        #[must_use]
        pub fn new() -> Self {
            Self {
                shared: Arc::new((
                    Mutex::new(Framer::new(1, MAX_BUFFERED_FRAMES)),
                    Condvar::new(),
                )),
                stop: Arc::new(AtomicBool::new(false)),
                worker: None,
                config: DEFAULT_CONFIG,
                started_at: Instant::now(),
            }
        }

        fn teardown(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.worker.take() {
                h.thread().unpark();
                let _ = h.join();
            }
        }
    }

    impl Default for CpalMicCapture {
        fn default() -> Self {
            Self::new()
        }
    }

    impl AudioCaptureBackend for CpalMicCapture {
        fn start(&mut self, requested: &AudioConfig) -> Result<AudioConfig, MediaError> {
            self.teardown(); // idempotent restart
            self.stop = Arc::new(AtomicBool::new(false));

            let channels = requested.channels.clamp(1, 2);
            let frame_len = (SAMPLE_RATE_HZ as usize / 1000)
                * (requested.frame_duration_us.max(1000) as usize / 1000)
                * channels as usize;
            let shared: Shared = Arc::new((
                Mutex::new(Framer::new(frame_len, MAX_BUFFERED_FRAMES)),
                Condvar::new(),
            ));

            // The cpal Stream is !Send: build + hold it on this worker thread. Report the negotiated
            // config (or a build error) back before we return.
            let (tx, rx) = mpsc::channel::<Result<AudioConfig, MediaError>>();
            let stop = Arc::clone(&self.stop);
            let cb_shared = Arc::clone(&shared);
            let negotiated = AudioConfig {
                sample_rate_hz: SAMPLE_RATE_HZ,
                channels,
                ..*requested
            };
            let worker = std::thread::Builder::new()
                .name("ras-mic".into())
                .spawn(move || run_stream(cb_shared, stop, tx, channels, negotiated))
                .map_err(|_| fail("could not spawn mic thread"))?;

            // Wait for the worker to open the device (or fail). The channel closing without a value
            // means the worker panicked building the stream → treat as a device failure.
            match rx.recv() {
                Ok(Ok(cfg)) => {
                    self.shared = shared;
                    self.worker = Some(worker);
                    self.config = cfg;
                    self.started_at = Instant::now();
                    Ok(cfg)
                }
                Ok(Err(e)) => {
                    let _ = worker.join();
                    Err(e)
                }
                Err(_) => {
                    let _ = worker.join();
                    Err(fail("mic device init failed"))
                }
            }
        }

        fn next_chunk(&mut self, timeout: Duration) -> Result<Option<CapturedAudio>, MediaError> {
            let (lock, cvar) = &*self.shared;
            let mut framer = lock.lock().unwrap_or_else(|p| p.into_inner());
            let deadline = Instant::now() + timeout;
            while !framer.has_frame() {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                let (guard, _res) = cvar
                    .wait_timeout(framer, deadline - now)
                    .unwrap_or_else(|p| p.into_inner());
                framer = guard;
            }
            let samples = match framer.take_frame() {
                Some(s) => s,
                None => return Ok(None),
            };
            drop(framer);
            let captured_at_us = self.started_at.elapsed().as_micros() as u64;
            Ok(Some(CapturedAudio {
                captured_at_us,
                samples,
            }))
        }

        fn config(&self) -> AudioConfig {
            self.config
        }

        fn stop(&mut self) {
            self.teardown();
        }
    }

    impl Drop for CpalMicCapture {
        fn drop(&mut self) {
            self.teardown();
        }
    }

    /// Open the default input device, start the stream, and keep it alive until `stop` is set. Reports
    /// the negotiated config back on `tx`. Runs entirely on the worker thread (the `Stream` never
    /// leaves it). Never logs a sample (Inv 8).
    fn run_stream(
        shared: Shared,
        stop: Arc<AtomicBool>,
        tx: mpsc::Sender<Result<AudioConfig, MediaError>>,
        channels: u8,
        negotiated: AudioConfig,
    ) {
        let host = cpal::default_host();
        let Some(device) = host.default_input_device() else {
            let _ = tx.send(Err(fail("no microphone found")));
            return;
        };
        let Ok(default_cfg) = device.default_input_config() else {
            let _ = tx.send(Err(fail("mic has no supported config")));
            return;
        };
        let sample_format = default_cfg.sample_format();
        let stream_cfg = cpal::StreamConfig {
            channels: u16::from(channels),
            sample_rate: cpal::SampleRate(SAMPLE_RATE_HZ),
            buffer_size: cpal::BufferSize::Default,
        };

        let push_shared = Arc::clone(&shared);
        let push = move |samples: Vec<i16>| {
            let (lock, cvar) = &*push_shared;
            let mut framer = lock.lock().unwrap_or_else(|p| p.into_inner());
            framer.push(&samples);
            let ready = framer.has_frame();
            drop(framer);
            if ready {
                cvar.notify_one();
            }
        };
        let err_fn = |e: cpal::StreamError| log::warn!("mic stream error: {e}");

        let built = match sample_format {
            cpal::SampleFormat::F32 => {
                let push = push.clone();
                device.build_input_stream(
                    &stream_cfg,
                    move |data: &[f32], _| push(data.iter().copied().map(f32_to_i16).collect()),
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let push = push.clone();
                device.build_input_stream(
                    &stream_cfg,
                    move |data: &[i16], _| push(data.to_vec()),
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let push = push.clone();
                device.build_input_stream(
                    &stream_cfg,
                    move |data: &[u16], _| push(data.iter().copied().map(u16_to_i16).collect()),
                    err_fn,
                    None,
                )
            }
            _ => {
                let _ = tx.send(Err(fail("unsupported mic sample format")));
                return;
            }
        };

        let stream = match built {
            Ok(s) => s,
            Err(_) => {
                let _ = tx.send(Err(fail("could not open mic at 48 kHz")));
                return;
            }
        };
        if stream.play().is_err() {
            let _ = tx.send(Err(fail("could not start mic stream")));
            return;
        }

        // Device is live: report the negotiated config, then hold the stream until asked to stop.
        let _ = tx.send(Ok(negotiated));
        while !stop.load(Ordering::SeqCst) {
            std::thread::park_timeout(Duration::from_millis(200));
        }
        drop(stream); // stops + releases the device
    }
}
