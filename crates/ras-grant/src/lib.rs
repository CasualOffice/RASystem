//! Access requests + sender-constrained session grants for Casual RAS (Phase 2, `docs/04 §4/§5`).
//!
//! This crate is the **authorization heart** of the MVP: a controller signs an [`AccessRequest`]
//! (Ed25519, endpoint-bound, short-lived, nonce'd); the host validates it, gets local consent
//! (elsewhere), and mints a **[`SessionGrant`]** — a **PASETO v4.public** token bound to both
//! endpoints (ADR-040 sender-constraint) and carrying `requested ∩ policy ∩ consented` capabilities.
//! A future control-plane issuer replaces only [`LocalHostGrantIssuer`] behind [`SessionGrantIssuer`];
//! the host **validator** and the wire are unchanged (ADR-003/004).
//!
//! Everything wire/token-facing is deterministically encoded and signed through the `ras-identity`
//! `KeyStore`/`verify` seam (no crypto primitive is re-implemented; ADR-065). The validators are
//! pure, no-I/O functions (unit + property tested); the PASETO envelope is verified against the
//! official test vectors (see `paseto`). Every rejection is a stable [`ErrorCode`] with no
//! verification oracle and no secret in the error (Inv 8).

mod paseto;

use async_trait::async_trait;
use bytes::Bytes;

use ras_bootstrap::{NonceCache, UnixMillis};
use ras_identity::{verify, KeyStore};
use ras_policy::{grantable, recognize, CapabilitySet};
use ras_protocol::{ErrorCode, RasError, PROTOCOL_VERSION};

pub use paseto::V4_PUBLIC_HEADER;

/// Grant/request errors reuse the shared taxonomy.
pub type GrantError = RasError;

/// Wire/format versions this crate emits and accepts (bumped only on a breaking layout change).
const REQUEST_VERSION: u8 = 1;
const GRANT_VERSION: u32 = 1;

/// Maximum lifetime of an [`AccessRequest`] (`docs/04 §4`: `expires_at ≤ issued_at + 5 min`).
pub const MAX_REQUEST_TTL_MS: u64 = 5 * 60 * 1000;
/// Tolerated forward clock skew when judging "issued in the future".
pub const CLOCK_SKEW_MS: u64 = 60 * 1000;
/// Bound on the controller's self-declared reason string (DoS; untrusted UI text).
pub const MAX_REASON: usize = 256;
/// Bound on the controller's display name (shared with the bootstrap wire bound).
pub const MAX_DISPLAY_NAME: usize = ras_protocol::MAX_DISPLAY_NAME;
/// Bounds on the requested/granted capability set (DoS on decode).
const MAX_CAPS: usize = 32;
const MAX_CAP_LEN: usize = 64;

const REQUEST_CTX: &[u8] = b"casual-ras/access-request/v1";

/// A CSPRNG-generated 16-byte id (request/session id or nonce). Fails closed if the OS RNG is absent.
pub fn fresh_id() -> Result<[u8; 16], GrantError> {
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id)
        .map_err(|_| RasError::fatal(ErrorCode::Internal, "csprng unavailable"))?;
    Ok(id)
}

// ── Small deterministic-encoding helpers ────────────────────────────────────────────────────────

fn put_u16_str(v: &mut Vec<u8>, s: &str) {
    let len = u16::try_from(s.len()).unwrap_or(u16::MAX);
    v.extend_from_slice(&len.to_be_bytes());
    v.extend_from_slice(&s.as_bytes()[..len as usize]);
}

fn put_caps(v: &mut Vec<u8>, caps: &CapabilitySet) {
    let count = u32::try_from(caps.len()).unwrap_or(u32::MAX);
    v.extend_from_slice(&count.to_be_bytes());
    // BTreeSet iterates in sorted order → deterministic, canonical encoding.
    for c in caps {
        let len = u32::try_from(c.len()).unwrap_or(u32::MAX);
        v.extend_from_slice(&len.to_be_bytes());
        v.extend_from_slice(c.as_bytes());
    }
}

