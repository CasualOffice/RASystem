//! Bootstrap-phase state for Casual RAS Phase 2: **rotating single-use connection tickets** and the
//! **replay caches** (consumed-ticket set + AccessRequest nonce cache).
//!
//! A [`ConnectionTicket`] is a *bootstrap artifact, not a credential* (`docs/16 §1.5`): host-signed,
//! generation-versioned, single-use, and short-lived. Stolen, it still cannot grant access without
//! local consent (Inv 1) — it only lets a controller *reach* the host and *ask*. The host mints
//! exactly **one live ticket at a time**: [`TicketAuthority::issue`] bumps the generation and
//! instantly invalidates every prior ticket, and [`TicketAuthority::consume`] fails closed on a bad
//! signature, wrong host, expiry, stale generation, or replay.
//!
//! Signing/verification goes through the [`ras_identity`] `KeyStore`/`verify` seam, so no
//! signature-primitive type leaks here (ADR-065). The dial info (endpoint id + relay/direct hints,
//! the Phase-1 `CASUALRAS1:` ticket) rides as an **opaque byte blob** — this crate never parses it,
//! so it stays free of the iroh transport tree.
//!
//! Time is always an explicit `now: UnixMillis` argument — there is no ambient clock, so every path
//! is deterministically testable and no hidden time source can be spoofed.

use std::collections::{HashMap, HashSet};

use ras_identity::{verify, KeyStore};
use ras_protocol::{ErrorCode, RasError};

/// Milliseconds since the Unix epoch (host wall clock). Used only for expiry/replay windows, never
/// for authorization decisions beyond "is this still fresh?".
pub type UnixMillis = u64;

/// Monotonic ticket generation. Minting a new ticket bumps this; a ticket whose generation is not
/// the current one is stale (a visible tamper/replay signal).
pub type TicketGeneration = u64;

/// A random 16-byte ticket identifier (the key in the consumed-set). 128 bits — collision-free in
/// practice, and unguessable so it cannot be enumerated.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TicketId(pub [u8; 16]);

impl core::fmt::Debug for TicketId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Short hex prefix; a ticket id is not a secret but there is no reason to dump all 16 bytes.
        write!(f, "TicketId({:02x}{:02x}…)", self.0[0], self.0[1])
    }
}

const TICKET_PREFIX: &str = "CASUALRAST1:";
/// Domain-separation tag mixed into the signed bytes so a ticket signature can never be replayed as
/// any other Casual RAS signature (grant, pairing, …) and vice versa.
const TICKET_CTX: &[u8] = b"casual-ras/ticket/v1";
const TICKET_VERSION: u8 = 1;
const SIG_LEN: usize = 64;
const HOST_ID_LEN: usize = 32;

/// Rotating single-use bootstrap artifact (`docs/16 §1.5`). Host-signed; hex-encoded as
/// `CASUALRAST1:<hex>`. **Not a credential** — see the module docs.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionTicket {
    /// Unguessable per-ticket id (consumed-set key).
    pub ticket_id: TicketId,
    /// The generation this ticket was minted at; must equal the authority's active generation.
    pub ticket_generation: TicketGeneration,
    /// Always `true` in the MVP: consuming it marks it spent.
    pub single_use: bool,
    /// The host this ticket targets (its Ed25519 public key). Binds the ticket to one host (Inv 3).
    pub host_id: [u8; HOST_ID_LEN],
    /// Absolute expiry (host wall clock, ms).
    pub expires_at: UnixMillis,
    /// Opaque dial info (the Phase-1 `CASUALRAS1:` endpoint ticket bytes). Never parsed here.
    pub dial: Vec<u8>,
    /// Host signature over `TICKET_CTX || body`.
    pub host_signature: [u8; SIG_LEN],
}

