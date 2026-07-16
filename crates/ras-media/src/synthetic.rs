//! Deterministic, dependency-free capture + encode for CI and the loopback harness (design §3.4).
//!
//! No GPU, no OS permissions, no wall-clock. [`SyntheticCaptureBackend`] emits a steady synthetic
//! frame; [`SyntheticEncoder`] turns each into a **structurally valid Annex-B access unit** — real
//! `0x00000001` start codes, stub SPS/PPS re-sent in-band on every IDR, an honored keyframe flag,
//! and the `frame_id` watermarked into the slice payload. That is exactly enough for transport
//! framing, loss handling, reorder-by-id, and keyframe-request plumbing to be exercised end-to-end
//! **without a real decoder**. It is deliberately *not* decodable H.264; a future `openh264` feature
//! (via `libloading`, never x264/GPL) can swap in genuinely decodable frames.

use bytes::{BufMut, Bytes, BytesMut};

use crate::audio::{
    AudioCaptureBackend, AudioCodec, AudioConfig, AudioEncoderBackend, CapturedAudio, EncodedAudio,
};
use crate::{
    CaptureOptions, CaptureTimestampUs, CapturedFrame, ColorSpace, EncodedFrame, FrameId,
    MediaError, PlatformSurface, ScreenCaptureBackend, StreamConfig, VideoCodec,
    VideoEncoderBackend, VideoTransportKind,
};

/// Annex-B start code prefixed before every NAL unit.
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// One synthetic captured frame. Carries only metadata — the "surface" is virtual.
pub struct SyntheticFrame {
    captured_at_us: CaptureTimestampUs,
    width: u32,
    height: u32,
}

impl CapturedFrame for SyntheticFrame {
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
        // No real GPU surface exists; the synthetic encoder never dereferences it.
        PlatformSurface::none()
    }
}

/// Deterministic capture source. Produces one frame per `next_frame`, timestamped on a synthetic
/// monotonic clock derived from the frame counter and fps (no wall-clock — reproducible in CI).
pub struct SyntheticCaptureBackend {
    width: u32,
    height: u32,
    fps: u32,
    counter: u64,
    started: bool,
}

impl SyntheticCaptureBackend {
    /// New backend producing `width`×`height` frames. `start` fixes the fps from [`CaptureOptions`].
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            fps: 60,
            counter: 0,
            started: false,
        }
    }

    fn stream_config(&self) -> StreamConfig {
        StreamConfig {
            codec: VideoCodec::H264AnnexB,
            width: self.width,
            height: self.height,
            fps: self.fps,
            target_bitrate_bps: 6_000_000,
            color: ColorSpace::Bt709Limited,
            video_transport: VideoTransportKind::PerFrameStream,
        }
    }
}

impl ScreenCaptureBackend for SyntheticCaptureBackend {
    type Frame<'a>
        = SyntheticFrame
    where
        Self: 'a;

    fn start(&mut self, opts: &CaptureOptions) -> Result<StreamConfig, MediaError> {
        self.fps = opts.target_fps.max(1);
        self.counter = 0;
        self.started = true;
        Ok(self.stream_config())
    }

    fn next_frame(
        &mut self,
        _timeout: core::time::Duration,
    ) -> Result<Option<Self::Frame<'_>>, MediaError> {
        // Synthetic source is never static and never blocks: it always has the next frame ready.
        let captured_at_us = self.counter.saturating_mul(1_000_000) / u64::from(self.fps);
        self.counter += 1;
        Ok(Some(SyntheticFrame {
            captured_at_us,
            width: self.width,
            height: self.height,
        }))
    }

    fn config(&self) -> StreamConfig {
        self.stream_config()
    }

    fn stop(&mut self) {
        self.started = false;
    }
}

