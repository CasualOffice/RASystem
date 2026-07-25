//! Signed contact signaling for Casual RAS — the pure security core (ADR-094/095).
//!
//! A **signal** is a tiny, self-authenticating message a contact sends out of session: a presence
//! **beacon** ("I'm online"), a direct **message**, or an **access-request intent** ("I'd like to view
//! your screen"). These ride iroh-gossip (presence) or the `casual-ras/signal/1` ALPN (messages /
//! intents) — added in a later increment. This module is the part that has to be exactly right and can
//! be fully tested off-device: **canonical signing + verification + contacts-only authorization.**
//!
//! Why sign at the app layer: iroh-gossip does **not** authenticate a message's author — a multi-hop
//! payload's `delivered_from` is only the forwarding neighbour, never the origin (ADR-094). So every
//! signal carries the sender's Ed25519 public key + a signature over its canonical bytes, and the
//! receiver **verifies the signature and then requires the sender to be a saved, non-blocked contact**
//! (deny-by-default, contacts-only). Authentication (who signed) then authorization (are they a
//! contact) — an unsigned, forged, or stranger signal is dropped. A verified `AccessRequestIntent` is
//! still only an *intent*: it raises a local consent prompt (Inv 1) and authorizes nothing (Inv 9).
//!
//! Secret hygiene (Inv 8): a `DirectMessage` body is [`Redacted`] and elided from `Debug`, so a signal
//! can never leak typed text to a log/trace.

use ras_identity::{ContactBook, ContactId, KeyStore, PUBLIC_KEY_LEN, SIGNATURE_LEN};
use ras_protocol::{CallMediaKind, ErrorCode, RasError, Redacted};

pub mod net;
pub mod presence;

/// Signaling errors reuse the shared taxonomy.
pub type SignalError = RasError;

/// Frame ceiling for one signal. Sits under iroh-gossip's default 4096-byte max message, so a signed
/// signal always fits a single gossip datagram (and comfortably in a direct signaling stream too).
pub const MAX_SIGNAL_FRAME: usize = 4096;
/// Max direct-message body (bytes). Short by design — a long conversation goes over the in-session chat
/// on the session ALPN, not over signaling.
pub const MAX_SIGNAL_TEXT: usize = 2048;
/// Max access-request reason string (bytes).
pub const MAX_SIGNAL_REASON: usize = 256;

const TAG_BEACON: u8 = 1;
const TAG_MESSAGE: u8 = 2;
const TAG_INTENT: u8 = 3;
const TAG_CALL_INVITE: u8 = 4;
const TAG_CALL_CANCEL: u8 = 5;

fn invalid() -> SignalError {
    RasError::fatal(ErrorCode::InvalidMessage, "malformed signal")
}

/// The signed content of a signaling message. Every variant carries `issued_at` (host clock, ms) for
/// freshness / replay bounding by the receiver.
#[derive(Clone, PartialEq, Eq)]
pub enum SignalPayload {
    /// "I am online at `issued_at`" — a presence heartbeat (ADR-094). A fresh, signed beacon from an
    /// active contact means that contact is currently reachable.
    PresenceBeacon { issued_at: u64 },
    /// A direct text message to a contact (ADR-095). The body is [`Redacted`] (Inv 8) — a secret in the
    /// same sense as chat; it must never reach a log/trace.
    DirectMessage { issued_at: u64, text: Redacted },
    /// A request to open a remote-access session (ADR-095). **Intent only** — never authorization: on
    /// receipt it raises a local consent prompt (Inv 1) and grants nothing. `reason` is a short human
    /// string shown in that prompt.
    AccessRequestIntent { issued_at: u64, reason: String },
    /// Ring a contact for a 1:1 call (ADR-104). **Intent only, authorizes nothing** (Inv 9) — like
    /// `AccessRequestIntent`, it raises a local incoming-call surface (Inv 1) and the callee's explicit
    /// accept is what starts the media session. `media` says whether the caller is offering voice or
    /// video; mic/camera are still separately capability-gated per-message host-side (ADR-103, Inv 15).
    CallInvite {
        issued_at: u64,
        /// Voice or video (canonical `CallMediaKind`, shared with the in-session control plane).
        media: CallMediaKind,
    },
    /// Withdraw a still-ringing call the caller placed (they hung up before the callee answered, when
    /// there is no session channel yet to carry a `CallHangup`). Content-free.
    CallCancel { issued_at: u64 },
}