impl core::fmt::Debug for ConnectionTicket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Content-free: no signature bytes, no dial blob (Inv 8-adjacent hygiene).
        f.debug_struct("ConnectionTicket")
            .field("ticket_id", &self.ticket_id)
            .field("ticket_generation", &self.ticket_generation)
            .field("single_use", &self.single_use)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl ConnectionTicket {
    /// The bytes covered by the host signature: the domain tag followed by the encoded body (every
    /// field except the signature itself). Re-encoded identically on both sides, so the signature is
    /// verified over a canonical form, not over received-and-trusted bytes.
    fn signing_input(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(TICKET_CTX.len() + 64 + self.dial.len());
        v.extend_from_slice(TICKET_CTX);
        self.encode_body_into(&mut v);
        v
    }

    /// Append the encoded body (no ctx, no signature) — the wire layout, all integers big-endian:
    /// `ver:u8 | ticket_id[16] | generation:u64 | single_use:u8 | host_id[32] | expires_at:u64 |
    ///  dial_len:u32 | dial[dial_len]`.
    fn encode_body_into(&self, v: &mut Vec<u8>) {
        v.push(TICKET_VERSION);
        v.extend_from_slice(&self.ticket_id.0);
        v.extend_from_slice(&self.ticket_generation.to_be_bytes());
        v.push(u8::from(self.single_use));
        v.extend_from_slice(&self.host_id);
        v.extend_from_slice(&self.expires_at.to_be_bytes());
        // dial is bounded by the caller (it is a small endpoint ticket); u32 length prefix.
        let dial_len = u32::try_from(self.dial.len()).unwrap_or(u32::MAX);
        v.extend_from_slice(&dial_len.to_be_bytes());
        v.extend_from_slice(&self.dial[..dial_len as usize]);
    }

    /// Encode as a copy-pasteable `CASUALRAST1:<hex>` string. The hex payload is `body || signature`.
    #[must_use]
    pub fn to_ticket(&self) -> String {
        let mut body = Vec::new();
        self.encode_body_into(&mut body);
        body.extend_from_slice(&self.host_signature);
        let mut s = String::with_capacity(TICKET_PREFIX.len() + body.len() * 2);
        s.push_str(TICKET_PREFIX);
        for b in &body {
            s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
            s.push(char::from_digit(u32::from(b & 0xf), 16).unwrap_or('0'));
        }
        s
    }

    /// Parse a ticket produced by [`to_ticket`](Self::to_ticket). **Fail-closed**: a wrong prefix,
    /// odd/short hex, an over-long field, a bad version, or trailing garbage is a typed, content-free
    /// error — never a partial or defaulted ticket. Does **not** verify the signature; that is the
    /// authority's job in [`TicketAuthority::consume`].
    pub fn from_ticket(ticket: &str) -> Result<Self, RasError> {
        let bad =
            || RasError::recoverable(ErrorCode::InvalidMessage, "malformed connection ticket");
        let hex = ticket.strip_prefix(TICKET_PREFIX).ok_or_else(bad)?;
        if hex.len() % 2 != 0 {
            return Err(bad());
        }
        let h = hex.as_bytes();
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        let mut i = 0;
        while i < h.len() {
            let hi = (h[i] as char).to_digit(16).ok_or_else(bad)?;
            let lo = (h[i + 1] as char).to_digit(16).ok_or_else(bad)?;
            bytes.push(((hi << 4) | lo) as u8);
            i += 2;
        }

        // Cursor decode; every read is bounds-checked against the remaining buffer.
        let mut c = 0usize;
        let take = |c: &mut usize, n: usize| -> Result<std::ops::Range<usize>, RasError> {
            let end = c.checked_add(n).ok_or_else(bad)?;
            if end > bytes.len() {
                return Err(bad());
            }
            let r = *c..end;
            *c = end;
            Ok(r)
        };

        let ver = bytes[take(&mut c, 1)?.start];
        if ver != TICKET_VERSION {
            return Err(bad());
        }
        let mut ticket_id = [0u8; 16];
        ticket_id.copy_from_slice(&bytes[take(&mut c, 16)?]);
        let gen_r = take(&mut c, 8)?;
        let ticket_generation = u64::from_be_bytes(bytes[gen_r].try_into().map_err(|_| bad())?);
        let single_use = match bytes[take(&mut c, 1)?.start] {
            0 => false,
            1 => true,
            _ => return Err(bad()), // only 0/1 are valid; never default a bool
        };
        let mut host_id = [0u8; HOST_ID_LEN];
        host_id.copy_from_slice(&bytes[take(&mut c, HOST_ID_LEN)?]);
        let exp_r = take(&mut c, 8)?;
        let expires_at = u64::from_be_bytes(bytes[exp_r].try_into().map_err(|_| bad())?);
        let dl_r = take(&mut c, 4)?;
        let dial_len = u32::from_be_bytes(bytes[dl_r].try_into().map_err(|_| bad())?) as usize;
        let dial = bytes[take(&mut c, dial_len)?].to_vec();
        let mut host_signature = [0u8; SIG_LEN];
        host_signature.copy_from_slice(&bytes[take(&mut c, SIG_LEN)?]);
        if c != bytes.len() {
            return Err(bad()); // trailing garbage
        }
        Ok(Self {
            ticket_id: TicketId(ticket_id),
            ticket_generation,
            single_use,
            host_id,
            expires_at,
            dial,
            host_signature,
        })
    }
}

