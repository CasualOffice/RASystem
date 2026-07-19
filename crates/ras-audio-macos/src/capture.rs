//! ScreenCaptureKit system-audio capture behind [`ras_media::AudioCaptureBackend`].
//!
//! See the crate docs for the push→pull adapter and the f32→i16 conversion. `unsafe` is confined
//! to this file (the FFI boundary) and wrapped behind the safe trait surface.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_audio_types::AudioBufferList;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};

use ras_media::{AudioCaptureBackend, AudioCodec, AudioConfig, CapturedAudio, MediaError};
use ras_protocol::{CaptureTimestampUs, ErrorCode, RasError};

/// Canonical audio config we negotiate: Opus-shaped 48 kHz stereo, 20 ms frames.
const SAMPLE_RATE_HZ: u32 = 48_000;
const CHANNELS: u8 = 2;

/// Bounded FIFO cap (interleaved i16 samples). ~2 s of 48 kHz stereo — well above the pump's
/// per-tick drain, so it only trims under pathological backpressure. Trimming drops the *oldest*.
const MAX_BUFFERED_SAMPLES: usize = SAMPLE_RATE_HZ as usize * CHANNELS as usize * 2;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A `Send` shim for a single-thread-owned `objc2` handle. The trait requires `Send` (the backend
/// is *moved* onto the audio thread); `objc2`'s `Retained` is conservatively `!Send`. After the
/// move these handles are only ever touched from that one thread and never shared. Mirrors
/// `ras-media-macos::Sendable`.
struct Sendable<T>(T);
// SAFETY: single-thread ownership after the backend is moved to the audio thread; never shared.
unsafe impl<T> Send for Sendable<T> {}

/// Shared FIFO between SCK's delegate queue and the pull thread. Audio must not drop mid-stream,
/// so we append (not freshest-wins). `captured_at_us` tracks the first *undrained* sample's time.
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
    failed: bool,
}

struct OutputIvars {
    slot: Arc<AudioSlot>,
}

define_class!(
    // NSObject subclass; SCK calls it on its own dispatch queue (not the main thread).
    #[unsafe(super(NSObject))]
    #[name = "RasMacAudioOutput"]
    #[ivars = OutputIvars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            of_type: SCStreamOutputType,
        ) {
            if of_type.0 != SCStreamOutputType::Audio.0 {
                return;
            }
            self.on_audio(sample);
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn did_stop(&self, _stream: &SCStream, _error: &NSError) {
            // Content-free: never log the error object. Mark failed so the pull side surfaces a
            // recoverable error and the caller rebuilds via `start`.
            let mut buf = lock(&self.ivars().slot.inner);
            buf.failed = true;
            self.ivars().slot.cv.notify_all();
        }
    }
);

impl AudioOutput {
    fn new(slot: Arc<AudioSlot>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { slot });
        unsafe { msg_send![super(this), init] }
    }

    /// Runs on SCK's audio-handler queue. Extracts interleaved f32 PCM, converts to i16, appends.
    fn on_audio(&self, sample: &CMSampleBuffer) {
        let slot = &self.ivars().slot;

        // Presentation time (µs) of this buffer's first sample, host monotonic.
        // SAFETY: valid sample buffer for the callback duration.
        let pts = unsafe { sample.presentation_time_stamp() };
        let captured_at_us = if pts.timescale != 0 {
            (pts.value as i128 * 1_000_000 / i128::from(pts.timescale)) as u64
        } else {
            0
        };

        let Some(samples) = extract_i16_pcm(sample) else {
            return;
        };
        if samples.is_empty() {
            return;
        }

        let mut buf = lock(&slot.inner);
        if buf.samples.is_empty() {
            buf.first_at_us = captured_at_us;
        }
        buf.samples.extend_from_slice(&samples);
        // Bounded: under pathological backpressure, drop the oldest samples (keep the newest).
        if buf.samples.len() > MAX_BUFFERED_SAMPLES {
            let overflow = buf.samples.len() - MAX_BUFFERED_SAMPLES;
            buf.samples.drain(0..overflow);
            // Timestamp is now approximate after a trim; the encoder tolerates this (loss glitch).
            buf.first_at_us = captured_at_us;
        }
        slot.cv.notify_one();
    }
}