impl core::fmt::Debug for SignalPayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The message body is elided by `Redacted`'s own Debug (Inv 8); tags/timestamps/reason are fine.
        match self {
            Self::PresenceBeacon { issued_at } => f
                .debug_struct("PresenceBeacon")
                .field("issued_at", issued_at)
                .finish(),
            Self::DirectMessage { issued_at, text } => f
                .debug_struct("DirectMessage")
                .field("issued_at", issued_at)
                .field("text", text)
                .finish(),
            Self::AccessRequestIntent { issued_at, reason } => f
                .debug_struct("AccessRequestIntent")
                .field("issued_at", issued_at)
                .field("reason", reason)
                .finish(),
            Self::CallInvite { issued_at, media } => f
                .debug_struct("CallInvite")
                .field("issued_at", issued_at)
                .field("media", media)
                .finish(),
            Self::CallCancel { issued_at } => f
                .debug_struct("CallCancel")
                .field("issued_at", issued_at)
                .finish(),
        }
    }
}

impl SignalPayload {
    fn issued_at(&self) -> u64 {
        match self {
            Self::PresenceBeacon { issued_at }
            | Self::DirectMessage { issued_at, .. }
            | Self::AccessRequestIntent { issued_at, .. }
            | Self::CallInvite { issued_at, .. }
            | Self::CallCancel { issued_at } => *issued_at,
        }
    }

    /// Reject an over-long string before signing (the decoder enforces the same bounds, so a valid
    /// signed signal always decodes).
    fn check_sizes(&self) -> Result<(), SignalError> {
        match self {
            Self::PresenceBeacon { .. } => Ok(()),
            Self::DirectMessage { text, .. } if text.reveal().len() > MAX_SIGNAL_TEXT => {
                Err(invalid())
            }
            Self::AccessRequestIntent { reason, .. } if reason.len() > MAX_SIGNAL_REASON => {
                Err(invalid())
            }
            _ => Ok(()),
        }
    }

    /// Deterministic canonical encoding — the exact bytes that are signed and verified.
    fn encode_canonical(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        let put_str = |out: &mut Vec<u8>, s: &[u8]| {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s);
        };
        match self {
            Self::PresenceBeacon { issued_at } => {
                out.push(TAG_BEACON);
                out.extend_from_slice(&issued_at.to_le_bytes());
            }
            Self::DirectMessage { issued_at, text } => {
                out.push(TAG_MESSAGE);
                out.extend_from_slice(&issued_at.to_le_bytes());
                put_str(&mut out, text.reveal().as_bytes());
            }
            Self::AccessRequestIntent { issued_at, reason } => {
                out.push(TAG_INTENT);
                out.extend_from_slice(&issued_at.to_le_bytes());
                put_str(&mut out, reason.as_bytes());
            }
            Self::CallInvite { issued_at, media } => {
                out.push(TAG_CALL_INVITE);
                out.extend_from_slice(&issued_at.to_le_bytes());
                out.push(media.to_u8());
            }
            Self::CallCancel { issued_at } => {
                out.push(TAG_CALL_CANCEL);
                out.extend_from_slice(&issued_at.to_le_bytes());
            }
        }
        out
    }

    /// Decode a canonical payload. **Fail-closed**: unknown tag, truncation, an over-long or non-UTF-8
    /// string, or any trailing byte is an error.
    fn decode_canonical(bytes: &[u8]) -> Result<Self, SignalError> {
        let mut p = 0usize;
        let take = |p: &mut usize, n: usize| -> Result<&[u8], SignalError> {
            let s = bytes
                .get(*p..p.checked_add(n).ok_or_else(invalid)?)
                .ok_or_else(invalid)?;
            *p += n;
            Ok(s)
        };
        let tag = take(&mut p, 1)?[0];
        let issued_at = u64::from_le_bytes(take(&mut p, 8)?.try_into().map_err(|_| invalid())?);
        let take_str = |p: &mut usize, max: usize| -> Result<String, SignalError> {
            let len = u32::from_le_bytes(take(p, 4)?.try_into().map_err(|_| invalid())?) as usize;
            if len > max {
                return Err(invalid());
            }
            let s = std::str::from_utf8(take(p, len)?).map_err(|_| invalid())?;
            Ok(s.to_string())
        };
        let payload = match tag {
            TAG_BEACON => Self::PresenceBeacon { issued_at },
            TAG_MESSAGE => Self::DirectMessage {
                issued_at,
                text: Redacted(take_str(&mut p, MAX_SIGNAL_TEXT)?),
            },
            TAG_INTENT => Self::AccessRequestIntent {
                issued_at,
                reason: take_str(&mut p, MAX_SIGNAL_REASON)?,
            },
            TAG_CALL_INVITE => Self::CallInvite {
                issued_at,
                // Fail-closed: an unknown media byte is a malformed signal, not a default kind.
                media: CallMediaKind::from_u8(take(&mut p, 1)?[0]).ok_or_else(invalid)?,
            },
            TAG_CALL_CANCEL => Self::CallCancel { issued_at },
            _ => return Err(invalid()),
        };
        if p != bytes.len() {
            return Err(invalid()); // trailing bytes ⇒ malformed (canonical is exact)
        }
        Ok(payload)
    }
}

