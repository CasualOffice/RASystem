//! WASAPI loopback system-audio capture behind [`ras_media::AudioCaptureBackend`].
//!
//! See the crate docs for the push→pull adapter and the f32→i16 conversion. `unsafe` is confined
//! to this file (the FFI boundary) and wrapped behind the safe trait surface.
//!
//! # Threading model
//! WASAPI's COM interfaces (`IAudioClient`, `IAudioCaptureClient`) are apartment-bound and **not**
//! `Send`, but `AudioCaptureBackend: Send` (the backend is *moved* onto the host audio thread).
//! We therefore never hold a COM object in the backend struct: `start` spawns a **dedicated capture
//! thread** that does `CoInitializeEx` → activates the client → runs the `GetBuffer` loop, and
//! pushes converted i16 PCM into a shared [`AudioSlot`] FIFO. The backend keeps only the
//! `Send`-safe FIFO + a stop flag + the join handle, so it moves across threads cleanly. `stop`
//! signals the flag and joins the thread (which tears the COM objects down on their owning thread).
//!
//! # Format assumption (documented, per task scope)
//! We query the endpoint's actual mix format (`GetMixFormat`) and convert from it, handling both the
//! common **32-bit IEEE-float** interleaved mix and the rarer **16-bit PCM** mix (incl. the
//! `WAVEFORMATEXTENSIBLE` sub-format tags). We do **not** resample or re-channel: the FIFO carries
//! the endpoint's native rate/channels, and we report them in the negotiated [`AudioConfig`]. The
//! host output path is nominally 48 kHz / 2ch (Opus-native); a shared-mode render endpoint on
//! desktop Windows is almost always exactly that, so no resampling is needed in practice. A fully
//! robust impl would insert an SRC (e.g. WASAPI's own `IAudioClient::Initialize` with
//! `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` + a requested 48k/2ch format, or an explicit resampler) when
//! the native format differs — that is the on-device follow-up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use ras_media::{AudioCaptureBackend, AudioCodec, AudioConfig, CapturedAudio, MediaError};
use ras_protocol::{CaptureTimestampUs, ErrorCode, RasError};

use windows::core::Interface;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

/// Canonical audio config we *report* when the endpoint matches (Opus-shaped 48 kHz stereo). The
/// FIFO carries the endpoint's native rate/channels; these are the negotiation defaults.
const DEFAULT_SAMPLE_RATE_HZ: u32 = 48_000;
const DEFAULT_CHANNELS: u8 = 2;

/// Bounded FIFO cap (interleaved i16 samples). ~2 s of 48 kHz stereo — well above the pump's
/// per-tick drain, so it only trims under pathological backpressure. Trimming drops the *oldest*.
const MAX_BUFFERED_SAMPLES: usize = DEFAULT_SAMPLE_RATE_HZ as usize * DEFAULT_CHANNELS as usize * 2;

/// Poll interval for the WASAPI capture loop. WASAPI's default shared-mode period is ~10 ms; we
/// poll a touch tighter so the FIFO stays fed without busy-spinning.
const CAPTURE_POLL: Duration = Duration::from_millis(5);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shared FIFO between the WASAPI capture thread and the pull thread. Audio must not drop
/// mid-stream, so we append (not freshest-wins). `first_at_us` tracks the first *undrained*
/// sample's time.
#[derive(Default)]
struct AudioSlot {
    inner: Mutex<AudioBuf>,
    cv: Condvar,
}

#[derive(Default)]
struct AudioBuf {
    samples: Vec<i16>,
    /// Host monotonic time of `samples[0]` (updated when the FIFO empties/refills).
    first_at_us: u64,
    /// Set by the capture thread if WASAPI fails mid-stream, so the pull side surfaces a
    /// recoverable error and the caller rebuilds via `start`.
    failed: bool,
}

/// WASAPI loopback system-audio source. Pull-based over an appending FIFO fed by a capture thread.
pub struct WindowsAudioCapture {
    slot: Arc<AudioSlot>,
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    config: Option<AudioConfig>,
}

