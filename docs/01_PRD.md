# Product Requirements Document

## 1. Product summary

The product is a white-label, embeddable remote access platform that allows software vendors to add secure screen viewing, remote control, collaboration, and approved support actions directly inside their existing applications.

It is not primarily a standalone remote desktop application. The main deliverables are native host SDKs, controller SDKs, runtime services, installer tooling, and reference applications.

## 2. Problem statement

Software vendors frequently need remote assistance, device control, diagnostics, or guided support inside their products. Building this capability requires solving:

- Cross-platform screen capture
- Low-latency encoding and rendering
- NAT traversal and relay fallback
- Secure controller and host identity
- Consent, authorization, and revocation
- Input correctness across operating systems
- Multi-user collaboration without cursor conflict
- Auditing and enterprise policy enforcement
- Installer, updater, and lifecycle integration

Existing remote desktop products often require users to leave the original application, install another branded product, share codes manually, or accept a user experience that cannot be deeply integrated.

## 3. Product vision

Enable an application developer to add remote support as a native product capability through a small integration surface:

```ts
const host = await RemoteHost.connect();
const ticket = await host.createConnectionTicket();

host.on("access-request", request => {
  showBrandedConsent(request);
});
```

On the controller side:

```ts
const controller = await RemoteController.create();
const host = await controller.connect(ticket);
const session = await host.requestAccess({
  capabilities: ["screen.view", "pointer.virtual"]
});
```

## 4. Target customers

Primary:

- Help desk and ticketing platforms
- Managed service provider software
- Endpoint and device management vendors
- Enterprise desktop applications
- Industrial and field-service applications
- Healthcare and regulated workflow applications
- Internal IT portals
- Education and lab-management systems

Secondary:

- Consumer support applications
- Remote onboarding tools
- Collaborative training platforms
- Embedded device administration software

## 5. Users and personas

### Host user

The person using the machine being viewed or controlled. The host user must understand who is requesting access, why, and which permissions are requested.

### Controller user

A support engineer, technician, administrator, trainer, or collaborator requesting access.

### Integrating developer

A developer embedding the host or controller SDK, customizing consent UX, connecting support tickets, and packaging the runtime.

### Security or compliance administrator

A future enterprise user responsible for policy, retention, allowed capabilities, audit review, and deployment controls.

## 6. Goals

- Deliver low-latency remote viewing and control.
- Support Windows first, followed by macOS and Linux.
- Provide native host and controller SDKs.
- Allow one installer to include the customer's application and the remote runtime.
- Use encrypted direct peer connectivity where possible.
- Fall back to relay transport without exposing plaintext.
- Provide virtual cursors for every participant.
- Prevent multiple users from fighting over the real system cursor.
- Enforce granular capabilities locally on the host.
- Support host-issued short-lived session grants in the MVP.
- Keep the grant contract compatible with future server issuance.
- Record tamper-evident local audit events.
- Keep privileged operations isolated from customer application code.

## 7. Non-goals for the initial release

- Central customer account management
- Multi-tenant cloud control plane
- Customer backend token issuance
- Full mobile host control
- Arbitrary shell execution
- Full unattended enterprise fleet management
- Browser-to-host direct Iroh connectivity
- Linux host support in the first production release
- Audio streaming in the earliest prototype
- Remote printing
- VPN or generic network tunneling
- File system browsing without explicit transfer requests

## 8. Core use cases

### Attended support

A host creates a short-lived connection ticket. The controller imports it, requests capabilities, and the host user approves the session.

### Trusted controller

A previously paired controller reconnects. The host applies local policy and may still require consent depending on configuration.

### View-only support

The controller receives screen frames and has a virtual pointer, but cannot inject input.

### Controlled remote interaction

The host grants a short control lease for mouse and keyboard. Other participants remain in pointer-only mode.

### Collaborative support

Multiple controllers view the same session, each with an independently rendered virtual cursor. Only one controller owns the OS-input lease.

### Approved support action

A controller requests a predefined action such as restart application or collect diagnostics. The host policy engine validates and optionally asks for local approval.

### Emergency revocation

The host user presses a visible stop control or emergency shortcut. All leases and channels are revoked immediately.

## 9. Functional requirements

### Host identity and pairing

- Generate persistent host identity.
- Create expiring connection tickets.
- Support one-time pairing codes.
- Store trusted controller public keys.
- Revoke a trusted controller locally.
- Rotate host identity through an explicit recovery process.

