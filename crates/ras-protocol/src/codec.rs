//! Protobuf serialization layer for the control channel.
//!
//! This module bridges the ergonomic, hand-rolled public API ([`ControlMsg`] and friends in the
//! crate root) to the generated prost wire types, which stay a private impl detail here (`mod pb`)
//! and are **never** re-exported. It provides:
//!
//! * [`encode`] / [`decode`] — [`ControlMsg`] ⇄ protobuf bytes (no length prefix).
//! * [`frame`] / [`try_read_frame`] — `u32-BE length | protobuf(ControlMsg)` framing with a
//!   [`MAX_CONTROL_FRAME`] DoS guard that fires on the length prefix, before allocation.
//!
//! Everything here is synchronous (no tokio); async stream I/O lives in `ras-transport-iroh`.
//!
//! Security posture: every decode failure is a typed [`RasError`] with a stable [`ErrorCode`] —
//! no panics, no `unwrap`/`expect` on the decode path. Error `context` is a static, content-free
//! string; decoded bytes are never embedded in an error or logged (Invariant 8). The
//! `AuthEnvelope` payload is opaque and round-trips losslessly. `ErrorCode`/`KeyframeReason` wire
//! numbers are mapped by explicit `match` (never `as i32` on the Rust enum), so the wire numbering
//! is append-only and cannot drift if the Rust enum is reordered.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use prost::Message;

use crate::{
    AccessOutcome, BootstrapMsg, ControlMsg, DecoderFeedback, ErrorCode, InputAction,
    InputEnvelope, KeyframeReason, KeyframeRequest, PointerButton, PointerUpdate, RasError,
    StreamConfigWire, MAX_CAPABILITIES, MAX_CAPABILITY_LEN, MAX_CONTROL_FRAME, MAX_DISPLAY_NAME,
    MAX_TEXT_INPUT,
};

/// Generated prost types for `proto/casual_ras.proto` (package `casual_ras.v1`).
///
/// DO NOT EDIT — produced at build time by `build.rs` (protox + prost-build) into `OUT_DIR`.
/// Internal to the codec; never re-exported. Edit the `.proto` and rebuild instead.
mod pb {
    // Generated code: silence all clippy/rustc lints scoped to this module only (the workspace
    // runs `-D warnings`). `dead_code` covers the unused `Ping` placeholder message.
    #![allow(
        clippy::all,
        clippy::pedantic,
        clippy::nursery,
        missing_docs,
        dead_code
    )]
    include!(concat!(env!("OUT_DIR"), "/casual_ras.v1.rs"));
}

// ---------------------------------------------------------------------------
// Enum mappings — explicit, append-only, no numeric cast on the Rust enum.
// ---------------------------------------------------------------------------

/// Rust [`KeyframeReason`] → wire enum. Total and infallible.
fn reason_to_pb(reason: KeyframeReason) -> pb::KeyframeReasonProto {
    match reason {
        KeyframeReason::StreamStart => pb::KeyframeReasonProto::KeyframeReasonStreamStart,
        KeyframeReason::UnrecoverableLoss => {
            pb::KeyframeReasonProto::KeyframeReasonUnrecoverableLoss
        }
        KeyframeReason::DecoderReset => pb::KeyframeReasonProto::KeyframeReasonDecoderReset,
        KeyframeReason::ConfigChanged => pb::KeyframeReasonProto::KeyframeReasonConfigChanged,
        KeyframeReason::PeriodicRefresh => pb::KeyframeReasonProto::KeyframeReasonPeriodicRefresh,
    }
}

/// Wire enum number → Rust [`KeyframeReason`]. Rejects `UNSPECIFIED (0)` and any unknown number
/// with a typed error — never a silent default.
fn reason_from_pb(raw: i32) -> Result<KeyframeReason, RasError> {
    match pb::KeyframeReasonProto::try_from(raw) {
        Ok(pb::KeyframeReasonProto::KeyframeReasonStreamStart) => Ok(KeyframeReason::StreamStart),
        Ok(pb::KeyframeReasonProto::KeyframeReasonUnrecoverableLoss) => {
            Ok(KeyframeReason::UnrecoverableLoss)
        }
        Ok(pb::KeyframeReasonProto::KeyframeReasonDecoderReset) => Ok(KeyframeReason::DecoderReset),
        Ok(pb::KeyframeReasonProto::KeyframeReasonConfigChanged) => {
            Ok(KeyframeReason::ConfigChanged)
        }
        Ok(pb::KeyframeReasonProto::KeyframeReasonPeriodicRefresh) => {
            Ok(KeyframeReason::PeriodicRefresh)
        }
        // KeyframeReasonUnspecified (0) or any unrecognized number.
        _ => Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "unknown keyframe reason",
        )),
    }
}

/// Rust [`ErrorCode`] → wire enum. Exhaustive over today's variants with **no wildcard**: adding a
/// new `ErrorCode` variant is a compile error here that forces the author to assign its stable wire
/// number (append-only numbering).
fn errorcode_to_pb(code: ErrorCode) -> pb::ErrorCodeProto {
    match code {
        ErrorCode::InvalidMessage => pb::ErrorCodeProto::ErrorCodeInvalidMessage,
        ErrorCode::UnsupportedVersion => pb::ErrorCodeProto::ErrorCodeUnsupportedVersion,
        ErrorCode::IdentityMismatch => pb::ErrorCodeProto::ErrorCodeIdentityMismatch,
        ErrorCode::SignatureInvalid => pb::ErrorCodeProto::ErrorCodeSignatureInvalid,
        ErrorCode::RequestExpired => pb::ErrorCodeProto::ErrorCodeRequestExpired,
        ErrorCode::ReplayDetected => pb::ErrorCodeProto::ErrorCodeReplayDetected,
        ErrorCode::ConsentDenied => pb::ErrorCodeProto::ErrorCodeConsentDenied,
        ErrorCode::CapabilityDenied => pb::ErrorCodeProto::ErrorCodeCapabilityDenied,
        ErrorCode::GrantInvalid => pb::ErrorCodeProto::ErrorCodeGrantInvalid,
        ErrorCode::LeaseInvalid => pb::ErrorCodeProto::ErrorCodeLeaseInvalid,
        ErrorCode::SessionRevoked => pb::ErrorCodeProto::ErrorCodeSessionRevoked,
        ErrorCode::TransportError => pb::ErrorCodeProto::ErrorCodeTransportError,
        ErrorCode::CaptureFailed => pb::ErrorCodeProto::ErrorCodeCaptureFailed,
        ErrorCode::EncoderFailed => pb::ErrorCodeProto::ErrorCodeEncoderFailed,
        ErrorCode::InputFailed => pb::ErrorCodeProto::ErrorCodeInputFailed,
        ErrorCode::PolicyChanged => pb::ErrorCodeProto::ErrorCodePolicyChanged,
        ErrorCode::Internal => pb::ErrorCodeProto::ErrorCodeInternal,
        ErrorCode::NormalClosure => pb::ErrorCodeProto::ErrorCodeNormalClosure,
    }
}

