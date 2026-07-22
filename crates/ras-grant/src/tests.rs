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

/// A request accepted with a **future-dated** `issued_at` (the max the clock-skew tolerance allows)
/// stays fresh until `issued_at + MAX_REQUEST_TTL_MS`, which is `CLOCK_SKEW_MS` beyond `now + cache_ttl`.
/// The nonce must be remembered for that FULL horizon — otherwise the identical signed bytes, replayed
/// after the old `now + ttl` retention but before the request's own expiry, would be accepted twice
/// (Inv 3 replay defense; found by the authorization-core adversarial review).
#[test]
fn a_future_dated_request_cannot_be_replayed_after_the_cache_ttl() {
    let mut fx = Fixture::new();
    let issued = NOW + CLOCK_SKEW_MS; // maximal permitted forward skew
    let req = AccessRequest::signed(
        &fx.controller,
        [1u8; 16],
        PROTOCOL_VERSION,
        fx.host_id,
        "n".to_string(),
        CTRL_EP,
        caps(&["screen.view"]),
        "r".to_string(),
        issued,
        issued + MAX_REQUEST_TTL_MS, // = NOW + CLOCK_SKEW_MS + MAX_REQUEST_TTL_MS
        [9u8; 16],
    )
    .unwrap();
    // Accepted now.
    assert!(validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx.nonces).is_ok());

    // Replay past the OLD `now + cache_ttl` (NOW + MAX_REQUEST_TTL_MS) but still within the request's own
    // freshness window: the request is still valid (expires at NOW + CLOCK_SKEW_MS + MAX_REQUEST_TTL_MS),
    // so ONLY the nonce cache can stop the replay — and it must.
    let replay_at = NOW + MAX_REQUEST_TTL_MS + 1;
    assert_eq!(
        validate_access_request(&req, &fx.host_id, &CTRL_EP, replay_at, &mut fx.nonces),
        Err(ErrorCode::ReplayDetected),
        "a future-dated request's nonce must be remembered until its own expiry, not merely now+ttl"
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

// ── Codec negotiation (media capability, NOT authorization — Inv 9) ───────────────────────────────

const MAC: HostEncodeCaps = HostEncodeCaps {
    h264: true,
    vp9: true,
    vp8: true,
};
const LINUX: HostEncodeCaps = HostEncodeCaps {
    h264: false,
    vp9: true,
    vp8: true,
};
const WINDOWS: HostEncodeCaps = HostEncodeCaps {
    h264: true,
    vp9: false,
    vp8: false,
};

#[test]
fn codec_prefs_round_trip_and_are_signature_covered() {
    let fx = Fixture::new();
    // H.264 first, VP9 fallback.
    let prefs = vec![VideoDecodeCodec::H264.tag(), VideoDecodeCodec::Vp9.tag()];
    let req = AccessRequest::signed_with_codecs(
        &fx.controller,
        [1u8; 16],
        PROTOCOL_VERSION,
        fx.host_id,
        "Viewer".into(),
        CTRL_EP,
        caps(&["screen.view"]),
        "help".into(),
        NOW,
        NOW + 1000,
        [9u8; 16],
        prefs.clone(),
    )
    .unwrap();
    let decoded = AccessRequest::decode(&req.encode()).unwrap();
    assert_eq!(decoded.viewer_codec_prefs, prefs);
    assert_eq!(decoded, req);
    // The prefs are part of the signed body: tampering with them breaks the signature.
    let mut fx2 = Fixture::new();
    assert!(validate_access_request(&req, &fx.host_id, &CTRL_EP, NOW, &mut fx2.nonces).is_ok());
    let mut bad = req.clone();
    bad.viewer_codec_prefs = vec![VideoDecodeCodec::Vp8.tag()];
    assert_eq!(
        validate_access_request(&bad, &fx.host_id, &CTRL_EP, NOW, &mut fx2.nonces),
        Err(ErrorCode::SignatureInvalid),
        "codec prefs are covered by the signature"
    );
}

#[test]
fn signed_with_codecs_drops_unknown_and_bounds_length() {
    let fx = Fixture::new();
    // 99 is unknown; the list is also over-long — both must be normalized before signing.
    let prefs = vec![99, VideoDecodeCodec::Vp9.tag(), 200, 0, 1, 2, 0, 1];
    let req = AccessRequest::signed_with_codecs(
        &fx.controller,
        [1u8; 16],
        PROTOCOL_VERSION,
        fx.host_id,
        "V".into(),
        CTRL_EP,
        caps(&["screen.view"]),
        "r".into(),
        NOW,
        NOW + 1000,
        [9u8; 16],
        prefs,
    )
    .unwrap();
    assert!(
        req.viewer_codec_prefs.len() <= 4,
        "bounded to MAX_VIEWER_CODECS"
    );
    assert!(
        req.viewer_codec_prefs
            .iter()
            .all(|t| VideoDecodeCodec::from_tag(*t).is_some()),
        "only known tags survive"
    );
    // First surviving tag is VP9 (the unknown 99 was dropped, preserving order).
    assert_eq!(
        req.viewer_codec_prefs.first(),
        Some(&VideoDecodeCodec::Vp9.tag())
    );
    // And it still round-trips.
    assert_eq!(AccessRequest::decode(&req.encode()).unwrap(), req);
}

#[test]
fn v1_request_still_decodes_with_empty_prefs() {
    // The default `signed` builder advertises no prefs (v2, count=0) → decodes to empty.
    let fx = Fixture::new();
    let req = fx.request(&["screen.view"]);
    let decoded = AccessRequest::decode(&req.encode()).unwrap();
    assert!(decoded.viewer_codec_prefs.is_empty());
}

#[test]
fn select_prefers_viewer_order_within_host_capability() {
    // macOS can do both: viewer preferring H.264 gets H.264.
    assert_eq!(
        select_encode_codec(
            &[VideoDecodeCodec::H264.tag(), VideoDecodeCodec::Vp9.tag()],
            MAC
        ),
        VideoDecodeCodec::H264
    );
    // Viewer preferring VP9 gets VP9.
    assert_eq!(
        select_encode_codec(
            &[VideoDecodeCodec::Vp9.tag(), VideoDecodeCodec::H264.tag()],
            MAC
        ),
        VideoDecodeCodec::Vp9
    );
}

#[test]
fn select_fails_safe_to_vp9_when_no_pref() {
    // Empty prefs / all-unknown → the host's default (VP9 where encodable): never a black screen.
    assert_eq!(select_encode_codec(&[], MAC), VideoDecodeCodec::Vp9);
    assert_eq!(select_encode_codec(&[], LINUX), VideoDecodeCodec::Vp9);
    assert_eq!(select_encode_codec(&[255, 254], MAC), VideoDecodeCodec::Vp9);
    // Windows (no VP9) defaults to H.264.
    assert_eq!(select_encode_codec(&[], WINDOWS), VideoDecodeCodec::H264);
}

#[test]
fn windows_vp9_only_viewer_falls_through_to_h264_documented_limitation() {
    // A VP9-only Linux viewer on a Windows host: Windows can't encode VP9, so it serves H.264.
    // The Linux viewer then surfaces its honest "can't decode" error — never a silent hang.
    assert_eq!(
        select_encode_codec(&[VideoDecodeCodec::Vp9.tag()], WINDOWS),
        VideoDecodeCodec::H264
    );
}

#[test]
fn linux_h264_only_viewer_falls_through_to_vp9_documented_limitation() {
    // Symmetric: an H.264-only viewer on a Linux host (no H.264 encode) gets VP9.
    assert_eq!(
        select_encode_codec(&[VideoDecodeCodec::H264.tag()], LINUX),
        VideoDecodeCodec::Vp9
    );
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

// ── Unattended access (§3.4, ADR-085) ─────────────────────────────────────────────────────────────

fn authz(caps_set: &[&str], expires_at: UnixMillis) -> UnattendedAuthorization {
    UnattendedAuthorization {
        controller_id: [0x42; 32],
        capabilities: caps(caps_set),
        expires_at,
    }
}

#[test]
fn unattended_proceeds_only_when_tier_paired_authorized_and_unexpired() {
    let auth = authz(&["screen.view", "pointer.virtual"], NOW + 60_000);
    // The one path that skips the live prompt: attested tier, paired, authorized, not expired.
    assert_eq!(
        unattended_decision(true, AssuranceTier::Tier1, Some(&auth), NOW),
        UnattendedDecision::Proceed
    );
    // A higher tier is still fine.
    assert_eq!(
        unattended_decision(true, AssuranceTier::Tier2, Some(&auth), NOW),
        UnattendedDecision::Proceed
    );
}

#[test]
fn unattended_tier0_is_capped_regardless_of_everything_else() {
    // Inv 16: a software-only (Tier 0) deployment can NEVER do unattended, even paired + authorized.
    let auth = authz(&["screen.view"], NOW + 60_000);
    assert_eq!(
        unattended_decision(true, AssuranceTier::Tier0, Some(&auth), NOW),
        UnattendedDecision::RequireAttendedConsent(UnattendedRefusal::InsufficientTier)
    );
}

#[test]
fn unattended_requires_pairing_authorization_and_freshness() {
    let auth = authz(&["screen.view"], NOW + 60_000);
    // Not paired → fall back to attended (Inv 1 — de-listing the key kills unattended).
    assert_eq!(
        unattended_decision(false, AssuranceTier::Tier1, Some(&auth), NOW),
        UnattendedDecision::RequireAttendedConsent(UnattendedRefusal::NotPaired)
    );
    // Paired + attested but no standing authorization on file.
    assert_eq!(
        unattended_decision(true, AssuranceTier::Tier1, None, NOW),
        UnattendedDecision::RequireAttendedConsent(UnattendedRefusal::NotAuthorized)
    );
    // Authorization present but expired (Inv 3 — never silently permanent). Boundary: `now == expiry`.
    let expired = authz(&["screen.view"], NOW);
    assert_eq!(
        unattended_decision(true, AssuranceTier::Tier1, Some(&expired), NOW),
        UnattendedDecision::RequireAttendedConsent(UnattendedRefusal::Expired)
    );
    // One ms before expiry still proceeds.
    let almost = authz(&["screen.view"], NOW + 1);
    assert_eq!(
        unattended_decision(true, AssuranceTier::Tier1, Some(&almost), NOW),
        UnattendedDecision::Proceed
    );
}
