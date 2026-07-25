# Phase Call — 1:1 voice/video calling design gate

> Design gate for the calling feature (ADR-103 media posture, ADR-104 signaling + FSM, both **Accepted**
> 2026-07). This is the plan-of-record for how calling is built; it is deliberately **bottom-up and
> additive**, mirroring how the remote-session stack was built (pure model → wire → OS → app). Nothing
> here weakens an existing invariant; the one deliberate expansion (Inv 12 media posture) is recorded in
> ADR-103 with the load-bearing "never recorded" part retained unchanged.

## 1. Goal

Add Teams/FaceTime-grade **1:1 voice and video calling** to the contacts-first shell (docs/25 §7/§12):
place a call to a saved contact, ring reliably, accept/decline, two-way audio + camera, mute/camera-off,
hang up — with an incoming-call surface and a picture-in-picture minimized state. Calling is **orthogonal
to remote access**: being in a call never grants screen view or OS control, and vice-versa.

## 2. Priorities (unchanged, §2)

Security → Latency → UX. Concretely for calls: deny-by-default mic/camera capabilities + per-message host
gating + unspoofable per-source indicators + always-available hang-up (security); reuse the datagram
(audio) and per-frame-stream (video) paths already tuned for latency, and never let a stalled camera feed
freeze audio or the End control (latency); the ring/incall/PiP polish comes third (UX).

## 3. The layers (build order)

Each layer is a separate, reviewable PR; each is testable before the next exists.

| # | Layer | Crate(s) | Verifiable off-device? | Status |
|---|-------|----------|------------------------|--------|
| L0 | **Capabilities** — `audio.mic.capture`, `video.camera.capture` (recognized-but-withheld, deny-by-default) | `ras-policy` | ✅ unit | **landed** |
| L1 | **Pure call FSM** — `CallState`/`CallEvent`/`transition`, total, fail-closed, terminal-once, clock-injected | `ras-call` (new) | ✅ unit | **landed** |
| L2 | **Signal wire (ring)** — `SignalPayload::CallInvite`/`CallCancel` (out-of-session, signed, contacts-only, replay-guarded) + the canonical `CallMediaKind` (in `ras-protocol`, shared with L3) | `ras-signal`, `ras-protocol` | ✅ unit/fuzz | **landed** |
| L3 | **In-session control wire** — `ControlMsg::CallAccept`/`CallReject`/`CallBusy`/`CallHangup`/`CallMuteState` (bounded fail-closed codec) reusing L2's `CallMediaKind` | `ras-protocol` | ✅ unit/fuzz | **landed** |
| L4a | **Pure call manager** — `CallManager` over the FSM: one-active-call, media downgrade, mute tracking, emergency-stop-overrides. Pure/stateful (no async/media) — the "brain" L4b + L6 drive | `ras-call` | ✅ unit | **landed** |
| L4b | **Runtime wiring** — drive `CallManager` from real signals/control in `ras-core`; content-free lifecycle/audit events; per-message mic/camera gate at the media boundary (lands with L5) | `ras-core` | ✅ loopback | pending |
| L5a | **Camera seam** — `CameraCaptureBackend` trait + synthetic double (a camera frame reuses the shared encoder/transport verbatim) | `ras-media` | ✅ unit | **landed** |
| L5b | **Microphone capture** — cross-platform `AudioCaptureBackend` impl via `cpal` (CoreAudio/WASAPI/ALSA); pure framing/conversion core unit-tested, cpal glue compile-gated | `ras-mic` | ◐ core-unit / device on-device | **landed (core-verified)** |
| L5c | **Camera + reuse** — per-OS `CameraCaptureBackend` impls (AVFoundation / Media Foundation / PipeWire); reuse Opus (audio) + `VideoEncoderBackend`/`PerFrameStream` (camera) verbatim | `ras-camera-{macos,windows,linux}` | ⚠️ on-device | pending |
| L6 | **App/shell** — real ring signal, incoming-call window, in-call stage, PiP, mute/camera toggles, ringtone; replace the current designed previews | `app/` | ⚠️ on-device | pending |

