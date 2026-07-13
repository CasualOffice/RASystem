//! Casual RAS — Phase-S media spike (throwaway).
//!
//! Runs a `FrameSource` and reports capture→encode timing (inter-frame interval, effective FPS,
//! frame sizes). With the synthetic source it validates the loop anywhere; with the Windows
//! DXGI+MF source (to implement in `frame_source.rs`) it measures real capture+encode latency.
//!
//! To measure true glass-to-glass, stream the emitted Annex-B frames to the WebCodecs harness
//! (`web/index.html`) over a localhost WebSocket — left as a TODO so this stays dependency-light.

mod frame_source;

use std::time::Instant;

use frame_source::{FrameSource, SyntheticSource};

fn main() {
    let frames: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);

    // Swap SyntheticSource for the Windows DxgiMfSource once implemented.
    let mut source = SyntheticSource::new(frames);

    let mut intervals_ms: Vec<f64> = Vec::with_capacity(frames as usize);
    let mut total_bytes: u64 = 0;
    let mut last = Instant::now();
    let start = Instant::now();
    let mut count = 0u64;

    while let Some(f) = source.next_frame() {
        let now = Instant::now();
        if count > 0 {
            intervals_ms.push((now - last).as_secs_f64() * 1000.0);
        }
        last = now;
        total_bytes += f.annexb.len() as u64;
        count += 1;
    }

    let elapsed = start.elapsed().as_secs_f64();
    let fps = if elapsed > 0.0 { count as f64 / elapsed } else { 0.0 };
    println!("frames: {count}  elapsed: {elapsed:.2} s  effective FPS: {fps:.1}");
    println!(
        "avg frame size: {:.1} KB  total: {:.1} MB",
        (total_bytes as f64 / count.max(1) as f64) / 1024.0,
        total_bytes as f64 / (1024.0 * 1024.0)
    );
    print_interval_stats(&mut intervals_ms);
    println!("(synthetic source has no real encode cost — implement the Windows DXGI+MF source in");
    println!(" frame_source.rs for real capture→encode latency; see spike/README.md)");
}

fn print_interval_stats(ms: &mut [f64]) {
    if ms.is_empty() {
        return;
    }
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |p: f64| ms[((ms.len() as f64 - 1.0) * p).round() as usize];
    println!(
        "inter-frame ms  min {:.1}  median {:.1}  p95 {:.1}  max {:.1}",
        ms[0],
        pct(0.50),
        pct(0.95),
        ms[ms.len() - 1]
    );
}
