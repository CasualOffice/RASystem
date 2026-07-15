//! PASETO **v4.public** (ADR-064/066): an algorithm-pinned Ed25519 signature over a small typed
//! claims blob, used for the sender-constrained [`SessionGrant`](crate::SessionGrant).
//!
//! Only the deterministic PASETO *envelope* is implemented here: PAE (pre-authentication encoding),
//! unpadded base64url, and the header/footer framing. The signature **primitive** is `ed25519-dalek`,
//! reached through the `ras-identity` `KeyStore`/`verify` seam (ADR-065), never re-implemented. The
//! envelope is verified byte-exact against the official PASETO v4 test vectors (`4-S-1/2/3`) in the
//! crate tests, so "hand-rolled" here means the well-specified length-prefix wrapper, not crypto.
//!
//! Reference: <https://github.com/paseto-standard/paseto-spec> (v4, `Sign`/`Verify`).

use ras_protocol::ErrorCode;

/// The only header this crate emits or accepts. Version + purpose are pinned (no algorithm agility).
pub const V4_PUBLIC_HEADER: &str = "v4.public.";
const SIG_LEN: usize = 64;

// ── base64url (unpadded), RFC 4648 §5 ──────────────────────────────────────────────────────────

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encode to unpadded base64url.
fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(B64URL[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL[(n & 0x3f) as usize] as char);
        }
    }
    out
}