/// Wire enum number → Rust [`ErrorCode`]. Rejects `UNSPECIFIED (0)` and any unknown number.
fn errorcode_from_pb(raw: i32) -> Result<ErrorCode, RasError> {
    match pb::ErrorCodeProto::try_from(raw) {
        Ok(pb::ErrorCodeProto::ErrorCodeInvalidMessage) => Ok(ErrorCode::InvalidMessage),
        Ok(pb::ErrorCodeProto::ErrorCodeUnsupportedVersion) => Ok(ErrorCode::UnsupportedVersion),
        Ok(pb::ErrorCodeProto::ErrorCodeIdentityMismatch) => Ok(ErrorCode::IdentityMismatch),
        Ok(pb::ErrorCodeProto::ErrorCodeSignatureInvalid) => Ok(ErrorCode::SignatureInvalid),
        Ok(pb::ErrorCodeProto::ErrorCodeRequestExpired) => Ok(ErrorCode::RequestExpired),
        Ok(pb::ErrorCodeProto::ErrorCodeReplayDetected) => Ok(ErrorCode::ReplayDetected),
        Ok(pb::ErrorCodeProto::ErrorCodeConsentDenied) => Ok(ErrorCode::ConsentDenied),
        Ok(pb::ErrorCodeProto::ErrorCodeCapabilityDenied) => Ok(ErrorCode::CapabilityDenied),
        Ok(pb::ErrorCodeProto::ErrorCodeGrantInvalid) => Ok(ErrorCode::GrantInvalid),
        Ok(pb::ErrorCodeProto::ErrorCodeLeaseInvalid) => Ok(ErrorCode::LeaseInvalid),
        Ok(pb::ErrorCodeProto::ErrorCodeSessionRevoked) => Ok(ErrorCode::SessionRevoked),
        Ok(pb::ErrorCodeProto::ErrorCodeTransportError) => Ok(ErrorCode::TransportError),
        Ok(pb::ErrorCodeProto::ErrorCodeCaptureFailed) => Ok(ErrorCode::CaptureFailed),
        Ok(pb::ErrorCodeProto::ErrorCodeEncoderFailed) => Ok(ErrorCode::EncoderFailed),
        Ok(pb::ErrorCodeProto::ErrorCodeInputFailed) => Ok(ErrorCode::InputFailed),
        Ok(pb::ErrorCodeProto::ErrorCodePolicyChanged) => Ok(ErrorCode::PolicyChanged),
        Ok(pb::ErrorCodeProto::ErrorCodeInternal) => Ok(ErrorCode::Internal),
        Ok(pb::ErrorCodeProto::ErrorCodeNormalClosure) => Ok(ErrorCode::NormalClosure),
        // ErrorCodeUnspecified (0) or any unrecognized number.
        _ => Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "unknown error code",
        )),
    }
}

// ---------------------------------------------------------------------------
// Message mappings.
// ---------------------------------------------------------------------------

/// Rust [`KeyframeRequest`] → wire. Total.
fn keyframe_to_pb(k: KeyframeRequest) -> pb::KeyframeRequest {
    pb::KeyframeRequest {
        since_frame: k.since_frame,
        reason: i32::from(reason_to_pb(k.reason)),
    }
}

/// Wire → Rust [`KeyframeRequest`]. Validates the reason enum.
fn keyframe_from_pb(k: pb::KeyframeRequest) -> Result<KeyframeRequest, RasError> {
    Ok(KeyframeRequest {
        since_frame: k.since_frame,
        reason: reason_from_pb(k.reason)?,
    })
}

/// Rust [`StreamConfigWire`] → wire. Total; `u8` tags widen to `u32`.
fn streamconfig_to_pb(s: StreamConfigWire) -> pb::StreamConfig {
    pb::StreamConfig {
        codec: s.codec,
        width: s.width,
        height: s.height,
        fps: s.fps,
        target_bitrate_bps: s.target_bitrate_bps,
        color: u32::from(s.color),
        video_transport: u32::from(s.video_transport),
    }
}

/// Wire → Rust [`StreamConfigWire`]. Range-checks the `u8` tags; `> u8::MAX` is rejected.
fn streamconfig_from_pb(s: pb::StreamConfig) -> Result<StreamConfigWire, RasError> {
    let color = u8::try_from(s.color)
        .map_err(|_| RasError::fatal(ErrorCode::InvalidMessage, "color tag out of u8 range"))?;
    let video_transport = u8::try_from(s.video_transport).map_err(|_| {
        RasError::fatal(
            ErrorCode::InvalidMessage,
            "video_transport tag out of u8 range",
        )
    })?;
    Ok(StreamConfigWire {
        codec: s.codec,
        width: s.width,
        height: s.height,
        fps: s.fps,
        target_bitrate_bps: s.target_bitrate_bps,
        color,
        video_transport,
    })
}

/// Rust [`DecoderFeedback`] → wire. Total; the `Option<KeyframeRequest>` maps 1:1.
fn feedback_to_pb(f: DecoderFeedback) -> pb::DecoderFeedback {
    pb::DecoderFeedback {
        last_decoded_frame: f.last_decoded_frame,
        frames_dropped: f.frames_dropped,
        decode_latency_us: f.decode_latency_us,
        keyframe_request: f.keyframe_request.map(keyframe_to_pb),
    }
}

/// Wire → Rust [`DecoderFeedback`]. Preserves the `Option` (None ⇔ absent, Some ⇔ present) and
/// validates the nested keyframe request's reason when present.
fn feedback_from_pb(f: pb::DecoderFeedback) -> Result<DecoderFeedback, RasError> {
    let keyframe_request = match f.keyframe_request {
        Some(k) => Some(keyframe_from_pb(k)?),
        None => None,
    };
    Ok(DecoderFeedback {
        last_decoded_frame: f.last_decoded_frame,
        frames_dropped: f.frames_dropped,
        decode_latency_us: f.decode_latency_us,
        keyframe_request,
    })
}

/// Rust [`ControlMsg`] → wire. Total and infallible (every value has a valid wire form).
fn control_to_pb(msg: ControlMsg) -> pb::ControlMsg {
    use pb::control_msg::Kind;
    let kind = match msg {
        ControlMsg::Hello { protocol_version } => Kind::Hello(pb::Hello { protocol_version }),
        ControlMsg::StreamConfig(s) => Kind::StreamConfig(streamconfig_to_pb(s)),
        ControlMsg::KeyframeRequest(k) => Kind::KeyframeRequest(keyframe_to_pb(k)),
        ControlMsg::Feedback(f) => Kind::Feedback(feedback_to_pb(f)),
        ControlMsg::AuthEnvelope { payload } => Kind::AuthEnvelope(pb::AuthEnvelope { payload }),
        ControlMsg::Bye { code } => Kind::Bye(pb::Bye {
            code: i32::from(errorcode_to_pb(code)),
        }),
        ControlMsg::Pointer(p) => Kind::Pointer(pb::PointerUpdate {
            x: u32::from(p.x),
            y: u32::from(p.y),
            visible: p.visible,
        }),
        ControlMsg::ControlRequest { capabilities } => {
            Kind::ControlRequest(pb::ControlRequest { capabilities })
        }
        ControlMsg::ControlGranted {
            lease_id,
            generation,
            capabilities,
            expires_at,
            signature,
        } => Kind::ControlGranted(pb::ControlGranted {
            lease_id: Bytes::copy_from_slice(&lease_id),
            generation,
            capabilities,
            expires_at,
            signature,
        }),
        ControlMsg::ControlRevoked { code } => Kind::ControlRevoked(pb::ControlRevoked {
            code: i32::from(errorcode_to_pb(code)),
        }),
        ControlMsg::Input(env) => Kind::Input(input_envelope_to_pb(env)),
    };
    pb::ControlMsg { kind: Some(kind) }
}

