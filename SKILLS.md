# SKILLS.md — Engineering Skill Map & Playbooks for Casual RAS

This document maps the **competency areas** the project requires to the **subsystems** they touch,
so contributors (and AI agents) know what expertise a given change needs, where the danger zones
are, and what to read first. The second half is a set of **playbooks** — repeatable procedures for
the tasks we'll do over and over.

Priorities everywhere: **Security → Latency → UX** (see `CLAUDE.md`).

---

## Part A — Skill domains

Each domain lists: what it covers · subsystems · danger zones · primary docs.

### A1. Rust systems & async
- **Covers:** the shared core, Tokio runtime, ownership across async tasks, backpressure, zero-copy
  buffers, lifecycle/shutdown.
- **Subsystems:** all `crates/*`, both Tauri backends.
- **Danger zones:** blocking the async executor on the media hot path; unbounded channels;
  panics on request paths; lock contention on session state.
- **Read:** `docs/02`, `docs/03`.

### A2. QUIC / Iroh networking & NAT traversal
- **Covers:** Iroh endpoints, ALPN routing, connections vs streams vs datagrams, hole punching,
  relay fallback, connection migration, congestion behavior.
- **Subsystems:** `ras-transport-iroh`, session/media transport.
- **Danger zones:** treating the transport as authorization (it is not); head-of-line blocking on
  reliable streams for video; datagram MTU limits; relay bandwidth cost; direct-connection failure
  on symmetric NAT / UDP-blocked networks.
- **Read:** `docs/09_TRANSPORT_IROH.md`, `docs/04`.

### A3. Low-latency video: capture, encode, decode, render
- **Covers:** GPU frame capture, H.264 encoding (baseline/low-latency, on-demand IDR), bitrate/
  frame-rate adaptation, WebCodecs decode, canvas/WebGL render, frame pacing, loss recovery.
- **Subsystems:** `ras-media`, host `platform/*`, controller UI video path.
- **Danger zones:** CPU copies of GPU textures; head-of-line blocking; unbounded decode queue;
  bitstream format mismatch (annexB vs avcC); stalls freezing the local cursor/UI.
- **Read:** `docs/10_MEDIA_PIPELINE.md`.

### A4. Windows platform engineering
- **Covers:** Windows.Graphics.Capture / Desktop Duplication, Media Foundation / NVENC / Quick
  Sync / AMF, `SendInput`, DPI & multi-monitor coordinate mapping, services, Session 0 isolation,
  secure desktop (UAC/lock/Ctrl-Alt-Del), named-pipe IPC.
- **Subsystems:** `host/platform/windows`, input helper, host service (hardening phase).
- **Danger zones:** Session 0 cannot touch the interactive desktop; capture/input blocked on the
  secure desktop; per-monitor DPI math; stuck keys; user-switch/session-change handling.
- **Read:** `docs/11_HOST_PLATFORM_WINDOWS.md`.

### A5. macOS platform engineering *(later)*
- **Covers:** ScreenCaptureKit, VideoToolbox, CGEvent input, Accessibility + Screen Recording
  permissions, LaunchDaemon/LaunchAgent, Keychain, notarization; WKWebView WebCodecs limits.
- **Subsystems:** `host/platform/macos`, controller on macOS.
- **Danger zones:** permission-prompt transitions; WKWebView may force a native video surface.
- **Read:** `docs/11` (macOS section, when added).

### A6. Applied cryptography & protocol security
- **Covers:** Ed25519 identities, signed grants/leases, endpoint binding, replay defense (nonces,
  sequences, generations), clock-skew handling, canonical serialization for signed structures,
  key storage (DPAPI/TPM/Keychain).
- **Subsystems:** `ras-identity`, `ras-grant`, `ras-protocol`, `ras-audit`.
- **Danger zones:** rolling your own crypto; non-canonical bytes under a signature; missing
  endpoint/expiry checks; nonce cache gaps; timing-unsafe comparisons.
- **Read:** `docs/04`, `docs/06`.

### A7. Privilege separation & OS security
- **Covers:** the narrow validated-command boundary to the input helper, least-privilege service
  design, authenticated local IPC (pipe ACLs / peer credentials), attack-surface reduction.
- **Subsystems:** input helper, host service, local IPC.
- **Danger zones:** widening the helper's accepted command set; unauthenticated local endpoints;
  passing raw network objects across the trust boundary.
- **Read:** `docs/06`, `docs/02` (trust boundaries).

### A8. Tauri v2 + React/TS frontend
- **Covers:** Tauri v2 command/event/`Channel` APIs, binary streaming to the webview, secure IPC
  surface, React session UI, WebCodecs integration, rendering.
