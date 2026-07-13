# 16 — Per-Device Access & Enrollment Model

> How each desktop deployment gates remote access with its own keys/factors, offered as **opt-in
> security tiers** the user/deployer chooses. Layered on the core Ed25519 identity + local consent +
> short-lived signed `SessionGrant`; factors are **preconditions to grant issuance**, never
> replacements for the core. Grounded July 2026. Priorities: **security → latency → UX**.

## 0. Two boundaries, kept separate

- **Identity** — "who is the controller, cryptographically?" (Ed25519 endpoint keys; Iroh
  authenticates this).
- **Authorization** — "is this attempt allowed by local policy, and is the approving human doing so
  *freely*?" Enrollment factors harden *this* boundary and compose **additively**.

The crucial honesty (from the fraud red-team): **most factors do nothing against a coached victim,
because the victim relays them.** Only out-of-band + hardware + non-skippable cool-off help, and only
probabilistically. Tiers therefore pair authentication strength with the **capability-containment +
friction** backstop in `docs/15`.

## 1. Factor catalog & anti-coercion property

| Factor | Storage | Defeats (remote attacker) | Helps vs coerced victim? |
|---|---|---|---|
| **Rotating single-use connection ticket** (always-on default) | host-signed, generation-tracked | stolen/leaked link, shoulder-surfed QR, replayed ticket | **No** (still requires consent) |
| Per-install pairing password | Argon2id verifier, TPM-sealed (DPAPI floor) | stolen ticket, rogue controller w/o secret | **No** (phishable/read-aloud) |
| TOTP | TPM-sealed secret | stolen ticket, remote-only | **No** (real-time relay) |
| Host-shown one-time PIN | ephemeral | blind relay, stolen-ticket | **No** once attacker sees screen |
| **FIDO2 / passkey** (WebAuthn / CTAP2 `hmac-secret`) | hardware, non-exportable, origin-bound | phishing, relay, rogue controller | **Partial** (no code to read; gesture still walk-through-able) |
| Windows Hello (TPM-backed) | TPM, gesture-gated | remote-only takeover (needs local human) | **Partial** (presence ≠ intent) |
| MSP unattended cred (vaulted, JIT, rotated) | vault, RBAC | stolen ticket, replay, single rogue controller | n/a (no seated user) |
| **Cool-off / out-of-band host-native confirmation** | — | — | **Best available — the only structural help** |

Honesty: **knowledge/possession factors (pairing password, TOTP, host-PIN) do essentially nothing
against coaching** — relayed. **Origin-bound/hardware/presence factors (FIDO2, Hello) resist remote
relay but not a walked-through gesture** — presence ≠ informed intent. (Note from crypto research:
**Apple Secure Enclave cannot hold Ed25519 — P-256 only**; account for this on any future macOS
hardware-bound identity.)

## 1.5 The always-on default: rotating single-use connection tickets

Every deployment gets this with no configuration; the **phone authenticator (TOTP/FIDO2) is an
optional upgrade** on top of it, not a prerequisite. A connection ticket (the link / QR the host
shares to invite a controller) is **single-use and self-rotating**:

- **Single-use.** A ticket is *consumed* on first successful use (bootstrap/pairing) and is dead
  thereafter. Replay of a consumed ticket is rejected.
- **Rotating — at most one live at a time.** Generating a new ticket increments the host's
  `active_ticket_generation` and **invalidates any previously outstanding ticket**. Regeneration is
  therefore an instant "revoke the old link" action.
- **Short expiry** on top (belt-and-suspenders), independent of use and rotation.

**Mechanism.** The host persists `active_ticket_generation` and the set of consumed `ticket_id`s
(extends the `used_request_nonces` / `active_grant_generations` tables in `docs/03 §11`). A ticket
carries `ticket_id`, `ticket_generation`, `single_use=true`, `expires_at`, host binding, and the
host signature. Validation requires **all** of: signature valid · host binding matches ·
`ticket_generation == active_ticket_generation` · `ticket_id` not in `consumed` · not expired. On
success the host adds `ticket_id` to `consumed` → the ticket is dead.

**What it mitigates:** a **stolen or leaked link**, a **shoulder-surfed QR code**, and a **replayed
ticket** — each is useless after one use, after the next regeneration, or after expiry, whichever is
first. If an attacker races and consumes the ticket first, the legitimate controller's use **fails
visibly** → a tamper signal the host surfaces and audits.

**What it does NOT solve (do not over-claim):** rotation protects the **bootstrap artifact**, not the
**endpoint private key**. True endpoint-key theft is a separate asset covered by TPM-sealed storage +
revocation + session-generation bump + emergency stop (§3). And a stolen ticket **never grants access
on its own** — bootstrap still requires **local host-user consent** (Invariant 1), plus the tier's
authenticator/PIN. The ticket is a *one-time, self-expiring introduction*, not an access token.

**After bootstrap.** A successful pairing stores the controller's Ed25519 identity in the trusted
registry; from then on that identity — not the ticket — is the durable trust anchor (re-consented per
policy). For **one-off attended support** with no persistent pairing, each session simply mints a
fresh single-use ticket. Either way the *session grant* remains short-lived, endpoint-bound, and
generation-versioned, so the full chain is: **single-use rotating ticket → local consent (+ optional
authenticator) → short-lived sender-bound grant → per-message capability checks.**

## 2. Named tiers

