//! PulseAudio monitor-source capture behind [`ras_media::AudioCaptureBackend`].
//!
//! See the crate docs for the monitor-source rationale and the push→pull (here: blocking-pull)
//! adapter. The safe `libpulse-simple-binding` wrapper means this file needs no `unsafe`.

use std::time::{Duration, Instant};

use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

use ras_media::{AudioCaptureBackend, AudioCodec, AudioConfig, CapturedAudio, MediaError};
use ras_protocol::{CaptureTimestampUs, ErrorCode, RasError};

/// Canonical audio config we negotiate: Opus-shaped 48 kHz stereo (matches `ras-audio-macos`).
const SAMPLE_RATE_HZ: u32 = 48_000;
const CHANNELS: u8 = 2;

/// The PulseAudio device to record. `@DEFAULT_MONITOR@` is a server-resolved alias for the default
/// sink's monitor source (`<default-sink>.monitor`) — i.e. exactly what the machine is *playing*.
/// PipeWire's `pipewire-pulse` shim honours the same alias, so this string covers both audio servers.
/// A specific monitor (e.g. `alsa_output.pci-0000_00_1f.3.analog-stereo.monitor`) could be passed
/// instead when the host offers a device picker; the default alias is the zero-config choice.
const DEFAULT_MONITOR_DEVICE: &str = "@DEFAULT_MONITOR@";

/// Read block size in **frames** (samples-per-channel). 960 frames @ 48 kHz = 20 ms — one Opus
/// frame's worth, so each `next_chunk` returns roughly one encoder frame and the blocking read is
/// bounded to ~20 ms, keeping the pump responsive.
const BLOCK_FRAMES: usize = 960;

/// Interleaved i16 samples per read block (`BLOCK_FRAMES × channels`).
const BLOCK_SAMPLES: usize = BLOCK_FRAMES * CHANNELS as usize;

/// PulseAudio monitor-source system-audio capture. Blocking-pull over `libpulse-simple`'s record API.
pub struct LinuxAudioCapture {
    /// The open record stream (`None` until `start`, dropped on `stop`).
    simple: Option<Simple>,
    config: Option<AudioConfig>,
    /// Host-monotonic base for `captured_at_us`. PulseAudio's `read()` is untimestamped, so we
    /// derive a monotonic capture time from a start instant + the running sample count (exact at the
    /// negotiated sample rate — good enough for A/V sync; the encoder tolerates small drift).
    start_instant: Option<Instant>,
    /// Frames delivered so far, for the derived timestamp.
    frames_emitted: u64,
}

impl LinuxAudioCapture {
    /// New, unstarted backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            simple: None,
            config: None,
            start_instant: None,
            frames_emitted: 0,
        }
    }
}

impl Default for LinuxAudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCaptureBackend for LinuxAudioCapture {
    fn start(&mut self, requested: &AudioConfig) -> Result<AudioConfig, MediaError> {
        self.stop();

        // We always negotiate 48 kHz stereo S16NE (PulseAudio does any resample/convert server-side
        // because we request that spec); carry the requested frame duration / bitrate for the
        // downstream Opus encoder.
        let negotiated = AudioConfig {
            codec: AudioCodec::Opus,
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: CHANNELS,
            frame_duration_us: requested.frame_duration_us,
            target_bitrate_bps: requested.target_bitrate_bps,
        };

        // Signed-16 native-endian, interleaved — the `CapturedAudio` contract. Requesting this spec
        // makes the server deliver i16 directly, so no client-side float→i16 on the happy path.
        let spec = Spec {
            format: Format::S16NE,
            rate: SAMPLE_RATE_HZ,
            channels: CHANNELS,
        };
        if !spec.is_valid() {
            return Err(RasError::fatal(
                ErrorCode::CaptureFailed,
                "invalid audio sample-spec",
            ));
        }

        // Direction::Record against the default sink's monitor source. `Simple::new` connects to the
        // user's PulseAudio/PipeWire session server; a failure here (no server, no monitor) is
        // fail-closed so the host refuses the audio plane. Never log the `PAErr` verbatim — keep it
        // content-free (Inv 8): the message is a fixed string, not the device/server detail.
        let simple = Simple::new(
            None,                         // default server (session socket)
            "casual-ras",                 // application name
            Direction::Record,            // recording (capturing playback via the monitor source)
            Some(DEFAULT_MONITOR_DEVICE), // the default sink's monitor source
            "system-audio",               // stream description
            &spec,
            None, // default channel map
            None, // default buffering attributes
        )
        .map_err(|_e| {
            RasError::fatal(
                ErrorCode::CaptureFailed,
                "PulseAudio monitor-source record stream unavailable",
            )
        })?;

        self.simple = Some(simple);
        self.config = Some(negotiated);
        self.start_instant = Some(Instant::now());
        self.frames_emitted = 0;
        Ok(negotiated)
    }