/// A signal that PASSED verification: the signature checks out AND the sender is a saved, non-blocked
/// contact AND it is fresh. The caller can act on this — subject, always, to the local user's consent
/// for anything that touches the screen/OS (Inv 1).
#[derive(Debug)]
pub struct VerifiedSignal {
    /// The authenticated, authorized sender (a saved active contact).
    pub sender: ContactId,
    /// The signal content.
    pub payload: SignalPayload,
    /// A per-message replay tag: the message's Ed25519 signature. Unique to this exact signed
    /// message (a verbatim replay carries the identical signature; any legitimately-distinct message
    /// — different content or a fresh ms-stamped `issued_at` — has a different one). A receiver can
    /// feed this to a [`SignalReplayGuard`] to drop a captured-and-replayed signal within the
    /// freshness window (the consent-fatigue surface a replayed `AccessRequestIntent` opens). Carried
    /// out here rather than dedup'd inside `verify_signed` so this module stays pure/stateless.
    pub replay_tag: [u8; SIGNATURE_LEN],
}

/// Sign `payload` with the local identity and produce the wire bytes to broadcast/send. Layout:
/// `sender_pubkey[32] · signature[64] · canonical_payload`. Errors if a string is over-long or the
/// frame would exceed [`MAX_SIGNAL_FRAME`].
pub fn encode_signed(ks: &dyn KeyStore, payload: &SignalPayload) -> Result<Vec<u8>, SignalError> {
    payload.check_sizes()?;
    let canonical = payload.encode_canonical();
    let sig = ks.sign(&canonical)?;
    let sender = ks.public_key();
    let mut out = Vec::with_capacity(PUBLIC_KEY_LEN + SIGNATURE_LEN + canonical.len());
    out.extend_from_slice(&sender);
    out.extend_from_slice(&sig);
    out.extend_from_slice(&canonical);
    if out.len() > MAX_SIGNAL_FRAME {
        return Err(invalid());
    }
    Ok(out)
}

/// Verify + authorize an inbound signal (the receiver's single entry point). Fail-closed, in order:
/// 1. bounded frame; 2. **signature over the exact wire bytes** (authenticates the sender key —
///    `SignatureInvalid`); 3. **contacts-only**: the sender must be a saved, non-blocked contact
///    (`CapabilityDenied` — a stranger or blocked peer is dropped); 4. decode; 5. **freshness**
///    (`|now − issued_at| ≤ max_age_ms` — bounds staleness *and* replay/future-dating → `RequestExpired`).
///
/// `now`/`max_age_ms` are passed in (this module stays clock-free and deterministic).
pub fn verify_signed(
    bytes: &[u8],
    book: &dyn ContactBook,
    now: u64,
    max_age_ms: u64,
) -> Result<VerifiedSignal, SignalError> {
    if bytes.len() > MAX_SIGNAL_FRAME {
        return Err(invalid());
    }
    let sender: [u8; PUBLIC_KEY_LEN] = bytes
        .get(..PUBLIC_KEY_LEN)
        .ok_or_else(invalid)?
        .try_into()
        .map_err(|_| invalid())?;
    let sig: [u8; SIGNATURE_LEN] = bytes
        .get(PUBLIC_KEY_LEN..PUBLIC_KEY_LEN + SIGNATURE_LEN)
        .ok_or_else(invalid)?
        .try_into()
        .map_err(|_| invalid())?;
    let canonical = bytes
        .get(PUBLIC_KEY_LEN + SIGNATURE_LEN..)
        .ok_or_else(invalid)?;
    // (2) Verify the signature over the EXACT wire payload bytes — never re-encode a decoded payload.
    ras_identity::verify(&sender, canonical, &sig)?;
    // (3) Contacts-only, deny-by-default: authenticated ≠ authorized. A stranger or blocked peer is
    // refused here, before we even decode the content.
    let sender_id = ContactId::from_bytes(sender);
    if !book.is_active_contact(&sender_id) {
        return Err(RasError::fatal(
            ErrorCode::CapabilityDenied,
            "signal from a non-contact",
        ));
    }
    // (4) Decode + (5) freshness.
    let payload = SignalPayload::decode_canonical(canonical)?;
    if now.abs_diff(payload.issued_at()) > max_age_ms {
        return Err(RasError::fatal(
            ErrorCode::RequestExpired,
            "stale or future-dated signal",
        ));
    }
    Ok(VerifiedSignal {
        sender: sender_id,
        payload,
        replay_tag: sig,
    })
}

