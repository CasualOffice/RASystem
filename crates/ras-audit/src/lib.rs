//! Casual RAS **audit journal** — the tamper-evident record of security-sensitive events (Invariant 10,
//! `docs/06 §12`).
//!
//! Two independent guarantees, together:
//! - **Hash chain (tamper-*evidence*).** Every [`AuditEntry`] commits to the previous entry's hash, so
//!   altering, reordering, or removing any middle entry breaks [`AuditJournal::verify`]. The chain alone
//!   is not *unforgeable* — anyone can recompute a fresh valid chain — which is why it is paired with:
//! - **Host signature (authenticity).** The host signs a [`Checkpoint`] over the current chain head with
//!   its identity key (the `ras-identity` [`KeyStore`] seam). A verifier who trusts the host public key
//!   can then detect *any* rewrite: a forged chain has a different head, so the old signed checkpoint no
//!   longer matches and no valid new one can be produced without the host key.
//!
//! **Content-free (Inv 8 / 11).** [`AuditEvent`] carries only enums + counters — never a screen pixel,
//! keystroke, clipboard byte, typed text, file content, path, or secret. A `content` field is *absent by
//! construction*: there is nowhere to put one.
//!
//! The journal + chain + checkpoint are **pure** — no clock (the caller passes timestamps) and no I/O —
//! so they are fully unit-testable. Durable persistence is a thin, separate layer ([`AuditLog`]): an
//! append-only, length-prefixed record file, crash-safe (a partial trailing record is ignored) and
//! restart-survivable (reload → [`verify_chain`] + a signed [`Checkpoint`] detects any rewrite).

use ras_identity::{verify, KeyStore, PUBLIC_KEY_LEN, SIGNATURE_LEN};
use ras_protocol::{ErrorCode, RasError};
use sha2::{Digest, Sha256};

/// Milliseconds since the Unix epoch (host clock). The caller supplies it — this crate reads no clock.
pub type UnixMillis = u64;

/// A 32-byte SHA-256 digest (a chain link / head).
pub type Hash = [u8; 32];

/// Domain-separation tags so an audit hash/signature can never be confused with any other use of the
/// host key or SHA-256 in the system.
const CHAIN_DOMAIN: &[u8] = b"casual-ras/audit-chain/v1";
const CHECKPOINT_DOMAIN: &[u8] = b"casual-ras/audit-checkpoint/v1";

/// A security-sensitive event, **content-free** (Inv 8/11): only enum tags + counters, never content.
/// `#[non_exhaustive]` — new event kinds are additive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditEvent {
    /// The session was authorized and started.
    SessionStarted,
    /// The local user granted connection consent (Inv 1).
    ConsentGranted,
    /// The local user denied consent, or it timed out.
    ConsentDenied,
    /// A session grant was issued at this generation.
    GrantIssued { generation: u32 },
    /// The OS-input control lease was granted at this generation (Phase 3).
    ControlLeaseGranted { generation: u32 },
    /// The control lease ended / a control request was refused, with a reason code.
    ControlLeaseRevoked { code: ErrorCode },
    /// An inbound OS-input event was rejected by the per-message gate (Inv 15), with a reason code.
    InputRejected { code: ErrorCode },
    /// Emergency stop / mid-session revoke fired (Inv 4), with the reason code.
    EmergencyStop { code: ErrorCode },
    /// A clipboard push was applied — **byte length only**, never the text (Inv 8).
    ClipboardApplied { len: u32 },
    /// A clipboard push was refused, with a reason code.
    ClipboardRejected { code: ErrorCode },
    /// Output audio streaming started (`audio.listen`).
    AudioStarted,
    /// Output audio streaming stopped.
    AudioStopped,
    /// A catalogued file push was accepted (metadata only — never a filename/path).
    FilePushAccepted,
    /// A file push was refused, with a reason code.
    FilePushRejected { code: ErrorCode },
    /// The session ended, with the closure/revoke reason code.
    SessionEnded { code: ErrorCode },
}

