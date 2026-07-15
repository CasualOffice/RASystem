# Phase 2 Design — Identity, Pairing & Authorization (→ M3)

> Scope: PHASE 2 = **no frames without authorization.** Persistent Ed25519 identities, a rotating
> single-use **connection ticket**, real **local consent** (Invariant 1), a signed **AccessRequest**,
> a short-lived **sender-constrained SessionGrant**, and **default-deny capability** recognition.
> Still **view-only + a visual pointer** (OS-input **leases** and multi-cursor are Phase 3); the
> control-plane grant *issuer*, higher assurance tiers (FIDO2/vault), and the hash-chained audit
> journal (ADR-042) are later phases. macOS remains the development-lead host.
>
> Priority order is **STRICT: Security (1) > Latency (2) > UX (3)**. Phase 2 *is* the security phase;
> where authorization work and latency/UX conflict, authorization wins — but the gate runs **once per
> session, before `Active`**, never on the per-frame hot path (§4).
>
> This is a **design document**: code blocks are compile-*conceptual* Rust with `todo!()` bodies,
> dependency-light. They are the source for the crate trait skeletons. No implementation lands until
> execution is approved.
>
> **Load-bearing invariants honored throughout** (`CLAUDE.md`):
> - The local user is the final owner; a controller *requests*, never self-authorizes (Inv 1).
> - Unknown capabilities are **denied**, never defaulted-on (Inv 2).
> - Grants/leases are short-lived, signed, and **bound** to host + controller + endpoint; expired or
>   endpoint-mismatched grants are rejected (Inv 3).
> - Emergency stop overrides everything (Inv 4) — unchanged from Phase 1, never gated behind auth.
> - Transport authenticates **identity**, never **authorization** (Inv 9). Iroh gives a secure pipe,
>   not permission.
> - Capability scope is enforced **per message, host-side** — never trust the controller's claim
>   (Inv 15, ADR-041).
> - A deployment advertises Tier ≥1 **only** if TPM-backed key storage is attested; software fallback
>   caps at Tier 0 (Inv 16).
> - Secrets never touch logs (Inv 8): private keys, grant/token contents, PINs, nonces.
>
> **Prior art this builds on (do not re-litigate):** ADR-003 (host issues grants), ADR-004
> (issuer-agnostic, endpoint-bound), ADR-040 (algorithm-pinned, sender-constrained grants),
> ADR-041 (per-message capability enforcement), the docs/04 wire/token contract, and the docs/16
> enrollment/tier/ticket model. This gate **operationalizes** them; the one open format choice is
> closed in **ADR-064** (§0).

---

## 0. What this gate decides (and what's already decided)

| Question | Resolution | Where |
|---|---|---|
| Who issues grants in the MVP? | The **host** (`LocalHostGrantIssuer`); a future server swaps only the *issuer* behind `SessionGrantIssuer`. | ADR-003/004, §3.3 |
| Grant token format | **PASETO v4.public** (Ed25519, algorithm-pinned) for the MVP host-issued grant; **Biscuit** reserved for when offline attenuation/delegation is needed (control-plane issuer, Phase 9). | **ADR-064 (new, Proposed)**, refines ADR-040 |
| Sender-constraint (stolen-grant defense) | The grant binds `controller_endpoint_id` = the iroh `EndpointId` the transport authenticated; a grant presented from any other endpoint is rejected. No separate DPoP proof needed — iroh already proves endpoint possession at the QUIC/TLS layer (Inv 9). | ADR-040, §5 |
| Capability representation | Namespaced dotted strings (`screen.view`, …), `CapabilitySet = BTreeSet<String>`; unknown-denied; recognition against a versioned catalogue. | docs/04 §8, `ras-policy` (exists), §3.4 |
| Ticket model | Rotating **single-use** ticket, one live at a time, generation-versioned, consumed-set. | docs/16 §1.5, §3.2 |
| MVP assurance tier | **Tier 0** (Ed25519 + rotating ticket + local consent + host-shown one-time PIN). Tier ≥1 (TPM attestation) is designed-for but not required to ship Phase 2. | docs/16, Inv 16, §3.1 |
| Consent mechanism | The existing interactive `GrantValidator` seam (§4); the app's real `LocalConsent` is extended to also **carry the AccessRequest** and **trigger grant issuance** on Allow. | phase-1 §5.5, §4 |