impl WindowsAudioCapture {
    /// New, unstarted backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: Arc::new(AudioSlot::default()),
            stop_flag: Arc::new(AtomicBool::new(false)),
            handle: None,
            config: None,
        }
    }
}

impl Default for WindowsAudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCaptureBackend for WindowsAudioCapture {
    fn start(&mut self, requested: &AudioConfig) -> Result<AudioConfig, MediaError> {
        self.stop();

        // Reset FIFO + stop flag for the new session.
        {
            let mut buf = lock(&self.slot.inner);
            buf.samples.clear();
            buf.first_at_us = 0;
            buf.failed = false;
        }
        self.stop_flag.store(false, Ordering::SeqCst);

        // The capture thread reports the endpoint's actual rate/channels back through this
        // one-shot channel so `start` can return the negotiated config synchronously (and surface
        // an init error fail-closed).
        let (tx, rx) = std::sync::mpsc::channel::<Result<(u32, u8), RasError>>();
        let slot = self.slot.clone();
        let stop_flag = self.stop_flag.clone();

        let handle = std::thread::Builder::new()
            .name("ras-audio-wasapi".into())
            .spawn(move || {
                capture_thread(&slot, &stop_flag, &tx);
            })
            .map_err(|_e| {
                RasError::fatal(
                    ErrorCode::CaptureFailed,
                    "failed to spawn audio capture thread",
                )
            })?;
        self.handle = Some(handle);

        // Wait for the thread's init result (device activation / format query).
        let (rate, channels) = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(fmt)) => fmt,
            Ok(Err(e)) => {
                self.stop();
                return Err(e);
            }
            Err(_) => {
                self.stop();
                return Err(RasError::fatal(
                    ErrorCode::CaptureFailed,
                    "WASAPI loopback init timed out",
                ));
            }
        };

        // We do not resample here (see module docs); honour the requested Opus frame/bitrate for
        // the downstream encoder, and report the endpoint's actual rate/channels.
        let negotiated = AudioConfig {
            codec: AudioCodec::Opus,
            sample_rate_hz: rate,
            channels,
            frame_duration_us: requested.frame_duration_us,
            target_bitrate_bps: requested.target_bitrate_bps,
        };
        self.config = Some(negotiated);
        Ok(negotiated)
    }

    fn next_chunk(&mut self, timeout: Duration) -> Result<Option<CapturedAudio>, MediaError> {
        let guard = lock(&self.slot.inner);
        if guard.failed {
            return Err(RasError::recoverable(
                ErrorCode::CaptureFailed,
                "audio stream stopped; restart",
            ));
        }
        let mut guard = if guard.samples.is_empty() {
            self.slot
                .cv
                .wait_timeout(guard, timeout)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0
        } else {
            guard
        };
        if guard.failed {
            return Err(RasError::recoverable(
                ErrorCode::CaptureFailed,
                "audio stream stopped; restart",
            ));
        }
        if guard.samples.is_empty() {
            return Ok(None); // timeout / silence
        }
        let samples = std::mem::take(&mut guard.samples);
        let captured_at_us = guard.first_at_us;
        Ok(Some(CapturedAudio {
            captured_at_us: captured_at_us as CaptureTimestampUs,
            samples,
        }))
    }

    fn config(&self) -> AudioConfig {
        self.config.unwrap_or(AudioConfig {
            codec: AudioCodec::Opus,
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            channels: DEFAULT_CHANNELS,
            frame_duration_us: 20_000,
            target_bitrate_bps: 96_000,
        })
    }

    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        // Wake any pull-side waiter and let the capture thread observe the flag.
        self.slot.cv.notify_all();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let mut buf = lock(&self.slot.inner);
        buf.samples.clear();
        buf.first_at_us = 0;
    }
}