### Access request

- Controller signs every access request.
- Request includes host ID, controller ID, reason, requested capabilities, nonce, and expiry.
- Host validates timestamp, signature, host binding, replay state, and policy.
- Host may approve a reduced capability set.
- Host must clearly display requested and granted permissions.

### Session grant

- Host issues a short-lived signed grant after approval.
- Grant is bound to host, controller, endpoint identities, capability set, generation, nonce, and expiry.
- Controller validates the host signature.
- Host validates the grant again when the authorized session starts.
- Grant never overrides emergency stop or local policy changes.

### Screen streaming

- Capture one display in the first prototype.
- Support multi-monitor selection before production release.
- Use H.264 baseline.
- Adapt frame rate, bitrate, and resolution.
- Separate cursor metadata from the encoded desktop when possible.
- Recover from temporary transport interruptions.

### Input

- Support pointer movement, pointer buttons, wheel, keyboard events, and text input.
- Use per-participant sequence numbers.
- Reject events without a valid control lease.
- Release all pressed keys when control changes or a session ends.
- Prevent stale controller input after lease generation changes.

### Virtual cursors and annotations

- Each participant gets a virtual pointer.
- Pointer-only participants cannot click.
- Virtual pointers are rendered by viewers.
- Host overlay is optional.
- Support transient circles, arrows, highlights, and click indicators later.

### Audit

- Store local append-only audit records.
- Hash-chain records per session.
- Sign records with the host identity.
- Record access requests, approvals, grants, control changes, capability changes, actions, transfers, and termination.
- Queue records while offline for future optional server upload.

### SDK integration

- Provide a stable C ABI around the Rust core.
- Provide Node/Electron wrapper first.
- Provide React controller components.
- Provide installer integration examples.
- Do not expose unauthenticated localhost HTTP APIs.
- Use authenticated named pipes or Unix domain sockets.

## 10. Product requirements by priority

### P0

- Windows host
- Native or Electron controller
- Iroh direct and relay transport
- Connection ticket
- Signed access request
- Local consent
- Host-issued session grant
- H.264 screen stream
- View-only mode
- Pointer and keyboard control
- Virtual participant pointers
- Single active control lease
- Emergency stop
- Local audit journal
- Signed runtime packaging

### P1

- Multi-monitor support
- Clipboard text
- Controlled file transfer
- Predefined action catalogue
- Host service plus session process separation
- Hardware encoder selection
- Reconnection
- React controller SDK
- Node host SDK
- Branded consent hooks
- Signed updater

### P2

- macOS host
- Session recording
- Multiple controllers
- Annotation tools
- Controller handoff
- Browser controller through gateway
- .NET and Swift bindings
- Enterprise packaging

### P3

- Linux host
- Server-issued grants
- Central audit ingestion
- Tenant policy
- Regional relay selection
- Fleet administration
- Mobile controller

## 11. Success metrics

Prototype:

- Median local-network glass-to-glass latency below 120 ms.
- Internet direct-session latency overhead below 80 ms beyond network RTT.
- Input acknowledgment below 100 ms on typical broadband.
- Session setup success above 95% in supported test networks.
- No stale input after control transfer.
- Host emergency stop takes effect within 250 ms locally.

SDK beta:

- Integrating developer can add attended view-only support in under one working day.
- Sample customer installer deploys both app and runtime silently.
- Crash of customer application does not compromise or crash the host service.
- All security-sensitive events appear in the local audit chain.

## 12. Risks

- Screen capture and permission differences across operating systems.
- Wayland limitations and inconsistent Linux portal behavior.
- Hardware encoder driver variation.
- Browser transport limitations.
- Complex keyboard layout and Unicode behavior.
- Local malware and compromised embedding applications.
- Relay bandwidth costs.
- Code signing and notarization requirements.
- Integration complexity becoming the real adoption barrier.

## 13. Product principles

- Local user remains the final owner of the physical machine.
- A controller requests; it does not self-authorize.
- Every privileged behavior is an explicit capability.
- One participant controls the OS pointer at a time by default.
- Other participants collaborate through virtual pointers and annotations.
- Trust decisions are enforceable by the host runtime.
- Transport encryption is necessary but not sufficient.
- SDK ergonomics are a primary product feature.