/// A bounds-checked cursor over a byte buffer; every read fails closed on truncation.
struct Cur<'a> {
    b: &'a [u8],
    c: usize,
}
impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, c: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ErrorCode> {
        let end = self.c.checked_add(n).ok_or(ErrorCode::InvalidMessage)?;
        if end > self.b.len() {
            return Err(ErrorCode::InvalidMessage);
        }
        let r = &self.b[self.c..end];
        self.c = end;
        Ok(r)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], ErrorCode> {
        self.take(N)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidMessage)
    }
    fn u8(&mut self) -> Result<u8, ErrorCode> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ErrorCode> {
        Ok(u16::from_be_bytes(self.arr()?))
    }
    fn u32(&mut self) -> Result<u32, ErrorCode> {
        Ok(u32::from_be_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Result<u64, ErrorCode> {
        Ok(u64::from_be_bytes(self.arr()?))
    }
    fn str16(&mut self, max: usize) -> Result<String, ErrorCode> {
        let len = self.u16()? as usize;
        if len > max {
            return Err(ErrorCode::InvalidMessage);
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ErrorCode::InvalidMessage)
    }
    fn caps(&mut self) -> Result<CapabilitySet, ErrorCode> {
        let count = self.u32()? as usize;
        if count > MAX_CAPS {
            return Err(ErrorCode::InvalidMessage);
        }
        let mut set = CapabilitySet::new();
        for _ in 0..count {
            let len = self.u32()? as usize;
            if len > MAX_CAP_LEN {
                return Err(ErrorCode::InvalidMessage);
            }
            let s = String::from_utf8(self.take(len)?.to_vec())
                .map_err(|_| ErrorCode::InvalidMessage)?;
            set.insert(s);
        }
        Ok(set)
    }
    fn end(self) -> Result<(), ErrorCode> {
        if self.c == self.b.len() {
            Ok(())
        } else {
            Err(ErrorCode::InvalidMessage) // trailing garbage
        }
    }
}

// ── AccessRequest (docs/04 §4) ──────────────────────────────────────────────────────────────────

/// A controller's signed request for access (`docs/04 §4`). Endpoint-bound (Inv 3/9), short-lived,
/// nonce'd. The signature is the controller's Ed25519 over `REQUEST_CTX || body`; it proves
/// possession of `controller_id`'s private key. Not a credential on its own — the host still requires
/// local consent (Inv 1).
#[derive(Clone, PartialEq, Eq)]
pub struct AccessRequest {
    /// Unique request id (echoed into the grant).
    pub request_id: [u8; 16],
    /// Controller's protocol major version.
    pub protocol_version: u32,
    /// The exact host being asked (its public identity).
    pub host_id: [u8; 32],
    /// The requesting controller's public identity.
    pub controller_id: [u8; 32],
    /// Untrusted, bounded UI text shown in the consent prompt.
    pub controller_display_name: String,
    /// The controller's iroh endpoint — must equal the connection the host authenticated (Inv 3/9).
    pub controller_endpoint_id: [u8; 32],
    /// Requested capabilities (recognized/intersected later; never trusted as-granted).
    pub requested_capabilities: CapabilitySet,
    /// Untrusted, bounded reason shown to the local user.
    pub reason: String,
    /// When the controller issued this (host wall clock, ms).
    pub issued_at: UnixMillis,
    /// Absolute expiry; validated `≤ issued_at + MAX_REQUEST_TTL_MS`.
    pub expires_at: UnixMillis,
    /// One-time replay nonce (host nonce cache).
    pub nonce: [u8; 16],
    /// Controller Ed25519 signature over `REQUEST_CTX || body`.
    pub signature: [u8; 64],
}

impl core::fmt::Debug for AccessRequest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Content-free: no signature/nonce bytes, no free-text reason (hygiene).
        f.debug_struct("AccessRequest")
            .field("protocol_version", &self.protocol_version)
            .field("requested_capabilities", &self.requested_capabilities)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl AccessRequest {
    /// Append the signed body (every field except the signature), the canonical wire layout.
    fn encode_body(&self, v: &mut Vec<u8>) {
        v.push(REQUEST_VERSION);
        v.extend_from_slice(&self.request_id);
        v.extend_from_slice(&self.protocol_version.to_be_bytes());
        v.extend_from_slice(&self.host_id);
        v.extend_from_slice(&self.controller_id);
        put_u16_str(v, &self.controller_display_name);
        v.extend_from_slice(&self.controller_endpoint_id);
        put_u16_str(v, &self.reason);
        v.extend_from_slice(&self.issued_at.to_be_bytes());
        v.extend_from_slice(&self.expires_at.to_be_bytes());
        v.extend_from_slice(&self.nonce);
        put_caps(v, &self.requested_capabilities);
    }

    /// Bytes covered by the controller signature: `REQUEST_CTX || body`.
    fn signing_input(&self) -> Vec<u8> {
        let mut v = Vec::from(REQUEST_CTX);
        self.encode_body(&mut v);
        v
    }

    /// Build and **sign** a request with `keystore` (the controller identity). `controller_id` is set
    /// to the keystore's public key so the signature is self-consistent.
    #[allow(clippy::too_many_arguments)] // a faithful docs/04 §4 record; a builder is churn here
    pub fn signed<K: KeyStore>(
        keystore: &K,
        request_id: [u8; 16],
        protocol_version: u32,
        host_id: [u8; 32],
        controller_display_name: String,
        controller_endpoint_id: [u8; 32],
        requested_capabilities: CapabilitySet,
        reason: String,
        issued_at: UnixMillis,
        expires_at: UnixMillis,
        nonce: [u8; 16],
    ) -> Result<Self, GrantError> {
        let mut req = Self {
            request_id,
            protocol_version,
            host_id,
            controller_id: keystore.public_key(),
            controller_display_name,
            controller_endpoint_id,
            requested_capabilities,
            reason,
            issued_at,
            expires_at,
            nonce,
            signature: [0u8; 64],
        };
        req.signature = keystore.sign(&req.signing_input())?;
        Ok(req)
    }

    /// The canonical wire encoding `body || signature` — the opaque bytes carried in
    /// `ras_protocol::BootstrapMsg::AccessRequest`.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut v = Vec::new();
        self.encode_body(&mut v);
        v.extend_from_slice(&self.signature);
        Bytes::from(v)
    }

    /// Decode from the wire bytes. **Fail-closed**: malformed/oversized/truncated → a stable error.
    /// Does **not** verify the signature; that is [`validate_access_request`]'s job.
    pub fn decode(bytes: &[u8]) -> Result<Self, ErrorCode> {
        let mut c = Cur::new(bytes);
        if c.u8()? != REQUEST_VERSION {
            return Err(ErrorCode::UnsupportedVersion);
        }
        let request_id = c.arr::<16>()?;
        let protocol_version = c.u32()?;
        let host_id = c.arr::<32>()?;
        let controller_id = c.arr::<32>()?;
        let controller_display_name = c.str16(MAX_DISPLAY_NAME)?;
        let controller_endpoint_id = c.arr::<32>()?;
        let reason = c.str16(MAX_REASON)?;
        let issued_at = c.u64()?;
        let expires_at = c.u64()?;
        let nonce = c.arr::<16>()?;
        let requested_capabilities = c.caps()?;
        let signature = c.arr::<64>()?;
        c.end()?;
        Ok(Self {
            request_id,
            protocol_version,
            host_id,
            controller_id,
            controller_display_name,
            controller_endpoint_id,
            requested_capabilities,
            reason,
            issued_at,
            expires_at,
            nonce,
            signature,
        })
    }
}