impl AuditEvent {
    /// Append this event's canonical, deterministic encoding — a discriminant byte plus fixed fields.
    /// `ErrorCode` is encoded by its stable 2-byte numeric [`ErrorCode::to_code`], so the encoding is
    /// fixed-size, round-trippable ([`Self::decode`]), and independent of enum ordering.
    fn encode(self, buf: &mut Vec<u8>) {
        fn put_code(buf: &mut Vec<u8>, code: ErrorCode) {
            buf.extend_from_slice(&code.to_code().to_be_bytes());
        }
        match self {
            AuditEvent::SessionStarted => buf.push(0),
            AuditEvent::ConsentGranted => buf.push(1),
            AuditEvent::ConsentDenied => buf.push(2),
            AuditEvent::GrantIssued { generation } => {
                buf.push(3);
                buf.extend_from_slice(&generation.to_be_bytes());
            }
            AuditEvent::ControlLeaseGranted { generation } => {
                buf.push(4);
                buf.extend_from_slice(&generation.to_be_bytes());
            }
            AuditEvent::ControlLeaseRevoked { code } => {
                buf.push(5);
                put_code(buf, code);
            }
            AuditEvent::InputRejected { code } => {
                buf.push(6);
                put_code(buf, code);
            }
            AuditEvent::EmergencyStop { code } => {
                buf.push(7);
                put_code(buf, code);
            }
            AuditEvent::ClipboardApplied { len } => {
                buf.push(8);
                buf.extend_from_slice(&len.to_be_bytes());
            }
            AuditEvent::ClipboardRejected { code } => {
                buf.push(9);
                put_code(buf, code);
            }
            AuditEvent::AudioStarted => buf.push(10),
            AuditEvent::AudioStopped => buf.push(11),
            AuditEvent::FilePushAccepted => buf.push(12),
            AuditEvent::FilePushRejected { code } => {
                buf.push(13);
                put_code(buf, code);
            }
            AuditEvent::SessionEnded { code } => {
                buf.push(14);
                put_code(buf, code);
            }
        }
    }

    /// Parse an event from the front of `buf` (the inverse of [`Self::encode`]). Returns the event and
    /// the number of bytes consumed. `None` on an unknown discriminant, a truncated field, or an
    /// unrecognized `ErrorCode` (fail-closed — never defaulted).
    fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        let (&disc, rest) = buf.split_first()?;
        let u32_at = |b: &[u8]| {
            let a: [u8; 4] = b.get(..4)?.try_into().ok()?;
            Some(u32::from_be_bytes(a))
        };
        let code_at = |b: &[u8]| {
            let a: [u8; 2] = b.get(..2)?.try_into().ok()?;
            ErrorCode::from_code(u16::from_be_bytes(a))
        };
        // Fixed-size fields: a u32 adds 4 bytes, an ErrorCode 2 — plus the 1 discriminant byte.
        Some(match disc {
            0 => (AuditEvent::SessionStarted, 1),
            1 => (AuditEvent::ConsentGranted, 1),
            2 => (AuditEvent::ConsentDenied, 1),
            3 => (
                AuditEvent::GrantIssued {
                    generation: u32_at(rest)?,
                },
                5,
            ),
            4 => (
                AuditEvent::ControlLeaseGranted {
                    generation: u32_at(rest)?,
                },
                5,
            ),
            5 => (
                AuditEvent::ControlLeaseRevoked {
                    code: code_at(rest)?,
                },
                3,
            ),
            6 => (
                AuditEvent::InputRejected {
                    code: code_at(rest)?,
                },
                3,
            ),
            7 => (
                AuditEvent::EmergencyStop {
                    code: code_at(rest)?,
                },
                3,
            ),
            8 => (AuditEvent::ClipboardApplied { len: u32_at(rest)? }, 5),
            9 => (
                AuditEvent::ClipboardRejected {
                    code: code_at(rest)?,
                },
                3,
            ),
            10 => (AuditEvent::AudioStarted, 1),
            11 => (AuditEvent::AudioStopped, 1),
            12 => (AuditEvent::FilePushAccepted, 1),
            13 => (
                AuditEvent::FilePushRejected {
                    code: code_at(rest)?,
                },
                3,
            ),
            14 => (
                AuditEvent::SessionEnded {
                    code: code_at(rest)?,
                },
                3,
            ),
            _ => return None,
        })
    }
}