**ADR-064 (Accepted — signed off).** *MVP grant = PASETO v4.public, not Biscuit.* Rationale: in
the MVP the **issuer and validator are the same host**, so Biscuit's headline features (offline
attenuation, Datalog delegation, third-party blocks) buy nothing yet, while adding a heavier Datalog
dependency and a larger audit surface on the security-critical path. PASETO v4.public is a pinned
Ed25519 signature over a small typed footer/claims blob — trivially auditable, `libsodium`-backed,
and sufficient because capability **reduction** is done by *re-issuing a lower-generation grant*
(host is online), not by client-side attenuation. Biscuit becomes the right tool when a
`ControlPlaneGrantIssuer` must mint a broad grant that the host or edge **attenuates offline** — we
adopt it then, behind the same `SessionGrantIssuer` seam, with no wire change to the *validator*.
This **refines** ADR-040 (which left "Biscuit *or* PASETO v4.public" open) — the sender-constraint,
algorithm-pinning, and endpoint-binding requirements of ADR-040 are all preserved. *If sign-off
prefers Biscuit now* (e.g. to exercise attenuation early), only `ras-grant`'s encoder/decoder change;
every other contract in this doc is format-agnostic.

---

## 1. Overview — the bootstrap → session authorization flow

Phase 1 dialed the **session** ALPN directly with a no-op auth gate. Phase 2 puts a **bootstrap**
phase in front of it: the controller proves who it is and *requests* access; the host validates the
request, gets **local human consent**, mints a **sender-constrained grant**, and only then does the
session ALPN carry frames. The `Active` state is still reachable **only** through the `Authorized`
edge that already exists (phase-1 §5.1) — Phase 2 supplies a *real* input to it, not a new branch.

### 1.1 Flow diagram

```
 CONTROLLER (Tauri, cross-platform)                     HOST (macOS-lead)
 ┌───────────────────────────────────────┐             ┌──────────────────────────────────────────┐
 │ import ConnectionTicket (CASUALRAS…)   │             │ ras-bootstrap: issue 1 live ticket        │
 │   ras-bootstrap::decode                │             │   {ticket_id, generation, single_use,     │
 │        │ dial bootstrap ALPN           │  QUIC       │    expires_at, host-bound, host-signed}   │
 │        ▼   casual-ras/bootstrap/1  ═══════════════►  │ validate ticket (sig, host-bind, current  │
 │ ClientHello / PairingRequest(id, sig)  │             │   generation, unconsumed, unexpired)      │
 │        │                               │             │   → consume: add ticket_id to consumed    │
 │        ▼                               │             │ ras-identity: is controller trusted?      │
 │ AccessRequest{caps, reason, nonce,     │  reliable   │   known → skip pairing; new → PairingReq  │
 │   endpoint_id, expires≤5m, sig} ═════════════════►   │ ras-grant: validate AccessRequest         │
 │                                        │             │   (sig, host match, endpoint match, ≤5m,  │
 │                                        │             │    nonce-fresh, caps recognized)          │
 │                                        │             │        │  → LocalConsent (Invariant 1)    │
 │                                        │             │        ▼  PROMPT the human: identity,     │
 │                                        │             │           reason, caps, one-time PIN      │
 │                                        │             │        │  Allow / reduce / view-only/Deny  │
 │ AccessDecision(grant) ◄══════════════════════════   │ ras-grant: LocalHostGrantIssuer.issue()   │
 │   PASETO v4.public, endpoint-bound     │             │   {session_id, caps′, generation, nonce,  │
 │        │ store; present on session      │             │    not_before, expires_at, host-signed}   │
 │        ▼                               │             │                                            │
 │ dial SESSION ALPN casual-ras/1         │  QUIC       │ ras-core: authorize(ctx{grant}) →          │
 │   ControlMsg::AuthEnvelope(grant bytes)═══════════►  │   ras-grant.validate(grant, peer_endpoint)│
 │                                        │             │   OK → SessionEvent::Authorized → Active  │
 │  ◄══════════ frames (Phase-1 path, unchanged) ═════  │   bad → Reject{GrantInvalid|…}             │
 └───────────────────────────────────────┘             └──────────────────────────────────────────┘
```

