//! Opus audio backend (ADR-080).
//!
//! Real [`AudioEncoderBackend`] + [`AudioDecoderBackend`] over libopus (via the safe `audiopus`
//! wrapper). Host→controller output audio is Opus at 48 kHz (ADR-077); this crate turns interleaved
//! i16 PCM into Opus packets and back. `unsafe` lives entirely in the external FFI crates — this crate
//! is `unsafe`-free.

use audiopus::{
    coder::{Decoder as OpusDec, Encoder as OpusEnc},
    packet::Packet,
    Application, Bitrate, Channels, MutSignals, SampleRate,
};
use bytes::Bytes;
use ras_media::{
    AudioCodec, AudioConfig, AudioDecoderBackend, AudioEncoderBackend, CapturedAudio, DecodedAudio,
    EncodedAudio, MediaError,
};
use ras_protocol::ErrorCode;

/// libopus' recommended maximum packet size (bytes) for a single frame.
const MAX_PACKET: usize = 4000;
/// Maximum samples **per channel** in one Opus frame (120 ms at 48 kHz) — the decode output ceiling.
const MAX_FRAME_SAMPLES_PER_CH: usize = 5760;

fn enc_err(msg: &'static str) -> MediaError {
    MediaError::fatal(ErrorCode::EncoderFailed, msg)
}
fn dec_err(msg: &'static str) -> MediaError {
    // A bad packet is a malformed message; other decode faults are also surfaced through this code.
    MediaError::fatal(ErrorCode::InvalidMessage, msg)
}

fn sample_rate(hz: u32) -> Result<SampleRate, MediaError> {
    SampleRate::try_from(hz as i32).map_err(|_| enc_err("unsupported opus sample rate"))
}
fn channels(n: u8) -> Result<Channels, MediaError> {
    match n {
        1 => Ok(Channels::Mono),
        2 => Ok(Channels::Stereo),
        _ => Err(enc_err("opus supports mono or stereo only")),
    }
}

fn placeholder_config() -> AudioConfig {
    AudioConfig {
        codec: AudioCodec::Opus,
        sample_rate_hz: 48_000,
        channels: 2,
        frame_duration_us: 20_000,
        target_bitrate_bps: 96_000,
    }
}

/// Opus encoder. Buffers sub-frame input and emits exactly one packet per complete frame (Opus needs
/// whole frames of a fixed size), honoring [`AudioEncoderBackend::encode`]'s `Ok(None)`-until-ready
/// contract.
pub struct OpusEncoder {
    enc: Option<OpusEnc>,
    config: AudioConfig,
    next_seq: u64,
    /// Interleaved samples not yet forming a full frame.
    pending: Vec<i16>,
    /// Samples per full frame = `frame_samples × channels`.
    frame_len: usize,
}

impl OpusEncoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            enc: None,
            config: placeholder_config(),
            next_seq: 0,
            pending: Vec::new(),
            frame_len: 0,
        }
    }
}

impl Default for OpusEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEncoderBackend for OpusEncoder {
    fn configure(&mut self, config: &AudioConfig) -> Result<(), MediaError> {
        let sr = sample_rate(config.sample_rate_hz)?;
        let ch = channels(config.channels)?;
        let mut enc = OpusEnc::new(sr, ch, Application::Audio)
            .map_err(|_| enc_err("opus encoder init failed"))?;
        enc.set_bitrate(Bitrate::BitsPerSecond(config.target_bitrate_bps as i32))
            .map_err(|_| enc_err("opus set_bitrate failed"))?;
        self.enc = Some(enc);
        self.config = *config;
        self.frame_len = config.frame_samples() as usize * config.channels as usize;
        self.next_seq = 0;
        self.pending.clear();
        Ok(())
    }

    fn encode(&mut self, chunk: CapturedAudio) -> Result<Option<EncodedAudio>, MediaError> {
        if self.frame_len == 0 {
            return Err(enc_err("opus encoder not configured"));
        }
        self.pending.extend_from_slice(&chunk.samples);
        if self.pending.len() < self.frame_len {
            return Ok(None); // buffer sub-frame input until a whole frame is available
        }
        let frame: Vec<i16> = self.pending.drain(..self.frame_len).collect();
        let enc = self
            .enc
            .as_ref()
            .ok_or_else(|| enc_err("opus encoder not configured"))?;
        let mut out = vec![0u8; MAX_PACKET];
        let n = enc
            .encode(&frame, &mut out)
            .map_err(|_| enc_err("opus encode failed"))?;
        let seq = self.next_seq;
        self.next_seq += 1;
        Ok(Some(EncodedAudio {
            seq,
            captured_at_us: chunk.captured_at_us,
            data: Bytes::copy_from_slice(&out[..n]),
            config: self.config,
        }))
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        let enc = self
            .enc
            .as_mut()
            .ok_or_else(|| enc_err("opus encoder not configured"))?;
        enc.set_bitrate(Bitrate::BitsPerSecond(bitrate_bps as i32))
            .map_err(|_| enc_err("opus set_bitrate failed"))?;
        self.config.target_bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn config(&self) -> AudioConfig {
        self.config
    }
}

/// Opus decoder (native-fallback path; the MVP decodes in JS via WebCodecs). One packet → one PCM
/// frame of interleaved i16.
pub struct OpusDecoder {
    dec: Option<OpusDec>,
    config: AudioConfig,
    channels: usize,
}

impl OpusDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            dec: None,
            config: placeholder_config(),
            channels: 2,
        }
    }
}

