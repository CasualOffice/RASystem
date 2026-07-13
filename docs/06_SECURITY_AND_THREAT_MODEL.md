# 06 — Security & Threat Model

> Security is **priority 1**: a realized security risk outranks any latency or UX benefit. This doc
> is the authoritative threat model, deepened with grounding research + adversarial red-team (July
> 2026). Companion docs: authorization/tokens `docs/04`, transport `docs/09`, Windows isolation
> `docs/11`, fraud `docs/15`, access tiers `docs/16`, risks `docs/13`, decisions `docs/14`.
> *Compliance items are directional, not legal advice.*

## 1. Objective

No system honestly promises zero threats. The objective: **reduce attack surface, isolate privileged
functionality, require explicit authorization, make access visible, and make meaningful actions
auditable** — and, uniquely for Casual RAS, **detect and add friction to fraud committed through the
system** without becoming surveillance software.

## 2. Assets

Host & controller signing keys · Iroh endpoint keys · session grants · control leases · screen
content · the input channel · clipboard/files · audit records · the update channel · the trusted-
controller registry · **enrollment factors (pairing secrets, TOTP, FIDO2 credentials)** · **fraud
verdict stream** (must remain content-free).

## 3. Threat actors

Malicious controller user · compromised controller app · compromised customer host app · local
malware · network attacker · **relay operator** · stolen ticket/token holder · malicious update
source · rogue paired controller · **social-engineering scammer coaching the local user** · user
attempting to bypass recording/audit · **insider abusing MSP unattended access**.

## 4. Core protections

Ed25519 identities (libsodium) · mutual endpoint binding · short-lived **sender-constrained** signed
grants · short-lived control leases · **per-message** capability enforcement · explicit consent ·
one active OS-input controller · privilege-separated input helper · authenticated local IPC · nonces
+ sequence numbers + generations + replay cache · signed updates · TPM/DPAPI-protected key storage ·
**SAS-bound emergency stop** · host-visible session indicator · tamper-evident audit chain ·
**on-device fraud/harm-prevention** · **tiered per-device enrollment factors**.

## 5. Prior-incident lessons → our mitigations

Real remote-access incidents that shape the design:

| Incident | What went wrong | Our mitigation |
|---|---|---|
| **TeamViewer 2016** (credential-stuffing → ransomware) | reused consumer passwords, unattended-access abuse | phishing-resistant MFA (FIDO2); tiered enrollment; anomalous-controller signal |
| **TeamViewer 2024** (APT29) | one employee account → corporate IT | tenant/network segmentation; least-privilege control-plane |
| **AnyDesk 2024** (source + **code-signing cert theft**) | signing cert stolen, used to sign malware | HSM/TPM-backed, short-lived signing kept off build/production; revocation (ADR-043) |
| **ScreenConnect CVE-2024-1709** (CVSS 10, auth bypass) → RCE | pre-auth setup endpoint; mass ransomware | eliminate pre-auth surface; **auth before any protocol handling**; fail-closed setup |
| **ScreenConnect CVE-2025-3935** (ViewState/machine-key) | stolen signing/machine keys → forged RCE | protect signing/machine keys as crown jewels; no risky deserialization |
| **BlueKeep CVE-2019-0708** (wormable pre-auth RDP RCE) | listening service, no auth | **no listening ports**; broker/consent-mediated connections; forced auto-update + KEV monitoring |
| **RDP as #1 access vector** (Sophos: ~90% of IR) | exposed RDP, weak/no MFA | no exposed ports; mandatory MFA; valid-account-abuse detection |
| **VNC exposure** (~8k with no auth; plaintext) | unauth/plaintext transport | **no unauthenticated or plaintext transport, ever** |
| **CISA AA23-025A** (RMM social-engineering) | portable RMM pushed via phishing for "refund"/tech-support scams | consent + session transparency; block anonymous portable execution; **fraud subsystem (`docs/15`)** |
| **RustDesk CVE-2026-57850/-58056** (coarse-role bypass) | scope not enforced per message | **per-message host-side capability enforcement (ADR-041)** |

## 6. Authorization & token security (see `docs/04`, `docs/16`)