/// Validate a decoded [`AccessRequest`] host-side. Ordered, fail-closed checks (`docs/04 §4`,
/// design §5); each maps to a stable [`ErrorCode`] with no oracle:
///
/// 1. `protocol_version` supported → else [`ErrorCode::UnsupportedVersion`].
/// 2. controller signature verifies over `REQUEST_CTX || body` → else [`ErrorCode::SignatureInvalid`]
///    (authenticity gate — every field below is only trusted once this passes).
/// 3. `host_id` == this host → else [`ErrorCode::IdentityMismatch`].
/// 4. `controller_endpoint_id` == the authenticated peer endpoint → else [`ErrorCode::IdentityMismatch`]
///    (sender-constraint, Inv 3/9 — the transport-authenticated identity, not a claim).
/// 5. freshness: not expired, TTL `≤ 5 min`, not issued in the future → else [`ErrorCode::RequestExpired`].
/// 6. nonce unseen → else [`ErrorCode::ReplayDetected`] (only authentic requests touch the cache).
/// 7. at least one requested capability is recognized → else [`ErrorCode::CapabilityDenied`].
pub fn validate_access_request(
    req: &AccessRequest,
    host_id: &[u8; 32],
    peer_endpoint: &[u8; 32],
    now: UnixMillis,
    nonces: &mut NonceCache,
) -> Result<(), ErrorCode> {
    if req.protocol_version != PROTOCOL_VERSION {
        return Err(ErrorCode::UnsupportedVersion);
    }
    verify(&req.controller_id, &req.signing_input(), &req.signature)
        .map_err(|_| ErrorCode::SignatureInvalid)?;
    if &req.host_id != host_id {
        return Err(ErrorCode::IdentityMismatch);
    }
    if &req.controller_endpoint_id != peer_endpoint {
        return Err(ErrorCode::IdentityMismatch);
    }
    if now > req.expires_at
        || req.expires_at < req.issued_at
        || req.expires_at.saturating_sub(req.issued_at) > MAX_REQUEST_TTL_MS
        || req.issued_at > now.saturating_add(CLOCK_SKEW_MS)
    {
        return Err(ErrorCode::RequestExpired);
    }
    nonces.check_and_insert(req.nonce, now)?;
    if recognize(&req.requested_capabilities).is_empty() {
        return Err(ErrorCode::CapabilityDenied);
    }
    Ok(())
}

