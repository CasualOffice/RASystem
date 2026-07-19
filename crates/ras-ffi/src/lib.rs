//! Casual RAS **C ABI** (SDK phase, S1 — ADR-096).
//!
//! A stable, panic-safe C interface over the **proven, synchronous** core: identity (generate /
//! load-or-create / public key / sign), the Crockford contact/pairing code, and connection-ticket
//! parsing. The async session/host/controller SDK (a callback/runtime FFI model) is the larger
//! follow-up; this surface is fully testable off-device.
//!
//! ABI conventions (stable-SDK load-bearing):
//! - **Opaque handles** via `Box::into_raw`/`from_raw`, each with an explicit `_free`.
//! - **Integer status codes** ([`CasualRasStatus`], `0 = OK`) + caller-provided out-params; no Rust
//!   type ever crosses the boundary.
//! - Every entry point is **`catch_unwind`-guarded**: a Rust panic can never unwind across the FFI
//!   boundary (that is undefined behaviour) — it becomes [`CasualRasStatus::INTERNAL`].
//! - **No secret ever crosses the boundary** (Inv 8): the identity handle signs and yields the
//!   *public* key only, exactly like `KeyStore`.

use std::ffi::{c_char, c_int, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::OnceLock;

use ras_identity::{
    contact_code, ContactId, KeyStore, SoftwareKeyStore, PUBLIC_KEY_LEN, SIGNATURE_LEN,
};

/// Status codes returned across the ABI. `0` is success; any non-zero is a failure.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CasualRasStatus {
    /// Success.
    Ok = 0,
    /// A required pointer argument was NULL.
    NullArgument = 1,
    /// An argument was present but invalid (bad UTF-8, malformed ticket, buffer too small, …).
    InvalidArgument = 2,
    /// An internal error — including a caught Rust panic (never unwinds across the boundary).
    Internal = 3,
}

/// Run an FFI body, converting a panic into [`CasualRasStatus::Internal`] so it never unwinds across
/// the C boundary (which would be undefined behaviour).
fn guard(f: impl FnOnce() -> CasualRasStatus) -> c_int {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(CasualRasStatus::Internal) as c_int
}

/// The library version as a static, NUL-terminated C string. **Do not free** the returned pointer.
#[no_mangle]
pub extern "C" fn casual_ras_version() -> *const c_char {
    static V: OnceLock<CString> = OnceLock::new();
    V.get_or_init(|| CString::new(env!("CARGO_PKG_VERSION")).unwrap_or_default())
        .as_ptr()
}

/// An opaque software identity (Ed25519 key store). Create with `casual_ras_identity_generate` or
/// `casual_ras_identity_load_or_create`; release with `casual_ras_identity_free`.
pub struct CasualRasIdentity {
    store: SoftwareKeyStore,
}

/// Generate a fresh, ephemeral identity. On success writes a new handle to `*out_identity`.
///
/// # Safety
/// `out_identity` must be a valid, writable pointer to a `CasualRasIdentity*`.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_identity_generate(
    out_identity: *mut *mut CasualRasIdentity,
) -> c_int {
    guard(|| {
        if out_identity.is_null() {
            return CasualRasStatus::NullArgument;
        }
        match SoftwareKeyStore::generate() {
            Ok(store) => {
                let handle = Box::new(CasualRasIdentity { store });
                // SAFETY: checked non-null above.
                unsafe { *out_identity = Box::into_raw(handle) };
                CasualRasStatus::Ok
            }
            Err(_) => CasualRasStatus::Internal,
        }
    })
}

/// Load the identity persisted at `path` (a UTF-8 C string), creating + persisting one if absent. On
/// success writes a new handle to `*out_identity`.
///
/// # Safety
/// `path` must be a valid NUL-terminated C string; `out_identity` a valid, writable pointer to a
/// `CasualRasIdentity*`.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_identity_load_or_create(
    path: *const c_char,
    out_identity: *mut *mut CasualRasIdentity,
) -> c_int {
    guard(|| {
        if path.is_null() || out_identity.is_null() {
            return CasualRasStatus::NullArgument;
        }
        // SAFETY: non-null checked; caller guarantees a valid NUL-terminated string.
        let Ok(path_str) = (unsafe { CStr::from_ptr(path) }).to_str() else {
            return CasualRasStatus::InvalidArgument;
        };
        match SoftwareKeyStore::load_or_create(Path::new(path_str)) {
            Ok(store) => {
                let handle = Box::new(CasualRasIdentity { store });
                unsafe { *out_identity = Box::into_raw(handle) };
                CasualRasStatus::Ok
            }
            Err(_) => CasualRasStatus::Internal,
        }
    })
}

