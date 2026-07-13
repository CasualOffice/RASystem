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
- ☐ `NET` **Run** across the network matrix (`docs/08 §3`): same-LAN, different NAT, **symmetric
  NAT**, **UDP-blocked/443-only**, relay-only, migration — record success + direct-vs-relay + RTT.
  *(Blocked: needs a Mac↔Linux two-machine run.)*
- ◐ `MED` Capture skeleton (`spike/latency-probe`, `FrameSource` + synthetic). **macOS-lead:**
  ScreenCaptureKit → VideoToolbox source documented to implement (Windows DXGI+MF noted for the port)
  — measures capture→encode on the Mac once implemented.
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
- ☑ `MED` Synthetic capture/encode doubles (`ras-media::synthetic`) + loopback transport
  (`ras-core::testkit`) + `webcodecs_string`.
- ☑ `NET`/`CORE` Adaptive-bitrate hook wired: `LatencyFirstAbr` + a 250 ms stats/ABR tick driving
  `set_bitrate` and emitting `ConnectionQuality` (control law stays spike-tunable).
- ☑ `UI`/`CORE` Frame-Channel codec (`ras-core::frame_channel`) — the 24-byte header contract shared
  with the future TS decoder worker.
- ☑ `NET`/`CORE` Real protobuf control-channel wire codec (`ras-protocol::codec`, offline `protox`
  codegen) + a generic async `FramedControlChannel` (`ras-transport-iroh`) that runs it over any
  `AsyncRead`/`AsyncWrite` (iroh's stream shape) — length-prefixed, `MAX_CONTROL_FRAME` DoS-guarded,
  tested over an in-memory duplex (round-trip, reassembly, split reads, oversized, peer-close).

*Real backends behind the seams — **gated on the Phase-S go/no-go / hardware**:*
- ☐ `MED` **macOS-lead:** ScreenCaptureKit capture + VideoToolbox encode behind the trait (Windows
  DXGI+MF is the later port). Cursor metadata out-of-band, dirty rects.
- ☐ `MED` HW encoder abstraction + OpenH264 `libloading` software fallback (never x264).
- ☐ `NET` `ras-transport-iroh`: real endpoint, versioned ALPN, channel plumbing over iroh 1.x
  (`Connection::stats()` feeds the existing ABR hook).
- ☐ `MED` FEC (`nanors`) + loss handling (freeze-on-last-good, PLI/IDR request) per `docs/10 §4`.
- ☐ `UI` Controller Tauri shell: Web Worker + `OffscreenCanvas` WebCodecs renderer over the
  frame-Channel codec; connection-state UI; **pin Tauri ≥ 2.11.1**, deny-by-default caps, strict CSP.
- ◐ `QA` Reconnection behavior documented + tested (loopback); perf harness in CI still to wire.

**③ Exit criteria:** stable ~30 FPS on standard desktop workloads · direct + relay sessions work ·
prototype latency targets measured · reconnection documented · local cursor stays responsive during
video stall.

**Risks addressed:** D2, D3, D5, D8, C4. **Effort:** ~6–8 wks.

---

## Phase 2 — Identity, pairing, authorization → **M3**

**Goal:** no frames without authorization. Rotating single-use tickets, consent, signed grants,
replay defense.

**① Design gate (`docs/design/phase-2-design.md`):** ticket/grant/lease wire structures (`docs/04`);
grant format decision (Biscuit vs PASETO, ADR-040); `SessionGrantIssuer` trait + `LocalHostGrantIssuer`;
consent-UI contract; replay-state schema (`consumed_tickets`, nonces, generations).

**② Build — tasks**
- ☐ `SEC` `ras-identity`: persistent Ed25519 host + controller identities; TPM-sealed storage (DPAPI
  fallback), key attestation for tier advertising.
- ☐ `SEC` `ras-bootstrap`: **rotating single-use connection tickets** (`docs/16 §1.5`) — issue,
  generation bump/invalidate-prior, single-use consume, expiry; CBOR+Base64URL/QR encoding.
- ☐ `SEC` Pairing flow + trusted-controller registry + revocation.
- ☐ `SEC` Signed `AccessRequest` validation (signature, endpoint binding, expiry ≤5 min, nonce,
  capability recognition) + `SessionGrant` issuance/validation, **sender-constrained** (DPoP-style).
- ☐ `SEC` Replay defense: nonce cache, ticket generation + consumed set, session generation.
- ☐ `UI` Branded consent UI (identity, reason, requested caps, recording state, duration, stop);
  approve/reduce/view-only/deny; host-shown one-time PIN (Tier 0).
- ☐ `SEC` `ras-policy`: capability intersection + local policy (default-deny unknown).
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

**③ Exit criteria:** external developer completes the sample in < 1 day · upgrade/uninstall tested ·
ABI compat tests pass · signed test binaries available.

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
