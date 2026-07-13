# Phase 0 Design Note — Foundations & first light (→ M0)

> Design gate for Phase 0 per `docs/17`. Reviewed against `CLAUDE.md` invariants before build.
> Scope: a building, tested, CI-backed monorepo skeleton + protocol source-of-truth + invariant
> scaffolding. **No feature logic** — crates are empty-but-compiling.

## 1. Objectives & non-objectives
- **Objective:** every later phase has a place to land — crate graph, error taxonomy, license gate,
  CI, logging policy.
- **Non-objective:** any capture/transport/auth logic; Tauri apps (those are Phase 1); real protobuf
  message set (placeholder only this phase).

## 2. Workspace & crate graph

Single Cargo workspace, `resolver = "2"`, members = `crates/*`. Dependency direction points inward;
no cycles. Phase 0 crates are **dependency-free (std only)** so the build is offline & instant; real
deps (iroh, tokio, prost, libsodium, windows-rs) arrive in the phase that first needs them.

```
ras-protocol   (wire types + error taxonomy home; leaf)
ras-identity   → (later: libsodium, TPM/DPAPI)
ras-grant      → ras-protocol, ras-identity
ras-policy     → ras-protocol            (capability intersection)
ras-control    → ras-protocol            (leases; later: input)
ras-audit      → ras-protocol            (later: hash chain)
ras-media      → ras-protocol            (later: capture/encode traits)
ras-transport-iroh → ras-protocol        (later: iroh)
ras-core       → all of the above        (orchestration; top of the graph)
```
`ras-ffi` (C ABI) is deferred to the SDK phase (M7). `host/` and `controller/` Tauri apps arrive at
Phase 1.

## 3. Error taxonomy & result codes
`ras-protocol` owns `ErrorCode` — a `#[non_exhaustive]` enum mirroring the stable machine-readable
codes in `docs/04 §14` (`INVALID_MESSAGE`, `SIGNATURE_INVALID`, `GRANT_INVALID`, `LEASE_INVALID`,
`REPLAY_DETECTED`, `CONSENT_DENIED`, `CAPABILITY_DENIED`, …). Rule (`CONTRIBUTING.md §5`): no
`unwrap`/`panic` on request/network/input paths — return a typed error carrying an `ErrorCode` + a
safe human message. Codes are stable across versions.

## 4. Invariants encoded as scaffolding
- **Unsafe:** workspace lint `unsafe_code = "deny"` (overridable per-crate later for FFI/platform,
  with justification). `forbid` where genuinely never needed.
- **Secrets in logs (Invariant 8):** a `tracing` setup guideline + a doc test placeholder; a future
  CI grep/lint will assert no secret-typed field is logged. Phase 0 documents the rule; enforcement
  test lands with the first real secret type (Phase 2).
- **License gate (ADR-051):** `deny.toml` allow-list = MIT / Apache-2.0 (+LLVM-exception) / BSD-2/3 /
  ISC / Zlib / Unicode / **MPL-2.0**; anything else (GPL/LGPL/AGPL/SSPL) fails `cargo deny check`.

## 5. Protocol source of truth
`proto/casual_ras.proto` is the wire source of truth (placeholder message this phase). Codegen
(prost + a vendored `protoc` so no system dependency) is wired when the first real message set lands
(Phase 1/2). Generated code is never hand-edited.

## 6. CI matrix
`.github/workflows/ci.yml`: `fmt --check` · `clippy -D warnings` · `test` · `cargo deny check` on
**ubuntu + windows + macos**. (Windows/macOS matter because the host is Windows-first and the
controller is cross-platform.)

## 7. Exit criteria (M0)
Builds on Win + mac dev machines · `fmt`/`clippy -D warnings`/`test` green · `cargo deny check`
green · protocol versioning rule documented · this note reviewed against invariants.

## 8. Deviations from the roadmap task list (recorded)
- **Protobuf codegen deferred within Phase 0** to keep the skeleton offline-buildable with no system
  `protoc`; the `.proto` + a `ras-protocol` module stub land now, codegen wiring is the first task of
  Phase 1. (Rationale: green offline build now > premature codegen.)
- Property testing uses a plain deterministic test in Phase 0 (proptest dependency added in Phase 2
  when capability intersection has real inputs).
