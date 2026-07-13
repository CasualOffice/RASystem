# Test, Performance, and Release Plan

## 1. Test layers

Unit:
- Capability intersection
- Grant validation
- Lease generation
- Replay cache
- State machines
- Ticket parser
- Audit hash chain
- Coordinate mapping
- Keyboard normalization

Property:
- Unknown capabilities always denied
- Reduced grants never expand permission
- Old control generation never becomes valid again
- Audit chain detects modification
- Session expiry bounds lease expiry

Fuzz:
- Protobuf decoders
- CBOR tickets
- Grant parser
- Access request parser
- IPC protocol
- Media frame metadata

Integration:
- Controller to host bootstrap
- Pairing
- Consent
- Session setup
- Direct and relay path
- Control transfer
- Reconnect
- Agent/helper restart
- Emergency stop
- Audit verification

End-to-end:
- Reference controller and host
- Customer application integration sample
- Installer upgrade and uninstall
- Windows user switch
- Lock/unlock
- Sleep/wake
- Network changes
- Relay outage

## 2. Performance tests

Measure:

- Capture latency
- Encode latency
- Network queue delay
- Decode latency
- Render latency
- Glass-to-glass latency
- Input to visible response
- CPU and GPU usage
- Memory
- Bitrate
- Frame drops
- Keyframe recovery

Workloads:

- Static document
- IDE and terminal
- Fast scrolling
- Browser animation
- Video playback
- Multi-monitor
- 1080p
- 1440p
- 4K
- Low bandwidth
- High latency
- Packet loss

## 3. Network matrix

- Same LAN
- Different NATs
- Symmetric NAT
- Corporate firewall
- UDP blocked
- Relay-only
- Wi-Fi to mobile hotspot
- Network migration
- 1%, 3%, and 5% packet loss
- 50, 150, and 300 ms RTT

## 4. Security verification

- Stolen ticket
- Expired ticket
- Modified access request
- Replayed nonce
- Stolen grant from another endpoint
- Old control lease
- Parallel control attempts
- Local IPC from unauthorized process
- Malformed helper request
- Runtime downgrade
- Unsigned update
- Audit record deletion and modification

## 5. Compatibility matrix

Windows:
- Windows 10 22H2
- Windows 11 supported releases
- Intel integrated GPU
- NVIDIA GPU
- AMD GPU
- No hardware encoder fallback
- Multiple DPI settings
- Multiple keyboard layouts
- Standard and elevated target applications

macOS later:
- Current and previous major releases
- Intel if supported
- Apple Silicon
- Multiple displays
- Screen recording permission transitions
- Accessibility permission transitions

## 6. Release channels

- Developer nightly
- Internal alpha
- Design partner alpha
- Closed beta SDK
- Release candidate
- Stable
- Long-term support channel later

## 7. Release artifacts

- Signed host runtime
- Signed controller runtime
- SDK packages
- C headers
- Node package
- React package
- Symbols stored privately
- SBOM
- Checksums
- Release notes
- Migration guide
- Known issues
- Protocol compatibility statement

## 8. Go/no-go criteria for Windows beta

- No known critical security issue
- Successful third-party security assessment
- Emergency stop verified
- Signed installer and update path
- Crash-free long-duration sessions
- Direct and relay reliability target met
- Input correctness across tested layouts
- Audit chain verified
- Documentation and sample app complete
- Customer integration completed by at least one design partner

## 9. Operational readiness

Even without a control plane, relay operations require:

- Regional relay deployment
- TLS and certificate management
- Bandwidth monitoring
- Rate limiting
- Abuse response
- Capacity alerts
- Version compatibility monitoring
- Privacy and metadata documentation
