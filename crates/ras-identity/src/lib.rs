//! Ed25519 application identities + platform key storage for Casual RAS (Phase 2).
//!
//! An **application identity** is a stable Ed25519 key — distinct from the iroh *endpoint* identity
//! the transport authenticates per connection (Inv 9). `HostId`/`ControllerId` are the public halves
//! (safe to share, never secret). The private key lives behind a [`KeyStore`]: the trait exposes
//! **sign + public material only** — never the secret (Inv 8) — and bounds the assurance tier a
//! deployment may advertise (Inv 16: software storage caps at Tier 0).
//!
//! The Ed25519 primitive is `ed25519-dalek` (already vendored via iroh), **confined to this crate**
//! (ADR-065): no dalek type crosses the API — callers see only raw `[u8; 32]` keys and `[u8; 64]`
//! signatures — so a libsodium / TPM / Secure-Enclave store is a drop-in `KeyStore` impl later.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use ras_protocol::{ErrorCode, RasError};

/// Identity errors reuse the shared taxonomy (no parallel enum).
pub type IdentityError = RasError;

/// Ed25519 public-key length.
pub const PUBLIC_KEY_LEN: usize = 32;
/// Ed25519 signature length.
pub const SIGNATURE_LEN: usize = 64;
/// Ed25519 secret-key seed length.
const SECRET_KEY_LEN: usize = 32;

fn sig_invalid() -> IdentityError {
    RasError::fatal(ErrorCode::SignatureInvalid, "ed25519 verification failed")
}
fn internal(ctx: &'static str) -> IdentityError {
    RasError::fatal(ErrorCode::Internal, ctx)
}

/// Define a public-identity newtype over a 32-byte Ed25519 key. Distinct types for host vs controller
/// so one can never be passed where the other is expected. Hex `Display`/`Debug` (public, not secret).
macro_rules! public_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name([u8; PUBLIC_KEY_LEN]);

        impl $name {
            /// Wrap raw public-key bytes.
            #[must_use]
            pub const fn from_bytes(bytes: [u8; PUBLIC_KEY_LEN]) -> Self {
                Self(bytes)
            }
            /// The raw public-key bytes.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // A short hex prefix is enough to identify a key in a UI/log; it is public anyway.
                for b in &self.0[..4] {
                    write!(f, "{b:02x}")?;
                }
                write!(f, "…")
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self)
            }
        }
    };
}

public_id!(
    HostId,
    "A host's stable public identity (Ed25519 public key)."
);
public_id!(
    ControllerId,
    "A controller's stable public identity (Ed25519 public key)."
);

/// Assurance tier a deployment may advertise (`docs/16`). Bounded by the key store (Inv 16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum AssuranceTier {
    /// Software key storage. The only tier a software store may advertise.
    Tier0,
    /// TPM / Keychain-sealed, attested.
    Tier1,
    /// Hardware (FIDO2) per session.
    Tier2,
    /// Vaulted, JIT, control-plane attested.
    Tier3,
}

/// Evidence that a private key is hardware-backed + non-exportable (TPM / Secure Enclave / FIDO2).
/// A software store returns `None`, so the deployment is capped at Tier 0 (Inv 16). The evidence is
/// verified by the enrollment / attestation path (later), not here.
#[derive(Clone)]
#[non_exhaustive]
pub struct KeyAttestation {
    /// Opaque platform attestation blob.
    pub evidence: Vec<u8>,
}
impl fmt::Debug for KeyAttestation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyAttestation({} bytes)", self.evidence.len())
    }
}

/// Where a private signing key lives. **Sign + public material only** — no secret-export path
/// (Inv 8). The advertisable tier is bounded by [`KeyStore::tier_ceiling`] (Inv 16).
pub trait KeyStore: Send + Sync {
    /// Sign `msg`; the key never leaves the store. Returns a raw 64-byte Ed25519 signature.
    fn sign(&self, msg: &[u8]) -> Result<[u8; SIGNATURE_LEN], IdentityError>;
    /// This store's public key (safe to share).
    fn public_key(&self) -> [u8; PUBLIC_KEY_LEN];
    /// Hardware attestation, if the platform can prove non-exportable hardware storage.
    fn attestation(&self) -> Option<KeyAttestation> {
        None
    }
    /// The highest tier this store may advertise (Inv 16). Software storage → `Tier0`.
    fn tier_ceiling(&self) -> AssuranceTier;
}

