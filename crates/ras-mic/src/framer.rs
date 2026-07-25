//! Pure, deterministic capture-framing core (no audio device, no clock) — the part of mic capture that
//! can be fully unit-tested off-device. The cpal callback pushes interleaved i16 samples in; the pull
//! side takes fixed-size frames out. **Bounded**: if the producer outruns the consumer (a stalled
//! encoder/network), the oldest samples are dropped so capture latency never grows without limit —
//! priority #2 (latency) over completeness, exactly like the video pacer drops frames.

use std::collections::VecDeque;

/// Scale a normalized float sample (`-1.0..=1.0`) to signed 16-bit PCM, clamping out-of-range input so
/// a hot mic can't wrap around into noise. Rounds to nearest.
#[must_use]
pub fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    // 32767 (not 32768) so +1.0 maps to i16::MAX and −1.0 to −32767 — symmetric, never overflows.
    (clamped * 32767.0).round() as i16
}

/// Convert an unsigned 16-bit sample (cpal `U16` format, mid-point 32768) to signed 16-bit.
#[must_use]
pub fn u16_to_i16(sample: u16) -> i16 {
    (i32::from(sample) - 32768) as i16
}

/// Accumulates interleaved i16 samples and hands out fixed-size frames.
pub struct Framer {
    buf: VecDeque<i16>,
    /// Samples per frame = channels × samples-per-channel-per-frame.
    frame_len: usize,
    /// Max buffered samples before the oldest are dropped (bounded latency).
    cap: usize,
}

impl Framer {
    /// A framer emitting `frame_len`-sample frames, buffering at most `max_frames` frames before it
    /// starts dropping the oldest samples. `frame_len` is clamped to ≥1 so it can always make progress.
    #[must_use]
    pub fn new(frame_len: usize, max_frames: usize) -> Self {
        let frame_len = frame_len.max(1);
        Self {
            buf: VecDeque::new(),
            frame_len,
            cap: frame_len * max_frames.max(1),
        }
    }

    /// Push newly captured interleaved samples. If this pushes the buffer past its cap, whole frames of
    /// the **oldest** audio are dropped first (drop in frame units so channel interleave stays aligned).
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().copied());
        while self.buf.len() > self.cap {
            // Drop one frame's worth from the front (never a partial frame — keeps L/R alignment).
            for _ in 0..self.frame_len {
                if self.buf.pop_front().is_none() {
                    break;
                }
            }
        }
    }

    /// Take one full frame if enough samples are buffered, else `None`.
    #[must_use]
    pub fn take_frame(&mut self) -> Option<Vec<i16>> {
        if self.buf.len() < self.frame_len {
            return None;
        }
        Some(self.buf.drain(..self.frame_len).collect())
    }

    /// Samples currently buffered.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Whether at least one full frame is ready.
    #[must_use]
    pub fn has_frame(&self) -> bool {
        self.buf.len() >= self.frame_len
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn f32_conversion_clamps_and_scales() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        // Out-of-range input is clamped, never wrapped.
        assert_eq!(f32_to_i16(2.5), 32767);
        assert_eq!(f32_to_i16(-9.0), -32767);
    }

    #[test]
    fn u16_conversion_is_midpoint_relative() {
        assert_eq!(u16_to_i16(32768), 0);
        assert_eq!(u16_to_i16(65535), 32767);
        assert_eq!(u16_to_i16(0), -32768);
    }

    #[test]
    fn frames_are_taken_only_when_full() {
        let mut f = Framer::new(4, 8);
        assert!(f.take_frame().is_none());
        f.push(&[1, 2, 3]);
        assert!(!f.has_frame());
        assert!(f.take_frame().is_none());
        f.push(&[4, 5]);
        assert!(f.has_frame());
        assert_eq!(f.take_frame().unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(f.buffered(), 1); // the leftover 5
        assert!(f.take_frame().is_none());
    }

    #[test]
    fn bounded_buffer_drops_oldest_whole_frames() {
        // Cap = 2 frames of 2 samples = 4 samples. Push 8 → the oldest 4 are dropped in frame units.
        let mut f = Framer::new(2, 2);
        f.push(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(f.buffered() <= 4, "buffer must stay within cap");
        // The most recent samples survive; the first frame out is not [1,2].
        let first = f.take_frame().unwrap();
        assert_ne!(first, vec![1, 2], "oldest audio should have been dropped");
        assert_eq!(first, vec![5, 6]);
    }

    #[test]
    fn zero_frame_len_is_clamped_and_does_not_panic() {
        let mut f = Framer::new(0, 0);
        f.push(&[1, 2, 3]);
        // frame_len clamped to 1, cap to 1 → always makes progress, never panics.
        assert!(f.take_frame().is_some());
    }
}