// ── SessionGrant (docs/04 §5) ───────────────────────────────────────────────────────────────────

/// Who minted a grant. The MVP host mints `LocalHost`; a control plane later mints `ControlPlane`
/// behind the same [`SessionGrantIssuer`] seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IssuerType {
    /// The host itself (MVP).
    LocalHost,
    /// A future control-plane issuer.
    ControlPlane,
}
impl IssuerType {
    const fn tag(self) -> u8 {
        match self {
            IssuerType::LocalHost => 0,
            IssuerType::ControlPlane => 1,
        }
    }
    const fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(IssuerType::LocalHost),
            1 => Some(IssuerType::ControlPlane),
            _ => None,
        }
    }
}

/// A short-lived, **sender-constrained** authorization (`docs/04 §5`). Bound to both endpoints
/// (ADR-040): a grant presented from any endpoint other than `controller_endpoint_id` is rejected,
/// so a stolen grant is useless without the controller's authenticated iroh connection. Carries the
/// immutable `granted_capabilities` (= `requested ∩ policy ∩ consented`); reduction is a re-issue at
/// a lower `session_generation`, never in-grant mutation.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionGrant {
    /// Grant format version.
    pub grant_version: u32,
    /// This session's id.
    pub session_id: [u8; 16],
    /// The originating request id.
    pub request_id: [u8; 16],
    /// The issuer's public identity (== the PASETO verifying key).
    pub issuer_id: [u8; 32],
    /// Which kind of issuer minted it.
    pub issuer_type: IssuerType,
    /// The host this grant is for.
    pub host_id: [u8; 32],
    /// The controller this grant authorizes.
    pub controller_id: [u8; 32],
    /// The host's iroh endpoint (bound, Inv 3).
    pub host_endpoint_id: [u8; 32],
    /// The controller's iroh endpoint (sender-constraint, ADR-040).
    pub controller_endpoint_id: [u8; 32],
    /// The immutable granted capability set.
    pub granted_capabilities: CapabilitySet,
    /// The policy version the grant was minted under.
    pub policy_version: u32,
    /// Session generation (reduction = re-issue at a lower generation).
    pub session_generation: u32,
    /// One-time session nonce.
    pub session_nonce: [u8; 16],
    /// When minted (ms).
    pub issued_at: UnixMillis,
    /// Not valid before (ms).
    pub not_before: UnixMillis,
    /// Absolute expiry — SHORT (ms).
    pub expires_at: UnixMillis,
}

