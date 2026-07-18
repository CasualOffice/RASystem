# Changelog

All notable changes to Casual RAS are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project is pre-release, so everything
lives under **Unreleased** until the first tagged version. Dates are ISO-8601.

## [Unreleased]

Casual RAS is an embeddable, white-label remote-access platform (Rust core + Tauri app, peer-to-peer
over Iroh/QUIC). Priorities are strictly **Security → Latency → UX**. This log summarizes the
capabilities implemented at the code level; on-device runtime verification status is tracked in
[`docs/17`](docs/17_ROADMAP_AND_MILESTONES.md) and the production gap list in
[`docs/21`](docs/21_PRODUCTION_READINESS_BACKLOG.md).

### Added

- **Peer-to-peer session transport** over Iroh/QUIC — separate control / per-frame-video / audio /
  health planes; NAT traversal with encrypted relay fallback; connection tickets (`CASUALRAS1:`).
- **Host-issued authorization** — signed, short-lived, endpoint-bound **PASETO v4.public** session
  grants (strict Ed25519), validated host-side; capabilities enforced **per message** (never the
  controller's claimed scope). Rotating single-use connection tickets + a bounded, TTL-swept nonce
  cache. Persistent paired-controller registry + an unattended-access decision model (Tier-gated).
- **Session reconnection** (ADR-091) — on a transport loss the controller re-dials and the host
  re-serves within the reconnect window; the grant is re-validated (no new authorization path; a
  re-dial from the same authenticated endpoint resumes without re-prompting consent). Resumes across
  **all three planes** (video, control, audio) with a forced keyframe — no black screen.
- **No-black-screen guarantee** — every resync path (connect, loss recovery, resolution/monitor change,
  renderer attach, reconnect) forces an IDR before the decoder sees a frame.
- **Remote control** — OS keyboard + mouse on macOS (CGEvent), Linux (XTEST), Windows (SendInput),
  behind a control lease + per-message capability gate + emergency stop. Complete HID keymaps
  (F-keys, navigation cluster, numpad) on all three; relative-pointer, lock-state sync, Cmd↔Ctrl remap.
- **Media** — ScreenCaptureKit + VideoToolbox (macOS, hardware), PipeWire / Windows.Graphics.Capture
  + OpenH264 (cross-platform, software); WebCodecs decode; runtime latency-first ABR; mid-session
  resolution/DPI/monitor-change handling; multi-monitor + HiDPI model; out-of-band cursor-shape channel.
- **Feature set** — clipboard (both directions, set-never-paste), file transfer (signed catalogue,
  `O_NOFOLLOW`/`CREATE_NEW` write backend, all three RustDesk CVE classes structurally defended),
  output audio (Opus over QUIC datagrams), in-session chat, connection-quality diagnostics readout.
- **Tamper-evident audit** — per-session SHA-256 hash chain of content-free events, made authentic by
  a host-signed checkpoint (verification pins the trusted host key); crash-safe append-only persistence.
- **Distribution** — CI release builds (macOS/Linux/Windows viewer, macOS host); wired Ed25519
  signed auto-update (key-inert until provisioned). `SECURITY.md` vulnerability-disclosure policy.

### Security

- Fixed an **audit-checkpoint authentication bypass** — `Checkpoint::verify` verified under the
  checkpoint's *embedded* signer instead of the verifier's trusted host key (a forgery oracle); it now
  requires the trusted key.
- Fixed several **Invariant-4** gaps: emergency stop now overrides an in-flight file offer, a clipboard
  push, and a file finalize; input dispatch, clipboard, and file writes all re-check `stop` before any
  OS-visible effect.
- Hardened **session reconnection** (ADR-091) against a silent re-dialer: the host's post-reconnect
  handshake reads are now window-bounded (symmetric with the controller), so a peer that re-establishes
  the transport but never presents its grant can no longer wedge or leak the host control task; teardown
  now aborts a parked control task so an emergency stop always reclaims it (Inv 4). Found and verified by
  an adversarial multi-agent review of the reconnection path.
- **Never-panic fuzz on every untrusted-input decoder** — control framing, the video/audio wire
  headers, PASETO grants + access requests, the audit log file, and pasted connection tickets.

### Known limitations

- **Unsigned by the OS** (no code-signing / notarization yet) — Gatekeeper/SmartScreen warn (ADR-072).
- On-device runtime verification is pending on Linux and Windows (Windows needs hardware the team
  lacks); the Windows host path is CI-compile-gated only.
- Audio is **output-only** (no mic), live-only (never recorded). No secure-desktop/UAC input injection,
  by design (Invariant 14).
