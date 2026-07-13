# Protocol and Token Specification

## 1. Protocol families

Bootstrap ALPN:
`com.vendor.remote.bootstrap/1`

Session ALPN:
`com.vendor.remote.session/1`

Future versions:
`com.vendor.remote.bootstrap/2`
`com.vendor.remote.session/2`

## 2. Encoding

- Protocol Buffers for messages
- Unsigned varint frame length prefix on reliable streams
- Maximum message size by message class
- Canonical serialization for signed structures
- CBOR only for portable connection tickets
- Base64URL for copyable ticket strings

## 3. Identity

Host:
- Stable Ed25519 signing key
- Stable host ID derived from public key plus format version
- Iroh endpoint identity bound during ticket/grant issuance

Controller:
- Stable Ed25519 signing key
- Controller ID derived from public key plus format version
- Iroh endpoint identity bound to each request/grant

Application identity and Iroh endpoint identity may use separate keys. The protocol must not assume they are permanently identical.

## 4. AccessRequest

Fields:

- request_id
- protocol_version
- host_id
- controller_id
- controller_display_name
- controller_endpoint_id
- requested_capabilities
- reason
- issued_at
- expires_at
- nonce
- signature

Validation:

- Exact target host
- Known or pairable controller
- Valid signature
- Maximum 5-minute request lifetime
- Nonce not previously accepted
- Supported protocol version
- Bounded display name and reason
- Capability identifiers recognized
- Endpoint identity equals current connection

## 5. SessionGrant

Fields:

- grant_version
- session_id
- request_id
- issuer_id
- issuer_type: LOCAL_HOST or CONTROL_PLANE
- host_id
- controller_id
- host_endpoint_id
- controller_endpoint_id
- granted_capabilities
- policy_version
- session_generation
- session_nonce
- issued_at
- not_before
- expires_at
- signature

Rules:

- Short lifetime
- Endpoint-bound
- Controller-bound
- Host-bound
- Issuer trust configurable
- Capabilities immutable within the signed grant
- Capability reduction allowed through a new generation
- Emergency stop always overrides grant

## 6. Grant issuer abstraction

```rust
trait SessionGrantIssuer {
    async fn issue(
        &self,
        request: ValidatedAccessRequest,
        granted: CapabilitySet
    ) -> Result<SignedSessionGrant>;
}
```

MVP:
- LocalHostGrantIssuer

Future:
- ControlPlaneGrantIssuer

Host validation is performed through a trust provider that supports multiple issuer types.

## 7. ControlLease

Fields:

- lease_id
- session_id
- participant_id
- capabilities
- generation
- issued_at
- expires_at
- host_signature

Rules:

- Very short lifetime, typically 30-120 seconds
- Renewable while session remains valid
- Only one active OS-input lease by default
- Generation increments on transfer
- Old generations rejected immediately
- Lease cannot exceed session grant expiry

## 8. Capability registry

Initial registry:

- screen.view
- screen.select_monitor
- pointer.virtual
- pointer.move
- pointer.click
- pointer.scroll
- keyboard.key
- keyboard.text
- annotation.create
- clipboard.read
- clipboard.write
- file.upload
- file.download
- action.request
- control.request
- control.transfer
- session.invite
- recording.start
- recording.stop

Capabilities must be versioned and centrally documented. Unknown capabilities are denied.

## 9. Bootstrap messages

- ClientHello
- HostHello
- PairingRequest
- PairingDecision
- AccessRequest
- AccessDecision
- SessionGrant
- SessionGrantAck
- CancelRequest
- Ping/Pong
- ProtocolError

No media or input messages are legal under bootstrap ALPN.

## 10. Session messages

Control:
- SessionHello
- SessionReady
- CapabilityUpdate
- ControlRequested
- ControlLease
- ControlRevoked
- SessionSuspended
- SessionTerminate
- ProtocolError

Media:
- StreamConfig
- VideoFrameHeader
- VideoFrameChunk
- KeyframeRequest
- DecoderFeedback

Input:
- PointerMove
- PointerButton
- PointerWheel
- KeyEvent
- TextInput
- ReleaseAllKeys

Collaboration:
- VirtualCursor
- Annotation
- ParticipantJoined
- ParticipantLeft

Actions:
- ActionRequest
- ActionApproval
- ActionStarted
- ActionOutputChunk
- ActionCompleted

## 11. Replay protection

- Request nonce cache
- **Connection-ticket generation** (`active_ticket_generation`; stale generations rejected)
- **Consumed-ticket set** (single-use tickets; a consumed `ticket_id` is rejected on reuse)
- Per-stream message sequence
- Per-participant input sequence
- Session generation
- Control lease generation
- Expiry validation
- Endpoint binding
- Duplicate action request IDs rejected

## 12. Normalized coordinates

Coordinates use floats in the inclusive range 0.0 to 1.0 relative to a display's current logical bounds.

Every pointer message includes:

- display_id
- normalized_x
- normalized_y
- display_layout_version

The host rejects events using a stale display layout version after monitor changes.

## 13. Keyboard representation

Support two forms:

Physical key:
- USB HID usage or platform-neutral physical code
- down/up
- modifiers
- repeat flag

Text input:
- UTF-8 text
- explicit text capability
- not used for shortcuts

This separation avoids forcing Unicode text through physical keyboard-layout emulation.

## 14. Error model

Error categories:

- INVALID_MESSAGE
- UNSUPPORTED_VERSION
- IDENTITY_MISMATCH
- SIGNATURE_INVALID
- REQUEST_EXPIRED
- REPLAY_DETECTED
- CONSENT_DENIED
- CAPABILITY_DENIED
- GRANT_INVALID
- LEASE_INVALID
- SESSION_REVOKED
- TRANSPORT_ERROR
- CAPTURE_FAILED
- ENCODER_FAILED
- INPUT_FAILED
- POLICY_CHANGED
- INTERNAL_ERROR

Errors exposed through SDKs must include stable machine-readable codes and safe human-readable messages.
