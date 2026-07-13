# Implementation Phases and Delivery Plan

> **Revised for the app-first strategy (ADR-020) and Casual RAS priorities (Security → Latency →
> UX).** We build two working reference apps sharing Rust crates directly, prove the hard parts, then
> extract SDKs. A throwaway **risk-validation spike** precedes real Phase 1 to convert the biggest
> unvalidated bets (latency, Iroh on hostile networks, DXGI recovery) from "assumed" to "measured"
> (`docs/13` D1/D7/C2). SDK work (the original Phase 5) moves *after* the apps prove the boundary.

## Phase S - Risk-validation spike (throwaway, 2–4 weeks)

Scope: validate the two riskiest assumptions before committing architecture.
- Iroh direct + relay connectivity across the enterprise network matrix (symmetric NAT, UDP-blocked,
  relay-only); connection migration.
- Windows DXGI capture → HW H.264 encode → **WebCodecs decode in a Tauri v2 webview** → measure
  glass-to-glass; DXGI `ACCESS_LOST` recovery; WebView2 `Channel` throughput.

Exit criteria:
- Latency targets (`docs/01 §11`) look achievable, or we re-plan (native surface / codec change).
- Direct-vs-relay behavior and the compositor-frame penalty are measured, not assumed.
- Go/no-go on the WebCodecs-vs-native-surface path for the MVP.

## Phase 0 - Foundations

Deliverables:
- Product requirements
- Architecture decisions
- Protocol schema skeleton
- Repository layout
- CI
- Rust workspace
- Threat model
- Test fixture framework

Exit criteria:
- Architecture review completed
- Build runs on Windows and macOS developer machines
- Protocol versioning rules documented

## Phase 1 - Transport and screen prototype

Scope:
- Windows host
- Rust controller
- Iroh endpoint connectivity
- Direct and relay testing
- Single monitor capture
- H.264 encode/decode
- Basic rendering
- RTT, bitrate, encode/decode metrics

Excluded:
- Pairing
- Consent
- Full audit
- SDK wrappers

Exit criteria:
- Stable 30 FPS on standard desktop workloads
- Direct and relay sessions work
- Prototype latency targets measured
- Reconnection behavior documented

## Phase 2 - Identity, pairing, and authorization

Scope:
- Host identity
- Controller identity
- Connection tickets
- One-time pairing code
- Signed access request
- Local consent
- Host-issued session grant
- Grant validation
- Request replay prevention

Exit criteria:
- Unknown controller cannot receive frames
- Replayed request rejected
- Expired ticket and grant rejected
- Host and controller validate each other

## Phase 3 - Remote control and collaboration

Scope:
- Pointer and keyboard
- Control leases
- Lease generation
- Virtual participant cursors
- Pointer-only mode
- Control request and transfer
- Emergency stop
- Key-state cleanup

Exit criteria:
- No two controllers inject real input concurrently by default
- Old lease input rejected after transfer
- Emergency stop prevents input within target time
- Separate cursors remain responsive during video loss

## Phase 4 - Runtime isolation and local audit

Scope:
- Windows service
- Interactive session agent
- Input helper
- Authenticated named pipe IPC
- SQLite state
- Signed audit chain
- Crash recovery
- Service watchdog

Exit criteria:
- Customer application crash does not expose privileged interface
- Input helper refuses malformed or unauthorized messages
- Audit sequence verifies after session
- Service restarts safely without stale leases

## Phase 5 - SDK beta

Scope:
- Stable C ABI
- Node/Electron host SDK
- Node/Electron controller SDK
- React components
- Installer toolkit
- Reference host and controller apps
- API documentation
- Sample integration

Exit criteria:
- External developer completes sample integration
- Upgrade/uninstall tested
- ABI compatibility tests pass
- Signed test binaries available

## Phase 6 - Windows production readiness

Scope:
- Multi-monitor
- Hardware encoder matrix
- Clipboard text
- Controlled file transfer
- Approved action catalogue
- Reconnection
- Signed updater
- Enterprise diagnostics
- Performance optimization

Exit criteria:
- Compatibility matrix passes
- Security review passes
- Long-duration stability test passes
- Release rollback tested
- Installer signed

## Phase 7 - macOS

Scope:
- ScreenCaptureKit
- VideoToolbox
- Accessibility permission handling
- LaunchDaemon and LaunchAgent
- Keychain identity storage
- Swift wrapper
- Notarized packaging

Exit criteria:
- Supported macOS versions pass
- Permission onboarding documented
- Sleep/wake and user-switch scenarios pass
- Notarization succeeds

## Phase 8 - Multi-party and recording

Scope:
- Multiple controller participants
- Annotations
- Host-side optional cursor overlay
- Session recording modes
- Participant timeline
- Recording manifest and hashes

Exit criteria:
- Multi-party cursor performance acceptable
- Recording disclosure and policy enforced
- Control transfer races tested

## Phase 9 - Server migration capability

Scope:
- Issuer trust provider
- Control-plane issuer support
- Central revocation adapter
- Audit upload batches
- Optional regional relay directory

Important:
The host validator and local policy enforcement remain unchanged.

Exit criteria:
- Same session protocol accepts local-host or server-issued grants
- Issuer type and trust roots are configurable
- Offline local mode still works if permitted

## Suggested team

Early:
- 2 Rust systems engineers
- 1 Windows media/input engineer
- 1 TypeScript/React SDK engineer
- 1 QA automation engineer
- Security consultant part-time

Later:
- macOS engineer
- Linux/Wayland engineer
- Developer experience and documentation engineer
- SRE for relay and future control plane

## Critical path

1. Capture and low-latency media
2. Input correctness
3. Host authorization contract
4. Process isolation
5. SDK ergonomics
6. Installer and updater
7. Platform expansion

## Defer deliberately

- Linux before Windows stability
- Arbitrary shell
- Generic file-system access
- Full tenant backend
- Fleet management
- Mobile host control
- Complex billing
