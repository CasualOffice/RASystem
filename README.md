# Casual RAS

**Casual RAS** is a white-label, embeddable remote-access platform. Software vendors embed it into
their own applications to add secure **screen viewing, a remote pointer, multi-user collaboration,
and — later — approved support actions**, natively and under their own brand, without sending users
to a separate remote-desktop product.

It is not a standalone remote-desktop app. The deliverables are a shared **Rust core**, a unified
desktop **application** that plays both roles (share and connect), and — later — **SDKs** extracted
from the proven core.

<p>
  <a href="https://github.com/CasualOffice/RASystem/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/CasualOffice/RASystem/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="Status" src="https://img.shields.io/badge/status-alpha-orange">
  <img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blue">
  <img alt="Platforms" src="https://img.shields.io/badge/share-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-informational">
  <img alt="Transport" src="https://img.shields.io/badge/transport-iroh%2FQUIC-6060d0">
</p>

**Website:** https://ras.casualoffice.org · **Downloads:** [GitHub Releases](https://github.com/CasualOffice/RASystem/releases)

![Casual RAS feature map — Media (screen capture, H.264/WebCodecs, Opus audio, cursor, multi-monitor, adaptive bitrate); Remote control (keyboard + mouse on macOS/Linux/Windows, control leases, emergency stop, per-message capability gate); Data channels (clipboard, file transfer, chat); Security (PASETO grants, local consent, hash-chained audit, paired-controller registry); Transport (iroh/QUIC, separate planes, NAT traversal, hardened session reconnection).](site/assets/feature-map.svg)

---

## Status — alpha, hardening toward production

The **security core and the full remote-access feature set are implemented at the code level**
(CI-green; unit-, property-, fuzz-, and loopback-tested) — signed authorization, per-message
capability enforcement, remote keyboard/mouse control on all three OSes, clipboard, file transfer,
audio, chat, cursor, and multi-monitor. The current focus is **production maturity**: on-device
verification on Linux/Windows, session reconnection, and signed distribution. We grade candidly on
*production behavior*, not "compiles + loopback-green" — the honest gap list is the
[production-readiness backlog](docs/21_PRODUCTION_READINESS_BACKLOG.md).

| Capability | State |
|---|---|
| Connect / view another machine | macOS · Linux · Windows (decode-only, ships everywhere) |
| Share this screen — macOS | Hardware (ScreenCaptureKit + VideoToolbox) — **on-device verified** |
| Share this screen — Linux · Windows | Implemented (PipeWire / Windows.Graphics.Capture → OpenH264); **on-device runtime verification pending** (Windows needs hardware the team lacks) |
| Remote control — full keyboard + mouse | All three backends (CGEvent / XTEST / SendInput) — complete keymaps, relative-pointer, Unicode text, lock-state sync. **macOS on-device verified**; Linux (X11/Xwayland) + Windows on-device pending (Windows blocked on hardware) |
| Signed grants · capability leases · per-message enforcement | Implemented — PASETO v4.public grants, host-authoritative capability gate (Inv 15) |
| Consent · always-visible indicator · emergency stop | Working |
| Tamper-evident audit (hash-chained, host-signed) | Implemented |
| Chat · clipboard · file transfer · output audio | **Wired into the app** (commands + UI) — clipboard & audio are consent-first host opt-ins (default off), file transfer keeps per-transfer Accept/Deny. All-OS capture backends (SCK/PipeWire/WASAPI audio; NSCursor/XFixes/GDI cursor) |
| Cursor channel · multi-monitor | Implemented at code level ([`docs/20`](docs/20_FEATURE_GAPS_AND_ROADMAP.md)); viewer cursor-render deferred (cursor is in the video) |
| **Session reconnection** across a network blip / NAT rebind | Implemented + adversarially hardened — controller re-dials, host re-serves, grant re-validated (never a new auth path), video/control/audio resume on a keyframe; window-bounded both ends so a silent re-dialer can't wedge the host (loopback-tested; iroh re-dial is the on-device step) |
| Signed/notarized installers · activated auto-update | **Not yet** — alpha builds ship unsigned (ADR-072) |
| Fraud-friction subsystem | Roadmap |

Live tracker: [`docs/17_ROADMAP_AND_MILESTONES.md`](docs/17_ROADMAP_AND_MILESTONES.md) · honest
production gap list: [`docs/21`](docs/21_PRODUCTION_READINESS_BACKLOG.md) · detailed engineering
status: [`CLAUDE.md §3`](CLAUDE.md).

## Priorities

**1. Security → 2. Latency → 3. UX.** When they conflict, the higher one wins. This ordering is a
decision rule enforced throughout the design, not a slogan ([`CLAUDE.md §2`](CLAUDE.md)).

## What makes it different

- **Embeddable and white-label** — a small core to embed, not a separate product your users are sent
  to. Your UI, your flow, your support workflow.
