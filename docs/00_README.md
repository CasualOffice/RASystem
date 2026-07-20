# Casual RAS — Documentation Pack

**Casual RAS** (Remote Access System) is a white-label, embeddable remote-access platform built
around: a native **host runtime** embedded on the controlled machine; a **controller app** (Tauri
v2) for the support/technician side; **Iroh/QUIC** encrypted P2P transport with NAT traversal and
relay fallback; **host-issued short-lived signed session grants** (no backend required in the MVP);
explicit **capabilities enforced per message**; **virtual multi-cursors**; an **on-device,
privacy-safe fraud/harm-prevention** subsystem; **tiered per-device access keys**; and
**tamper-evident local audit**.

Priorities, in strict order: **1. Security → 2. Latency → 3. UX.**

> **Status: design phase — documentation only, no code yet (by design).** Start with the root
> `README.md` and `CLAUDE.md`.

## How the docs fit together

**Root (operating & reference):**
- `README.md` — public overview and front door.
- `CLAUDE.md` — the operating contract: priorities as a decision rule, non-negotiable invariants,
  locked strategy decisions, tech stack, doc map, definition of done.
- `CONTRIBUTING.md` — workflow, coding standards, review + testing gates.
- `SKILLS.md` — engineering skill map + reusable playbooks.
- `LICENSE.md` — intended license (Apache-2.0 for the whole repo; AGPL/SSPL rejected).

**Product & architecture (`docs/`):**
1. `01_PRD.md` — Product requirements.
2. `02_ARCHITECTURE.md` — Components, trust boundaries, process model.
3. `03_HLD.md` — Runtime flows and state machines.
4. `04_PROTOCOL_AND_TOKEN_SPEC.md` — Wire protocol, grants, leases, capabilities.
5. `05_SDK_SPECIFICATION.md` — Host/controller/React SDK surfaces (extraction phase).
6. `06_SECURITY_AND_THREAT_MODEL.md` — Assets, actors, threats, mitigations, compliance.
7. `07_IMPLEMENTATION_PHASES.md` — Delivery phases and exit criteria.
8. `08_TEST_AND_RELEASE_PLAN.md` — Verification, performance, compatibility, release.

**Grounded deep-dives (`docs/`, research-backed, July 2026):**
9. `09_TRANSPORT_IROH.md` — Iroh/QUIC transport + caveats.
10. `10_MEDIA_PIPELINE.md` — Capture → encode → transport → decode → render.
11. `11_HOST_PLATFORM_WINDOWS.md` — Windows host internals & OS isolation.
12. `12_CONTROLLER_TAURI.md` — Controller architecture & video path.
13. `13_RISK_REGISTER_AND_CAVEATS.md` — Severity-ranked risks + validation plan.
14. `14_DECISIONS_ADR.md` — Architecture Decision Records (incl. licensing).
15. `15_FRAUD_AND_HARM_PREVENTION.md` — Anti-scam / harmful-action-blocking design.
16. `16_ACCESS_AND_ENROLLMENT_MODEL.md` — Per-device keys + authenticator tiers.
17. `17_ROADMAP_AND_MILESTONES.md` — Milestones + phase-wise task plan (design gate → build → verify).
18. `18_HOST_PLATFORM_MACOS.md` — macOS host deep-dive (development-lead platform, ADR-054).
19. `19_CROSS_PLATFORM_HOST_RESEARCH.md` — Linux/Windows capture·input·encode·build survey (permissive stack).
20. `20_FEATURE_GAPS_AND_ROADMAP.md` — Where we lapse vs incumbents + safe designs + priority.
21. `21_PRODUCTION_READINESS_BACKLOG.md` — Prioritized production-grade gap backlog (P0/P1), graded on behavior.
22. `22_LEARNINGS_TRACKER.md` — Study-only research learnings + half-done-implementation fix tracker (☐/◐/☑).

## Authority model (MVP)

The controller creates and signs an access request. The host validates it, obtains local approval,
issues a short-lived signed session grant, and enforces all capabilities locally **per message**. A
future control-plane can replace the grant *issuer* while the host validator and capability
enforcement remain unchanged.

## Note on docs 01–08

Docs `01`–`08` are the original design pack, being progressively refreshed under the Casual RAS name
to fold in the app-first strategy, the collapsed-process MVP posture, the priority ordering, and the
fraud/access/licensing decisions. Where `01`–`08` and `09`–`16` differ, **the deep-dives (`09`–`16`)
and the ADR log (`14`) are authoritative** — they carry the latest grounded decisions.