/// Deterministic encoder. Honors `request_keyframe` / `set_bitrate`, forces the first frame to an
/// IDR (a decoder must start on a keyframe), and re-sends stub SPS+PPS in-band on every IDR so any
/// keyframe is self-contained (mirrors the real Annex-B contract).
pub struct SyntheticEncoder {
    config: StreamConfig,
    next_frame_id: FrameId,
    force_keyframe: bool,
    first: bool,
}

impl SyntheticEncoder {
    /// New encoder. `configure` must be called before `encode`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: StreamConfig {
                codec: VideoCodec::H264AnnexB,
                width: 0,
                height: 0,
                fps: 0,
                target_bitrate_bps: 0,
                color: ColorSpace::Bt709Limited,
                video_transport: VideoTransportKind::PerFrameStream,
            },
            next_frame_id: 0,
            force_keyframe: false,
            first: true,
        }
    }

    /// Append one NAL: start code + one-byte NAL header + payload.
    fn push_nal(buf: &mut BytesMut, nal_header: u8, payload: &[u8]) {
        buf.put_slice(&START_CODE);
        buf.put_u8(nal_header);
        buf.put_slice(payload);
    }
}

impl Default for SyntheticEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoEncoderBackend for SyntheticEncoder {
    fn configure(&mut self, config: &StreamConfig) -> Result<(), MediaError> {
        self.config = *config;
        self.next_frame_id = 0;
        self.first = true;
        self.force_keyframe = false;
        Ok(())
    }

    fn encode<F: CapturedFrame>(&mut self, frame: F) -> Result<Option<EncodedFrame>, MediaError> {
        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;
        let is_keyframe = self.first || self.force_keyframe;
        self.first = false;
        self.force_keyframe = false;

        // Watermark the frame id into the slice payload so tests can verify reorder-by-id and loss.
        let watermark = frame_id.to_be_bytes();
        let mut buf = BytesMut::with_capacity(64);
        if is_keyframe {
            // Stub SPS (type 7 ⇒ 0x67) + PPS (type 8 ⇒ 0x68), re-sent every IDR.
            Self::push_nal(&mut buf, 0x67, &[0x42, 0x00, 0x1F]);
            Self::push_nal(&mut buf, 0x68, &[0xCE, 0x3C, 0x80]);
            // IDR slice (type 5 ⇒ 0x65).
            Self::push_nal(&mut buf, 0x65, &watermark);
        } else {
            // Non-IDR slice (type 1 ⇒ 0x61).
            Self::push_nal(&mut buf, 0x61, &watermark);
        }

        Ok(Some(EncodedFrame {
            frame_id,
            captured_at_us: frame.captured_at_us(),
            is_keyframe,
            data: Bytes::from(buf),
            config: self.config,
        }))
    }

    fn request_keyframe(&mut self, _reason: ras_protocol::KeyframeReason) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        self.config.target_bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn config(&self) -> StreamConfig {
        self.config
    }
}

/// Deterministic tone source (ADR-077): emits one full frame of a 440 Hz sine per `next_chunk`, with
/// no audio device and no wall-clock. Exercises the [`AudioCaptureBackend`] seam in CI.
pub struct SyntheticAudioCapture {
    config: AudioConfig,
    phase: f32,
    clock_us: CaptureTimestampUs,
    started: bool,
}

impl SyntheticAudioCapture {
    /// New source at the Opus defaults (48 kHz stereo, 20 ms). `start` must be called before pulling.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: AudioConfig {
                codec: AudioCodec::Opus,
                sample_rate_hz: 48_000,
                channels: 2,
                frame_duration_us: 20_000,
                target_bitrate_bps: 96_000,
            },
            phase: 0.0,
            clock_us: 0,
            started: false,
        }
    }
}

impl Default for SyntheticAudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCaptureBackend for SyntheticAudioCapture {
    fn start(&mut self, requested: &AudioConfig) -> Result<AudioConfig, MediaError> {
        self.config = *requested;
        self.phase = 0.0;
        self.clock_us = 0;
        self.started = true;
        Ok(self.config)
    }

