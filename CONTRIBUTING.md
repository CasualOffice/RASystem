# Contributing to Casual RAS

Thanks for working on **Casual RAS**. This is a security-first remote-access system, so the bar
for changes — especially on authorization, input, transport, and audit paths — is deliberately
high. Read `CLAUDE.md` first; it defines the priorities (**Security → Latency → UX**) and the
**Non-Negotiable Invariants** that gate every change.

> **Status:** the project is in the *design phase*. Today, "contributing" means improving the
> design docs. The build/test tooling below describes the target state and will become active when
> execution begins.

---

## 1. Ground rules

- **Design before code.** Land the design in `docs/` (and an ADR for anything structural) before
  implementing.
- **Security wins ties.** If a change trades away any invariant in `CLAUDE.md §5` for latency or
  UX, it does not merge without an ADR and explicit sign-off.
- **The wire protocol lives in `proto/`.** It is the source of truth. Never hand-edit generated
  code; regenerate it.
- **No secret ever reaches a log or crash dump** (keys, tokens/grants, clipboard, typed text, file
  contents, screen pixels). This includes temporary debug lines.

---

## 2. Development environment (target)

Prerequisites (to be pinned precisely in `docs/` as we choose versions):

- Rust (stable, edition set in the workspace `Cargo.toml`) with `rustfmt` and `clippy`. **This is
  all you need for the core crates** — `cargo build/clippy/test/deny` run with no OS/GPU/Node deps
  (protocol codegen is pure-Rust `protox`, no system `protoc`).
- **Controller (Tauri) only:** Node.js LTS + a package manager (pnpm preferred) + Tauri v2 OS
  prerequisites (macOS/WKWebView here; WebView2 on Windows later).
- **macOS host capture/encode (dev-lead, ADR-054):** a real GUI login session + **Screen Recording**
  permission granted to the running binary (TCC) — capture cannot be verified headlessly/over SSH.
  Input work additionally needs the **Accessibility/PostEvent** grant.
- **Platform bring-up follows the spike pattern:** a throwaway probe under `spike/` is authored and
  compile-verified, then **run on the target hardware by a developer** (as with `spike/iroh-probe`);
  numbers/behaviour are recorded before the real backend lands. Author ≠ runtime-verified until a
  developer runs it on-device.

---

## 3. Branching & commits

- Branch from the default branch: `feat/<area>-<short-desc>`, `fix/<area>-<short-desc>`,
  `docs/<short-desc>`, `chore/<short-desc>`.
- Use **Conventional Commits**: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `perf:`,
  `build:`, `chore:`, plus a scope, e.g. `feat(grant): endpoint-bind session grants`.
- Keep commits focused and reviewable. Security-path changes should not be buried inside large
  unrelated diffs.
- Do not commit secrets, key material, or captured frames/recordings — not even in test fixtures.

---

## 4. Pull requests & review

- Every PR describes **what**, **why**, and **security impact** (even if "none").
- **Security-sensitive areas require a second reviewer** and a note on threat-model impact.
  These areas include: `ras-identity`, `ras-grant`, `ras-policy`, `ras-control`, `ras-audit`,
  `ras-transport-iroh`, the input helper, local IPC, and anything touching capabilities, consent,
  or the emergency stop.
- Any change to the **capability registry**, **wire protocol**, **token/grant structure**, or the
  **priority ordering** must link an ADR in `docs/14_DECISIONS_ADR.md`.
- PRs that touch a documented invariant must state which one and how it is preserved.

---

## 5. Coding standards

### Rust
- `cargo fmt` clean; `cargo clippy` with warnings denied in CI.
- **No `unwrap()`/`expect()`/`panic!` on any request-handling, network, or input path.** Use typed
  errors that map to the stable error codes in `docs/04`. Panics are acceptable only for genuine
  invariant violations that indicate a bug, never for attacker-controlled input.
