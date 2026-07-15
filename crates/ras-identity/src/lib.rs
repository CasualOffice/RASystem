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

use std::collections::HashSet;
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

/// The paired-controller registry (`docs/16 §11`). A controller is trusted only after the local user
/// accepts its pairing; **de-listing is a kill-switch** (one of the three, `docs/16`). MVP is
/// in-memory; the SQLite-backed impl (restart-survival) lands with identity persistence.
pub trait TrustedControllers: Send + Sync {
    /// Whether this controller has been paired and not since revoked.
    fn is_trusted(&self, id: &ControllerId) -> bool;
    /// Record a controller as trusted (after a human pairing accept).
    fn trust(&self, id: ControllerId);
    /// De-list a controller (kill-switch). Idempotent.
    fn revoke(&self, id: &ControllerId);
}

/// In-memory [`TrustedControllers`] for the attended MVP. A host restart clears it (attended sessions
/// end on restart anyway); durable pairing is the SQLite impl (later).
#[derive(Default)]
pub struct InMemoryTrustedControllers {
    inner: Mutex<HashSet<ControllerId>>,
}

impl InMemoryTrustedControllers {
    /// A fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<ControllerId>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl TrustedControllers for InMemoryTrustedControllers {
    fn is_trusted(&self, id: &ControllerId) -> bool {
        self.lock().contains(id)
    }
    fn trust(&self, id: ControllerId) {
        self.lock().insert(id);
    }
    fn revoke(&self, id: &ControllerId) {
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

    #[test]
    fn trusted_registry_trust_lookup_revoke() {
        let reg = InMemoryTrustedControllers::new();
        let id = ControllerId::from_bytes([7u8; PUBLIC_KEY_LEN]);
        assert!(!reg.is_trusted(&id));
        reg.trust(id);
        assert!(reg.is_trusted(&id));
        reg.revoke(&id);
        assert!(!reg.is_trusted(&id), "de-listing is a kill-switch");
    }
}
