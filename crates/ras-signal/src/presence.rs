//! Pairwise presence over gossip (ADR-094) — the pure, correctness-critical parts: the topic
//! derivation and the presence state machine. The async gossip broadcast/receive task that drives
//! them (subscribe → periodically broadcast a signed [`crate::SignalPayload::PresenceBeacon`] → verify
//! inbound beacons contacts-only → feed this tracker) lands in the next increment; keeping these pure
//! lets them be tested exhaustively off-device.

use std::collections::HashMap;

use iroh_gossip::TopicId;
use ras_identity::ContactId;

/// Domain separator so a presence topic can never collide with any other gossip use of the same keys.
const TOPIC_DOMAIN: &[u8] = b"casual-ras/presence/v1";

/// Derive the **pairwise gossip topic** for two contacts. Both sides compute the same 32-byte
/// `TopicId` from the two identities — order-independent (the keys are sorted) and domain-separated.
///
/// Security note (MVP): the topic is derived from the two **public** keys, so anyone who knows *both*
/// pubkeys can compute it and join the topic — learning only that the pair is online. That is the
/// **only** thing a topic-guesser gains: beacons are SIGNED and verified **contacts-only** (see
/// [`crate::verify_signed`]), so a guesser can never inject a valid beacon or impersonate a contact
/// (ADR-094). A pairing-secret-derived topic (unguessable even to someone holding both pubkeys) is the
/// hardening follow-up, once the pairing handshake exchanges a shared secret.
#[must_use]
pub fn pairwise_topic(a: &ContactId, b: &ContactId) -> TopicId {
    use sha2::{Digest, Sha256};
    // Sort so `pairwise_topic(a, b) == pairwise_topic(b, a)`.
    let (lo, hi) = if a.as_bytes() <= b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    };
    let mut h = Sha256::new();
    h.update(TOPIC_DOMAIN);
    h.update(lo.as_bytes());
    h.update(hi.as_bytes());
    let digest: [u8; 32] = h.finalize().into();
    TopicId::from_bytes(digest)
}

/// Tracks which contacts are currently **online**, from the signed presence beacons the gossip task
/// feeds it. Pure + clock-free (the caller passes `now`, ms). A contact is online iff a beacon was seen
/// within a staleness window — so a few dropped beacons read as offline, which absorbs gossip's
/// best-effort, unordered, lossy delivery without flapping.
#[derive(Debug, Default)]
pub struct PresenceTracker {
    last_seen: HashMap<ContactId, u64>,
}

impl PresenceTracker {
    /// A fresh, empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a **verified** beacon from `contact` observed at `now` (ms). Monotonic: an out-of-order
    /// older beacon never moves `last_seen` backwards (gossip delivers unordered).
    pub fn observe(&mut self, contact: ContactId, now: u64) {
        let e = self.last_seen.entry(contact).or_insert(0);
        *e = (*e).max(now);
    }

    /// Whether `contact` is online at `now`: a beacon was seen within `window_ms`.
    #[must_use]
    pub fn is_online(&self, contact: &ContactId, now: u64, window_ms: u64) -> bool {
        self.last_seen
            .get(contact)
            .is_some_and(|&t| now.saturating_sub(t) <= window_ms)
    }

    /// Drop a contact's presence (the last gossip link went down, or the user removed/blocked them).
    pub fn forget(&mut self, contact: &ContactId) {
        self.last_seen.remove(contact);
    }

    /// Every contact currently online at `now`.
    #[must_use]
    pub fn online_now(&self, now: u64, window_ms: u64) -> Vec<ContactId> {
        self.last_seen
            .iter()
            .filter(|(_, &t)| now.saturating_sub(t) <= window_ms)
            .map(|(id, _)| *id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cid(n: u8) -> ContactId {
        ContactId::from_bytes([n; 32])
    }

    #[test]
    fn pairwise_topic_is_order_independent_deterministic_and_pair_specific() {
        let a = cid(1);
        let b = cid(2);
        let c = cid(3);
        assert_eq!(
            pairwise_topic(&a, &b),
            pairwise_topic(&b, &a),
            "order-independent"
        );
        assert_eq!(
            pairwise_topic(&a, &b),
            pairwise_topic(&a, &b),
            "deterministic"
        );
        assert_ne!(
            pairwise_topic(&a, &b),
            pairwise_topic(&a, &c),
            "a different pair ⇒ a different topic"
        );
    }

    #[test]
    fn pairwise_topic_is_domain_separated_from_raw_key_material() {
        // The topic is a hash, not the raw keys concatenated — so it never equals either key's bytes.
        let a = cid(9);
        let b = cid(10);
        let t = pairwise_topic(&a, &b);
        assert_ne!(t.as_bytes(), a.as_bytes());
        assert_ne!(t.as_bytes(), b.as_bytes());
    }

    #[test]
    fn presence_online_within_window_then_stale() {
        let mut p = PresenceTracker::new();
        let alice = cid(1);
        p.observe(alice, 10_000);
        assert!(
            p.is_online(&alice, 12_000, 5_000),
            "seen 2s ago, 5s window ⇒ online"
        );
        assert!(
            !p.is_online(&alice, 16_000, 5_000),
            "seen 6s ago, 5s window ⇒ offline"
        );
        // An unknown contact is never online.
        assert!(!p.is_online(&cid(2), 12_000, 5_000));
    }

    #[test]
    fn presence_is_monotonic_and_forgettable() {
        let mut p = PresenceTracker::new();
        let alice = cid(1);
        p.observe(alice, 10_000);
        p.observe(alice, 4_000); // older (out-of-order) beacon must not regress last_seen
        assert!(
            p.is_online(&alice, 11_000, 2_000),
            "still fresh from the 10_000 beacon"
        );
        p.forget(&alice);
        assert!(!p.is_online(&alice, 11_000, 2_000), "forgotten ⇒ offline");
    }

    #[test]
    fn online_now_lists_only_fresh_contacts() {
        let mut p = PresenceTracker::new();
        p.observe(cid(1), 10_000);
        p.observe(cid(2), 3_000);
        let mut online = p.online_now(11_000, 5_000);
        online.sort_by_key(|c| *c.as_bytes());
        assert_eq!(online, vec![cid(1)], "only the contact within the window");
    }
}
