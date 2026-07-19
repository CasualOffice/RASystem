# Webapp / Browser Controller — Feasibility & Architecture (refines ADR-057)

> Status: **RESEARCH / DESIGN.** Answers "can the controller SDK run in a browser over QUIC or
> WebRTC, with a relay?" Confirms ADR-057's direction and sharpens it with two load-bearing
> corrections. **No implementation until the §5 backend decision is made.**

## Verdict

**Feasible — but only over WebRTC data channels, and it inherently requires a backend: a signaling
rendezvous + a self-hosted TURN server.** There is no browser transport that can dial the iroh QUIC
core directly, and none that does iroh-style NAT hole-punching to a NAT'd host. The webapp controller
is therefore a genuine **second transport** (`ras-transport-webrtc`) *and* the project's **first
real always-on server component**.

## 1. QUIC-in-browser → iroh: impossible
Browsers expose no raw UDP / bare-QUIC socket. The only QUIC access is **WebTransport (HTTP/3)**, which
runs *on* HTTP/3 (ALPN `h3`, a `CONNECT :protocol=webtransport` handshake) — **not wire-compatible with
iroh's raw QUIC + custom ALPN**. A browser cannot be an iroh peer directly.

## 2. iroh's WASM browser support exists — but relay-only, so it doesn't help
iroh compiles to WASM and a browser node **can reach native iroh peers — but only relay-only over
WebSocket** (the sandbox can't send UDP; direct WebTransport/WebRTC paths are *future*). That routes
**every video frame through an iroh relay** — 100% relayed, worst-case latency, no P2P upside. Reject.
(`n0-computer/web-transport-iroh` is WebTransport-semantics-over-an-existing-iroh-conn, **not**
browser-direct-to-native, and was archived 2026-03.)

## 3. WebRTC — the answer (with caveats)
- **Data channels:** reliable/ordered for **control** (carries the existing `ControlMsg` codec +
  `AuthEnvelope` grant unchanged); unreliable/unordered for **video** (existing 44-byte per-frame
  header + Annex-B; chunk access units ≤16 KiB; the default 128 KiB SCTP window caps high-RTT
  throughput — re-use our frame-drop/keyframe logic). Reuse the **proven WebCodecs → canvas** render.
- **Host crate:** **`str0m`** is the better fit (sans-IO — feed it packets+time, matches our DI-seam
  loop; has **BWE/TWCC**), but its **P2P path is less-tested** (proven use is server SFU). **`webrtc-rs`**
  has stronger desktop→browser precedent but **no sender-side congestion-control estimator** — our
  `LatencyFirstAbr` already carries that, so it's a fit either way. Prototype on str0m, keep webrtc-rs
  as fallback; spike the P2P path on-device early (Phase-S discipline).
- **Everything above the `SessionTransport` seam survives unchanged** — session FSM, control codec,
  grants/leases/per-message gate, ABR, frame header, consent, audit. This validates ADR-057.

## 4. WebTransport — rejected for the MVP
Client↔server only: **no ICE, no NAT traversal, no P2P.** It requires the host to be a **public HTTP/3
server** (valid TLS or ≤14-day pinned cert) — a NAT'd host is unreachable, so it **mandates an always-on
cloud QUIC relay (100% relayed, always)** — strictly worse than WebRTC's partial relay. Interesting only
as a *future* consolidation transport if a cloud gateway already exists.

## 5. The infra tier — the unavoidable "backend" (DECISION NEEDED)
A webapp controller **cannot exist without**:
1. **Signaling rendezvous** — a tiny WebSocket service to exchange SDP + trickled ICE (+ the host's
   identity commitment). For native↔native this can ride the existing iroh channel; for a **cold web
   embedder** (browser user who never met the host) there is no prior channel, so a rendezvous is
   required. Signaling solves *discovery*, never NAT traversal.
2. **Public STUN** — bootstrap (leaks only reflexive addresses, never content).
3. **Self-hosted TURN (coturn)** — **unavoidable** for symmetric-NAT / CGNAT / corporate-firewall cases.
   Consumer TURN-required ≈ **15% (±a lot)**; **enterprise far higher** (20–85%) — and a remote-desktop
   **host is often on a corporate LAN**, so plan for the high end. Run **TURN on TCP 443** (firewall
   punch), ephemeral HMAC creds. **Cost:** TURN relays the full H.264 both ways (~4.5 GB/hr at 10 Mbps)
   → ~$0.23/hr Cloudflare / ~$1.80/hr Twilio / **$20–40/mo self-hosted coturn**.

> **⚠️ Strategic tension (the decision).** CLAUDE.md §6 / strategy **S2/S9 defer *all* server infra to
> Phase 9**. The webapp controller is **inherently a backend feature** — signaling + TURN are not
> optional. So **the webapp-controller track and the "no backend until Phase 9" posture are mutually
> exclusive.** Either (a) the webapp controller moves to **Phase 9**, or (b) a **scoped-exception ADR**
> admits a signaling+TURN component earlier. This is a product/strategy call for the owner, not a
> silent engineering choice.

## 6. Security — the load-bearing correction to ADR-057 (needs its own ADR)
Today iroh gives a property browsers do **not**: the **QUIC/TLS endpoint identity *is* the peer's
Ed25519 key**, so the transport itself proves *which* peer, and the grant sender-constraint binds to
`SessionAuthContext.peer_identity` at the moment the endpoint is proven (Inv 3/9).

**Browser WebRTC/WebTransport authenticate NEITHER identity NOR authority at the transport layer** —
their DTLS/TLS certs are self-signed + ephemeral. ADR-057's "signed grants layered on top" *understates*
this: DTLS-SRTP only proves the peer holds the key for the cert whose fingerprint is in the SDP
`a=fingerprint` line — and **that binding is only as trustworthy as the signaling channel** (an on-path
attacker who rewrites the SDP substitutes their own fingerprint). WebTransport is worse (browser client
is anonymous at the transport layer).

**The fix (what libp2p does):** an **app-layer signed handshake over the already-encrypted channel** —
the peer proves possession of its Ed25519 identity key as the **first message** on the data channel —
**channel-bound to the transport cert hash** (fold the DTLS `getFingerprints()` / WebTransport
`serverCertificateHashes` value into the signed transcript, like libp2p's Noise **prologue** /
`webtransport_certhashes`). Without the channel binding, a TURN/MITM that terminates one encrypted leg
and re-originates another could pass identity proofs through.

**Consequence:** over WebRTC the app-layer handshake carries **both** the identity proof (formerly
iroh's job) **and** the existing grant; `SessionAuthContext.peer_identity` is populated from *this
app-layer-authenticated key*, and `establish()` must not return until it completes. **Inv 9 still holds**
— but "transport" now includes our app-layer handshake, because the browser transport authenticates
neither. **This needs its own ADR; it is the crux of doing the webapp controller securely.**

## 7. Recommended minimal viable version
View-only browser controller → one native macOS host → WebRTC unreliable data channel (video) + reliable
channel (control) → public STUN + one self-hosted coturn (TCP 443) → signaling over a minimal WebSocket
rendezvous → reuse the WebCodecs decoder. App-layer signed+channel-bound identity handshake (§6) is
mandatory from day one. Defer input/control-lease until the video path is proven (stage like the native
track). Host crate: prototype str0m, fall back to webrtc-rs; on-device P2P spike first.

## 8. Open decisions
1. **Backend timing (§5):** webapp controller → Phase 9, or a scoped-exception ADR for signaling+TURN now?
2. **Security handshake (§6):** approve the app-layer signed + cert-hash-channel-bound identity handshake
   as the new peer-authentication mechanism for non-iroh transports (write the ADR).
3. **Host crate:** str0m (fit + BWE, less-tested P2P) vs webrtc-rs (precedent, no BWE) — decide after the spike.