### 1.2 Prose walkthrough

1. **Ticket (out-of-band).** The host mints exactly **one live** rotating ticket (`ras-bootstrap`),
   shown as a `CASUALRAS…` string / QR. Minting a new one bumps `active_ticket_generation` and
   instantly invalidates the prior (docs/16 §1.5). The ticket is a *bootstrap artifact*, not a
   credential — stolen, it still cannot grant access without local consent (Inv 1).
2. **Bootstrap connect.** The controller dials the **bootstrap ALPN** (`casual-ras/bootstrap/1`,
   separate from the session ALPN). iroh authenticates the controller's `EndpointId` (identity, not
   authority — Inv 9). The host validates the ticket **and consumes it** (single-use).
3. **Pair (first time only).** Unknown controller → the host shows name + key fingerprint; on human
   accept, the controller's Ed25519 public key is stored in `trusted_controllers` (docs/16). Known
   controller → skip straight to the access request.
4. **AccessRequest.** The controller sends a signed `AccessRequest` (docs/04 §4): requested
   capabilities, reason, nonce, `controller_endpoint_id`, `expires_at ≤ 5 min`. The host validates
   it (§5, ordered checks) — signature, exact host match, endpoint == current connection, freshness,
   nonce-not-seen, capabilities recognized.
5. **Consent (Invariant 1).** The host prompts the **local human** (§6): who is asking, why, what
   caps, recording state, a host-shown **one-time PIN** (Tier 0). Allow / reduce-to-view-only / Deny.
   Deny or timeout ⇒ `AccessDecision(denied, ConsentDenied)`, no grant.
6. **Grant issuance.** On Allow, `LocalHostGrantIssuer` mints a **PASETO v4.public** `SessionGrant`
   bound to `{host_id, controller_id, host_endpoint_id, controller_endpoint_id, session_id,
   granted_capabilities (= requested ∩ policy ∩ consented), session_generation, not_before,
   expires_at}` and host-signs it. It is returned on the bootstrap channel.
7. **Session.** The controller dials the **session ALPN** and presents the grant in
   `ControlMsg::AuthEnvelope`. `ras-core`'s `authorize()` (the phase-1 §5.5 seam, now real) validates
   the grant against the freshly-authenticated peer endpoint and emits `Authorized` → the existing
   state machine reaches `Active`. Any mismatch ⇒ `Reject{code}` → `Rejected` (terminal). From here
   the **Phase-1 frame path is unchanged**.

---

## 2. Canonical types & crate homes

New/extended types and where they live. Everything wire-facing is protobuf in `proto/` (source of
truth); the grant *token* is PASETO (its own signed envelope) carried as opaque bytes in
`AuthEnvelope`. Field names match docs/04 exactly.

| Type | Home crate | Status today |
|---|---|---|
| `HostIdentity`, `ControllerIdentity`, `KeyStore`, `AssuranceTier` | `ras-identity` | **empty stub** → populate |
| `ConnectionTicket`, `TicketGeneration`, `ConsumedTickets`, `NonceCache` | `ras-bootstrap` (**new crate**) | create |
| `AccessRequest`, `SessionGrant`, `SessionGrantIssuer`, `LocalHostGrantIssuer` | `ras-grant` | **empty stub** → populate |
| `CapabilitySet`, `Capability` catalogue, `intersect`, `recognize` | `ras-policy` | **partial** (`intersect` exists) → extend |
| `ControlLease`, generation model | `ras-control` | **stub, Phase 3** — only the *grant-side* `session_generation` is Phase 2 |
| Bootstrap `ControlMsg` variants; grant/lease payload messages | `ras-protocol` | `AuthEnvelope` slot + all auth `ErrorCode`s exist → add bootstrap messages |
| `SessionAuthContext` (extend), `GrantValidator` (real impls) | `ras-core` | seam exists → fill |

