# CLAUDE.md — Operating Guide for Casual RAS

> This file is the single source of truth for how anyone (human or AI agent) works in this
> repository. Read it fully before proposing changes. If a change would contradict anything
> under **Non-Negotiable Invariants**, stop and raise it instead of implementing it.

---

## 1. What this project is

**Casual RAS** (Casual Remote Access System) is an **embeddable, white-label remote-access
platform**. Software vendors embed it into their own applications to add secure screen
viewing, remote control, multi-user collaboration, and approved support actions — without
sending their users to a separate branded remote-desktop product.

It is **not** primarily a standalone remote-desktop app. The end products are:
a native **host runtime** (embedded in the customer's app / on the controlled machine),
a **controller app** (the technician/support side), a shared **Rust core**, and — later —
**SDKs** extracted from that core.

Transport is peer-to-peer over **Iroh/QUIC** (encrypted, NAT-traversing, relay-fallback).
Authorization in the MVP is **host-issued**: the host validates a signed access request,
gets local consent, and issues a short-lived signed **session grant**. A future server can
replace only the grant *issuer* without changing the host validator or the wire protocol.

---

## 2. Priorities — the ordering is a decision rule, not a slogan

**1. Security → 2. Latency → 3. UX.**

When two of these conflict, the higher one wins. Concretely:

- **Security beats latency.** Never skip consent, grant validation, capability checks, lease
  checks, or audit writes to shave milliseconds. Do not cache authorization decisions past
  their signed expiry to "go faster."
- **Security beats UX.** Never hide active remote control, remove the emergency stop, or
  suppress an OS permission prompt to make onboarding smoother.
- **Latency beats UX.** Prefer a responsive local cursor and fast frame path over richer but
  slower UI. A stalled video must never freeze the controller's own pointer or the stop button.

If you believe a specific case justifies inverting the order, that is an architecture decision:
write an ADR (see `docs/14_DECISIONS_ADR.md`) and get sign-off. Do not invert it silently.

---

## 3. Current status

- **Phase 0 complete — Milestone M0 reached.** The design doc set is done and the Cargo workspace
  skeleton builds clean. Next up: **Phase S** (risk-validation spike) per `docs/17`.
- **What exists:** dependency-free crate skeletons under `crates/` (`ras-protocol` with the error
  taxonomy, `ras-policy` with capability-intersection + invariant tests, `ras-core` wiring the
  graph, and stubs for identity/grant/control/audit/media/transport-iroh); `deny.toml` license gate;
  `.github/workflows/ci.yml`; `proto/casual_ras.proto` placeholder. The Tauri host/controller apps
  and real subsystem logic land from Phase 1 on.
- **Build/verify commands** (all green as of M0):
  - `cargo build --workspace`
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --all`
  - `cargo deny check` (license gate: allows MIT/Apache/BSD/ISC/Zlib/MPL; denies GPL/LGPL/AGPL/SSPL)
- **Deviation on record** (`docs/design/phase-0-design.md §8`): protobuf codegen is deferred to
  Phase 1 to keep the skeleton offline-buildable without a system `protoc`.

---

## 4. Strategy decisions already made (do not re-litigate without an ADR)

| # | Decision | Rationale |
|---|----------|-----------|
| S1 | **App-first, extract SDKs later.** Build two working reference apps (host + controller) that share Rust crates *directly*, then draw the SDK boundary around the proven crates and add C ABI / N-API. | You cannot validate an SDK surface without a real consumer. SDK-first produces the wrong ABI. |
| S2 | **Controller = Tauri v2** (Rust core + React/TS webview). | Core is already Rust; Tauri reuses the crates in-process with no ABI, fastest iteration. |
| S3 | **Video render path = WebCodecs → canvas/WebGL** in the webview for the MVP. Rust pushes encoded H.264 chunks to JS via Tauri v2's binary `Channel`; `VideoDecoder` decodes; render to canvas. Native-surface fallback reserved for when latency won't close (notably macOS/WKWebView). | Single clean data path, fastest to a working demo. |
| S4 | **Collapse the host process model for the MVP** into one user-space process (capture + encode + Iroh + consent + input). Re-separate into system service + session agent + privileged input helper as a dedicated hardening phase, once the end-to-end system works. | The 3-process split is production security hardening, not functionality. Separating later is mechanical, and we'll know the real boundary messages. **This is a temporary MVP posture — the security story is not complete until it is separated.** |
| S5 | **macOS is the development-lead host platform; Windows remains the production target** (Linux last). | Team is on Mac+Linux — lead on what's testable (ScreenCaptureKit/VideoToolbox/CGEvent); Windows is a port when hardware/CI is available. Architecture is platform-abstracted so this is a scheduling choice (ADR-054, amends ADR-010). |
| S6 | **Rust shared core**, protobuf wire protocol for high-frequency channels, CBOR only for portable tickets. | Cross-platform, performant, versionable. |
| S7 | **No arbitrary shell / no generic filesystem browsing.** Support actions are a signed catalogue with strict argument schemas. | Attack-surface reduction; enterprise/regulated buyers. |
| S8 | **Fraud/harm-prevention is a first-class, on-device, privacy-safe subsystem** — friction + containment against coached-victim scams, strong prevention against remote attackers, honest about its limits. | Differentiator for regulated verticals; over-claiming is a liability. |
| S9 | **Licensing: Apache-2.0 for the whole repo; reject AGPL/SSPL** (MPL-2.0 is the only alternative under consideration). *Add full LICENSE + codec-patent counsel sign-off before opening the repo.* | Permissive embedding is the point of an SDK; Apache adds a patent grant. Trade-off: no license-based moat — differentiation is execution/brand/hosted, not the license. |

See `docs/14_DECISIONS_ADR.md` for the full ADR log and the reasoning behind each.

---

## 5. Non-Negotiable Invariants (security-critical — must never regress)

These are load-bearing for the product's security promise. A change that weakens any of them is
rejected by default, regardless of latency or UX benefit:

1. **The local user is the final owner of the machine.** A controller *requests*; it never
   self-authorizes.
2. **Every privileged behavior is an explicit, named capability.** Unknown capabilities are
   **denied**, never defaulted-on.
3. **Grants and leases are short-lived, signed, and bound** to host + controller + endpoint
   identities. Expired or endpoint-mismatched grants are rejected.
4. **Emergency stop always overrides everything** — grant, lease, policy, in-flight input — and
   takes effect within the target time (≤250 ms locally).
5. **One active OS-input controller at a time by default.** Everyone else is a *virtual* cursor
   that cannot inject input.
6. **The input helper accepts only a narrow, validated set** of normalized input commands. Never
   shell commands, executable paths, OS API names, raw network objects, or controller-supplied
   file paths.
7. **Consent is honest and unspoofable.** Active remote control is always visible; recording is
   always disclosed; the stop control is always present. White-labeling may not hide these.
8. **Secrets never touch logs or crash dumps**: private keys, grant/token contents, clipboard
   data, typed text, file contents, screen pixels.
9. **Transport encryption is necessary but not sufficient** — authorization is enforced by the
   host, not by the transport layer. Iroh gives us a secure pipe, not permission.
10. **Audit is append-only and hash-chained** per session and signed by the host identity.
    Security-sensitive events are recorded.

**Fraud & harm-prevention invariants** (see `docs/15`, `docs/16`):

11. **The fraud-protection subsystem is a pure on-device `content → verdict` function.** Content
    (URLs, titles, field labels, clipboard/key values, pixels) never crosses a process or network
    boundary — only content-free verdict enums do. A `content` field is forbidden in verdict/console
    payloads **at compile time**. No per-URL cloud lookups (the lookup *is* the exfiltration).
12. **The fraud analyzer is inert unless a host-authorized remote session grant is live.** Zero
    content at rest: no screenshots, no keystroke logs, no session recording in the fraud subsystem.
13. **Every enforcement action is a pause with a one-action local-user recovery.** Resume authority
    belongs only to the local user on a controller-blind channel; the controller can never resume.
14. **Never build a secure-desktop/UAC input-injection bypass; never request UIAccess.** The
    emergency stop rides the kernel-owned SAS (Ctrl+Alt+Del) path and overrides any active grant.
15. **Enforce capability scope per message, host-side** — never trust the controller's claimed
    scope (RustDesk CVE-2026-57850 class). Capabilities are fine-grained and never paywalled in the
    core.
16. **A deployment may advertise assurance Tier ≥1 only if TPM-backed key storage is attested**;
    software-fallback installs are capped at Tier 0. No phishable factor recovers a
    phishing-resistant one.
17. **Public protection claims must distinguish prevent (remote-attacker) vs deter (coached victim)
    vs cannot-stop.** Never claim to "prevent scams," "detect credential capture," or offer
    "tamper-resistant" or machine-level protection (`docs/15 §6`).
18. **No GPL/LGPL/AGPL/SSPL in the linked dependency graph** (MIT/Apache-2.0/BSD/ISC/Zlib/**MPL-2.0**
    are fine; `cargo-deny` fails the build on denied licenses). The project itself is **Apache-2.0**.
    RustDesk (AGPL) is study-only, never linked or vendored; pull `scrap`/capture/codec crates from
    permissive upstreams, never the RustDesk fork.

If you're unsure whether something touches an invariant, assume it does and flag it.

---

## 6. Target tech stack

**Native core / host:** Rust, Tokio, **Iroh 1.x (pin exact, no `unstable-*`)**, Prost/Protobuf,
`tracing`, SQLite (rusqlite/SQLx), **libsodium Ed25519**, grant format **Biscuit** (or PASETO
v4.public), platform crates (`windows-rs`). Capture **DXGI Desktop Duplication** (`scrap`/upstream);
input **`enigo`** (upstream MIT) / raw `windows-rs`; encode Media Foundation → NVENC/AMF/oneVPL,
software fallback **OpenH264 (`libloading`) — never x264/GPL**. FEC via `nanors`. C ABI (`cbindgen`)
+ N-API — *deferred to the SDK phase*.

**Controller:** **Tauri v2 (pin ≥ 2.11.1** — Origin-Confusion CVE), React + TypeScript UI,
WebCodecs `VideoDecoder`, canvas/WebGL rendering; deny-by-default capabilities + Isolation + strict
CSP; remote feed to canvas only.

**Supply chain:** `cargo-deny` license gate (**deny GPL/LGPL/AGPL/SSPL as build-breaking**);
`cargo-about`/`cargo-bundle-licenses` → `THIRD-PARTY-NOTICES`; CycloneDX SBOM per release; EV
code-signing with keys in HSM/TPM off build machines.

**Host consent UI (MVP):** small Tauri v2 window (React) so both apps share one UI stack.

**Backend:** none for the MVP. A future control plane (issuer/audit/relay directory) is
explicitly out of scope until Phase 9.

Exact crate choices and versions are pinned in `docs/09`–`docs/12` once research lands. Do not
introduce a new significant dependency without noting it in the relevant doc and, if it touches a
security boundary, an ADR.

---

## 7. Target repository structure

Not yet created. When execution starts, follow this layout (adapted from `docs/02_ARCHITECTURE.md`):

```text
casual-ras/
  crates/                 # shared Rust core (the future SDK internals)
    ras-core/             # session orchestration, state machines
    ras-protocol/         # protobuf messages, framing, versioning
    ras-identity/         # Ed25519 identities, key storage
    ras-grant/            # access requests, session grants, issuer trait
    ras-policy/           # capability intersection, local policy
    ras-control/          # control leases, generations, input routing
    ras-media/            # capture/encode/decode traits + pipeline
    ras-audit/            # hash-chained signed audit journal
    ras-transport-iroh/   # Iroh endpoint, ALPN routing, relay
    ras-ffi/              # C ABI (SDK phase only)
  host/                   # Tauri v2 host app (MVP: single process)
    src-tauri/
    ui/                   # React consent/session-indicator UI
    platform/windows/     # WGC capture, MF/NVENC encode, SendInput
  controller/             # Tauri v2 controller app
    src-tauri/
    ui/                   # React session UI (future React SDK)
  proto/                  # .proto sources (source of truth for the wire)
  docs/                   # architecture + design docs
  examples/               # integration samples (later)
```

---

## 8. Where to find things (doc map)

| Doc | Contents |
|-----|----------|
| `README.md` | Public overview, vision, quick architecture, doc index |
| `CLAUDE.md` | **This file** — operating rules, invariants, decisions |
| `CONTRIBUTING.md` | Workflow, standards, review & testing gates |
| `SKILLS.md` | Engineering skill map + reusable playbooks |
| `docs/01_PRD.md` | Product requirements |
| `docs/02_ARCHITECTURE.md` | Components, boundaries, process model |
| `docs/03_HLD.md` | Runtime flows and state machines |
| `docs/04_PROTOCOL_AND_TOKEN_SPEC.md` | Wire protocol, grants, leases, capabilities |
| `docs/05_SDK_SPECIFICATION.md` | Host/controller/React SDK surfaces (later phase) |
| `docs/06_SECURITY_AND_THREAT_MODEL.md` | Assets, actors, threats, mitigations |
| `docs/07_IMPLEMENTATION_PHASES.md` | Delivery phases and exit criteria |
| `docs/08_TEST_AND_RELEASE_PLAN.md` | Verification, performance, release strategy |
| `docs/09_TRANSPORT_IROH.md` | Iroh/QUIC deep-dive + caveats |
| `docs/10_MEDIA_PIPELINE.md` | Capture → encode → transport → decode → render |
| `docs/11_HOST_PLATFORM_WINDOWS.md` | Windows host internals & OS isolation |
| `docs/12_CONTROLLER_TAURI.md` | Controller architecture & video path |
| `docs/13_RISK_REGISTER_AND_CAVEATS.md` | Consolidated risks with severity + mitigation + validation |
| `docs/14_DECISIONS_ADR.md` | Architecture Decision Records (incl. licensing) |
| `docs/15_FRAUD_AND_HARM_PREVENTION.md` | Anti-scam / harmful-action-blocking design + honest limits |
| `docs/16_ACCESS_AND_ENROLLMENT_MODEL.md` | Per-device keys + authenticator security tiers |
| `docs/17_ROADMAP_AND_MILESTONES.md` | Milestones + phase-wise task plan with per-phase design gates |
| `docs/18_HOST_PLATFORM_MACOS.md` | macOS host deep-dive (dev-lead platform, ADR-054) — *in progress* |
| `docs/design/phase-<n>-design.md` | Per-phase design notes (written at each phase's design gate) |

---

## 9. How to work in this repo (for AI agents especially)

- **Design before code.** We are in the design phase. Produce/adjust docs; do not write
  implementation code until the user approves execution.
- **Match the surrounding code and docs** in tone, structure, and naming.
- **Keep the wire protocol in `proto/` as the source of truth.** Never hand-edit generated code.
- **Any decision that affects a security boundary, the wire protocol, or the priority ordering
  requires an ADR** in `docs/14`.
- **Never introduce code that logs a secret** (see Invariant 8) — this includes debug/trace lines.
- **Prefer explicit, typed errors** with stable machine-readable codes (see the error model in
  `docs/04`). No silent failures on a security path.
- **Flag, don't guess.** If a fact about Iroh/Windows/WebCodecs/crypto is uncertain, mark it and
  ask for validation rather than asserting it.
- **Cost/scope awareness:** this is a large multi-year system. Keep the MVP surface ruthlessly
  small (Windows host + Tauri controller, view-only then single control lease). Resist scope creep
  into P1+ features.

---

## 10. Definition of done (for any change, once we're building)

- Meets the Non-Negotiable Invariants.
- Has tests appropriate to its layer (unit / property / fuzz / integration — see `CONTRIBUTING.md`).
- Security-sensitive changes have a second reviewer and, where relevant, an updated threat model.
- Docs updated (including this file's status section and any affected ADR).
- No secret-leaking logs; no new unauthenticated local endpoints; no new capability that isn't in
  the registry in `docs/04`.