/// Read the single interleaved AudioBuffer out of the CMSampleBuffer and convert f32 → i16.
///
/// SCK output audio is 32-bit float, interleaved, one `AudioBuffer` (mNumberBuffers == 1). We copy
/// the retained block buffer's PCM out, clamp to [-1, 1], and scale to i16. Returns `None` on any
/// non-PCM / error / unexpected layout (fail-closed — a dropped chunk is a glitch, never a crash).
fn extract_i16_pcm(sample: &CMSampleBuffer) -> Option<Vec<i16>> {
    // Zeroed AudioBufferList to be filled in; a retained block buffer keeps `mData` alive until we
    // drop it. `[AudioBuffer; 1]` is the flexible-array header — one interleaved buffer for SCK.
    let mut abl = AudioBufferList {
        mNumberBuffers: 0,
        mBuffers: [objc2_core_audio_types::AudioBuffer {
            mNumberChannels: 0,
            mDataByteSize: 0,
            mData: std::ptr::null_mut(),
        }; 1],
    };
    let mut block_buffer: *mut objc2_core_media::CMBlockBuffer = std::ptr::null_mut();

    // `kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment` == 1u32 << 0. Contiguous +
    // 16-byte aligned so the interleaved f32 read below is well-defined.
    const ASSURE_16_BYTE_ALIGNMENT: u32 = 1;

    // SAFETY: `abl`/`block_buffer` are valid out-pointers sized for one buffer; on `noErr` SCK
    // fills `abl.mBuffers[0]` with an interleaved PCM buffer whose lifetime is owned by the
    // returned `block_buffer` (which we release at function end).
    let status = unsafe {
        sample.audio_buffer_list_with_retained_block_buffer(
            std::ptr::null_mut(),
            &mut abl as *mut AudioBufferList,
            std::mem::size_of::<AudioBufferList>(),
            None,
            None,
            ASSURE_16_BYTE_ALIGNMENT,
            &mut block_buffer as *mut *mut objc2_core_media::CMBlockBuffer,
        )
    };

    // Guard: release the retained block buffer on every return path.
    let _release = ReleaseBlockBuffer(block_buffer);

    if status != 0 || abl.mNumberBuffers == 0 {
        return None;
    }

    let ab = abl.mBuffers[0];
    if ab.mData.is_null() || ab.mDataByteSize == 0 {
        return None;
    }

    // Interleaved 32-bit float → i16 with clamping.
    let float_count = ab.mDataByteSize as usize / std::mem::size_of::<f32>();
    if float_count == 0 {
        return None;
    }
    // SAFETY: `mData` points to `mDataByteSize` contiguous bytes of interleaved f32 PCM
    // (kAudioFormatFlagIsFloat is SCK's output format), 16-byte aligned per the flag above; we
    // only read `float_count` in-bounds elements and copy them out before `block_buffer` drops.
    let floats = unsafe { std::slice::from_raw_parts(ab.mData as *const f32, float_count) };
    let mut out = Vec::with_capacity(float_count);
    for &f in floats {
        out.push(f32_to_i16(f));
    }
    Some(out)
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

/// RAII release of the retained `CMBlockBuffer` returned by
/// `audio_buffer_list_with_retained_block_buffer` (it hands back a +1 reference we must release).
struct ReleaseBlockBuffer(*mut objc2_core_media::CMBlockBuffer);
impl Drop for ReleaseBlockBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is either null (skipped) or a +1-retained CMBlockBuffer from SCK;
            // reconstructing the `CFRetained` and dropping it balances that retain. CMBlockBuffer
            // is a CFType.
            let nn = unsafe {
                std::ptr::NonNull::new_unchecked(self.0.cast::<objc2_core_media::CMBlockBuffer>())
            };
            // SAFETY: `nn` owns the +1 reference from SCK; `CFRetained` drops → releases it.
            unsafe {
                objc2_core_foundation::CFRetained::from_raw(nn);
            }
        }
    }
}