/// A software Ed25519 key store (Tier 0). Optionally persisted as 32 raw secret bytes at a path
/// (mode `0600` on unix). **Non-exporting**: there is no API to read the secret back out, and `Debug`
/// is redacted (Inv 8). Not hardware-attested, so it can never advertise Tier ≥1 (Inv 16).
pub struct SoftwareKeyStore {
    signing: SigningKey,
}

impl SoftwareKeyStore {
    /// Build a store from a known 32-byte Ed25519 seed. This is a key **import**, never an export
    /// (Inv 8): there is still no path to read the secret back out. Used to load a persisted identity
    /// and to drive standard test vectors; it does not weaken the non-exporting guarantee.
    #[must_use]
    pub fn from_seed(seed: [u8; SECRET_KEY_LEN]) -> Self {
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// A fresh in-memory key (ephemeral; not persisted). Used for tests and one-off sessions.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut secret = [0u8; SECRET_KEY_LEN];
        getrandom::getrandom(&mut secret).map_err(|_| internal("csprng unavailable"))?;
        let store = Self {
            signing: SigningKey::from_bytes(&secret),
        };
        secret.fill(0); // best-effort scrub of the local copy
        Ok(store)
    }

    /// Load the key persisted at `path`, or generate + persist one if the file is absent.
    /// Fails closed (`Internal`) on a malformed file — never silently regenerates over bad data.
    pub fn load_or_create(path: &Path) -> Result<Self, IdentityError> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let secret: [u8; SECRET_KEY_LEN] = bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| internal("malformed key file"))?;
                Ok(Self {
                    signing: SigningKey::from_bytes(&secret),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut secret = [0u8; SECRET_KEY_LEN];
                getrandom::getrandom(&mut secret).map_err(|_| internal("csprng unavailable"))?;
                write_secret(path, &secret)?;
                let store = Self {
                    signing: SigningKey::from_bytes(&secret),
                };
                secret.fill(0);
                Ok(store)
            }
            Err(_) => Err(internal("key file unreadable")),
        }
    }

    /// This store's host identity.
    #[must_use]
    pub fn host_id(&self) -> HostId {
        HostId::from_bytes(self.public_key())
    }

    /// This store's controller identity.
    #[must_use]
    pub fn controller_id(&self) -> ControllerId {
        ControllerId::from_bytes(self.public_key())
    }
}

/// Write 32 secret bytes to `path`, `0600` on unix (owner-only). Best-effort perms elsewhere.
fn write_secret(path: &Path, secret: &[u8; SECRET_KEY_LEN]) -> Result<(), IdentityError> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| internal("cannot create key file"))?;
        f.write_all(secret)
            .map_err(|_| internal("cannot write key file"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, secret).map_err(|_| internal("cannot write key file"))
    }
}

impl fmt::Debug for SoftwareKeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render the secret. The public key identifies the store safely.
        write!(
            f,
            "SoftwareKeyStore(pub={})",
            HostId::from_bytes(self.public_key())
        )
    }
}

impl KeyStore for SoftwareKeyStore {
    fn sign(&self, msg: &[u8]) -> Result<[u8; SIGNATURE_LEN], IdentityError> {
        Ok(self.signing.sign(msg).to_bytes())
    }
    fn public_key(&self) -> [u8; PUBLIC_KEY_LEN] {
        self.signing.verifying_key().to_bytes()
    }
    fn tier_ceiling(&self) -> AssuranceTier {
        AssuranceTier::Tier0 // software storage — never advertises Tier ≥1 (Inv 16)
    }
}

/// Verify a 64-byte signature over `msg` by the identity `pubkey`. **Strict** (`verify_strict`
/// rejects small-order / malleable variants). Any failure → `SignatureInvalid` — no distinction is
/// leaked about *why* it failed.
pub fn verify(
    pubkey: &[u8; PUBLIC_KEY_LEN],
    msg: &[u8],
    sig: &[u8; SIGNATURE_LEN],
) -> Result<(), IdentityError> {
    let vk = VerifyingKey::from_bytes(pubkey).map_err(|_| sig_invalid())?;
    let signature = Signature::from_bytes(sig);
    vk.verify_strict(msg, &signature).map_err(|_| sig_invalid())
}

