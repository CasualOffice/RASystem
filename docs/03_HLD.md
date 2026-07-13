# High-Level Design

## 1. Startup

Host startup:

1. Load or generate host identity.
2. Load local policy and trusted controller registry.
3. Open encrypted local audit database.
4. Start local IPC endpoint.
5. Start Iroh endpoint and register bootstrap/session ALPN handlers.
6. Detect active interactive user session.
7. Launch or connect to the session agent.
8. Publish host-ready state to the integration SDK.

Controller startup:

1. Load or generate controller identity.
2. Initialize Iroh endpoint.
3. Initialize decoder and renderer capabilities.
4. Wait for a connection ticket or paired host selection.

## 2. Connection ticket

The host produces a short-lived ticket containing:

- Ticket version
- Ticket ID
- Ticket generation (`active_ticket_generation` at issue time)
- Single-use flag (default true)
- Host ID
- Host public key fingerprint
- Iroh endpoint ID
- Relay hints
- Bootstrap ALPN
- Optional one-time pairing secret
- Expiry
- Host signature

Tickets are **single-use and self-rotating**: at most one is valid at a time, generating a new one
invalidates the previous, and a ticket is consumed on first successful use. See
`docs/16 §1.5` for the mechanism and `docs/04 §11` for the replay-state tables.

Ticket encoding should use canonical CBOR followed by Base64URL. A QR representation may use the same payload.

## 3. Pairing flow

1. Controller imports ticket.
2. Controller opens bootstrap connection.
3. Host validates ticket signature, host binding, expiry, **current generation
   (`ticket_generation == active_ticket_generation`), and that the ticket ID is unconsumed**, plus
   the pairing secret. On success the ticket ID is marked consumed (dead). A stale-generation,
   already-consumed, or expired ticket is rejected — and a failed use after theft surfaces as a
   tamper signal.
4. Controller sends identity and signed pairing request.
5. Host displays controller name and fingerprint.
6. Host user accepts.
7. Host stores the controller public key.
8. Controller stores the host public key.
9. Both sides log pairing result.

## 4. Access flow

1. Controller builds requested capability set.
2. Controller creates nonce and expiry.
3. Controller signs canonical request bytes.
4. Host validates identity, signature, replay, expiry, and host binding.
5. Host applies local policy.
6. Host displays consent.
7. Host user approves full or reduced capabilities.
8. Host issues signed session grant.
9. Controller validates the host grant.
10. Controller opens authorized session ALPN.
11. Host validates grant and endpoint binding.
12. Both sides negotiate protocol version, codec, monitor, and feature set.
13. Session becomes active.

## 5. Media flow

Capture:
- Session agent captures GPU frames.
- Cursor is extracted where the platform supports it.
- Privacy filters are applied before encoding.
- Frame controller selects frame rate and scale.
- Hardware encoder is preferred.
- Encoder outputs frame units with frame ID and timestamps.

Transport:
- Keyframe metadata is reliable.
- Frame chunks use datagrams or independent streams.
- Stale incomplete frames may be abandoned.
- Decoder sends loss and timing feedback.

Rendering:
- Controller decodes into GPU surfaces.
- Virtual participant cursors are rendered after frame composition.
- Local controller cursor remains responsive even when video stalls.
- Optional annotations render in a separate overlay layer.

## 6. Control flow

1. Participant requests control.
2. Host evaluates policy and consent.
3. Host revokes previous lease if present.
4. Host increments control generation.
5. Host issues signed lease.
6. New controller sends events with lease ID, generation, and sequence.
7. Host verifies before forwarding to input helper.
8. Input helper injects normalized OS events.
9. On transfer or termination, host sends key-release state.

## 7. Virtual cursor flow

- Every participant sends normalized display coordinates.
- Host may relay pointer events to other participants.
- Virtual pointer updates are not injected into OS input.
- Viewer renders participant name, state, and cursor.
- Host overlay may display virtual pointers to the local user.
- Pointer events are rate-limited and latest-state wins.

## 8. Consent model

Consent display must show:

- Controller identity
- Device/controller name
- Reason
- Requested permissions
- Whether recording is active
- Whether local user can stop the session
- Session duration or expiry

Consent result:

- Approve requested set
- Approve reduced set
- View only
- Deny
- Trust controller with explicit future policy

## 9. Failure handling

Connection loss:
- Freeze or blank screen according to policy.
- Immediately suspend input.
- Preserve lease state only for a short reconnect window.
- Require generation confirmation after reconnect.

Session agent crash:
- Host service revokes input.
- Restart agent where allowed.
- Never continue remote control without a valid capture/session agent.

Customer app crash:
- Host runtime continues only according to configured policy.
- Attended sessions may terminate if consent UI owner disappears.

Input helper crash:
- Host revokes control lease.
- Force release-key recovery on restart.

Clock skew:
- Use bounded wall-clock tolerance.
- Use host monotonic timers after grant validation.

## 10. State machines

Session:

```text
Created
 -> BootstrapConnected
 -> AccessRequested
 -> AwaitingConsent
 -> GrantIssued
 -> SessionConnecting
 -> Active
 -> Suspended
 -> Terminated

Any state -> Rejected
Any active state -> Revoked
GrantIssued/Active -> Expired
```

Control:

```text
NoController
 -> Requested
 -> LeaseIssued
 -> ActiveController
 -> TransferPending
 -> LeaseRevoked
 -> NoController
```

## 11. Local persistence

SQLite tables:

- host_identity_metadata
- trusted_controllers
- used_request_nonces
- consumed_tickets
- active_ticket_generation
- active_grant_generations
- revoked_grants
- local_policy
- audit_events
- audit_batches
- action_catalogue
- update_state

Private keys should not be stored as plaintext SQLite values. Store key references or encrypted blobs protected by the platform secret store.

## 12. Observability

Metrics:

- Session setup duration
- Direct versus relay connection
- RTT and packet loss
- Encode and decode latency
- Capture FPS and transmitted FPS
- Bitrate and frame drops
- Input acknowledgment
- Lease transfer time
- Audit queue depth
- Agent/helper restart count

Logs must avoid:

- Session token contents
- Private keys
- Clipboard contents
- Typed text
- Raw file content
- Screen pixels