/// Wire → Rust [`ControlMsg`]. Partial: an unset oneof or any invalid enum/range is a typed
/// [`RasError`] with [`ErrorCode::InvalidMessage`].
fn control_from_pb(proto: pb::ControlMsg) -> Result<ControlMsg, RasError> {
    use pb::control_msg::Kind;
    match proto.kind {
        Some(Kind::Hello(h)) => Ok(ControlMsg::Hello {
            protocol_version: h.protocol_version,
        }),
        Some(Kind::StreamConfig(s)) => Ok(ControlMsg::StreamConfig(streamconfig_from_pb(s)?)),
        Some(Kind::KeyframeRequest(k)) => Ok(ControlMsg::KeyframeRequest(keyframe_from_pb(k)?)),
        Some(Kind::Feedback(f)) => Ok(ControlMsg::Feedback(feedback_from_pb(f)?)),
        Some(Kind::AuthEnvelope(a)) => Ok(ControlMsg::AuthEnvelope { payload: a.payload }),
        Some(Kind::Bye(b)) => Ok(ControlMsg::Bye {
            code: errorcode_from_pb(b.code)?,
        }),
        Some(Kind::Pointer(p)) => Ok(ControlMsg::Pointer(PointerUpdate {
            // u16 fixed-point on the wire as uint32: out-of-range is a malformed message.
            x: u16::try_from(p.x).map_err(|_| {
                RasError::fatal(ErrorCode::InvalidMessage, "pointer x out of range")
            })?,
            y: u16::try_from(p.y).map_err(|_| {
                RasError::fatal(ErrorCode::InvalidMessage, "pointer y out of range")
            })?,
            visible: p.visible,
        })),
        Some(Kind::ControlRequest(r)) => Ok(ControlMsg::ControlRequest {
            capabilities: validate_capabilities(r.capabilities)?,
        }),
        Some(Kind::ControlGranted(g)) => Ok(ControlMsg::ControlGranted {
            lease_id: arr16(&g.lease_id, "control_granted.lease_id")?,
            generation: g.generation,
            capabilities: validate_capabilities(g.capabilities)?,
            expires_at: g.expires_at,
            signature: g.signature,
        }),
        Some(Kind::ControlRevoked(r)) => Ok(ControlMsg::ControlRevoked {
            code: errorcode_from_pb(r.code)?,
        }),
        Some(Kind::Input(env)) => Ok(ControlMsg::Input(input_envelope_from_pb(env)?)),
        // No valid empty control message: unset oneof (empty bytes, or a future variant an old
        // build doesn't recognize) is rejected, never silently defaulted.
        None => Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "empty control message",
        )),
    }
}

/// Decode a wire `bytes` field to a fixed 16-byte lease id; wrong length → malformed message.
fn arr16(bytes: &Bytes, ctx: &'static str) -> Result<[u8; 16], RasError> {
    <[u8; 16]>::try_from(bytes.as_ref())
        .map_err(|_| RasError::fatal(ErrorCode::InvalidMessage, ctx))
}

/// Validate an untrusted capability list: bounded count and per-identifier length (DoS guard). The
/// identifiers themselves are recognized/denied later by `ras-policy` (unknown-denied); the codec
/// only enforces size.
fn validate_capabilities(caps: Vec<String>) -> Result<Vec<String>, RasError> {
    if caps.len() > MAX_CAPABILITIES {
        return Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "too many capabilities",
        ));
    }
    if caps.iter().any(|c| c.len() > MAX_CAPABILITY_LEN) {
        return Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "capability identifier too long",
        ));
    }
    Ok(caps)
}

fn pointerbutton_to_pb(b: PointerButton) -> pb::PointerButtonProto {
    match b {
        PointerButton::Left => pb::PointerButtonProto::PointerButtonLeft,
        PointerButton::Right => pb::PointerButtonProto::PointerButtonRight,
        PointerButton::Middle => pb::PointerButtonProto::PointerButtonMiddle,
    }
}

/// `i32` wire tag → `PointerButton`; UNSPECIFIED / unknown → malformed (never silently defaulted).
fn pointerbutton_from_pb(tag: i32) -> Result<PointerButton, RasError> {
    match pb::PointerButtonProto::try_from(tag) {
        Ok(pb::PointerButtonProto::PointerButtonLeft) => Ok(PointerButton::Left),
        Ok(pb::PointerButtonProto::PointerButtonRight) => Ok(PointerButton::Right),
        Ok(pb::PointerButtonProto::PointerButtonMiddle) => Ok(PointerButton::Middle),
        Ok(pb::PointerButtonProto::PointerButtonUnspecified) | Err(_) => Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "invalid pointer button",
        )),
    }
}

fn input_envelope_to_pb(env: InputEnvelope) -> pb::InputEnvelope {
    pb::InputEnvelope {
        lease_id: Bytes::copy_from_slice(&env.lease_id),
        generation: env.generation,
        seq: env.seq,
        action: Some(input_action_to_pb(env.action)),
    }
}

fn input_envelope_from_pb(env: pb::InputEnvelope) -> Result<InputEnvelope, RasError> {
    Ok(InputEnvelope {
        lease_id: arr16(&env.lease_id, "input.lease_id")?,
        generation: env.generation,
        seq: env.seq,
        action: input_action_from_pb(
            env.action
                .ok_or_else(|| RasError::fatal(ErrorCode::InvalidMessage, "input action unset"))?,
        )?,
    })
}

fn input_action_to_pb(a: InputAction) -> pb::InputAction {
    use pb::input_action::Action;
    let action = match a {
        InputAction::PointerMove {
            display_id,
            nx,
            ny,
            layout_version,
        } => Action::PointerMove(pb::PointerMove {
            display_id,
            nx: u32::from(nx),
            ny: u32::from(ny),
            layout_version,
        }),
        InputAction::PointerButton {
            display_id,
            nx,
            ny,
            layout_version,
            button,
            down,
        } => Action::PointerButton(pb::PointerButtonEvent {
            display_id,
            nx: u32::from(nx),
            ny: u32::from(ny),
            layout_version,
            button: i32::from(pointerbutton_to_pb(button)),
            down,
        }),
        InputAction::PointerWheel { dx, dy } => Action::PointerWheel(pb::PointerWheel {
            dx: i32::from(dx),
            dy: i32::from(dy),
        }),
        InputAction::KeyEvent {
            hid_usage,
            down,
            modifiers,
        } => Action::KeyEvent(pb::KeyEvent {
            hid_usage: u32::from(hid_usage),
            down,
            modifiers: u32::from(modifiers),
        }),
        InputAction::TextInput { utf8 } => Action::TextInput(pb::TextInput { utf8 }),
        InputAction::ReleaseAllKeys => Action::ReleaseAllKeys(pb::ReleaseAllKeys {}),
    };
    pb::InputAction {
        action: Some(action),
    }
}

/// Decode a normalized coordinate (`u16` fixed-point carried in `uint32`); out-of-range → malformed.
fn norm_coord(v: u32, ctx: &'static str) -> Result<u16, RasError> {
    u16::try_from(v).map_err(|_| RasError::fatal(ErrorCode::InvalidMessage, ctx))
}

fn input_action_from_pb(a: pb::InputAction) -> Result<InputAction, RasError> {
    use pb::input_action::Action;
    match a.action {
        Some(Action::PointerMove(m)) => Ok(InputAction::PointerMove {
            display_id: m.display_id,
            nx: norm_coord(m.nx, "pointer_move.nx out of range")?,
            ny: norm_coord(m.ny, "pointer_move.ny out of range")?,
            layout_version: m.layout_version,
        }),
        Some(Action::PointerButton(b)) => Ok(InputAction::PointerButton {
            display_id: b.display_id,
            nx: norm_coord(b.nx, "pointer_button.nx out of range")?,
            ny: norm_coord(b.ny, "pointer_button.ny out of range")?,
            layout_version: b.layout_version,
            button: pointerbutton_from_pb(b.button)?,
            down: b.down,
        }),
        Some(Action::PointerWheel(w)) => Ok(InputAction::PointerWheel {
            dx: i16::try_from(w.dx)
                .map_err(|_| RasError::fatal(ErrorCode::InvalidMessage, "wheel dx out of range"))?,
            dy: i16::try_from(w.dy)
                .map_err(|_| RasError::fatal(ErrorCode::InvalidMessage, "wheel dy out of range"))?,
        }),
        Some(Action::KeyEvent(k)) => Ok(InputAction::KeyEvent {
            hid_usage: u16::try_from(k.hid_usage).map_err(|_| {
                RasError::fatal(ErrorCode::InvalidMessage, "key hid_usage out of range")
            })?,
            down: k.down,
            modifiers: u8::try_from(k.modifiers).map_err(|_| {
                RasError::fatal(ErrorCode::InvalidMessage, "key modifiers out of range")
            })?,
        }),
        Some(Action::TextInput(t)) => {
            if t.utf8.len() > MAX_TEXT_INPUT {
                return Err(RasError::fatal(
                    ErrorCode::InvalidMessage,
                    "text input too long",
                ));
            }
            Ok(InputAction::TextInput { utf8: t.utf8 })
        }
        Some(Action::ReleaseAllKeys(_)) => Ok(InputAction::ReleaseAllKeys),
        None => Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "input action unset",
        )),
    }
}

