//! Frame sources for the Phase-S media spike.
//!
//! A `FrameSource` yields encoded Annex-B H.264 frames. Two implementations:
//!   * `SyntheticSource` — std-only, generates sized dummy frames; validates the timing harness
//!     anywhere.
//!   * `windows_dxgi` (to implement, `#[cfg(windows)]`) — the real DXGI capture → Media Foundation
//!     H.264 encode path.

use std::time::Instant;

/// One encoded frame handed to the transport / decoder.
// `seq`/`is_keyframe` are consumed by the real transport + decoder wiring (not the synthetic loop).
#[allow(dead_code)]
pub struct EncodedFrame {
    pub seq: u64,
    /// Wall-clock capture instant, for measuring capture→encode latency.
    pub captured_at: Instant,
    pub is_keyframe: bool,
    /// Annex-B bitstream (start-code delimited; SPS/PPS in-band on keyframes).
    pub annexb: Vec<u8>,
}

/// A source of encoded frames.
pub trait FrameSource {
    /// Produce the next frame, or `None` when the source is exhausted/stopped.
    fn next_frame(&mut self) -> Option<EncodedFrame>;
}

/// Std-only synthetic source: fixed-size payloads, a keyframe every `gop` frames. It does NOT
/// produce real H.264 — it exists to exercise the timing loop and (later) the transport. Real
/// encode-latency numbers come from the Windows source.
pub struct SyntheticSource {
    seq: u64,
    remaining: u64,
    gop: u64,
    frame_bytes: usize,
}

impl SyntheticSource {
    pub fn new(frames: u64) -> Self {
        Self {
            seq: 0,
            remaining: frames,
            gop: 60,
            frame_bytes: 12_000,
        }
    }
}

impl FrameSource for SyntheticSource {
    fn next_frame(&mut self) -> Option<EncodedFrame> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let is_keyframe = self.seq % self.gop == 0;
        let f = EncodedFrame {
            seq: self.seq,
            captured_at: Instant::now(),
            is_keyframe,
            annexb: vec![0u8; self.frame_bytes],
        };
        self.seq += 1;
        Some(f)
    }
}

// ---------------------------------------------------------------------------------------------
// macOS (DEVELOPMENT LEAD, ADR-054): ScreenCaptureKit capture → VideoToolbox H.264 encode.
// TO IMPLEMENT for real capture→encode numbers on the Mac. See docs/18 (macOS host deep-dive).
//
// Outline:
//   1. Request the Screen-Recording TCC permission (SCShareableContent triggers the prompt).
//   2. Build an `SCStream` with `SCContentFilter` (target display) + `SCStreamConfiguration`
//      (pixelFormat BGRA/NV12, minimumFrameInterval for target FPS, showsCursor as desired).
//      Frames arrive as `CMSampleBuffer` wrapping a `CVPixelBuffer`/`IOSurface` (GPU, zero-copy).
//   3. Feed the CVPixelBuffer into a `VTCompressionSession` configured for low latency:
//        kVTCompressionPropertyKey_RealTime = true,
//        AllowFrameReordering = false            (no B-frames — the real latency win),
//        ProfileLevel = H264_Main_AutoLevel, AverageBitRate (CBR-ish),
//        MaxKeyFrameInterval large + force IDR on demand via the per-frame frameProperties
//        (kVTEncodeFrameOptionKey_ForceKeyFrame). Emit Annex-B (SPS/PPS in-band).
//   4. In the VT output callback, wrap the sample into `EncodedFrame`; stamp `captured_at` at the
//      SCStream frame time to measure capture→encode latency. Apple-Silicon media engine gives
//      hardware encode for free.
//
// Rust crates to evaluate (verify licenses — must be permissive per ADR-051):
//   screencapturekit, core-media/core-video (objc2/icrate family), VideoToolbox via objc2 or a
//   bindgen shim. Accessibility (AXIsProcessTrusted) is only needed for INPUT injection, not capture.
//
// #[cfg(target_os = "macos")]
// pub mod macos_sck { /* implement SckVtSource: FrameSource */ }
//
// WINDOWS PORT (later): DXGI Desktop Duplication → Media Foundation H.264 (see docs/11 §2):
//   IDXGIOutput1::DuplicateOutput; AcquireNextFrame with WAIT_TIMEOUT/ACCESS_LOST handling;
//   async HW MFT with D3D11 texture-in; B-frames off, Main, CBR, infinite-GOP + ForceKeyFrame.
//
// #[cfg(windows)]
// pub mod windows_dxgi { /* implement DxgiMfSource: FrameSource */ }