/// One journal entry, linked to its predecessor by [`Self::prev_hash`] and committing to itself in
/// [`Self::entry_hash`] (which becomes the next entry's `prev_hash`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// Monotonic index within the session (0-based).
    pub seq: u64,
    /// Caller-supplied timestamp (host clock, ms).
    pub timestamp: UnixMillis,
    /// The previous entry's `entry_hash` (or the journal's genesis hash for `seq == 0`).
    pub prev_hash: Hash,
    /// The recorded event (content-free).
    pub event: AuditEvent,
    /// `SHA-256(CHAIN_DOMAIN || seq || prev_hash || timestamp || event)` — the chain link.
    pub entry_hash: Hash,
}

/// Compute an entry's hash over its canonical bytes. Pure/deterministic.
fn hash_entry(seq: u64, prev_hash: &Hash, timestamp: UnixMillis, event: AuditEvent) -> Hash {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(prev_hash);
    buf.extend_from_slice(&timestamp.to_be_bytes());
    event.encode(&mut buf);
    let mut h = Sha256::new();
    h.update(CHAIN_DOMAIN);
    h.update(&buf);
    h.finalize().into()
}

/// The genesis hash for a session — binds the whole chain to the session id (a chain from another
/// session can't be spliced in, because its `seq == 0` entry commits to a different genesis).
#[must_use]
pub fn genesis_hash(session_id: &[u8; 16]) -> Hash {
    let mut h = Sha256::new();
    h.update(CHAIN_DOMAIN);
    h.update(b"genesis");
    h.update(session_id);
    h.finalize().into()
}

/// Why a journal failed [`AuditJournal::verify`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditError {
    /// The entry at this `seq` is inconsistent — its `prev_hash` doesn't match the running head, its
    /// `seq` is out of order, or its `entry_hash` doesn't match its recomputed contents (tamper /
    /// reorder / truncation-in-the-middle).
    ChainBroken { seq: u64 },
}

/// An append-only, hash-chained audit journal for one session (Inv 10). Sign a [`Checkpoint`] over its
/// head to make rewrites detectable by anyone holding the host public key.
#[derive(Clone, Debug)]
pub struct AuditJournal {
    session_id: [u8; 16],
    entries: Vec<AuditEntry>,
    head: Hash,
}

impl AuditJournal {
    /// A fresh, empty journal for `session_id`. The head starts at the session genesis hash.
    #[must_use]
    pub fn new(session_id: [u8; 16]) -> Self {
        Self {
            head: genesis_hash(&session_id),
            session_id,
            entries: Vec::new(),
        }
    }

    /// Append an event at `timestamp`, extending the chain. Returns the new entry. Append-only: there is
    /// no API to edit or remove an entry.
    pub fn append(&mut self, event: AuditEvent, timestamp: UnixMillis) -> &AuditEntry {
        let seq = self.entries.len() as u64;
        let prev_hash = self.head;
        let entry_hash = hash_entry(seq, &prev_hash, timestamp, event);
        self.head = entry_hash;
        self.entries.push(AuditEntry {
            seq,
            timestamp,
            prev_hash,
            event,
            entry_hash,
        });
        // Just pushed, so `last` is present.
        self.entries.last().unwrap_or_else(|| unreachable!())
    }

    /// The current chain head (the last entry's hash, or the genesis hash if empty).
    #[must_use]
    pub fn head(&self) -> Hash {
        self.head
    }

    /// The session id this journal is bound to.
    #[must_use]
    pub fn session_id(&self) -> &[u8; 16] {
        &self.session_id
    }

    /// The recorded entries in order.
    #[must_use]
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Recompute the chain from genesis and verify every link. Detects any content tamper, reorder, or
    /// removal of a middle entry (a truncation of the *tail* verifies structurally — that is what the
    /// signed [`Checkpoint`] over the head is for).
    ///
    /// # Errors
    /// [`AuditError::ChainBroken`] at the first inconsistent `seq`.
    pub fn verify(&self) -> Result<(), AuditError> {
        verify_chain(&self.session_id, &self.entries)
    }

