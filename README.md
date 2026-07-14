# Casual RAS — Remote Access System

**Casual RAS** is a **white-label, embeddable remote-access platform**. Software vendors embed it
into their own applications to add secure **screen viewing, a remote pointer, multi-user
collaboration, and (later) approved support actions** — natively, without sending users to a separate
branded remote-desktop product.

It is **not** primarily a standalone remote-desktop app. The deliverables are a native **host
runtime**, a **controller**, a shared **Rust core**, and — later — **SDKs** extracted from that core.

**🌐 Site:** https://casualoffice.github.io/RASystem/ · **📦 Downloads:** [Releases](https://github.com/CasualOffice/RASystem/releases)

> **Status: alpha (Phase 1 in progress).** A **two-machine app works today**: one unified desktop app
> (ADR-062) that can **Share** this screen or **Connect** to another, peer-to-peer over **real iroh/QUIC**,
> with a connection-ticket flow, **real local Allow/Deny consent** (Invariant 1), and a remote "look
> here" pointer. The full session spine runs end-to-end over two live iroh endpoints. **Sharing is
> macOS-only** for now (ScreenCaptureKit + VideoToolbox); **connecting works on macOS/Linux/Windows**
> (decode-only). Release builds and this marketing site are wired via GitHub Actions. Still ahead:
> Linux/Windows capture backends, the signed grant/lease/capability model, and the fraud-friction
> subsystem. Live tracker: `docs/17`; detailed status: `CLAUDE.md §3`.

## Priorities (in strict order)

**1. Security → 2. Latency → 3. UX.** When they conflict, the higher one wins. This ordering is a
decision rule enforced throughout the docs, not a slogan (`CLAUDE.md §2`).

## What makes it different

- **Embeddable & white-label** — a small SDK surface, not a separate product.
- **Peer-to-peer over Iroh/QUIC** — encrypted, NAT-traversing, relay-fallback; no backend required
  for the MVP. A connection ticket carries the host's identity + addresses so the viewer dials directly.
- **Consent-first** — the local user is the final owner: a viewer is held in the handshake (no pixels)
  until the local user clicks **Allow**; Deny or timeout refuses fail-closed. A controller *requests*;
  it never self-authorizes. The product build does not even link a "skip consent" path.
- **Host-issued authorization** *(roadmap)* — the host will validate a signed access request and issue
  a short-lived signed **session grant**; a future server replaces only the *issuer*, not the validator
  or wire protocol. Today's consent is real but coarse (no signed grants/leases/capabilities yet).
- **Capability-based, per-message enforcement** *(roadmap)* — fine-grained permissions checked
  host-side on every message (a class of bug that has bitten incumbents).
- **Virtual multi-cursor collaboration** — one real OS-input controller at a time; everyone else is a
  rendered virtual pointer. The alpha is view-only + a visual remote pointer (no input injection yet).
- **On-device fraud & harm-prevention** *(roadmap)* — a privacy-safe, on-device subsystem designed to
  add **friction/containment** against remote-access scams. It is honest about limits: it aims to
  **deter** a coached victim and **contain** a remote attacker — never to "prevent scams" or be
  "tamper-resistant" (`docs/15`, Invariant 17).
- **Tamper-evident local audit** and **tiered per-device keys/authenticators** *(roadmap, `docs/16`)*,
  built for regulated verticals (healthcare, MSPs, enterprise IT).

## Try the alpha

Download an installer from [Releases](https://github.com/CasualOffice/RASystem/releases) (macOS
`.dmg`; Linux `.AppImage`/`.deb`; Windows NSIS), or build from source:

```bash
cd app/src-tauri
cargo run          # opens the unified app: Share this screen, or Connect to a screen
```

Two-machine flow: on the sharing machine choose **Share this screen** → copy the `CASUALRAS1:…`
ticket → send it. On the other machine choose **Connect to a screen** → paste → **Connect**. Back on
the sharer, click **Allow**. Move the mouse over the video to point at things on the shared screen.
(macOS first-run prompts for Screen Recording; unsigned alpha builds: right-click → Open.)

## Architecture at a glance

```
Unified desktop app (Tauri v2) — one binary, both roles (ADR-062)
  ├─ Connect (viewer): WebCodecs H.264 → canvas + remote pointer   [macOS/Linux/Windows]
  └─ Share (agent): ScreenCaptureKit → VideoToolbox H.264          [macOS today]
        │
        └─ Iroh/QUIC (encrypted P2P, relay fallback) — the Host is the authorization authority:
             local Allow/Deny consent · always-visible indicator · emergency stop
             (roadmap: Ed25519 signed grants · capability leases · tamper-evident audit)
```