/// Host-side ticket rotation + single-use replay state (`docs/16`). Exactly one live ticket at a
/// time: minting a new one bumps the generation, invalidating every prior ticket regardless of the
/// consumed-set. Generic over the [`KeyStore`] so a TPM/Keychain store is a drop-in later.
pub struct TicketAuthority<K: KeyStore> {
    keystore: K,
    host_id: [u8; HOST_ID_LEN],
    active_generation: TicketGeneration,
    consumed: HashSet<TicketId>,
    ttl_ms: u64,
}

impl<K: KeyStore> TicketAuthority<K> {
    /// Build an authority over `keystore` (the host identity) with a per-ticket lifetime of `ttl_ms`.
    /// The active generation starts at 0, so the first [`issue`](Self::issue) mints generation 1.
    pub fn new(keystore: K, ttl_ms: u64) -> Self {
        let host_id = keystore.public_key();
        Self {
            keystore,
            host_id,
            active_generation: 0,
            consumed: HashSet::new(),
            ttl_ms,
        }
    }

    /// The host identity these tickets are bound to.
    #[must_use]
    pub fn host_id(&self) -> [u8; HOST_ID_LEN] {
        self.host_id
    }

    /// The current live generation (0 before any ticket is issued).
    #[must_use]
    pub fn active_generation(&self) -> TicketGeneration {
        self.active_generation
    }

    /// Mint a new single-use ticket carrying `dial` (opaque endpoint dial info). Bumps the generation
    /// — instantly invalidating any prior ticket — and clears the now-irrelevant consumed-set (only
    /// the current generation's single ticket can ever be consumed, so this is safe and bounds
    /// memory). Signs `TICKET_CTX || body` with the host key.
    pub fn issue(&mut self, dial: Vec<u8>, now: UnixMillis) -> Result<ConnectionTicket, RasError> {
        self.active_generation = self.active_generation.saturating_add(1);
        self.consumed.clear();

        let mut id = [0u8; 16];
        getrandom::getrandom(&mut id)
            .map_err(|_| RasError::fatal(ErrorCode::Internal, "csprng unavailable"))?;

        let mut ticket = ConnectionTicket {
            ticket_id: TicketId(id),
            ticket_generation: self.active_generation,
            single_use: true,
            host_id: self.host_id,
            expires_at: now.saturating_add(self.ttl_ms),
            dial,
            host_signature: [0u8; SIG_LEN],
        };
        ticket.host_signature = self.keystore.sign(&ticket.signing_input())?;
        Ok(ticket)
    }

    /// Validate **and consume** a presented ticket. Ordered, fail-closed checks (each maps to a
    /// stable [`ErrorCode`], no reason leaked beyond the code):
    ///
    /// 1. `host_id` matches this host → else [`ErrorCode::IdentityMismatch`].
    /// 2. host signature verifies over `TICKET_CTX || body` → else [`ErrorCode::SignatureInvalid`].
    /// 3. not expired (`now <= expires_at`) → else [`ErrorCode::RequestExpired`].
    /// 4. generation is the current one → else [`ErrorCode::ReplayDetected`] (stale = a tamper signal).
    /// 5. not already consumed → else [`ErrorCode::ReplayDetected`].
    ///
    /// Only on all-pass is the ticket marked consumed (single-use). Consuming is the *only* mutation,
    /// and it happens last, so a rejected ticket never changes state.
    pub fn consume(&mut self, ticket: &ConnectionTicket, now: UnixMillis) -> Result<(), ErrorCode> {
        if ticket.host_id != self.host_id {
            return Err(ErrorCode::IdentityMismatch);
        }
        verify(
            &self.host_id,
            &ticket.signing_input(),
            &ticket.host_signature,
        )
        .map_err(|_| ErrorCode::SignatureInvalid)?;
        if now > ticket.expires_at {
            return Err(ErrorCode::RequestExpired);
        }
        if ticket.ticket_generation != self.active_generation {
            return Err(ErrorCode::ReplayDetected);
        }
        if self.consumed.contains(&ticket.ticket_id) {
            return Err(ErrorCode::ReplayDetected);
        }
        self.consumed.insert(ticket.ticket_id);
        Ok(())
    }
}