// ---------------------------------------------------------------------------
// Bootstrap-channel mappings (Phase-2 authorization handshake).
// ---------------------------------------------------------------------------

/// Decode a wire `bytes` field to a fixed 32-byte Ed25519 key; wrong length → malformed message.
/// Fail-closed: a key of any other length can never be silently truncated or zero-padded.
fn arr32(bytes: &Bytes, ctx: &'static str) -> Result<[u8; 32], RasError> {
    <[u8; 32]>::try_from(bytes.as_ref())
        .map_err(|_| RasError::fatal(ErrorCode::InvalidMessage, ctx))
}

/// Rust [`BootstrapMsg`] → wire. Total and infallible (every value has a valid wire form). Fixed
/// 32-byte ids widen to `bytes`; the `AssuranceTier` tag widens `u8` → `u32`.
fn bootstrap_to_pb(msg: BootstrapMsg) -> pb::BootstrapMsg {
    use pb::bootstrap_msg::Kind;
    let kind = match msg {
        BootstrapMsg::ClientHello { protocol_version } => {
            Kind::ClientHello(pb::ClientHello { protocol_version })
        }
        BootstrapMsg::HostHello { host_id, tier } => Kind::HostHello(pb::HostHello {
            host_id: Bytes::copy_from_slice(&host_id),
            tier: u32::from(tier),
        }),
        BootstrapMsg::PairingRequest {
            controller_id,
            display_name,
            pubkey,
            signature,
        } => Kind::PairingRequest(pb::PairingRequest {
            controller_id: Bytes::copy_from_slice(&controller_id),
            display_name,
            pubkey: Bytes::copy_from_slice(&pubkey),
            signature,
        }),
        BootstrapMsg::PairingDecision { accepted } => {
            Kind::PairingDecision(pb::PairingDecision { accepted })
        }
        BootstrapMsg::AccessRequest { canonical } => {
            Kind::AccessRequest(pb::AccessRequestMsg { canonical })
        }
        BootstrapMsg::AccessDecision(outcome) => {
            let decision = match outcome {
                // Allowed: grant present, denied left UNSPECIFIED (0).
                AccessOutcome::Allowed { grant } => pb::AccessDecision {
                    grant: Some(grant),
                    denied: i32::from(pb::ErrorCodeProto::ErrorCodeUnspecified),
                },
                // Denied: no grant, a concrete non-UNSPECIFIED reason.
                AccessOutcome::Denied { code } => pb::AccessDecision {
                    grant: None,
                    denied: i32::from(errorcode_to_pb(code)),
                },
            };
            Kind::AccessDecision(decision)
        }
        BootstrapMsg::CancelRequest => Kind::CancelRequest(pb::CancelRequest {}),
        BootstrapMsg::ProtocolError { code } => Kind::ProtocolError(pb::ProtocolError {
            code: i32::from(errorcode_to_pb(code)),
        }),
    };
    pb::BootstrapMsg { kind: Some(kind) }
}

/// Wire → Rust [`BootstrapMsg`]. Partial: unset oneof, wrong-length id, over-long display name, an
/// out-of-range tier tag, or a malformed [`AccessOutcome`] is a typed [`RasError`]
/// ([`ErrorCode::InvalidMessage`], except the mapped [`ErrorCode`]s which round-trip exactly).
fn bootstrap_from_pb(proto: pb::BootstrapMsg) -> Result<BootstrapMsg, RasError> {
    use pb::bootstrap_msg::Kind;
    match proto.kind {
        Some(Kind::ClientHello(h)) => Ok(BootstrapMsg::ClientHello {
            protocol_version: h.protocol_version,
        }),
        Some(Kind::HostHello(h)) => {
            let tier = u8::try_from(h.tier)
                .ok()
                .filter(|t| *t <= 3)
                .ok_or_else(|| {
                    RasError::fatal(ErrorCode::InvalidMessage, "tier tag out of range")
                })?;
            Ok(BootstrapMsg::HostHello {
                host_id: arr32(&h.host_id, "host_id not 32 bytes")?,
                tier,
            })
        }
        Some(Kind::PairingRequest(p)) => {
            if p.display_name.len() > MAX_DISPLAY_NAME {
                return Err(RasError::fatal(
                    ErrorCode::InvalidMessage,
                    "display name too long",
                ));
            }
            Ok(BootstrapMsg::PairingRequest {
                controller_id: arr32(&p.controller_id, "controller_id not 32 bytes")?,
                display_name: p.display_name,
                pubkey: arr32(&p.pubkey, "pubkey not 32 bytes")?,
                signature: p.signature,
            })
        }
        Some(Kind::PairingDecision(d)) => Ok(BootstrapMsg::PairingDecision {
            accepted: d.accepted,
        }),
        Some(Kind::AccessRequest(a)) => Ok(BootstrapMsg::AccessRequest {
            canonical: a.canonical,
        }),
        Some(Kind::AccessDecision(d)) => {
            // Exactly one of {grant, denied}: reject both-set and neither-set. `denied` UNSPECIFIED
            // (0) means "no denial"; any other value is a concrete reason.
            let denied_set = d.denied != i32::from(pb::ErrorCodeProto::ErrorCodeUnspecified);
            match (d.grant, denied_set) {
                (Some(grant), false) => Ok(BootstrapMsg::AccessDecision(AccessOutcome::Allowed {
                    grant,
                })),
                (None, true) => Ok(BootstrapMsg::AccessDecision(AccessOutcome::Denied {
                    code: errorcode_from_pb(d.denied)?,
                })),
                _ => Err(RasError::fatal(
                    ErrorCode::InvalidMessage,
                    "access decision must set exactly one of grant/denied",
                )),
            }
        }
        Some(Kind::CancelRequest(_)) => Ok(BootstrapMsg::CancelRequest),
        Some(Kind::ProtocolError(e)) => Ok(BootstrapMsg::ProtocolError {
            code: errorcode_from_pb(e.code)?,
        }),
        None => Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "empty bootstrap message",
        )),
    }
}

// ---------------------------------------------------------------------------
// Public codec API — synchronous.
// ---------------------------------------------------------------------------

/// Serialize one [`ControlMsg`] to protobuf bytes (no length prefix).
///
/// Infallible: `prost::Message::encode_to_vec` cannot fail, so there is no `unwrap`/`expect`.
#[must_use]
pub fn encode(msg: &ControlMsg) -> Bytes {
    Bytes::from(control_to_pb(msg.clone()).encode_to_vec())
}

/// Decode one protobuf [`ControlMsg`] (no length prefix).
///
/// Every malformed input — bad protobuf, unset oneof, `UNSPECIFIED`/unknown enum, out-of-range
/// `u8` tag — returns a typed [`RasError`] with [`ErrorCode::InvalidMessage`]. Never panics; the
/// error `context` never embeds decoded bytes (Invariant 8).
pub fn decode(bytes: &[u8]) -> Result<ControlMsg, RasError> {
    let proto = pb::ControlMsg::decode(bytes)
        .map_err(|_| RasError::fatal(ErrorCode::InvalidMessage, "control decode failed"))?;
    control_from_pb(proto)
}

/// Frame one message: 4-byte big-endian length prefix + protobuf body.
///
/// Control messages are structurally tiny (config/feedback), so the body length always fits a
/// `u32`; the receiver enforces [`MAX_CONTROL_FRAME`] on the way in.
#[must_use]
pub fn frame(msg: &ControlMsg) -> Bytes {
    let body = encode(msg);
    let mut out = BytesMut::with_capacity(4 + body.len());
    // Lossless: control frames are KiB-scale, far below u32::MAX. Saturate defensively rather than
    // wrap, so a pathological body can never encode a truncated length prefix.
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    out.put_u32(len);
    out.put_slice(&body);
    out.freeze()
}

