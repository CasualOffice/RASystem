# System Architecture

## 1. Architectural overview

The initial system has two trusted endpoints and no central authorization server:

```text
Controller Application
  -> Controller SDK
  -> Controller Core
  -> Iroh Endpoint
  -> encrypted direct or relay transport
  -> Host Iroh Endpoint
  -> Host Service
  -> Session Agent
  -> Capture / Encoder / Input Helper
```

The host is the authorization authority in the MVP.

## 2. Major components

### Controller application

Customer-owned technician or support UI.

Responsibilities:

- Display pairing and connection UI
- Request capabilities
- Render remote frames
- Display participant cursors
- Send virtual pointer and control events
- Show control ownership
- Surface errors and session state

### Controller SDK

Responsibilities:

- Generate and protect controller identity
- Parse connection tickets
- Establish bootstrap connections
- Sign access requests
- Validate host-issued grants
- Establish authorized session connections
- Decode and render media
- Send input only when lease is active
- Expose typed lifecycle events

### Host integration SDK

A thin SDK used by the customer's application.

Responsibilities:

- Connect to the local host runtime
- Create connection tickets
- Receive access-request events
- Present customer-branded consent
- Approve, reduce, or reject requested capabilities
- Expose session state and emergency stop
- Register approved action handlers

The integration SDK is not the privileged capture or input engine.

### Host service

A persistent OS service.

Responsibilities:

- Own host identity
- Own Iroh endpoint
- Validate requests and grants
- Evaluate local policy
- Issue host-signed grants
- Maintain authoritative session state
- Issue and revoke control leases
- Write audit journal
- Coordinate interactive session agents
- Manage updates and recovery

### Session agent

Runs inside the logged-in user's interactive session.

Responsibilities:

- Screen capture
- Consent UI fallback
- Host-visible session indicator
- Optional host-side cursor overlays
- Clipboard integration
- Encoder pipeline
- User-session lifecycle handling

### Input helper

Minimal privileged process.

Responsibilities:

- Receive validated, normalized input commands
- Enforce capability and lease generation
- Inject pointer and keyboard input
- Release stuck keys on termination
- Refuse arbitrary commands

### Iroh transport adapter

Responsibilities:

- Endpoint creation
- ALPN routing
- Address discovery
- NAT traversal
- Relay fallback
- QUIC stream and datagram management
- Transport metrics
- Connection migration and reconnection support

### Local policy engine

Responsibilities:

- Intersect requested capabilities with configured policy
- Require consent based on controller trust and capability
- Apply maximum session duration
- Deny restricted actions
- Enforce recording, file, and clipboard settings
- Re-evaluate policy when local state changes

### Audit journal

Responsibilities:

- Append events before action acknowledgment where required
- Hash-chain events
- Sign events
- Encrypt local storage
- Detect corruption
- Export future server-upload batches

## 3. Process model

```text
Customer Application
   |
   | authenticated IPC
   v
Host Service (system)
   |-- Identity manager
   |-- Policy engine
   |-- Session state
   |-- Iroh endpoint
   |-- Audit journal
   |
   +--> Session Agent (interactive user)
   |      |-- Capture
   |      |-- Encoder
   |      |-- Consent UI
   |      |-- Overlay
   |
   +--> Input Helper (minimal privilege)
   |
   +--> Updater
```

## 4. Trust boundaries

Boundary A: Controller to host bootstrap connection

Only pairing, identity, access request, token submission, ping, and cancellation messages are allowed.

Boundary B: Customer application to host service

Authenticated local IPC with OS ACLs and per-install credentials. The customer app cannot directly invoke raw input APIs.

Boundary C: Host service to input helper

Narrow binary protocol. Only normalized input messages and explicit key-release commands.

Boundary D: Host service to future server

Not part of MVP. Future communication must not weaken local enforcement.

Boundary E: Relay

Relay sees encrypted packets and connection metadata but does not authorize sessions or decrypt content.

## 5. Data plane

Logical channels:

- Control stream: lifecycle, capability changes, lease changes, termination
- Input stream: ordered keyboard and click events
- Pointer datagrams: current pointer position and virtual cursor updates
- Media datagrams or short streams: encoded frame chunks
- Clipboard stream: explicit clipboard messages
- File streams: independent resumable transfers
- Action stream: predefined action request and result
- Telemetry stream: bitrate, RTT, loss, encode/decode timing
- Audit acknowledgments: optional controller-host event receipts

## 6. Platform abstraction

Shared Rust traits:

```rust
trait ScreenCaptureBackend {}
trait VideoEncoderBackend {}
trait InputBackend {}
trait ClipboardBackend {}
trait SecretStore {}
trait LocalIpcServer {}
trait HostOverlay {}
```

Implementations:

Windows:
- Windows.Graphics.Capture
- Media Foundation / NVENC / Quick Sync / AMF
- SendInput
- Named pipes
- DPAPI / TPM

macOS:
- ScreenCaptureKit
- VideoToolbox
- CGEvent
- XPC or Unix sockets
- Keychain

Linux:
- PipeWire + XDG portals
- VA-API / NVENC
- RemoteDesktop portal or XTest
- Unix sockets
- libsecret or protected encrypted storage

## 7. Technology stack

Native:
- Rust
- Tokio
- Iroh
- Prost/Protocol Buffers
- tracing
- OpenTelemetry
- SQLite through SQLx or rusqlite
- C ABI through cbindgen
- Node wrapper through N-API

Controller UI:
- React
- TypeScript
- WebGL or platform video surface
- Electron reference application initially

Backend:
- None required for MVP
- Future control plane may use Java 21, Spring Boot 3, PostgreSQL, Redis, S3-compatible storage, and OpenSearch

## 8. Repository structure

```text
remote-platform/
  crates/
    remote-core/
    remote-protocol/
    remote-identity/
    remote-bootstrap/
    remote-grant/
    remote-policy/
    remote-session/
    remote-control/
    remote-audit/
    remote-media/
    remote-actions/
    remote-transport-iroh/
    remote-ffi/
  host/
    service/
    session-agent/
    input-helper/
    updater/
    platforms/windows/
    platforms/macos/
    platforms/linux/
  controller/
    core/
    electron-reference/
    react/
  sdk/
    c/
    node/
    dotnet/
    swift/
  proto/
  installers/
  examples/
  docs/
```

## 9. Deployment architecture

Prototype:
- Host executable
- Controller executable
- Public Iroh relay or development relay
- No server

Production SDK:
- Signed host service
- Signed session agent
- Signed input helper
- Customer application with host SDK
- Controller application with controller SDK
- Vendor-operated or customer-operated relays

## 10. Architecture decisions

ADR-001: Rust is the shared native core.
ADR-002: The SDK talks to a separate host service.
ADR-003: The host issues grants in MVP.
ADR-004: Grants are issuer-agnostic and endpoint-bound.
ADR-005: Iroh provides transport, not authorization.
ADR-006: One active OS-input controller by default.
ADR-007: Additional cursors are virtual.
ADR-008: No arbitrary shell execution.
ADR-009: Protocol uses Protobuf, not JSON, for high-frequency channels.
ADR-010: Windows is the first host platform.