/// Milliseconds since the Unix epoch (host clock). Callers pass the time in — this crate stays
/// **pure** (no clock read), so tests are deterministic. Aliased locally to stay dependency-light.
pub type UnixMillis = u64;

/// A controller the local user has paired (`docs/16 §11`, ADR-084 / §3.5). Persisted host-side after a
/// first attended, consented pairing so future sessions from the same key skip re-pairing — but they
/// **still mint a fresh, short-lived grant** (Inv 3) and **still honor emergency stop** (Inv 4). The
/// registry authenticates *identity*, never confers *authority* (Inv 9); a lookup can never authorize
/// input on its own. Content-light — a public key + a user label + timestamps; no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedController {
    /// The controller's stable public identity (the allow-list key).
    pub id: ControllerId,
    /// A human label the local user set at pairing (e.g. "Alice's laptop"). Never security-bearing.
    pub label: String,
    /// When the controller was first paired (host clock).
    pub first_paired_at: UnixMillis,
    /// When it was last seen connecting (host clock); refreshed per session for the UI.
    pub last_seen_at: UnixMillis,
}

/// Crockford-base32 rendering of a controller's public identity, grouped into 4-char blocks for human
/// comparison — the pairing code shown **alongside** the QR (host displays, controller scans; the QR
/// carries the machine-readable key, this is the eyeball/verbal check). Deterministic; the Crockford
/// alphabet omits `I L O U` so it survives readback without typos. No hash is applied: `ControllerId`
/// is already a 256-bit uniform Ed25519 key, so rendering it directly *is* the Syncthing-style device
/// id (Syncthing digests only because its input is a large certificate).
#[must_use]
pub fn pairing_code(id: &ControllerId) -> String {
    crockford_code(id.as_bytes())
}

/// The same grouped Crockford-base32 rendering for a saved [`Contact`]'s identity (ADR-092): the
/// eyeball/verbal check shown alongside the QR when adding a contact. Identical algorithm to
/// [`pairing_code`] — a contact and a paired controller share the 32-byte Ed25519 key space.
#[must_use]
pub fn contact_code(id: &ContactId) -> String {
    crockford_code(id.as_bytes())
}

/// Grouped Crockford-base32 of a 32-byte public identity (the shared body of [`pairing_code`] /
/// [`contact_code`]). Deterministic; the alphabet omits `I L O U` so it survives readback without
/// typos; no hash is applied (the input is already a uniform 256-bit key).
fn crockford_code(bytes: &[u8; PUBLIC_KEY_LEN]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut out = String::with_capacity(52 + 12);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    let mut symbols: usize = 0;
    let push = |idx: usize, out: &mut String, symbols: &mut usize| {
        if *symbols > 0 && symbols.is_multiple_of(4) {
            out.push('-');
        }
        out.push(ALPHABET[idx] as char);
        *symbols += 1;
    };
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        nbits += 8;
        while nbits >= 5 {
            nbits -= 5;
            push(((acc >> nbits) & 0x1f) as usize, &mut out, &mut symbols);
        }
        acc &= (1u32 << nbits) - 1; // keep only the ≤4 leftover bits — no accumulation/overflow
    }
    if nbits > 0 {
        push(
            ((acc << (5 - nbits)) & 0x1f) as usize,
            &mut out,
            &mut symbols,
        );
    }
    out
}

/// What the pairing layer should do when a controller connects (ADR-084 / §3.5). It governs the
/// **pairing prompt only** — never authority: a [`Self::SkipPairingPrompt`] controller **still** goes
/// through fresh grant issuance, per-message capability enforcement, and emergency stop (Inv 3/4/9). A
/// registry lookup cannot, by itself, authorize anything — hence this enum carries no capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairingDecision {
    /// Known controller: skip the attended pairing accept — but issue a fresh grant as always.
    SkipPairingPrompt,
    /// Unknown controller: require the local user's explicit pairing accept first (Inv 1).
    RequirePairingPrompt,
}

/// Decide the pairing step from a registry membership check. Pure — the caller consults its
/// [`PairingRegistry`] and passes the result. Deliberately trivial and authority-free: it only decides
/// whether the *human pairing prompt* is shown, so it can never be a place authority leaks in.
#[must_use]
pub fn pairing_decision(is_paired: bool) -> PairingDecision {
    if is_paired {
        PairingDecision::SkipPairingPrompt
    } else {
        PairingDecision::RequirePairingPrompt
    }
}

