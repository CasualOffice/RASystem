//! Unit + property tests for `ras-grant` — the authorization heart, so the security matrix
//! (design §9) lives here: tampered / endpoint-mismatched / expired / replayed / capability cases
//! all fail closed, and the decoders never panic on hostile bytes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use proptest::prelude::*;
use ras_bootstrap::NonceCache;
use ras_identity::SoftwareKeyStore;
use ras_policy::phase2_default_policy;

const HOST_EP: [u8; 32] = [0x11; 32];
const CTRL_EP: [u8; 32] = [0x22; 32];
const NOW: UnixMillis = 1_000_000;

fn caps(items: &[&str]) -> CapabilitySet {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// A host id, a controller keystore, and a fresh nonce cache. (These tests exercise the *validators*,
/// which take the host id + verifying key directly; grant *issuance* uses `issue_for` below.)
struct Fixture {
    host_id: [u8; 32],
    controller: SoftwareKeyStore,
    nonces: NonceCache,
}
impl Fixture {
    fn new() -> Self {
        let host_id = SoftwareKeyStore::generate().unwrap().public_key();
        Self {
            host_id,
            controller: SoftwareKeyStore::generate().unwrap(),
            nonces: NonceCache::new(MAX_REQUEST_TTL_MS, 4096),
        }
    }

    /// A well-formed, signed request from the controller for `req_caps`.
    fn request(&self, req_caps: &[&str]) -> AccessRequest {
        AccessRequest::signed(
            &self.controller,
            [1u8; 16],
            PROTOCOL_VERSION,
            self.host_id,
            "Tech Support".to_string(),
            CTRL_EP,
            caps(req_caps),
            "help with printer".to_string(),
            NOW,
            NOW + MAX_REQUEST_TTL_MS,
            [9u8; 16],
        )
        .unwrap()
    }
}

// ── AccessRequest: encode/decode + validation matrix ────────────────────────────────────────────

#[test]
fn access_request_round_trips() {
    let fx = Fixture::new();
    let req = fx.request(&["screen.view", "pointer.virtual"]);
    let decoded = AccessRequest::decode(&req.encode()).unwrap();
    assert_eq!(decoded, req);
}

#[test]
fn valid_request_passes() {
    let mut fx = Fixture::new();
    let req = fx.request(&["screen.view"]);
    assert!(validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces).is_ok());
}

#[test]
fn tampered_request_fails_signature() {
    let mut fx = Fixture::new();
    let mut req = fx.request(&["screen.view"]);
    req.reason.push('!'); // change a signed field without re-signing
    assert_eq!(
        validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces),
        Err(ErrorCode::SignatureInvalid)
    );
}

#[test]
fn wrong_host_is_rejected() {
    let mut fx = Fixture::new();
    let req = fx.request(&["screen.view"]);
    let other_host = [0xEE; 32];
    assert_eq!(
        validate_access_request(&req, &other_host, &CTRL_EP, NOW, &mut fx.nonces),
        Err(ErrorCode::IdentityMismatch)
    );
}

#[test]
fn endpoint_mismatch_is_rejected() {
    let mut fx = Fixture::new();
    let req = fx.request(&["screen.view"]);
    // The controller connected from a DIFFERENT endpoint than it claims → sender-constraint fails.
    let actual_peer = [0x33; 32];
    assert_eq!(
        validate_access_request(&req, &fx.host_id, &actual_peer, NOW, &mut fx.nonces),
        Err(ErrorCode::IdentityMismatch)
    );
}

#[test]
fn expired_request_is_rejected() {
    let mut fx = Fixture::new();
    let req = fx.request(&["screen.view"]); // expires at NOW + 5min
    let later = NOW + MAX_REQUEST_TTL_MS + 1;
    assert_eq!(
        validate_access_request(&req, &fx.host_id, &CTRL_EP, later, &mut fx.nonces),
        Err(ErrorCode::RequestExpired)
    );
}

#[test]
fn over_long_ttl_is_rejected() {
    let mut fx = Fixture::new();
    let req = AccessRequest::signed(
        &fx.controller,
        [1u8; 16],
        PROTOCOL_VERSION,
        fx.host_id,
        "n".to_string(),
        CTRL_EP,
        caps(&["screen.view"]),
        "r".to_string(),
        NOW,
        NOW + MAX_REQUEST_TTL_MS + 1, // TTL > 5 min
        [9u8; 16],
    )
    .unwrap();
    assert_eq!(
        validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces),
        Err(ErrorCode::RequestExpired)
    );
}