### 2.1 Identity & tiers (`ras-identity`)

```rust
/// A stable application identity = an Ed25519 keypair + a derived id (public key + format version).
/// Distinct from the iroh *endpoint* identity (which the transport authenticates per connection).
pub struct HostIdentity { /* signing key handle (in KeyStore), public host_id */ }
pub struct ControllerIdentity { /* same shape, controller side */ }

/// Where the private key actually lives. The tier a deployment may *advertise* is bounded by this
/// (Invariant 16): only an attested hardware-backed store unlocks Tier ≥1.
pub trait KeyStore: Send + Sync {
    fn sign(&self, msg: &[u8]) -> Result<Signature, IdentityError>; // key never leaves the store
    fn public_key(&self) -> VerifyingKey;
    /// Attestation evidence, if the platform can prove hardware-backed non-exportable storage.
    fn attestation(&self) -> Option<KeyAttestation>;
    fn tier_ceiling(&self) -> AssuranceTier; // TPM-attested → ≥1; software → Tier0 (Inv 16)
}

#[non_exhaustive]
pub enum AssuranceTier { Tier0, Tier1, Tier2, Tier3 } // docs/16; MVP ships Tier0

// MVP KeyStore impls: macOS file-backed (Tier0) now; TPM/Keychain-sealed (Tier≥1) later.
// NOTE (docs/16 caveat): Apple Secure Enclave is P-256 only — a future macOS hardware-bound identity
// cannot hold Ed25519; account for it then, do not assume Ed25519 in the Secure Enclave.
```

Secrets discipline (Inv 8): `KeyStore` exposes **sign/verify only** — no key export path; `Debug`
is redacted; keys/PINs/nonces never enter `tracing`.

### 2.2 Connection ticket (`ras-bootstrap`, new crate)

```rust
/// Rotating single-use bootstrap artifact (docs/16 §1.5). Host-signed; CBOR + Base64URL/QR encoded
/// (CBOR only for portable artifacts — S6). NOT a credential.
pub struct ConnectionTicket {
    pub ticket_id: TicketId,
    pub ticket_generation: TicketGeneration, // == active_ticket_generation at issue
    pub single_use: bool,                    // always true in MVP
    pub host_id: HostId,                     // binding
    pub expires_at: UnixMillis,
    pub host_signature: Signature,
    // + host endpoint addrs/relay for the dial (reuses the Phase-1 EndpointAddr ticket bytes).
}

/// Host-side rotation + replay state (docs/16). One live ticket at a time.
pub struct TicketAuthority { /* active_generation, consumed: HashSet<TicketId>, store */ }
impl TicketAuthority {
    pub fn issue(&mut self) -> ConnectionTicket { todo!("bump generation, invalidate prior, sign") }
    pub fn consume(&mut self, t: &ConnectionTicket) -> Result<(), ErrorCode> {
        todo!("verify sig+host+expiry; require generation==active; reject if in consumed; then mark consumed")
        // stale generation / already-consumed / expired → RequestExpired|ReplayDetected (visible tamper signal)
    }
}
```

### 2.3 AccessRequest (`ras-grant`) — fields per docs/04 §4

```rust
pub struct AccessRequest {
    pub request_id: RequestId,
    pub protocol_version: u32,
    pub host_id: HostId,                       // exact target host
    pub controller_id: ControllerId,
    pub controller_display_name: BoundedString, // length-bounded (DoS)
    pub controller_endpoint_id: EndpointId,     // == current connection (Inv 3/9)
    pub requested_capabilities: CapabilitySet,
    pub reason: BoundedString,
    pub issued_at: UnixMillis,
    pub expires_at: UnixMillis,                 // ≤ issued_at + 5 min
    pub nonce: Nonce,                           // replay cache
    pub signature: Signature,                   // controller Ed25519 over the canonical encoding
}
```

### 2.4 SessionGrant (`ras-grant`) — fields per docs/04 §5, PASETO-encoded