/// The paired-controller registry (`docs/16 §11`, ADR-084). The local user owns the list (Inv 1);
/// **de-listing is a kill-switch** — removing a key revokes its skip-pairing standing (future sessions
/// require a fresh attended accept). MVP is in-memory; a SQLite-backed impl (restart-survival) is the
/// durable follow-up.
pub trait PairingRegistry: Send + Sync {
    /// Whether this controller is paired (and not since revoked).
    fn is_paired(&self, id: &ControllerId) -> bool;
    /// Record/refresh a pairing (after a human accept). For an existing id this refreshes the label +
    /// `last_seen_at` but **preserves the original `first_paired_at`** (a re-pair is not a new pairing).
    fn pair(&self, controller: PairedController);
    /// Look up a paired controller's full record.
    fn get(&self, id: &ControllerId) -> Option<PairedController>;
    /// All paired controllers, for the host's management UI. Order is unspecified.
    fn list(&self) -> Vec<PairedController>;
    /// Refresh `last_seen_at` for a paired controller (no-op if unknown).
    fn touch(&self, id: &ControllerId, now: UnixMillis);
    /// De-list a controller (kill-switch). Idempotent.
    fn revoke(&self, id: &ControllerId);
}

/// In-memory [`PairingRegistry`] for the attended MVP. A host restart clears it; durable pairing is the
/// SQLite impl (later).
#[derive(Default)]
pub struct InMemoryPairingRegistry {
    inner: Mutex<HashMap<ControllerId, PairedController>>,
}

impl InMemoryPairingRegistry {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ControllerId, PairedController>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PairingRegistry for InMemoryPairingRegistry {
    fn is_paired(&self, id: &ControllerId) -> bool {
        self.lock().contains_key(id)
    }
    fn pair(&self, controller: PairedController) {
        let mut g = self.lock();
        if let Some(existing) = g.get_mut(&controller.id) {
            existing.label = controller.label;
            existing.last_seen_at = controller.last_seen_at;
            // first_paired_at is preserved — a re-pair does not reset the pairing age.
        } else {
            g.insert(controller.id, controller);
        }
    }
    fn get(&self, id: &ControllerId) -> Option<PairedController> {
        self.lock().get(id).cloned()
    }
    fn list(&self) -> Vec<PairedController> {
        self.lock().values().cloned().collect()
    }
    fn touch(&self, id: &ControllerId, now: UnixMillis) {
        if let Some(c) = self.lock().get_mut(id) {
            c.last_seen_at = now;
        }
    }
    fn revoke(&self, id: &ControllerId) {
        self.lock().remove(id);
    }
}

// ─── Contacts: the bidirectional address book (ADR-092 / `docs/20 §3.5`) ────────────────────────────

public_id!(
    ContactId,
    "A saved peer's stable public identity (Ed25519 public key) — also its iroh `EndpointId`, so it is directly dialable (ADR-093)."
);

/// A saved peer in the local user's **address book** (ADR-092). Unlike [`PairedController`] (host-side:
/// controllers that connect *to* this machine), a `Contact` is **bidirectional** — any peer you have
/// mutually saved and can reach **by identity, not by ticket** (ADR-093): to view/share, message, or
/// request remote access from. The `id` is the peer's Ed25519 public key, which is also its iroh
/// `EndpointId`, so it is directly dialable. Saving a contact confers **no authority** (Inv 1/9): every
/// session it initiates or receives still runs full local consent + a fresh short-lived grant + the
/// per-message gate + emergency stop. Content-light — a public key + a user label + timestamps + a
/// block flag; no secret (Inv 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    /// The peer's stable public identity (= its iroh `EndpointId`, the dial key).
    pub id: ContactId,
    /// A human label the local user set (e.g. "Alice's laptop"). Never security-bearing.
    pub label: String,
    /// When the contact was first added (local clock).
    pub added_at: UnixMillis,
    /// When the contact was last seen online / connecting (local clock); refreshed for the UI.
    pub last_seen_at: UnixMillis,
    /// Whether the local user has **blocked** this contact. A blocked contact can neither be reached nor
    /// deliver a message / access-request (the deny-by-default abuse control, ADR-094/095). Kept in the
    /// book rather than deleted so a block survives a re-add until explicitly unblocked or removed.
    pub blocked: bool,
}

