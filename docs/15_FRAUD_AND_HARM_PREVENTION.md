# 15 — Fraud & Harm-Prevention Design

> This subsystem detects and reduces fraud/abuse committed **through** a Casual RAS session against
> the local user (remote-access social-engineering scams), and blocks harmful actions performed via
> a remote session. It is a headline differentiator. It is also the single most privacy-sensitive
> component in the product — designed so it protects the user **without becoming spyware**.
>
> Grounded in the fraud/security research + adversarial red-team (July 2026). Priorities:
> **security → latency → UX**.

## 0. The honest core constraint (read this first)

**No host-side control defeats a fully-cooperating, phone-coached victim using their own hands on
their own machine.** The attacker's control channel is the phone call — out-of-band, invisible to
the host — and the victim is a trusted local actor. So this subsystem is **friction, containment,
and harm-reduction** against the coached-victim case, plus **genuinely strong prevention** against
the *remote-attacker* case (stolen ticket, rogue controller, remote-driven credential entry).

We must never claim to "prevent scams," "detect credential capture," or offer "tamper-resistant" or
machine-level protection (§6). Over-claiming to regulated customers is both a trust and a legal
liability.

Scale context (citable): IC3 tech-support fraud **$1.46B (2024)** (seniors ~58%); FTC imposter
scams **$3.5B (2025)**; Phantom-Hacker **>$542M** (H1 2023). *(Do not cite the uncorroborated
"$800M–$1.2B RAT / 75%" vendor figure.)*

## 1. Architecture: a pure on-device `content → verdict` function

**The fraud-protection subsystem is a pure on-device function `content → verdict`. Content never
crosses a process or network boundary — only content-free verdict enums do. It is inert unless a
host-authorized remote session grant is live.**

URLs, window titles, field labels, clipboard values, key values, and screen pixels are
matched/classified in volatile memory and reduced to enums like
`credential_field_focused_during_remote_session`, `sensitive_destination{category}`,
`remote_input_into_credential_field`. **A `content` field is forbidden in verdict payloads at
compile time.** No per-URL cloud lookups — the lookup *is* the exfiltration.

This one invariant is what simultaneously clears GDPR data-minimization, the ECPA/CIPA
third-party-eavesdropper line, the pen-register/URL-egress theory, HIPAA's incidental-disclosure
lane, GLBA service-provider scope, and app-store anti-spyware policy. These become the new
Non-Negotiable Invariants in `CLAUDE.md`.

## 2. Risk engine

A host-side, event-driven risk engine runs **only while a live session grant exists** (hard scope
gate). It fuses cheap signals into a continuous score plus a set of **hard triggers** that fire
regardless of score. Rules:
- **Fail-safe:** score-computation failure escalates, never de-escalates.
- **Hard triggers bypass the score** (secure-desktop switch, `IsPassword`-focus + remote-input
  origin, protected-app focus).
- **Score-based interruptions require a high-confidence composite**, never a single soft signal —
  the primary alert-fatigue defense.
- Weights/lists are **server-updatable and signed**; all matching is local.

### 2.1 Signals we WILL use (cheap, robust, privacy-safe)
| ID | Signal | Source (Windows) | Role |
|----|--------|------------------|------|
| S1 | Foreground app + title | `EVENT_SYSTEM_FOREGROUND`, `QueryFullProcessImageNameW`, Authenticode publisher | risk input |
| S2 | UAC / secure-desktop switch | `EVENT_SYSTEM_DESKTOPSWITCH` + `consent.exe`/`LogonUI.exe` | **hard trigger → input freeze** |
| S3 | Password/secure field focused | UIA `UIA_IsPasswordPropertyId` (role only, never value) | hard trigger in composite |
| S6 | **Input origin + timing + target-role** | host mediates injection → **remote-vs-local origin known exactly** (highest-fidelity signal) | strong / hard trigger |
| — | Concurrent telephony during session | mic/telephony active | strong risk input (evadable — never sole gate) |
| — | First-time / anomalous controller | first-ever peer, odd geo/ASN, unattended-just-enabled, session right after inbound call | risk input; first-time+sensitive = hard trigger |
| — | Repeat sessions over time to same peer | Phantom-Hacker pattern | risk input |

