# 09 — Transport Deep-Dive: Iroh / QUIC

> Grounded as of July 2026. Confidence: facts below are from Iroh 1.x docs/crates.io unless
> flagged `[verify]`. This doc defines how Casual RAS uses Iroh and — more importantly — what Iroh
> does **not** do for us.

## 1. Role of the transport (and the one thing to never forget)

Iroh gives Casual RAS **authenticated, always-encrypted QUIC connections dialed by public key**,
with automatic NAT hole-punching and relay fallback. That is all it gives us.

**Iroh authenticates *identity* (which key you're talking to). It does NOT do *authorization*
(whether that key may view the screen or inject input).** For a remote-desktop product,
authorization *is* the security story, and it is 100% our responsibility — see Invariant 9 in
`CLAUDE.md`. Anyone who steals an endpoint's private key *is* that identity to Iroh. Pairing,
consent, allow-listing, grants, leases, and revocation are all ours to build.

## 2. Iroh 1.0 facts & vocabulary

- **Version:** `iroh` reached **1.0.0 (2026-06-15)**; pin an exact 1.x (e.g. `=1.0.2`) and track the
  CHANGELOG rather than floating on "latest". Dual MIT/Apache-2.0 (license-clean for us). MSRV ~1.91.
- **Vocabulary changed in the 1.0 line** (v0.94 "Endpoint Takeover"): `NodeId → EndpointId`,
  `NodeAddr → EndpointAddr`, `Endpoint::node_id() → Endpoint::id()`. **Most tutorials and
  AI-generated code still say `NodeId` and are stale** — validate every sample against 1.x and use
  the `EndpointId` vocabulary in our code and docs.
- **Core objects:** one `Endpoint` per process (bound to an Ed25519 keypair; the `EndpointId` *is*
  the public key). `.connect(addr, ALPN)` / `.accept()`. On a `Connection`: `open_bi/accept_bi`,
  `open_uni/accept_uni`, and unreliable `send_datagram/read_datagram`.
- **Gotcha:** a stream is only *accepted* on the peer once the initiator has **sent its first
  byte**. A silent stream is invisible — account for this in handshakes.
- **ALPN:** we pick versioned protocol strings. Two families (mirrors `docs/04`): bootstrap
  (`casual-ras/bootstrap/1`) and session (`casual-ras/session/1`). **Reject connections whose ALPN
  or remote `EndpointId` we don't expect, in the `accept()` handler.**

## 3. NAT traversal & relay reality

- **Direct-connection rate:** n0 reports **~90% of connections** and **~95% of data volume** go
  direct. Our latency/quality design must **degrade gracefully on the ~10% relayed** path, not
  assume direct.
- **Hard cases → relay fallback:** symmetric NAT (unpredictable external port) and UDP-blocked
  corporate networks. Iroh can relay over **HTTPS/443** to survive UDP-hostile networks `[verify
  behavior when ALL UDP blocked but 443 open — put in the enterprise test matrix]`.
- **Relay ≠ no-direct:** setup bytes flow over the relay in parallel with hole-punching, then the
  connection **upgrades to direct** and migrates off the relay. "Uses relay" during setup is normal.
- **What relays see:** metadata only — source/dest `EndpointId`s, timing, packet sizes, the
  connection graph. **Payload is E2E QUIC/TLS-encrypted; relays cannot decrypt it.** (Metadata
  leakage is a real threat-model line item — see `docs/06`.)

## 4. Relay strategy — a firm decision, not optional

n0 **explicitly says the public relays are dev/test only** (rate-limited, no SLA). **Casual RAS
self-hosts `iroh-relay` (or uses a managed relay) for production**, in the regions our users live.
Use relay **token auth + allow-list** so only our fleet can use our relays; this also keeps
connection-graph metadata in-house. Ports: HTTP 3340 / QUIC 7824 / HTTPS 443; built-in ACME for
TLS. Develop on `RelayMode::Default`/`Staging`, ship on `Custom(RelayMap)`.

## 5. Channel design for Casual RAS

Split logical channels **by reliability requirement** (this is the crux of low-latency video):

| Channel | Iroh primitive | Why |
|---|---|---|
| Control / lifecycle / lease changes | reliable **bidi stream** | ordered, loss-intolerant |
| Input (pointer/keyboard) | reliable **stream**, per-participant sequence | must not drop or reorder |
| Screen video | **droppable** — see below | a late frame is worthless |
| Pointer/virtual-cursor updates | **datagrams** (latest-wins) | tiny, high-rate, loss-tolerant |
| Clipboard / file transfer | reliable streams | integrity required |

**Video transport — two viable patterns (decide in the media-pipeline spike):**
1. **One QUIC stream per frame / keyframe interval** that we can `reset()`/abandon on loss, so a
   lost packet only stalls that frame — never a single long-lived video stream (within-stream
   head-of-line blocking would stall fresh frames behind stale ones). Iroh docs lean this way
   (MoQ-style; see also `iroh-roq`).
2. **Datagrams + application-level fragmentation + FEC.** `max_datagram_size()` is bounded by path
   MTU — plan a **safe ~1200-byte payload** and do our own fragment/reassemble. Pair with
   **forward error correction (`nanors`, MIT)** rather than retransmit, since ARQ adds an RTT.

Prior art (Moonlight/Sunshine) favors **per-frame Reed-Solomon FEC (block depth = 1 frame, good
below ~3–5% loss)** and **reference-frame-invalidation over IDR-on-loss** to avoid bitrate spikes —
see `docs/10`.

## 6. Congestion, adaptivity, migration

- Underlying Quinn defaults to **CUBIC**; loss-based CC can add bufferbloat/latency spikes.
  **Evaluate BBR** and, regardless, **cap the encoder bitrate to the measured path** — never let the
  encoder outrun the congestion window. Drive adaptive bitrate from `Connection::stats()`/`rtt()`.
- **Connection migration:** Iroh 1.0's multipath model + relay/direct healing is designed to
  **survive Wi-Fi↔cellular/VPN changes** (`paths_stream`, `path_events`). Free good UX for a mobile
  controller — surface connection state (direct vs relayed) in the UI.

## 7. Caveats (the things that will bite)

- Authorization is entirely ours (§1). Treat Iroh as a secure pipe, not a permission system.
- Public relays will throttle real users — self-host before launch (§4).
- ~10% of sessions are relay-only — degrade gracefully.
- `NodeId→EndpointId` rename makes most existing code samples stale.
- Datagrams are ~1200 B and unreliable — we own framing, fragmentation, loss handling.
- Streams accept only after first byte — matters for handshakes.
- Pin exact version; avoid `unstable-*` feature flags (outside 1.0 stability guarantees).
- Validate QUIC/UDP behavior under **Windows Defender Firewall** (first-bind prompt) early.

## 8. Decisions & open validation

- **ADR:** Iroh is transport, not authorization (extends ADR-005). Self-hosted production relays.
  Video over droppable transport (per-frame streams or datagram+FEC — finalize post-spike).
- **Spike must measure** (see `docs/07` revised Phase 1): direct vs relay setup success across the
  enterprise network matrix; datagram vs per-frame-stream latency/loss behavior; CUBIC vs BBR.

## 9. Sources
crates.io/crates/iroh · docs.iroh.computer/concepts/endpoints · docs.rs/iroh (Connection, RelayMode) ·
iroh.computer/blog (1.0-rc, healing-connections, 0.94 endpoint-takeover) · github.com/n0-computer/iroh
issues #2317, #3301 · docs.rs/quinn (TransportConfig / congestion) · RFC 9221 (QUIC datagrams),
RFC 9000, RFC 7250.