    /// Sign a [`Checkpoint`] over the current head with the host key (authenticity, Inv 10).
    ///
    /// # Errors
    /// Propagates a [`KeyStore`] signing failure.
    pub fn checkpoint<K: KeyStore>(&self, keystore: &K) -> Result<Checkpoint, RasError> {
        let seq = self.entries.len() as u64;
        let msg = checkpoint_message(&self.session_id, seq, &self.head);
        let signature = keystore.sign(&msg)?;
        Ok(Checkpoint {
            session_id: self.session_id,
            entry_count: seq,
            head_hash: self.head,
            signer: keystore.public_key(),
            signature,
        })
    }
}

/// Verify a standalone entry list against a session's genesis (e.g. loaded from a store).
///
/// # Errors
/// [`AuditError::ChainBroken`] at the first inconsistent `seq`.
pub fn verify_chain(session_id: &[u8; 16], entries: &[AuditEntry]) -> Result<(), AuditError> {
    let mut head = genesis_hash(session_id);
    for (i, e) in entries.iter().enumerate() {
        let expected_seq = i as u64;
        if e.seq != expected_seq || e.prev_hash != head {
            return Err(AuditError::ChainBroken { seq: expected_seq });
        }
        let recomputed = hash_entry(e.seq, &e.prev_hash, e.timestamp, e.event);
        if recomputed != e.entry_hash {
            return Err(AuditError::ChainBroken { seq: expected_seq });
        }
        head = e.entry_hash;
    }
    Ok(())
}

/// A host-signed commitment to a journal's head at a point in time (Inv 10). Anyone holding the host
/// public key can verify it; a rewritten journal produces a different head, so its old checkpoint no
/// longer verifies and a new valid one cannot be forged without the host key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The session this checkpoint is for.
    pub session_id: [u8; 16],
    /// How many entries the head covers.
    pub entry_count: u64,
    /// The committed chain head.
    pub head_hash: Hash,
    /// The signing host's public identity.
    pub signer: [u8; PUBLIC_KEY_LEN],
    /// Ed25519 signature over the canonical checkpoint message.
    pub signature: [u8; SIGNATURE_LEN],
}

impl Checkpoint {
    /// Verify this checkpoint's signature against its embedded signer, **and** that it commits to
    /// `expected_head` for `session_id`. Fail-closed: any mismatch returns `false`.
    #[must_use]
    pub fn verify(&self, session_id: &[u8; 16], expected_head: &Hash) -> bool {
        if &self.session_id != session_id || &self.head_hash != expected_head {
            return false;
        }
        let msg = checkpoint_message(&self.session_id, self.entry_count, &self.head_hash);
        verify(&self.signer, &msg, &self.signature).is_ok()
    }
}

/// The canonical bytes a [`Checkpoint`] signs over.
fn checkpoint_message(session_id: &[u8; 16], entry_count: u64, head_hash: &Hash) -> Vec<u8> {
    let mut msg = Vec::with_capacity(CHECKPOINT_DOMAIN.len() + 16 + 8 + 32);
    msg.extend_from_slice(CHECKPOINT_DOMAIN);
    msg.extend_from_slice(session_id);
    msg.extend_from_slice(&entry_count.to_be_bytes());
    msg.extend_from_slice(head_hash);
    msg
}

// ── Durable persistence: an append-only record log (Inv 10) ───────────────────────────────────────
//
// The journal is pure in-memory above; this is the durable layer. Records are length-prefixed and
// **append-only**; a crash mid-write leaves an incomplete trailing record that [`AuditLog::load`]
// stops at (never corrupting the valid prefix). Loaded entries are validated with [`verify_chain`],
// and a signed [`Checkpoint`] over the head makes any *rewrite* of the file detectable.

/// Fixed part of a serialized entry: `seq(8) ‖ timestamp(8) ‖ prev_hash(32) ‖ entry_hash(32)`, then the
/// variable event encoding.
const ENTRY_HEADER_LEN: usize = 8 + 8 + 32 + 32;
/// Upper bound on one serialized record (a real record is ~85 bytes); guards `load` against a corrupt
/// length prefix.
const MAX_RECORD_LEN: usize = 4096;