/// A bounded, TTL-swept replay cache for one-time nonces (the AccessRequest nonce, `docs/04 §4`;
/// shared with `ras-grant`'s request validation). A nonce is accepted **at most once** within its
/// TTL window; a repeat is [`ErrorCode::ReplayDetected`].
///
/// Fail-closed under flood: entries are swept on every insert, and if the cache is still at its hard
/// `max_entries` ceiling afterwards, a *new* nonce is rejected rather than growing unbounded or
/// evicting a still-valid entry (which would open a replay window). Size the TTL to the request
/// validity window (≤5 min) so honest traffic drains long before the ceiling.
pub struct NonceCache {
    ttl_ms: u64,
    max_entries: usize,
    seen: HashMap<[u8; 16], UnixMillis>, // nonce -> expiry
}

impl NonceCache {
    /// A cache holding a nonce for `ttl_ms`, capped at `max_entries` live nonces.
    #[must_use]
    pub fn new(ttl_ms: u64, max_entries: usize) -> Self {
        Self {
            ttl_ms,
            max_entries,
            seen: HashMap::new(),
        }
    }

    /// Drop every nonce whose expiry has passed.
    fn sweep(&mut self, now: UnixMillis) {
        self.seen.retain(|_, expiry| *expiry > now);
    }

    /// Number of live (unexpired-as-of-last-sweep) nonces. Test/metrics aid.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether the cache currently holds no nonces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Record `nonce` as used, or reject it. Returns:
    /// * `Ok(())` — fresh nonce, now remembered until `now + ttl_ms`.
    /// * `Err(ReplayDetected)` — this nonce was already seen within its TTL.
    /// * `Err(Internal)` — the cache is saturated even after sweeping (fail-closed DoS ceiling).
    pub fn check_and_insert(&mut self, nonce: [u8; 16], now: UnixMillis) -> Result<(), ErrorCode> {
        self.sweep(now);
        if self.seen.contains_key(&nonce) {
            return Err(ErrorCode::ReplayDetected);
        }
        if self.seen.len() >= self.max_entries {
            // Fail closed: never evict a still-valid nonce to make room (that would reopen replay).
            return Err(ErrorCode::Internal);
        }
        self.seen.insert(nonce, now.saturating_add(self.ttl_ms));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_identity::SoftwareKeyStore;

    fn authority(ttl: u64) -> TicketAuthority<SoftwareKeyStore> {
        TicketAuthority::new(SoftwareKeyStore::generate().unwrap(), ttl)
    }

    #[test]
    fn issue_then_consume_succeeds_once() {
        let mut a = authority(60_000);
        let t = a.issue(b"dial-info".to_vec(), 1_000).unwrap();
        assert_eq!(t.ticket_generation, 1);
        assert!(a.consume(&t, 2_000).is_ok());
        // Second use is a replay.
        assert_eq!(a.consume(&t, 3_000), Err(ErrorCode::ReplayDetected));
    }

    #[test]
    fn ticket_string_round_trips() {
        let mut a = authority(60_000);
        let t = a.issue(b"CASUALRAS1:deadbeef".to_vec(), 1_000).unwrap();
        let parsed = ConnectionTicket::from_ticket(&t.to_ticket()).unwrap();
        assert_eq!(parsed, t);
        // And a freshly-parsed copy still consumes.
        assert!(a.consume(&parsed, 2_000).is_ok());
    }

    #[test]
    fn issuing_a_new_ticket_invalidates_the_prior_generation() {
        let mut a = authority(60_000);
        let old = a.issue(b"d".to_vec(), 1_000).unwrap();
        let _new = a.issue(b"d".to_vec(), 1_000).unwrap();
        // The old ticket is now stale-generation, even though it was never consumed.
        assert_eq!(a.consume(&old, 2_000), Err(ErrorCode::ReplayDetected));
    }

    #[test]
    fn expired_ticket_is_rejected() {
        let mut a = authority(1_000);
        let t = a.issue(b"d".to_vec(), 1_000).unwrap(); // expires at 2_000
        assert_eq!(a.consume(&t, 2_001), Err(ErrorCode::RequestExpired));
    }

    #[test]
    fn wrong_host_is_rejected() {
        let mut a = authority(60_000);
        let mut t = a.issue(b"d".to_vec(), 1_000).unwrap();
        t.host_id[0] ^= 0xff; // retarget to a different host
        assert_eq!(a.consume(&t, 2_000), Err(ErrorCode::IdentityMismatch));
    }

    #[test]
    fn tampered_ticket_fails_signature() {
        let mut a = authority(60_000);
        let mut t = a.issue(b"d".to_vec(), 1_000).unwrap();
        t.expires_at += 10_000_000; // extend expiry without re-signing
        assert_eq!(a.consume(&t, 2_000), Err(ErrorCode::SignatureInvalid));
    }

    #[test]
    fn a_ticket_from_another_host_is_rejected() {
        let mut issuer = authority(60_000);
        let mut other = authority(60_000);
        let t = issuer.issue(b"d".to_vec(), 1_000).unwrap();
        // A different authority (different host key) must not accept it: host_id mismatch first.
        assert_eq!(other.consume(&t, 2_000), Err(ErrorCode::IdentityMismatch));
    }

    #[test]
    fn from_ticket_is_fail_closed() {
        let mut a = authority(60_000);
        let good = a.issue(b"d".to_vec(), 1_000).unwrap().to_ticket();
        assert!(ConnectionTicket::from_ticket(&good).is_ok());
        assert!(ConnectionTicket::from_ticket("NOPE:00").is_err());
        assert!(ConnectionTicket::from_ticket(&format!("{good}0")).is_err()); // odd hex
        assert!(ConnectionTicket::from_ticket(&format!("{good}00")).is_err()); // trailing garbage
        assert!(ConnectionTicket::from_ticket(&format!("{TICKET_PREFIX}zz")).is_err()); // bad hex
        assert!(ConnectionTicket::from_ticket(&format!("{TICKET_PREFIX}00")).is_err());
        // too short
    }

    /// A connection ticket is **user-pasted, untrusted input**: `from_ticket` must never panic on
    /// arbitrary strings, only return a typed error. Deterministic dep-free fuzz over arbitrary
    /// printable strings, every truncation of a valid ticket (each field boundary), and single-byte
    /// mutations of a valid one.
    #[test]
    fn from_ticket_never_panics_on_arbitrary_input() {
        let mut a = authority(60_000);
        let good = a.issue(b"dial".to_vec(), 1_000).unwrap().to_ticket();

        // Every prefix of a valid ticket (walks each length/field boundary of the parser).
        for i in 0..=good.len() {
            if good.is_char_boundary(i) {
                let _ = ConnectionTicket::from_ticket(&good[..i]); // must not panic
            }
        }

        // Arbitrary printable-ASCII strings, with and without the real prefix.
        let mut state: u64 = 0xdead_beef_cafe_babe;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 40) as u8
        };
        for len in 0..300usize {
            let s: String = (0..len).map(|_| (0x20 + (next() % 0x5e)) as char).collect();
            let _ = ConnectionTicket::from_ticket(&s);
            let _ = ConnectionTicket::from_ticket(&format!("{TICKET_PREFIX}{s}"));
        }