impl Drop for WindowsAudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The sample layout the endpoint mix format resolves to.
#[derive(Clone, Copy)]
enum SampleKind {
    F32,
    I16,
}

/// Owns the WASAPI COM objects on their apartment thread and runs the loopback drain loop until
/// the stop flag is set. Sends the negotiated `(rate, channels)` (or an init error) once, then
/// loops feeding the FIFO. Any mid-stream error marks the FIFO `failed` and returns.
fn capture_thread(
    slot: &Arc<AudioSlot>,
    stop_flag: &Arc<AtomicBool>,
    init_tx: &std::sync::mpsc::Sender<Result<(u32, u8), RasError>>,
) {
    // SAFETY: single COM init/uninit pair scoped to this thread's lifetime; MTA so we never need a
    // message pump. A prior init on this (fresh) thread is impossible.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        let _ = init_tx.send(Err(RasError::fatal(
            ErrorCode::CaptureFailed,
            "CoInitializeEx failed",
        )));
        return;
    }
    // RAII: balance CoInitializeEx on every return path below.
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            // SAFETY: balances the CoInitializeEx above on this same thread.
            unsafe { CoUninitialize() };
        }
    }
    let _com = ComGuard;

    let run = || -> Result<(IAudioClient, IAudioCaptureClient, u32, u8, SampleKind), RasError> {
        // SAFETY: standard WASAPI activation. `CoCreateInstance` yields the device enumerator;
        // `GetDefaultAudioEndpoint(eRender, ...)` the default *output* device; `Activate` the
        // `IAudioClient`. Each `?` surfaces the HRESULT as a fail-closed error (never a panic).
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|_e| com_err("device enumerator unavailable"))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|_e| com_err("no default render endpoint"))?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|_e| com_err("IAudioClient activation failed"))?;

            // Query the endpoint's mix format; this is the format loopback delivers.
            let mix_ptr = client
                .GetMixFormat()
                .map_err(|_e| com_err("GetMixFormat failed"))?;
            if mix_ptr.is_null() {
                return Err(com_err("GetMixFormat returned null"));
            }
            // RAII: CoTaskMemFree the mix format on return.
            let _fmt_guard = CoMemGuard(mix_ptr.cast());

            let wfx = &*mix_ptr;
            let channels = wfx.nChannels.min(u16::from(u8::MAX)) as u8;
            let rate = wfx.nSamplesPerSec;
            let kind =
                classify_format(wfx).ok_or_else(|| com_err("unsupported endpoint mix format"))?;

            // Loopback shared-mode init. hnsBufferDuration 0 = default period; loopback ignores it.
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    0,
                    0,
                    mix_ptr,
                    None,
                )
                .map_err(|_e| com_err("IAudioClient::Initialize(loopback) failed"))?;

            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(|_e| com_err("IAudioCaptureClient unavailable"))?;

            client
                .Start()
                .map_err(|_e| com_err("IAudioClient::Start failed"))?;

            Ok((client, capture, rate, channels, kind))
        }
    };

    let (client, capture, rate, channels, kind) = match run() {
        Ok(v) => v,
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };
    // Report negotiated format to `start`.
    let _ = init_tx.send(Ok((rate, channels)));

    // Drain loop: pull every available loopback packet, convert to i16, append to the FIFO.
    while !stop_flag.load(Ordering::SeqCst) {
        // SAFETY: `capture` is a live IAudioCaptureClient; GetNextPacketSize/GetBuffer/ReleaseBuffer
        // form the documented WASAPI drain protocol. `GetBuffer` hands back a pointer valid until
        // the matching `ReleaseBuffer`; we copy the PCM out before releasing.
        let drained = unsafe { drain_packets(&capture, kind, channels, slot) };
        if let Err(()) = drained {
            let mut buf = lock(&slot.inner);
            buf.failed = true;
            slot.cv.notify_all();
            break;
        }
        // Sleep out the poll interval, but wake early on stop.
        if stop_flag.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(CAPTURE_POLL);
    }

    // SAFETY: stop the client; ignore the HRESULT (best-effort teardown). `client`/`capture` drop
    // here, releasing their COM refs on this owning thread.
    unsafe {
        let _ = client.Stop();
    }
}