impl AuditEntry {
    /// Serialize to a self-contained record (the inverse is [`Self::from_record`]).
    fn to_record(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(ENTRY_HEADER_LEN + 8);
        v.extend_from_slice(&self.seq.to_be_bytes());
        v.extend_from_slice(&self.timestamp.to_be_bytes());
        v.extend_from_slice(&self.prev_hash);
        v.extend_from_slice(&self.entry_hash);
        self.event.encode(&mut v);
        v
    }

    /// Parse a record produced by [`Self::to_record`]. `None` on a short/oversized buffer, an
    /// undecodable event, or trailing garbage (the record must be exactly header + event bytes).
    fn from_record(buf: &[u8]) -> Option<Self> {
        if buf.len() < ENTRY_HEADER_LEN {
            return None;
        }
        let seq = u64::from_be_bytes(buf[0..8].try_into().ok()?);
        let timestamp = u64::from_be_bytes(buf[8..16].try_into().ok()?);
        let prev_hash: Hash = buf[16..48].try_into().ok()?;
        let entry_hash: Hash = buf[48..ENTRY_HEADER_LEN].try_into().ok()?;
        let (event, consumed) = AuditEvent::decode(&buf[ENTRY_HEADER_LEN..])?;
        if ENTRY_HEADER_LEN + consumed != buf.len() {
            return None; // no trailing bytes — a record is exact
        }
        Some(AuditEntry {
            seq,
            timestamp,
            prev_hash,
            event,
            entry_hash,
        })
    }
}

/// An append-only, file-backed audit record log (Inv 10 durable store). Each [`AuditEntry`] is written
/// as a `u32` big-endian length prefix followed by its record bytes. Restart-survivable: reload with
/// [`Self::load`] and validate with [`verify_chain`] + a signed [`Checkpoint`].
#[derive(Clone, Debug)]
pub struct AuditLog {
    path: std::path::PathBuf,
}

