//! Runnable end-to-end demo of the Phase-1 spine — **no iroh, no OS, no GPU**.
//!
//! Wires a [`HostSession`] (synthetic capture + encode) to a [`ControllerSession`] over the in-memory
//! loopback transport and streams frames into a printing [`FrameSink`], so you can watch the whole
//! orchestration — handshake, authorize gate, droppable video, keyframe round-trip, adaptive-bitrate
//! ticks — actually run on your machine today.
//!
//! Run it:
//! ```text
//! cargo run -p ras-core --example loopback_demo --features testkit
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ras_core::deps::AllowAllValidator;
use ras_core::media::synthetic::{SyntheticCaptureBackend, SyntheticEncoder};
use ras_core::media::{MonitorId, StreamConfig};
use ras_core::testkit::loopback_pair;
use ras_core::transport::{EndpointAddr, EndpointId};
use ras_core::{
    ControllerSession, ControllerSessionConfig, CoreError, FrameSink, HostSession,
    HostSessionConfig, LifecycleEvent, PushResult, StopReason,
};

/// A `FrameSink` that just prints what arrives (a stand-in for the WebCodecs worker).
#[derive(Default)]
struct PrintingSink {
    frames: AtomicU64,
    keyframes: AtomicU64,
}

impl FrameSink for PrintingSink {
    fn configure(&self, config: &StreamConfig) -> Result<(), CoreError> {
        println!(
            "  [renderer] configured: {}x{} @ {} fps, codec {}",
            config.width,
            config.height,
            config.fps,
            config.codec.webcodecs_string(config.width, config.height)
        );
        Ok(())
    }

    fn push(&self, frame: ras_core::media::EncodedFrame) -> PushResult {
        let n = self.frames.fetch_add(1, Ordering::Relaxed) + 1;
        if frame.is_keyframe {
            self.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        // Print the first few, then every 30th, plus every keyframe — enough to see it flow.
        if n <= 3 || n.is_multiple_of(30) || frame.is_keyframe {
            println!(
                "  [renderer] frame #{:<4} id={:<4} {:>3} bytes{}",
                n,
                frame.frame_id,
                frame.data.len(),
                if frame.is_keyframe { "  <IDR>" } else { "" }
            );
        }
        PushResult::Sent
    }
}

#[tokio::main]
async fn main() -> Result<(), CoreError> {
    println!("Casual RAS — loopback demo (synthetic capture → controller, in-memory transport)\n");

    let (host_tp, ctrl_tp) = loopback_pair();

    let host = HostSession::new(
        HostSessionConfig::new(MonitorId(0)),
        host_tp,
        SyntheticCaptureBackend::new(1280, 720),
        SyntheticEncoder::new(),
        Arc::new(AllowAllValidator),
    );
    let controller = ControllerSession::new(
        ControllerSessionConfig::new(EndpointAddr::new(EndpointId([0u8; 32]))),
        ctrl_tp,
    );

    println!("• host.start()  — accept, handshake, authorize gate, negotiate stream");
    let mut host_events = host.start().await?;
    println!("• controller.connect()");
    let _ctrl_events = controller.connect().await?;
    println!(
        "• host={:?}  controller={:?}\n",
        host.state(),
        controller.state()
    );

    let sink = Arc::new(PrintingSink::default());
    controller.attach_renderer(sink.clone()).await?;

    // Let it stream for a bit; surface a couple of host lifecycle events as they arrive.
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        while let Ok(ev) = host_events.try_recv() {
            if let LifecycleEvent::ConnectionQuality { sample } = ev {
                println!(
                    "  [quality] {:?} path, {} ms RTT, {} fps delivered, bitrate {} kbps",
                    sample.path,
                    sample.rtt_ms,
                    sample.delivered_fps,
                    host.current_bitrate_bps() / 1000
                );
            }
        }
    }

    println!("\n• controller.request_keyframe() — PLI round-trip over the control channel");
    controller
        .request_keyframe(ras_core::protocol::KeyframeReason::UnrecoverableLoss)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    println!("\n• teardown");
    controller.disconnect(StopReason::UserRequested).await;
    host.stop(StopReason::UserRequested).await;

    println!(
        "\nDone. delivered {} frames ({} keyframes). host={:?} controller={:?}",
        sink.frames.load(Ordering::Relaxed),
        sink.keyframes.load(Ordering::Relaxed),
        host.state(),
        controller.state()
    );
    Ok(())
}