- **Peer-to-peer over iroh/QUIC** — encrypted, NAT-traversing, with an encrypted relay fallback. No
  application backend is required for the MVP; a connection ticket carries the host's identity and
  addresses so the viewer dials directly.
- **Consent-first** — the local user is the final owner of the machine. A viewer is held in the
  handshake, with no pixels sent, until the local user clicks **Allow**; Deny or a timeout refuses
  fail-closed. A controller requests; it never self-authorizes. The shipping build does not even link
  a "skip consent" path.
- **Host-issued authorization** — the host validates a signed access request and issues a short-lived,
  endpoint-bound, signed **PASETO v4.public** session grant; a future server replaces only the
  *issuer*, never the validator or the wire protocol. *(Wired end-to-end in the app: the two-phase
  bootstrap → signed access request → grant → session flow, with real local Allow/Deny consent.)*
- **Capability-based, per-message enforcement** — fine-grained permissions checked host-side on every
  message, never trusting the controller's claimed scope (the RustDesk-CVE-2026-57850 class). *(The
  host-authoritative gate is implemented and unit-tested.)*
- **Virtual multi-cursor collaboration** — one real OS-input controller at a time; everyone else is a
  rendered virtual pointer. Full remote keyboard/mouse control is implemented on all three OSes
  (on-device verification pending); collaboration UI is on the roadmap.
- **On-device fraud and harm-prevention** *(roadmap)* — a privacy-safe, on-device subsystem designed
  to add friction and containment against remote-access scams. It is honest about its limits: it aims
  to **deter** a coached victim and **contain** a remote attacker — it does not claim to "prevent
  scams" or be "tamper-resistant" ([`docs/15`](docs/15_FRAUD_AND_HARM_PREVENTION.md), Invariant 17).
- **Tamper-evident local audit** *(implemented)* — a per-session SHA-256 hash chain of content-free
  events, made authentic by a host-signed checkpoint, with crash-safe append-only persistence — plus
  **tiered per-device keys** *(model landed; TPM-attested Tier ≥1 on the roadmap,
  [`docs/16`](docs/16_ACCESS_AND_ENROLLMENT_MODEL.md))*, built for regulated verticals — healthcare,
  MSPs, enterprise IT.

## Try the alpha

