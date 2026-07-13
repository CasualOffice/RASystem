# Licensing — Casual RAS

**Intended license: Apache License 2.0 for the entire repository.**

> This is a statement of intended posture. Before the repo is opened externally, add the full
> `LICENSE` (Apache-2.0 text) and, if desired, a `NOTICE` file. Obtain counsel sign-off on the
> codec-patent question below. This document is not legal advice.

## Why Apache-2.0

- **Permissive** — anyone (including our customers) may embed Casual RAS in proprietary applications
  with no copyleft obligation. This is the whole point of an embeddable SDK; AGPL/SSPL are rejected
  because they would force licensees to open-source their apps.
- **Explicit patent grant + patent-retaliation clause** — stronger contributor/user protection than
  MIT.
- **Rust-ecosystem norm** — composes cleanly with the MIT/Apache-2.0 crates we build on.

**Consequence to accept deliberately:** Apache-2.0 places no field-of-use restriction, so competitors
may also use the code — including the fraud/harm-prevention subsystem. The product's differentiation
therefore rests on execution, brand, operated relays/control-plane, and support — **not** on the
license. (If you instead want core-file improvements to stay open when others use them while still
allowing proprietary embedding, switch the repo to **MPL-2.0** — file-level weak copyleft. This is
the only other license under consideration.)

## Dependency-license hygiene (enforced in CI)

Keeping the project cleanly permissive and embeddable:

- **Allowed dependency licenses:** MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, Unicode-DFS, and
  **MPL-2.0** (file-level copyleft is compatible with shipping a permissive project).
- **Denied (build-breaking via `cargo-deny`):** **GPL, LGPL, AGPL, SSPL** and any strong/network
  copyleft — these would impose their terms on the combined work and defeat embeddability.
- **RustDesk (AGPL-3.0) is study-only — never linked, copied, or vendored.** Pull `scrap`, capture,
  and codec crates from their **permissive upstream** sources, never RustDesk's patched fork.
- **Codec patents are separate from copyright licenses.** BSD-2 on `openh264` grants no H.264 patent
  rights; prefer OS/GPU hardware encoders or a royalty-free codec default (AV1). Flag for IP counsel.
- `THIRD-PARTY-NOTICES` is generated per dependency change (`cargo-about` / `cargo-bundle-licenses`);
  a CycloneDX SBOM ships per release.

## Contributions

Use a **DCO** (`Signed-off-by`) or a lightweight CLA. Under Apache-2.0 a CLA is optional (the license
already grants inbound rights incl. patents), but a DCO keeps provenance clean.

## Required before opening the repo

- Add the full Apache-2.0 `LICENSE` text (and optional `NOTICE`).
- Decide the H.264/H.265 patent strategy vs a royalty-free codec (IP counsel).
- Confirm no denied-license dependency is in the graph (`cargo-deny` green).
