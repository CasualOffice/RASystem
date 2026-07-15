# 17 — Roadmap, Milestones & Phase-Wise Task Plan

> The execution plan for Casual RAS. Every phase runs the same rhythm:
> **① Design gate → ② Build → ③ Verify/Exit.** No phase starts building until its design note is
> written and reviewed; no phase is "done" until its exit criteria and the relevant `docs/13` risk
> rows are satisfied. Priorities everywhere: **Security → Latency → UX**.
>
> This supersedes the high-level `docs/07` with milestones, granular tasks, dependencies, and a
> design gate per phase. Statuses: ☐ not started · ◐ in progress · ☑ done.

## How to read this

- **Milestones (M0–M9)** are demonstrable capabilities — the "is it real yet?" checkpoints.
- **Workstreams** are parallel tracks; each task is tagged with one:
  `CORE` (crates/protocol/state) · `NET` (Iroh/transport) · `MED` (capture/encode/decode/render) ·
  `WIN` (Windows platform) · `SEC` (identity/auth/grants/audit) · `FRD` (fraud/harm-prevention) ·
  `UI` (Tauri/React controller + consent) · `SDK` (ABI/bindings) · `INF` (CI/build/release) ·
  `QA` (test/fuzz/perf).
- **Design gate** = a short `docs/design/phase-<n>-design.md` note (interfaces, data shapes,
  sequence diagrams, open questions) reviewed against the invariants before code. This is the
  "design each phase-wise system, then execute" rule.

## Platform lead (ADR-054)

**Development leads on macOS** (ScreenCaptureKit / VideoToolbox / CGEvent) because that's the team's
testable hardware; **Windows remains the production target**, ported when Windows hardware/CI is
available. The host is platform-abstracted, so every phase below applies to whichever host backend
is active — read "host capture/encode/input" as **macOS-first, Windows-port-later**. Windows-specific
detail lives in `docs/11`; macOS detail in `docs/18` (host deep-dive). All non-host work
(core/protocol/security/transport/controller/fraud-logic) is cross-platform and unaffected.

## Milestone ladder

| M | Name | Demonstrable capability | Phase |
|---|------|-------------------------|-------|
| **M0** | Foundations & first light | Workspace builds on Win+mac, CI green, proto codegen works | 0 |
| **M1** | Feasibility proven | Measured latency + Iroh NAT/relay + DXGI→WebCodecs path; go/no-go | S |
| **M2** | First pixels | Windows host screen → controller view-only over Iroh (direct + relay) | 1 |
| **M3** | Trusted session | Identity + rotating single-use ticket + consent + signed grants; unknown controller cannot view | 2 |
| **M4** | Remote control | Input + control leases + virtual cursors + emergency stop; no stale input after transfer | 3 |
| **M5** | Hardened runtime | Service + session-agent + input-helper split, authenticated IPC, tamper-evident audit, crash recovery | 4 |
| **M6** | Fraud & access tiers | On-device risk engine + enforcement ladder + tiered enrollment (Standard→Hardened) | F |
| **M7** | SDK beta | C ABI + Node/Electron + React components + installer + design-partner sample | 5 |
| **M8** | Windows production | Multi-monitor, HW-encoder matrix, clipboard, file transfer, actions, signed updater, EV-signed | 6 |
| **M9** | Expansion | macOS host · multi-party/recording · server-issued grants | 7–9 |

**Critical path:** M0 → M1 → M2 → M3 → M4 → M5 → M6 → M7 → M8. Media (`MED`) and transport (`NET`)
can proceed in parallel with security (`SEC`) after M0; the controller UI (`UI`) tracks M2+.

---

## Phase 0 — Foundations & first light → **M0**

**Goal:** a building, tested, CI-backed monorepo skeleton with the protocol source of truth and the
invariants encoded as lint/test scaffolding — so every later phase has a place to land.

**① Design gate (`docs/design/phase-0-design.md`):** workspace crate graph & dependency direction;
`proto/` layout + codegen pipeline; error-type + result-code conventions (`docs/04 §14`); logging
policy that forbids secrets (Invariant 8); CI matrix (Win 10 22H2 / Win 11 / macOS dev).

**② Build — tasks**
- ☑ `INF` Create Cargo workspace + the crate skeletons from `CLAUDE.md §7` (empty but compiling).
- ☑ `INF` `cargo-deny` license gate (deny GPL/LGPL/AGPL/SSPL; allow MIT/Apache/BSD/ISC/Zlib/MPL) —
  wire into CI so a bad dep fails the build (ADR-051).