Download an installer from [Releases](https://github.com/CasualOffice/RASystem/releases) — macOS
`.dmg`, Linux `.AppImage` / `.deb`, Windows NSIS — or build from source:

```bash
cd app/src-tauri
cargo run          # opens the unified app: "Share this screen" or "Connect to a screen"
```

**Two-machine flow.** On the sharing machine choose **Share this screen**, copy the `CASUALRAS1:…`
ticket, and send it. On the other machine choose **Connect to a screen**, paste the ticket, and click
**Connect**. Back on the sharing machine, click **Allow**. Move the pointer over the video to point at
things on the shared screen.

> macOS prompts for Screen Recording permission on first run. Alpha builds are unsigned: on macOS
> right-click → **Open** the first time; on Windows choose **More info → Run anyway**.

## Architecture

```
Unified desktop app (Tauri v2) — one binary, both roles (ADR-062)
  ├─ Connect (viewer):  H.264 → WebCodecs VideoDecoder → canvas + remote pointer   [macOS · Linux · Windows]
  └─ Share  (agent):    macOS   → ScreenCaptureKit + VideoToolbox (hardware)        [on-device verified]
                        Linux   → PipeWire / xdg-desktop-portal  ┐
                        Windows → Windows.Graphics.Capture       ├─ scap → OpenH264 (software)
        │
        └─ iroh / QUIC (encrypted P2P, relay fallback) — the host is the authorization authority:
             local Allow/Deny consent · always-visible indicator · emergency stop
             Ed25519/PASETO signed grants · capability leases · tamper-evident hash-chained audit
             remote keyboard + mouse injection (CGEvent · XTEST · SendInput)
```

The viewer decodes H.264 with **WebCodecs** and renders to canvas. **macOS is the development-lead
host platform; Windows remains the production target** (ADR-054). The WebCodecs render path is
validated on both Blink and WebKit/WKWebView (the macOS Tauri engine): decode about 1 ms at 60 fps
with zero drops.

**App-first, extract SDKs later.** We build one reference application that shares the Rust crates
directly and prove the hard parts — latency, NAT traversal, authorization — then draw the SDK
boundary around the proven crates. An SDK surface cannot be validated without a real consumer.

## Repository layout

```text
crates/                 # shared Rust core (the future SDK internals)
  ras-protocol/         # error taxonomy, control-message set, wire ids + protobuf codec
  ras-media/            # capture/encode/decode traits + synthetic doubles
  ras-media-macos/      # macOS backend: ScreenCaptureKit + VideoToolbox (FFI; unsafe confined here)
  ras-media-scap/       # cross-platform capture (PipeWire / WGC / SCK) via the scap crate
  ras-media-openh264/   # software H.264 encoder (BGRA → I420 → Annex-B) for Linux/Windows
  ras-audio-opus/       # Opus audio encoder/decoder (output audio, ADR-077/080)
  ras-transport-iroh/   # concrete iroh endpoint: control + per-frame video + audio + health planes
  ras-core/             # session state machine + orchestrators + ABR + frame codec + iroh adapter
  ras-host/             # headless host CLI (no-GUI share)
  ras-identity/         # Ed25519 identities · KeyStore seam · paired-controller registry
  ras-bootstrap/        # rotating single-use connection tickets + replay/nonce cache
  ras-grant/            # signed access requests · PASETO v4.public grants · unattended-access model
  ras-policy/           # capability intersection · signed-catalogue file push
  ras-control/          # control leases · generations · per-message OS-input gate (Inv 15)
  ras-input-{macos,linux,windows}/   # OS keyboard/mouse injection (CGEvent · XTEST · SendInput)
  ras-clipboard/        # cross-platform clipboard write (set-never-paste)
  ras-files/            # safe file-write backend (O_NOFOLLOW|O_EXCL / CREATE_NEW)
  ras-audit/            # hash-chained, host-signed, content-free audit journal
app/                    # unified Tauri v2 desktop app — Share + Connect in one binary
site/                   # marketing site (GitHub Pages)
proto/                  # .proto wire source of truth
spike/                  # throwaway risk-validation probes (iroh + WebCodecs + macOS capture)
docs/                   # architecture + design docs, ADRs, roadmap
```

## Build, test, and workspace gates

```bash
cargo build --workspace                                   # builds clean
cargo test --all                                          # unit + property + e2e (loopback + iroh)
cargo clippy --all-targets --all-features -- -D warnings  # lint gate
cargo deny check                                          # license gate (no GPL/AGPL/SSPL)

# Watch the session spine run end-to-end — synthetic capture → controller, no iroh/OS/GPU:
cargo run -p ras-core --example loopback_demo --features testkit
```

The Tauri app (`app/`) is kept out of the workspace because of its heavy WebView dependencies — build
it from `app/src-tauri`.

## Documentation

| Doc | Contents |
|-----|----------|
| [`CLAUDE.md`](CLAUDE.md) | Operating contract: priorities, invariants, decisions, tech stack, live status |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Workflow, standards, review and testing gates |
| [`docs/01`–`08`](docs/) | PRD, architecture, HLD, protocol, SDK, security, phases, test/release |
| [`docs/09_TRANSPORT_IROH.md`](docs/09_TRANSPORT_IROH.md) | Iroh/QUIC deep-dive and caveats |
| [`docs/10_MEDIA_PIPELINE.md`](docs/10_MEDIA_PIPELINE.md) | Capture → encode → transport → decode → render |
| [`docs/14_DECISIONS_ADR.md`](docs/14_DECISIONS_ADR.md) | Architecture Decision Records (incl. licensing, unified app, cross-platform) |
| [`docs/15_FRAUD_AND_HARM_PREVENTION.md`](docs/15_FRAUD_AND_HARM_PREVENTION.md) | Anti-scam / harm-prevention design and honest limits |
| [`docs/16_ACCESS_AND_ENROLLMENT_MODEL.md`](docs/16_ACCESS_AND_ENROLLMENT_MODEL.md) | Per-device keys and security tiers |
| [`docs/17_ROADMAP_AND_MILESTONES.md`](docs/17_ROADMAP_AND_MILESTONES.md) | Live progress tracker — milestones and per-phase tasks |
| [`docs/18_HOST_PLATFORM_MACOS.md`](docs/18_HOST_PLATFORM_MACOS.md) | macOS host deep-dive (dev-lead platform) |
| [`app/README.md`](app/README.md) | The unified desktop app — flow, security posture, build |

New here? Read [`CLAUDE.md`](CLAUDE.md) first, then
[`docs/02_ARCHITECTURE.md`](docs/02_ARCHITECTURE.md),
[`docs/14_DECISIONS_ADR.md`](docs/14_DECISIONS_ADR.md), and the live status in
[`docs/17`](docs/17_ROADMAP_AND_MILESTONES.md).

## License

**Apache-2.0** for the whole repository — permissive, with an explicit patent grant, and the norm in
the Rust ecosystem. Customers may embed Casual RAS in proprietary applications with no copyleft
obligation. GPL / LGPL / AGPL / SSPL are rejected and blocked at build time by `cargo-deny`; MPL-2.0
is the only alternative under consideration. The full LICENSE text and codec-patent counsel sign-off
land before a formal release (see [`docs/14` ADR-051](docs/14_DECISIONS_ADR.md)).

---

*Casual RAS is pre-release software under active development. Everything here reflects current
decisions and is subject to change through the ADR process in [`docs/14`](docs/14_DECISIONS_ADR.md).*
