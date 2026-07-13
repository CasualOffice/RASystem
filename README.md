# Casual RAS — Remote Access System

**Casual RAS** is a **white-label, embeddable remote-access platform**. Software vendors embed it
into their own applications to add secure **screen viewing, remote control, multi-user
collaboration, and approved support actions** — natively, without sending users to a separate
branded remote-desktop product.

It is **not** primarily a standalone remote-desktop app. The deliverables are a native **host
runtime**, a **controller app**, a shared **Rust core**, and — later — **SDKs** extracted from that
core.

> **Status: early execution (Phase 1).** Milestone **M0** is reached (design set complete, Cargo
> workspace builds clean). The **spike-independent Phase-1 orchestration spine is implemented and
> green** — the session state machine, DI seams, host/controller orchestrators, synthetic
> capture/encode doubles, an in-memory loopback transport, adaptive-bitrate + connection-quality, and
> the frame-Channel codec — all exercised end-to-end by a runnable demo and `cargo test` with **no
> iroh, no OS, no GPU**. The real iroh transport, macOS/Windows capture backends, and the Tauri apps
> stay stubbed behind traits until the **Phase-S risk spike** clears (WebCodecs half measured = GO;
> the iroh network-matrix probe is the remaining gate). Live tracker: `docs/17`; status: `CLAUDE.md §3`.

## Priorities (in strict order)

**1. Security → 2. Latency → 3. UX.** When they conflict, the higher one wins. This ordering is a
decision rule enforced throughout the docs, not a slogan (`CLAUDE.md §2`).

## What makes it different

- **Embeddable & white-label** — a small SDK surface, not a separate product.
- **Peer-to-peer over Iroh/QUIC** — encrypted, NAT-traversing, relay-fallback; no backend required
  for the MVP.
- **Host-issued authorization** — the host validates a signed access request, gets local consent,
  and issues a short-lived signed **session grant**. A future server can replace only the *issuer*
  without changing the validator or wire protocol.
- **Capability-based, per-message enforcement** — fine-grained permissions checked host-side on
  every message (a class of bug that has bitten incumbents).
- **Virtual multi-cursor collaboration** — one real OS cursor at a time; everyone else gets a
  rendered virtual pointer.
- **On-device fraud & harm-prevention** — detects and adds friction to remote-access *scams* and
  blocks harmful actions performed through a session, while remaining **on-device and privacy-safe
  (never spyware)** — and honest about the hard limit that a fully-coached victim can't be stopped
  host-side (`docs/15`).
- **Tamper-evident local audit**, **tiered per-device access keys/authenticators** (`docs/16`), and
  **EV-signed runtime** built for regulated verticals (healthcare, MSPs, enterprise IT).

## Architecture at a glance

```
Controller app (Tauri v2: Rust core + React UI)
  └─ Iroh/QUIC (encrypted P2P, relay fallback) ─┐
                                                 ▼
Host runtime (macOS dev-lead, Windows prod)  Host — the authorization authority
  ├─ capture (SCK/DXGI) → H.264 encode (HW)   ├─ Ed25519 identity, signed grants, consent
  ├─ input injection (CGEvent/SendInput)      ├─ capability policy + control leases
  ├─ on-device fraud/harm-prevention          ├─ tamper-evident audit
  └─ (MVP: one process; later: service +      └─ emergency stop (SAS-bound)
       session-agent + privileged input-helper)
```

The controller decodes H.264 with **WebCodecs** and renders to canvas. **macOS is the development-lead
host platform; Windows remains the production target** (ADR-054). The WebCodecs bet is validated on
both Blink and **WebKit/WKWebView** (the macOS Tauri engine) — decode ~1 ms at 60 fps, 0 drops.

## Build strategy

**App-first, extract SDKs later.** We build two working reference apps that share Rust crates
directly, prove the hard parts (latency, NAT traversal, input correctness, authorization), then draw
the SDK boundary around the proven crates. An SDK surface can't be validated without a real consumer.