    fn next_chunk(
        &mut self,
        _timeout: core::time::Duration,
    ) -> Result<Option<CapturedAudio>, MediaError> {
        if !self.started {
            return Ok(None);
        }
        let per_channel = self.config.frame_samples() as usize;
        let channels = self.config.channels as usize;
        let two_pi = 2.0 * core::f32::consts::PI;
        let step = two_pi * 440.0 / self.config.sample_rate_hz as f32;
        let mut samples = Vec::with_capacity(per_channel * channels);
        for _ in 0..per_channel {
            let v = (self.phase.sin() * f32::from(i16::MAX)) as i16;
            self.phase += step;
            if self.phase > two_pi {
                self.phase -= two_pi;
            }
            // Same sample on every channel (a mono tone spread across the interleaved frame).
            for _ in 0..channels {
                samples.push(v);
            }
        }
        let captured_at_us = self.clock_us;
        self.clock_us = self
            .clock_us
            .saturating_add(u64::from(self.config.frame_duration_us));
        Ok(Some(CapturedAudio {
            captured_at_us,
            samples,
        }))
    }

    fn config(&self) -> AudioConfig {
        self.config
    }

    fn stop(&mut self) {
        self.started = false;
    }
}

/// Passthrough audio "encoder" (ADR-077): serializes the interleaved PCM to little-endian bytes and
/// stamps a monotonic `seq`. Not Opus — exactly enough to exercise transport framing, loss-by-`seq`,
/// and the [`AudioEncoderBackend`] seam without libopus, mirroring [`SyntheticEncoder`] for video.
pub struct SyntheticAudioEncoder {
    config: AudioConfig,
    next_seq: u64,
}

impl SyntheticAudioEncoder {
    /// New encoder. `configure` must be called before `encode`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: AudioConfig {
                codec: AudioCodec::Opus,
                sample_rate_hz: 0,
                channels: 0,
                frame_duration_us: 0,
                target_bitrate_bps: 0,
            },
            next_seq: 0,
        }
    }
}

impl Default for SyntheticAudioEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEncoderBackend for SyntheticAudioEncoder {
    fn configure(&mut self, config: &AudioConfig) -> Result<(), MediaError> {
        self.config = *config;
        self.next_seq = 0;
        Ok(())
    }