/// Drain all currently-available loopback packets into the FIFO. Returns `Err(())` on a WASAPI
/// error the caller should surface as recoverable.
///
/// # Safety
/// `capture` must be a live, started `IAudioCaptureClient`.
unsafe fn drain_packets(
    capture: &IAudioCaptureClient,
    kind: SampleKind,
    channels: u8,
    slot: &Arc<AudioSlot>,
) -> Result<(), ()> {
    loop {
        let packet_frames = match capture.GetNextPacketSize() {
            Ok(n) => n,
            Err(_) => return Err(()),
        };
        if packet_frames == 0 {
            return Ok(());
        }

        let mut data: *mut u8 = std::ptr::null_mut();
        let mut num_frames: u32 = 0;
        let mut flags: u32 = 0;
        if capture
            .GetBuffer(&mut data, &mut num_frames, &mut flags, None, None)
            .is_err()
        {
            return Err(());
        }

        // AUDCLNT_BUFFERFLAGS_SILENT == 0x2: the buffer is silence; emit zeros (keeps A/V in sync)
        // without reading `data` (which may be null/stale for a silent packet).
        const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
        let n_ch = channels.max(1) as usize;
        let total = num_frames as usize * n_ch;

        let converted: Vec<i16> = if num_frames == 0 {
            Vec::new()
        } else if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 || data.is_null() {
            vec![0i16; total]
        } else {
            convert_pcm(data, total, kind)
        };

        // Release before touching the FIFO lock (keeps the WASAPI buffer held for the minimum time).
        if capture.ReleaseBuffer(num_frames).is_err() {
            return Err(());
        }

        if !converted.is_empty() {
            append_samples(slot, &converted);
        }
    }
}

/// Convert `total` interleaved samples at `data` to i16. `data` points to `total` samples of the
/// given `kind` (f32 or i16).
///
/// # Safety
/// `data` must point to at least `total` in-bounds samples of `kind`'s element type.
unsafe fn convert_pcm(data: *const u8, total: usize, kind: SampleKind) -> Vec<i16> {
    match kind {
        SampleKind::F32 => {
            let floats = std::slice::from_raw_parts(data.cast::<f32>(), total);
            floats.iter().map(|&f| f32_to_i16(f)).collect()
        }
        SampleKind::I16 => {
            // Already i16 interleaved — copy through.
            let src = std::slice::from_raw_parts(data.cast::<i16>(), total);
            src.to_vec()
        }
    }
}

/// Append converted samples to the shared FIFO, bounding the buffer (drops oldest under
/// backpressure). Timestamp: we don't have a per-packet host clock here without QPC bookkeeping, so
/// the first undrained sample carries `first_at_us = 0` (relative-only). A/V sync uses the encoder's
/// own monotonic `seq`; the wall-clock stamp is best-effort. (On-device follow-up: thread QPC.)
fn append_samples(slot: &Arc<AudioSlot>, converted: &[i16]) {
    let mut buf = lock(&slot.inner);
    buf.samples.extend_from_slice(converted);
    if buf.samples.len() > MAX_BUFFERED_SAMPLES {
        let overflow = buf.samples.len() - MAX_BUFFERED_SAMPLES;
        buf.samples.drain(0..overflow);
    }
    slot.cv.notify_one();
}

