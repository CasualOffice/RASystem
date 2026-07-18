# Security Policy

Casual RAS is a remote-access platform: security is the product's first priority, ahead of latency
and UX (see [`CLAUDE.md §2`](CLAUDE.md) and the [Non-Negotiable Invariants](CLAUDE.md#5-non-negotiable-invariants-security-critical--must-never-regress)).
We take vulnerability reports seriously and appreciate coordinated disclosure.

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.** Report privately via **GitHub's private
vulnerability reporting** on this repository:

> Repository → **Security** tab → **Report a vulnerability** (GitHub Security Advisories)

If you cannot use that channel, open a minimal public issue asking a maintainer to open a private
advisory — **without** any vulnerability detail — and we will invite you to the private thread.

Please include, as far as you can:

- affected component (crate under `crates/`, the `app/`, the transport, or the authorization path);
- the version / commit;
- a description of the impact and which [invariant](CLAUDE.md#5-non-negotiable-invariants-security-critical--must-never-regress)
  it breaks;
- reproduction steps or a proof of concept.

We will acknowledge a report within **3 business days**, aim to confirm and trial a fix promptly, and
coordinate a disclosure date with you. We credit reporters who wish to be named.

## Scope

**In scope** — anything that breaks a Non-Negotiable Invariant, for example:

- authorization bypass: acting without a valid, unexpired, endpoint-bound grant, or **per-message
  capability scope not being enforced host-side** (the RustDesk-CVE-2026-57850 class, Inv 3/9/15);
- input injection outside the narrow, validated normalized-command set — shell, paths, OS-API names,
  raw keysyms, or controller-supplied file paths (Inv 6);
- **emergency stop** failing to override a grant/lease/in-flight input (Inv 4);
- a **secret** (private key, grant/token bytes, clipboard data, typed text, file contents, screen
  pixels) reaching a log, trace, or crash dump (Inv 8);
- consent being suppressed, spoofed, or hidden — active remote control, recording disclosure, or the
  stop control not being visible (Inv 1/7);
- audit records being forgeable or the hash chain / signed checkpoint not detecting tampering (Inv 10);
- a **panic or crash on untrusted input** (any wire decoder, a pasted connection ticket, or the audit
  log file) — these are fuzzed and must fail closed, never panic.

**Out of scope**

- The deliberately-refused features documented in [`docs/20 §4`](docs/20_FEATURE_GAPS_AND_ROADMAP.md)
  and Invariant 17 — e.g. we **do not** build a secure-desktop/UAC input-injection bypass or request
  UIAccess (Inv 14); the login screen and UAC prompts are intentionally **not** remotely controllable.
  A report that these are "not bypassable" is working as designed.
- Findings that require an attacker who already has local admin/root on the host.
- The current **alpha builds ship unsigned by the OS** (no code-signing / notarization yet — ADR-072);
  Gatekeeper/SmartScreen warnings are known and expected, not a vulnerability.

## Supported versions

Casual RAS is pre-release (alpha). Only the **latest** release / `main` is supported; please reproduce
against current `main` before reporting.

## Our security model (context for reporters)

- **Host-issued, capability-scoped authorization.** A controller *requests*; the host validates a
  signed, short-lived, endpoint-bound **PASETO v4.public** grant and enforces every capability
  **per message, host-side** — it never trusts the controller's claimed scope.
- **Transport authenticates identity, not authority.** Iroh/QUIC gives an encrypted, peer-authenticated
  pipe; authorization is enforced by the host on top of it (Inv 9).
- **Consent-first and honest.** The local user is the final owner of the machine; active control is
  always visible and the emergency stop always overrides.
- **Content-free by construction.** Audit events, fraud verdicts, and lifecycle events carry only enum
  tags and counters — never content (Inv 8/11).
- **Honest claims (Inv 17).** We distinguish what we **prevent** (a remote attacker), **deter** (a
  coached victim), and **cannot stop**; see [`docs/15`](docs/15_FRAUD_AND_HARM_PREVENTION.md). We do not
  claim to "prevent scams" or offer "tamper-resistant" protection.

Full threat model: [`docs/06_SECURITY_AND_THREAT_MODEL.md`](docs/06_SECURITY_AND_THREAT_MODEL.md).

## Safe harbor

We will not pursue or support legal action against researchers who, in good faith, follow this policy:
who avoid privacy violations, service degradation, and data destruction; who test only against their
own installations; and who give us reasonable time to remediate before public disclosure.

## Supply chain

Dependencies are gated by `cargo-deny` (advisories + bans + licenses + sources) in CI; GPL/LGPL/AGPL/
SSPL are build-breaking (Inv 18). Release integrity signing (Tauri Ed25519 auto-update) is wired and
activates with key provisioning (ADR-078); OS code-signing/notarization is pending (ADR-072).
