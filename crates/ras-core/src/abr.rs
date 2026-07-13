//! A concrete latency-first adaptive-bitrate controller (design §3.6 / Q-ABR-HOME).
//!
//! The design intentionally left the *control law* for the Phase-S bandwidth-estimate numbers to
//! tune; this is a sane, conservative default that already honors the priority order: it **caps
//! bitrate to the deliverable rate** (never queues to "use" bandwidth that isn't there), reacts
//! **every tick via `set_bitrate`** (keyframe-free — an IDR spikes bitrate and hurts latency), and
//! reserves a forced keyframe strictly for a genuine decoder resync signalled by feedback. FEC in
//! the transport is the preferred loss response, so loss here only *lowers the target*.

use crate::{AdaptiveBitrateController, BitrateDecision};
use ras_protocol::DecoderFeedback;
use ras_transport_iroh::ConnHealth;

/// Loss fraction above which we actively back the bitrate off.
const LOSS_BACKOFF_THRESHOLD: f32 = 0.02;
/// Multiplicative decrease applied when loss exceeds the threshold (AIMD-style).
const BACKOFF_FACTOR: f32 = 0.8;
/// Fraction of the estimated deliverable rate we allow ourselves to fill (headroom for pacing).
const BANDWIDTH_UTILISATION: f32 = 0.9;

/// Latency-first ABR. Additive-increase toward the bandwidth-derived ceiling, multiplicative
/// decrease on loss; the target is always clamped to `[floor, ceiling]`.
#[derive(Debug, Clone)]
pub struct LatencyFirstAbr {
    floor_bps: u32,
    ceiling_bps: u32,
    current_bps: u32,
    /// Additive step per tick when we have headroom.
    step_bps: u32,
}

impl LatencyFirstAbr {
    /// New controller. `max_bps` is the negotiated session ceiling; `initial_bps`/`floor_bps` are
    /// clamped into `[floor, max]`. The additive step is 10% of the ceiling.
    #[must_use]
    pub fn new(floor_bps: u32, max_bps: u32, initial_bps: u32) -> Self {
        let ceiling_bps = max_bps.max(floor_bps);
        Self {
            floor_bps,
            ceiling_bps,
            current_bps: initial_bps.clamp(floor_bps, ceiling_bps),
            step_bps: (ceiling_bps / 10).max(1),
        }
    }

    /// The bitrate the controller currently targets.
    #[must_use]
    pub fn current_bps(&self) -> u32 {
        self.current_bps
    }
}

impl AdaptiveBitrateController for LatencyFirstAbr {
    fn on_tick(
        &mut self,
        health: &ConnHealth,
        feedback: Option<DecoderFeedback>,
    ) -> BitrateDecision {
        // Ceiling for *this* tick: the smaller of the session ceiling and a fraction of what the
        // path can actually deliver. `as` truncation is fine — bitrates are far below u32::MAX here.
        let deliverable = (health.estimated_bandwidth_bps as f32 * BANDWIDTH_UTILISATION) as u32;
        let tick_ceiling = self.ceiling_bps.min(deliverable).max(self.floor_bps);

        if health.loss_fraction > LOSS_BACKOFF_THRESHOLD {
            // Multiplicative decrease; keep the last-good frame on screen (FEC, not IDR, recovers).
            let reduced = (self.current_bps as f32 * BACKOFF_FACTOR) as u32;
            self.current_bps = reduced.clamp(self.floor_bps, tick_ceiling);
        } else {
            // Additive increase toward the ceiling.
            let raised = self.current_bps.saturating_add(self.step_bps);
            self.current_bps = raised.clamp(self.floor_bps, tick_ceiling);
        }

        // Latency-first: only force an IDR when the decoder itself asks for one (resync), never as a
        // routine loss response.
        let force_keyframe = feedback
            .and_then(|f| f.keyframe_request)
            .map(|kr| kr.reason);

        BitrateDecision {
            target_bitrate_bps: self.current_bps,
            force_keyframe,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_protocol::{KeyframeReason, KeyframeRequest};
    use ras_transport_iroh::{LinkState, PathKind};

    fn health(bw_bps: u32, loss: f32) -> ConnHealth {
        ConnHealth {
            path: PathKind::Direct,
            rtt_us: 8_000,
            loss_fraction: loss,
            estimated_bandwidth_bps: bw_bps,
            frames_dropped: 0,
            state: LinkState::Live,
        }
    }

    #[test]
    fn caps_to_ninety_percent_of_deliverable_bandwidth() {
        let mut abr = LatencyFirstAbr::new(500_000, 10_000_000, 500_000);
        // Bandwidth is the binding constraint: 4 Mbps → ceiling 3.6 Mbps. Drive several ticks up.
        for _ in 0..100 {
            abr.on_tick(&health(4_000_000, 0.0), None);
        }
        assert_eq!(abr.current_bps(), 3_600_000);
    }

    #[test]
    fn respects_session_ceiling_when_bandwidth_is_abundant() {
        let mut abr = LatencyFirstAbr::new(500_000, 6_000_000, 500_000);
        for _ in 0..200 {
            abr.on_tick(&health(100_000_000, 0.0), None);
        }
        assert_eq!(abr.current_bps(), 6_000_000);
    }

    #[test]
    fn backs_off_on_loss_but_never_below_floor() {
        let mut abr = LatencyFirstAbr::new(1_000_000, 10_000_000, 8_000_000);
        for _ in 0..50 {
            abr.on_tick(&health(50_000_000, 0.10), None);
        }
        assert_eq!(abr.current_bps(), 1_000_000);
    }

    #[test]
    fn forwards_decoder_keyframe_request_but_not_otherwise() {
        let mut abr = LatencyFirstAbr::new(500_000, 6_000_000, 2_000_000);
        assert_eq!(
            abr.on_tick(&health(50_000_000, 0.0), None).force_keyframe,
            None
        );
        let fb = DecoderFeedback {
            last_decoded_frame: 10,
            frames_dropped: 0,
            decode_latency_us: 5_000,
            keyframe_request: Some(KeyframeRequest {
                since_frame: 10,
                reason: KeyframeReason::DecoderReset,
            }),
        };
        assert_eq!(
            abr.on_tick(&health(50_000_000, 0.0), Some(fb))
                .force_keyframe,
            Some(KeyframeReason::DecoderReset)
        );
    }
}