/// Free an identity handle. A NULL pointer is a no-op. Never call twice on the same handle.
///
/// # Safety
/// `identity` must be a handle returned by a `casual_ras_identity_*` constructor, or NULL.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_identity_free(identity: *mut CasualRasIdentity) {
    if !identity.is_null() {
        // SAFETY: reclaim the Box we leaked in the constructor.
        drop(unsafe { Box::from_raw(identity) });
    }
}

/// Write the identity's 32-byte Ed25519 **public** key into `out` (which must have room for 32 bytes).
///
/// # Safety
/// `identity` must be a valid handle; `out` must point to at least 32 writable bytes.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_identity_public_key(
    identity: *const CasualRasIdentity,
    out: *mut u8,
) -> c_int {
    guard(|| {
        // SAFETY: validated non-null; caller guarantees a live handle.
        let (Some(id), false) = (unsafe { identity.as_ref() }, out.is_null()) else {
            return CasualRasStatus::NullArgument;
        };
        let pk = id.store.public_key();
        // SAFETY: `out` has room for PUBLIC_KEY_LEN bytes per the contract.
        unsafe { std::ptr::copy_nonoverlapping(pk.as_ptr(), out, PUBLIC_KEY_LEN) };
        CasualRasStatus::Ok
    })
}

/// Sign `msg_len` bytes at `msg` with the identity key; write the 64-byte Ed25519 signature into `out`
/// (which must have room for 64 bytes). The private key never leaves the handle.
///
/// # Safety
/// `identity` a valid handle; `msg` points to `msg_len` readable bytes (may be NULL iff `msg_len==0`);
/// `out` points to at least 64 writable bytes.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_identity_sign(
    identity: *const CasualRasIdentity,
    msg: *const u8,
    msg_len: usize,
    out: *mut u8,
) -> c_int {
    guard(|| {
        let Some(id) = (unsafe { identity.as_ref() }) else {
            return CasualRasStatus::NullArgument;
        };
        if out.is_null() || (msg.is_null() && msg_len != 0) {
            return CasualRasStatus::NullArgument;
        }
        // SAFETY: caller guarantees `msg_len` readable bytes; empty slice for a null+0 message.
        let bytes = if msg_len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(msg, msg_len) }
        };
        match id.store.sign(bytes) {
            Ok(sig) => {
                // SAFETY: `out` has room for SIGNATURE_LEN bytes per the contract.
                unsafe { std::ptr::copy_nonoverlapping(sig.as_ptr(), out, SIGNATURE_LEN) };
                CasualRasStatus::Ok
            }
            Err(_) => CasualRasStatus::Internal,
        }
    })
}

/// Render a 32-byte identity as its grouped Crockford-base32 **contact/pairing code** (the human
/// verification string) into `out`, a caller buffer of `out_cap` bytes. Writes a NUL terminator;
/// returns [`CasualRasStatus::InvalidArgument`] if `out_cap` is too small (needs ~64 bytes).
///
/// # Safety
/// `id32` points to 32 readable bytes; `out` points to `out_cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_contact_code(
    id32: *const u8,
    out: *mut c_char,
    out_cap: usize,
) -> c_int {
    guard(|| {
        if id32.is_null() || out.is_null() {
            return CasualRasStatus::NullArgument;
        }
        // SAFETY: caller guarantees 32 readable bytes.
        let mut id = [0u8; PUBLIC_KEY_LEN];
        unsafe { std::ptr::copy_nonoverlapping(id32, id.as_mut_ptr(), PUBLIC_KEY_LEN) };
        let code = contact_code(&ContactId::from_bytes(id));
        let bytes = code.as_bytes();
        if bytes.len() + 1 > out_cap {
            return CasualRasStatus::InvalidArgument;
        }
        // SAFETY: verified `out_cap >= bytes.len() + 1`.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), bytes.len());
            *out.add(bytes.len()) = 0; // NUL terminator
        }
        CasualRasStatus::Ok
    })
}

/// Parse a connection **ticket** (`CASUALRAS1:…`) and write the peer's 32-byte endpoint identity into
/// `out` (room for 32 bytes). Fail-closed: a malformed ticket returns
/// [`CasualRasStatus::InvalidArgument`].
///
/// # Safety
/// `ticket` a valid NUL-terminated C string; `out` points to at least 32 writable bytes.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_ticket_endpoint_id(
    ticket: *const c_char,
    out: *mut u8,
) -> c_int {
    guard(|| {
        if ticket.is_null() || out.is_null() {
            return CasualRasStatus::NullArgument;
        }
        let Ok(ticket_str) = (unsafe { CStr::from_ptr(ticket) }).to_str() else {
            return CasualRasStatus::InvalidArgument;
        };
        match ras_transport_iroh::EndpointAddr::from_ticket(ticket_str) {
            Ok(addr) => {
                // SAFETY: `out` has room for 32 bytes per the contract.
                unsafe { std::ptr::copy_nonoverlapping(addr.id.0.as_ptr(), out, PUBLIC_KEY_LEN) };
                CasualRasStatus::Ok
            }
            Err(_) => CasualRasStatus::InvalidArgument,
        }
    })
}