impl core::fmt::Debug for SessionGrant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionGrant")
            .field("grant_version", &self.grant_version)
            .field("issuer_type", &self.issuer_type)
            .field("granted_capabilities", &self.granted_capabilities)
            .field("session_generation", &self.session_generation)
            .field("not_before", &self.not_before)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl SessionGrant {
    /// The deterministic claims blob used as the PASETO message `m`.
    fn encode_claims(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&self.grant_version.to_be_bytes());
        v.extend_from_slice(&self.session_id);
        v.extend_from_slice(&self.request_id);
        v.extend_from_slice(&self.issuer_id);
        v.push(self.issuer_type.tag());
        v.extend_from_slice(&self.host_id);
        v.extend_from_slice(&self.controller_id);
        v.extend_from_slice(&self.host_endpoint_id);
        v.extend_from_slice(&self.controller_endpoint_id);
        v.extend_from_slice(&self.policy_version.to_be_bytes());
        v.extend_from_slice(&self.session_generation.to_be_bytes());
        v.extend_from_slice(&self.session_nonce);
        v.extend_from_slice(&self.issued_at.to_be_bytes());
        v.extend_from_slice(&self.not_before.to_be_bytes());
        v.extend_from_slice(&self.expires_at.to_be_bytes());
        put_caps(&mut v, &self.granted_capabilities);
        v
    }

    fn decode_claims(m: &[u8]) -> Result<Self, ErrorCode> {
        let mut c = Cur::new(m);
        let grant_version = c.u32()?;
        let session_id = c.arr::<16>()?;
        let request_id = c.arr::<16>()?;
        let issuer_id = c.arr::<32>()?;
        let issuer_type = IssuerType::from_tag(c.u8()?).ok_or(ErrorCode::InvalidMessage)?;
        let host_id = c.arr::<32>()?;
        let controller_id = c.arr::<32>()?;
        let host_endpoint_id = c.arr::<32>()?;
        let controller_endpoint_id = c.arr::<32>()?;
        let policy_version = c.u32()?;
        let session_generation = c.u32()?;
        let session_nonce = c.arr::<16>()?;
        let issued_at = c.u64()?;
        let not_before = c.u64()?;
        let expires_at = c.u64()?;
        let granted_capabilities = c.caps()?;
        c.end()?;
        Ok(Self {
            grant_version,
            session_id,
            request_id,
            issuer_id,
            issuer_type,
            host_id,
            controller_id,
            host_endpoint_id,
            controller_endpoint_id,
            granted_capabilities,
            policy_version,
            session_generation,
            session_nonce,
            issued_at,
            not_before,
            expires_at,
        })
    }
}

/// Parameters the issuer stamps onto a grant that are not carried by the request.
#[derive(Debug, Clone)]
pub struct SessionParams {
    /// The new session id.
    pub session_id: [u8; 16],
    /// The host's iroh endpoint id.
    pub host_endpoint_id: [u8; 32],
    /// The session generation (initial issue: pick a starting value; reductions go lower).
    pub session_generation: u32,
    /// One-time session nonce.
    pub session_nonce: [u8; 16],
    /// When minted (ms).
    pub issued_at: UnixMillis,
    /// Not valid before (ms).
    pub not_before: UnixMillis,
    /// Absolute expiry — keep SHORT (ms).
    pub expires_at: UnixMillis,
}

/// The issuer seam (ADR-003/004). The MVP host validates **and** issues; a future control plane
/// replaces only this impl. Async because a networked issuer awaits I/O — [`LocalHostGrantIssuer`]
/// resolves immediately (pure CPU).
#[async_trait]
pub trait SessionGrantIssuer: Send + Sync {
    /// Mint a PASETO v4.public grant for `req`, granting `requested ∩ policy ∩ consented`.
    async fn issue(
        &self,
        req: &AccessRequest,
        consented: &CapabilitySet,
        session: &SessionParams,
    ) -> Result<Bytes, GrantError>;
}

/// The MVP host-side issuer: mints + PASETO-signs grants with the host key. Holds the host default
/// policy and its version; the granted set is always `recognize(requested) ∩ policy ∩ consented`.
pub struct LocalHostGrantIssuer<K: KeyStore> {
    keystore: K,
    host_id: [u8; 32],
    policy: CapabilitySet,
    policy_version: u32,
}