    fn encode(&mut self, chunk: CapturedAudio) -> Result<Option<EncodedAudio>, MediaError> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut buf = BytesMut::with_capacity(chunk.samples.len() * 2);
        for s in &chunk.samples {
            buf.put_i16_le(*s);
        }
        Ok(Some(EncodedAudio {
            seq,
            captured_at_us: chunk.captured_at_us,
            data: Bytes::from(buf),
            config: self.config,
        }))
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        self.config.target_bitrate_bps = bitrate_bps;
        Ok(())
    }

    fn config(&self) -> AudioConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_protocol::KeyframeReason;

    fn drive(n: usize) -> (SyntheticCaptureBackend, SyntheticEncoder, Vec<EncodedFrame>) {
        let mut cap = SyntheticCaptureBackend::new(1280, 720);
        let cfg = cap
            .start(&CaptureOptions {
                monitor: crate::MonitorId(0),
                target_fps: 60,
                excluded_window_ids: vec![],
            })
            .unwrap();
        let mut enc = SyntheticEncoder::new();
        enc.configure(&cfg).unwrap();
        let mut out = Vec::new();
        for _ in 0..n {
            let f = cap
                .next_frame(core::time::Duration::from_millis(1))
                .unwrap()
                .unwrap();
            out.push(enc.encode(f).unwrap().unwrap());
        }
        (cap, enc, out)
    }

    #[test]
    fn first_frame_is_keyframe_rest_are_not() {
        let (_, _, frames) = drive(4);
        assert!(frames[0].is_keyframe, "decoder must be able to start");
        assert!(frames[1..].iter().all(|f| !f.is_keyframe));
    }

    #[test]
    fn frame_ids_are_monotonic_and_annexb_framed() {
        let (_, _, frames) = drive(3);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.frame_id, i as u64);
            assert_eq!(
                &f.data[..4],
                &START_CODE,
                "starts with an Annex-B start code"
            );
        }
    }

    #[test]
    fn keyframes_carry_sps_pps_in_band() {
        let (_, _, frames) = drive(1);
        let kf = &frames[0];
        // SPS (0x67) then PPS (0x68) then IDR slice (0x65) all present in-band.
        assert_eq!(kf.data[4], 0x67);
        assert!(kf.data.windows(1).any(|w| w[0] == 0x68));
        assert!(kf.data.windows(1).any(|w| w[0] == 0x65));
    }

    #[test]
    fn request_keyframe_forces_next_idr() {
        let mut cap = SyntheticCaptureBackend::new(640, 480);
        let cfg = cap
            .start(&CaptureOptions {
                monitor: crate::MonitorId(0),
                target_fps: 30,
                excluded_window_ids: vec![],
            })
            .unwrap();
        let mut enc = SyntheticEncoder::new();
        enc.configure(&cfg).unwrap();
        let dur = core::time::Duration::from_millis(1);
        let _ = enc.encode(cap.next_frame(dur).unwrap().unwrap()).unwrap(); // keyframe (first)
        let f2 = enc
            .encode(cap.next_frame(dur).unwrap().unwrap())
            .unwrap()
            .unwrap();
        assert!(!f2.is_keyframe);
        enc.request_keyframe(KeyframeReason::UnrecoverableLoss);
        let f3 = enc
            .encode(cap.next_frame(dur).unwrap().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            f3.is_keyframe,
            "request_keyframe forces the next frame to an IDR"
        );
    }

    #[test]
    fn set_bitrate_retargets_without_reconfigure() {
        let mut enc = SyntheticEncoder::new();
        enc.configure(&StreamConfig {
            codec: VideoCodec::H264AnnexB,
            width: 640,
            height: 480,
            fps: 30,
            target_bitrate_bps: 1_000_000,
            color: ColorSpace::Bt709Limited,
            video_transport: VideoTransportKind::PerFrameStream,
        })
        .unwrap();
        enc.set_bitrate(2_500_000).unwrap();
        assert_eq!(enc.config().target_bitrate_bps, 2_500_000);
    }

    #[test]
    fn synthetic_audio_capture_encode_roundtrip() {
        let mut cap = SyntheticAudioCapture::new();
        let cfg = cap.start(&cap.config()).unwrap();
        let mut enc = SyntheticAudioEncoder::new();
        enc.configure(&cfg).unwrap();

        let per_channel = cfg.frame_samples() as usize;
        let expected_samples = per_channel * cfg.channels as usize;
        let dur = core::time::Duration::from_millis(1);

        let mut prev_seq = None;
        for _ in 0..5 {
            let chunk = cap.next_chunk(dur).unwrap().unwrap();
            assert_eq!(
                chunk.samples.len(),
                expected_samples,
                "a full frame is frame_samples × channels"
            );
            let pkt = enc.encode(chunk).unwrap().unwrap();
            // seq is monotonic starting at 0; each i16 sample became 2 LE bytes.
            if let Some(p) = prev_seq {
                assert_eq!(pkt.seq, p + 1, "packet seq is gap-free monotonic");
            } else {
                assert_eq!(pkt.seq, 0);
            }
            prev_seq = Some(pkt.seq);
            assert_eq!(pkt.data.len(), expected_samples * 2);
            assert_eq!(pkt.config.codec, AudioCodec::Opus);
        }

        // ABR retargets the live encoder without a reconfigure.
        enc.set_bitrate(64_000).unwrap();
        assert_eq!(enc.config().target_bitrate_bps, 64_000);

        cap.stop();
        assert!(
            cap.next_chunk(dur).unwrap().is_none(),
            "a stopped source yields no chunks"
        );
    }
}