### 2.2 Signals we WON'T use by default (and why)
- **Browser URL (UIA `Value`)** — fragile/localized/spoofable; used **only** reduced to a local
  category boolean, never egressed, never cloud-looked-up. Advisory scorer, never a sole gate.
- **Clipboard content** — off by default; if enabled, **format/length/entropy-bucket only**,
  discarded in-frame (reading the value = reading the secret it protects; high FP on password
  managers).
- **Screen-region OCR (OTP/seed)** — off by default: surveillance-grade, latency-heavy (violates
  priority-2), read-aloud-evadable. Event-gated last resort only.
- **Session recording** — **not part of this subsystem at all**; it is content-at-rest that pulls in
  BAA/GLBA scope. If a deployer wants recording it is a separate, separately-consented product.

Honest limits: `IsPassword` false-negatives on canvas/Electron/plain-text (seed-phrase) fields; URL
lists evaded by look-alike/regional/in-app destinations; read-aloud OTP never touches the machine.
These raise attacker cost; they do not close the coached-victim gap.

## 3. Enforcement ladder (monotonic, fail-safe)

`persistent banner → local re-consent → input-suspend → video-mask → auto-pause (freeze+full mask)
→ terminate + cool-down`

- **Every rung is a pause with a one-action local-user recovery — never a dead-end.** Resume
  authority belongs **only to the local user on a controller-blind channel**; the controller can
  never resume.
- Escalation is monotonic; computation failure escalates.
- **Fail direction is persona-dependent** (below): fail-closed for consumers, fail-to-warn for
  attended support (a guard that fails closed on an MSP fleet is a self-inflicted outage).

### 3.1 Deployment profiles (split by *who deploys*, not just risk)
- **Consumer-Protect** (aggressive, default for consumer-branded installs): hard input-gate on
  credential + remote-origin; finance-denylist → mask+pause; fail-closed. The victim never
  legitimately does remote IT on themselves → low FP, high value. Every block locally recoverable.
- **Attended-Support** (default for MSP/IT SDK licensees): **warn-and-observe.** No auto-termination
  on credential/UAC entry (techs must type passwords). Interrupt only on high-confidence composites.
  Trusted-controller allowlist self-suppresses routine prompts. Time-boxed "allow privileged input"
  grant for elevation. Accessibility & known-tool allowlists ON.
- **Unattended/Fleet:** coerced-victim consent/cool-off layer **disabled by design** (no seated
  user) — mutually exclusive with the above; relies on vaulted per-machine creds, JIT+rotation,
  audit, emergency-stop, honest "remote session active" attestation.

### 3.2 Default-ON vs opt-in
**Always-ON, all profiles (near-zero FP):** always-visible session indicator + **SAS-bound
emergency stop** (kernel-owned Ctrl+Alt+Del — uninterceptable; the single most reliable anti-scam
primitive); **no secure-desktop/UAC injection bypass** (we never request UIAccess → credential/
elevation prompts black out remotely, session continues, local user completes them); S1/S2/S3/S6 as
risk inputs; controller-blind **warn-only banner** on `IsPassword`+remote-origin; **shadow/audit-only
mode for the first N sessions on any new fleet** before enforcement (turns unknowable FP risk into
measured data — highest-priority rollout feature).

**Opt-in / policy-gated (default OFF):** hard input-block on credential+remote-origin (non-consumer);
finance-denylist auto-pause/terminate; session-scoped WFP/DNS financial block; whole-screen privacy;
clipboard heuristic; OCR.

## 4. Protected contexts

| Context | Detect | Action (baseline / strict) | Enterprise exception |
|---|---|---|---|
| Password/secure field | UIA `IsPassword` | warn banner / suspend remote input + mask field | named controller allowlist → audit-only |
| UAC / secure desktop | `DESKTOPSWITCH` + `consent.exe` | freeze injected input; OS blacks out remote side; **session continues** | time-boxed "allow privileged input" grant, local-approved, logged |
| Banking/financial URL | local signed category list → `sensitive_destination{category}` (raw URL discarded in-frame) | mask+banner / auto-pause | per-deployer allowlist by publisher+host pattern |
| OTP / 2FA / seed | `IsPassword`+remote-origin composite (OCR off by default) | as above | OCR only by explicit policy, event-gated, ephemeral, disclosed by name |