- **Subsystems:** `controller/`, host consent UI.
- **Danger zones:** exposing privileged IPC to the renderer; JSON-encoding frame data; UI able to
  hide the session indicator / stop control.
- **Read:** `docs/12_CONTROLLER_TAURI.md`.

### A9. SDK & ABI design *(SDK-extraction phase)*
- **Covers:** stable C ABI (opaque handles, explicit ownership, no exceptions across the boundary),
  N-API Node binding, versioned ABI negotiation, later .NET/Swift.
- **Subsystems:** `ras-ffi`, `sdk/*`.
- **Danger zones:** leaking Rust struct layout; ownership/lifetime bugs across FFI; ABI breaks.
- **Read:** `docs/05`.

### A10. Audit, observability & telemetry
- **Covers:** hash-chained signed audit journal, signed checkpoints, corruption detection, metrics
  (latency, RTT, loss, encode/decode timing), tracing that never logs secrets.
- **Subsystems:** `ras-audit`, telemetry across core.
- **Danger zones:** logging secrets/pixels/keystrokes; breaking the hash chain; audit writes on the
  latency-critical path done wrong (must be ordered but not block frames).
- **Read:** `docs/06` (audit), `docs/03` (observability).

### A11. QA, fuzzing & performance testing
- **Covers:** unit/property/fuzz/integration/E2E strategy, protocol fuzzers, latency/throughput
  measurement, network-impairment matrices, race testing (lease transfer).
- **Subsystems:** all.
- **Danger zones:** security paths without tests; perf numbers measured without a defined workload/
  network profile.
- **Read:** `docs/08`.

### A12. Build, packaging & release security
- **Covers:** code signing / notarization, MSI/WiX + silent install, service registration, signed
  updater with rollback protection, SBOM, dependency audit.
- **Subsystems:** installers, updater, CI/release.
- **Danger zones:** unsigned binaries/updates; loading unsigned plugins into privileged processes;
  version rollback attacks.
- **Read:** `docs/05` (installer), `docs/08` (release), `docs/06` (update security).

---

## Part B — Playbooks

Repeatable procedures. Follow them so the security posture stays consistent.

### P1. Add a new capability
1. Propose it in `docs/04` capability registry with a precise meaning and version.
2. Decide default policy (**default = denied / off**) and whether it requires per-use consent.
3. Add capability-intersection + "unknown-denied" property tests.
4. Wire enforcement at the host (grant → policy → per-message check). Never trust the controller's
   claim.
5. Update consent UI to display it honestly. Add an ADR if it changes the trust model.

### P2. Add or change a protocol message
1. Edit the `.proto` source; regenerate — never hand-edit generated code.
2. Keep it additive; never reuse field numbers; bump the protocol version per `docs/04`.
3. Add/adjust fuzz targets for any new parser path.
4. Note wire compatibility in the PR; update `docs/04`.

### P3. Change anything on the authorization path (identity/grant/lease/policy)
1. Write/adjust an ADR — this is by definition security-sensitive.
2. Preserve: signature verification, endpoint binding, expiry, replay defense, generation checks.
3. Add security tests (stolen/expired/replayed/cross-endpoint/old-generation).
4. Require a second reviewer + threat-model note (`CONTRIBUTING.md §4`).

### P4. Run a latency spike / measure the media path
1. Fix the workload (static doc / IDE / scrolling / video) and network profile (LAN / RTT / loss)
   from `docs/08`.
2. Measure each stage: capture → encode → network queue → decode → render (glass-to-glass) and
   input-to-visible.
3. Report numbers *with* the workload+network context; never a bare "X ms".
4. Compare against targets in `docs/01 §11` and record in the risk register if a target is at risk.

### P5. Threat-check a change (quick gate before merge)
Ask, and answer in the PR: Does it (a) touch a Non-Negotiable Invariant? (b) widen the input
helper's accepted commands? (c) add an unauthenticated local endpoint? (d) log anything sensitive?
(e) weaken grant/lease/consent/emergency-stop? (f) add a capability defaulting to on? Any "yes"
requires explicit justification, tests, and usually an ADR.

### P6. Extract an SDK from a proven crate *(SDK phase)*
1. Confirm the crate's surface is exercised by a real reference app (that's the validation).
2. Design opaque-handle C ABI; explicit ownership; stable error codes; ABI version negotiation.
3. Add ABI compatibility tests; document callback threading.
4. Layer N-API/React on top without leaking privileged IPC to the renderer.
