# SDK Specification

## 1. SDK layers

- Rust core libraries
- Stable C ABI
- Host integration SDK
- Controller SDK
- React controller components
- Reference applications
- Installer toolkit

## 2. Host SDK API

Core operations:

- connect to local runtime
- query runtime version and capabilities
- enroll or initialize host
- create connection ticket
- list trusted controllers
- revoke controller
- set consent callback
- approve/reject request
- observe active sessions
- stop session
- register predefined actions
- export audit records
- query platform permission state

Example:

```ts
const host = await RemoteHost.connect({
  timeoutMs: 3000
});

const status = await host.getStatus();

const ticket = await host.createConnectionTicket({
  expiresInSeconds: 300,
  pairingMode: "one-time-code"
});

host.on("access-request", async req => {
  const decision = await consentUi(req);
  await req.respond(decision);
});
```

## 3. Controller SDK API

Core operations:

- create/load controller identity
- import ticket
- pair with host
- request access
- wait for approval
- start session
- render into supplied surface
- move virtual pointer
- request control
- send input
- transfer control
- request approved action
- end session

Example:

```ts
const controller = await RemoteController.create({
  displayName: "Support Agent"
});

const remoteHost = await controller.connect(ticket);

const pending = await remoteHost.requestAccess({
  reason: "Ticket SUP-4812",
  capabilities: [
    "screen.view",
    "pointer.virtual",
    "pointer.move",
    "pointer.click",
    "keyboard.key"
  ]
});

const session = await pending.waitForApproval();
await session.attach(renderer);
```

## 4. Event model

Host events:

- runtime-ready
- permission-required
- pairing-request
- access-request
- session-started
- session-suspended
- session-ended
- control-changed
- action-requested
- audit-error
- update-required

Controller events:

- connecting
- paired
- approval-pending
- approved
- rejected
- session-ready
- stream-configured
- control-granted
- control-revoked
- participant-joined
- quality-changed
- disconnected
- session-ended

## 5. ABI rules

- Opaque handles
- Explicit ownership
- No Rust struct layout exposed
- Result codes plus error objects
- Callback thread behavior documented
- ABI version negotiation
- Backward-compatible additions
- Stable string encoding: UTF-8
- Byte buffers use pointer, length, and release callback
- No exceptions across C boundary

## 6. Node SDK

Use N-API rather than direct V8 APIs.

Requirements:

- Promise-based async methods
- Typed event emitter
- AbortSignal support
- Worker-thread-safe callbacks
- Explicit runtime lifecycle
- Electron main-process integration
- Renderer process should not directly access privileged host IPC

## 7. React controller SDK

Components:

- RemoteSessionView
- SessionToolbar
- ParticipantCursorLayer
- AnnotationLayer
- ConnectionQualityIndicator
- ControlRequestDialog
- MonitorSelector
- ConsentSummary
- SessionEndSummary

Hooks:

- useRemoteSession
- useParticipants
- useControlLease
- useConnectionQuality
- useRemoteMonitors

## 8. Installer toolkit

Windows:
- MSI merge module or WiX fragment
- Silent install
- Service registration
- Upgrade and repair
- Uninstall cleanup
- Customer-defined service display name within policy
- Code-signing integration

macOS:
- PKG component
- LaunchDaemon and LaunchAgent setup
- Permission onboarding
- Notarization-compatible packaging

Linux:
- deb/rpm packaging
- systemd units
- user-session agent setup

## 9. Version compatibility

Runtime reports:

- runtime semantic version
- ABI version
- protocol versions
- platform capabilities
- codec capabilities
- optional feature flags

SDK refuses unsafe incompatibility but should support a configurable compatibility window.

## 10. White-labeling

Allowed:

- Application-facing names
- Consent window branding
- Icons and logo
- Support reason text
- Toolbar layout
- Session indicator styling
- Customer legal URLs

Not allowed:

- Hiding active remote control
- Removing emergency stop
- Misrepresenting recording
- Changing security-critical permission meaning
- Suppressing operating-system permission prompts