/// The local user's contacts book (ADR-092). The user owns it (Inv 1); **blocking or removing a contact
/// is the kill-switch**. Every method is identity-only — the book decides *who you can find/hear from*,
/// **never** what they may do (Inv 9): even an active contact goes through full consent + a fresh grant.
/// MVP is in-memory; a SQLite-backed impl (restart-survival) is the durable follow-up.
pub trait ContactBook: Send + Sync {
    /// Add or update a contact (after a human accept at pairing). For an existing id this refreshes the
    /// label + `last_seen_at` but **preserves the original `added_at` and the `blocked` flag** — a re-add
    /// is not a new add and must never silently unblock (Inv 1).
    fn upsert(&self, contact: Contact);
    /// Look up a contact by identity.
    fn get(&self, id: &ContactId) -> Option<Contact>;
    /// Whether `id` is a saved, **non-blocked** contact — the deny-by-default gate for reachability,
    /// messages, and access-requests (ADR-094/095, contacts-only). A blocked or unknown id ⇒ `false`.
    fn is_active_contact(&self, id: &ContactId) -> bool;
    /// All contacts, for the management UI. Order is unspecified.
    fn list(&self) -> Vec<Contact>;
    /// Refresh `last_seen_at` (no-op if unknown).
    fn touch(&self, id: &ContactId, now: UnixMillis);
    /// Block a contact (kill-switch; keeps the record). Idempotent; no-op if unknown.
    fn block(&self, id: &ContactId);
    /// Unblock a contact. Idempotent; no-op if unknown.
    fn unblock(&self, id: &ContactId);
    /// Remove a contact entirely. Idempotent.
    fn remove(&self, id: &ContactId);
}

/// In-memory [`ContactBook`] for the MVP (a restart clears it; the SQLite durable impl is the follow-up,
/// ADR-092). `unsafe`-free, poison-tolerant like [`InMemoryPairingRegistry`].
#[derive(Default)]
pub struct InMemoryContactBook {
    inner: Mutex<HashMap<ContactId, Contact>>,
}

impl InMemoryContactBook {
    /// A fresh, empty contacts book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ContactId, Contact>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ContactBook for InMemoryContactBook {
    fn upsert(&self, contact: Contact) {
        let mut g = self.lock();
        if let Some(existing) = g.get_mut(&contact.id) {
            existing.label = contact.label;
            existing.last_seen_at = contact.last_seen_at;
            // added_at + blocked preserved — a re-add is not a new add and never silently unblocks.
        } else {
            g.insert(contact.id, contact);
        }
    }
    fn get(&self, id: &ContactId) -> Option<Contact> {
        self.lock().get(id).cloned()
    }
    fn is_active_contact(&self, id: &ContactId) -> bool {
        self.lock().get(id).is_some_and(|c| !c.blocked)
    }
    fn list(&self) -> Vec<Contact> {
        self.lock().values().cloned().collect()
    }
    fn touch(&self, id: &ContactId, now: UnixMillis) {
        if let Some(c) = self.lock().get_mut(id) {
            c.last_seen_at = now;
        }
    }
    fn block(&self, id: &ContactId) {
        if let Some(c) = self.lock().get_mut(id) {
            c.blocked = true;
        }
    }
    fn unblock(&self, id: &ContactId) {
        if let Some(c) = self.lock().get_mut(id) {
            c.blocked = false;
        }
    }
    fn remove(&self, id: &ContactId) {
        self.lock().remove(id);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ras-id-{}-{}", std::process::id(), tag))
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let msg = b"access-request-canonical-bytes";
        let sig = ks.sign(msg).unwrap();
        assert!(verify(&ks.public_key(), msg, &sig).is_ok());
    }

