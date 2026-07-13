//! Wire protocol types, framing, versioning, and the stable error taxonomy for Casual RAS.
//!
//! The protobuf message set (`proto/casual_ras.proto`) is the wire source of truth; codegen is
//! wired in a later phase. This crate currently hosts the protocol version and [`ErrorCode`].

/// Current bootstrap/session protocol major version. See `docs/04`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Stable, machine-readable error codes exposed across SDK and wire boundaries.
///
/// Mirrors the error model in `docs/04 §14`. Codes are stable across releases: add new variants,
/// never repurpose existing ones. String forms via [`ErrorCode::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    /// Malformed or unparseable message.
    InvalidMessage,
    /// Protocol version not supported.
    UnsupportedVersion,
    /// Identity does not match the expected/bound endpoint.
    IdentityMismatch,
    /// Signature verification failed.
    SignatureInvalid,
    /// Request or ticket expired.
    RequestExpired,
    /// Replay of a nonce/ticket/generation detected.
    ReplayDetected,
    /// Local user denied consent.
    ConsentDenied,
    /// Requested capability not permitted by policy.
    CapabilityDenied,
    /// Session grant invalid (binding/expiry/signature).
    GrantInvalid,
    /// Control lease invalid (generation/expiry).
    LeaseInvalid,
    /// Session was revoked (incl. emergency stop).
    SessionRevoked,
    /// Transport-level failure.
    TransportError,
    /// Screen capture failure.
    CaptureFailed,
    /// Encoder failure.
    EncoderFailed,
    /// Input injection failure.
    InputFailed,
    /// Local policy changed mid-session.
    PolicyChanged,
    /// Unexpected internal error.
    Internal,
}

impl ErrorCode {
    /// The stable wire/string form, e.g. `"SIGNATURE_INVALID"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidMessage => "INVALID_MESSAGE",
            ErrorCode::UnsupportedVersion => "UNSUPPORTED_VERSION",
            ErrorCode::IdentityMismatch => "IDENTITY_MISMATCH",
            ErrorCode::SignatureInvalid => "SIGNATURE_INVALID",
            ErrorCode::RequestExpired => "REQUEST_EXPIRED",
            ErrorCode::ReplayDetected => "REPLAY_DETECTED",
            ErrorCode::ConsentDenied => "CONSENT_DENIED",
            ErrorCode::CapabilityDenied => "CAPABILITY_DENIED",
            ErrorCode::GrantInvalid => "GRANT_INVALID",
            ErrorCode::LeaseInvalid => "LEASE_INVALID",
            ErrorCode::SessionRevoked => "SESSION_REVOKED",
            ErrorCode::TransportError => "TRANSPORT_ERROR",
            ErrorCode::CaptureFailed => "CAPTURE_FAILED",
            ErrorCode::EncoderFailed => "ENCODER_FAILED",
            ErrorCode::InputFailed => "INPUT_FAILED",
            ErrorCode::PolicyChanged => "POLICY_CHANGED",
            ErrorCode::Internal => "INTERNAL_ERROR",
        }
    }
}

impl core::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_have_stable_strings() {
        assert_eq!(ErrorCode::SignatureInvalid.as_str(), "SIGNATURE_INVALID");
        assert_eq!(ErrorCode::Internal.as_str(), "INTERNAL_ERROR");
        assert_eq!(ErrorCode::CapabilityDenied.to_string(), "CAPABILITY_DENIED");
    }
}