- ☑ `INF` CI: fmt + clippy (deny warnings) + test + `cargo-deny` on ubuntu + Win + mac runners.
- ☑ `CORE` `proto/casual_ras.proto` + **Prost codegen wired** (Phase 1) — offline via `protox` (no
  system `protoc`), `ras-protocol::codec` maps `ControlMsg` ⇄ generated wire types with framing + a
  `MAX_CONTROL_FRAME` DoS guard; generated code never committed/hand-edited. 20 round-trip + adversarial tests.
- ☑ `CORE` Error taxonomy (`ras-protocol::ErrorCode`) mapping to the stable codes in `docs/04 §14`.
- ◐ `CORE` `unsafe_code = "deny"` workspace lint in place; secret-free `tracing` setup pending
  (lands with the first real secret type, Phase 2).
- ◐ `SEC` `ras-identity` crate stub created; Ed25519 keypair gen + key-storage trait are Phase 2.
- ☑ `QA` Test-fixture pattern + first invariant tests (capability intersection: unknown-denied,
  reduced-never-expands) in `ras-policy`.

**③ Exit criteria:** ☑ builds on mac dev machine (Win/ubuntu via CI) · ☑ `fmt`/`clippy -D
warnings`/`test`/`cargo deny` green · ☑ protocol versioning documented (`docs/04`,
`PROTOCOL_VERSION`) · ☑ design note reviewed against invariants. **→ M0 reached.**

**Risks addressed:** foundation for A1; license hygiene (ADR-051). **Effort:** ~2–3 wks · **Status:
☑ done (M0).**

---

## Phase S — Risk-validation spike (throwaway) → **M1**

**Goal:** convert the biggest unvalidated bets (D1 latency, D7 Iroh-on-hostile-networks, C2 DXGI
recovery, D6 WebView2 IPC) from "assumed" to "measured" **before** investing in real architecture.
Code here is disposable.

**① Design gate (`docs/design/phase-S-design.md`):** exact measurement methodology per `SKILLS.md`
P4 (fixed workloads + network profiles); success thresholds from `docs/01 §11`; what a "no-go"
triggers (native-surface pivot / codec change).