/// Verify a 64-byte Ed25519 signature (`sig`) over `msg_len` bytes at `msg` against the 32-byte public
/// key `pubkey`. Returns [`CasualRasStatus::Ok`] if valid, [`CasualRasStatus::InvalidArgument`] if it
/// does not verify (or the key is malformed). The counterpart of `casual_ras_identity_sign`.
///
/// # Safety
/// `pubkey` points to 32 readable bytes; `sig` to 64 readable bytes; `msg` to `msg_len` readable bytes
/// (may be NULL iff `msg_len == 0`).
#[no_mangle]
pub unsafe extern "C" fn casual_ras_verify(
    pubkey: *const u8,
    msg: *const u8,
    msg_len: usize,
    sig: *const u8,
) -> c_int {
    guard(|| {
        if pubkey.is_null() || sig.is_null() || (msg.is_null() && msg_len != 0) {
            return CasualRasStatus::NullArgument;
        }
        let mut pk = [0u8; PUBLIC_KEY_LEN];
        let mut signature = [0u8; SIGNATURE_LEN];
        // SAFETY: caller guarantees 32 + 64 readable bytes respectively.
        unsafe {
            std::ptr::copy_nonoverlapping(pubkey, pk.as_mut_ptr(), PUBLIC_KEY_LEN);
            std::ptr::copy_nonoverlapping(sig, signature.as_mut_ptr(), SIGNATURE_LEN);
        }
        let bytes = if msg_len == 0 {
            &[][..]
        } else {
            // SAFETY: caller guarantees `msg_len` readable bytes.
            unsafe { std::slice::from_raw_parts(msg, msg_len) }
        };
        match ras_identity::verify(&pk, bytes, &signature) {
            Ok(()) => CasualRasStatus::Ok,
            Err(_) => CasualRasStatus::InvalidArgument,
        }
    })
}

