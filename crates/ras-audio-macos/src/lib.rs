//! macOS **system-audio** capture behind [`ras_media::AudioCaptureBackend`] (ADR-077 audio pipeline).
//!
//! ScreenCaptureKit can tap the machine's *output* audio: an `SCStream` configured with
//! `capturesAudio = true` delivers audio sample buffers (`SCStreamOutputType::Audio`) alongside — or,
//! as here, instead of — video. We run an **audio-only** stream (minimal video config as SCK still
//! requires a display content filter) with `excludesCurrentProcessAudio = true` so we never capture
//! our own playback, and feed the interleaved i16 PCM through the pull-based trait so the host's
//! `with_audio(...)` pump (encoded by `ras-audio-opus`) streams it to the controller.
//!
//! **Push→pull adapter.** SCK is push-based (it calls an `SCStreamOutput` delegate on a private
//! dispatch queue); the trait is pull-based (`next_chunk(timeout)`). Unlike video (freshest-wins,
//! droppable), audio must not drop samples mid-stream — a gap is an audible glitch — so the delegate
//! **appends** each buffer's samples into a shared FIFO and `next_chunk` drains whatever has
//! accumulated, returning `Ok(None)` on a timeout / silence. A bounded cap discards the oldest
//! samples only under pathological backpressure (the pump should keep up at 48 kHz).
//!
//! **PCM conversion.** SCK audio is 32-bit float, interleaved, at the configured sample-rate /
//! channel-count. We read the single `AudioBuffer` out of the `AudioBufferList` and convert
//! `f32 → i16` with clamping to `[-1.0, 1.0]` (SCK samples can transiently exceed unity), matching
//! `CapturedAudio`'s interleaved-i16 contract.
//!
//! **TCC.** System-audio capture is gated by the **Screen-Recording** permission (the same grant as
//! video). This crate does not request it — the app surfaces the prompt (as it does for video).
//!
//! This is an FFI-bearing platform crate (CONTRIBUTING §5): the workspace's `unsafe_code = deny` is
//! relaxed to `allow` **here only**, with `unsafe` confined behind the safe trait surface. On
//! non-macOS targets the crate is intentionally an **empty no-op backend** so `cargo build
//! --workspace` stays green on Linux/Windows CI (the `objc2` dependencies are
//! `cfg(target_os = "macos")`-gated).

#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
pub use capture::MacAudioCapture;

// ---------------------------------------------------------------------------------------------
// Non-macOS stub: an empty no-op `AudioCaptureBackend` so the workspace builds everywhere. It
// never yields audio (`start` fails fail-closed) — the app only wires it under `cfg(macos)` anyway.
// ---------------------------------------------------------------------------------------------
#[cfg(not(target_os = "macos"))]
mod stub {
    use core::time::Duration;
    use ras_media::{AudioCaptureBackend, AudioConfig, CapturedAudio, MediaError};
    use ras_protocol::{ErrorCode, RasError};

    /// No-op audio capture backend for non-macOS targets. Present so the crate compiles on every
    /// platform; `start` fails fail-closed since there is no macOS SCK audio tap here.
    #[derive(Debug, Default)]
    pub struct MacAudioCapture {
        config: Option<AudioConfig>,
    }

    impl MacAudioCapture {
        /// New, unstarted no-op backend.
        #[must_use]
        pub fn new() -> Self {
            Self { config: None }
        }
    }

    impl AudioCaptureBackend for MacAudioCapture {
        fn start(&mut self, _requested: &AudioConfig) -> Result<AudioConfig, MediaError> {
            Err(RasError::fatal(
                ErrorCode::CaptureFailed,
                "ras-audio-macos is a no-op on non-macOS targets",
            ))
        }

        fn next_chunk(&mut self, _timeout: Duration) -> Result<Option<CapturedAudio>, MediaError> {
            Ok(None)
        }

        fn config(&self) -> AudioConfig {
            self.config.unwrap_or(AudioConfig {
                codec: ras_media::AudioCodec::Opus,
                sample_rate_hz: 48_000,
                channels: 2,
                frame_duration_us: 20_000,
                target_bitrate_bps: 96_000,
            })
        }

        fn stop(&mut self) {}
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::MacAudioCapture;