impl<K: KeyStore> LocalHostGrantIssuer<K> {
    /// Build an issuer over the host `keystore`, its grantable `policy`, and the `policy_version`.
    pub fn new(keystore: K, policy: CapabilitySet, policy_version: u32) -> Self {
        let host_id = keystore.public_key();
        Self {
            keystore,
            host_id,
            policy,
            policy_version,
        }
    }

    /// The host identity these grants are issued by/for.
    #[must_use]
    pub fn host_id(&self) -> [u8; 32] {
        self.host_id
    }
}

#[async_trait]
impl<K: KeyStore> SessionGrantIssuer for LocalHostGrantIssuer<K> {
    async fn issue(
        &self,
        req: &AccessRequest,
        consented: &CapabilitySet,
        session: &SessionParams,
    ) -> Result<Bytes, GrantError> {
        // requested ∩ policy ∩ consented — unknown-denied, never widened (ras-policy).
        let granted = grantable(&req.requested_capabilities, &self.policy, consented);
        if granted.is_empty() {
            return Err(RasError::fatal(
                ErrorCode::CapabilityDenied,
                "no capability granted",
            ));
        }
        let grant = SessionGrant {
            grant_version: GRANT_VERSION,
            session_id: session.session_id,
            request_id: req.request_id,
            issuer_id: self.host_id,
            issuer_type: IssuerType::LocalHost,
            host_id: self.host_id,
            controller_id: req.controller_id,
            host_endpoint_id: session.host_endpoint_id,
            controller_endpoint_id: req.controller_endpoint_id,
            granted_capabilities: granted,
            policy_version: self.policy_version,
            session_generation: session.session_generation,
            session_nonce: session.session_nonce,
            issued_at: session.issued_at,
            not_before: session.not_before,
            expires_at: session.expires_at,
        };
        let token =
            paseto::v4_public_sign(|m| self.keystore.sign(m), &grant.encode_claims(), b"", b"")?;
        Ok(Bytes::from(token.into_bytes()))
    }
}

/// Validate a PASETO grant token host-side (design §5). `host_verifying_key` is the issuer's public
/// key (== `host_id` in the MVP where issuer == host; separate for the issuer-agnostic seam).
/// Ordered, fail-closed checks:
///
/// 1. token is UTF-8 and the PASETO v4.public signature verifies under `host_verifying_key` → else
///    [`ErrorCode::GrantInvalid`] (uniform — no signature/parse oracle).
/// 2. `grant_version` supported → else [`ErrorCode::UnsupportedVersion`].
/// 3. `issuer_id` == the verifying key and `host_id` == this host → else [`ErrorCode::IdentityMismatch`].
/// 4. `controller_endpoint_id` == the authenticated peer endpoint → else [`ErrorCode::IdentityMismatch`]
///    (sender-constraint, ADR-040 — a stolen grant from another endpoint is rejected).
/// 5. time: `not_before ≤ now ≤ expires_at` → else [`ErrorCode::GrantInvalid`].
pub fn validate_grant(
    token: &[u8],
    host_id: &[u8; 32],
    host_verifying_key: &[u8; 32],
    peer_endpoint: &[u8; 32],
    now: UnixMillis,
) -> Result<SessionGrant, ErrorCode> {
    let token = core::str::from_utf8(token).map_err(|_| ErrorCode::GrantInvalid)?;
    let m = paseto::v4_public_verify(host_verifying_key, token, b"", b"")
        .map_err(|_| ErrorCode::GrantInvalid)?;
    let grant = SessionGrant::decode_claims(&m).map_err(|_| ErrorCode::GrantInvalid)?;

    if grant.grant_version != GRANT_VERSION {
        return Err(ErrorCode::UnsupportedVersion);
    }
    if &grant.issuer_id != host_verifying_key || &grant.host_id != host_id {
        return Err(ErrorCode::IdentityMismatch);
    }
    if &grant.controller_endpoint_id != peer_endpoint {
        return Err(ErrorCode::IdentityMismatch);
    }
    if now < grant.not_before || now > grant.expires_at {
        return Err(ErrorCode::GrantInvalid);
    }
    Ok(grant)
}

#[cfg(test)]
mod tests;