The viewer decodes H.264 with **WebCodecs** and renders to canvas. **macOS is the development-lead
host platform; Windows remains the production target** (ADR-054). The WebCodecs bet is validated on
both Blink and **WebKit/WKWebView** (the macOS Tauri engine) — decode ~1 ms at 60 fps, 0 drops.

## Build strategy

**App-first, extract SDKs later.** We build one reference app that shares Rust crates directly, prove
the hard parts (latency, NAT traversal, authorization), then draw the SDK boundary around the proven
crates. An SDK surface can't be validated without a real consumer.

## Repository layout

```text
crates/                 # shared Rust core (the future SDK internals)
  ras-protocol/         # error taxonomy, control-message set, wire ids + protobuf codec
  ras-media/            # capture/encode/decode traits + synthetic doubles
  ras-media-macos/      # macOS backend: ScreenCaptureKit + VideoToolbox (FFI; unsafe confined here)
  ras-transport-iroh/   # concrete iroh endpoint: control + per-frame video + health planes
  ras-core/             # session state machine + orchestrators + ABR + frame codec + iroh adapter
  ras-host/             # headless host CLI (no-GUI share)
  ras-{identity,grant,policy,control,audit}/  # subsystem stubs (Phase 2+)
app/                    # unified Tauri v2 desktop app — Share + Connect in one binary
site/                   # marketing site (GitHub Pages)
proto/                  # .proto wire source of truth
spike/                  # throwaway risk-validation probes (iroh + WebCodecs + macOS capture)
docs/                   # architecture + design docs, ADRs, roadmap
```

## Build, test & the workspace gates

```bash
cargo build --workspace                                   # builds clean
cargo test --all                                          # unit + property + e2e (loopback + iroh)
cargo clippy --all-targets --all-features -- -D warnings  # lint gate
cargo deny check                                          # license gate (no GPL/AGPL/SSPL)

# Watch the session spine run end-to-end — synthetic capture → controller, no iroh/OS/GPU:
cargo run -p ras-core --example loopback_demo --features testkit
```

The Tauri app (`app/`) is kept **out of the workspace** (heavy WebView deps) — build it from
`app/src-tauri`.

## Documentation

| Doc | Contents |
|-----|----------|
| `CLAUDE.md` | Operating contract: priorities, invariants, decisions, tech stack, **live status** |
| `CONTRIBUTING.md` | Workflow, standards, review & testing gates |
| `SKILLS.md` | Engineering skill map + reusable playbooks |
| `docs/01_PRD.md` … `08_TEST_AND_RELEASE_PLAN.md` | Product, architecture, HLD, protocol, SDK, security, phases, test/release |
| `docs/09_TRANSPORT_IROH.md` | Iroh/QUIC deep-dive + caveats |
| `docs/10_MEDIA_PIPELINE.md` | Capture → encode → transport → decode → render |
| `docs/11_HOST_PLATFORM_WINDOWS.md` | Windows host internals & OS isolation |
| `docs/12_CONTROLLER_TAURI.md` | Controller architecture & video path |
| `docs/13_RISK_REGISTER_AND_CAVEATS.md` | Severity-ranked risks + validation plan |
| `docs/14_DECISIONS_ADR.md` | Architecture Decision Records (incl. licensing, unified app) |
| `docs/15_FRAUD_AND_HARM_PREVENTION.md` | Anti-scam / harm-prevention design + honest limits |
| `docs/16_ACCESS_AND_ENROLLMENT_MODEL.md` | Per-device keys + security tiers |
| `docs/17_ROADMAP_AND_MILESTONES.md` | **Live progress tracker** — milestones + per-phase ☐/◐/☑ tasks |
| `docs/18_HOST_PLATFORM_MACOS.md` | macOS host deep-dive (dev-lead platform) |
| `docs/design/phase-*.md` | Per-phase design gates + recorded spike results |
| `app/README.md` | The unified desktop app — flow, security posture, build |

New here? Read **`CLAUDE.md`** first, then `docs/02_ARCHITECTURE.md`, `docs/14_DECISIONS_ADR.md`, and
the live status in `docs/17`.

## Licensing

**Apache-2.0 for the whole repository** (permissive, explicit patent grant, Rust-ecosystem norm).
Customers may embed Casual RAS in proprietary apps with no copyleft obligation. **GPL/LGPL/AGPL/SSPL
are rejected** and blocked at build time by `cargo-deny`; **MPL-2.0** is the only alternative under
consideration. *Add the full LICENSE text + codec-patent counsel sign-off before a formal release.*
See `docs/14 ADR-051`.

---

*Casual RAS is pre-release software under active development. Everything here reflects current
decisions and is subject to change via the ADR process in `docs/14`.*
