# 13 — Risk Register & Caveats

> Consolidated, severity-ranked risks from the design + grounding research + adversarial red-team.
> Each: **Risk · Severity · Mitigation · How we validate.** Severity = Critical / High / Medium.
> This is the doc to re-read before each phase exit (`docs/07`) and before any external claim.

## A. Product / strategic

| # | Risk | Sev | Mitigation | Validation |
|---|------|-----|-----------|-----------|
| A1 | **Scope is multi-year; lean team stalls on breadth** | High | Ruthless MVP: Windows host + Tauri controller, view-only → single control lease; defer P1+ | Phase exits in `docs/07`; each phase ships a working slice |
| A2 | **Integration complexity becomes the real adoption barrier** (PRD-flagged) | High | SDK ergonomics is a product feature; app-first proves the surface before ABI (ADR-020) | Design-partner integrates the sample in < 1 day (`docs/01 §11`) |
| A3 | **Over-claiming fraud protection → trust + legal liability** | Critical | Honest prevent/deter/cannot-stop language (ADR-050, `docs/15 §6`); never "prevents scams" | Legal review of all marketing; claims map to `docs/15 §6` |
| A4 | **Relay bandwidth is recurring OpEx with no pricing answer** | Medium | Self-host relays (ADR-034); model cost against ~10% relayed / ~5% data-volume | Cost model before launch; monitor direct-vs-relay ratio |

## B. Security (priority 1 — a realized risk here outranks all latency/UX wins)