```rust
pub struct SessionGrant {
    pub grant_version: u32,
    pub session_id: SessionId,
    pub request_id: RequestId,
    pub issuer_id: IssuerId,
    pub issuer_type: IssuerType,               // LocalHost (MVP) | ControlPlane (later)
    pub host_id: HostId,
    pub controller_id: ControllerId,
    pub host_endpoint_id: EndpointId,          // both endpoints bound (Inv 3)
    pub controller_endpoint_id: EndpointId,    // sender-constraint (ADR-040)
    pub granted_capabilities: CapabilitySet,   // requested ∩ policy ∩ consented; immutable in-grant
    pub policy_version: u32,
    pub session_generation: u32,               // reduction = re-issue at lower generation
    pub session_nonce: Nonce,
    pub issued_at: UnixMillis,
    pub not_before: UnixMillis,
    pub expires_at: UnixMillis,                // SHORT — see §5 for the concrete TTL
    // signature is the PASETO v4.public envelope, not a struct field.
}
```

### 2.5 Capabilities (`ras-policy`, extend the existing `intersect`)

`ras-policy` already has `type CapabilitySet = BTreeSet<String>` and
`intersect(&CapabilitySet, &CapabilitySet) -> CapabilitySet` with tests for unknown-denied /
reduced-never-expands. Phase 2 adds:

```rust
/// The versioned, centrally-documented capability catalogue (docs/04 §8/§14). MVP recognizes the
/// view-only subset; input caps are recognized-but-unused until Phase 3 (host still denies them).
pub const CATALOGUE_V1: &[&str] = &[
    "screen.view", "screen.select_monitor", "pointer.virtual", "annotation.create",
    // input/clipboard/file/recording caps exist in the catalogue but are NOT grantable in Phase 2.
];

/// Recognize (default-deny unknown — Inv 2): drop any requested cap not in the catalogue *before*
/// intersection, so an unknown cap can never survive into a grant.
pub fn recognize(requested: &CapabilitySet) -> CapabilitySet { todo!("requested ∩ catalogue") }

/// Grant caps = recognize(requested) ∩ host_policy ∩ consented. Never expands (property-tested).
pub fn grantable(req: &CapabilitySet, policy: &CapabilitySet, consented: &CapabilitySet)
    -> CapabilitySet { todo!() }
```

### 2.6 Wire additions (`ras-protocol`)

Bootstrap-phase `ControlMsg` variants (docs/04 §9), additive to the existing enum (which already has
the session-phase `AuthEnvelope` slot). All error paths reuse the **existing** `ErrorCode` taxonomy
(`RequestExpired`, `ReplayDetected`, `ConsentDenied`, `CapabilityDenied`, `GrantInvalid`,
`IdentityMismatch`, `SignatureInvalid`, `UnsupportedVersion`, `SessionRevoked`) — no new codes.

```rust
// Bootstrap ALPN messages (protobuf; length-prefixed with the existing MAX_CONTROL_FRAME guard).
ClientHello { protocol_version: u32 }
HostHello   { host_id, tier: AssuranceTier }
PairingRequest  { controller_id, display_name, pubkey, sig }
PairingDecision { accepted: bool }
AccessRequestMsg(AccessRequest)
AccessDecision  { grant: Option<Bytes /*PASETO*/>, denied: Option<ErrorCode> }
CancelRequest
ProtocolError { code: ErrorCode }
```

---

## 3. Crate interfaces (conceptual, `todo!()` bodies)

### 3.1 `ras-identity` — identities + key storage + tier ceiling
Populate the empty stub with §2.1. MVP: a file-backed `KeyStore` (Tier 0, redacted, non-exporting)
and the `trusted_controllers` registry (SQLite, docs/16 §11): store/lookup/de-list controller
pubkeys. De-listing is one of the three kill-switches.

### 3.2 `ras-bootstrap` — rotating tickets + replay state (§2.2)
`TicketAuthority::{issue, consume}` + the `NonceCache` (bounded, TTL-swept) shared with AccessRequest
validation. Encode/decode `CASUALRAS…` (reuse the Phase-1 `EndpointAddr::to_ticket` bytes for the
dial info, wrap with the ticket claims). Fail-closed decode.