        // Single-byte mutations of a valid ticket's bytes (kept when still valid UTF-8).
        let bytes = good.into_bytes();
        for i in 0..bytes.len().min(200) {
            let mut m = bytes.clone();
            m[i] ^= 0xFF;
            if let Ok(s) = std::str::from_utf8(&m) {
                let _ = ConnectionTicket::from_ticket(s); // must not panic
            }
        }
    }

    #[test]
    fn nonce_cache_detects_replay() {
        let mut c = NonceCache::new(300_000, 1024);
        let n = [7u8; 16];
        assert!(c.check_and_insert(n, 1_000).is_ok());
        assert_eq!(c.check_and_insert(n, 1_500), Err(ErrorCode::ReplayDetected));
        // A different nonce is fine.
        assert!(c.check_and_insert([8u8; 16], 1_500).is_ok());
    }

    #[test]
    fn nonce_cache_expires_and_readmits() {
        let mut c = NonceCache::new(1_000, 1024);
        let n = [1u8; 16];
        assert!(c.check_and_insert(n, 1_000).is_ok()); // expires at 2_000
                                                       // After expiry the nonce window has closed; the request itself would also be expired, but the
                                                       // cache must not falsely flag a *fresh* reuse of a long-gone nonce as a replay.
        assert!(c.check_and_insert(n, 2_001).is_ok());
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn nonce_cache_is_bounded_fail_closed() {
        let mut c = NonceCache::new(300_000, 2);
        assert!(c.check_and_insert([1u8; 16], 1_000).is_ok());
        assert!(c.check_and_insert([2u8; 16], 1_000).is_ok());
        // At the ceiling with two still-valid nonces → a third fresh nonce is refused, not evicted.
        assert_eq!(
            c.check_and_insert([3u8; 16], 1_000),
            Err(ErrorCode::Internal)
        );
    }
}