impl AuditLog {
    /// A log backed by `path` (created on first [`Self::append`]; not opened until then).
    #[must_use]
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Append one entry durably (open-append, write length-prefixed record, flush). Append-only — never
    /// rewrites earlier bytes.
    ///
    /// # Errors
    /// Any filesystem error opening/writing/flushing the log.
    pub fn append(&self, entry: &AuditEntry) -> std::io::Result<()> {
        use std::io::Write as _;
        let record = entry.to_record();
        let len = u32::try_from(record.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "audit record too large")
        })?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(&len.to_be_bytes())?;
        f.write_all(&record)?;
        f.flush()
    }

    /// Load every complete record in order. A partial/corrupt **trailing** record (e.g. a crash during
    /// append) ends the read at the last valid entry — it never fails the whole load or corrupts the
    /// valid prefix. Middle tampering is *not* silently accepted: the loaded entries still verify via
    /// [`verify_chain`], which breaks on any altered link. A missing file loads as empty.
    ///
    /// # Errors
    /// A filesystem read error other than "not found".
    pub fn load(&self) -> std::io::Result<Vec<AuditEntry>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 4 <= bytes.len() {
            // `pos + 4 <= len` is guaranteed by the loop condition, so index directly (no fallible cast).
            let len =
                u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                    as usize;
            if len == 0 || len > MAX_RECORD_LEN || pos + 4 + len > bytes.len() {
                break; // corrupt length or truncated trailing record — stop at the valid prefix
            }
            let Some(entry) = AuditEntry::from_record(&bytes[pos + 4..pos + 4 + len]) else {
                break; // undecodable trailing record — stop
            };
            out.push(entry);
            pos += 4 + len;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_identity::SoftwareKeyStore;

    const SID: [u8; 16] = [0xAB; 16];

    fn populated() -> AuditJournal {
        let mut j = AuditJournal::new(SID);
        j.append(AuditEvent::SessionStarted, 1000);
        j.append(AuditEvent::ConsentGranted, 1001);
        j.append(AuditEvent::GrantIssued { generation: 1 }, 1002);
        j.append(AuditEvent::ControlLeaseGranted { generation: 1 }, 1003);
        j.append(
            AuditEvent::InputRejected {
                code: ErrorCode::ReplayDetected,
            },
            1004,
        );
        j.append(
            AuditEvent::EmergencyStop {
                code: ErrorCode::SessionRevoked,
            },
            1005,
        );
        j.append(
            AuditEvent::SessionEnded {
                code: ErrorCode::SessionRevoked,
            },
            1006,
        );
        j
    }

    #[test]
    fn chain_links_and_verifies() {
        let j = populated();
        assert_eq!(j.len(), 7);
        assert!(j.verify().is_ok());
        // Each entry's prev_hash is the previous entry's entry_hash; seq is monotonic from 0.
        assert_eq!(j.entries()[0].prev_hash, genesis_hash(&SID));
        for w in j.entries().windows(2) {
            assert_eq!(w[1].prev_hash, w[0].entry_hash);
            assert_eq!(w[1].seq, w[0].seq + 1);
        }
        assert_eq!(j.head(), j.entries().last().unwrap().entry_hash);
    }

    #[test]
    fn append_is_deterministic() {
        assert_eq!(populated().head(), populated().head());
        // A different session id → a different chain (genesis binds it).
        let mut other = AuditJournal::new([0x01; 16]);
        other.append(AuditEvent::SessionStarted, 1000);
        let mut same = AuditJournal::new(SID);
        same.append(AuditEvent::SessionStarted, 1000);
        assert_ne!(other.head(), same.head());
    }

    #[test]
    fn tampering_content_breaks_the_chain() {
        let mut e = populated().entries().to_vec();
        // Flip the recorded event of a middle entry, keep its (now-stale) entry_hash → detected.
        e[3].event = AuditEvent::AudioStarted;
        assert_eq!(
            verify_chain(&SID, &e),
            Err(AuditError::ChainBroken { seq: 3 })
        );
    }

    #[test]
    fn reordering_breaks_the_chain() {
        let mut e = populated().entries().to_vec();
        e.swap(2, 4);
        assert!(verify_chain(&SID, &e).is_err());
    }

    #[test]
    fn removing_a_middle_entry_breaks_the_chain() {
        let mut e = populated().entries().to_vec();
        e.remove(3); // the next entry's prev_hash + seq no longer line up
        assert_eq!(
            verify_chain(&SID, &e),
            Err(AuditError::ChainBroken { seq: 3 })
        );
    }

    #[test]
    fn signed_checkpoint_round_trips_and_catches_rewrites() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let j = populated();
        let cp = j.checkpoint(&ks).unwrap();
        // Verifies against the genuine head + signer.
        assert!(cp.verify(&SID, &j.head()));
        assert_eq!(cp.entry_count, 7);

        // A rewritten journal has a different head, so the old checkpoint no longer matches it…
        let mut forged = AuditJournal::new(SID);
        forged.append(AuditEvent::SessionStarted, 1000); // attacker's shorter, "clean" history
        assert!(!cp.verify(&SID, &forged.head()));

        // …and a checkpoint the attacker signs with their OWN key doesn't verify as the host.
        let attacker = SoftwareKeyStore::generate().unwrap();
        let forged_cp = forged.checkpoint(&attacker).unwrap();
        // It self-verifies (attacker key over attacker head)…
        assert!(forged_cp.verify(&SID, &forged.head()));
        // …but is not the host's key, so a verifier pinning the host key rejects it.
        assert_ne!(forged_cp.signer, ks.public_key());

        // A tampered head with the genuine signature fails (signature covers the head).
        let mut bad = cp.clone();
        bad.head_hash[0] ^= 0xFF;
        assert!(!bad.verify(&SID, &bad.head_hash));
        // Wrong session id fails too.
        assert!(!cp.verify(&[0x00; 16], &j.head()));
    }

    #[test]
    fn empty_journal_verifies_and_checkpoints() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let j = AuditJournal::new(SID);
        assert!(j.is_empty());
        assert!(j.verify().is_ok());
        assert_eq!(j.head(), genesis_hash(&SID));
        let cp = j.checkpoint(&ks).unwrap();
        assert!(cp.verify(&SID, &genesis_hash(&SID)));
        assert_eq!(cp.entry_count, 0);
    }

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ras-audit-{}-{}", std::process::id(), tag))
    }

    #[test]
    fn event_encode_decode_round_trips_every_variant() {
        let events = [
            AuditEvent::SessionStarted,
            AuditEvent::ConsentGranted,
            AuditEvent::ConsentDenied,
            AuditEvent::GrantIssued { generation: 7 },
            AuditEvent::ControlLeaseGranted { generation: 42 },
            AuditEvent::ControlLeaseRevoked {
                code: ErrorCode::CapabilityDenied,
            },
            AuditEvent::InputRejected {
                code: ErrorCode::ReplayDetected,
            },
            AuditEvent::EmergencyStop {
                code: ErrorCode::SessionRevoked,
            },
            AuditEvent::ClipboardApplied { len: 12_345 },
            AuditEvent::ClipboardRejected {
                code: ErrorCode::CapabilityDenied,
            },
            AuditEvent::AudioStarted,
            AuditEvent::AudioStopped,
            AuditEvent::FilePushAccepted,
            AuditEvent::FilePushRejected {
                code: ErrorCode::InvalidMessage,
            },
            AuditEvent::SessionEnded {
                code: ErrorCode::NormalClosure,
            },
        ];
        for ev in events {
            let mut buf = Vec::new();
            ev.encode(&mut buf);
            let (decoded, consumed) = AuditEvent::decode(&buf).expect("decodes");
            assert_eq!(decoded, ev);
            assert_eq!(consumed, buf.len(), "consumes exactly its own bytes");
        }
        // Fail-closed: unknown discriminant, truncated field, empty.
        assert!(AuditEvent::decode(&[0xFF]).is_none());
        assert!(AuditEvent::decode(&[3, 0, 0]).is_none()); // GrantIssued needs a full u32
        assert!(AuditEvent::decode(&[]).is_none());
    }

    #[test]
    fn file_log_persists_reloads_and_verifies() {
        let path = tmp_path("persist");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::new(&path);
        let j = populated();
        for e in j.entries() {
            log.append(e).unwrap();
        }
        let loaded = log.load().unwrap();
        assert_eq!(
            loaded,
            j.entries(),
            "reloaded entries match the journal exactly"
        );
        assert!(
            verify_chain(&SID, &loaded).is_ok(),
            "the reloaded chain verifies"
        );
        // A signed checkpoint over the original head still matches the reloaded head → no rewrite.
        let ks = SoftwareKeyStore::generate().unwrap();
        let cp = j.checkpoint(&ks).unwrap();
        assert!(cp.verify(&SID, &loaded.last().unwrap().entry_hash));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_log_tolerates_a_truncated_trailing_record() {
        let path = tmp_path("truncated");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::new(&path);
        let j = populated();
        for e in j.entries() {
            log.append(e).unwrap();
        }
        // Chop the last few bytes (a crash mid-append): load returns the intact prefix, not an error.
        let full = std::fs::read(&path).unwrap();
        std::fs::write(&path, &full[..full.len() - 3]).unwrap();
        let loaded = log.load().unwrap();
        assert!(
            loaded.len() == j.len() - 1 && !loaded.is_empty(),
            "only the torn trailing record is dropped"
        );
        assert!(
            verify_chain(&SID, &loaded).is_ok(),
            "the surviving prefix still verifies"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_log_middle_tamper_is_caught_by_verify_chain() {
        let path = tmp_path("tamper");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::new(&path);
        let j = populated();
        for e in j.entries() {
            log.append(e).unwrap();
        }
        // Swap the first record's event to a *different but same-length* variant, leaving its committed
        // entry_hash stale: it still parses, but the recomputed hash no longer matches → chain breaks.
        let mut bytes = std::fs::read(&path).unwrap();
        let ev_byte = 4 + ENTRY_HEADER_LEN; // u32 len + entry header → the event discriminant
        assert_eq!(bytes[ev_byte], 0, "first event is SessionStarted (disc 0)");
        bytes[ev_byte] = 2; // → ConsentDenied, also a 1-byte event
        std::fs::write(&path, &bytes).unwrap();
        let loaded = log.load().unwrap();
        assert_eq!(
            verify_chain(&SID, &loaded),
            Err(AuditError::ChainBroken { seq: 0 }),
            "a tampered committed entry must break the chain"
        );
        let _ = std::fs::remove_file(&path);
    }
}