**Cross-cutting exception machinery (enterprise-configurable, all logged content-free):** controller
allowlist (Ed25519 identity / signed publisher); protected-app/URL exceptions by Authenticode
publisher + host pattern; **mandatory default accessibility allowlist** (screen readers, voice
control, assistive input — exempt from the "dictated input" heuristic; ADA/508/EN 301 549 risk
otherwise); shadow/audit-only mode; per-tier control-plane policy, admin-locked, auto-lifted at
session end.

## 5. Privacy & legal guardrails (summary — full text in `docs/06`)

Hard **DO:** all analysis on-device/in-process/volatile; content-free verdicts only; enable-time
consent **separate** from connect-consent, revocable, honored completely; persistent "protection
active" indicator; clipboard by format/length/entropy-bucket only; keystrokes by origin/timing/role
only; fail toward the user (warn/cool-off/soft-block) never "log for later"; all-party-consent
default; ship DPIA + LIA + BAA templates.

Hard **DO-NOT:** never transmit screen/keystrokes/field-text/full-URLs/titles/clipboard to
controller/relay/vendor/telemetry/crash logs; never persist content; no OCR by default; never run
covertly or outside an active session; no `content` field anywhere; never repurpose fraud signals
for productivity/marketing/profiling/model-training; no per-URL cloud categorization.

## 6. Honest protection claims (contract-safe language)

- **PREVENT (remote-attacker):** cryptographically prevents unauthorized *remote* access, replay,
  stolen-ticket, phished-code, credential/relay attacks; blocks a *remote operator* from directly
  driving credential entry / elevation / flagged navigation *through the session* ("unless the local
  user performs it themselves"); always-available SAS-bound emergency stop; all detection on-device.
- **DETER / reduce (coached victim, evidence-backed):** mandatory cool-off + specific directed
  warnings that **reduce, not eliminate,** social-engineering losses; raises attacker cost and shifts
  determined attackers off-product; emits an honest remote-session signal cooperating banks can act
  on.
- **NEVER claim:** ❌ "prevents remote-access scams / stops fraud through the session" · ❌ "prevents
  the user entering passwords / visiting banking sites" (only when the *remote peer drives*, only for
  detected fields/listed sites) · ❌ "detects credential/OTP/seed capture" · ❌ "tamper-resistant"
  (must be user-disableable) · ❌ any machine-level scam protection.

**One-line posture:** *Casual RAS makes remote-access fraud harder, slower, noisier, and brand-safe,
and demonstrably rescues some coached victims through forced friction; it cannot stop a user fully
manipulated into using their own hands on their own machine, and must never claim to.*

## 7. Open questions (see `docs/14 ADR` + §10 of the synthesis)
Cool-off durations & gated capability classes per vertical; tamper-resistance vs. anti-stalkerware
tension (disable = high-friction, controller-blind, logged); concurrent-telephony detection
acceptability; and live-assumption validation (Chromium `IsPassword`/URL behavior — native UIA only
from Chrome 138; `Windows.Media.Ocr` MSIX-identity vs Rust packaging; cross-integrity `consent.exe`
enumeration from medium-IL; FIDO2 ergonomics via `webauthn.dll` in Tauri/Rust).

## 8. Sources
FBI IC3 2024 report; FTC imposter-scam data 2025; CISA AA23-025A (RMM abuse); Microsoft UI Automation
(`IsPassword`, focus events); Windows secure-desktop / `EVENT_SYSTEM_DESKTOPSWITCH`; ECPA/CIPA +
*Mikulsky/Sanchez/D'Antonio* wiretap precedents; GDPR Art. 5/35, Art.29 WP 2/2017; Singapore ABS
cooling-off (Oct 2025); Android "break the spell" delay. (Full citations in the research bundle at
`scratchpad/fraud_research.md`.)