- **Algorithm-pinned signed grants**, not hand-rolled JWT (avoids `alg:none`/downgrade class).
  Prefer **Biscuit** (offline attenuation + Datalog caveats + per-block revocation IDs — ideal for
  capability scoping/revocation) or **PASETO v4.public**.
- **Sender-constrained** (DPoP-style): grant bound to endpoint + identity so a stolen grant is inert
  without the endpoint's private key. Strict audience check at the verifier.
- **Replay defense in layers:** short TTL + one-time-use nonce/`jti` (verifier persists seen IDs for
  the window) + per-identity generation counter + sequence numbers + endpoint binding.
- **Clock skew:** small leeway on `exp`/`nbf` only, **never on `iat`** (future-dated tokens are a
  replay red flag). Validity windows are wall-clock; monotonic clocks only for local elapsed timing.
- **Revocation:** short TTL + shared blocklist (secret-free, safe to distribute) for emergency kills
  + generation bump for mass invalidation + emergency stop overriding all.
- **Ed25519:** libsodium (rejects non-canonical/small-order encodings; note cross-library EdDSA
  verification differences — pin a strict profile + test vectors). **Apple Secure Enclave is P-256
  only** — plan software Ed25519 or P-256 for macOS hardware-bound identity.
- **Key storage:** **TPM-sealed** (non-exportable, anti-hammering, attestable) preferred; DPAPI as a
  capped fallback (Windows "non-exportable" without a TPM is a policy flag, not hardware).

## 7. Privilege separation (the input helper)

Model on OpenSSH/Chromium privsep: network-facing code is unprivileged; the **input helper is the
privileged broker** accepting only a **fixed, enumerable, per-caller-allowlisted set of typed
operations**, failing closed, validating **every field including referenced resources** (the OpenVPN
CVE chain shows length/DLL-path validation failures even with a typed channel).

- **IPC authentication:** **never authenticate the pipe peer by PID** (`GetNamedPipeClientProcessId`
  is spoofable/duplicatable — CVE-2019-19470). On Windows, **impersonate the client and inspect its
  token/SID**; on Unix use `SO_PEERCRED`. Harden the pipe: restrictive SD (never NULL),
  `FILE_FLAG_FIRST_PIPE_INSTANCE`, secure prefix, logon-SID DACL. Local IPC needs no TLS (DACL + peer
  creds suffice).
- The helper accepts only: normalized pointer move/button/wheel, normalized key event, release-all-
  keys. **Never** shell commands, executable paths, OS API names, raw network objects, or
  controller-supplied file paths.
- Run under a **virtual service account** with minimal `RequiredPrivileges`, not blanket SYSTEM.
- Input injection needs privilege because UIPI blocks are silent (`docs/11 §3`) — constrain *what*
  it injects, not just that it can.

## 8. Transport & relay security (see `docs/09`)

- **E2E encryption** (QUIC/TLS 1.3 always on); content keys live only at the endpoints — **the relay
  cannot decrypt**. Enforce TLS 1.3 minimum on relay legs; eliminate in-band algorithm negotiation
  to kill downgrade attacks; verify the TLS 1.3 downgrade sentinel.
- **MITM defense:** pinned endpoint identities (we control both ends). Prefer SPKI *pinsets* for
  rotation.
- **Metadata is not hidden by E2E:** the relay still sees endpoint IPs, timing, sizes, and the
  connection graph. Document this; self-hosting relays keeps that metadata in-house.
- **Forward secrecy** via TLS 1.3 ephemerals; for long sessions, **periodic re-key** for
  post-compromise security.

## 9. Media privacy

E2E transport; relay cannot decrypt; privacy masks applied before encoding; recording clearly
disclosed and **not part of the fraud subsystem** (`docs/15 §1.4`); frames not written to disk unless
a recording policy requires it; crash dumps exclude frame buffers. DRM/HDCP content is black by the
OS.

## 10. Fraud & harm-prevention security (summary — full design in `docs/15`)

- **On-device `content → verdict`**: content never crosses a process/network boundary; only
  content-free verdict enums do; analyzer inert unless a live grant exists; zero content at rest.