/// Try to read one framed message from `buf`, consuming exactly one frame's bytes on success.
///
/// Returns:
/// * `Ok(Some(msg))` — a full frame was present and consumed from `buf`.
/// * `Ok(None)` — need more bytes (prefix or body incomplete); `buf` is left **untouched**.
/// * `Err(RasError)` — the length prefix exceeds [`MAX_CONTROL_FRAME`] (DoS guard), or the framed
///   body is malformed. Both carry [`ErrorCode::InvalidMessage`].
///
/// The DoS guard fires on the length prefix **before** waiting for or allocating the body, so a
/// hostile header claiming gigabytes is rejected immediately.
pub fn try_read_frame(buf: &mut BytesMut) -> Result<Option<ControlMsg>, RasError> {
    if buf.len() < 4 {
        return Ok(None); // not even a length prefix yet
    }
    // Peek the length without consuming, so a partial body doesn't lose the prefix.
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_CONTROL_FRAME {
        // DoS guard: reject BEFORE waiting for / allocating `len` bytes.
        return Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "control frame too large",
        ));
    }
    if buf.len() < 4 + len {
        return Ok(None); // full body not yet buffered
    }
    buf.advance(4); // consume prefix
    let body = buf.split_to(len); // consume body
    decode(&body).map(Some)
}

/// Serialize one [`BootstrapMsg`] to protobuf bytes (no length prefix). Infallible.
#[must_use]
pub fn encode_bootstrap(msg: &BootstrapMsg) -> Bytes {
    Bytes::from(bootstrap_to_pb(msg.clone()).encode_to_vec())
}

/// Decode one protobuf [`BootstrapMsg`] (no length prefix).
///
/// Every malformed input — bad protobuf, unset oneof, wrong-length id, over-long display name,
/// out-of-range tier, a both-set/neither-set access decision, `UNSPECIFIED`/unknown enum — returns
/// a typed [`RasError`]. Never panics; error `context` never embeds decoded bytes (Invariant 8).
pub fn decode_bootstrap(bytes: &[u8]) -> Result<BootstrapMsg, RasError> {
    let proto = pb::BootstrapMsg::decode(bytes)
        .map_err(|_| RasError::fatal(ErrorCode::InvalidMessage, "bootstrap decode failed"))?;
    bootstrap_from_pb(proto)
}

/// Frame one [`BootstrapMsg`]: 4-byte big-endian length prefix + protobuf body. Same framing and
/// [`MAX_CONTROL_FRAME`] DoS guard as the session control channel.
#[must_use]
pub fn frame_bootstrap(msg: &BootstrapMsg) -> Bytes {
    let body = encode_bootstrap(msg);
    let mut out = BytesMut::with_capacity(4 + body.len());
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    out.put_u32(len);
    out.put_slice(&body);
    out.freeze()
}

