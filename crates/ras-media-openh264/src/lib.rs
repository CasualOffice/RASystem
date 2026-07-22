//! Cross-platform **software H.264 encoder** (OpenH264) implementing [`ras_media::VideoEncoderBackend`].
//!
//! It consumes CPU **BGRA** frames — a capture backend hands them over as a
//! [`ras_media::SurfaceKind::CpuBgra`] surface — converts to I420, and emits an **Annex-B** access
//! unit with in-band SPS/PPS on every IDR, matching the wire contract the WebCodecs viewer expects
//! (no out-of-band `description`). It is the encoder for the Linux and Windows software capture
//! backends (ADR-063); on macOS the hardware VideoToolbox path is used instead.
//!
//! Builds on every desktop OS (the `openh264` crate compiles Cisco's BSD-2 source). This crate is
//! FFI-adjacent, so the workspace `unsafe_code = deny` is relaxed here (CONTRIBUTING §5): `unsafe`
//! is confined to the borrowed-surface dereference and the single-thread `Send` shim.

use bytes::Bytes;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;
use ras_media::{
    CapturedFrame, ColorSpace, CpuBgraFrame, EncodedFrame, MediaError, StreamConfig, SurfaceKind,
    VideoCodec, VideoTransportKind,
};
use ras_protocol::{ErrorCode, KeyframeReason, RasError};

/// Codec capabilities of this backend (OpenH264 software encode = H.264 only). Used by the app to
/// build the host's [`ras_grant::HostEncodeCaps`] for codec negotiation.
pub const SUPPORTS_H264: bool = true;
/// This backend does not encode VP9.
pub const SUPPORTS_VP9: bool = false;

/// Default target bitrate advertised in [`StreamConfig`]. The encoder is built in bitrate rate-control
/// mode at this value and retargeted at runtime by the ABR via [`OpenH264Encoder::set_bitrate`].
const DEFAULT_BITRATE_BPS: u32 = 8_000_000;

/// The single-monitor [`StreamConfig`] these software backends negotiate: H.264 Annex-B,
/// per-frame-stream transport (post-spike default), limited-range BT.709 declared for parity with
/// the macOS path.
#[must_use]
pub fn default_stream_config(width: u32, height: u32, fps: u32) -> StreamConfig {
    StreamConfig {
        codec: VideoCodec::H264AnnexB,
        width,
        height,
        fps,
        target_bitrate_bps: DEFAULT_BITRATE_BPS,
        color: ColorSpace::Bt709Limited,
        video_transport: VideoTransportKind::PerFrameStream,
    }
}

fn enc_fatal(context: &'static str) -> MediaError {
    RasError::fatal(ErrorCode::EncoderFailed, context)
}

/// Software H.264 encoder over OpenH264.
pub struct OpenH264Encoder {
    config: StreamConfig,
    /// Lazily built on first `configure`/`encode`.
    enc: Option<Encoder>,
    /// Reused I420 buffer, rebuilt when the frame dimensions change.
    yuv: Option<(u32, u32, YUVBuffer)>,
    /// Contiguous BGRA scratch used when the source has row padding (`stride != width*4`) or an odd
    /// dimension is cropped to even.
    repack: Vec<u8>,
    /// Emit the next frame as a forced IDR (startup + on demand). Infinite GOP otherwise.
    force_idr: bool,
    /// Monotonic frame id; a gap on the wire means loss.
    next_id: u64,
}

// The encoder is owned and driven from a single media thread (moved there once, never shared);
// OpenH264's `ISVCEncoder` is used single-threaded. Same rationale as the macOS backend's shim.
unsafe impl Send for OpenH264Encoder {}