/// ScreenCaptureKit system-audio source. Audio-only stream, pull-based over an appending FIFO.
pub struct MacAudioCapture {
    stream: Option<Sendable<Retained<SCStream>>>,
    output: Option<Sendable<Retained<AudioOutput>>>,
    queue: Option<Sendable<DispatchRetained<DispatchQueue>>>,
    slot: Arc<AudioSlot>,
    config: Option<AudioConfig>,
}

impl MacAudioCapture {
    /// New, unstarted backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stream: None,
            output: None,
            queue: None,
            slot: Arc::new(AudioSlot::default()),
            config: None,
        }
    }
}

impl Default for MacAudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCaptureBackend for MacAudioCapture {
    fn start(&mut self, requested: &AudioConfig) -> Result<AudioConfig, MediaError> {
        self.stop();

        // We always negotiate 48 kHz stereo (SCK's default output-audio format); the encoder is
        // configured from the returned config. Honour the requested frame duration / bitrate for
        // the downstream Opus encoder.
        let negotiated = AudioConfig {
            codec: AudioCodec::Opus,
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: CHANNELS,
            frame_duration_us: requested.frame_duration_us,
            target_bitrate_bps: requested.target_bitrate_bps,
        };

        let content = shareable_content()?;
        // SAFETY: `content` is live; `displays()` returns its display array.
        let displays = unsafe { content.displays() };
        let display = displays
            .firstObject()
            .ok_or_else(|| RasError::fatal(ErrorCode::CaptureFailed, "no display available"))?;

        // SCK still requires a content filter (a display) even for an audio-only stream.
        let excluded: Retained<NSArray<SCWindow>> = NSArray::new();
        // SAFETY: `display` + `excluded` outlive the init call.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            )
        };

        // SAFETY: fresh config; audio setters take plain scalars. Video is kept minimal (SCK
        // requires a valid width/height even when we only consume audio).
        let config = unsafe {
            let c = SCStreamConfiguration::new();
            c.setCapturesAudio(true);
            c.setExcludesCurrentProcessAudio(true); // never capture our own output (feedback)
            c.setSampleRate(SAMPLE_RATE_HZ as isize);
            c.setChannelCount(CHANNELS as isize);
            // Minimal video config — a 2x2 surface at 1 fps; SCK requires non-zero dimensions but we
            // register no Screen output, so these frames are produced-and-discarded cheaply.
            c.setWidth(2);
            c.setHeight(2);
            c.setMinimumFrameInterval(CMTime::new(1, 1));
            c.setQueueDepth(3);
            c
        };

        // Reset the FIFO for the new session.
        {
            let mut buf = lock(&self.slot.inner);
            buf.samples.clear();
            buf.first_at_us = 0;
            buf.failed = false;
        }

        let output = AudioOutput::new(self.slot.clone());
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

        let queue = DispatchQueue::new("com.casualras.audio", None);
        let sc_output = ProtocolObject::from_ref(&*output);
        // SAFETY: registers our output for Audio buffers on a dedicated serial queue.
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    sc_output,
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|_e| {
                    RasError::fatal(ErrorCode::CaptureFailed, "addStreamOutput(audio) failed")
                })?;
        }

        start_capture_blocking(&stream)?;

        // SAFETY (all three): single-thread ownership after the backend is moved to the audio thread.
        self.stream = Some(Sendable(stream));
        self.output = Some(Sendable(output));
        self.queue = Some(Sendable(queue));
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
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: CHANNELS,
            frame_duration_us: 20_000,
            target_bitrate_bps: 96_000,
        })
    }

    fn stop(&mut self) {
        if let Some(s) = self.stream.as_ref() {
            stop_capture_blocking(&s.0);
        }
        self.stream = None;
        self.output = None;
        self.queue = None;
        let mut buf = lock(&self.slot.inner);
        buf.samples.clear();
        buf.first_at_us = 0;
    }
}

/// Synchronously fetch shareable content (SCK's API is completion-handler based).
fn shareable_content() -> Result<Retained<SCShareableContent>, MediaError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, _err: *mut NSError| {
            // SAFETY: SCK hands back a +0 autoreleased content (or NULL + a +0 error).
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
            "start_capture(audio) failed (Screen-Recording permission?)",
        )),
        Err(_) => Err(RasError::fatal(
            ErrorCode::CaptureFailed,
            "start_capture(audio) timed out",
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
}
