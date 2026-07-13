//! On-device verification for `ras-media-macos`: drive real ScreenCaptureKit capture → VideoToolbox
//! encode **through the `ras-media` traits** and validate the `EncodedFrame` contract end to end.
//!
//! Run on a Mac with a GUI session + Screen-Recording permission (NOT headless/SSH):
//!   cargo run -p ras-media-macos --example capture_encode                # 120 frames → capture_traits.h264
//!   cargo run -p ras-media-macos --example capture_encode -- 300 out.h264
//!
//! Verify the output decodes:  ffprobe -show_streams capture_traits.h264

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("capture_encode is macOS-only (ras-media-macos is empty on other targets).");
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::cmp::Ordering;
    use std::io::Write;
    use std::time::{Duration, Instant};

    use ras_media::{CaptureOptions, MonitorId, ScreenCaptureBackend, VideoEncoderBackend};
    use ras_media_macos::{MacScreenCapture, VideoToolboxEncoder};

    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let out_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "capture_traits.h264".to_string());

    let mut cap = MacScreenCapture::new();
    let cfg = cap.start(&CaptureOptions {
        monitor: MonitorId(0),
        target_fps: 60,
        excluded_window_ids: vec![],
    })?;
    println!(
        "negotiated: {}x{} @ {}fps, {} bps",
        cfg.width, cfg.height, cfg.fps, cfg.target_bitrate_bps
    );

    let mut enc = VideoToolboxEncoder::new();
    enc.configure(&cfg)?;

    let mut file = std::fs::File::create(&out_path)?;
    let mut frames = 0usize;
    let mut keyframes = 0usize;
    let mut bytes = 0usize;
    let mut first_is_keyframe: Option<bool> = None;
    let mut monotonic = true;
    let mut prev_id: Option<u64> = None;
    let mut lat_ms: Vec<f64> = Vec::new();
    let timeout = Duration::from_millis(200);

    while frames < n {
        let Some(frame) = cap.next_frame(timeout)? else {
            continue; // static screen: SCK coalesces; keep waiting
        };
        let t0 = Instant::now();
        let Some(ef) = enc.encode(frame)? else {
            continue;
        };
        lat_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        // Contract checks (the reason this example exists).
        assert_eq!(&ef.data[..4], &[0, 0, 0, 1], "every AU is Annex-B start-code framed");
        if first_is_keyframe.is_none() {
            first_is_keyframe = Some(ef.is_keyframe);
        }
        if let Some(p) = prev_id {
            if ef.frame_id != p + 1 {
                monotonic = false;
            }
        }
        prev_id = Some(ef.frame_id);
        if ef.is_keyframe {
            keyframes += 1;
            assert_eq!(ef.data[4] & 0x1F, 7, "keyframe carries SPS (NAL 7) in-band");
        }

        file.write_all(&ef.data)?;
        bytes += ef.data.len();
        frames += 1;
    }
    cap.stop();
    file.flush()?;

    lat_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let med = lat_ms.get(lat_ms.len() / 2).copied().unwrap_or(0.0);
    println!(
        "frames={frames} keyframes={keyframes} first_is_keyframe={first_is_keyframe:?} \
         monotonic_ids={monotonic}"
    );
    println!(
        "mean {:.1} KB/frame · encode(+sync drain) median {med:.2} ms · wrote {out_path}",
        bytes as f64 / frames.max(1) as f64 / 1024.0
    );
    assert_eq!(first_is_keyframe, Some(true), "the first emitted frame must be a keyframe");
    assert!(monotonic, "frame ids must be gap-free monotonic");
    println!("OK — capture→encode via the ras-media traits produced a valid Annex-B stream.");
    println!("Verify decode:  ffprobe -v error -show_entries stream=codec_name,width,height -of default=nk=1 {out_path}");
    Ok(())
}
