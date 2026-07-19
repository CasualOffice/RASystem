//! Windows **system-audio** capture behind [`ras_media::AudioCaptureBackend`] (ADR-077 audio
//! pipeline), the Windows sibling of `ras-audio-macos`'s ScreenCaptureKit tap.
//!
//! WASAPI **loopback** taps the machine's *output* (render) audio: an `IAudioClient` initialized
//! against the **default render endpoint** with `AUDCLNT_STREAMFLAGS_LOOPBACK` delivers the exact
//! PCM being played, which an `IAudioCaptureClient::GetBuffer` loop drains. We feed the interleaved
//! i16 PCM through the pull-based trait so the host's `with_audio(...)` pump (encoded by
//! `ras-audio-opus`) streams it to the controller. Direction is host→controller only — no
//! microphone, no two-way, and no recording at rest (Inv 12).
//!
//! **Push→pull adapter.** WASAPI is event/poll based; the trait is pull-based
//! (`next_chunk(timeout)`). A dedicated capture thread runs the WASAPI loop and **appends** each
//! packet's samples into a shared FIFO (audio must not drop mid-stream — a gap is an audible
//! glitch), and `next_chunk` drains whatever has accumulated, returning `Ok(None)` on a timeout /
//! silence. A bounded cap discards the oldest samples only under pathological backpressure.
//!
//! **PCM conversion.** The default render mix format is almost always 32-bit float, interleaved,
//! at the endpoint's native rate/channels. We convert `f32 → i16` with clamping to `[-1.0, 1.0]`
//! (non-finite → silence), matching `CapturedAudio`'s interleaved-i16 contract. The int-16 mix
//! path (rare) is also handled. See `capture.rs` for the **format assumption** notes.
//!
//! **Permissions.** Loopback of the default render endpoint needs no special OS grant on desktop
//! Windows (unlike microphone capture). This crate requests nothing; the app gates streaming on the
//! `audio.listen` capability + local consent (Inv 15) and shows the Inv-7 "AUDIO SHARED" indicator.
//!
//! This is an FFI-bearing platform crate (CONTRIBUTING §5): the workspace's `unsafe_code = deny` is
//! relaxed to `allow` **here only**, with `unsafe` confined behind the safe trait surface. On
//! non-Windows targets the crate is intentionally an **empty no-op backend** so `cargo build
//! --workspace` stays green on macOS/Linux CI (the `windows` dependency is
//! `cfg(target_os = "windows")`-gated).

#[cfg(target_os = "windows")]
mod capture;
#[cfg(target_os = "windows")]
pub use capture::WindowsAudioCapture;

// ---------------------------------------------------------------------------------------------
// Non-Windows stub: an empty no-op `AudioCaptureBackend` so the workspace builds everywhere. It
// never yields audio (`start` fails fail-closed) — the app only wires it under `cfg(windows)`.
// Mirrors `ras-audio-macos`'s non-macOS stub.
// ---------------------------------------------------------------------------------------------
#[cfg(not(target_os = "windows"))]
mod stub {
    use core::time::Duration;
    use ras_media::{AudioCaptureBackend, AudioConfig, CapturedAudio, MediaError};
    use ras_protocol::{ErrorCode, RasError};

    /// No-op audio capture backend for non-Windows targets. Present so the crate compiles on every
    /// platform; `start` fails fail-closed since there is no WASAPI loopback tap here.
    #[derive(Debug, Default)]
    pub struct WindowsAudioCapture {
        config: Option<AudioConfig>,
    }

    impl WindowsAudioCapture {
        /// New, unstarted no-op backend.
        #[must_use]
        pub fn new() -> Self {
            Self { config: None }
        }
    }

    impl AudioCaptureBackend for WindowsAudioCapture {
        fn start(&mut self, _requested: &AudioConfig) -> Result<AudioConfig, MediaError> {
            Err(RasError::fatal(
                ErrorCode::CaptureFailed,
                "ras-audio-windows is a no-op on non-Windows targets",
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

#[cfg(not(target_os = "windows"))]
pub use stub::WindowsAudioCapture;