    #[test]
    fn verify_rejects_tampered_message() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let sig = ks.sign(b"original").unwrap();
        assert!(verify(&ks.public_key(), b"tampered", &sig).is_err());
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let a = SoftwareKeyStore::generate().unwrap();
        let b = SoftwareKeyStore::generate().unwrap();
        let sig = a.sign(b"m").unwrap();
        assert!(verify(&b.public_key(), b"m", &sig).is_err());
    }

    #[test]
    fn verify_rejects_tampered_signature() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let mut sig = ks.sign(b"m").unwrap();
        sig[0] ^= 0xff;
        assert!(verify(&ks.public_key(), b"m", &sig).is_err());
    }

    #[test]
    fn software_store_is_capped_at_tier0() {
        let ks = SoftwareKeyStore::generate().unwrap();
        assert_eq!(ks.tier_ceiling(), AssuranceTier::Tier0);
        assert!(ks.attestation().is_none());
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let rendered = format!("{ks:?}");
        // Only the public key (as a short hex id) may appear; no 32-byte secret material.
        assert!(rendered.starts_with("SoftwareKeyStore(pub="));
    }

    #[test]
    fn load_or_create_persists_the_same_key() {
        let path = tmp_path("persist");
        let _ = std::fs::remove_file(&path);
        let first = SoftwareKeyStore::load_or_create(&path).unwrap();
        let second = SoftwareKeyStore::load_or_create(&path).unwrap();
        assert_eq!(
            first.public_key(),
            second.public_key(),
            "same file → same identity"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_create_fails_closed_on_malformed_file() {
        let path = tmp_path("malformed");
        std::fs::write(&path, b"not a 32-byte key").unwrap();
        assert!(SoftwareKeyStore::load_or_create(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    fn paired(
        id: ControllerId,
        label: &str,
        first: UnixMillis,
        seen: UnixMillis,
    ) -> PairedController {
        PairedController {
            id,
            label: label.to_string(),
            first_paired_at: first,
            last_seen_at: seen,
        }
    }

    #[test]
    fn pairing_registry_pair_lookup_revoke_is_a_kill_switch() {
        let reg = InMemoryPairingRegistry::new();
        let id = ControllerId::from_bytes([7u8; PUBLIC_KEY_LEN]);
        assert!(!reg.is_paired(&id));
        reg.pair(paired(id, "Alice's laptop", 1000, 1000));
        assert!(reg.is_paired(&id));
        assert_eq!(reg.get(&id).unwrap().label, "Alice's laptop");
        assert_eq!(reg.list().len(), 1);
        reg.revoke(&id);
        assert!(!reg.is_paired(&id), "de-listing is a kill-switch");
        assert!(reg.get(&id).is_none());
    }

    #[test]
    fn re_pairing_preserves_first_paired_at_and_touch_updates_last_seen() {
        let reg = InMemoryPairingRegistry::new();
        let id = ControllerId::from_bytes([9u8; PUBLIC_KEY_LEN]);
        reg.pair(paired(id, "old label", 1000, 1000));
        // A re-pair refreshes the label + last_seen but must NOT reset the pairing age.
        reg.pair(paired(id, "new label", 5000, 5000));
        let rec = reg.get(&id).unwrap();
        assert_eq!(
            rec.first_paired_at, 1000,
            "re-pair must not reset the pairing age"
        );
        assert_eq!(rec.label, "new label");
        assert_eq!(rec.last_seen_at, 5000);
        // touch only moves last_seen.
        reg.touch(&id, 9000);
        assert_eq!(reg.get(&id).unwrap().last_seen_at, 9000);
        assert_eq!(reg.get(&id).unwrap().first_paired_at, 1000);
        // touch on an unknown id is a no-op.
        reg.touch(&ControllerId::from_bytes([1u8; PUBLIC_KEY_LEN]), 9000);
    }

    #[test]
    fn pairing_decision_governs_the_prompt_only_never_authority() {
        // Known → skip the prompt; unknown → require it. This is the whole decision surface: a
        // 2-variant enum with NO capabilities, so a registry hit can never authorize input on its own —
        // a skipped-prompt controller still goes through fresh grant issuance (Inv 3/9).
        let reg = InMemoryPairingRegistry::new();
        let known = ControllerId::from_bytes([2u8; PUBLIC_KEY_LEN]);
        let unknown = ControllerId::from_bytes([3u8; PUBLIC_KEY_LEN]);
        reg.pair(paired(known, "known", 1, 1));
        assert_eq!(
            pairing_decision(reg.is_paired(&known)),
            PairingDecision::SkipPairingPrompt
        );
        assert_eq!(
            pairing_decision(reg.is_paired(&unknown)),
            PairingDecision::RequirePairingPrompt
        );
        // Revocation flips a known controller back to requiring the attended accept.
        reg.revoke(&known);
        assert_eq!(
            pairing_decision(reg.is_paired(&known)),
            PairingDecision::RequirePairingPrompt
        );
    }

    #[test]
    fn pairing_code_is_deterministic_grouped_and_key_specific() {
        let a = ControllerId::from_bytes([0xABu8; PUBLIC_KEY_LEN]);
        let b = ControllerId::from_bytes([0xCDu8; PUBLIC_KEY_LEN]);
        let code_a = pairing_code(&a);
        assert_eq!(code_a, pairing_code(&a), "deterministic");
        assert_ne!(code_a, pairing_code(&b), "distinct keys → distinct codes");
        // 32 bytes = 256 bits → 52 Crockford-base32 symbols, grouped in 4s (12 dashes).
        assert_eq!(code_a.chars().filter(|c| *c != '-').count(), 52);
        assert_eq!(code_a.matches('-').count(), 12);
        // Crockford alphabet only: uppercase A–Z (no I,L,O,U) + digits, plus the group separator.
        assert!(code_a
            .chars()
            .all(|c| c == '-' || c.is_ascii_uppercase() || c.is_ascii_digit()));
        assert!(!code_a.contains(['I', 'L', 'O', 'U']));
    }

    // ─── Contacts (ADR-092) ──────────────────────────────────────────────────────────────────────

    fn contact(n: u8, label: &str, added: UnixMillis) -> Contact {
        Contact {
            id: ContactId::from_bytes([n; PUBLIC_KEY_LEN]),
            label: label.to_string(),
            added_at: added,
            last_seen_at: added,
            blocked: false,
        }
    }

    #[test]
    fn contact_upsert_preserves_added_at_and_block_on_re_add() {
        let book = InMemoryContactBook::new();
        let id = ContactId::from_bytes([5u8; PUBLIC_KEY_LEN]);
        book.upsert(contact(5, "Alice", 1000));
        book.block(&id); // user blocked Alice
                         // A later re-add (e.g. re-pairing) refreshes label + last_seen but must NOT reset the
                         // pairing age and must NOT silently unblock (Inv 1).
        book.upsert(contact(5, "Alice (laptop)", 5000));
        let got = book.get(&id).unwrap();
        assert_eq!(got.label, "Alice (laptop)");
        assert_eq!(got.last_seen_at, 5000);
        assert_eq!(got.added_at, 1000, "added_at preserved across re-add");
        assert!(got.blocked, "a re-add must not silently unblock");
        assert!(!book.is_active_contact(&id), "blocked ⇒ not active");
    }

    #[test]
    fn is_active_contact_is_the_deny_by_default_gate() {
        let book = InMemoryContactBook::new();
        let saved = ContactId::from_bytes([2u8; PUBLIC_KEY_LEN]);
        let stranger = ContactId::from_bytes([3u8; PUBLIC_KEY_LEN]);
        book.upsert(contact(2, "Bob", 1));
        assert!(
            book.is_active_contact(&saved),
            "saved + non-blocked ⇒ active"
        );
        assert!(
            !book.is_active_contact(&stranger),
            "unknown identity ⇒ refused (contacts-only)"
        );
        book.block(&saved);
        assert!(!book.is_active_contact(&saved), "blocked ⇒ refused");
        book.unblock(&saved);
        assert!(book.is_active_contact(&saved), "unblock restores active");
        book.remove(&saved);
        assert!(book.get(&saved).is_none(), "removed ⇒ gone");
        assert!(!book.is_active_contact(&saved));
    }

    #[test]
    fn contact_touch_and_list() {
        let book = InMemoryContactBook::new();
        book.upsert(contact(1, "A", 10));
        book.upsert(contact(2, "B", 20));
        book.touch(&ContactId::from_bytes([1u8; PUBLIC_KEY_LEN]), 999);
        assert_eq!(book.list().len(), 2);
        assert_eq!(
            book.get(&ContactId::from_bytes([1u8; PUBLIC_KEY_LEN]))
                .unwrap()
                .last_seen_at,
            999
        );
        // touch on an unknown id is a no-op (never panics / never inserts).
        book.touch(&ContactId::from_bytes([9u8; PUBLIC_KEY_LEN]), 1);
        assert_eq!(book.list().len(), 2);
    }

    #[test]
    fn contact_code_matches_pairing_code_over_the_same_key() {
        // A contact and a paired controller share the 32-byte key space, so the human verification code
        // is identical for the same bytes (one algorithm, `crockford_code`).
        let bytes = [0x5Au8; PUBLIC_KEY_LEN];
        assert_eq!(
            contact_code(&ContactId::from_bytes(bytes)),
            pairing_code(&ControllerId::from_bytes(bytes))
        );
    }
}