impl Default for OpenH264Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenH264Encoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: default_stream_config(1920, 1080, 60),
            enc: None,
            yuv: None,
            repack: Vec::new(),
            force_idr: true,
            next_id: 0,
        }
    }

    fn build_encoder(&self) -> Result<Encoder, MediaError> {
        // Bitrate rate-control mode so the encoder actually tracks `target_bitrate_bps` (its default
        // is quality mode at ~120 kbps, which ignores our target). Declaring the frame rate lets the
        // controller pace bits/sec correctly. ABR then retargets the live encoder via `set_bitrate`.
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(self.config.target_bitrate_bps.max(1)))
            .max_frame_rate(FrameRate::from_hz(self.config.fps.max(1) as f32));
        Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|_| enc_fatal("openh264 encoder init failed"))
    }

    /// Read the borrowed CPU BGRA descriptor out of a captured frame's surface (fail-closed on any
    /// mismatch), returning `(bytes, stride, width, height)` — dimensions cropped to even.
    fn bgra<F: CapturedFrame>(frame: &F) -> Result<(&[u8], usize, u32, u32), MediaError> {
        let surface = frame.platform_surface();
        let ptr = surface
            .as_ptr(SurfaceKind::CpuBgra)
            .ok_or_else(|| enc_fatal("expected a CpuBgra surface"))?;
        // SAFETY: the paired software capture backend set this pointer to a `CpuBgraFrame` it owns
        // for the lifetime of `frame` (ADR-058/063). We only read it within this call.
        let desc = unsafe { &*(ptr.as_ptr() as *const CpuBgraFrame) };
        if desc.data.is_null() || desc.width == 0 || desc.height == 0 {
            return Err(enc_fatal("empty CpuBgra surface"));
        }
        let w = desc.width & !1;
        let h = desc.height & !1;
        if w == 0 || h == 0 {
            return Err(enc_fatal("frame too small"));
        }
        let needed = desc
            .stride
            .checked_mul(desc.height as usize)
            .ok_or_else(|| enc_fatal("stride overflow"))?;
        if desc.stride < (desc.width as usize) * 4 || desc.len < needed {
            return Err(enc_fatal("CpuBgra buffer too small for its dimensions"));
        }
        // SAFETY: bounds validated above; the buffer is borrowed for the call.
        let bytes = unsafe { core::slice::from_raw_parts(desc.data, desc.len) };
        Ok((bytes, desc.stride, w, h))
    }
}

impl ras_media::VideoEncoderBackend for OpenH264Encoder {
    fn configure(&mut self, config: &StreamConfig) -> Result<(), MediaError> {
        self.config = *config;
        self.enc = Some(self.build_encoder()?);
        self.yuv = None;
        self.force_idr = true;
        Ok(())
    }

    fn encode<F: CapturedFrame>(&mut self, frame: F) -> Result<Option<EncodedFrame>, MediaError> {
        if self.enc.is_none() {
            self.enc = Some(self.build_encoder()?);
        }
        let captured_at_us = frame.captured_at_us();
        let (bytes, stride, w, h) = Self::bgra(&frame)?;
        let row = (w as usize) * 4;

        // Feed a tightly-packed BGRA slice. Repack when the source has row padding or was cropped.
        let packed: &[u8] = if stride == row && bytes.len() >= row * h as usize {
            &bytes[..row * h as usize]
        } else {
            self.repack.resize(row * h as usize, 0);
            for y in 0..h as usize {
                let src = &bytes[y * stride..y * stride + row];
                self.repack[y * row..y * row + row].copy_from_slice(src);
            }
            &self.repack
        };

        // Reuse the I420 buffer across frames; rebuild on a dimension change.
        let need_new = !matches!(self.yuv, Some((yw, yh, _)) if yw == w && yh == h);
        if need_new {
            self.yuv = Some((w, h, YUVBuffer::new(w as usize, h as usize)));
        }
        let (_, _, yuv) = self
            .yuv
            .as_mut()
            .ok_or_else(|| enc_fatal("no yuv buffer"))?;
        yuv.read_rgb(BgraSliceU8::new(packed, (w as usize, h as usize)));

        let enc = self.enc.as_mut().ok_or_else(|| enc_fatal("no encoder"))?;
        if self.force_idr {
            enc.force_intra_frame();
            self.force_idr = false;
        }
        let bitstream = enc
            .encode(yuv)
            .map_err(|_| enc_fatal("openh264 encode failed"))?;

        let frame_type = bitstream.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(None); // encoder coalesced/skipped — nothing to send (static screen)
        }
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        let is_keyframe = matches!(frame_type, FrameType::IDR | FrameType::I);
        let frame_id = self.next_id;
        self.next_id += 1;