## Repository layout

```text
crates/                 # shared Rust core (the future SDK internals)
  ras-protocol/         # error taxonomy, control-message set, wire ids
  ras-media/            # capture/encode/decode traits + synthetic doubles
  ras-transport-iroh/   # transport interfaces (iroh wiring lands post-spike)
  ras-core/             # session state machine + orchestrators + ABR + frame codec
  ras-{identity,grant,policy,control,audit}/  # subsystem stubs (Phase 2+)
proto/                  # .proto wire source of truth (codegen wired in Phase 1)
spike/                  # throwaway risk-validation probes (iroh + WebCodecs harness)
docs/                   # architecture + design docs, ADRs, roadmap
```

Full target layout (incl. the Tauri `host/` + `controller/` apps) in `CLAUDE.md §7`.

## Build, test & run the demo

```bash
cargo build --workspace                                   # builds clean
cargo test --all                                          # unit + property + e2e (loopback)
cargo clippy --all-targets --all-features -- -D warnings  # lint gate
cargo deny check                                          # license gate (no GPL/AGPL/SSPL)

# Watch the Phase-1 spine run end-to-end — synthetic capture → controller, no iroh/OS/GPU:
cargo run -p ras-core --example loopback_demo --features testkit
```

The demo prints the handshake, stream negotiation (`avc1.4D401F`), droppable video, adaptive-bitrate
ramp, a keyframe (PLI) round-trip, and clean teardown.

## Documentation

| Doc | Contents |
|-----|----------|
| `CLAUDE.md` | Operating contract: priorities, invariants, decisions, tech stack |
| `CONTRIBUTING.md` | Workflow, standards, review & testing gates |
| `SKILLS.md` | Engineering skill map + reusable playbooks |
| `docs/01_PRD.md` … `08_TEST_AND_RELEASE_PLAN.md` | Product, architecture, HLD, protocol, SDK, security, phases, test/release |
| `docs/09_TRANSPORT_IROH.md` | Iroh/QUIC deep-dive + caveats |
| `docs/10_MEDIA_PIPELINE.md` | Capture → encode → transport → decode → render |
| `docs/11_HOST_PLATFORM_WINDOWS.md` | Windows host internals & OS isolation |
| `docs/12_CONTROLLER_TAURI.md` | Controller architecture & video path |
| `docs/13_RISK_REGISTER_AND_CAVEATS.md` | Severity-ranked risks + validation plan |
| `docs/14_DECISIONS_ADR.md` | Architecture Decision Records (incl. licensing) |
| `docs/15_FRAUD_AND_HARM_PREVENTION.md` | Anti-scam / harm-prevention design |
| `docs/16_ACCESS_AND_ENROLLMENT_MODEL.md` | Per-device keys + security tiers |
| `docs/17_ROADMAP_AND_MILESTONES.md` | **Live progress tracker** — milestones + per-phase ☐/◐/☑ tasks |
| `docs/18_HOST_PLATFORM_MACOS.md` | macOS host deep-dive (dev-lead platform) |
| `docs/design/phase-*.md` | Per-phase design gates + recorded spike results |

New here? Read **`CLAUDE.md`** first, then `docs/02_ARCHITECTURE.md`, `docs/14_DECISIONS_ADR.md`, and
the live status in `docs/17`.

## Licensing

Intended license: **Apache-2.0 for the whole repository** (permissive, explicit patent grant,
Rust-ecosystem norm). Customers may embed Casual RAS in proprietary apps with no copyleft
obligation. **AGPL/SSPL are rejected** (they would force customers to open-source their apps);
**MPL-2.0** is the only alternative under consideration. *Add the full LICENSE text + codec-patent
counsel sign-off before opening the repo.* See `LICENSE.md` and `docs/14 ADR-051`.

---

*Casual RAS is in active design. Everything here reflects current decisions and grounded research
(July 2026) and is subject to change via the ADR process in `docs/14`.*
