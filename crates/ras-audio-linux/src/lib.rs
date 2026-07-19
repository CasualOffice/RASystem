//! Linux **system-audio** capture behind [`ras_media::AudioCaptureBackend`] (ADR-077 audio pipeline).
//!
//! The Linux sibling of `ras-audio-macos`. Where macOS taps output audio through ScreenCaptureKit,
//! Linux records the default sink's **monitor source** — every PulseAudio sink `foo` exposes a
//! read-only `foo.monitor` source carrying exactly what is being played back, which is the machine's
//! *output* audio. We open a PulseAudio **record** stream against that monitor source and feed the
//! interleaved i16 PCM through the pull-based trait so the host's `with_audio(...)` pump (encoded by
//! `ras-audio-opus`) streams it to the controller. Direction is host→controller only — no microphone,
//! no two-way voice, no recording (Inv 12: audio is live-only).
//!
//! **Why PulseAudio and not raw ALSA/PipeWire native.** The monitor-source concept is a PulseAudio
//! abstraction, and PipeWire ships `pipewire-pulse` — a drop-in PulseAudio server shim exposing the
//! identical `libpulse` ABI — on every modern desktop. So a single `libpulse` client works against
//! *both* a classic PulseAudio server and a PipeWire system, which is the portable path. Raw ALSA has
//! no portable "capture what's playing" and raw PipeWire (`pipewire` crate) would need its own graph
//! wiring for the same result.
//!
//! **Sample format.** We request the sample-spec `S16NE` (signed-16 native-endian), 48 kHz, stereo
//! directly from PulseAudio, so the *server* does any float→i16 conversion and resampling and hands us
//! interleaved i16 already in `CapturedAudio`'s contract — no client-side conversion on the happy
//! path. (A defensive f32→i16 helper is kept for the raw-format fallback and unit-tested.)
//!
//! **Push→pull adapter.** `libpulse-simple`'s `read()` is a *blocking pull* — the opposite of macOS's
//! push delegate — so there is no separate FIFO thread: `next_chunk(timeout)` reads one bounded block
//! straight off the record stream. Audio must not drop mid-stream (a gap is an audible glitch), so we
//! read a fixed small block (~20 ms) and return it whole; `Ok(None)` is reserved for a clean timeout /
//! silence path. The blocking read is bounded by the requested block size, so the pump stays responsive.
//!
//! **Permissions.** Recording a monitor source needs access to the user's PulseAudio/PipeWire session
//! socket (the normal desktop case — no elevation). There is no separate OS permission prompt like
//! macOS TCC; if the session bus is unreachable, `start` fails fail-closed and the host refuses the
//! audio plane.
//!
//! This is a platform backend crate (CONTRIBUTING §5): the workspace's `unsafe_code = deny` is relaxed
//! to `allow` **here only** for parity with the other backend crates, though the safe `libpulse-*`
//! binding crates mean we currently use no `unsafe` at all. On non-Linux targets the crate is
//! intentionally an **empty no-op backend** so `cargo build --workspace` stays green on macOS/Windows
//! CI (the `libpulse-*` dependencies are `cfg(target_os = "linux")`-gated).

#[cfg(target_os = "linux")]
mod capture;
#[cfg(target_os = "linux")]
pub use capture::LinuxAudioCapture;

// ---------------------------------------------------------------------------------------------
// Non-Linux stub: an empty no-op `AudioCaptureBackend` so the workspace builds everywhere. It
// never yields audio (`start` fails fail-closed) — the app only wires it under `cfg(linux)` anyway.
// Mirrors `ras-audio-macos`'s non-macOS stub exactly.
// ---------------------------------------------------------------------------------------------
#[cfg(not(target_os = "linux"))]
mod stub {
    use core::time::Duration;
    use ras_media::{AudioCaptureBackend, AudioConfig, CapturedAudio, MediaError};
    use ras_protocol::{ErrorCode, RasError};

    /// No-op audio capture backend for non-Linux targets. Present so the crate compiles on every
    /// platform; `start` fails fail-closed since there is no PulseAudio monitor source here.
    #[derive(Debug, Default)]
    pub struct LinuxAudioCapture {
        config: Option<AudioConfig>,
    }

    impl LinuxAudioCapture {
        /// New, unstarted no-op backend.
        #[must_use]
        pub fn new() -> Self {
            Self { config: None }
        }
    }

    impl AudioCaptureBackend for LinuxAudioCapture {
        fn start(&mut self, _requested: &AudioConfig) -> Result<AudioConfig, MediaError> {
            Err(RasError::fatal(
                ErrorCode::CaptureFailed,
                "ras-audio-linux is a no-op on non-Linux targets",
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

#[cfg(not(target_os = "linux"))]
pub use stub::LinuxAudioCapture;