        Ok(Some(EncodedFrame {
            frame_id,
            captured_at_us,
            is_keyframe,
            data: Bytes::from(data),
            config: self.config,
        }))
    }

    fn request_keyframe(&mut self, _reason: KeyframeReason) {
        self.force_idr = true;
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), MediaError> {
        self.config.target_bitrate_bps = bitrate_bps;
        // Retarget the live encoder's rate controller without forcing a keyframe — the latency-first
        // ABR ticks frequently and an IDR per change would spike latency (design §3.6). If the encoder
        // hasn't been built yet, the new target is picked up at build time via `build_encoder`.
        if let Some(enc) = self.enc.as_mut() {
            let mut info = openh264_sys2::TagBitrateInfo {
                iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
                iBitrate: bitrate_bps.min(i32::MAX as u32) as i32,
            };
            // SAFETY: `enc` is an initialized OpenH264 encoder used single-threaded here; SetOption
            // with ENCODER_OPTION_BITRATE reads the `TagBitrateInfo` we own for the duration of the
            // call and copies it. `raw_api` is the sanctioned escape hatch for options the safe
            // wrapper doesn't expose.
            let rc = unsafe {
                enc.raw_api().set_option(
                    openh264_sys2::ENCODER_OPTION_BITRATE,
                    core::ptr::addr_of_mut!(info).cast(),
                )
            };
            if rc != 0 {
                return Err(enc_fatal("openh264 SetOption(BITRATE) failed"));
            }
        }
        Ok(())
    }

    fn config(&self) -> StreamConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_media::{PlatformSurface, VideoEncoderBackend};

    /// A synthetic captured frame backed by a CPU BGRA buffer + its descriptor.
    struct Frame {
        desc: CpuBgraFrame,
        w: u32,
        h: u32,
    }
    impl CapturedFrame for Frame {
        fn captured_at_us(&self) -> u64 {
            1234
        }
        fn width(&self) -> u32 {
            self.w
        }
        fn height(&self) -> u32 {
            self.h
        }
        fn platform_surface(&self) -> PlatformSurface<'_> {
            PlatformSurface::from_ptr(core::ptr::from_ref(&self.desc).cast(), SurfaceKind::CpuBgra)
        }
    }

    fn gradient(w: u32, h: u32, stride: usize) -> Vec<u8> {
        let mut buf = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = y * stride + x * 4;
                buf[i] = (x * 4) as u8; // B
                buf[i + 1] = (y * 4) as u8; // G
                buf[i + 2] = 128; // R
                buf[i + 3] = 255; // A
            }
        }
        buf
    }

    fn nal_present(data: &[u8], nal_type: u8) -> bool {
        data.windows(5)
            .any(|w| w[..4] == [0, 0, 0, 1] && (w[4] & 0x1f) == nal_type)
    }

    #[test]
    fn encodes_bgra_to_annexb_keyframe_with_inband_sps_pps() {
        let (w, h) = (128u32, 96u32);
        let stride = (w * 4) as usize;
        let buf = gradient(w, h, stride);
        let frame = Frame {
            desc: CpuBgraFrame {
                data: buf.as_ptr(),
                len: buf.len(),
                stride,
                width: w,
                height: h,
            },
            w,
            h,
        };

        let mut enc = OpenH264Encoder::new();
        enc.configure(&default_stream_config(w, h, 60)).unwrap();
        let out = enc
            .encode(frame)
            .expect("encode ok")
            .expect("a frame is produced");

        assert!(out.is_keyframe, "first frame must be a keyframe");
        assert_eq!(out.frame_id, 0);
        assert_eq!(out.captured_at_us, 1234);
        assert_eq!(&out.data[..4], &[0, 0, 0, 1], "Annex-B start code");
        assert!(nal_present(&out.data, 7), "SPS in-band");
        assert!(nal_present(&out.data, 8), "PPS in-band");
        assert!(nal_present(&out.data, 5), "IDR slice");
    }

    #[test]
    fn handles_row_padding_and_odd_dimensions() {
        // Odd width (cropped to 100) and a padded stride.
        let (w, h) = (101u32, 64u32);
        let stride = (w as usize) * 4 + 48; // padded
        let buf = gradient(w, h, stride);
        let frame = Frame {
            desc: CpuBgraFrame {
                data: buf.as_ptr(),
                len: buf.len(),
                stride,
                width: w,
                height: h,
            },
            w,
            h,
        };
        let mut enc = OpenH264Encoder::new();
        enc.configure(&default_stream_config(w & !1, h, 60))
            .unwrap();
        let out = enc.encode(frame).expect("encode ok").expect("a frame");
        assert!(out.is_keyframe);
        assert!(nal_present(&out.data, 5), "IDR slice present");
    }

    #[test]
    fn rejects_wrong_surface_kind() {
        struct Bad;
        impl CapturedFrame for Bad {
            fn captured_at_us(&self) -> u64 {
                0
            }
            fn width(&self) -> u32 {
                64
            }
            fn height(&self) -> u32 {
                64
            }
            fn platform_surface(&self) -> PlatformSurface<'_> {
                PlatformSurface::none()
            }
        }
        let mut enc = OpenH264Encoder::new();
        enc.configure(&default_stream_config(64, 64, 60)).unwrap();
        assert!(
            enc.encode(Bad).is_err(),
            "must fail-close on a non-CpuBgra surface"
        );
    }

    #[test]
    fn second_frame_then_keyframe_on_request() {
        let (w, h) = (96u32, 64u32);
        let stride = (w * 4) as usize;
        let buf = gradient(w, h, stride);
        let mk = || Frame {
            desc: CpuBgraFrame {
                data: buf.as_ptr(),
                len: buf.len(),
                stride,
                width: w,
                height: h,
            },
            w,
            h,
        };
        let mut enc = OpenH264Encoder::new();
        enc.configure(&default_stream_config(w, h, 60)).unwrap();
        let f0 = enc.encode(mk()).unwrap().unwrap();
        assert!(f0.is_keyframe);
        // A subsequent frame need not be a keyframe...
        let _f1 = enc.encode(mk()).unwrap();
        // ...but a requested keyframe forces an IDR again.
        enc.request_keyframe(KeyframeReason::DecoderReset);
        let f2 = enc.encode(mk()).unwrap().unwrap();
        assert!(f2.is_keyframe, "forced keyframe after request");
        assert!(nal_present(&f2.data, 7), "SPS repeated on the forced IDR");
    }

    /// Frame-varying pseudo-random content (deterministic LCG): hard to compress and different every
    /// frame, so P-frames carry real residual and the bitrate cap actually binds.
    fn noisy(w: u32, h: u32, stride: usize, seed: u32) -> Vec<u8> {
        let mut buf = vec![0u8; stride * h as usize];
        let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        for y in 0..h as usize {
            for x in 0..w as usize {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = (s >> 24) as u8;
                let i = y * stride + x * 4;
                buf[i] = n;
                buf[i + 1] = n.wrapping_add((x as u8).wrapping_add(seed as u8));
                buf[i + 2] = n.wrapping_add(y as u8);
                buf[i + 3] = 255;
            }
        }
        buf
    }

    /// Runtime ABR: after `set_bitrate` lowers the target, the live encoder must produce
    /// substantially smaller access units for the same class of content — no reconfigure, no
    /// keyframe. This exercises the `SetOption(BITRATE)` path end-to-end.
    #[test]
    fn runtime_set_bitrate_shrinks_output() {
        let (w, h) = (320u32, 240u32);
        let stride = (w * 4) as usize;
        let mut enc = OpenH264Encoder::new();
        enc.configure(&default_stream_config(w, h, 30)).unwrap();

        // Encode one noisy frame; return the produced byte length (0 if the encoder skipped it).
        fn push(enc: &mut OpenH264Encoder, w: u32, h: u32, stride: usize, seed: u32) -> usize {
            let buf = noisy(w, h, stride, seed);
            let frame = Frame {
                desc: CpuBgraFrame {
                    data: buf.as_ptr(),
                    len: buf.len(),
                    stride,
                    width: w,
                    height: h,
                },
                w,
                h,
            };
            let n = enc
                .encode(frame)
                .expect("encode ok")
                .map_or(0, |f| f.data.len());
            drop(buf); // keep the borrowed buffer alive across the encode call
            n
        }

        // Warm up at the default 8 Mbps so the rate controller converges, then measure P-frame bytes.
        for seed in 0..12 {
            push(&mut enc, w, h, stride, seed);
        }
        let high: usize = (100..140)
            .map(|seed| push(&mut enc, w, h, stride, seed))
            .sum();

        // Drop to 2 Mbps at runtime (no reconfigure / keyframe), let it converge, then measure. This
        // is low enough to clearly bind but high enough that the controller quantizes rather than
        // skipping frames outright, so we still get non-empty access units to compare.
        enc.set_bitrate(2_000_000).expect("set_bitrate ok");
        for seed in 200..230 {
            push(&mut enc, w, h, stride, seed);
        }
        let low: usize = (300..340)
            .map(|seed| push(&mut enc, w, h, stride, seed))
            .sum();

        assert!(
            high > 0 && low > 0,
            "both phases must produce frames (high={high}, low={low})"
        );
        assert!(
            low * 2 < high,
            "lowering the bitrate must shrink output (high={high} bytes, low={low} bytes)"
        );
    }
}