/// A bounded, TTL-swept dedup of recently-seen signal [`replay_tag`](VerifiedSignal::replay_tag)s, so a
/// receiver can drop a **verbatim replay** of a signed signal within the freshness window. Mirrors
/// [`ras_bootstrap::NonceCache`] in shape (stateful `&mut self`, clock-free — the caller supplies
/// `now`), keyed by the 64-byte signature.
///
/// Retention only needs to cover the freshness window: a replay older than `max_age_ms` is already
/// rejected by [`verify_signed`]'s freshness check, so an entry can be swept once it's that stale.
/// Bounded by `max_entries` — at the ceiling a *new* tag is refused (returns "replay") rather than
/// evicting a still-live entry, so the cache can never be flooded into forgetting a real recent signal
/// (fail-closed under pressure, exactly like the bootstrap nonce cache).
pub struct SignalReplayGuard {
    /// tag → expiry (ms). An entry is live until `now > expiry`.
    seen: std::collections::HashMap<[u8; SIGNATURE_LEN], u64>,
    max_entries: usize,
}

impl SignalReplayGuard {
    /// `max_entries` bounds memory (a DoS ceiling). `1024` is ample: legitimate out-of-session signals
    /// (calls / messages) are rare and user-driven.
    #[must_use]
    pub fn new(max_entries: usize) -> Self {
        Self {
            seen: std::collections::HashMap::new(),
            max_entries,
        }
    }