#[test]
fn future_dated_request_is_rejected() {
    let mut fx = Fixture::new();
    let future = NOW + CLOCK_SKEW_MS + 1;
    let req = AccessRequest::signed(
        &fx.controller,
        [1u8; 16],
        PROTOCOL_VERSION,
        fx.host_id,
        "n".to_string(),
        CTRL_EP,
        caps(&["screen.view"]),
        "r".to_string(),
        future,
        future + 1000,
        [9u8; 16],
    )
    .unwrap();
    assert_eq!(
        validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces),
        Err(ErrorCode::RequestExpired)
    );
}

#[test]
fn replayed_nonce_is_rejected() {
    let mut fx = Fixture::new();
    let req = fx.request(&["screen.view"]);
    assert!(validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces).is_ok());
    // Same request (same nonce) presented again → replay.
    assert_eq!(
        validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces),
        Err(ErrorCode::ReplayDetected)
    );
}

#[test]
fn a_bad_signature_does_not_consume_the_nonce() {
    // An unauthenticated attacker must not be able to poison the nonce cache: the nonce is only
    // recorded AFTER the signature verifies.
    let mut fx = Fixture::new();
    let mut forged = fx.request(&["screen.view"]);
    forged.signature[0] ^= 0xff;
    assert_eq!(
        validate_access_request(&forged, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces),
        Err(ErrorCode::SignatureInvalid)
    );
    assert!(
        fx.nonces.is_empty(),
        "forged request must not touch the cache"
    );
    // The genuine request with that same nonce still goes through.
    let genuine = fx.request(&["screen.view"]);
    assert!(validate_access_request(&genuine, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces).is_ok());
}

#[test]
fn only_unknown_capabilities_is_denied() {
    let mut fx = Fixture::new();
    let req = fx.request(&["totally.unknown", "made.up"]);
    assert_eq!(
        validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces),
        Err(ErrorCode::CapabilityDenied)
    );
}

// ── SessionGrant: issuance + validation matrix ──────────────────────────────────────────────────

/// Issue a grant with the fixture's own host keystore (so the validating key matches), returning
/// (issuer_keystore_pubkey, token). Uses a request bound to that host id.
async fn issue_for(
    req_caps: &[&str],
    consented: &[&str],
    session: SessionParams,
) -> (SoftwareKeyStore, [u8; 32], AccessRequest, Bytes) {
    let host = SoftwareKeyStore::from_seed([42u8; 32]);
    let host_id = host.public_key();
    let controller = SoftwareKeyStore::generate().unwrap();
    let req = AccessRequest::signed(
        &controller,
        [1u8; 16],
        PROTOCOL_VERSION,
        host_id,
        "n".to_string(),
        CTRL_EP,
        caps(req_caps),
        "r".to_string(),
        NOW,
        NOW + MAX_REQUEST_TTL_MS,
        [9u8; 16],
    )
    .unwrap();
    let issuer = LocalHostGrantIssuer::new(
        SoftwareKeyStore::from_seed([42u8; 32]),
        phase2_default_policy(),
        1,
    );
    let token = issuer
        .issue(&req, &caps(consented), &session)
        .await
        .unwrap();
    (host, host_id, req, token)
}

fn session_at(gen: u32, not_before: UnixMillis, expires_at: UnixMillis) -> SessionParams {
    SessionParams {
        session_id: [5u8; 16],
        host_endpoint_id: HOST_EP,
        session_generation: gen,
        session_nonce: [6u8; 16],
        issued_at: NOW,
        not_before,
        expires_at,
    }
}

#[tokio::test]
async fn issue_then_validate_succeeds_and_intersects_caps() {
    let session = session_at(1, NOW, NOW + 60_000);
    // Requests view+pointer+keyboard; policy is view-only+pointer+annotation; consents to view+pointer.
    let (_h, host_id, _req, token) = issue_for(
        &["screen.view", "pointer.virtual", "keyboard.key"],
        &["screen.view", "pointer.virtual"],
        session,
    )
    .await;
    let grant = validate_grant(&token, &host_id, &host_id, &CTRL_EP, NOW + 1_000).unwrap();
    // keyboard.key requested + not consented + not in policy → dropped. Exactly view+pointer granted.
    assert_eq!(
        grant.granted_capabilities,
        caps(&["screen.view", "pointer.virtual"])
    );
    assert_eq!(grant.controller_endpoint_id, CTRL_EP);
    assert_eq!(grant.host_endpoint_id, HOST_EP);
}

#[tokio::test]
async fn grant_from_another_endpoint_is_rejected() {
    let session = session_at(1, NOW, NOW + 60_000);
    let (_h, host_id, _req, token) = issue_for(&["screen.view"], &["screen.view"], session).await;
    // A thief presents the grant from a different (authenticated) endpoint → sender-constraint fails.
    let thief_ep = [0xAB; 32];
    assert_eq!(
        validate_grant(&token, &host_id, &host_id, &thief_ep, NOW + 1_000),
        Err(ErrorCode::IdentityMismatch)
    );
}