/// Try to read one framed [`BootstrapMsg`] from `buf`, consuming exactly one frame on success.
///
/// Same contract as [`try_read_frame`]: `Ok(Some)` on a full frame (consumed), `Ok(None)` if more
/// bytes are needed (`buf` untouched), `Err` if the length prefix exceeds [`MAX_CONTROL_FRAME`]
/// (DoS guard, before allocation) or the body is malformed.
pub fn try_read_bootstrap_frame(buf: &mut BytesMut) -> Result<Option<BootstrapMsg>, RasError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_CONTROL_FRAME {
        return Err(RasError::fatal(
            ErrorCode::InvalidMessage,
            "bootstrap frame too large",
        ));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    buf.advance(4);
    let body = buf.split_to(len);
    decode_bootstrap(&body).map(Some)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const ALL_REASONS: [KeyframeReason; 5] = [
        KeyframeReason::StreamStart,
        KeyframeReason::UnrecoverableLoss,
        KeyframeReason::DecoderReset,
        KeyframeReason::ConfigChanged,
        KeyframeReason::PeriodicRefresh,
    ];

    const ALL_CODES: [ErrorCode; 18] = [
        ErrorCode::InvalidMessage,
        ErrorCode::UnsupportedVersion,
        ErrorCode::IdentityMismatch,
        ErrorCode::SignatureInvalid,
        ErrorCode::RequestExpired,
        ErrorCode::ReplayDetected,
        ErrorCode::ConsentDenied,
        ErrorCode::CapabilityDenied,
        ErrorCode::GrantInvalid,
        ErrorCode::LeaseInvalid,
        ErrorCode::SessionRevoked,
        ErrorCode::TransportError,
        ErrorCode::CaptureFailed,
        ErrorCode::EncoderFailed,
        ErrorCode::InputFailed,
        ErrorCode::PolicyChanged,
        ErrorCode::Internal,
        ErrorCode::NormalClosure,
    ];

    /// encode → decode is the identity for a message. Requires `ControlMsg: PartialEq` — provided
    /// by comparing via re-encode where the enum isn't `PartialEq`.
    fn assert_roundtrip(msg: &ControlMsg) {
        let decoded = decode(&encode(msg)).expect("roundtrip decode");
        // ControlMsg isn't PartialEq, so compare the canonical wire bytes both ways.
        assert_eq!(
            encode(&decoded).as_ref(),
            encode(msg).as_ref(),
            "re-encode mismatch"
        );
    }

    #[test]
    fn roundtrip_hello() {
        for v in [0u32, 1, u32::MAX] {
            assert_roundtrip(&ControlMsg::Hello {
                protocol_version: v,
            });
        }
    }

    #[test]
    fn roundtrip_stream_config() {
        for (color, vt) in [(0u8, 0u8), (1, 1), (255, 255)] {
            let m = ControlMsg::StreamConfig(StreamConfigWire {
                codec: "avc1.4D401F—ünïcode".to_string(),
                width: 1920,
                height: 1080,
                fps: 60,
                target_bitrate_bps: 8_000_000,
                color,
                video_transport: vt,
            });
            assert_roundtrip(&m);
            // Also verify the decoded value matches field-by-field.
            let decoded = decode(&encode(&m)).unwrap();
            match decoded {
                ControlMsg::StreamConfig(w) => {
                    assert_eq!(w.color, color);
                    assert_eq!(w.video_transport, vt);
                    assert_eq!(w.width, 1920);
                }
                _ => panic!("wrong variant"),
            }
        }
    }

    #[test]
    fn roundtrip_keyframe_request() {
        for reason in ALL_REASONS {
            for since in [0u64, 41, u64::MAX] {
                let m = ControlMsg::KeyframeRequest(KeyframeRequest {
                    since_frame: since,
                    reason,
                });
                assert_roundtrip(&m);
                let decoded = decode(&encode(&m)).unwrap();
                match decoded {
                    ControlMsg::KeyframeRequest(k) => {
                        assert_eq!(k.reason, reason);
                        assert_eq!(k.since_frame, since);
                    }
                    _ => panic!("wrong variant"),
                }
            }
        }
    }

    #[test]
    fn roundtrip_feedback_with_and_without_keyframe() {
        let none = ControlMsg::Feedback(DecoderFeedback {
            last_decoded_frame: 100,
            frames_dropped: 2,
            decode_latency_us: 5000,
            keyframe_request: None,
        });
        assert_roundtrip(&none);
        match decode(&encode(&none)).unwrap() {
            ControlMsg::Feedback(f) => assert!(f.keyframe_request.is_none()),
            _ => panic!("wrong variant"),
        }

        let some = ControlMsg::Feedback(DecoderFeedback {
            last_decoded_frame: 200,
            frames_dropped: 0,
            decode_latency_us: 3000,
            keyframe_request: Some(KeyframeRequest {
                since_frame: 199,
                reason: KeyframeReason::DecoderReset,
            }),
        });
        assert_roundtrip(&some);
        match decode(&encode(&some)).unwrap() {
            ControlMsg::Feedback(f) => {
                let k = f.keyframe_request.expect("Some survives");
                assert_eq!(k.reason, KeyframeReason::DecoderReset);
                assert_eq!(k.since_frame, 199);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_auth_envelope() {
        for payload in [
            Bytes::new(),
            Bytes::from_static(&[0x00]),
            Bytes::from((0u32..65536).map(|i| (i % 256) as u8).collect::<Vec<u8>>()),
        ] {
            let m = ControlMsg::AuthEnvelope {
                payload: payload.clone(),
            };
            assert_roundtrip(&m);
            // Empty-payload AuthEnvelope must decode back to the AuthEnvelope variant (proto3
            // oneof discriminant survives a default inner scalar), byte-exact.
            match decode(&encode(&m)).unwrap() {
                ControlMsg::AuthEnvelope { payload: got } => assert_eq!(got, payload),
                _ => panic!("wrong variant"),
            }
        }
    }

    #[test]
    fn roundtrip_bye_all_error_codes() {
        let mut seen_tags = std::collections::HashSet::new();
        for code in ALL_CODES {
            let m = ControlMsg::Bye { code };
            assert_roundtrip(&m);
            match decode(&encode(&m)).unwrap() {
                ControlMsg::Bye { code: got } => assert_eq!(got, code),
                _ => panic!("wrong variant"),
            }
            // Each code maps to a distinct wire tag (guards accidental renumber/collision).
            let tag = i32::from(errorcode_to_pb(code));
            assert!(seen_tags.insert(tag), "duplicate wire tag for {code:?}");
        }
        assert_eq!(seen_tags.len(), ALL_CODES.len());
    }

    #[test]
    fn roundtrip_pointer() {
        use crate::PointerUpdate;
        for (x, y, visible) in [
            (0u16, 0u16, true),
            (65535, 65535, false),
            (12345, 54321, true),
        ] {
            let m = ControlMsg::Pointer(PointerUpdate { x, y, visible });
            assert_roundtrip(&m);
            match decode(&encode(&m)).unwrap() {
                ControlMsg::Pointer(p) => {
                    assert_eq!(p.x, x);
                    assert_eq!(p.y, y);
                    assert_eq!(p.visible, visible);
                }
                _ => panic!("wrong variant"),
            }
        }
    }

    #[test]
    fn roundtrip_control_request_and_granted() {
        use crate::InputEnvelope;
        let req = ControlMsg::ControlRequest {
            capabilities: vec!["pointer.move".to_string(), "keyboard.key".to_string()],
        };
        assert_roundtrip(&req);

        let granted = ControlMsg::ControlGranted {
            lease_id: [7u8; 16],
            generation: 42,
            capabilities: vec!["pointer.move".to_string()],
            expires_at: 1_700_000_000_000,
            signature: Bytes::from_static(&[1, 2, 3, 4]),
        };
        assert_roundtrip(&granted);
        match decode(&encode(&granted)).unwrap() {
            ControlMsg::ControlGranted {
                lease_id,
                generation,
                capabilities,
                expires_at,
                signature,
            } => {
                assert_eq!(lease_id, [7u8; 16]);
                assert_eq!(generation, 42);
                assert_eq!(capabilities, vec!["pointer.move".to_string()]);
                assert_eq!(expires_at, 1_700_000_000_000);
                assert_eq!(signature.as_ref(), &[1, 2, 3, 4]);
            }
            _ => panic!("wrong variant"),
        }

        let revoked = ControlMsg::ControlRevoked {
            code: ErrorCode::ConsentDenied,
        };
        assert_roundtrip(&revoked);

        // A minimal ReleaseAllKeys envelope also round-trips (empty inner action message).
        let rel = ControlMsg::Input(InputEnvelope {
            lease_id: [0u8; 16],
            generation: 1,
            seq: 9,
            action: InputAction::ReleaseAllKeys,
        });
        assert_roundtrip(&rel);
    }

    #[test]
    fn roundtrip_all_input_actions() {
        use crate::InputEnvelope;
        let actions = [
            InputAction::PointerMove {
                display_id: 3,
                nx: 0,
                ny: 65535,
                layout_version: 5,
            },
            InputAction::PointerButton {
                display_id: 0,
                nx: 32000,
                ny: 100,
                layout_version: 1,
                button: PointerButton::Right,
                down: true,
            },
            InputAction::PointerWheel { dx: -120, dy: 240 },
            InputAction::KeyEvent {
                hid_usage: 0x04, // 'a'
                down: true,
                modifiers: 0b0000_0010,
            },
            InputAction::TextInput {
                utf8: "héllo, 世界".to_string(),
            },
            InputAction::ReleaseAllKeys,
        ];
        for (i, action) in actions.into_iter().enumerate() {
            let m = ControlMsg::Input(InputEnvelope {
                lease_id: [(i as u8); 16],
                generation: i as u32,
                seq: i as u64 * 1000,
                action: action.clone(),
            });
            assert_roundtrip(&m);
            match decode(&encode(&m)).unwrap() {
                ControlMsg::Input(env) => assert_eq!(env.action, action),
                _ => panic!("wrong variant"),
            }
        }
    }

    #[test]
    fn input_coordinate_and_button_out_of_range_are_rejected() {
        // nx > u16::MAX in the wire uint32 → InvalidMessage (fail-closed, never truncated).
        let bad_coord = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::Input(pb::InputEnvelope {
                lease_id: Bytes::copy_from_slice(&[0u8; 16]),
                generation: 0,
                seq: 0,
                action: Some(pb::InputAction {
                    action: Some(pb::input_action::Action::PointerMove(pb::PointerMove {
                        display_id: 0,
                        nx: 70_000, // > 65535
                        ny: 0,
                        layout_version: 0,
                    })),
                }),
            })),
        };
        assert_eq!(
            control_from_pb(bad_coord).unwrap_err().code,
            ErrorCode::InvalidMessage
        );

        // UNSPECIFIED button tag → InvalidMessage (never silently defaulted to Left).
        let bad_button = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::Input(pb::InputEnvelope {
                lease_id: Bytes::copy_from_slice(&[0u8; 16]),
                generation: 0,
                seq: 0,
                action: Some(pb::InputAction {
                    action: Some(pb::input_action::Action::PointerButton(
                        pb::PointerButtonEvent {
                            display_id: 0,
                            nx: 0,
                            ny: 0,
                            layout_version: 0,
                            button: 0, // UNSPECIFIED
                            down: true,
                        },
                    )),
                }),
            })),
        };
        assert_eq!(
            control_from_pb(bad_button).unwrap_err().code,
            ErrorCode::InvalidMessage
        );
    }

    #[test]
    fn oversized_text_capabilities_and_lease_id_are_rejected() {
        // Text input over MAX_TEXT_INPUT → rejected.
        let long_text = "x".repeat(MAX_TEXT_INPUT + 1);
        let m = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::Input(pb::InputEnvelope {
                lease_id: Bytes::copy_from_slice(&[0u8; 16]),
                generation: 0,
                seq: 0,
                action: Some(pb::InputAction {
                    action: Some(pb::input_action::Action::TextInput(pb::TextInput {
                        utf8: long_text,
                    })),
                }),
            })),
        };
        assert_eq!(
            control_from_pb(m).unwrap_err().code,
            ErrorCode::InvalidMessage
        );

        // A 15-byte lease id (wrong length) → rejected, never zero-padded.
        let short_lease = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::Input(pb::InputEnvelope {
                lease_id: Bytes::copy_from_slice(&[0u8; 15]),
                generation: 0,
                seq: 0,
                action: Some(pb::InputAction {
                    action: Some(pb::input_action::Action::ReleaseAllKeys(
                        pb::ReleaseAllKeys {},
                    )),
                }),
            })),
        };
        assert_eq!(
            control_from_pb(short_lease).unwrap_err().code,
            ErrorCode::InvalidMessage
        );

        // Too many capabilities → rejected.
        let too_many = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::ControlRequest(pb::ControlRequest {
                capabilities: (0..=MAX_CAPABILITIES).map(|i| format!("c.{i}")).collect(),
            })),
        };
        assert_eq!(
            control_from_pb(too_many).unwrap_err().code,
            ErrorCode::InvalidMessage
        );
    }

    #[test]
    fn input_action_with_unset_oneof_is_rejected() {
        // An InputEnvelope with no action set (unset nested oneof) → InvalidMessage, never defaulted.
        let m = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::Input(pb::InputEnvelope {
                lease_id: Bytes::copy_from_slice(&[0u8; 16]),
                generation: 0,
                seq: 0,
                action: None,
            })),
        };
        assert_eq!(
            control_from_pb(m).unwrap_err().code,
            ErrorCode::InvalidMessage
        );
    }

    #[test]
    fn frame_roundtrip() {
        let m = ControlMsg::Hello {
            protocol_version: 7,
        };
        let framed = frame(&m);
        let mut buf = BytesMut::from(framed.as_ref());
        let got = try_read_frame(&mut buf).expect("ok").expect("some");
        assert_eq!(encode(&got).as_ref(), encode(&m).as_ref());
        assert!(buf.is_empty(), "frame fully consumed");
    }

    #[test]
    fn frame_then_extra_bytes() {
        let a = ControlMsg::Hello {
            protocol_version: 1,
        };
        let b = ControlMsg::Bye {
            code: ErrorCode::SessionRevoked,
        };
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&frame(&a));
        buf.extend_from_slice(&frame(&b));

        let first = try_read_frame(&mut buf).expect("ok").expect("some");
        assert_eq!(encode(&first).as_ref(), encode(&a).as_ref());
        // The second frame remains buffered.
        let second = try_read_frame(&mut buf).expect("ok").expect("some");
        assert_eq!(encode(&second).as_ref(), encode(&b).as_ref());
        assert!(buf.is_empty());
    }

    #[test]
    fn decode_garbage_bytes() {
        let err = decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[test]
    fn decode_empty_is_unset_oneof() {
        // Empty bytes decode to a ControlMsg with an unset oneof → rejected.
        let err = decode(&[]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
        assert_eq!(err.context, "empty control message");
    }

    #[test]
    fn decode_unknown_oneof_field() {
        // A ControlMsg whose only set field is an unknown tag (99). prost drops the unknown field,
        // leaving an unset oneof → rejected as InvalidMessage.
        let mut buf = BytesMut::new();
        // field 99, wire type 0 (varint): tag = (99 << 3) | 0 = 792 => varint [0x98, 0x06]
        buf.put_u8(0x98);
        buf.put_u8(0x06);
        buf.put_u8(0x01); // varint value
        let err = decode(&buf).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[test]
    fn decode_unspecified_and_unknown_enum() {
        // Bye with code = ERROR_CODE_UNSPECIFIED (0) → rejected.
        let unspecified = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::Bye(pb::Bye { code: 0 })),
        };
        let err = decode(&unspecified.encode_to_vec()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);

        // Bye with an out-of-range code (999) → rejected.
        let unknown = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::Bye(pb::Bye { code: 999 })),
        };
        let err = decode(&unknown.encode_to_vec()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);

        // KeyframeRequest with reason = 0 (UNSPECIFIED) → rejected.
        let reason_unspec = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::KeyframeRequest(
                pb::KeyframeRequest {
                    since_frame: 1,
                    reason: 0,
                },
            )),
        };
        let err = decode(&reason_unspec.encode_to_vec()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[test]
    fn decode_out_of_range_u8_tag() {
        let m = pb::ControlMsg {
            kind: Some(pb::control_msg::Kind::StreamConfig(pb::StreamConfig {
                codec: "avc1".to_string(),
                width: 1,
                height: 1,
                fps: 1,
                target_bitrate_bps: 1,
                color: 256, // out of u8 range
                video_transport: 0,
            })),
        };
        let err = decode(&m.encode_to_vec()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[test]
    fn try_read_frame_oversized_rejected_on_header() {
        // A length prefix of MAX_CONTROL_FRAME + 1 with NO body present → rejected on the header.
        let mut buf = BytesMut::new();
        let oversized = u32::try_from(MAX_CONTROL_FRAME + 1).unwrap();
        buf.put_u32(oversized);
        // Intentionally no body bytes.
        let err = try_read_frame(&mut buf).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
        assert_eq!(err.context, "control frame too large");
    }

    #[test]
    fn try_read_frame_partial_leaves_buf_intact() {
        // < 4 bytes → Ok(None), buf untouched.
        let mut buf = BytesMut::from(&[0x00, 0x00][..]);
        let before = buf.len();
        assert!(try_read_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf.len(), before);

        // Valid prefix, short body → Ok(None), buf untouched (prefix not lost).
        let mut buf = BytesMut::new();
        buf.put_u32(10); // claims 10 bytes
        buf.put_slice(&[1, 2, 3]); // only 3 present
        let before = buf.len();
        assert!(try_read_frame(&mut buf).unwrap().is_none());
        assert_eq!(buf.len(), before);
    }

    #[test]
    fn try_read_frame_truncated_body_consumes_and_errors() {
        // Full-length prefix, body present but garbage → Err, and the frame is consumed.
        let body = [0xFFu8, 0xFF, 0xFF, 0xFF];
        let mut buf = BytesMut::new();
        buf.put_u32(u32::try_from(body.len()).unwrap());
        buf.put_slice(&body);
        let err = try_read_frame(&mut buf).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
        assert!(buf.is_empty(), "malformed frame is consumed");
    }

    #[test]
    fn wire_tags_are_stable() {
        // Regression fence: exact wire numbers must never drift.
        assert_eq!(i32::from(errorcode_to_pb(ErrorCode::InvalidMessage)), 1);
        assert_eq!(i32::from(errorcode_to_pb(ErrorCode::SignatureInvalid)), 4);
        assert_eq!(i32::from(errorcode_to_pb(ErrorCode::Internal)), 17);
        assert_eq!(i32::from(reason_to_pb(KeyframeReason::StreamStart)), 1);
        assert_eq!(i32::from(reason_to_pb(KeyframeReason::PeriodicRefresh)), 5);
    }

    // ── Bootstrap-channel codec ────────────────────────────────────────────────────────────────

    /// encode → decode is the identity for a bootstrap message (compared via re-encode).
    fn assert_bootstrap_roundtrip(msg: &BootstrapMsg) {
        let decoded = decode_bootstrap(&encode_bootstrap(msg)).expect("roundtrip decode");
        assert_eq!(
            encode_bootstrap(&decoded).as_ref(),
            encode_bootstrap(msg).as_ref(),
            "re-encode mismatch"
        );
    }

    #[test]
    fn roundtrip_all_bootstrap_variants() {
        let msgs = [
            BootstrapMsg::ClientHello {
                protocol_version: 1,
            },
            BootstrapMsg::HostHello {
                host_id: [0xAB; 32],
                tier: 0,
            },
            BootstrapMsg::HostHello {
                host_id: [0x01; 32],
                tier: 3,
            },
            BootstrapMsg::PairingRequest {
                controller_id: [0x22; 32],
                display_name: "Alice's Laptop — ünïcode".to_string(),
                pubkey: [0x33; 32],
                signature: Bytes::from_static(&[9u8; 64]),
            },
            BootstrapMsg::PairingDecision { accepted: true },
            BootstrapMsg::PairingDecision { accepted: false },
            BootstrapMsg::AccessRequest {
                canonical: Bytes::from_static(b"opaque-signed-access-request"),
            },
            BootstrapMsg::AccessDecision(AccessOutcome::Allowed {
                grant: Bytes::from_static(b"v4.public.opaque-paseto"),
            }),
            BootstrapMsg::AccessDecision(AccessOutcome::Denied {
                code: ErrorCode::ConsentDenied,
            }),
            BootstrapMsg::CancelRequest,
            BootstrapMsg::ProtocolError {
                code: ErrorCode::UnsupportedVersion,
            },
        ];
        for m in &msgs {
            assert_bootstrap_roundtrip(m);
        }
    }

    #[test]
    fn bootstrap_frame_roundtrips_and_consumes() {
        let m = BootstrapMsg::ClientHello {
            protocol_version: 7,
        };
        let mut buf = BytesMut::from(frame_bootstrap(&m).as_ref());
        let got = try_read_bootstrap_frame(&mut buf).unwrap().unwrap();
        assert_eq!(
            encode_bootstrap(&got).as_ref(),
            encode_bootstrap(&m).as_ref()
        );
        assert!(buf.is_empty(), "frame fully consumed");
    }

    #[test]
    fn bootstrap_rejects_wrong_length_id() {
        // Hand-build a HostHello proto with a 31-byte host_id → must fail closed.
        let proto = pb::BootstrapMsg {
            kind: Some(pb::bootstrap_msg::Kind::HostHello(pb::HostHello {
                host_id: Bytes::from_static(&[0u8; 31]),
                tier: 0,
            })),
        };
        let bytes = proto.encode_to_vec();
        let err = decode_bootstrap(&bytes).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[test]
    fn bootstrap_rejects_out_of_range_tier() {
        let proto = pb::BootstrapMsg {
            kind: Some(pb::bootstrap_msg::Kind::HostHello(pb::HostHello {
                host_id: Bytes::from_static(&[0u8; 32]),
                tier: 4, // only 0..=3 are valid tiers
            })),
        };
        let err = decode_bootstrap(&proto.encode_to_vec()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[test]
    fn bootstrap_rejects_overlong_display_name() {
        let proto = pb::BootstrapMsg {
            kind: Some(pb::bootstrap_msg::Kind::PairingRequest(
                pb::PairingRequest {
                    controller_id: Bytes::from_static(&[0u8; 32]),
                    display_name: "x".repeat(MAX_DISPLAY_NAME + 1),
                    pubkey: Bytes::from_static(&[0u8; 32]),
                    signature: Bytes::new(),
                },
            )),
        };
        let err = decode_bootstrap(&proto.encode_to_vec()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[test]
    fn access_decision_rejects_both_and_neither() {
        // Both set → ambiguous → reject.
        let both = pb::BootstrapMsg {
            kind: Some(pb::bootstrap_msg::Kind::AccessDecision(
                pb::AccessDecision {
                    grant: Some(Bytes::from_static(b"g")),
                    denied: i32::from(pb::ErrorCodeProto::ErrorCodeConsentDenied),
                },
            )),
        };
        assert_eq!(
            decode_bootstrap(&both.encode_to_vec()).unwrap_err().code,
            ErrorCode::InvalidMessage
        );
        // Neither set (no grant, denied UNSPECIFIED) → reject.
        let neither = pb::BootstrapMsg {
            kind: Some(pb::bootstrap_msg::Kind::AccessDecision(
                pb::AccessDecision {
                    grant: None,
                    denied: i32::from(pb::ErrorCodeProto::ErrorCodeUnspecified),
                },
            )),
        };
        assert_eq!(
            decode_bootstrap(&neither.encode_to_vec()).unwrap_err().code,
            ErrorCode::InvalidMessage
        );
    }

    #[test]
    fn bootstrap_empty_oneof_is_rejected() {
        let empty = pb::BootstrapMsg { kind: None };
        let err = decode_bootstrap(&empty.encode_to_vec()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }

    #[test]
    fn bootstrap_frame_guard_rejects_oversize_prefix() {
        let mut buf = BytesMut::new();
        buf.put_u32(u32::try_from(MAX_CONTROL_FRAME + 1).unwrap());
        buf.put_slice(&[0u8; 8]);
        let err = try_read_bootstrap_frame(&mut buf).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidMessage);
    }
}

/// Generative (property) tests. The control codec parses **untrusted** bytes off the wire, so the
/// load-bearing properties are: (1) `decode`/`try_read_frame` never panic on *any* input, and
/// (2) `decode(encode(m))` is the identity for every well-formed `ControlMsg`.
#[cfg(test)]
mod proptests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use proptest::prelude::*;

    fn arb_reason() -> impl Strategy<Value = KeyframeReason> {
        prop_oneof![
            Just(KeyframeReason::StreamStart),
            Just(KeyframeReason::UnrecoverableLoss),
            Just(KeyframeReason::DecoderReset),
            Just(KeyframeReason::ConfigChanged),
            Just(KeyframeReason::PeriodicRefresh),
        ]
    }

    fn arb_code() -> impl Strategy<Value = ErrorCode> {
        prop_oneof![
            Just(ErrorCode::InvalidMessage),
            Just(ErrorCode::UnsupportedVersion),
            Just(ErrorCode::IdentityMismatch),
            Just(ErrorCode::SignatureInvalid),
            Just(ErrorCode::RequestExpired),
            Just(ErrorCode::ReplayDetected),
            Just(ErrorCode::ConsentDenied),
            Just(ErrorCode::CapabilityDenied),
            Just(ErrorCode::GrantInvalid),
            Just(ErrorCode::LeaseInvalid),
            Just(ErrorCode::SessionRevoked),
            Just(ErrorCode::TransportError),
            Just(ErrorCode::CaptureFailed),
            Just(ErrorCode::EncoderFailed),
            Just(ErrorCode::InputFailed),
            Just(ErrorCode::PolicyChanged),
            Just(ErrorCode::Internal),
        ]
    }

    fn arb_keyframe_request() -> impl Strategy<Value = KeyframeRequest> {
        (any::<u64>(), arb_reason()).prop_map(|(since_frame, reason)| KeyframeRequest {
            since_frame,
            reason,
        })
    }

    fn arb_control_msg() -> impl Strategy<Value = ControlMsg> {
        prop_oneof![
            any::<u32>().prop_map(|protocol_version| ControlMsg::Hello { protocol_version }),
            (
                any::<String>(),
                any::<u32>(),
                any::<u32>(),
                any::<u32>(),
                any::<u32>(),
                any::<u8>(),
                any::<u8>()
            )
                .prop_map(
                    |(codec, width, height, fps, target_bitrate_bps, color, video_transport)| {
                        ControlMsg::StreamConfig(StreamConfigWire {
                            codec,
                            width,
                            height,
                            fps,
                            target_bitrate_bps,
                            color,
                            video_transport,
                        })
                    }
                ),
            arb_keyframe_request().prop_map(ControlMsg::KeyframeRequest),
            (
                any::<u64>(),
                any::<u32>(),
                any::<u32>(),
                proptest::option::of(arb_keyframe_request())
            )
                .prop_map(
                    |(last_decoded_frame, frames_dropped, decode_latency_us, keyframe_request)| {
                        ControlMsg::Feedback(DecoderFeedback {
                            last_decoded_frame,
                            frames_dropped,
                            decode_latency_us,
                            keyframe_request,
                        })
                    }
                ),
            proptest::collection::vec(any::<u8>(), 0..512).prop_map(|b| ControlMsg::AuthEnvelope {
                payload: Bytes::from(b)
            }),
            arb_code().prop_map(|code| ControlMsg::Bye { code }),
        ]
    }

    proptest! {
        /// Hostile input must never panic the decoder — only Ok or a typed Err.
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = decode(&bytes);
        }

        /// Framed reads over arbitrary bytes never panic and never over-allocate (the guard is inside).
        #[test]
        fn try_read_frame_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut buf = BytesMut::from(bytes.as_slice());
            let _ = try_read_frame(&mut buf);
        }

        /// The bootstrap decoder parses untrusted handshake bytes — it must never panic either.
        #[test]
        fn decode_bootstrap_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = decode_bootstrap(&bytes);
            let mut buf = BytesMut::from(bytes.as_slice());
            let _ = try_read_bootstrap_frame(&mut buf);
        }

        /// Every well-formed message round-trips: decode(encode(m)) re-encodes to the same bytes.
        #[test]
        fn roundtrip_is_identity(msg in arb_control_msg()) {
            let encoded = encode(&msg);
            let decoded = decode(&encoded).expect("well-formed message decodes");
            let re_encoded = encode(&decoded);
            prop_assert_eq!(re_encoded.as_ref(), encoded.as_ref());
        }

        /// A framed well-formed message reads back byte-identical and fully consumes its frame.
        #[test]
        fn frame_roundtrip_is_identity(msg in arb_control_msg()) {
            let mut buf = BytesMut::from(frame(&msg).as_ref());
            let got = try_read_frame(&mut buf).expect("ok").expect("some");
            let got_bytes = encode(&got);
            let msg_bytes = encode(&msg);
            prop_assert_eq!(got_bytes.as_ref(), msg_bytes.as_ref());
            prop_assert!(buf.is_empty());
        }
    }
}