    /// Returns `true` if `tag` is **fresh** (first seen) and records it until `now + ttl_ms`; returns
    /// `false` if it's a replay (already recorded) **or** the cache is full (fail-closed). Sweeps
    /// expired entries first so a long-lived process doesn't grow unbounded.
    pub fn check_and_remember(&mut self, tag: [u8; SIGNATURE_LEN], now: u64, ttl_ms: u64) -> bool {
        self.seen.retain(|_, expiry| now <= *expiry);
        if self.seen.contains_key(&tag) {
            return false; // replay
        }
        if self.seen.len() >= self.max_entries {
            return false; // full → refuse (never evict a live entry to admit a new one)
        }
        self.seen.insert(tag, now.saturating_add(ttl_ms));
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_identity::{Contact, ContactBook, InMemoryContactBook, SoftwareKeyStore};

    const MAX_AGE: u64 = 60_000; // 60 s freshness window

    fn book_with(ks: &SoftwareKeyStore, blocked: bool) -> InMemoryContactBook {
        let book = InMemoryContactBook::new();
        let contact = Contact {
            id: ContactId::from_bytes(ks.public_key()),
            label: "peer".into(),
            added_at: 0,
            last_seen_at: 0,
            blocked,
        };
        book.upsert(contact);
        book
    }

    #[test]
    fn beacon_round_trips_from_an_active_contact() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, false);
        let wire = encode_signed(&ks, &SignalPayload::PresenceBeacon { issued_at: 1000 }).unwrap();
        let v = verify_signed(&wire, &book, 1200, MAX_AGE).unwrap();
        assert_eq!(v.sender, ContactId::from_bytes(ks.public_key()));
        assert!(matches!(
            v.payload,
            SignalPayload::PresenceBeacon { issued_at: 1000 }
        ));
    }

    #[test]
    fn message_and_intent_round_trip() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, false);
        let msg = SignalPayload::DirectMessage {
            issued_at: 500,
            text: Redacted("hi there 👋".into()),
        };
        let v = verify_signed(&encode_signed(&ks, &msg).unwrap(), &book, 500, MAX_AGE).unwrap();
        match v.payload {
            SignalPayload::DirectMessage { text, .. } => assert_eq!(text.reveal(), "hi there 👋"),
            _ => panic!("wrong variant"),
        }
        let intent = SignalPayload::AccessRequestIntent {
            issued_at: 500,
            reason: "remote support".into(),
        };
        let v = verify_signed(&encode_signed(&ks, &intent).unwrap(), &book, 500, MAX_AGE).unwrap();
        assert!(matches!(
            v.payload,
            SignalPayload::AccessRequestIntent { .. }
        ));
    }

    #[test]
    fn call_signals_round_trip_signed_and_contacts_only() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, false);
        for media in [CallMediaKind::Voice, CallMediaKind::Video] {
            let invite = SignalPayload::CallInvite {
                issued_at: 500,
                media,
            };
            let v =
                verify_signed(&encode_signed(&ks, &invite).unwrap(), &book, 500, MAX_AGE).unwrap();
            match v.payload {
                SignalPayload::CallInvite { media: m, .. } => assert_eq!(m, media),
                _ => panic!("wrong variant"),
            }
        }
        let cancel = SignalPayload::CallCancel { issued_at: 500 };
        let v = verify_signed(&encode_signed(&ks, &cancel).unwrap(), &book, 500, MAX_AGE).unwrap();
        assert!(matches!(v.payload, SignalPayload::CallCancel { .. }));
    }

    #[test]
    fn call_invite_with_unknown_media_byte_fails_closed() {
        // A CallInvite whose media byte is neither 1 (Voice) nor 2 (Video) must be rejected, not
        // silently defaulted — an unknown/future media kind is a malformed signal.
        let mut canonical = SignalPayload::CallInvite {
            issued_at: 7,
            media: CallMediaKind::Voice,
        }
        .encode_canonical();
        *canonical.last_mut().unwrap() = 9; // corrupt the trailing media byte
        assert!(SignalPayload::decode_canonical(&canonical).is_err());
    }

    #[test]
    fn replay_guard_rejects_a_verbatim_replay_but_admits_distinct_signals() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, false);

        // One signed intent, verified twice (a passive observer capturing + re-injecting the exact
        // signed bytes → the same `replay_tag`).
        let wire = encode_signed(
            &ks,
            &SignalPayload::AccessRequestIntent {
                issued_at: 500,
                reason: "support".into(),
            },
        )
        .unwrap();
        let a = verify_signed(&wire, &book, 500, MAX_AGE).unwrap();
        let b = verify_signed(&wire, &book, 501, MAX_AGE).unwrap();
        assert_eq!(
            a.replay_tag, b.replay_tag,
            "a verbatim replay reuses the tag"
        );

        let mut guard = SignalReplayGuard::new(16);
        assert!(
            guard.check_and_remember(a.replay_tag, 500, MAX_AGE),
            "first sighting is fresh"
        );
        assert!(
            !guard.check_and_remember(b.replay_tag, 501, MAX_AGE),
            "the verbatim replay is rejected"
        );

        // A legitimately-distinct signal (different reason ⇒ different signature) is still admitted.
        let other = encode_signed(
            &ks,
            &SignalPayload::AccessRequestIntent {
                issued_at: 500,
                reason: "different".into(),
            },
        )
        .unwrap();
        let c = verify_signed(&other, &book, 502, MAX_AGE).unwrap();
        assert_ne!(a.replay_tag, c.replay_tag);
        assert!(
            guard.check_and_remember(c.replay_tag, 502, MAX_AGE),
            "a distinct signal is not a replay"
        );
    }

    #[test]
    fn replay_guard_forgets_stale_entries_and_is_bounded() {
        let mut guard = SignalReplayGuard::new(2);
        // Insert at t=0 with a 100ms ttl.
        assert!(guard.check_and_remember([1u8; SIGNATURE_LEN], 0, 100));
        // Well past expiry: the same tag is admitted again (a replay outside the freshness window is
        // already rejected by `verify_signed`'s freshness check, so the guard needn't remember it).
        assert!(guard.check_and_remember([1u8; SIGNATURE_LEN], 1000, 100));
        // Bound: fill to capacity within one window, then a NEW tag is refused (fail-closed, never
        // evicts a live entry to admit a new one).
        assert!(guard.check_and_remember([2u8; SIGNATURE_LEN], 1000, 100));
        assert!(
            !guard.check_and_remember([3u8; SIGNATURE_LEN], 1000, 100),
            "at capacity a new tag is refused, not admitted by evicting a live one"
        );
    }

    #[test]
    fn a_tampered_payload_fails_signature() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, false);
        let mut wire = encode_signed(&ks, &SignalPayload::PresenceBeacon { issued_at: 1 }).unwrap();
        let last = wire.len() - 1;
        wire[last] ^= 0xff; // flip an issued_at byte in the canonical tail
        let err = verify_signed(&wire, &book, 1, MAX_AGE).unwrap_err();
        assert_eq!(err.code, ErrorCode::SignatureInvalid);
    }

    #[test]
    fn a_forged_sender_key_fails_signature() {
        // Sign with A, then overwrite the sender field with B's key: the signature no longer matches.
        let a = SoftwareKeyStore::generate().unwrap();
        let b = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&b, false);
        let mut wire = encode_signed(&a, &SignalPayload::PresenceBeacon { issued_at: 1 }).unwrap();
        wire[..PUBLIC_KEY_LEN].copy_from_slice(&b.public_key());
        assert_eq!(
            verify_signed(&wire, &book, 1, MAX_AGE).unwrap_err().code,
            ErrorCode::SignatureInvalid
        );
    }

    #[test]
    fn a_non_contact_is_refused_even_with_a_valid_signature() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let empty = InMemoryContactBook::new(); // sender is NOT saved
        let wire = encode_signed(&ks, &SignalPayload::PresenceBeacon { issued_at: 1 }).unwrap();
        assert_eq!(
            verify_signed(&wire, &empty, 1, MAX_AGE).unwrap_err().code,
            ErrorCode::CapabilityDenied
        );
    }

    #[test]
    fn a_blocked_contact_is_refused() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, true); // saved but blocked
        let wire = encode_signed(&ks, &SignalPayload::PresenceBeacon { issued_at: 1 }).unwrap();
        assert_eq!(
            verify_signed(&wire, &book, 1, MAX_AGE).unwrap_err().code,
            ErrorCode::CapabilityDenied
        );
    }

    #[test]
    fn stale_and_future_dated_signals_are_rejected() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, false);
        let wire =
            encode_signed(&ks, &SignalPayload::PresenceBeacon { issued_at: 100_000 }).unwrap();
        // Too old: now far past issued_at + window.
        assert_eq!(
            verify_signed(&wire, &book, 100_000 + MAX_AGE + 1, MAX_AGE)
                .unwrap_err()
                .code,
            ErrorCode::RequestExpired
        );
        // Future-dated beyond the window (replay/clock-skew bound).
        assert_eq!(
            verify_signed(&wire, &book, 100_000 - MAX_AGE - 1, MAX_AGE)
                .unwrap_err()
                .code,
            ErrorCode::RequestExpired
        );
    }

    #[test]
    fn oversize_text_is_refused_on_encode() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let big = SignalPayload::DirectMessage {
            issued_at: 0,
            text: Redacted("x".repeat(MAX_SIGNAL_TEXT + 1)),
        };
        assert!(encode_signed(&ks, &big).is_err());
    }

    #[test]
    fn decoder_never_panics_on_arbitrary_or_truncated_bytes() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let book = book_with(&ks, false);
        let good = encode_signed(
            &ks,
            &SignalPayload::DirectMessage {
                issued_at: 1,
                text: Redacted("payload".into()),
            },
        )
        .unwrap();
        // Every truncation of a valid frame must return Err (never panic).
        for n in 0..good.len() {
            let _ = verify_signed(&good[..n], &book, 1, MAX_AGE);
        }
        // A spread of arbitrary byte patterns and lengths must never panic.
        for len in [0usize, 1, 32, 96, 97, 200, 5000] {
            for seed in 0u8..=255 {
                let junk: Vec<u8> = (0..len).map(|i| seed ^ (i as u8)).collect();
                let _ = verify_signed(&junk, &book, 1, MAX_AGE);
            }
        }
    }
}