### 3.3 `ras-grant` — the issuer seam (the heart of Phase 2)

```rust
/// The swap point (ADR-003/004). The MVP host validates AND issues; a future control plane replaces
/// ONLY this impl — the host validator and the wire are unchanged.
#[async_trait]
pub trait SessionGrantIssuer: Send + Sync {
    async fn issue(&self, req: &AccessRequest, consented: &CapabilitySet, session: SessionParams)
        -> Result<Bytes /*PASETO grant*/, GrantError>;
}

pub struct LocalHostGrantIssuer { /* host KeyStore, policy_version */ }
impl SessionGrantIssuer for LocalHostGrantIssuer { /* mint + PASETO-sign, §2.4 */ }

/// Pure, host-side validation — no I/O, unit + property + fuzz tested. Ordered checks in §5.
pub fn validate_access_request(req: &AccessRequest, host: &HostId, peer_endpoint: &EndpointId,
    now: UnixMillis, nonces: &mut NonceCache) -> Result<(), ErrorCode> { todo!() }

pub fn validate_grant(grant_bytes: &[u8], host: &HostId, peer_endpoint: &EndpointId,
    now: UnixMillis, verifier: &VerifyingKey) -> Result<SessionGrant, ErrorCode> { todo!() }
```

### 3.4 `ras-policy` — extend as §2.5. `ras-control` stays a **Phase-3** stub (leases/multi-cursor);
Phase 2 only produces the `session_generation` field on the grant.

---

## 4. Filling the `ras-core` auth seam (phase-1 §5.5 → real)

The seam is already shaped for this (Inv-safe, additive). Phase 2 changes are **additive**, no
renamed variant, no signature break:

1. **Extend `SessionAuthContext`** (already `#[non_exhaustive]`) so `authorize()` sees what it needs:
   ```rust
   #[non_exhaustive]
   pub struct SessionAuthContext {
       pub peer_identity: PeerIdentity,     // iroh-authenticated EndpointId (existing)
       pub access_request: bytes::Bytes,    // existing slot — now the PASETO grant on the session ALPN
       // additive:
       pub host_id: HostId,
       pub now: UnixMillis,
   }
   ```
2. **The real `GrantValidator`.** The session-phase validator (`GrantSessionValidator`) parses
   `access_request` as the PASETO grant and calls `ras_grant::validate_grant` against
   `peer_identity` (the endpoint iroh just authenticated) — enforcing the sender-constraint at the
   exact moment the endpoint is proven. `Ok → GrantDecision::Authorized(caps)`; mismatch →
   `Denied(GrantInvalid|IdentityMismatch|RequestExpired)`. *The bootstrap-phase consent* (steps 4–6)
   uses the same seam shape via the app's real `LocalConsent` (which already returns
   `NeedConsent`/`Denied` today) — Phase 2 wires its Allow to `LocalHostGrantIssuer.issue`.
3. **`GrantDecision::Authorized` carries the capability set** (additive tuple field) so the session
   starts knowing its granted caps for the per-message checks (Inv 15 / ADR-041) — even though the
   MVP grants only view-only caps, the *enforcement path* exists so Phase 3 input is additive.
4. **State machine unchanged.** `Active` is still reached only via `SessionEvent::Authorized`
   (phase-1 §5.1). `Reject{code}` → `Rejected`; grant `expires_at` reached mid-session →
   `Expire{code}` → `Expired`. Emergency stop → `Revoke` → `Revoked`, **overriding a valid grant**
   (Inv 4) exactly as today.
5. **Latency:** all of this runs in `ControlEstablished`, **before** `Active` and before any frame.
   The per-frame path never validates a grant. Per-*message* capability checks (Phase 3 input) are
   O(1) set lookups, off the video path.