| # | Risk | Sev | Mitigation | Validation |
|---|------|-----|-----------|-----------|
| B1 | **Social-engineering / unattended-access abuse** (the #1 real-world remote-access harm) | Critical | SAS-bound emergency stop + always-visible indicator + per-session consent + tiered enrollment + friction/cool-off (`docs/15`, `docs/16`) | Red-team scam walkthroughs; measure cool-off abandon rate |
| B2 | **Session-grant theft / replay** | Critical | Sender-constrained endpoint+identity binding (a stolen grant is inert); short TTL + nonce one-time-use + generation counter + emergency blocklist (ADR-040) | Security tests: stolen grant from another endpoint, replayed nonce, old generation (`docs/08 §4`) |
| B2b | **Connection-link / pairing-ticket theft** (stolen or shoulder-surfed link) | High | **Rotating single-use tickets** — consumed on first use, regeneration invalidates prior, short expiry; access still needs local consent (ADR-053, `docs/16 §1.5`) | Security tests: replayed ticket, stale-generation ticket, race-consume detection |
| B3 | **Per-message authorization bypass** (RustDesk CVE-2026-57850 class) | Critical | Enforce capability scope **per message, host-side**; never trust controller-claimed scope (ADR-041) | Property test: reduced grant never expands; fuzz message/scope mismatch |
| B4 | **Privileged input-helper compromise via IPC** | High | OpenSSH/Chromium privsep; authenticate peer by **token/SID or SO_PEERCRED, never PID**; hardened pipe SD + FIRST_PIPE_INSTANCE + secure prefix; validate every field incl. referenced resources; fail closed | IPC authz tests from unauthorized process; malformed helper request fuzz (OpenVPN-CVE-chain analogue) |
| B5 | **Code-signing key compromise** (AnyDesk-2024) | High | HSM/TPM-backed signing kept off build/production; short-lived + revocable; EV cert (ADR-043) | Key-handling audit; revocation drill |
| B6 | **Fraud subsystem itself becomes spyware / legal liability** | Critical | On-device `content → verdict` only; content never egresses/persists; scope-gated; opt-in + disclosed (ADR-044, `docs/15 §5`) | Privacy red-team; schema check that no `content` field compiles; DPIA/LIA on file |
| B7 | **Transport MITM / downgrade** | High | Pinned endpoint identities (we control both ends); TLS 1.3 min on relay legs; no in-band algorithm negotiation; verify downgrade sentinel | Stolen-ticket + MITM tests across the network matrix |
| B8 | **Audit forgery / truncation** | Medium | Hash chain + forward-secure seal + signed Merkle checkpoint + external witness/RFC 3161 + TPM monotonic counter; strict redaction (ADR-042) | Property test: chain detects modification; truncation-detection test |
| B9 | **Metadata leakage at the relay** | Medium | Document what the relay infers (IPs, timing, sizes, graph); self-host to keep it in-house; minimize relay-visible identifiers | Threat-model line item in `docs/06`; relay-operator review |
| B10 | **Tauri Origin-Confusion / renderer reaching privileged IPC** | High | Pin Tauri ≥ 2.11.1; deny-by-default capabilities; Isolation + strict CSP; remote feed to canvas only (ADR-021) | Dependency pin check in CI; capability-surface review |
| B11 | **Compliance gaps block regulated deals** | Medium | Admin-enforceable SSO+MFA, RBAC, exportable/SIEM audit, ≥6-yr configurable retention, BAA-ready, SOC 2 Type II path, FIPS-validated crypto, all-party-consent UX | Procurement checklist; SOC 2 readiness assessment |

## C. Platform / Windows

| # | Risk | Sev | Mitigation | Validation |
|---|------|-----|-----------|-----------|
| C1 | **Single-process MVP is blind on secure desktop & to elevated windows** | High | Enumerate the cliffs honestly (`docs/11 §1`); design the service/agent/helper split boundary now (ADR-023) | Documented behavior across lock/unlock/UAC/user-switch |
| C2 | **DXGI ACCESS_LOST on every desktop/mode transition** | High | Build the release + `DuplicateOutput()` re-acquire loop from day one; handle WAIT_TIMEOUT | Spike: measure recovery time across transitions |
| C3 | **UIPI silently drops input into higher-integrity targets** | Medium | Document; target model uses SYSTEM helper; MVP surfaces the limitation to the user | Test injection into elevated window; assert graceful surfacing |
| C4 | **Mixed-DPI / multi-monitor click drift** | Medium | Per-Monitor-V2 manifest + normalized-coord mapping recipe (`docs/11 §3`) | Multi-monitor mixed-DPI click-accuracy test |
| C5 | **Unsigned/unreputable binary flagged as PUA / SmartScreen-blocked** | High | EV code-signing + reputation build + MS re-eval submission (ADR-043) | Fresh-install SmartScreen/AV pass on clean machines |

## D. Media / transport engineering

| # | Risk | Sev | Mitigation | Validation |
|---|------|-----|-----------|-----------|
| D1 | **Latency targets unvalidated (the core engineering bet)** | High | Spike measures capture→encode→decode→render per stage on defined workloads/networks | Meet `docs/01 §11` targets in the spike or re-plan |
| D2 | **WebCodecs avcC/annexB mismatch = silent decode failure** | Medium | Emit Annex-B, omit `description`; keyframe-first after configure/reset (`docs/10 §5`) | Decode conformance test; first-frame-is-IDR assertion |
| D3 | **VideoFrame leak stalls HW decoder / crashes (<100 frames)** | High | `close()` every frame within a frame or two; manage input+output backpressure (`docs/10 §6`) | Soak test watching decoder buffer pool + memory |
| D4 | **~1 compositor-frame penalty misses a tight SLA** | Medium | Accept for MVP; native-surface fallback trigger defined (ADR-022) | Glass-to-glass measurement vs SLA; profile compositor/present |
| D5 | **~10% of sessions relay-only (symmetric NAT / UDP-blocked)** | Medium | Degrade quality gracefully; surface connection state; adaptive bitrate | Network matrix: symmetric NAT, UDP-blocked, relay-only (`docs/08 §3`) |
| D6 | **WebView2 large-payload IPC regression on Windows** | Medium | Encoded chunks are small (likely fine); benchmark; localhost-WS fallback (`docs/12 §3`) | Early WebView2 Channel throughput benchmark |
| D7 | **Iroh 1.0 is young; behavior on hostile networks unproven for us** | Medium | Pin exact version; avoid `unstable-*`; validate on the enterprise network matrix | Spike: direct/relay success + migration across the matrix |
| D8 | **DRM/HDCP black frames, HDR wash-out, rotated capture, headless no-display** | Low-Med | Document limits; tone-map HDR; rotate; virtual display driver for headless | Capture matrix across content/display types |

## E. Cross-platform / later

| # | Risk | Sev | Mitigation | Validation |
|---|------|-----|-----------|-----------|
| E1 | **macOS WKWebView WebCodecs unconfirmed; open ~3s H.264 decode bug** | Medium | Runtime-probe `isConfigSupported()`; no B-frames mitigates ordering bug; native surface if needed | Probe on min macOS; watch webcodecs #899 |
| E2 | **Linux WebKitGTK too fragile for WebCodecs** | Medium | Plan native-surface path for Linux from the start | Deferred to Phase 7+ |
| E3 | **Apple Secure Enclave can't hold Ed25519 (P-256 only)** | Low | Plan software Ed25519 or P-256 hardware-bound identity on macOS | macOS identity-storage design review |
| E4 | **Multi-cursor remote control has active patents** | Medium | Freedom-to-operate review (US 11,956,290 / US 7,825,896) before shipping multi-party | FTO opinion before Phase 8 |

## How to use this register
- Re-read the relevant rows at each **phase exit** (`docs/07`) and treat unvalidated High/Critical
  rows as blockers.
- Any new **Critical** discovered during build gets an ADR (`docs/14`) if it changes the design.
- The spike (revised Phase 1) exists to convert D1/D7/C2 from "unvalidated" to "measured".