    fn next_chunk(&mut self, timeout: Duration) -> Result<Option<CapturedAudio>, MediaError> {
        let Some(simple) = self.simple.as_ref() else {
            return Err(RasError::recoverable(
                ErrorCode::CaptureFailed,
                "audio capture not started",
            ));
        };

        // Derive the capture timestamp of this block's first sample from the monotonic start + the
        // frames already emitted (exact at the negotiated rate). This is set *before* the blocking
        // read so it names the first sample about to be read, matching macOS's presentation-time base.
        let captured_at_us: u64 = self
            .start_instant
            .map(|_| self.frames_emitted * 1_000_000 / u64::from(SAMPLE_RATE_HZ))
            .unwrap_or(0);

        // `libpulse-simple`'s `read` blocks until the byte buffer is full. It offers no per-call
        // timeout, so we honour `timeout` conservatively: only issue a read when at least one block
        // can plausibly arrive within it (a 20 ms block needs ~20 ms of wall time). If the caller's
        // timeout is shorter than one block, report silence rather than over-block the pump.
        if timeout < Duration::from_millis(15) {
            return Ok(None);
        }

        let mut bytes = [0u8; BLOCK_SAMPLES * 2]; // i16 → 2 bytes each
        simple.read(&mut bytes).map_err(|_e| {
            // Content-free (Inv 8). A read error means the stream broke; the caller rebuilds via start.
            RasError::recoverable(
                ErrorCode::CaptureFailed,
                "audio monitor read failed; restart",
            )
        })?;

        // Reinterpret the little/native-endian byte pairs as i16 (S16NE == native byte order, so a
        // straight `from_ne_bytes` per pair is correct). No content is logged.
        let samples: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|b| i16::from_ne_bytes([b[0], b[1]]))
            .collect();

        self.frames_emitted += BLOCK_FRAMES as u64;

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
        // Dropping the `Simple` disconnects the record stream and releases the monitor source.
        self.simple = None;
        self.start_instant = None;
        self.frames_emitted = 0;
    }
}

/// Convert one f32 sample in nominal [-1.0, 1.0] to i16 with clamping. Non-finite → 0 (silence).
///
/// Unused on the happy path (we request S16NE so PulseAudio delivers i16 directly), but kept and
/// unit-tested as the conversion used if a raw-float fallback format is ever adopted. Mirrors
/// `ras-audio-macos::f32_to_i16` byte-for-byte.
#[inline]
#[allow(dead_code)]
fn f32_to_i16(f: f32) -> i16 {
    if !f.is_finite() {
        return 0;
    }
    let clamped = f.clamp(-1.0, 1.0);
    (clamped * 32767.0).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_sizing_is_20ms_stereo() {
        // 960 frames @ 48 kHz = 20 ms; interleaved stereo doubles the sample count.
        assert_eq!(BLOCK_FRAMES, 960);
        assert_eq!(BLOCK_SAMPLES, 1920);
        // One Opus 20 ms frame at 48 kHz is exactly 960 samples-per-channel.
        let cfg = AudioConfig {
            codec: AudioCodec::Opus,
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: CHANNELS,
            frame_duration_us: 20_000,
            target_bitrate_bps: 96_000,
        };
        assert_eq!(cfg.frame_samples(), BLOCK_FRAMES as u32);
    }

    #[test]
    fn s16ne_byte_reinterpretation_roundtrips() {
        // The `next_chunk` decode path: native-endian byte pairs → i16.
        let originals: [i16; 4] = [0, 1, -1, i16::MIN];
        let mut bytes = Vec::new();
        for &s in &originals {
            bytes.extend_from_slice(&s.to_ne_bytes());
        }
        let decoded: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|b| i16::from_ne_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(decoded, originals);
    }

    #[test]
    fn f32_to_i16_maps_and_clamps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        assert_eq!(f32_to_i16(2.0), 32767);
        assert_eq!(f32_to_i16(-2.0), -32767);
        assert_eq!(f32_to_i16(f32::NAN), 0);
        assert_eq!(f32_to_i16(f32::INFINITY), 0);
    }

    #[test]
    fn timestamp_advances_with_frames() {
        // At 48 kHz, one 960-frame block is exactly 20_000 µs.
        let per_block_us = BLOCK_FRAMES as u64 * 1_000_000 / u64::from(SAMPLE_RATE_HZ);
        assert_eq!(per_block_us, 20_000);
    }
}