6. **`AllowAllValidator` is dropped from the shipping build** — already `#[cfg(insecure-no-auth)]`
   and mutually exclusive with auth; the unified app already builds `default-features = false`, so it
   is not even linked (done in Phase 1's app work).

---

## 5. Validation order & replay-state schema

**Ordered checks (fail-closed, first failure wins, cheapest/most-decisive first)** — mirrors
docs/04 §11:

*Ticket consume (bootstrap):* ① host signature valid → ② `host_id` binds to us → ③ not expired →
④ `ticket_generation == active_ticket_generation` (else `ReplayDetected`) → ⑤ `ticket_id ∉ consumed`
(else `ReplayDetected`) → **then** mark consumed.

*AccessRequest:* ① `protocol_version` supported (`UnsupportedVersion`) → ② signature valid over
canonical bytes (`SignatureInvalid`) → ③ `host_id` == us (`IdentityMismatch`) →
④ `controller_endpoint_id` == authenticated peer (`IdentityMismatch`) → ⑤ `now ≤ expires_at` and
`expires_at − issued_at ≤ 5 min` (`RequestExpired`) → ⑥ `nonce ∉ NonceCache` (`ReplayDetected`) →
⑦ bounded display_name/reason → ⑧ capabilities recognized (unknown-denied, Inv 2). **Then** consent.

*SessionGrant (session ALPN):* ① PASETO v4.public verify with host key (`GrantInvalid`) →
② `grant_version` supported → ③ `host_id`/`host_endpoint_id` == us → ④ `controller_endpoint_id` ==
authenticated peer (**sender-constraint**, `IdentityMismatch`) → ⑤ `not_before ≤ now ≤ expires_at`
(`RequestExpired`) → ⑥ `session_generation` current (`ReplayDetected`) → ⑦ caps ⊆ catalogue.

**Concrete TTLs (this gate sets, ADR-064):** AccessRequest ≤ **5 min** (docs/04); SessionGrant
**≤ 10 min** default, and **never exceeds the session** (a mid-session `expires_at` → `Expired`);
ticket **≤ 15 min**. All configurable down, not up.

**Replay-state schema (host, in-memory + SQLite mirror):**

```
active_ticket_generation : u32                         -- one live ticket; bump invalidates prior
consumed_tickets         : HashSet<TicketId>           -- single-use; TTL-swept at expiry
nonce_cache              : LRU/TTL set of Nonce         -- AccessRequest replay window (≥ max req TTL)
session_generation       : per-session u32             -- reduction / revoke bump
trusted_controllers      : SQLite {controller_id, pubkey, paired_at}  -- de-list = kill-switch
```

---

## 6. Consent-UI contract (Invariant 1, 7)

The host consent prompt (reusing the app's existing consent window) MUST show, content-free-safe:
**who** (controller display name + key fingerprint), **why** (bounded reason), **what**
(human-readable requested capabilities), **recording state**, **session duration/expiry**, and an
always-present **Stop** (Inv 7 — white-labeling may not hide these). Actions: **Allow** /
**Reduce to view-only** / **Deny**. Tier 0 additionally shows a **host-generated one-time PIN** the
remote party must read back out-of-band (docs/16). Resume/authority is **local-only**: the controller
can never self-approve, reduce-upward, or resume (Inv 1). Deny or a timeout (default 90 s, matching
the current `LocalConsent`) ⇒ `ConsentDenied`, fail-closed, no grant.

---

## 7. What stays stubbed after Phase 2

- **OS-input leases + generations + multi-cursor** (`ras-control`) — Phase 3. Phase 2 grants only
  view-only + virtual-pointer caps; input caps are *recognized but never grantable*.
- **Control-plane grant issuer** (`ControlPlaneGrantIssuer`) + **Biscuit attenuation** — Phase 9;
  the `SessionGrantIssuer` seam and format-agnostic validator make it a drop-in (ADR-064).
- **Assurance Tier ≥1** (TPM/Keychain-sealed keys, key attestation, FIDO2/vault, Windows Hello) —
  designed-for in `ras-identity` (§2.1) but the MVP ships **Tier 0**; Inv 16 is enforced by
  `KeyStore::tier_ceiling` refusing to advertise ≥1 without attestation.
- **Hash-chained signed audit journal** (`ras-audit`, ADR-042) — later; Phase 2 emits the
  content-free lifecycle events it will consume, but the journal itself is out of scope.

---

## 8. Open questions — resolutions

- **Q-GRANT-FMT — RESOLVED.** MVP `SessionGrant` = **PASETO v4.public** (ADR-064 Accepted). Biscuit
  reserved for the later offline-attenuating control-plane issuer, behind the unchanged
  `SessionGrantIssuer` seam.
- **Q-PAIR-TOFU — MVP default.** First-pairing is trust-on-first-use gated by human key-fingerprint
  check **+ host-shown one-time PIN** (Tier 0). A stronger out-of-band channel (short-authentication-
  string / numeric compare) is deferred to the tier ladder (Tier ≥1), not required for the attended
  MVP.
- **Q-NONCE-WINDOW — MVP default.** Nonce-cache retention = `max(AccessRequest TTL)` (i.e. ≥5 min),
  TTL-swept. A longer defense-in-depth window is a fleet-hardening knob, not MVP.
- **Q-GEN-STORE — MVP default.** MVP scope is **attended-only**: `session_generation` /
  `consumed_tickets` live in-memory (a host restart ends live sessions anyway). The SQLite mirror for
  restart-survival lands with unattended/fleet (Phase 9). `trusted_controllers` **is** persisted
  (SQLite) from the MVP, since pairings must survive restart.

---

## 9. Security test matrix (exit criteria)

Every row is a required test before M3 (`docs/17` Phase-2 ③). Unit + property + fuzz + integration.

| Attack / property | Expected | Layer |
|---|---|---|
| Unknown (unpaired, un-consented) controller | never reaches `Active`; no frames | integration |
| Stolen/leaked ticket, reused | `ReplayDetected` (consumed set) | `ras-bootstrap` |
| Stale-generation ticket (after rotation) | `ReplayDetected` | `ras-bootstrap` |
| Expired ticket / AccessRequest / grant | `RequestExpired` | `ras-grant` |
| Replayed AccessRequest nonce | `ReplayDetected` | `ras-grant` |
| Grant presented from a **different endpoint** | `IdentityMismatch` (sender-constraint) | `ras-grant` + `ras-core` |
| Modified/forged AccessRequest or grant signature | `SignatureInvalid` / `GrantInvalid` | `ras-grant` |
| Cross-host grant (right controller, wrong host) | `IdentityMismatch` | `ras-grant` |
| Unknown capability requested | dropped before grant; never in `granted_capabilities` | `ras-policy` (property) |
| Reduced grant | granted ⊆ requested, **never expands** | `ras-policy` (property) |
| Consent Deny / timeout | `ConsentDenied`, no grant, fail-closed | `ras-core` + app |
| Emergency stop during a valid grant | `Revoked` overrides grant ≤250 ms (Inv 4) | `ras-core` (exists) |
| `insecure-no-auth` in an auth build | **does not compile** (feature-exclusive) | build gate |

---

## 10. Execution sequence

1. `ras-protocol`: bootstrap `ControlMsg` variants + `proto/` (no new `ErrorCode`).
2. `ras-identity`: `KeyStore` (file-backed Tier 0), identities, `trusted_controllers`.
3. `ras-policy`: `recognize` + catalogue + `grantable` (extend existing `intersect`).
4. `ras-bootstrap`: `TicketAuthority` + `NonceCache` + `CASUALRAS…` codec.
5. `ras-grant`: `AccessRequest`/`SessionGrant` types, `validate_*`, `SessionGrantIssuer` +
   `LocalHostGrantIssuer` (PASETO).
6. `ras-core`: extend `SessionAuthContext`; real `GrantSessionValidator`; wire the bootstrap phase +
   consent → issuance; keep `Active` reachable only via `Authorized`.
7. App: extend `LocalConsent` to carry the AccessRequest + one-time PIN + Allow→issue; bootstrap-then-
   session dial; consent-UI fields (§6).
8. Security test matrix (§9); property/fuzz on the validators; `cargo-deny` on any new dep (PASETO
   crate must be MIT/Apache/BSD — verify before adding, Inv 18).

**Exit → M3:** unknown controller cannot receive frames; replayed/expired/stale/cross-endpoint
tokens rejected; host and controller authenticate each other; every §9 row is green.