L0–L4 are fully verifiable off-device (the project's established bar); L5–L6 need real hardware (mic,
camera, TCC/permission prompts, two machines).

## 4. This gate's deliverable (L0 + L1)

- **L0** — two capabilities in `ras-policy`, added to `CATALOGUE_V1` (so a request naming them is
  *understood*, Inv 2) but **absent from every default-grantable policy** (`phase2`/`phase3`), so a
  request + consent for them grants nothing under the default policy. Unit-tested parallel to
  `audio.listen`.
- **L1** — `ras-call`: a pure `transition(CallState, CallEvent) -> CallTransition` mirroring the session
  FSM. States: `Idle → Dialing|Ringing → Connecting → Active`, terminals `Ended`/`Declined`/`Missed`/
  `Failed`. Load-bearing properties, all unit-tested:
  - **Inv 1** — an inbound ring stays in `Ringing` until `LocalAccept`; a stray `MediaConnected`/
    `RemoteAccepted` in `Ringing` is `Invalid` (consent can't be bypassed by an out-of-order event).
  - **Inv 4** — `Hangup` is valid from *every* in-progress state → `Ended` (emergency stop can always end
    a call); it is `Invalid` from `Idle` (nothing to end) and terminals.
  - **Fail-closed** — any pairing not explicitly allowed is `Invalid`; terminals absorb every event.
  - **Content-free** — every type is `Copy` tags + `ErrorCode`; no label, pixel, or sample (Inv 8).

## 5. Security posture (carried from ADR-103/104)

- **Consent (Inv 1).** A `CallInvite` authorizes nothing (an intent, like `AccessRequestIntent`). The
  callee's explicit `LocalAccept` is what triggers the media session dial. Mic and camera each turn on
  only inside an accepted, live call — never ambiently.
- **Capabilities (Inv 2/15).** `audio.mic.capture` / `video.camera.capture` are deny-by-default and
  enforced **per-message host-side** — a captured mic/camera frame is authorized against the live grant at
  the moment of send, so "in a call" ≠ "may be heard/seen." Muting/camera-off toggles the capability
  mid-call without ending it. Each side grants these for *its own* mic/camera; a peer can never enable
  your capture.
- **Never recorded (Inv 12 core, retained).** Call media is live-only: encode→wire→decode→play/render,
  never written to disk, never buffered for replay. No recording path exists or is added.
- **Disclosure (Inv 7).** Mic and camera each get their own always-visible active indicator, backed by
  the secure-window capture-exclusion so it can't be re-captured/spoofed. Stop/End is always present.
- **Replay/rate (ADR-102).** The ring is Ed25519-signed, contacts-only, freshness-bounded, and
  `SignalReplayGuard`-deduped; a flood is answered `Busy` (one-active-call), not surfaced.
- **Emergency stop (Inv 4).** A hang-up or the global stop drives the FSM to a terminal state and tears
  down media capture synchronously before its next send.

## 6. Explicitly out of scope

Group calls (1:1 only); any recording/persistence (would reverse Inv 12's core — a separate ADR);
call-media entering the fraud subsystem (forbidden — content-at-rest, Inv 11); an in-app camera/mic
*preview* that persists (preview is live-only like the call).

## 7. Deviations from the ADR notes (as landed)

- The FSM shipped as crate **`ras-call`**, not a `ras-core::call` module (crate-per-concern, dependency-
  light, `ras-core`-free) — recorded in ADR-104's deviation note.
- `CallMedia { Voice, Video }` lives in `ras-call` for now; it becomes / is re-exported as
  `CallMediaKind` from `ras-protocol` when the L3 wire lands (one canonical type across both planes).
- One-active-call is a caller-side rule layered on the FSM (the FSM refuses a second `Dial`/`Ring` from a
  non-`Idle` state), landing with L2/L4.
