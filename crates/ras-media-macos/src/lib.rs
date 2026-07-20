//! macOS host media backend (ADR-054): **ScreenCaptureKit** capture + **VideoToolbox** H.264 encode
//! behind the `ras-media` traits ([`ras_media::ScreenCaptureBackend`] /
//! [`ras_media::VideoEncoderBackend`]).
//!
//! This is an FFI-bearing platform crate (CONTRIBUTING §5): the workspace's `unsafe_code = deny` is
//! relaxed to `allow` **here only**, with `unsafe` confined behind the safe trait surface — no raw
//! pointers/handles escape. It uses the pure-Rust `objc2` framework bindings (no Swift bridge),
//! validated end-to-end by `spike/macos-capture` (`docs/design/phase-S-design.md §4.1`).
//!
//! On non-macOS targets the crate is intentionally **empty** so `cargo build --workspace` stays
//! green on Linux CI (the `objc2` dependencies are `cfg(target_os = "macos")`-gated).

#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod cursor;
#[cfg(target_os = "macos")]
mod encode;

#[cfg(target_os = "macos")]
pub use capture::MacScreenCapture;
#[cfg(target_os = "macos")]
pub use cursor::{DisplayBounds, MacCursorObserver};
#[cfg(target_os = "macos")]
pub use encode::VideoToolboxEncoder;

#[cfg(target_os = "macos")]
pub(crate) use imp::*;

#[cfg(target_os = "macos")]
mod imp {
    use ras_media::{ColorSpace, StreamConfig, VideoCodec, VideoTransportKind};

    /// Annex-B NAL start code prefixed before every NAL unit.
    pub(crate) const START_CODE: [u8; 4] = [0, 0, 0, 1];

    /// Default per-frame bitrate target (8 Mbps) — realistic for a full-screen desktop feed; the ABR
    /// hook in `ras-core` retargets it per RTT via [`ras_media::VideoEncoderBackend::set_bitrate`].
    pub(crate) const DEFAULT_BITRATE_BPS: u32 = 8_000_000;

    /// The single-monitor [`StreamConfig`] the macOS backends negotiate. H.264 Annex-B, limited-range
    /// BT.709, per-frame-stream video transport (the post-spike default).
    #[must_use]
    pub(crate) fn default_stream_config(width: u32, height: u32, fps: u32) -> StreamConfig {
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

    /// A `Send` shim for a single-thread-owned `objc2`/CoreFoundation handle.
    ///
    /// The `ras-media` traits require `Send` (the backend is *moved* onto `ras-core`'s dedicated
    /// media thread), but `objc2`'s `Retained`/`CFRetained` are conservatively `!Send`. Both the
    /// `SCStream` and `VTCompressionSession` handles wrapped here are, after that move, used **only**
    /// from that one thread and never shared — and both APIs are additionally documented thread-safe.
    pub(crate) struct Sendable<T>(T);

    impl<T> Sendable<T> {
        /// Wrap a handle as `Send`.
        ///
        /// # Safety
        /// The caller guarantees the wrapped handle is only ever accessed from the single thread that
        /// owns the backend after it is moved there, and is never shared across threads.
        pub(crate) unsafe fn new(v: T) -> Self {
            Self(v)
        }
    }

    impl<T> core::ops::Deref for Sendable<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }

    impl<T> core::ops::DerefMut for Sendable<T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.0
        }
    }

    // SAFETY: see the type-level doc — single-thread ownership after the move; handles never shared.
    unsafe impl<T> Send for Sendable<T> {}
}