**② Build — tasks** (spike scaffolded in `spike/`; **measurement runs on the user's Mac**)
- ☑ `NET` Two-endpoint Iroh 1.x connect probe (`spike/iroh-probe`) — direct/relay + RTT under load.
  Builds clean on **iroh 1.0.2** and **localhost-validated**: 300-frame echo + observed relay→direct
  upgrade (`remote_info`-based path classifier). Turnkey for the two-machine run below.
- ☐ `NET` **Run** across the network matrix (`docs/08 §3`): same-LAN, different NAT, **symmetric
  NAT**, **UDP-blocked/443-only**, relay-only, migration — record success + direct-vs-relay + RTT.
  *(Blocked: needs a Mac↔Linux two-machine run.)*
- ☑ `MED` **macOS capture→encode spike** (`spike/macos-capture`) — real ScreenCaptureKit + real
  VideoToolbox H.264 via pure-Rust `objc2` bindings (no Swift bridge). **On-device run: GO (both
  halves).** Frame-accurate 16.67 ms/60 fps SCK cadence on change (static-frame coalescing = bandwidth
  feature); pixel extraction ~20–40 µs/frame; **encode latency ~11 ms med / ~13 ms p95** at 60 fps;
  Annex-B output decodes cleanly (`ffprobe`: h264 1470×956). Numbers in
  `docs/design/phase-S-design.md §4.1`. (Windows DXGI+MF is the later port.)
- ☑ `MED`+`UI` **Turnkey WebCodecs loopback harness** (`spike/latency-probe/web/index.html`) —
  encode→decode→canvas latency, avcC/annexB, frame-close, compositor-frame toggle.
- ◐ `QA` **Run** the probes; compile the latency report; record the compositor-frame penalty.
  Results in `docs/design/phase-S-design.md §4.1`. **Done:** WebCodecs measured on **both Chrome
  (e2e 7.1/10.5 ms) and Safari/WebKit (e2e 4.0/5.0 ms)** — 60 fps, 0 drops, decode ~1 ms. Safari =
  the WKWebView engine ⇒ **the WebCodecs bet is GO including the macOS-lead path; native-surface
  PIVOT is off the table.** **Pending:** the rVFC-toggle compositor penalty (refinement) and the
  whole iroh network matrix (the deciding unknown for M1).

**③ Exit criteria (go/no-go):** latency targets look achievable or we re-plan · direct+relay work on
the matrix · WebCodecs path viable on WebView2 (or decision to go native-surface) · **written go/no-go
recorded as an ADR.** *(Blocked on the user running the scaffolded probes.)*

**Risks addressed:** D1, D7, C2, D6, D4. **Effort:** ~2–4 wks. **Gate: no Phase 1 until M1 passes.**

---

## Phase 1 — Transport & screen prototype → **M2**

**Goal:** real (non-throwaway) Windows host → Tauri controller, **view-only**, single monitor, over
Iroh direct + relay, using the crate structure — no auth yet.

**① Design gate (`docs/design/phase-1-design.md`):** `ras-media` capture/encode/decode traits;
`ras-transport-iroh` channel map (`docs/09 §5`) — control stream + video (per-frame stream *or*
datagram+FEC, per spike result); frame framing + decoder-feedback protocol; `VideoFrame` lifecycle
(close discipline, `docs/10 §6`).

**② Build — tasks**

*Spike-independent spine — **done ahead of the transport/UI** (all green: build/clippy/test/deny),
exercised end-to-end on an in-memory loopback with no iroh/OS/GPU:*
- ☑ `CORE` Session state machine (Created→…→Active) + security-terminal states, with the
  suspend/reconnect path and exhaustive invariant tests (emergency-stop overrides, terminal finality).
- ☑ `CORE` Canonical cross-crate types + DI seams (`ras-core::deps`), typed lifecycle events
  (`ras-core::event`), no-op auth seam (`AllowAllValidator`).
- ☑ `CORE` Host + controller orchestrators (`ras-core::session`) — handshake, authorize gate, stream
  negotiation, droppable video, keyframe round-trip, suspend/reconnect, teardown.
- ☑ `CORE`+`SEC` **Emergency-stop / revoke runtime path (Invariant 4)** — `HostSession::emergency_stop`
  drives the audit-distinct `Revoke → Revoked` edge, halts the media pump before its next send (the
  pump re-checks the stop flag between encode and send, so no frame leaks post-revoke), and flushes a
  best-effort, time-bounded `Bye{SessionRevoked}` so the controller ends `Revoked` (not a plain peer
  close). Loopback-tested: ≤250 ms local halt, no post-stop frames, idempotent/non-downgradable,
  revoke propagates to the controller.
- ☑ `CORE`+`SEC` **Three distinct teardown paths (ADR-056)** — added `ErrorCode::NormalClosure` to the
  wire so a clean stop is separable from a crash and from a revoke: **`Bye{NormalClosure}` →
  `PeerClosed → Terminated`** (graceful `stop`/`disconnect`, prompt — no suspend); **`Bye{SessionRevoked}`
  → `Revoke → Revoked`** (host emergency stop only; the host treats a *controller*-claimed revoke as an
  ordinary close — Inv. 1/15); **missing `Bye` → `TransportLost → Suspended`** (reconnect window). Each
  loopback-tested.
- ☑ `MED` Synthetic capture/encode doubles (`ras-media::synthetic`) + loopback transport
  (`ras-core::testkit`, now with a `LoopbackCut` fault handle that severs the link mid-session to
  exercise the abrupt-loss/suspend path without abusing `stop`) + `webcodecs_string`.
- ☑ `NET`/`CORE` Adaptive-bitrate hook wired: `LatencyFirstAbr` + a 250 ms stats/ABR tick driving
  `set_bitrate` and emitting `ConnectionQuality` (control law stays spike-tunable).
- ☑ `UI`/`CORE` Frame-Channel codec (`ras-core::frame_channel`) — the 24-byte header contract shared
  with the future TS decoder worker.
- ☑ `NET`/`CORE` Real protobuf control-channel wire codec (`ras-protocol::codec`, offline `protox`
  codegen) + a generic async `FramedControlChannel` (`ras-transport-iroh`) that runs it over any
  `AsyncRead`/`AsyncWrite` (iroh's stream shape) — length-prefixed, `MAX_CONTROL_FRAME` DoS-guarded,
  tested over an in-memory duplex (round-trip, reassembly, split reads, oversized, peer-close).
- ☑ `MED`/`CORE` **Controller loss-handling policy (docs/10 §4)** — `FrameDropped` handling is now
  `DropReason`-aware via a pure, exhaustively-tested `loss_action`: a *stale* (superseded) drop is
  benign (no IDR spam), while an unrecoverable gap freezes on the last good frame (re-gates P-frames
  until the next IDR) and requests one fresh keyframe. Exercised end-to-end over the loopback with a
  fault-injected drop (drop → keyframe request → host IDR → sink). *(The FEC codec + real drop
  generation stay gated below.)*

*Real backends behind the seams — **gated on the Phase-S go/no-go / hardware**:*
- ◐ `MED` **macOS-lead:** ScreenCaptureKit capture + VideoToolbox encode behind the trait (Windows
  DXGI+MF is the later port). **Landed + on-device verified:** the `ras-media-macos` crate implements
  `ScreenCaptureBackend` (SCK push-delegate → latest-frame pull adapter) and `VideoEncoderBackend`
  (VideoToolbox H.264: realtime, no B-frames, Baseline, ∞-GOP + forced-IDR-on-demand, ABR bitrate),
  wired through the real `PlatformSurface` seam (ADR-058). Driven end-to-end through the `ras-media`
  traits (`--example capture_encode`): first-frame keyframe, gap-free monotonic ids, Annex-B + in-band
  SPS/PPS, `ffprobe`-clean h264, ~8 ms encode. Pure-Rust `objc2` (no Swift bridge); empty on non-macOS
  so CI stays green. `excluded_window_ids` → `SCWindow` mapping is **done** (the filter now excludes
  our own overlay/consent/indicator windows, matched by CGWindowID, so they never re-enter the shared
  feed; the app supplies the ids from each Tauri window's `NSWindow.windowNumber`). **Remaining:**
  cursor metadata out-of-band, dirty rects, and pipelined (async) encode emission.
- ◐ `MED` HW encoder abstraction + OpenH264 software fallback (never x264). **Software encoder landed**
  (`ras-media-openh264`, built from Cisco BSD-2 source): BGRA→I420→Annex-B, in-band SPS/PPS per IDR,
  forced-IDR-on-demand, and **runtime ABR** — built in bitrate rate-control mode at the negotiated
  target, retargeted keyframe-free via `SetOption(ENCODER_OPTION_BITRATE)`; unit-tested that a runtime
  bitrate drop shrinks output. **Remaining:** hardware encoders (VideoToolbox exists on macOS; VAAPI/
  MF/NVENC ☐) and the runtime `libloading` variant.
- ☑ `NET` `ras-transport-iroh`: real endpoint, versioned ALPN, channel plumbing over iroh 1.x
  (`Connection::stats()` feeds the existing ABR hook). **Control + video + health planes done** (iroh
  `=1.0.2`): real `Endpoint`/`Session`/`ControlChannel`, ALPN `casual-ras/1` + single-bidi-stream
  control topology, **host-opens** (ADR-059 amended after a real two-endpoint run surfaced the
  controller-opens deadlock the loopback masked); the **`PerFrameStream` video path** — one uni QUIC
  stream per frame, a 44-byte per-frame header carrying `StreamConfig`, bounded drop-at-source sink,
  source-side gap → `FrameDropped` synthesis (ADR-060); and a **`HealthObserver`** deriving
  `ConnHealth` (rtt/bandwidth/path from the selected path's `PathStats`, **windowed** loss from
  `ConnectionStats`). The **`IrohSessionTransport: SessionTransport` adapter** (in `ras-core`) makes
  the loopback→iroh swap transparent: the full spine runs end-to-end over two real iroh endpoints
  (`spine_runs_over_real_iroh_transport`) with **no orchestrator change**. Hermetic tests: control
  round-trip asserting peer `EndpointId` (Invariant 9), a real per-frame-stream video exchange with
  gap detection + live health read, a header round-trip / fail-closed-decode unit test, and the
  full-spine iroh e2e. Loss is now **windowed** (`HealthObserver` remembers the previous datagram
  counters and reports loss over the interval since the last read, so a burst no longer permanently
  depresses the ABR; unit-tested recovery-after-burst / idle / clamping). **Deferred (additive):**
  reset-on-stale + FEC / the `DatagramFec` alternative. The cross-machine two-laptop run remains the
  developer-owned on-device step.
- ☐ `MED` FEC (`nanors`) + the *transport-side* loss detection that generates `FrameDropped` per
  `docs/10 §4` (the controller-side reaction to those events is done + tested above).
- ◐ `UI` Controller Tauri shell (`controller/`). **Landed + compiles (Tauri 2.11.5, ≥2.11.1 pin):**
  the video path is proven — Rust pushes each encoded access unit as the canonical
  `ras_core::frame_channel` blob (24-byte `RAS1` header + Annex-B) over a **binary** Tauri `Channel`;
  the webview decodes with a WebCodecs `VideoDecoder` and renders to a `<canvas>`, drops deltas until
  the first IDR, and drives forced-IDR-on-demand (`request_keyframe`) to cover the infinite-GOP
  startup race + decoder reset. Frames flow through the **real `ras-core` spine** — a `HostSession`
  (real `ras-media-macos` backends) + `ControllerSession` over the in-memory **loopback transport**, so
  each frame traverses handshake → authorize-gate → grant → media pump → teardown and keyframe requests
  ride the control channel (the loopback e2e path, now with real macOS media). Runnable **glass-to-glass
  on one Mac before iroh** (steps 2+3 collapsed); the loopback swaps for the concrete iroh transport
  behind the same `SessionTransport` seam. Static frontend via `withGlobalTauri` (no bundler);
  `core:default` capability; CSP set; always-visible LIVE indicator (Invariant 7). **Remaining:** Web
  Worker + `OffscreenCanvas` renderer, React/TS UI + strict-CSP hardening, connection-state UI, the
  host **consent window** (replacing the `AllowAllValidator` no-op seam), and the iroh transport (step 4).
- ◐ `QA` Reconnection behavior documented + tested (loopback; the `LoopbackFaults` handle injects both
  an abrupt link cut and `FrameDropped` events, so suspend/reconnect *and* loss handling are exercised
  without abusing `stop`); **generative/fuzz property tests** over the untrusted-input surface: the control codec, frame
  codec, and state machine (`proptest` — decode never panics on arbitrary bytes; round-trip identity;
  terminal-absorbing; revoke-always-wins), **and the `FramedControlChannel` reader** — the code that
  will parse bytes off iroh's streams — fuzzed for no-panic/no-hang on adversarial input, correct
  reassembly under arbitrary chunking (1 byte … multi-frame), and an oversized length prefix leaving
  the stream permanently refused (DoS guard fires before body allocation). **Perf harness wired**
  (`crates/ras-core/benches/hot_paths.rs`, hand-rolled — no criterion/dev-dep weight; `cargo bench` in
  CI): per-op baselines on the per-frame/per-message hot paths (dev laptop: state transition ~6 ns,
  control-codec round-trip ~130 ns, frame-Channel encode+parse ~26 ns — all sub-µs) behind a loose
  1 ms/op sanity ceiling that trips on a gross regression without flaking on runner noise.

**③ Exit criteria:** stable ~30 FPS on standard desktop workloads · direct + relay sessions work ·
prototype latency targets measured · reconnection documented · local cursor stays responsive during
video stall.

**Risks addressed:** D2, D3, D5, D8, C4. **Effort:** ~6–8 wks.

---

## Phase 2 — Identity, pairing, authorization → **M3**

**Goal:** no frames without authorization. Rotating single-use tickets, consent, signed grants,
replay defense.

**① Design gate (`docs/design/phase-2-design.md`) — ☑ signed off.** Written & approved: the
bootstrap→session authorization flow; ticket/grant/lease wire structures (per `docs/04`); the grant
**format decision concretized in ADR-064** (MVP = PASETO v4.public, sender-constrained, **Accepted**;
Biscuit reserved for the offline-attenuating control-plane issuer); the Ed25519-primitive decision
(**ADR-065** — `ed25519-dalek` behind the `KeyStore` seam, not a new libsodium binding);
`SessionGrantIssuer` + `LocalHostGrantIssuer`; how the Phase-1 §5.5 `GrantValidator` seam is filled
additively; the ordered validation checks; consent-UI contract; replay-state schema
(`consumed_tickets`, nonce cache, generations); the M3 security-test matrix; and the crate execution
sequence (bottom-up: policy → identity → wire → bootstrap → grant → core → app).

**② Build — tasks** (execution in progress, bottom-up)
- ☑ `SEC` `ras-policy`: capability **catalogue v1** + `recognize` (default-deny unknown, Inv 2) +
  `grantable` (`recognize ∩ policy ∩ consented`, never-expands) + `phase2_default_policy`
  (view-only + visual pointer + annotation). 7 tests.
- ☑ `CORE` `ras-protocol`: the **bootstrap-ALPN wire message set** (`BootstrapMsg` enum + proto oneof
  + codec) — separate from session `ControlMsg` for type-level channel separation; fail-closed decode
  (32-byte ids, bounded display name, tier range, exactly-one `AccessDecision`) + fuzz. No new
  `ErrorCode`s.
- ◐ `SEC` `ras-identity`: Ed25519 identities + `KeyStore` (sign/public only, no export — Inv 8) +
  `SoftwareKeyStore` (**Tier 0**, ephemeral or `0600`-persisted, redacted, fail-closed) + strict
  `verify` + in-memory `TrustedControllers` (de-list kill-switch). **Pending:** TPM/Keychain-sealed
  storage + key attestation for Tier ≥1; SQLite-durable registry.
- ◐ `SEC` `ras-bootstrap`: **rotating single-use connection tickets** (`docs/16 §1.5`) — `TicketAuthority`
  issue (generation bump + invalidate-prior + consumed-set clear), fail-closed `consume` (host match →
  signature → expiry → current-generation → not-consumed, each a stable `ErrorCode`), host-signed via
  the `KeyStore`/`verify` seam with a domain-separation tag, opaque dial blob (iroh-free), and the
  `NonceCache` (bounded, TTL-swept, fail-closed on saturation) shared with AccessRequest validation.
  11 tests. **Deviation:** ticket string is the hand-rolled `CASUALRAST1:<hex>` layout (matching the
  Phase-1 `CASUALRAS1:` codec, dependency-free), not CBOR+Base64URL — same fail-closed guarantees, no
  new dep. **Pending:** QR rendering.
- ☐ `SEC` Pairing flow + trusted-controller registry + revocation.
- ◐ `SEC` Signed `AccessRequest` validation (ordered fail-closed: version → signature → host match →
  endpoint binding → freshness ≤5 min/no-future → nonce → capability recognition) + `SessionGrant`
  **PASETO v4.public** issuance/validation, **sender-constrained** to the controller endpoint
  (ADR-040). `LocalHostGrantIssuer` behind the `SessionGrantIssuer` seam; grant caps =
  `recognize(requested) ∩ policy ∩ consented`. PASETO envelope hand-written over dalek (**ADR-066**)
  and verified byte-exact against the official v4 vectors (4-S-1/2/3). 27 tests (incl. property/fuzz).
- ◐ `CORE` `ras-core` auth seam filled (design §4): `SessionAuthContext` extended (host id + now,
  additive), `GrantDecision::Authorized(CapabilitySet)` (per-message scope for Phase 3), and the real
  `GrantSessionValidator` calling `ras_grant::validate_grant` against the transport-authenticated
  endpoint (sender-constraint enforced at the moment identity is proven). The controller presents its
  grant in `ControlMsg::AuthEnvelope`; the host reads it before authorizing. Loopback e2e tests moved
  to concurrent `join!` (the host now needs the controller mid-handshake, as over real iroh). Direct
  validator test proves valid→Authorized, wrong-endpoint→IdentityMismatch, tampered→GrantInvalid; full
  ras-core suite (32 incl. real-iroh e2e) green. **Pending:** the app's two-phase bootstrap wiring +
  M3 security-test matrix (next).
- ◐ `SEC` Replay defense: **nonce cache** (bounded, TTL-swept, fail-closed) shared by request
  validation; **ticket generation + consumed set** (in `ras-bootstrap`). Session-generation field is
  carried on the grant; the lease/generation *runtime* is Phase 3.
- ☐ `UI` Branded consent UI (identity, reason, requested caps, recording state, duration, stop);
  approve/reduce/view-only/deny; host-shown one-time PIN (Tier 0).
- ☐ `QA` Security tests: stolen/expired/replayed ticket, stale-generation ticket, modified request,
  cross-endpoint grant; property tests (unknown denied, reduced never expands).

**③ Exit criteria:** unknown controller cannot receive frames · replayed/expired/stale-generation
ticket rejected · host & controller validate each other · every path has the security tests above.

**Risks addressed:** B2, B2b, B3 (foundation), B7. **Effort:** ~6–8 wks.

---

## Phase 3 — Remote control & collaboration → **M4**

**Goal:** safe input. Control leases, per-message capability enforcement, virtual cursors, emergency
stop.

**① Design gate (`docs/design/phase-3-design.md`):** input message schema + normalized-coord model
(`docs/04 §12`, `docs/11 §3`); lease/generation state machine; per-message capability-check point;
emergency-stop path (SAS-bound); virtual-cursor relay.

**② Build — tasks**
- ☐ `WIN` Input injection: `SendInput` ABSOLUTE|VIRTUALDESK, PMv2 manifest, normalized 0..1→pixel
  recipe, Unicode + scan-code paths, pressed-key tracking + KEYUP-on-change.
- ☐ `CORE` Control leases: issue/renew, generation increment on transfer, old-generation rejection.
- ☐ `SEC` **Per-message capability enforcement, host-side** (ADR-041) — the RustDesk-CVE fix.
- ☐ `CORE` Virtual multi-cursor relay (normalized coords, latest-wins, rate-limited); pointer-only
  participants cannot inject.
- ☐ `SEC`+`UI` Emergency stop: SAS-bound path + always-visible session indicator; revokes all
  leases/channels ≤250 ms.
- ☐ `WIN` Key-state cleanup on transfer/termination/disconnect.
- ☐ `QA` Lease-transfer race tests; "no two controllers inject concurrently"; old-lease-input
  rejected; emergency-stop timing.

**③ Exit criteria:** no two controllers inject real input concurrently by default · old lease input
rejected after transfer · emergency stop within target time · virtual cursors responsive during video
loss.

**Risks addressed:** B3, B1 (indicator/stop), C3. **Effort:** ~6–8 wks.

---

## Phase 4 — Runtime isolation & local audit → **M5**

**Goal:** collapse-to-split. Turn the single-process MVP into service + session-agent + privileged
input-helper with authenticated IPC and tamper-evident audit.

**① Design gate (`docs/design/phase-4-design.md`):** the three-process boundary + the narrow
validated input-helper command schema; IPC auth (token/SID impersonation, hardened pipe SD,
FIRST_PIPE_INSTANCE, secure prefix — **never PID**); "which desktop am I on" abstraction; audit chain
+ checkpoint design (`docs/06 §12`).

**② Build — tasks**
- ☐ `WIN` Windows service (LocalSystem/VSA) + per-session agent via `WTSQueryUserToken` →
  `CreateProcessAsUser`; session-change handling (+ poll `OpenInputDesktop` for UAC/CAD switch).
- ☐ `WIN` Minimal privileged **input-helper**: accepts only normalized input + release-all-keys;
  validates every field incl. referenced resources; fails closed.
- ☐ `SEC` Authenticated named-pipe IPC (peer token/SID check; hardened SD).
- ☐ `SEC` `ras-audit`: hash-chained signed journal + forward-secure keys + periodic Merkle
  checkpoint + TPM monotonic counter; encrypted at rest; never logs content.
- ☐ `WIN` Crash recovery + watchdog: helper crash → revoke lease + force key-release; agent crash →
  revoke input; service restart without stale leases.
- ☐ `QA` IPC-authz tests from unauthorized process; malformed-helper fuzz; audit-chain
  modification/truncation detection; restart-without-stale-lease.

**③ Exit criteria:** customer-app crash doesn't expose the privileged interface · helper refuses
malformed/unauthorized messages · audit verifies after a session · service restarts without stale
leases.

**Risks addressed:** B4, B8, C1. **Effort:** ~8–10 wks.

---

## Phase F — Fraud, harm-prevention & access tiers → **M6**

**Goal:** the Casual-RAS differentiator. On-device risk engine + enforcement ladder + tiered
enrollment. (Depends on consent/grant/session-agent infra from Phases 2–4.)

**① Design gate (`docs/design/phase-F-design.md`):** the content-free verdict schema (compile-time
`content`-forbidden); signal collectors (S1/S2/S3/S6) and their APIs; risk-engine scoring + hard
triggers; enforcement-ladder state machine + controller-blind recovery; enrollment-tier state; the
privacy DO/DO-NOT checklist (`docs/15 §5`).

**② Build — tasks**
- ☐ `FRD` Verdict types (content-free, type-enforced) + on-device scope gate (inert without live
  grant).
- ☐ `FRD` Signal collectors: foreground/desktop-switch (`EVENT_SYSTEM_*`), UIA `IsPassword`,
  input-origin/timing, first-time/anomalous-controller, concurrent-telephony.
- ☐ `FRD` Risk engine: scoring + hard triggers + fail-safe escalation; signed server-updatable
  weights/lists (matching local only).
- ☐ `FRD`+`UI` Enforcement ladder (banner→re-consent→input-suspend→video-mask→auto-pause→terminate),
  local-user-only controller-blind recovery; persona profiles (Consumer-Protect / Attended-Support /
  Unattended); **shadow/audit-only mode** for new fleets.
- ☐ `SEC` Tiered enrollment (`docs/16`): pairing password, TOTP, **FIDO2/WebAuthn** (PRF→grant
  fusion), Windows Hello; cool-off + directed warnings; tier attestation in the grant.
- ☐ `FRD`+`QA` Privacy tests (no `content` compiles; verdict egress content-free; analyzer inert
  without grant); scam-walkthrough red-team; false-positive tuning on the profiles.

**③ Exit criteria:** fraud subsystem passes privacy tests · enforcement ladder recoverable & fail-safe
· tiers compose with grants and attest correctly · honest claim-language review (`docs/15 §6`) done.

**Risks addressed:** B1, B6, B2b. **Effort:** ~8–10 wks. **Legal:** DPIA/LIA + cool-off/vertical
decisions (`docs/14` open decisions) before enabling enforcement defaults.

---

## Phase 5 — SDK beta → **M7**

**Goal:** draw the SDK boundary around the proven crates (the app-first payoff).

**① Design gate:** C ABI surface (opaque handles, ownership, stable error codes, ABI-version
negotiation); N-API shape; React component/hook API (`docs/05`).

**② Build — tasks**
- ☐ `SDK` `ras-ffi` C ABI + `cbindgen` headers. ☐ `SDK` Node/Electron host + controller SDK (N-API).
- ☐ `SDK` React components/hooks (from the controller UI). ☐ `INF` Installer toolkit (WiX/MSI, silent
  install, service registration). ☐ `SDK` Reference host + controller apps + sample integration + API
  docs. ☐ `QA` ABI compatibility tests; external-dev integration dry-run.
- ☐ `NET`/`UI`/`SDK` **Browser/webapp controller over WebRTC (ADR-057, deferred integration track).**
  Second transport adapter behind `SessionTransport` — WebRTC data channels + DTLS-SRTP, ICE with
  public STUN (self-hosted TURN when direct fails), grants validated host-side (Invariant 9) — plus a
  JS/web controller SDK droppable into a customer's web product. Reuses the transport-agnostic core;
  caps a pure-browser controller at assurance Tier 0 (no TPM). Kicks off only after the native
  iroh MVP (M2) is proven; may prompt reassessing iroh-vs-WebRTC consolidation if it becomes primary.

**③ Exit criteria:** external developer completes the sample in < 1 day · upgrade/uninstall tested ·
ABI compat tests pass · signed test binaries available · (if the WebRTC track ships) a webapp
controller embeds in a sample web app and connects to a native host.

**Risks addressed:** A2. **Effort:** ~8–10 wks.

---

## Phase 6 — Windows production readiness → **M8**

**① Design gate:** multi-monitor selection + layout-version model; HW-encoder capability matrix;
file-transfer + action-catalogue schemas; updater + rollback.

**② Build — tasks**
- ☐ `MED` Multi-monitor + selection; HW-encoder matrix (NVENC/QSV/AMF + no-HW fallback). ☐ `CORE`
  Clipboard text; controlled file transfer (per-transfer approval, hashing, scan hook); signed action
  catalogue. ☐ `NET` Reconnection hardening. ☐ `INF` Signed updater (rollback protection, staged);
  **EV code-signing** (keys in HSM/TPM off build). ☐ `INF` SBOM + enterprise diagnostics. ☐ `QA`
  Compatibility matrix (`docs/08 §5`), long-duration stability, security review, release-rollback.

**③ Exit criteria:** go/no-go criteria in `docs/08 §8` met (no critical security issue, third-party
security assessment, EV-signed installer, crash-free long sessions, direct+relay reliability, input
across layouts, audit verified, one design-partner integration complete).

**Risks addressed:** C5, D5, B5. **Effort:** ~10–12 wks.

---

## Phases 7–9 — Expansion → **M9**

- **Phase 7 — macOS host:** ScreenCaptureKit, VideoToolbox, Accessibility/Screen-Recording
  permissions, LaunchDaemon/Agent, Keychain (P-256 for hardware-bound identity — SE can't hold
  Ed25519), notarization; runtime-probe WebCodecs on WKWebView, native-surface fallback.
- **Phase 8 — multi-party & recording:** multiple controllers, annotations, host cursor overlay,
  recording as a **separate consented product** (FTO review on multi-cursor patents first).
- **Phase 9 — server migration:** `ControlPlaneGrantIssuer`, central revocation, audit upload,
  regional relay directory — **host validator + local enforcement unchanged**.

Each keeps the design-gate rhythm; detailed task breakdowns are authored when the phase is reached.

---

## Cross-phase standing workstreams
- `INF` CI/release hygiene, `cargo-deny`, SBOM, signing — maintained every phase.
- `QA` The test pyramid (`docs/08`) grows with each phase; security paths never merge without tests.
- `SEC` Threat-model (`docs/06`) + risk-register (`docs/13`) re-reviewed at every phase exit.
- **Docs:** each phase updates its `docs/design/phase-<n>-design.md`, the affected specs, and any ADR.

## Governance
- A phase is **not done** until: exit criteria met · relevant `docs/13` High/Critical rows validated
  · security-path tests present · design note + specs + ADRs updated · `CLAUDE.md §3` status bumped.
- Any mid-phase discovery that changes the design gets an ADR (`docs/14`).
