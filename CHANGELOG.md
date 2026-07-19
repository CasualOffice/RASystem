# Changelog

All notable changes to Casual RAS are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project is pre-release, so everything
lives under **Unreleased** until the first tagged version. Dates are ISO-8601.

## [Unreleased]

_Nothing yet._

## [0.0.1] — 2026-07-19 · first alpha (unsigned, draft pre-release)

Casual RAS is an embeddable, white-label remote-access platform (Rust core + Tauri app, peer-to-peer
over Iroh/QUIC). Priorities are strictly **Security → Latency → UX**. This log summarizes the
capabilities implemented at the code level; on-device runtime verification status is tracked in
[`docs/17`](docs/17_ROADMAP_AND_MILESTONES.md) and the production gap list in
[`docs/21`](docs/21_PRODUCTION_READINESS_BACKLOG.md).

### Added

- **Full feature set wired into the app** — chat, clipboard sync, file transfer, and output audio are
  now surfaced in the unified desktop app (commands + polished UI), not just complete in the Rust core.
  Consent-first: clipboard and audio are host **opt-in** (default off, disclosed) since neither has a
  per-message gate; file transfer keeps its per-transfer Accept/Deny; an always-visible "AUDIO SHARED"
  indicator discloses audio (Inv 7).
- **All-OS capture backends** — audio (macOS ScreenCaptureKit / Linux PipeWire-Pulse / Windows WASAPI
  loopback) and cursor-shape (macOS `NSCursor` / Linux XFixes / Windows GDI). macOS is on-device-verified;
  Linux/Windows are compile/CI-gated pending hardware. **Bidirectional:** either machine can be host or
  controller.

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

- Fixed a **silent clipboard-capability grant** (the RustDesk / Reverse-RDP injection class) found when
  wiring clipboard into the app: a plain view-Allow forged the *consented* set to the app's maximal
  ceiling, so `clipboard.write` (which has no per-message gate) was granted with no clipboard-specific
  consent. Now the issued grant's consented set reflects a real choice — clipboard (and audio) ride a
  disclosed host opt-in, not a view-Allow (Inv 1/2/7). Also fixed `recognize()` stripping the dynamic
  `file.push.<name>` namespace (which had made file transfer fail-closed dead) and a viewer UI that
  didn't disable its panels on host-initiated session end.
- Fixed an **audit-checkpoint authentication bypass** — `Checkpoint::verify` verified under the
  checkpoint's *embedded* signer instead of the verifier's trusted host key (a forgery oracle); it now
  requires the trusted key.
- Fixed several **Invariant-4** gaps: emergency stop now overrides an in-flight file offer, a clipboard
  push, and a file finalize; input dispatch, clipboard, and file writes all re-check `stop` before any
  OS-visible effect.
- Fixed three **stuck-key hazards** on the input path (Inv 4/5), all one root cause — the OS sink's held
  keys were only flushed on explicit stop/teardown, not on the other lease-death paths: (1) an
  emergency-stop `release_all` could race an in-flight key-down and leave it physically held — the
  stop-recheck and OS injection are now serialized under the input-sink lock so a concurrent flush can't
  interleave; (2) an expired lease never flushed held keys (and the gate refuses the key-up), so a held
  modifier stuck until teardown — the stats tick now sweeps an expired lease and releases its keys, even
  for an idle controller; (3) re-issuing/transferring the lease didn't release the prior holder's keys —
  it now flushes before minting the replacement. Found and verified by an adversarial multi-agent review
  of the input/OS-injection path (the other two lenses — FFI safety and secret hygiene — passed clean).
- Hardened **session reconnection** (ADR-091) against a silent re-dialer: the host's post-reconnect
  handshake reads are now window-bounded (symmetric with the controller), so a peer that re-establishes
  the transport but never presents its grant can no longer wedge or leak the host control task; teardown
  now aborts a parked control task so an emergency stop always reclaims it (Inv 4). Found and verified by
  an adversarial multi-agent review of the reconnection path.
- Closed an **AccessRequest replay window**: the single-use nonce was remembered for the cache TTL
  (`MAX_REQUEST_TTL_MS`) but a request accepted with a future-dated `issued_at` stays fresh for
  `MAX_REQUEST_TTL_MS + CLOCK_SKEW_MS`, so identical signed bytes could be replayed in the ~60s gap. The
  nonce is now remembered until the request's own `expires_at` (its true replay horizon), independent of
  cache-TTL sizing. Found and verified by an adversarial multi-agent review of the authorization core
  (the other four lenses — grant forgery, endpoint binding/expiry, per-message capability scope,
  unattended/pairing — passed clean).
- Fixed two **indicator-honesty gaps** (Inv 7) in the app: (1) closing the main window left the detached
  capture→stream loop running with the in-app indicator + Stop destroyed (the overlay window kept the
  process alive) — a window-close/exit handler now halts the share deterministically in-process; (2) the
  indicator lived only in the ordinary minimizable main window — an always-visible "REMOTE VIEWING/CONTROL
  ACTIVE" badge now renders on the always-on-top overlay covering the shared display, and the main window
  is non-minimizable during an active share so the Stop control stays reachable. Found by an adversarial
  multi-agent review of the app integration layer (consent-integrity, secret-hygiene, command-surface,
  and input-forwarding lenses passed clean).
- **Never-panic fuzz on every untrusted-input decoder** — control framing, the video/audio wire
  headers, PASETO grants + access requests, the audit log file, and pasted connection tickets.

### Fixed

- **Decoder not reconfigured on a mid-stream resolution / monitor / DPI change** — the host reconfigured
  its encoder and forced a keyframe, but the controller kept its decoder at the original dimensions, so a
  live monitor/resolution change rendered torn/stretched (or error-looped to black). The controller now
  reconfigures the decoder atomically with the keyframe that carries the new dimensions. Found by an
  adversarial multi-agent review of the media/transport path; also closed the test gap that let it ship.

### Known limitations

- **Video smoothness under packet loss** — the per-frame video streams are drained serially on the
  receiver, so QUIC's transport-level head-of-line-freedom between streams isn't carried through at the
  app layer: under loss, a stalled frame delays already-arrived frames behind it (bursty delivery). This
  is a smoothness cost only — control and audio ride separate planes, so the stop button is unaffected —
  and the concurrent-drain fix is deferred to on-device media tuning (it needs real lossy-network
  validation to avoid premature keyframe requests under ordinary jitter).

- **Unsigned by the OS** (no code-signing / notarization yet) — Gatekeeper/SmartScreen warn (ADR-072).
- On-device runtime verification is pending on Linux and Windows (Windows needs hardware the team
  lacks); the Windows host path is CI-compile-gated only.
- Audio is **output-only** (no mic), live-only (never recorded). No secure-desktop/UAC input injection,
  by design (Invariant 14).
