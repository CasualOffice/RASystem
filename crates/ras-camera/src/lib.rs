//! Camera capture for 1:1 calls (ADR-103, L5c) behind [`ras_media::CameraCaptureBackend`].
//!
//! A camera frame is converted to CPU BGRA and exposed as a [`ras_media::CapturedFrame`] with a
//! [`ras_media::SurfaceKind::CpuBgra`] surface — the exact input the shared software encoder already
//! consumes — so the camera reuses the video encoder + transport + WebCodecs decode verbatim, with no
//! new codec or wire (ADR-103 §reuse-the-pipeline).
//!
//! The pixel-conversion + frame-adapter core ([`convert`]) is pure and unit-tested off-device. The
//! concrete OS capture (nokhwa: AVFoundation / Media Foundation / V4L2) is behind the OFF-by-default
//! `capture` feature, so the default build carries no camera system dependency; the backend needs real
//! hardware + an OS camera-permission grant to run (on-device follow-up).

pub mod convert;
pub use convert::{rgb8_to_bgra, rgba8_to_bgra, CameraBuf, CameraFrame};

#[cfg(all(
    feature = "capture",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
mod backend;
#[cfg(all(
    feature = "capture",
    any(target_os = "macos", target_os = "linux", target_os = "windows")
))]
pub use backend::NokhwaCameraCapture;