#[tokio::test]
async fn grant_verified_with_wrong_key_is_rejected() {
    let session = session_at(1, NOW, NOW + 60_000);
    let (_h, host_id, _req, token) = issue_for(&["screen.view"], &["screen.view"], session).await;
    let wrong_key = [0xCD; 32];
    assert_eq!(
        validate_grant(&token, &host_id, &wrong_key, &CTRL_EP, NOW + 1_000),
        Err(ErrorCode::GrantInvalid)
    );
}

#[tokio::test]
async fn grant_for_wrong_host_is_rejected() {
    let session = session_at(1, NOW, NOW + 60_000);
    let (_h, host_id, _req, token) = issue_for(&["screen.view"], &["screen.view"], session).await;
    let other_host = [0xEF; 32];
    // Signature still verifies (right key), but host_id claim != this host.
    assert_eq!(
        validate_grant(&token, &other_host, &host_id, &CTRL_EP, NOW + 1_000),
        Err(ErrorCode::IdentityMismatch)
    );
}

#[tokio::test]
async fn expired_and_not_yet_valid_grants_are_rejected() {
    let (_h, host_id, _req, token) = issue_for(
        &["screen.view"],
        &["screen.view"],
        session_at(1, NOW + 10_000, NOW + 60_000),
    )
    .await;
    // Before not_before.
    assert_eq!(
        validate_grant(&token, &host_id, &host_id, &CTRL_EP, NOW),
        Err(ErrorCode::GrantInvalid)
    );
    // After expiry.
    assert_eq!(
        validate_grant(&token, &host_id, &host_id, &CTRL_EP, NOW + 60_001),
        Err(ErrorCode::GrantInvalid)
    );
}

#[tokio::test]
async fn tampered_grant_token_is_rejected() {
    let (_h, host_id, _req, token) = issue_for(
        &["screen.view"],
        &["screen.view"],
        session_at(1, NOW, NOW + 60_000),
    )
    .await;
    let mut bytes = token.to_vec();
    // Flip a byte in the base64 payload region (past the "v4.public." header).
    bytes[15] ^= 0x01;
    assert_eq!(
        validate_grant(&bytes, &host_id, &host_id, &CTRL_EP, NOW + 1_000),
        Err(ErrorCode::GrantInvalid)
    );
}

#[tokio::test]
async fn issuing_with_no_grantable_capability_errors() {
    // Requests only a capability the policy withholds in Phase 2 (keyboard) → nothing to grant.
    let host = SoftwareKeyStore::from_seed([42u8; 32]);
    let host_id = host.public_key();
    let controller = SoftwareKeyStore::generate().unwrap();
    let req = AccessRequest::signed(
        &controller,
        [1u8; 16],
        PROTOCOL_VERSION,
        host_id,
        "n".to_string(),
        CTRL_EP,
        caps(&["keyboard.key"]),
        "r".to_string(),
        NOW,
        NOW + MAX_REQUEST_TTL_MS,
        [9u8; 16],
    )
    .unwrap();
    let issuer = LocalHostGrantIssuer::new(
        SoftwareKeyStore::from_seed([42u8; 32]),
        phase2_default_policy(),
        1,
    );
    let err = issuer
        .issue(
            &req,
            &caps(&["keyboard.key"]),
            &session_at(1, NOW, NOW + 60_000),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CapabilityDenied);
}

// ── Property / fuzz: decoders never panic on hostile bytes ───────────────────────────────────────

proptest! {
    #[test]
    fn access_request_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let _ = AccessRequest::decode(&bytes);
    }

    #[test]
    fn validate_grant_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let host = [0u8; 32];
        let _ = validate_grant(&bytes, &host, &host, &CTRL_EP, NOW);
    }

    /// Any well-formed request round-trips through encode/decode byte-for-byte.
    #[test]
    fn request_round_trip_is_identity(
        display in "[ -~]{0,64}",
        reason in "[ -~]{0,128}",
        issued in any::<u64>(),
    ) {
        let ks = SoftwareKeyStore::generate().unwrap();
        let req = AccessRequest::signed(
            &ks, [3u8;16], PROTOCOL_VERSION, [4u8;32], display, [5u8;32],
            caps(&["screen.view", "pointer.virtual"]), reason, issued, issued.saturating_add(1000), [6u8;16],
        ).unwrap();
        let decoded = AccessRequest::decode(&req.encode()).unwrap();
        prop_assert_eq!(decoded, req);
    }
}
