//! Hand-rolled latency micro-benchmarks for the per-frame / per-message hot paths.
//!
//! No `criterion` on purpose: it would drag a large dev-dependency tree through the `cargo-deny`
//! license gate for numbers we mostly want as a coarse baseline. This prints ns/op and enforces a
//! *loose* sanity ceiling — enough to catch a gross regression (a hang, an accidental O(n²), an
//! allocation storm) without flaking on CI-runner noise. It is **not** a precise microbenchmark;
//! for real tuning, measure on the target hardware.
//!
//! These paths run 30–60×/second per session, so latency (priority #2) cares that they stay
//! sub-microsecond and allocation-bounded. Run: `cargo bench -p ras-core`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use ras_core::frame_channel::{encode_frame_blob, parse_header};
use ras_core::{transition, SessionEvent, SessionState};
use ras_media::{ColorSpace, EncodedFrame, StreamConfig, VideoCodec, VideoTransportKind};
use ras_protocol::{codec, ControlMsg, DecoderFeedback};

/// Loose per-op ceiling: any per-frame/per-message op above this means something is badly wrong
/// (hang, O(n²), alloc storm), not runner jitter. Fail on that; never on 2× noise.
const SANITY_CEILING_NS: f64 = 1_000_000.0; // 1 ms

/// Time `iters` invocations of `body`, returning ns/op. Warms up first so we don't measure cold
/// caches / first-touch allocation.
fn bench<F: FnMut()>(iters: u64, mut body: F) -> f64 {
    for _ in 0..(iters / 10).max(1) {
        body();
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        body();
    }
    let elapsed = t0.elapsed();
    elapsed.as_nanos() as f64 / iters as f64
}

fn sample_config() -> StreamConfig {
    StreamConfig {
        codec: VideoCodec::H264AnnexB,
        width: 1920,
        height: 1080,
        fps: 60,
        target_bitrate_bps: 8_000_000,
        color: ColorSpace::Bt709Limited,
        video_transport: VideoTransportKind::PerFrameStream,
    }
}

/// A representative encoded frame (~1 KB Annex-B payload, in line with the WebCodecs-spike chunk
/// sizes) — `encode_frame_blob` pays one copy of this at the Channel boundary.
fn sample_frame() -> EncodedFrame {
    EncodedFrame {
        frame_id: 1 << 40, // > 2^32, exercises the u64 header path
        captured_at_us: 1 << 41,
        is_keyframe: true,
        data: Bytes::from(vec![0x41u8; 1024]),
        config: sample_config(),
    }
}

fn main() {
    let mut rows: Vec<(&str, f64)> = Vec::new();

    // 1. Pure state-machine transition — runs on every control event.
    rows.push((
        "transition(Active, TransportLost)",
        bench(2_000_000, || {
            let t = transition(
                black_box(SessionState::Active),
                black_box(SessionEvent::TransportLost),
            );
            black_box(t);
        }),
    ));

    // 2. Control-message codec round-trip (frame → try_read_frame) — per control message.
    let fb = ControlMsg::Feedback(DecoderFeedback {
        last_decoded_frame: 1 << 40,
        frames_dropped: 2,
        decode_latency_us: 800,
        keyframe_request: None,
    });
    rows.push((
        "codec frame+try_read_frame (Feedback)",
        bench(500_000, || {
            let framed = codec::frame(black_box(&fb));
            let mut buf = BytesMut::from(&framed[..]);
            let msg = codec::try_read_frame(&mut buf)
                .expect("valid frame")
                .expect("complete frame");
            black_box(msg);
        }),
    ));

    // 3. Frame-Channel blob encode + header parse — per video frame (the IPC-boundary path).
    let frame = sample_frame();
    rows.push((
        "frame_channel encode_blob+parse_header",
        bench(1_000_000, || {
            let blob = encode_frame_blob(black_box(&frame));
            let hdr = parse_header(black_box(&blob)).expect("valid header");
            black_box(hdr);
        }),
    ));

    println!("\n  hot-path micro-benchmarks (ns/op, lower is better)");
    println!("  {:-<52}", "");
    for (name, nsop) in &rows {
        println!("  {name:<40} {nsop:>8.1} ns");
    }

    for (name, nsop) in &rows {
        assert!(
            *nsop < SANITY_CEILING_NS,
            "{name} took {nsop:.0} ns/op, over the {SANITY_CEILING_NS:.0} ns sanity ceiling — \
             likely a regression, not runner noise"
        );
    }
    println!("\n  all hot paths within the {SANITY_CEILING_NS:.0} ns/op sanity ceiling.\n");
}