/// Decode unpadded base64url. **Fail-closed**: any non-alphabet byte, embedded padding, or an
/// impossible final-group length (a lone trailing char) is rejected — never partial output.
fn b64url_decode(input: &str) -> Result<Vec<u8>, ()> {
    fn val(c: u8) -> Result<u32, ()> {
        match c {
            b'A'..=b'Z' => Ok(u32::from(c - b'A')),
            b'a'..=b'z' => Ok(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(c - b'0') + 52),
            b'-' => Ok(62),
            b'_' => Ok(63),
            _ => Err(()),
        }
    }
    let bytes = input.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err(()); // a single leftover char can never encode a byte
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (k, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * k);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

// ── PAE + v4.public sign/verify ────────────────────────────────────────────────────────────────

/// Pre-Authentication Encoding: `LE64(count) || (LE64(len(piece)) || piece)*`. The high bit of each
/// length is cleared per spec (our sizes never approach 2^63, so this is exact, not lossy).
fn pae(pieces: &[&[u8]]) -> Vec<u8> {
    fn le64(n: u64) -> [u8; 8] {
        (n & 0x7fff_ffff_ffff_ffff).to_le_bytes()
    }
    let mut out = Vec::new();
    out.extend_from_slice(&le64(pieces.len() as u64));
    for p in pieces {
        out.extend_from_slice(&le64(p.len() as u64));
        out.extend_from_slice(p);
    }
    out
}

/// Sign message `m` with public footer `f` and implicit assertion `i`, producing a `v4.public.` token.
/// `sign` is the Ed25519 signer over arbitrary bytes (the host `KeyStore`). The signature covers
/// `PAE([header, m, f, i])`, so both the payload and any footer/implicit binding are authenticated.
pub fn v4_public_sign<E>(
    sign: impl Fn(&[u8]) -> Result<[u8; SIG_LEN], E>,
    m: &[u8],
    f: &[u8],
    i: &[u8],
) -> Result<String, E> {
    let m2 = pae(&[V4_PUBLIC_HEADER.as_bytes(), m, f, i]);
    let sig = sign(&m2)?;
    let mut body = Vec::with_capacity(m.len() + SIG_LEN);
    body.extend_from_slice(m);
    body.extend_from_slice(&sig);
    let mut token = String::from(V4_PUBLIC_HEADER);
    token.push_str(&b64url_encode(&body));
    if !f.is_empty() {
        token.push('.');
        token.push_str(&b64url_encode(f));
    }
    Ok(token)
}

/// Verify a `v4.public.` token against `pubkey`, requiring the exact expected footer `f` and implicit
/// assertion `i`. Returns the recovered message `m` on success. **Fail-closed**, and it never reveals
/// *which* step failed: every malformed/forged/mismatched case maps to [`ErrorCode::SignatureInvalid`]
/// (bad header/base64/length/footer or a failed signature) — no verification oracle.
pub fn v4_public_verify(
    pubkey: &[u8; 32],
    token: &str,
    f: &[u8],
    i: &[u8],
) -> Result<Vec<u8>, ErrorCode> {
    let bad = || ErrorCode::SignatureInvalid;
    let rest = token.strip_prefix(V4_PUBLIC_HEADER).ok_or_else(bad)?;

    // The body base64url contains no '.', so at most one '.' separates payload from footer.
    let mut parts = rest.split('.');
    let payload_b64 = parts.next().ok_or_else(bad)?;
    let footer_b64 = parts.next();
    if parts.next().is_some() {
        return Err(bad()); // extra sections → malformed
    }

    let raw = b64url_decode(payload_b64).map_err(|()| bad())?;
    if raw.len() < SIG_LEN {
        return Err(bad());
    }
    let (m, sig) = raw.split_at(raw.len() - SIG_LEN);

    // The provided footer must decode to exactly the expected footer (constant-time-agnostic: a
    // footer is public data, not a secret, so a plain compare is fine).
    let actual_footer = match footer_b64 {
        Some(fb) => b64url_decode(fb).map_err(|()| bad())?,
        None => Vec::new(),
    };
    if actual_footer != f {
        return Err(bad());
    }

    let sig_arr: [u8; SIG_LEN] = sig.try_into().map_err(|_| bad())?;
    let m2 = pae(&[V4_PUBLIC_HEADER.as_bytes(), m, f, i]);
    ras_identity::verify(pubkey, &m2, &sig_arr).map_err(|_| bad())?;
    Ok(m.to_vec())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use ras_identity::{KeyStore, SoftwareKeyStore};

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Official PASETO v4 test vectors (paseto-standard/test-vectors, v4.json).
    const VEC_SK: &str = "b4cbfb43df4ce210727d953e4a713307fa19bb7d9f85041438d9e11b942a37741eb9dbbbbc047c03fd70604e0071f0987e16b28b757225c11f00415d0e20b1a2";
    const VEC_PK: &str = "1eb9dbbbbc047c03fd70604e0071f0987e16b28b757225c11f00415d0e20b1a2";
    const VEC_PAYLOAD: &str =
        r#"{"data":"this is a signed message","exp":"2022-01-01T00:00:00+00:00"}"#;

    fn vec_keystore() -> SoftwareKeyStore {
        let seed: [u8; 32] = unhex(VEC_SK)[..32].try_into().unwrap();
        SoftwareKeyStore::from_seed(seed)
    }
    fn vec_pubkey() -> [u8; 32] {
        unhex(VEC_PK).try_into().unwrap()
    }

    /// 4-S-1: no footer, no implicit — our sign must reproduce the official token byte-for-byte, and
    /// verify must recover the exact payload.
    #[test]
    fn official_vector_4_s_1() {
        const TOKEN: &str = "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9bg_XBBzds8lTZShVlwwKSgeKpLT3yukTw6JUz3W4h_ExsQV-P0V54zemZDcAxFaSeef1QlXEFtkqxT1ciiQEDA";
        let ks = vec_keystore();
        let token = v4_public_sign(|m| ks.sign(m), VEC_PAYLOAD.as_bytes(), b"", b"").unwrap();
        assert_eq!(token, TOKEN, "sign must match the official token");
        let m = v4_public_verify(&vec_pubkey(), TOKEN, b"", b"").unwrap();
        assert_eq!(m, VEC_PAYLOAD.as_bytes());
    }

    /// 4-S-2: with a footer, no implicit.
    #[test]
    fn official_vector_4_s_2() {
        const TOKEN: &str = "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9v3Jt8mx_TdM2ceTGoqwrh4yDFn0XsHvvV_D0DtwQxVrJEBMl0F2caAdgnpKlt4p7xBnx1HcO-SPo8FPp214HDw.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9";
        const FOOTER: &str = r#"{"kid":"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN"}"#;
        let ks = vec_keystore();
        let token = v4_public_sign(
            |m| ks.sign(m),
            VEC_PAYLOAD.as_bytes(),
            FOOTER.as_bytes(),
            b"",
        )
        .unwrap();
        assert_eq!(token, TOKEN);
        let m = v4_public_verify(&vec_pubkey(), TOKEN, FOOTER.as_bytes(), b"").unwrap();
        assert_eq!(m, VEC_PAYLOAD.as_bytes());
        // Wrong expected footer must fail closed.
        assert!(v4_public_verify(&vec_pubkey(), TOKEN, b"", b"").is_err());
    }

    /// 4-S-3: with a footer AND an implicit assertion (bound into the signature, not transmitted).
    #[test]
    fn official_vector_4_s_3() {
        const TOKEN: &str = "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9NPWciuD3d0o5eXJXG5pJy-DiVEoyPYWs1YSTwWHNJq6DZD3je5gf-0M4JR9ipdUSJbIovzmBECeaWmaqcaP0DQ.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9";
        const FOOTER: &str = r#"{"kid":"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN"}"#;
        const IMPLICIT: &str = r#"{"test-vector":"4-S-3"}"#;
        let ks = vec_keystore();
        let token = v4_public_sign(
            |m| ks.sign(m),
            VEC_PAYLOAD.as_bytes(),
            FOOTER.as_bytes(),
            IMPLICIT.as_bytes(),
        )
        .unwrap();
        assert_eq!(token, TOKEN);
        assert!(
            v4_public_verify(&vec_pubkey(), TOKEN, FOOTER.as_bytes(), IMPLICIT.as_bytes()).is_ok()
        );
        // A different implicit assertion must fail: the binding is authenticated even though it is
        // never sent on the wire.
        assert!(v4_public_verify(&vec_pubkey(), TOKEN, FOOTER.as_bytes(), b"wrong").is_err());
    }

    #[test]
    fn pae_matches_reference_examples() {
        // From the PASETO spec's PAE section.
        assert_eq!(pae(&[]), 0u64.to_le_bytes());
        assert_eq!(
            pae(&[b""]),
            [1, 0, 0, 0, 0, 0, 0, 0, /*len0*/ 0, 0, 0, 0, 0, 0, 0, 0]
        );
        let got = pae(&[b"test"]);
        let mut want = Vec::new();
        want.extend_from_slice(&1u64.to_le_bytes());
        want.extend_from_slice(&4u64.to_le_bytes());
        want.extend_from_slice(b"test");
        assert_eq!(got, want);
    }

    #[test]
    fn base64url_round_trips_and_rejects_junk() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\x00\xff\x10\x81"] {
            let enc = b64url_encode(data);
            assert!(!enc.contains('='), "no padding");
            assert_eq!(b64url_decode(&enc).unwrap(), data);
        }
        assert!(b64url_decode("****").is_err()); // non-alphabet
        assert!(b64url_decode("A").is_err()); // lone final char
        assert!(b64url_decode("AB=C").is_err()); // padding is not accepted
    }

    #[test]
    fn round_trip_with_generated_key_and_tamper_fails() {
        let ks = SoftwareKeyStore::generate().unwrap();
        let token = v4_public_sign(|m| ks.sign(m), b"claims-blob", b"", b"").unwrap();
        assert_eq!(
            v4_public_verify(&ks.public_key(), &token, b"", b"").unwrap(),
            b"claims-blob"
        );
        // Flip one payload byte in the token → signature fails.
        let mut chars: Vec<char> = token.chars().collect();
        let idx = V4_PUBLIC_HEADER.len() + 2;
        chars[idx] = if chars[idx] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert!(v4_public_verify(&ks.public_key(), &tampered, b"", b"").is_err());
        // Wrong key fails.
        let other = SoftwareKeyStore::generate().unwrap();
        assert!(v4_public_verify(&other.public_key(), &token, b"", b"").is_err());
    }
}