impl Default for OpusDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDecoderBackend for OpusDecoder {
    fn configure(&mut self, config: &AudioConfig) -> Result<(), MediaError> {
        let sr = sample_rate(config.sample_rate_hz)?;
        let ch = channels(config.channels)?;
        self.dec = Some(OpusDec::new(sr, ch).map_err(|_| dec_err("opus decoder init failed"))?);
        self.config = *config;
        self.channels = config.channels as usize;
        Ok(())
    }

    fn decode(&mut self, packet: &EncodedAudio) -> Result<Option<DecodedAudio>, MediaError> {
        let ch = self.channels.max(1);
        let dec = self
            .dec
            .as_mut()
            .ok_or_else(|| dec_err("opus decoder not configured"))?;
        let mut out = vec![0i16; MAX_FRAME_SAMPLES_PER_CH * ch];
        let pkt =
            Packet::try_from(packet.data.as_ref()).map_err(|_| dec_err("invalid opus packet"))?;
        let signals = MutSignals::try_from(&mut out[..])
            .map_err(|_| dec_err("opus output buffer invalid"))?;
        let per_channel = dec
            .decode(Some(pkt), signals, false)
            .map_err(|_| dec_err("opus decode failed"))?;
        out.truncate(per_channel * ch);
        Ok(Some(DecodedAudio { samples: out }))
    }

    fn reset(&mut self) {
        // audiopus has no explicit reset — rebuild the decoder to clear state.
        if let (Ok(sr), Ok(ch)) = (
            sample_rate(self.config.sample_rate_hz),
            channels(self.config.channels),
        ) {
            self.dec = OpusDec::new(sr, ch).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AudioConfig {
        AudioConfig {
            codec: AudioCodec::Opus,
            sample_rate_hz: 48_000,
            channels: 2,
            frame_duration_us: 20_000,
            target_bitrate_bps: 96_000,
        }
    }

    fn tone_frame(c: AudioConfig, phase: &mut f32) -> CapturedAudio {
        let per = c.frame_samples() as usize;
        let ch = c.channels as usize;
        let step = 2.0 * std::f32::consts::PI * 440.0 / c.sample_rate_hz as f32;
        let mut samples = Vec::with_capacity(per * ch);
        for _ in 0..per {
            let v = (phase.sin() * 16_000.0) as i16;
            *phase += step;
            for _ in 0..ch {
                samples.push(v);
            }
        }
        CapturedAudio {
            captured_at_us: 0,
            samples,
        }
    }

    #[test]
    fn encode_decode_roundtrip_preserves_frame_and_energy() {
        let c = cfg();
        let mut enc = OpusEncoder::new();
        enc.configure(&c).unwrap();
        let mut dec = OpusDecoder::new();
        dec.configure(&c).unwrap();
        let frame_total = c.frame_samples() as usize * c.channels as usize;

        let mut phase = 0.0f32;
        let mut last_decoded = None;
        let mut prev_seq = None;
        // Prime a few frames (Opus warms up over the first packets), asserting each encodes to a
        // non-empty, gap-free-seq packet and decodes back to a full frame.
        for _ in 0..5 {
            let chunk = tone_frame(c, &mut phase);
            let pkt = enc
                .encode(chunk)
                .unwrap()
                .expect("a full frame yields a packet");
            assert!(!pkt.data.is_empty(), "opus packet is non-empty");
            if let Some(p) = prev_seq {
                assert_eq!(pkt.seq, p + 1, "packet seq is gap-free monotonic");
            }
            prev_seq = Some(pkt.seq);
            let decoded = dec.decode(&pkt).unwrap().expect("a packet decodes");
            last_decoded = Some(decoded);
        }
        let decoded = last_decoded.unwrap();
        assert_eq!(
            decoded.samples.len(),
            frame_total,
            "decoded frame size matches the encoder frame"
        );
        let peak = decoded
            .samples
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap();
        assert!(
            peak > 1000,
            "the 440 Hz tone survives the codec (peak {peak})"
        );
    }

    #[test]
    fn set_bitrate_is_accepted_live() {
        let mut enc = OpusEncoder::new();
        enc.configure(&cfg()).unwrap();
        enc.set_bitrate(32_000).unwrap();
        assert_eq!(enc.config().target_bitrate_bps, 32_000);
    }

    #[test]
    fn sub_frame_input_is_buffered_until_a_full_frame() {
        let c = cfg();
        let mut enc = OpusEncoder::new();
        enc.configure(&c).unwrap();
        let frame_total = c.frame_samples() as usize * c.channels as usize;
        // Half a frame → nothing emitted yet (buffered).
        let half = CapturedAudio {
            captured_at_us: 0,
            samples: vec![0i16; frame_total / 2],
        };
        assert!(enc.encode(half).unwrap().is_none());
        // The second half completes a frame → a packet.
        let half2 = CapturedAudio {
            captured_at_us: 0,
            samples: vec![0i16; frame_total / 2],
        };
        assert!(enc.encode(half2).unwrap().is_some());
    }

    #[test]
    fn unconfigured_encoder_errors() {
        let mut enc = OpusEncoder::new();
        let chunk = CapturedAudio {
            captured_at_us: 0,
            samples: vec![0i16; 8],
        };
        assert!(enc.encode(chunk).is_err());
    }
}