/// Build an id-only connection **ticket** (`CASUALRAS1:…`) for the 32-byte endpoint identity `id32`,
/// writing the NUL-terminated string into `out` (a caller buffer of `out_cap` bytes). The counterpart
/// of [`casual_ras_ticket_endpoint_id`]; returns [`CasualRasStatus::InvalidArgument`] if `out_cap` is
/// too small.
///
/// # Safety
/// `id32` points to 32 readable bytes; `out` to `out_cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn casual_ras_ticket_from_endpoint_id(
    id32: *const u8,
    out: *mut c_char,
    out_cap: usize,
) -> c_int {
    guard(|| {
        if id32.is_null() || out.is_null() {
            return CasualRasStatus::NullArgument;
        }
        // SAFETY: caller guarantees 32 readable bytes.
        let mut id = [0u8; PUBLIC_KEY_LEN];
        unsafe { std::ptr::copy_nonoverlapping(id32, id.as_mut_ptr(), PUBLIC_KEY_LEN) };
        let ticket =
            ras_transport_iroh::EndpointAddr::new(ras_transport_iroh::EndpointId(id)).to_ticket();
        let bytes = ticket.as_bytes();
        if bytes.len() + 1 > out_cap {
            return CasualRasStatus::InvalidArgument;
        }
        // SAFETY: verified `out_cap >= bytes.len() + 1`.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), bytes.len());
            *out.add(bytes.len()) = 0;
        }
        CasualRasStatus::Ok
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn version_is_a_valid_c_string() {
        let p = casual_ras_version();
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn identity_generate_pubkey_sign_free_round_trip() {
        let mut id: *mut CasualRasIdentity = std::ptr::null_mut();
        assert_eq!(
            unsafe { casual_ras_identity_generate(&mut id) },
            CasualRasStatus::Ok as c_int
        );
        assert!(!id.is_null());

        let mut pk = [0u8; PUBLIC_KEY_LEN];
        assert_eq!(
            unsafe { casual_ras_identity_public_key(id, pk.as_mut_ptr()) },
            CasualRasStatus::Ok as c_int
        );
        assert_ne!(pk, [0u8; PUBLIC_KEY_LEN], "a real key was written");

        let msg = b"casual-ras ffi test";
        let mut sig = [0u8; SIGNATURE_LEN];
        assert_eq!(
            unsafe { casual_ras_identity_sign(id, msg.as_ptr(), msg.len(), sig.as_mut_ptr()) },
            CasualRasStatus::Ok as c_int
        );
        // Verify the signature with the core verifier against the exported public key.
        assert!(ras_identity::verify(&pk, msg, &sig).is_ok());

        unsafe { casual_ras_identity_free(id) };
    }

    #[test]
    fn null_arguments_are_rejected_not_crashed() {
        assert_eq!(
            unsafe { casual_ras_identity_generate(std::ptr::null_mut()) },
            CasualRasStatus::NullArgument as c_int
        );
        assert_eq!(
            unsafe { casual_ras_identity_public_key(std::ptr::null(), std::ptr::null_mut()) },
            CasualRasStatus::NullArgument as c_int
        );
        // Freeing NULL is a safe no-op.
        unsafe { casual_ras_identity_free(std::ptr::null_mut()) };
    }

    #[test]
    fn contact_code_matches_the_core_and_bounds_the_buffer() {
        let id = [0x5Au8; PUBLIC_KEY_LEN];
        let expected = contact_code(&ContactId::from_bytes(id));

        let mut buf = vec![0i8; 128];
        assert_eq!(
            unsafe { casual_ras_contact_code(id.as_ptr(), buf.as_mut_ptr(), buf.len()) },
            CasualRasStatus::Ok as c_int
        );
        let got = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().unwrap();
        assert_eq!(got, expected);

        // Too-small buffer is refused, not overflowed.
        let mut tiny = vec![0i8; 4];
        assert_eq!(
            unsafe { casual_ras_contact_code(id.as_ptr(), tiny.as_mut_ptr(), tiny.len()) },
            CasualRasStatus::InvalidArgument as c_int
        );
    }

    #[test]
    fn ticket_parse_round_trips_and_rejects_garbage() {
        // Build a real id-only ticket from a known id, then parse it back through the ABI.
        let id = [7u8; PUBLIC_KEY_LEN];
        let ticket =
            ras_transport_iroh::EndpointAddr::new(ras_transport_iroh::EndpointId(id)).to_ticket();
        let cticket = CString::new(ticket).unwrap();
        let mut out = [0u8; PUBLIC_KEY_LEN];
        assert_eq!(
            unsafe { casual_ras_ticket_endpoint_id(cticket.as_ptr(), out.as_mut_ptr()) },
            CasualRasStatus::Ok as c_int
        );
        assert_eq!(out, id);

        let bad = CString::new("NOT-A-TICKET").unwrap();
        assert_eq!(
            unsafe { casual_ras_ticket_endpoint_id(bad.as_ptr(), out.as_mut_ptr()) },
            CasualRasStatus::InvalidArgument as c_int
        );
    }

    #[test]
    fn verify_accepts_a_real_signature_and_rejects_a_tampered_one() {
        let mut id: *mut CasualRasIdentity = std::ptr::null_mut();
        unsafe { casual_ras_identity_generate(&mut id) };
        let mut pk = [0u8; PUBLIC_KEY_LEN];
        unsafe { casual_ras_identity_public_key(id, pk.as_mut_ptr()) };
        let msg = b"verify me";
        let mut sig = [0u8; SIGNATURE_LEN];
        unsafe { casual_ras_identity_sign(id, msg.as_ptr(), msg.len(), sig.as_mut_ptr()) };

        assert_eq!(
            unsafe { casual_ras_verify(pk.as_ptr(), msg.as_ptr(), msg.len(), sig.as_ptr()) },
            CasualRasStatus::Ok as c_int
        );
        sig[0] ^= 0xff; // tamper
        assert_eq!(
            unsafe { casual_ras_verify(pk.as_ptr(), msg.as_ptr(), msg.len(), sig.as_ptr()) },
            CasualRasStatus::InvalidArgument as c_int
        );
        unsafe { casual_ras_identity_free(id) };
    }

    #[test]
    fn ticket_build_round_trips_through_parse() {
        let id = [0x11u8; PUBLIC_KEY_LEN];
        let mut buf = vec![0i8; 256];
        assert_eq!(
            unsafe { casual_ras_ticket_from_endpoint_id(id.as_ptr(), buf.as_mut_ptr(), buf.len()) },
            CasualRasStatus::Ok as c_int
        );
        // Parse the built ticket back — same id (build ⇄ parse are inverses).
        let mut out = [0u8; PUBLIC_KEY_LEN];
        assert_eq!(
            unsafe { casual_ras_ticket_endpoint_id(buf.as_ptr(), out.as_mut_ptr()) },
            CasualRasStatus::Ok as c_int
        );
        assert_eq!(out, id);

        // Too-small buffer refused, not overflowed.
        let mut tiny = vec![0i8; 8];
        assert_eq!(
            unsafe {
                casual_ras_ticket_from_endpoint_id(id.as_ptr(), tiny.as_mut_ptr(), tiny.len())
            },
            CasualRasStatus::InvalidArgument as c_int
        );
    }
}
