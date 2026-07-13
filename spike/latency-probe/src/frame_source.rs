//! Frame sources for the Phase-S media spike.
//!
//! A `FrameSource` yields encoded Annex-B H.264 frames. Two implementations:
//!   * `SyntheticSource` — std-only, generates sized dummy frames; validates the timing harness
//!     anywhere.
//!   * `windows_dxgi` (to implement, `#[cfg(windows)]`) — the real DXGI capture → Media Foundation
//!     H.264 encode path.

use std::time::Instant;

/// One encoded frame handed to the transport / decoder.
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
// Windows DXGI capture → Media Foundation H.264 encode — TO IMPLEMENT for real capture numbers.
//
// Outline (see docs/10 §2-3 and docs/11 §2 for the exact APIs and caveats):
//   1. Create D3D11 device; `IDXGIOutput1::DuplicateOutput` for the target monitor.
//   2. Loop: `AcquireNextFrame` (nonzero timeout).
//        - `DXGI_ERROR_WAIT_TIMEOUT` -> static screen; repeat last frame, continue.
//        - `DXGI_ERROR_ACCESS_LOST`  -> release + re-`DuplicateOutput` (rebuild every transition).
//      Pull separate cursor metadata via `GetFramePointerShape` (send out-of-band).
//   3. Feed the GPU texture into an async hardware H.264 MFT (`MFTEnumEx` with
//      HARDWARE|ASYNCMFT), D3D11 texture-in (zero copy via `IMFDXGIDeviceManager`).
//      Config: B-frames off (`CODECAPI_AVEncMPVDefaultBPictureCount = 0`), Main profile, CBR,
//      infinite GOP + `CODECAPI_AVEncVideoForceKeyFrame` on demand, `CODECAPI_AVLowLatencyMode`.
//      Emit Annex-B (SPS/PPS in-band). Software fallback: OpenH264 via `libloading` (never x264).
//   4. Wrap the encoder output in `EncodedFrame`; stamp `captured_at` at AcquireNextFrame time to
//      measure capture→encode latency.
//
// #[cfg(windows)]
// pub mod windows_dxgi { /* implement DxgiMfSource: FrameSource */ }