/// Classify a `WAVEFORMATEX` mix format into the sample layout we can convert. Handles plain
/// `WAVE_FORMAT_IEEE_FLOAT` / `WAVE_FORMAT_PCM` and the `WAVEFORMATEXTENSIBLE` sub-format GUIDs.
/// Returns `None` for anything else (unsupported → fail-closed).
fn classify_format(wfx: &WAVEFORMATEX) -> Option<SampleKind> {
    match u32::from(wfx.wFormatTag) {
        WAVE_FORMAT_IEEE_FLOAT if wfx.wBitsPerSample == 32 => Some(SampleKind::F32),
        WAVE_FORMAT_PCM if wfx.wBitsPerSample == 16 => Some(SampleKind::I16),
        WAVE_FORMAT_EXTENSIBLE => {
            // The header is actually a WAVEFORMATEXTENSIBLE; read the SubFormat GUID. The struct is
            // `#[repr(packed)]`, so we `read_unaligned` the field through a raw pointer rather than
            // taking a (potentially misaligned) reference to it.
            // SAFETY: WASAPI guarantees a WAVEFORMATEXTENSIBLE when wFormatTag == EXTENSIBLE, so the
            // memory at `wfx` is at least `size_of::<WAVEFORMATEXTENSIBLE>()`.
            let ext_ptr = (wfx as *const WAVEFORMATEX).cast::<WAVEFORMATEXTENSIBLE>();
            let sub_format = unsafe { std::ptr::addr_of!((*ext_ptr).SubFormat).read_unaligned() };
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && wfx.wBitsPerSample == 32 {
                Some(SampleKind::F32)
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM_SUBTYPE() && wfx.wBitsPerSample == 16 {
                Some(SampleKind::I16)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `KSDATAFORMAT_SUBTYPE_PCM` — inlined so we don't pull an extra import path; the canonical value
/// (`00000001-0000-0010-8000-00aa00389b71`).
#[allow(non_snake_case)]
fn KSDATAFORMAT_SUBTYPE_PCM_SUBTYPE() -> windows::core::GUID {
    windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71)
}

/// Convert one f32 sample in nominal [-1.0, 1.0] to i16 with clamping. Non-finite → 0 (silence).
#[inline]
fn f32_to_i16(f: f32) -> i16 {
    if !f.is_finite() {
        return 0;
    }
    let clamped = f.clamp(-1.0, 1.0);
    // 32767.0 keeps the positive range in bounds; -1.0 maps to -32767 (symmetric, avoids i16::MIN
    // overflow on the round-trip).
    (clamped * 32767.0).round() as i16
}

/// Build a content-free capture error. Never embeds a device name / HRESULT text (Inv 8 — nothing
/// audio-content-bearing, and we keep it deterministic).
fn com_err(msg: &'static str) -> RasError {
    RasError::fatal(ErrorCode::CaptureFailed, msg)
}

/// RAII for a `CoTaskMemAlloc`-owned block (the `GetMixFormat` result).
struct CoMemGuard(*mut core::ffi::c_void);
impl Drop for CoMemGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a CoTaskMem block returned by GetMixFormat; free it once.
            unsafe { CoTaskMemFree(Some(self.0)) };
        }
    }
}

// Interface trait is used for `.cast()`-style ergonomics on COM pointers where needed; keep the
// import referenced so it doesn't warn if a future edit drops its only use.
const _: fn() = || {
    fn _assert_interface<T: Interface>() {}
    _assert_interface::<IAudioClient>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_i16_maps_and_clamps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        // Out-of-range clamps, not wraps.
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-2.0), -32767);
        // Non-finite → silence.
        assert_eq!(f32_to_i16(f32::NAN), 0);
        assert_eq!(f32_to_i16(f32::INFINITY), 0);
    }

    #[test]
    fn classify_plain_float32() {
        let wfx = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        assert!(matches!(classify_format(&wfx), Some(SampleKind::F32)));
    }

    #[test]
    fn classify_plain_pcm16() {
        let wfx = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        assert!(matches!(classify_format(&wfx), Some(SampleKind::I16)));
    }

    #[test]
    fn classify_rejects_unknown() {
        let wfx = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 0,
            nBlockAlign: 0,
            wBitsPerSample: 24, // 24-bit PCM: not one of our two handled layouts
            cbSize: 0,
        };
        assert!(classify_format(&wfx).is_none());
    }
}