- Validate all network input at the boundary; pass only narrow, normalized representations inward
  (this is the core of the privilege-separation model).
- Prefer `#![forbid(unsafe_code)]` per crate; where `unsafe` is unavoidable (FFI, platform APIs),
  isolate it, document the invariant it upholds, and review it explicitly.
- **Platform FFI & the `unsafe_code = "deny"` workspace lint.** The workspace denies `unsafe` globally.
  OS capture/encode/input (ScreenCaptureKit, VideoToolbox, CGEvent; later DXGI/MF/SendInput) needs FFI,
  so those backends live in **dedicated platform crates** (e.g. `ras-media-macos`, `ras-input-macos`,
  later `*-windows`) that opt back in with a crate-scoped override:
  ```toml
  [lints.rust]
  unsafe_code = "allow"   # FFI backend crate; unsafe confined here, wrapped in a safe API
  ```
  Rules for such a crate: (1) it exposes only a **safe** public surface implementing the `ras-media`/
  input traits — no raw pointers/handles escape; (2) every `unsafe` block carries a `// SAFETY:` note;
  (3) it is a **security-sensitive area** (§4) → second reviewer; (4) core/protocol/policy crates keep
  `unsafe` denied. This keeps the `unsafe` blast radius to the thin platform layer the traits abstract.
- Constant-time comparison for secrets/signatures; use vetted crypto crates — **never roll your
  own crypto**.

### TypeScript / React (controller & consent UIs)
- Strict TypeScript (`strict: true`), ESLint + Prettier clean.
- The **renderer/webview never gets direct access to privileged host IPC.** Privileged calls go
  through the Tauri Rust layer with an explicit, minimal command surface.
- Keep the video hot path allocation-light; never route raw frame pixels through JSON IPC.
- UI must not be able to suppress the session indicator, recording disclosure, or stop control.

### Protocol (`proto/`)
- Additive, backward-compatible changes preferred; never reuse field numbers.
- Bump protocol version per the rules in `docs/04`; document compatibility in the PR.

---

## 6. Testing gates

Tests are scoped to the layer they protect (see `docs/08_TEST_AND_RELEASE_PLAN.md`):

- **Unit:** capability intersection, grant validation, lease generations, replay cache, state
  machines, ticket parser, audit hash chain, coordinate mapping, keyboard normalization.
- **Property:** unknown capabilities always denied; reduced grants never expand permission; old
  control generation never becomes valid again; audit chain detects modification; session expiry
  bounds lease expiry.
- **Fuzz:** every parser that touches untrusted bytes — protobuf decoders, CBOR tickets, grant and
  access-request parsers, IPC protocol, media frame metadata.
- **Integration / E2E:** pairing, consent, session setup, direct + relay paths, control transfer,
  reconnect, agent/helper restart, emergency stop, audit verification.
- **Security tests:** stolen/expired ticket, modified request, replayed nonce, stolen grant from
  another endpoint, old lease, parallel control attempts, unauthorized local IPC, malformed helper
  request, downgrade, unsigned update, audit tampering.

A change to a security path without a corresponding test does not merge.

---

## 7. Documentation requirements

- Update the relevant `docs/` file with any design change.
- Add or update an **ADR** for structural/security/protocol/priority decisions.
- Keep `CLAUDE.md §3 (status)` accurate as the project moves from design to execution.
- New capabilities, protocol messages, and error codes must be reflected in `docs/04`.

---

## 8. Security disclosure

Do not open public issues for vulnerabilities. Report privately to the maintainers (a
`SECURITY.md` with the contact and disclosure policy will be added before any external release).
Security fixes may be developed in private and released with coordinated disclosure.

---

## 9. Definition of done

A change is done when it: upholds the invariants; has layer-appropriate tests; passes fmt/clippy/
lint/CI; updates docs and any ADR; leaks no secrets in logs; and — for security-sensitive areas —
has a second reviewer and a threat-model note.
