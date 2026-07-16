//! Audio pipeline (host → controller output audio, ADR-077).
//!
//! Mirrors the video pipeline's shape — a pull-based capture backend, an encoder backend, and a
//! decoder seam — defined here as traits + canonical types. The MVP direction is **host output
//! (system) audio → controller** only: no microphone, no two-way voice, and no recording (Inv 12 —
//! audio is live-only, never retained at rest). Streaming is gated on the `audio.listen` capability +
//! local consent and always shows an Inv-7 "AUDIO SHARED" indicator (enforced by the host/app, not
//! this crate). Concrete Opus encode + OS capture are the follow-up; like the video traits before
//! their backends landed, this crate stays dependency-light.

use crate::MediaError;
use bytes::Bytes;
use ras_protocol::CaptureTimestampUs;

/// The audio codec we emit. Opus only — royalty-free (Inv 18), low-latency, WebCodecs-native.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioCodec {
    /// One Opus packet per encoded chunk. Decodes with a WebCodecs `AudioDecoder` configured `"opus"`.
    Opus,
}

impl AudioCodec {
    /// The WebCodecs `AudioDecoder` codec string. This projection lives only at the JS boundary; the
    /// wire/in-memory type stays the enum.
    #[must_use]
    pub fn webcodecs_string(self) -> &'static str {
        match self {
            AudioCodec::Opus => "opus",
        }
    }
}

/// Negotiated audio-stream parameters. Opus defaults: 48 kHz, stereo, 20 ms frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    /// Codec (Opus).
    pub codec: AudioCodec,
    /// Sample rate (Hz). Opus operates internally at 48 kHz, so 48000 is the canonical choice.
    pub sample_rate_hz: u32,
    /// Channel count (1 = mono, 2 = stereo).
    pub channels: u8,
    /// Encoder frame duration (µs). Opus supports 2.5/5/10/20/40/60 ms; 20 000 (20 ms) is the
    /// latency/overhead sweet spot.
    pub frame_duration_us: u32,
    /// Target bitrate (bits/sec). ~64 kbps mono voice … ~128 kbps stereo music.
    pub target_bitrate_bps: u32,
}

impl AudioConfig {
    /// Samples **per channel** in one encoder frame at this config
    /// (`sample_rate_hz × frame_duration_us / 1_000_000`).
    #[must_use]
    pub fn frame_samples(self) -> u32 {
        ((u64::from(self.sample_rate_hz) * u64::from(self.frame_duration_us)) / 1_000_000) as u32
    }
}

/// A captured PCM chunk (interleaved signed-16 samples) — the encoder's input. Carries a host-
/// monotonic timestamp for A/V sync. For a full frame, `samples.len() == frame_samples × channels`.
#[derive(Debug, Clone)]
pub struct CapturedAudio {
    /// Capture time of the first sample, on the host monotonic clock.
    pub captured_at_us: CaptureTimestampUs,
    /// Interleaved signed 16-bit PCM (L, R, L, R… for stereo).
    pub samples: Vec<i16>,
}

/// One encoded audio packet. Analogous to [`crate::EncodedFrame`], but audio has **no keyframes** —
/// each Opus packet is independently decodable once the decoder has warmed up — so a `seq` gap is the
/// only loss signal.
#[derive(Debug, Clone)]
pub struct EncodedAudio {
    /// Monotonic packet id; a gap means loss.
    pub seq: u64,
    /// Host monotonic capture time of the packet's first sample.
    pub captured_at_us: CaptureTimestampUs,
    /// One complete codec packet (an Opus packet).
    pub data: Bytes,
    /// The config this packet was encoded under.
    pub config: AudioConfig,
}

/// Host audio source (system/output audio). Pull-based with timeout, like
/// [`crate::ScreenCaptureBackend`]: `Ok(None)` on silence/timeout so the pump never blocks. Direction
/// is host→controller only (no microphone in the MVP).
pub trait AudioCaptureBackend: Send {
    /// Begin capture at the requested config; returns the negotiated [`AudioConfig`].
    ///
    /// # Errors
    /// Device/permission failure.
    fn start(&mut self, requested: &AudioConfig) -> Result<AudioConfig, MediaError>;

    /// Block until the next chunk or `timeout`. `Ok(None)` = timed out / silent.
    ///
    /// # Errors
    /// A recoverable device error means the caller rebuilds via [`Self::start`].
    fn next_chunk(
        &mut self,
        timeout: core::time::Duration,
    ) -> Result<Option<CapturedAudio>, MediaError>;

    /// The negotiated config.
    fn config(&self) -> AudioConfig;

    /// Stop capture and release the device.
    fn stop(&mut self);
}

/// Opus encoder seam. Synchronous single-chunk call on the audio thread.
pub trait AudioEncoderBackend: Send {
    /// Configure for a stream.
    ///
    /// # Errors
    /// Encoder init failure.
    fn configure(&mut self, config: &AudioConfig) -> Result<(), MediaError>;

    /// Encode one PCM chunk. May buffer sub-frame input and return `Ok(None)` until a full frame is
    /// available.
    ///
    /// # Errors
    /// Encoder failure.
    fn encode(&mut self, chunk: CapturedAudio) -> Result<Option<EncodedAudio>, MediaError>;

    /// Retarget bitrate mid-stream (driven by ABR), without a reconfigure.
    ///
    /// # Errors
    /// Encoder failure.
    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError>;

    /// The negotiated config.
    fn config(&self) -> AudioConfig;
}

/// A decoded PCM chunk (native-fallback path; the MVP decodes in JS via WebCodecs `AudioDecoder`).
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// Interleaved signed 16-bit PCM.
    pub samples: Vec<i16>,
}

/// Native audio-decode fallback seam (parity with [`crate::DecoderBackend`]; the WebCodecs path lives
/// in JS). The first packet after `configure`/`reset` warms the decoder.
pub trait AudioDecoderBackend: Send {
    /// Configure for a stream.
    ///
    /// # Errors
    /// Decoder init failure.
    fn configure(&mut self, config: &AudioConfig) -> Result<(), MediaError>;

    /// Decode one packet.
    ///
    /// # Errors
    /// Decode failure (recover via [`Self::reset`]).
    fn decode(&mut self, packet: &EncodedAudio) -> Result<Option<DecodedAudio>, MediaError>;

    /// Reset decoder state after loss/error.
    fn reset(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_samples_matches_opus_defaults() {
        let cfg = AudioConfig {
            codec: AudioCodec::Opus,
            sample_rate_hz: 48_000,
            channels: 2,
            frame_duration_us: 20_000, // 20 ms
            target_bitrate_bps: 96_000,
        };
        // 48 kHz × 20 ms = 960 samples per channel (the canonical Opus frame).
        assert_eq!(cfg.frame_samples(), 960);
        assert_eq!(AudioCodec::Opus.webcodecs_string(), "opus");
    }
}