- **Standard (Tier 0, consumer default):** Ed25519 + **rotating single-use connection ticket**
  (§1.5) + local consent + **host-shown one-time PIN**. **No phone authenticator required** — it is
  the optional upgrade at Tier 1+. Defeats stolen/leaked link, phished code, replayed ticket, blind
  remote takeover. Does NOT defeat attacker-who-sees-screen or coercion. Keys DPAPI-min, TPM-sealed
  if available.
- **Recommended (Tier 1):** + per-install pairing password + **TPM-backed Windows Hello** local
  confirmation + **mandatory cool-off for first-time/unknown controllers**. Adds a local-human
  requirement + first real coercion friction. *Software-fallback Hello caps advertised tier at 0*
  (verified via TPM key attestation).
- **Hardened (Tier 2, healthcare/regulated/high-value):** + **FIDO2 hardware key per session**
  (origin-bound; optional `hmac-secret`/PRF-derived approval key fused into grant issuance) +
  enforced cool-off with typed acknowledgment on first-time/elevated sessions. Register ≥2
  authenticators. TOTP only as an explicitly-labeled weaker fallback. Best practical coerced-victim
  posture short of removing self-service; residual human risk handled by capability containment
  (`docs/15`).
- **Enterprise/MSP (Tier 3, fleet/unattended):** admin-provisioned **per-machine, vaulted, JIT +
  auto-rotated** unattended creds via `ControlPlaneGrantIssuer`; hardware-backed control-plane key;
  TPM attestation of every enrolled host; full audit; admin-locked policy; optional technician-side
  FIDO2 to check creds out. Coerced-victim consent layer out of scope by design → must be
  **explicit, admin-gated, never silently enabled on consumer installs.** Attended enterprise
  sessions layer Tier 2 on top.

## 3. Key storage / rotation / revocation / recovery

- **Storage:** host Ed25519 key, pairing verifier, TOTP secret **TPM-sealed when present**
  (non-exportable, anti-hammering, attestable); DPAPI only as a capped fallback. FIDO2 credential in
  hardware. *A deployment may advertise Tier ≥1 only if TPM-backed storage is attested; software
  fallback caps at Tier 0.* (Windows "non-exportable" without TPM is a DPAPI policy flag, not
  hardware — SYSTEM can extract it; treat as anti-casual-copy only.)
- **Revocation — three independent kill-switches:** trusted-controller de-listing; session-generation
  bump invalidating live grants; **emergency stop overrides all grant validity** (incl. unattended).
  Lost FIDO2 key → remove its credential ID from the allow-list. Compromised control plane →
  fleet-wide revoke + forced re-enrollment.
- **Recovery:** always require **at least the next-weaker independent factor** (backup FIDO2 key;
  Hello PIN behind TPM lockout; admin-signed fleet recovery). **Never let a phishable factor recover
  a phishing-resistant one** — that collapses the tier.

## 4. Composition with Ed25519 + signed grants

Factors are evaluated in the `SessionGrantIssuer`:
- **Enrollment factors gate `AccessRequest` acceptance.**
- **Approval factors gate the Ed25519 signature** (Hello gesture, FIDO2 assertion, elapsed cool-off).
- Record `assurance_tier` + `factors_satisfied` in the grant for verifiability/audit.
- **FIDO2 `hmac-secret`/PRF can HKDF a hardware-bound approval secret required as grant-issuance
  input**, so a grant **cannot be minted without the physical authenticator present**.
- Fleet path uses the existing `ControlPlaneGrantIssuer` (see `docs/04 §6`); emergency stop overrides
  all grants.

Grant/token format itself: use an **algorithm-pinned signed structure** — the research recommends
**Biscuit** (Apache-2.0; offline attenuation + Datalog caveats + per-block revocation IDs, ideal for
capability scoping and revocation) or PASETO v4.public, over hand-rolled JWT. Sender-constrain the
grant to endpoint + identity (DPoP-style) so a stolen grant is inert without the endpoint's private
key. Ed25519 via **libsodium** (rejects non-canonical/small-order points; note cross-library EdDSA
verification differences). See `docs/04` and `docs/06`.

## 5. The coerced-victim backstop (not an auth factor)

Because no factor stops willing cooperation, the real backstop is **friction + capability
containment** (owned by `docs/15`): non-skippable cool-off with **specific, directed, scam-aware
wording on a controller-blind surface** ("Is someone on the phone telling you to approve this? Hang
up and call your bank on the number on your card."); default-deny sensitive **capability classes**
(finance navigation, credential-field interaction) during interactive remote sessions; capability
elevation forces re-consent + cool-off. Mirrors evidence-backed banking cooling-off periods. It
**reduces, never eliminates,** coerced-victim losses.

## 6. Decisions & open validation
- **ADR:** tiered enrollment (Standard/Recommended/Hardened/Enterprise) composing with Ed25519 signed
  grants; TPM-sealed storage with attestation-gated tier advertising; FIDO2 PRF fused to grant
  issuance; no phishable factor recovers a phishing-resistant one.
- **Open:** minimum tier binding per vertical (mandate TPM+cool-off for healthcare by contract, attest
  in grant to catch white-label downgrades?); FIDO2 RP-ID/attestation ergonomics via
  `webauthn.dll`/`libfido2` in a Tauri/Rust context `[verify]`.

## 7. Sources
WebAuthn / CTAP2 (`hmac-secret`, PRF); Microsoft TPM Platform Crypto Provider, Windows Hello, DPAPI;
NCC Group "exporting non-exportable keys"; Apple Secure Enclave (P-256 only); Biscuit
(biscuitsec.org), PASETO spec; RFC 9449 (DPoP); RFC 8032 + "Taming the many EdDSAs"; libsodium.
(Full citations in `scratchpad/casual-ras-security.md`.)