- Enforcement is a **local-user-only, controller-blind** ladder (banner → … → terminate).
- **No secure-desktop/UAC injection bypass; no UIAccess.** Emergency stop rides SAS.
- **Honest claims** (prevent remote-attacker / deter coached-victim / cannot-stop) — never
  "prevents scams" or "detects credential capture."

## 11. File transfer, clipboard, controlled actions

File transfer disabled by default; separate upload/download capabilities; per-transfer or
per-session approval; filename normalization; destination restrictions; size limits; hashing;
malware-scan hook; no auto-execution; metadata-only audit. Clipboard: separate read/write; text-only
initially; local notification on use; size limits; **never log content**. Actions: **signed local
catalogue**, strict argument schema, timeout, output limit, required-approval rule, **no arbitrary
shell**, executable hash/embedded impl, audit start/result/exit.

## 12. Audit integrity

**Tamper-evident, not tamper-proof.** Hash chain (each entry hashes the previous) + **forward-secure
key evolution** (protects the past against a later host compromise) + **periodic signed Merkle
checkpoint** (Signed Tree Head) + **external witness / RFC 3161 timestamp** + **TPM monotonic
counter on seals** (anti-truncation/backdating). A current-key/root compromise still permits forward
forgery — that's the honest limit. **Never log secrets/screen/keystrokes/URLs/clipboard**; audit
records capture *metadata* (who, target, when, duration, capabilities exercised), never content.

## 13. Update security

Signed release manifest + signed binaries; **HSM/TPM-backed EV signing kept off build/production,
short-lived + revocable** (AnyDesk-2024 lesson); certificate/key rotation policy; version-rollback
protection; staged rollout; recovery channel; **no unsigned plugin loading into privileged
processes**; SBOM (CycloneDX); dependency vulnerability + license scanning (`cargo-deny`).

## 14. Compliance signals (directional — see `docs/16`, get counsel)

- **HIPAA:** unique per-user identity (no shared logins), emergency/break-glass access,
  auto-logoff, at-rest + in-transit encryption, **audit controls (required, no opt-out)**, BAA for
  vendor + subprocessors. The zero-content fraud design preserves HIPAA's incidental-disclosure lane.
- **SOC 2 Type II:** unique IDs + MFA + encryption/key management, anomaly detection + exportable
  logs / SIEM integration, change management.
- **Consent/recording:** default to **all-party consent** (cross-state sessions invoke the strictest
  standard); visible "being viewed/recorded" indicator + logged pre-session consent.
- **Enterprise procurement table-stakes:** admin-enforceable SSO (SAML) + org-wide MFA, RBAC,
  session recording (as a separate product), IP allowlisting, SIEM export, ≥6-year configurable
  retention, FIPS-validated crypto for federal buyers.

## 15. Security testing (see `docs/08`)

Fuzz all untrusted-byte parsers (protobuf, CBOR tickets, grant/request, IPC, media metadata);
property tests (unknown capability denied, reduced grant never expands, old generation never valid,
audit chain detects modification, session expiry bounds lease); replay + lease-race + IPC-authz +
privilege-escalation tests; **fraud-subsystem privacy tests** (no `content` field compiles; verdict
egress is content-free; analyzer inert without a grant); scam-walkthrough red-team; static analysis;
dependency + license audit; **third-party penetration test before any production release**.

## 16. Top prioritized risks
See `docs/13_RISK_REGISTER_AND_CAVEATS.md §B` for the ranked security risk table with mitigations and
validation. The top four: social-engineering/unattended abuse (B1), grant theft/replay (B2),
per-message authorization bypass (B3), and the fraud subsystem itself becoming spyware (B6).

## 17. Sources
RFC 9449 (DPoP), 8725bis (JWT BCP), 8032 + "Taming the many EdDSAs", 8446 (TLS 1.3), 3161, 6962 (CT);
Biscuit + PASETO specs; libsodium; OpenSSH privsep, Chromium sandbox; CVE-2019-19470, OpenVPN CVE
chain; TeamViewer/AnyDesk/ScreenConnect/BlueKeep advisories; CISA AA23-025A; NIST SP 800-46/-53
(AC-17), 800-92; OWASP Logging/Pinning; HHS HIPAA §164.312; Schneier–Kelsey + Ma&Tsudik (audit).
Full citations in `scratchpad/casual-ras-security.md`.
