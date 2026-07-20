# 14 — Architecture Decision Records (ADR Log)

> One entry per significant, hard-to-reverse decision. Format: **Decision · Status · Context ·
> Consequences.** Statuses: `Accepted` (decided), `Provisional` (decided for MVP, revisit),
> `Proposed` (needs sign-off — usually legal/product). Supersedes the inline ADR-001…010 in
> `docs/02`; those are folded in below.

## Foundational (from `docs/02`, carried forward)

- **ADR-001 · Rust shared core · Accepted.** One Rust workspace of core crates underpins host,
  controller, and future SDKs. Cross-platform, performant, versionable.
- **ADR-002 · SDK talks to a separate host runtime · Accepted** (relaxed for MVP — see ADR-020).
- **ADR-003 · Host issues grants in the MVP · Accepted.** No backend required; the host is the
  authorization authority.
- **ADR-004 · Grants are issuer-agnostic and endpoint-bound · Accepted.** A future
  `ControlPlaneGrantIssuer` can replace the issuer without touching the validator (`docs/04 §6`).
- **ADR-005 · Iroh is transport, not authorization · Accepted.** Iroh authenticates identity, never
  permission. Authorization is entirely ours (`docs/09`).
- **ADR-006 · One active OS-input controller by default · Accepted.**
- **ADR-007 · Additional cursors are virtual · Accepted.**
- **ADR-008 · No arbitrary shell execution · Accepted.** Actions are a signed catalogue with strict
  argument schemas.
- **ADR-009 · Protobuf for high-frequency channels, CBOR only for portable tickets · Accepted.**
- **ADR-010 · Windows is the first host platform · Accepted.**

## Strategy & build approach

- **ADR-020 · App-first, extract SDKs later · Accepted.** Build two working reference apps (host +
  controller) sharing Rust crates directly; draw the SDK boundary + C ABI/N-API around proven crates
  afterward. *Rationale:* an SDK surface can't be validated without a real consumer. *Consequence:*
  Phase 1 delivers apps, not an ABI; relaxes ADR-002 for the MVP.
- **ADR-021 · Controller is Tauri v2 (Rust + React/TS) · Accepted.** Reuses the Rust core in-process,
  no ABI. **Pin Tauri ≥ 2.11.1** (Origin-Confusion advisory GHSA-7gmj-67g7-phm9). *Consequence:*
  deny-by-default capabilities, Isolation pattern, strict CSP, remote feed rendered to canvas only
  (`docs/12`).
- **ADR-022 · Controller video path is WebCodecs → canvas for the MVP · Accepted.** Encoded H.264
  pushed to the webview via Tauri `Channel`+`Raw`; `VideoDecoder` decode; GPU-resident render.
  Native-surface fallback is the planned v2 / Linux path (`docs/10 §7`, `docs/12 §5`).
- **ADR-054 · macOS is the development-lead host platform; Windows remains the production target ·
  Accepted (amends ADR-010).** The team develops on Mac + Linux (no Windows hardware), and a Windows
  VM on Apple Silicon gives unrepresentative GPU-capture latency. Because the host is
  platform-abstracted (`ScreenCaptureBackend`/`InputBackend`), leading on **macOS**
  (ScreenCaptureKit + VideoToolbox + CGEvent) is a *scheduling* choice, not an architecture change,
  and yields a working end-to-end demo on hardware we can actually test. **Windows stays a
  first-class supported/production target**, ported when Windows hardware/CI is available. ADR-010
  ("Windows is the first host platform") is superseded for *development order* only; the market
  priority is unchanged. Consequence: macOS host caveats (Screen-Recording & Accessibility TCC,
  secure-input mode, LaunchDaemon-vs-Agent window-server access, notarization) become near-term;
  Secure Enclave holds P-256 not Ed25519 (`docs/06 §6`).
- **ADR-023 · Collapse the host process model for the MVP · Provisional.** One user-space process
  (capture+encode+Iroh+consent+input) for the MVP; split into service + session-agent + input-helper
  as a hardening phase. **Design the IPC + "which desktop am I on" boundary now** so the split is
  mechanical. *Consequence:* the MVP is blind on the secure desktop and to elevated windows — an
  honestly-documented cliff (`docs/11 §1`), not a shipping security posture.
- **ADR-057 · Native Tauri controller first; browser/webapp controller via WebRTC as a later
  integration track · Accepted (extends ADR-021/022, resolves the controller-form fork).** The
  flagship MVP controller stays **native Tauri v2 + iroh P2P + WebCodecs-in-webview** (ADR-021/022,
  S6 iroh unchanged): it keeps the direct, hole-punched, low-latency path (priority #2), imposes no
  browser-transport constraints, and is the fastest route to the M2 reference that proves the
  latency/security story end-to-end. A **browser/webapp controller** — the SDK-embeddable "drop into
  any web product" form — is a **deliberately deferred second track**, carried by **WebRTC**, chosen
  because it is the *only* browser transport that preserves true P2P (ICE/STUN, hole-punching);
  WebTransport/WebSocket are rejected for that path because they require a browser-trusted TLS cert on
  a publicly-reachable endpoint, i.e. a cloud gateway (server infra deferred to Phase 9) and the loss
  of P2P. **Signalling/STUN/TURN:** bootstrap with public STUN (e.g. Google) for reflexive-address
  discovery; add **TURN** (relay) only when direct fails, self-hosted for production privacy
  (parallels ADR-034; public STUN leaks only reflexive-address metadata, never content).
  - *Why this is affordable:* the whole core is transport-agnostic above the DI seams
    (`SessionTransport`/`VideoSinkDyn`/`VideoSourceDyn`) — session state machine, control
    protocol/codec, grants/auth seam, ABR, loss handling, frame-Channel header all survive unchanged;
    the WebRTC track swaps only the transport adapter + render host. **Invariant 9 holds regardless of
    transport** (the host enforces authorization), so adding a less-trusted browser controller does
    not weaken the security foundation.
  - *Consequences accepted, to revisit when the WebRTC track starts:* two transports to maintain
    (iroh native↔native, WebRTC native↔browser) — reassess consolidating on WebRTC iff the browser
    controller becomes primary; WebRTC media rides DTLS-SRTP with our signed grants layered on top
    (host still issues/validates); a browser controller has **no TPM-backed key storage**, so it is
    capped at assurance **Tier 0** (ADR-049 / Invariant 16); relayed (TURN) sessions will not match
    native iroh glass-to-glass latency — an honest, documented trade for embedding reach.

## Media & transport

- **ADR-030 · DXGI Desktop Duplication is primary capture · Accepted.** WGC fallback for per-window /
  hybrid-GPU. Rationale: lowest latency, no capture border, separate cursor metadata, dirty rects.
- **ADR-031 · HW H.264, B-frames off, Main profile, CBR, infinite-GOP + forced-IDR · Accepted.**
  Zero-copy D3D11 texture-in. MF MFT first, direct NVENC/AMF/oneVPL when ultra-low-latency knobs are
  needed.
- **ADR-032 · OpenH264 (`libloading`) software fallback — never x264 · Accepted.** x264/libx264 is
  GPL (source-release trap). H.264/H.265 *patent* posture deferred to counsel (see ADR-051).
- **ADR-033 · Annex-B bitstream; FEC over ARQ; RFI/intra-refresh over IDR-on-loss · Accepted.**
  Robust to loss, no out-of-band `description` to keep in sync, no bitrate spikes. FEC via `nanors`
  (MIT).
- **ADR-034 · Self-hosted production relays · Accepted.** Public n0 relays are dev/test only. Relay
  token-auth + allow-list; keeps connection-graph metadata in-house.
- **ADR-056 · A benign `NormalClosure` code makes a clean `Bye` distinguishable from a crash and from
  a revoke · Accepted.** The control-channel `Bye` carries an `ErrorCode` "reason," but Phase-1 had no
  non-error closure reason — so a graceful stop either sent no `Bye` (indistinguishable from transport
  death: the peer suspends and waits out the whole reconnect window before timing out) or would have
  to borrow an error code. Add **`ErrorCode::NormalClosure`** (wire tag `ERROR_CODE_NORMAL_CLOSURE =
  18`, append-only) as the canonical "intentional teardown, no fault" reason — analogous to WebSocket
  1000 / QUIC application-error 0. This yields three distinct, audit-meaningful teardown paths, each
  mapping to its own terminal edge: **clean `Bye{NormalClosure}` → `PeerClosed → Terminated`**
  (immediately, no suspend), **`Bye{SessionRevoked}` → `Revoke → Revoked`** (host emergency stop only;
  a controller can never revoke — Invariants 1/13), and **a *missing* `Bye` (channel death) →
  `TransportLost → Suspended`** (honor the reconnect window). Non-breaking: the hand-rolled
  `ErrorCode` is `#[non_exhaustive]` and the protobuf mapping is wildcard-free, so the new variant is a
  compile-time forcing function across the codec, never a silent default.
- **ADR-058 · `PlatformSurface` carries a tagged borrowed pointer so a real encoder can reach the
  captured GPU surface · Accepted.** The Phase-1 `ras-media` seam left `PlatformSurface` as pure
  `PhantomData`; the synthetic encoder works only because it *fabricates* Annex-B from frame metadata.
  A real `VideoEncoderBackend::encode<F: CapturedFrame>` is generic over the frame, so through the
  trait it can see only `width/height/captured_at` — never the actual `CVPixelBuffer`/D3D11 texture.
  Fix: `PlatformSurface<'a>` now holds `{ ptr: *const c_void, kind: SurfaceKind }` (a borrowed handle
  tied to the frame's lifetime) plus a `SurfaceKind` tag (`None`, `MacCoreVideoPixelBuffer`, Windows
  variants later). The producing capture backend fills it via a **safe** `from_ptr` constructor
  (storing a pointer is not `unsafe` — only dereferencing is), so **`ras-media` stays
  `unsafe_code = deny`**; the consuming encoder recovers it via `as_ptr(expect: SurfaceKind)` which
  returns the pointer **iff the tag matches** (fail-closed) and then dereferences it *inside the
  platform crate* (`ras-media-macos`, `unsafe_code = allow`). This is sound because `HostSession<C,E>`
  only ever pairs a capture backend with its matching same-platform encoder (`media_pump` feeds
  `C::Frame` straight into `E::encode`), and the pointer never crosses to `ras-core`/transport/
  controller — core never dereferences it. The `kind` tag is a defensive guard, not the primary safety
  argument. Synthetic capture returns `PlatformSurface::none()` and the synthetic encoder ignores it
  (unchanged behaviour). Additive/non-breaking: `SurfaceKind` is `#[non_exhaustive]`.

- **ADR-059 · Transport ALPN `casual-ras/1`; the control channel rides one bidi QUIC stream ·
  Accepted.** The concrete `ras-transport-iroh` first increment needs two wire commitments. **(1)
  ALPN:** every endpoint binds and dials with the single application protocol id `casual-ras/1`.
  ALPN is matched in the QUIC/TLS handshake, so a peer speaking any other protocol (or a stale
  Casual RAS wire version) is refused *before any application byte is exchanged* — fail-closed at
  the TLS layer, the earliest possible point. The trailing `/1` is the transport-wire major version;
  it bumps only on a breaking framing/stream-topology change, never for an additive `ControlMsg`
  variant (those are already versioned inside the protobuf). **(2) Control-stream topology:** the
  reliable, ordered control channel is exactly one bidirectional QUIC stream, **opened by the host
  and accepted by the controller** (amended — see below), so both ends deterministically bind the
  same stream without a negotiation round-trip. It carries the length-prefixed `ControlMsg` framing
  (`u32-BE len | protobuf`, `MAX_CONTROL_FRAME` DoS guard) already fuzzed in `FramedControlChannel`.
  Video rides *separate* per-frame unidirectional streams (ADR-060), each **also opened by the host**,
  so a stalled or reset video frame can never head-of-line-block control or the emergency stop (the
  latency invariant). This is a wire commitment because it fixes who-opens-what and the ALPN string;
  it does **not** touch authorization — QUIC/TLS authenticates *identity* (each side reads the other's
  `EndpointId` as the connection remote), never *authority* (Invariant 9). Grants/leases still ride
  opaque in `ControlMsg::AuthEnvelope` and are validated host-side. Verified by a hermetic loopback
  integration test (two real iroh endpoints, direct-address dial, `Hello`⇄`Bye` round-trip, both
  sides assert the peer's authenticated `EndpointId`).
  - **Amendment (control-stream opener): the *host* opens, not the dialer.** The initial draft had the
    *dialer* (controller) open the control stream. That deadlocks over real QUIC: a freshly-opened
    stream is surfaced to the *acceptor* only once the *opener* first writes, but in the Casual RAS
    handshake the **host speaks first** (`Hello` → `StreamConfig`) while the controller reads first —
    so a controller-opened stream leaves the host's `accept_bi` waiting for a write that never comes,
    and the host waiting to `accept` before it can write. The in-memory loopback masked this (its
    channel is pre-wired and direction-agnostic); a real two-endpoint iroh run surfaced it. Fix: **the
    opener is always the first speaker → the host opens** the control stream (and every video
    uni-stream). The host is thus the uniform *stream* opener; the controller only *dials the
    connection*. No wire-format or ALPN change — purely which side calls `open_bi`/`accept_bi` — so
    `casual-ras/1` stands. Verified by the `ras-core` spine running end-to-end over two real iroh
    endpoints (`iroh_transport::tests::spine_runs_over_real_iroh_transport`).

- **ADR-060 · Video rides one unidirectional QUIC stream per frame (`PerFrameStream`); a 44-byte
  header carries the per-frame `StreamConfig` · Accepted.** `StreamConfig::video_transport` already
  names two droppable options (`PerFrameStream` vs `DatagramFec`, ADR pending on the spike); this
  fixes the wire for the first one, which the iroh transport implements now. **(1) Topology:** the
  host opens a fresh uni stream per encoded frame, writes `[header | Annex-B AU]`, and FINs it; the
  controller accepts streams and reads each to the FIN. Distinct streams are independently ordered in
  QUIC, so a lost or stalled frame **cannot** head-of-line-block a later frame *or* the control
  stream — the load-bearing latency invariant. This is why video is emphatically **not** on the
  reliable control stream. **(2) Header (44 bytes, little-endian, ADR-060):** `magic("RVF1"):u32 |
  version:u8 | flags:u8 | codec:u8 | color:u8 | video_transport:u8 | reserved[3] | width:u32 |
  height:u32 | fps:u32 | target_bitrate_bps:u32 | frame_id:u64 | captured_at_us:u64`, then the AU as
  the stream remainder. The full `StreamConfig` travels **per frame** (not once at session start)
  because the path is droppable/out-of-order: a resolution/bitrate change must arrive atomically with
  the IDR it applies to, or a decoder that missed the setup frame would misparse. Decode is
  **fail-closed**: bad magic, unknown version, an out-of-range enum discriminant, or a short header
  drops *that frame only* (the connection survives); `read_to_end` is bounded by `MAX_VIDEO_FRAME`
  (8 MiB) so a hostile oversized stream is aborted, not buffered. **(3) Droppability & loss
  signalling:** the sink is a bounded channel (depth 4) drained by a writer task — `send_frame` is
  non-blocking and returns `DroppedCongested` when full (a slow path sheds frames at the source
  rather than building latency). The source tracks the next-expected `frame_id` and synthesizes
  exactly one `FrameDropped{first_missing_id}` on a gap, *before* the next frame, so `ras-core`
  coalesces a run of drops into one keyframe request instead of freezing; a stale/reordered frame
  behind the watermark is dropped. **Not authorization** (Invariant 9): the stream carries opaque
  encoded bytes; grants/leases never ride it. Verified by a hermetic loopback test (real per-frame
  uni streams, faithful reconstruction incl. per-frame config, and the synthesized-gap path) plus a
  header round-trip / fail-closed-decode unit test. **Deferred:** true reset-on-stale of an in-flight
  stream (currently drop-at-enqueue), FEC, and the `DatagramFec` alternative — all additive behind
  `video_transport`.

- **ADR-061 · Remote pointer as a `ControlMsg::Pointer` — a visual "look here" cursor, explicitly not
  OS input · Accepted.** The alpha collaboration model is **screen-share + a remote pointer**, not
  remote control: the controller streams its cursor position and the host shows a "look here" overlay,
  so a technician can point ("click *there* to connect") without touching the host's mouse/keyboard.
  Wire: a new `PointerUpdate { x:u16, y:u16, visible:bool }` on the reliable control channel
  (`ControlMsg::Pointer`, proto oneof field 7), controller → host. Coordinates are **normalized
  fixed-point** (`0..=65535` = `0.0..=1.0` of the shared frame) so they survive any resolution/scaling
  on either side; the codec rejects an out-of-range value as `InvalidMessage` (fail-closed). **Why
  this is safe / carries no input-injection risk:** a pointer position is *pixels on a screen*, never
  an OS event — it is not routed to any input helper, injects no click/keypress, and cannot move the
  host's real cursor. It therefore sits **entirely outside Invariants 6 (input helper) and 14
  (secure-desktop/SAS)** — those govern injected input, which this is not. It is also not authority
  (Invariant 9): the pointer is advisory UI. Delivery is **best-effort, latency-first**: the sender
  `try_send`s and drops an update if the control task is briefly behind (a stale pointer is worthless),
  and the host surfaces it as a content-free `LifecycleEvent::RemotePointer`. Verified end-to-end over
  the real spine (loopback e2e: controller `send_pointer` → host `RemotePointer` event) plus a codec
  round-trip. The **on-screen host overlay that draws the pointer** lands with the host GUI; until
  then the `ras-host` CLI logs the arriving position so a two-machine run can confirm the path.

- **ADR-062 · One unified desktop app that plays both roles (agent *and* controller), not two
  binaries · Accepted (amends S2/S4).** The shipped product is a **single app** (`app/`, Tauri v2):
  a home screen offers **Share this screen** (agent) and **Connect to a screen** (viewer), and one
  binary does both. *Motivation:* nobody installs two separate apps for the two ends of a remote
  session — a real product (AnyDesk/TeamViewer-shaped) is one download that can share or connect. The
  earlier split into a standalone `controller/` and `host/` Tauri app was a build-phase convenience,
  not a product decision; it is collapsed here. **This does not weaken any invariant or change the
  wire.** The two roles remain the same `ras-core` orchestrators (`ControllerSession` /
  `HostSession`) over the same `SessionTransport`/iroh seam; they are merely surfaced from one webview
  and one process. The unified app is built with `ras-core` `default-features = false`, so the Share
  role uses the **real `LocalConsent` `GrantValidator`** (Invariant 1) and the `insecure-no-auth`
  `AllowAllValidator` is **not linked** — consequently the old macOS-only **local loopback self-mirror
  is dropped** (it required the no-op validator; it was a dev test, not a product feature). **Platform
  asymmetry is explicit:** Connect is decode-only and ships on macOS/Linux/Windows; Share needs a
  capture backend and is macOS-only until the Linux/Windows backends land, so `start_sharing` returns
  a clear "not available on this platform yet" off macOS while Connect keeps working. This supersedes
  the two separate release artifacts — the release workflow now bundles the one app on all three OSes.
  The headless `ras-host` CLI (workspace crate) stays for no-GUI/testing use. *Consequence:* the host
  process is still the collapsed single-process MVP posture of **S4** (re-separation into
  service/agent/input-helper remains the hardening-phase work); unifying the *UI* of the two ends does
  not change that server-side split.

- **ADR-063 · Cross-platform sharing = PipeWire (Linux) + DXGI (Windows) capture → a shared OpenH264
  software encoder over a CPU-BGRA seam · Accepted.** To make **Share** work beyond macOS, each
  platform gets a `ras-media::ScreenCaptureBackend` and they feed **one** cross-platform encoder:
  - **Encoder — `ras-media-openh264`** (`VideoEncoderBackend`). Software H.264 via the permissive
    **OpenH264** crate (Cisco **BSD-2**; CLAUDE §6's sanctioned fallback — *never x264/GPL*). Consumes
    CPU **BGRA**, converts to I420, emits **Annex-B with in-band SPS/PPS on every IDR** (the wire
    contract the WebCodecs viewer already expects) with forced-IDR-on-demand + infinite GOP. It builds
    on every desktop OS, so it is verifiable **locally and in CI** (unit-tested here: Annex-B keyframe
    with SPS/PPS/IDR, row-padding + odd-dim handling, fail-close on a wrong surface). macOS keeps its
    **hardware VideoToolbox** path; this software path is the Linux/Windows default. *Patent flag
    (carried from ADR-051):* building OpenH264 from source grants **no H.264 patent rights** — flag for
    IP counsel before a formal (non-alpha) release; production may switch to `libloading` a
    Cisco-distributed binary or to OS hardware encoders (VAAPI / Media Foundation / NVENC).
  - **The CPU-frame seam — `SurfaceKind::CpuBgra` + `CpuBgraFrame`** (`ras-media`). A software capture
    hands the encoder a **borrowed** top-down BGRA buffer via the existing tagged-`PlatformSurface`
    mechanism (ADR-058): `ras-media` stays `unsafe`-free (it only stores a pointer); the dereference is
    confined to the encoder crate, fail-closed on a kind mismatch. Additive — the macOS
    `MacCoreVideoPixelBuffer` surface is untouched.
  - **Linux capture — PipeWire + `xdg-desktop-portal` (ScreenCast).** Chosen over X11/`x11rb` because
    the portal path works on **both Wayland and X11** (Wayland is the modern Ubuntu default and blocks
    legacy X11 screen grabs). A bonus: the portal's own screen-picker is an **OS-level consent
    surface** that complements (never replaces) the app's Allow/Deny. DMA-buf zero-copy is a follow-up;
    the alpha maps buffers to CPU BGRA for the software encoder.
  - **Windows capture — DXGI Desktop Duplication.** The standard low-latency desktop capture; the alpha
    copies the duplicated surface to CPU BGRA for the software encoder (a hardware-encode zero-copy path
    via Media Foundation is a follow-up).
  - **No invariant or wire change.** Consent, the always-visible indicator, and the stop control are
    unchanged; only the *source* of pixels differs per OS. **Consequence:** `cargo build --workspace`
    now compiles OpenH264's C/C++ (a C++ toolchain + `nasm` on x86 — added to CI). Runtime correctness
    of the two OS capture backends is an **on-device** step (not reproducible in CI); the shared
    encoder is verified now.

## Security, authorization, fraud

- **ADR-040 · Algorithm-pinned signed grants, sender-constrained · Accepted.** Prefer **Biscuit**
  (attenuation + Datalog + per-block revocation) or PASETO v4.public over hand-rolled JWT;
  endpoint+identity bound (DPoP-style) so a stolen grant is inert; libsodium Ed25519.
- **ADR-041 · Per-message capability enforcement, host-side · Accepted.** Never trust the
  controller's claimed scope. *Directly motivated by RustDesk CVE-2026-57850/-58056, where coarse
  connect-time roles weren't enforced per message.* Fine-grained asymmetric capabilities live in the
  core, **not paywalled**.
- **ADR-042 · Tamper-evident audit is first-class · Accepted.** Hash chain + forward-secure key
  evolution + periodic signed Merkle checkpoint + external witness/RFC 3161 timestamp + TPM monotonic
  counter on seals. "Tamper-evident, not tamper-proof." Never log screen/keystrokes/secrets.
- **ADR-043 · EV code-signing from the first external build · Accepted.** Unsigned = SmartScreen/AV
  (PUA) blocked *and* impersonation-prone. Signing keys in HSM/TPM, off build/production, short-lived
  + revocable (AnyDesk-2024 lesson).
- **ADR-044 · On-device `content → verdict` fraud architecture · Accepted.** All fraud detection runs
  on-host in volatile memory; only content-free verdict enums egress; analyzer inert unless a live
  grant exists. `content` field forbidden at compile time (`docs/15`).
- **ADR-045 · Persona-split enforcement profiles · Accepted.** Consumer-Protect (aggressive,
  fail-closed) vs Attended-Support (warn-only, fail-open) vs Unattended/Fleet (consent layer
  disabled). Warn-and-observe default; new fleets run **shadow/audit-only first**.
- **ADR-046 · Enforcement ladder with local-user-only, controller-blind recovery · Accepted.**
  banner→re-consent→input-suspend→video-mask→auto-pause→terminate; resume authority is local-only.
- **ADR-047 · No UIAccess; lean on the Windows secure desktop · Accepted.** We never build a
  secure-desktop injection bypass; credential/UAC prompts black out remotely by design, session
  continues.
- **ADR-048 · SAS-bound emergency stop · Accepted.** Panic path rides kernel-owned Ctrl+Alt+Del and
  overrides all grants.
- **ADR-055 · macOS input injection lives in the unprivileged per-user agent, never root ·
  Accepted.** On macOS a **root** process can *bypass secure keyboard entry* (typing into password
  fields) — a power we explicitly do not want, since secure input is part of our harm-prevention
  boundary (`docs/15`). It's also mandatory that the injecting/capturing process holds the TCC grants
  *in the GUI session* (a root LaunchDaemon has no WindowServer). So capture + injection live in the
  per-user **LaunchAgent**; any root daemon (identity/audit/update) delegates to it over XPC. Gate
  injection on TCC **PostEvent** (`CGPreflightPostEventAccess`), not Accessibility (`docs/18 §0`).
- **ADR-049 · Tiered enrollment composing with signed grants · Accepted.** Standard/Recommended/
  Hardened/Enterprise; TPM-sealed storage with attestation-gated tier advertising (software fallback
  capped at Tier 0); FIDO2 PRF may fuse to grant issuance; no phishable factor recovers a
  phishing-resistant one (`docs/16`).
- **ADR-050 · Coerced-victim defense is friction + capability containment, not an auth factor ·
  Accepted.** Non-skippable cool-off + directed warnings + default-deny capability classes;
  explicitly harm-reduction. Public claims must distinguish prevent / deter / cannot-stop.
- **ADR-052 · Session recording excluded from the fraud subsystem · Accepted.** If offered, it's a
  separate, separately-consented product with its own DPIA/BAA.
- **ADR-053 · Rotating single-use connection tickets are the always-on default; phone authenticator
  is optional · Accepted.** A ticket is consumed on first use and dead thereafter; generating a new
  one bumps `active_ticket_generation` and invalidates the previous (at most one live), on top of a
  short expiry. Mitigates stolen/leaked/shoulder-surfed/replayed links. *Scope:* protects the
  bootstrap artifact, **not** the endpoint private key (that stays covered by TPM storage +
  revocation + generation bump + emergency stop), and a ticket never grants access without local
  consent. TOTP/FIDO2 are the optional Tier 1+ upgrade, not a prerequisite (`docs/16 §1.5`).
- **ADR-064 · MVP `SessionGrant` = PASETO v4.public, not Biscuit · Accepted** (signed off;
  refines ADR-040). In the MVP the **issuer and validator are the same host**, so Biscuit's headline
  features — offline attenuation, Datalog delegation, third-party blocks — buy nothing yet while
  adding a heavier dependency and a larger audit surface on the security-critical path. Use **PASETO
  v4.public**: a pinned Ed25519 signature (libsodium) over a small typed claims/footer blob —
  trivially auditable and sufficient, because capability **reduction** is done by *re-issuing a
  lower-generation grant* (the host is online), not by client-side attenuation. All of ADR-040's
  requirements are preserved: algorithm-pinned, endpoint+identity-bound, **sender-constrained** (the
  grant binds `controller_endpoint_id` to the iroh `EndpointId` the QUIC/TLS handshake already
  authenticated — so a stolen grant is inert, no separate DPoP proof needed). **Biscuit is adopted
  later**, behind the unchanged `SessionGrantIssuer` seam, when a `ControlPlaneGrantIssuer` must mint
  a broad grant that the host/edge **attenuates offline** (Phase 9) — no wire change to the
  *validator*. If sign-off prefers Biscuit now, only `ras-grant`'s encoder/decoder changes; every
  other Phase-2 contract is format-agnostic. See `docs/design/phase-2-design.md §0`.

- **ADR-065 · Ed25519 primitive = `ed25519-dalek` (already vendored), not a new libsodium binding ·
  Accepted** (refines ADR-040/CLAUDE.md §6 "libsodium Ed25519"). `ed25519-dalek` is **already in the
  dependency graph** — iroh authenticates every endpoint with it (the transport identity *is* an
  Ed25519 key), so it is code we already trust on the security path. Using it for application
  identities + `AccessRequest`/ticket signatures adds **zero new crypto dependency**, avoids a C
  dependency (`libsodium-sys`), and keeps a **single** audited Ed25519 implementation rather than
  two. The primitive is **confined behind `ras-identity`'s `KeyStore` trait** (the trait exposes only
  raw `[u8;32]` public keys and `[u8;64]` signatures — no dalek types leak), so swapping to libsodium
  or a hardware/TPM store later is a `KeyStore` impl change, not an API change. Keys are generated
  from `getrandom` (avoids the dalek↔rand_core version coupling). PASETO v4.public grants (ADR-064)
  reuse this same primitive. *If sign-off prefers a libsodium binding, only the `KeyStore` impl
  changes.* Pinned `=3.0.0-rc.0` to match iroh 1.0.2's tree (the RC iroh already ships).

- **ADR-066 · PASETO v4.public envelope is implemented in-crate over `ed25519-dalek`, not via a
  PASETO library · Accepted** (implements ADR-064; consistent with ADR-065). The MVP grant format is
  fixed (ADR-064 = PASETO v4.public). For the *implementation*, `ras-grant` writes the deterministic
  PASETO **envelope** itself — PAE (pre-authentication encoding), unpadded base64url, and the
  header/footer framing (~120 lines) — and signs via the existing `ras-identity` `KeyStore`/`verify`
  seam. The signature **primitive is not hand-rolled**: `ed25519-dalek` does the signing/verification,
  exactly as ADR-065 mandates.
  - **Why not a PASETO crate.** `rusty_paseto` pulls a **second** `ed25519-dalek` (2.x) *and* `ring`,
    directly violating ADR-065's single-audited-impl posture and enlarging the security-critical tree.
    `pasetors` avoids the dalek skew but introduces `orion` as a **separate** Ed25519 implementation
    used only for the grant path — two audited-but-distinct Ed25519 stacks in one binary. The
    in-crate envelope keeps **one** Ed25519 implementation (dalek), adds **zero** new supply-chain
    surface (nothing new to license-gate, Inv 18), and keeps the whole grant path auditable in a
    single small module.
  - **Why this is safe despite "don't roll your own crypto."** The hand-written part is a
    length-prefixed byte concatenation + base64, **not** a cryptographic primitive. It is pinned to
    the spec and verified **byte-for-byte against the official PASETO v4 test vectors** (`4-S-1`
    no-footer, `4-S-2` footer, `4-S-3` footer+implicit) in `ras-grant`'s tests — sign reproduces each
    official token exactly and verify recovers each payload, so a spec deviation fails the build.
  - **Reversibility.** The format is unchanged, so swapping to a PASETO library later (or to Biscuit
    for the offline-attenuating control-plane issuer, ADR-064) touches only `ras-grant`'s
    encoder/decoder — no wire or validator change. *Decision made under a priorities call
    (Security 1 > Latency 2 > UX 3): an internal grant-token format is not user-facing, so UX is
    unaffected; the single-impl, zero-new-dep security posture wins.*

## Phase 3 — remote control & collaboration (`docs/design/phase-3-design.md`)

- **ADR-067 · Phase-3 OS-input wire = a dedicated `ControlMsg::Input(InputEnvelope)`, distinct from
  the visual `Pointer` · Accepted** (refines docs/04 §12/§13; sibling of ADR-061). OS input rides a
  new `ControlMsg::Input` carrying `{lease_id, generation, seq, action}`, where `action` is a nested
  oneof (`PointerMove`/`PointerButton`/`PointerWheel`/`KeyEvent`/`TextInput`/`ReleaseAllKeys`).
  - **Distinct from `Pointer` (ADR-061).** The visual `Pointer` has *no* lease, is never injected, and
    sits deliberately **outside** Invariants 6/14. Folding OS input into it would blur that boundary.
    Two variants → the host routes `Pointer` to the overlay and `Input` to the enforcement gate with
    zero ambiguity about which path enforces per-message capability (Inv 15).
  - **Coordinates = normalized fixed-point `0..=65535` (== 0.0..=1.0)** of a named `display_id`, plus
    a `layout_version`, **not** the raw float docs/04 §12 sketched. This reuses ADR-061's encoding
    (one coordinate model across visual + OS-input pointers), is wire-compact, and — critically — the
    controller **never sends pixels** (Inv 6); the host maps normalized→pixels *after* authorization
    using its own `CaptureGeometry`. A `layout_version` mismatch after a monitor change drops the
    event (`StaleLayout`).
  - **Keyboard = physical USB-HID usage + explicit modifier bitset**, never a keysym string; Unicode
    `TextInput` is a separate, separately-capped path (`keyboard.text`) for layout-independent entry,
    never for shortcuts. Input payloads (typed text/key values) are redacted in all logs (Inv 8).
  - **Reversibility:** additive oneof fields (8–11) on the existing `ControlMsg`; the hand-rolled enum
    stays the public API. No change to any Phase-1/2 contract.

- **ADR-068 · macOS OS-input backend is a new unprivileged `ras-input-macos` crate over CGEvent; the
  `OsInputSink` trait lives in `unsafe`-free `ras-control` · Accepted** (implements ADR-055; mirrors
  ADR-058's `ras-media`/`ras-media-macos` split). The narrow input surface (`OsInputSink`: normalized
  coords + the closed action set only — Inv 6) is a **pure trait in `ras-control`**; the OS backend is
  a **new FFI crate** where all `unsafe` is confined (CONTRIBUTING §5), empty on non-macOS so Linux CI
  stays green.
  - **CGEvent, not Accessibility-gated.** Injection uses `CGEventCreateMouseEvent`/
    `CGEventCreateKeyboardEvent` + `CGEventPost(kCGHIDEventTap, …)` and `CGEventKeyboardSetUnicodeString`
    for text. The permission is the **PostEvent** TCC bucket (`CGPreflightPostEventAccess` /
    `CGRequestPostEventAccess`), *not* Accessibility (docs/18 §0); `CGEventPost` fails **silently**
    when ungranted, so `input_permitted()` preflights and the host **refuses the lease** rather than
    no-op-injecting.
  - **Deliberately unprivileged** (ADR-055): a per-user LaunchAgent, never root. Consequence: it
    **cannot** inject into a Secure-Input (password/login) field — a *feature* (the fraud-model
    boundary), surfaced honestly, never bypassed. Root could defeat Secure Input; we refuse that power
    (Inv 14).
  - **Tracked key/button state** in the backend makes `release_all` exact (key-state cleanup on
    transfer/disconnect/stop). Bindings are pure-Rust `objc2` + `core-graphics` (permissive); `enigo`
    (MIT) is an acceptable fallback but raw CGEvent is preferred for `release_all` precision. Any new
    dep must clear `cargo-deny` (Inv 18). Linux (`uinput`/libei) + Windows (`SendInput`) backends are
    deferred, additive behind the trait.

- **ADR-069 · The control lease is host-authoritative live state, not a trusted bearer token ·
  Accepted** (operationalizes Inv 5/15, ADR-041). `ControlGranted` is **host-signed** on the wire for
  the *future* process split (S4: a separate privileged input helper will need to verify it), but MVP
  per-message enforcement (`LeaseManager::authorize_input`) checks the **host's own** generation
  counter, active-lease id, monotonic `seq`, and clamped capability set — the controller's
  `generation`/`lease_id` in each `Input` are *claims that must match*, never authority.
  - **Why.** In the collapsed MVP process (S4) issuer = validator = one host process; a signed bearer
    token would buy nothing while adding a verify on the input hot path. Host-authoritative state makes
    the RustDesk CVE-2026-57850 class (client-asserted scope) **structurally impossible**: there is no
    field a controller can set to widen its own scope. Transfer/stop **bump the generation**, so every
    in-flight event of the prior generation is instantly stale — the M4 "old-lease input rejected"
    exit criterion falls out of the generation compare, not out of token expiry.
  - **The gate is O(1)** (integer compares + one `BTreeSet` lookup), on the control task, off the
    per-frame video path (ADR-060) — the latency invariant (priority 2) is untouched.
  - **Reversibility:** when S4's process split lands, the input helper verifies the existing
    `ControlGranted` signature; the wire and the `LeaseManager` logic are unchanged — only *where* the
    check runs moves.

- **ADR-070 · Linux OS-input backend is a new `ras-input-linux` crate; X11 XTest (`x11rb`) first, with
  `uinput` + libei as additive follow-ups · Accepted** (implements ADR-054 cross-platform intent;
  mirrors ADR-068's macOS split; grounded in `docs/19`). A new backend crate fills the same
  `ras-control::OsInputSink` seam behind the same host-authoritative gate (ADR-069); it is empty on
  non-Linux so macOS/Windows CI stays green.
  - **X11 XTest via `x11rb` (MIT/Apache-2.0), and it is `unsafe`-free.** Unlike `ras-input-macos`
    (CGEvent FFI), `x11rb` is a **pure-Rust** X11 protocol client — no C bindings, no `unsafe`. So this
    crate keeps the workspace default `unsafe_code = "deny"` (it does *not* relax it as ADR-068 did).
    Injection is `XTEST` `fake_input` (motion/button/key press+release) against the root window;
    normalized coords map to global pixels **host-side after authorization** (Inv 6), reusing the
    macOS backend's `set_display_bounds` capture-geometry seam.
  - **Deliberately unprivileged (ADR-055).** The X11 path connects to `$DISPLAY` as the logged-in user
    — no root, no `/dev/uinput`. Consequence, surfaced honestly: it works only inside an **X11 (or
    Xwayland) session**; on a pure-Wayland compositor XTest reaches only Xwayland clients, not the
    Wayland desktop. Fail-closed: no reachable X server ⇒ `input_permitted()` is `false` and the host
    **refuses the lease** (never a silent no-op) — same contract as the macOS PostEvent preflight.
  - **Keyboard = USB-HID usage → Linux evdev keycode (`+8` = X keycode).** A closed HID→evdev table
    (Inv 6), never a keysym. The X11 modifier model has no per-event flag, so the backend **reconciles
    a held-modifier set** (fake press/release of the modifier keycodes) to match each event's modifier
    bitset, and tracks pressed keys/buttons/modifiers for an exact `release_all` (Inv 4). `TextInput`
    (the separate `keyboard.text` cap, withheld by `phase3_default_policy`) needs server keymap
    remapping and is **not supported on X11 v1** — it fails closed rather than mis-typing.
  - **Follow-ups, additive behind the trait (`docs/19 §3`):** a **`uinput` privileged-helper** backend
    (X11/Wayland-agnostic, needs a udev `uaccess` rule — the S4 privileged-input-helper boundary) for
    robustness + unattended, and the **`ashpd` `RemoteDesktop` + `reis` libei** consented-Wayland path
    (both MIT; `reis` is pre-1.0 — pin exact). None link GPL/FFmpeg (Inv 18).
  - **Verification honesty:** the crate **cross-compile-*checks* for `x86_64-unknown-linux-gnu` from the
    macOS dev machine** (pure-Rust deps, no cross-linker needed) and its pure-logic tables are
    unit-tested; the live XTest injection is an **on-device step on the developer's Linux machine**
    (an X11/Xwayland session), the Linux analogue of the macOS on-device row (`docs/19 §7`).

- **ADR-071 · Windows OS-input backend is a new `ras-input-windows` crate over `SendInput`
  (`windows-rs`), in-session, no UIAccess · Accepted** (implements ADR-054's Windows-production intent;
  mirrors ADR-068/070; grounded in `docs/19 §4`). A third backend fills the same
  `ras-control::OsInputSink` seam behind the host-authoritative gate (ADR-069); empty on non-Windows so
  macOS/Linux CI stays green.
  - **`SendInput` via `windows-rs` (MIT OR Apache-2.0).** Injection is `SendInput` with `INPUT`/
    `MOUSEINPUT`/`KEYBDINPUT`. `windows-rs` is FFI, so — like `ras-input-macos` (and unlike
    `ras-input-linux`) — this crate **relaxes `unsafe_code` to `allow`** (CONTRIBUTING §5), confined
    behind the safe `OsInputSink` surface. Pointer moves are **absolute** over the virtual desktop
    (`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, `0..=65535` normalized to the virtual-screen
    metrics from `GetSystemMetrics`); the host maps normalized→pixels→absolute **after** authorization
    (Inv 6), reusing the `set_display_bounds` capture-geometry seam.
  - **In-session, deliberately no UIAccess (Inv 14).** The backend runs in the interactive user session
    and does **not** carry a `uiAccess="true"` manifest, so it **cannot** drive elevated windows or the
    secure desktop (UAC/lock/login) — by design, never bypassed. Emergency stop stays the always-visible
    Stop button (+ kernel SAS, which no user-mode injector can synthesize). This is now also enforced by
    the platform: Microsoft's **Jan-2026 credential-UI hardening** blocks remote input into
    credential/secure-desktop surfaces regardless (`docs/19 §1.2/§4`) — the invariant is aligned with
    the OS direction, and the limit is **documented to users** (`docs/11`).
  - **No TCC-style preflight.** Windows has no per-app input-permission prompt, so `input_permitted()`
    returns `true` (session-level injection is available); injection into higher-integrity windows fails
    silently at the OS (UIPI) and is out of scope. Keyboard = closed USB-HID → Windows virtual-key
    table (Inv 6, never a keysym) with held-modifier reconciliation (no per-event modifier flag);
    `TextInput` **is** implementable here via `KEYEVENTF_UNICODE` (UTF-16 units) — cleaner than X11 —
    but still gated by the `keyboard.text` capability (withheld by `phase3_default_policy`). Tracked
    keys/buttons/modifiers give an exact best-effort `release_all` (Inv 4).
  - **Build/verify:** cross-compile-*checks* for `x86_64-pc-windows-msvc` from the macOS dev machine
    (windows-rs is pure-Rust bindings; `check` needs no MSVC linker); the live `SendInput` run is an
    **on-device step requiring Windows hardware the team does not yet have** — so Windows stays
    **CI-compile-gated** (`windows-latest`) until a device/runner exists (`docs/19 §4`). The `uinput`/
    libei Linux follow-ups and a Windows Session-0 service/agent split (S4) remain separate, additive.

- **ADR-072 · Release builds ship UNSIGNED (no OS code-signing / notarization) until a GitHub sponsor
  funds the certificates; update *integrity* signing (free) is still adopted when auto-update lands ·
  Accepted.** Distinguishes two independent signing layers that are often conflated:
  - **OS code-signing / notarization — DEFERRED until funded.** Apple Developer Program ($99/yr) +
    notarization (macOS Gatekeeper) and Windows Authenticode, ideally an **EV cert on an HSM**
    (hundreds/yr) are what stop Gatekeeper/SmartScreen from warning. These cost recurring money the
    project does not have pre-revenue, so **alpha/beta artifacts are shipped unsigned** — users see a
    Gatekeeper/SmartScreen warning and must explicitly allow the app. This is an honest, disclosed
    alpha posture, **gated on obtaining a GitHub Sponsors (or equivalent) funding source**; it is a
    *hardening-phase* step, not an architectural one, and nothing in the code depends on it.
  - **Update-integrity signing — free, adopted from day one of auto-update.** Tauri v2's updater signs
    release artifacts with a **self-generated Ed25519/minisign keypair** (no certificate authority, no
    cost); the app embeds the **pinned public key** and verifies every update before applying. This is
    what actually prevents a malicious-update supply-chain attack (the AnyDesk-2024 / fake-`rustdesk`
    class) — and it is **independent of OS code-signing**. So "unsigned build" means *the OS doesn't
    vouch for the installer*, **not** *updates are unverified*: when auto-update ships (`docs/20 §2.4`,
    Wave 1) it MUST carry Ed25519 update-signing with the private key **off build machines** (in the
    CI secret store now, an HSM once funded).
  - **Consequence & honesty (Inv 17):** the download page / README must state builds are unsigned and
    show the expected fingerprint so users can verify out-of-band; do not imply OS-level trust we don't
    have. `cargo-deny` (Inv 18) + a CycloneDX SBOM per release are the supply-chain hygiene we *can*
    afford now. Revisit and flip to signed + notarized the moment funding lands — a config/CI change,
    no code impact.

- **ADR-073 · Host cursor SHAPE rides an out-of-band `ControlMsg::CursorShape`, rendered client-side;
  never baked into the video · Accepted** (`docs/20 §2.5`; grounded in the cross-device display
  research). The host's OS cursor was only visible *inside* the encoded video, so under any stall/
  compression it lagged and blurred — a **Priority-2 (latency)** regression. Every desktop-grade tool
  (RFB `-239`, SPICE cursor channel, RDP `TS_CACHEDPOINTERATTRIBUTE`, CRD `CursorShapeInfo`, RustDesk
  `CursorData`) sends cursor **shape** out-of-band and composites it client-side at zero latency; only
  game-streamers bake it in (and consequently can't show shape changes).
  - **Wire:** three new `ControlMsg` variants (proto oneof 12–14, append-only): `CursorShape{ id,
    hotspot_x, hotspot_y, width, height, rgba }` (full shape, **cached by `id`**), `CursorCached{ id }`
    (reuse a prior shape without resending RGBA), `CursorHidden` (draw nothing). RGBA is top-down,
    exactly `width * height * 4` bytes.
  - **This is display data, not input — outside Invariant 6.** It flows host→controller only; it never
    reaches the input gate, carries no capability, and cannot inject anything. (Sibling of ADR-061's
    visual `Pointer`, opposite direction.)
  - **Fail-closed decode (the security-relevant part):** the codec bounds dimensions to
    `MAX_CURSOR_DIM = 256` (real cursors are ≤ 32², ≤ 128² on HiDPI), **rejects any RGBA whose length ≠
    `width*height*4`** (no truncation/over-read into a renderer), rejects zero dimensions, and rejects
    a hot-spot outside the image. Covered by by-example negatives + the `decode_never_panics` fuzz +
    the `roundtrip_is_identity` property generator.
  - **Scope:** this ADR landed the **wire + fail-closed codec** first; the **`ras-core` plumbing +
    DI seams now land too** (verifiable off-device). Cursor *position* is deliberately **not** in this
    message — in control mode the controller composites at its own pointer; a position-sync message is a
    later addition for the view-only case.
  - **`ras-core` plumbing (NOW LANDED).** Two object-safe seams mirror the video path: a host-side
    `CursorObserver` (`async fn next() -> Option<CursorFrame>` where `CursorFrame` is `Shape(CursorShape)`
    | `Hidden`) injected by `HostSession::with_cursor_observer`, and a controller-side `CursorSink`
    (`set_shape`/`set_cached`/`hide`) attached by `ControllerSession::attach_cursor_sink`. Cursor pixels
    are **display data** routed through their own sink — deliberately **not** the (content-free) lifecycle
    events, and `CursorShape`'s `Debug` elides the RGBA so a bitmap never lands in a log. A host **cursor
    task** watches the observer, **dedups host-side** (an id seen before → `CursorCached`; else a fresh
    `CursorShape`, recorded **only after** a successful enqueue so a dropped shape is re-sent, never
    referenced before the controller holds it), **re-validates the same bounds the receiver's codec
    enforces** (skip-don't-send a malformed shape), and forwards over the reliable control channel the
    host loop owns via a bounded drop-newest queue (cursor is advisory — it never backpressures control).
    The send-side "seen" set is capped (128 ids, oldest-evicted). Aborted on teardown (advisory data, no
    cleanup obligation — unlike input's key-release). Loopback-tested: a repeated id arrives as
    `CursorCached`, not a re-sent shape; a unit test pins the eviction property (an id pushed past the cap
    is re-sent full, never referenced as cached).
  - **Cache interop contract (shared, not per-side).** A `CursorCached` reference only resolves if the
    controller's `CursorSink` caches with a policy compatible with the host's send-side cache. That cap is
    now the single public `ras_core::CURSOR_CACHE_CAP` (= 128), and the `CursorSink` trait doc pins the
    contract the app must honor: retain **≥ CURSOR_CACHE_CAP** distinct shapes by id, evict **oldest-first
    (FIFO, not LRU)**, and **cache even a dropped-render shape** (there is no upstream re-request — the host
    only re-sends once it too evicts the id). Violating it is a stale/blank cursor (a render glitch, never a
    security issue — cursor pixels are display-only).
  - **Deferred (on-device/GUI):** host cursor **capture** (per-OS: `NSCursor`/`CGImage`,
    `XFixesGetCursorImage`, `GetCursorInfo`+`DrawIconEx`) behind the `CursorObserver` seam, controller
    **render** (a `CursorSink` that draws the cached RGBA on the pointer overlay/WebCodecs canvas), and a
    `cursor_embedded` fallback for backends that can't exclude the HW cursor.

- **ADR-074 · Lock-key state is synced authoritatively via `InputAction::SetLockState`, not by
  forwarding lock-key edges · Accepted** (refines ADR-067; `docs/20 §2.6`; keyboard cross-device
  research). Forwarding a CapsLock/NumLock *keypress* between two machines with independent lock state
  guarantees drift (every VNC/RDP/Sunshine tracker documents stuck-Shift / inverted-Caps). Instead a
  new closed `InputAction::SetLockState { caps_lock, num_lock }` carries the **desired state**; the
  host **slaves** its OS lock keys to it — Chrome Remote Desktop's model.
  - **Closed action, gated on `keyboard.key`** (Inv 6/15): it changes what the keyboard produces, so
    `required_cap` returns `keyboard.key` — a pointer-only lease cannot flip CapsLock (tested). Routed
    through the same per-message `authorize_input` gate as every other action.
  - **Idempotent, host-authoritative reconciliation:** each backend **reads the live OS lock state**
    and taps the lock key **only on a mismatch** — never blindly toggles. Windows: `GetKeyState` low
    bit + `SendInput` VK_CAPITAL/VK_NUMLOCK. Linux/X11: the `QueryPointer` modifier mask (Lock/Mod2) +
    XTEST CapsLock/NumLock keycodes. macOS: `CGEventSourceFlagsState` AlphaShift + a CapsLock keycode
    tap (no NumLock concept) — **best-effort**, as reliable programmatic CapsLock may need IOKit
    (`IOHIDSetModifierLockState`), verified on-device.
  - **Non-breaking rollout:** the `OsInputSink::set_lock_state` trait method has a **default no-op**, so
    test doubles and any backend that can't sync are unaffected; the three real backends override it.
  - **Verify:** wire/codec + gate + dispatch + all three backend overrides are green
    (cross-compile-checked per target, roundtrip + fuzz + a capability-gating unit test). Live lock
    reconciliation is the on-device row; the app forwarding the controller's own
    `getModifierState('CapsLock'/'NumLock')` as `SetLockState` on change **has now landed** (ADR-074
    app wiring — see ADR-075's sibling note).

- **ADR-075 · Cmd↔Ctrl primary-modifier remap is a controller-side, explicit, user-visible policy —
  no new wire surface, no host change · Accepted** (`docs/20 §2.6`; keyboard cross-device research).
  ⌘/Win/Super is **one** HID usage (0x0700E3) with three OS meanings, so a Mac operator's ⌘C reaches a
  Windows/Linux host as **Win+C** and Mac muscle memory fails. Parsec/TeamViewer both ship a "use Mac
  shortcuts" toggle; we adopt the same shape.
  - **Controller-side only.** A visible **default-OFF** checkbox in the Connect bar. When on, the app
    rewrites, for outgoing input, the **left/right Control (0xe0/0xe4) ↔ GUI/⌘ (0xe3/0xe7)** HID
    usages *and* swaps the matching **Ctrl(0x02)↔Cmd(0x08) modifier bits** so the flags the host
    applies stay consistent with the swapped keys. **Scoped to only the primary modifier** — every
    other key passes through untouched.
  - **Why it's not a security change.** The host is **unchanged**: it still receives closed HID usages
    + a modifier bitset and still authorizes every keystroke identically through `authorize_input`
    (Inv 6/15). The remap **cannot expand authority** — a swapped ⌘ is still `keyboard.key`, subject to
    the same lease/capability gate. It is a *presentation* choice about which of two already-permitted
    modifier usages to transmit. Recorded as an ADR only for auditability (it's the "policy above
    passthrough" docs/20 flagged) — **never silent**, deterministic, and reversible from the UI.
  - **Not auto-enabled.** The swap is wrong for Mac→Mac (⌘ must stay ⌘), and the controller does not
    yet learn the host OS, so auto-detect is unsafe today. It stays a manual toggle; auto-enable when
    (controller is macOS ∧ host advertises non-macOS) is a future enhancement once host-OS is surfaced.
  - **Sibling: lock-state app wiring.** The same Connect-side keyboard handler now also implements the
    ADR-074 controller half — it reads `getModifierState('CapsLock'/'NumLock')` off each key event and
    sends `SetLockState` on change, and it **stops forwarding the raw CapsLock/NumLock key edges**
    (forwarding the toggle would race the state sync and cancel it). Lock keys are now *state-only*.
  - **Verify:** app `check`/`clippy` clean; the remap + lock-sync are JS in the Connect webview, so
    the end-to-end behavior (⌘C → Ctrl+C on a real Windows/Linux host; Caps stays in sync) is the
    on-device/GUI row.

- **ADR-076 · Clipboard text sync is an explicit, capability-gated push with a hard no-auto-paste rule
  · Accepted** (`docs/20 §2.3`; clipboard cross-device research). Every incumbent syncs the clipboard,
  and the CVE record is damning: Check Point's Reverse-RDP showed a malicious *host* silently reading
  the controller's clipboard **and pushing content the user never copied**, chained with path traversal
  to RCE; RustDesk leaked pre-connection and cross-session clipboards. We adopt clipboard **text** sync
  only under rules that sever those chains, and land the **security spine** (wire + policy gate +
  fail-closed codec) now; the OS backend + app wiring are the follow-up.
  - **The one load-bearing rule: no auto-paste, ever.** Sync is an **explicit push** — the receiver
    only **populates the OS clipboard**; it **never injects a paste keystroke**. Auto-paste + input
    injection *is* the hijack-to-RCE chain, so keeping paste a manual local act severs it. This rule is
    a receiver-side invariant enforced where the clipboard is set (the OS backend), documented on the
    wire type, and called out as separate from authorization.
  - **Direction is a capability, enforced host-side per message (Inv 15).** Reusing the existing
    catalogue caps: controller→host push requires **`clipboard.write`**, host→controller requires
    **`clipboard.read`** — `ras_policy::clipboard_push_allowed(direction, granted)`, a pure gate that
    never trusts the peer's claim. Both are **recognized but withheld** (absent from every `*_GRANTABLE`
    set) → **default OFF** (tested). No `clipboard.files` — that is file transfer (§3.3), not smuggled
    through the clipboard.
  - **Content is a secret (Inv 8).** `ControlMsg::ClipboardText` carries the text in a `Redacted`
    newtype whose `Debug` prints only a byte count, so the payload **cannot** leak through a derived
    `Debug`/`tracing` field/crash dump — a compile-time-ish guarantee stronger than `TextInput`'s
    by-discipline note (which should adopt `Redacted` too, follow-up). Bounded by `MAX_CLIPBOARD_BYTES`
    (768 KiB, under `MAX_CONTROL_FRAME`); oversize is **refused, never truncated** (truncation silently
    corrupts). Bytes pass through as-is — no CRLF/LF normalization (it would corrupt non-plain text).
  - **Orchestrator wiring — NOW LANDED.** A `ras_control::ClipboardSink` DI seam (deliberately *not*
    part of `OsInputSink` — setting the clipboard is not OS input and is gated by a separate capability)
    with `HostSession::with_clipboard_sink`; the host control loop handles `ControlMsg::ClipboardText`
    by capturing the session's granted caps at authorization, calling `clipboard_push_allowed`
    (controller→host), and — only if allowed and a backend is wired — invoking `set_text` (which sets,
    never pastes). Outcomes are content-free `LifecycleEvent::ClipboardApplied { len }` /
    `ClipboardRejected { code }` (Inv 8 — the byte length, never the text). Fail-closed: no capability
    or no backend ⇒ `CapabilityDenied`, sink untouched. `ControllerSession::send_clipboard_text` is the
    push API the app's "Send clipboard" will call. Two loopback tests: granted → reaches the sink once
    + `ClipboardApplied`; withheld → `ClipboardRejected` + sink never touched (Inv 15).
  - **Host→controller direction (`clipboard.read`) — NOW LANDED (both directions wired).**
    `HostSession::send_clipboard_text` gates the host's own clipboard push on `clipboard.read` against
    the session grant (Inv 15) and, if allowed, forwards it over the generalized outbound-control channel
    (shared with cursor/chat); the controller applies it via a `ClipboardSink` it attaches
    (`attach_clipboard_sink`) — **set, never pasted** — and surfaces the same content-free
    `ClipboardApplied{len}`/`ClipboardRejected{code}`. Audited too. A loopback test proves both: granted →
    the controller's sink receives the host text + a `ClipboardApplied`; withheld → the host gate drops it
    so **nothing crosses the wire** (Inv 15). Clipboard is now symmetric behind its two direction caps.
  - **Emergency stop overrides an in-flight push (Inv 4).** Unlike OS input — whose in-flight events a
    stop neutralizes via the lease **generation bump** — a clipboard push is gated only on the *static*
    granted caps, which a stop doesn't change. So `host_handle_clipboard` re-checks `stop` before setting
    the OS clipboard (mirroring the input path's authorize→dispatch re-check), closing the window between a
    stop and the control loop breaking. A push can never apply on a revoked session.
  - **Deferred to follow-up (GUI/on-device):** the app "Send clipboard" button + a "clipboard shared"
    indicator (Inv 7), echo-suppression ownership tag, and the rule that a **pre-connection** clipboard is
    never auto-synced. (The per-OS `ClipboardSink` impl landed as `ras-clipboard`, ADR-079.)
  - **Verify:** wire round-trip + oversize-refusal + `Debug`-redaction (ras-protocol), the
    per-direction/default-denied gate + recognized-but-withheld (ras-policy), the two host-loop loopback
    tests (ras-core), decoder fuzz — all green. Real OS clipboard set + no-paste is the on-device row.

- **ADR-077 · Audio is host→controller output-audio only, Opus, gated + disclosed; seam-first · Accepted**
  (`docs/20 §2.1`; audio cross-device research). Every incumbent streams the remote machine's sound;
  we add it under the same discipline as screen view and land the **capability + media seam** now,
  deferring the concrete Opus codec + OS capture (exactly how the video traits preceded their backends).
  - **Scope — deliberately narrow.** MVP direction is **host output (system) audio → controller** only:
    **no microphone, no two-way voice, no recording.** Audio is **live-only, never retained at rest**
    (Inv 12 — the fraud subsystem holds zero content; a stream is not a recording). Mic/2-way voice is a
    separate future capability, not a default-on expansion of this one.
  - **Gated + disclosed.** A new `audio.listen` capability — **recognized but withheld → default OFF**
    (in no `*_GRANTABLE` set; a deployment must explicitly widen policy). When active it always shows an
    Inv-7 "AUDIO SHARED" indicator (host/app-enforced), the audio analogue of the always-visible
    viewing/control indicators — white-labeling may not hide it.
  - **Codec = Opus** (royalty-free, Inv 18; low-latency; WebCodecs-native — decodes with an
    `AudioDecoder` configured `"opus"`, mirroring the video WebCodecs path). Defaults 48 kHz / stereo /
    20 ms frames.
  - **Seam, this ADR.** `ras-media::audio` defines the pipeline as traits + canonical types —
    `AudioConfig`, `CapturedAudio` (interleaved i16 PCM), `EncodedAudio` (one Opus packet, monotonic
    `seq`, **no keyframes** — each packet is independently decodable), and `AudioCaptureBackend` /
    `AudioEncoderBackend` / `AudioDecoderBackend`, structurally parallel to the video traits. A
    dependency-free `SyntheticAudioCapture` (tone source) + `SyntheticAudioEncoder` (PCM→bytes
    passthrough) exercise the seam in CI. **No new C dependency** yet (libopus lands with the real
    backend, behind its own license note).
  - **Host pump + gate — LANDED.** `HostSession::with_audio(capture, encoder)` injects the pipeline;
    after authorization the host starts an audio pump thread (mirroring the video media thread) **iff the
    grant carries `audio.listen`** — the Inv-15 host-side audio gate — and the transport carries an audio
    plane. The pump captures→encodes→`send_audio`, re-checks the stop flag between encode and send
    (Inv 4), and is joined on teardown.
  - **Transport plane + controller ingest — NOW LANDED.** The egress `AudioSink` is fetched from the
    **transport** (`SessionTransport::audio_sink()`), symmetric to the video plane — the transport owns
    the wire path, the host owns the *right* to be heard (the gate precedes the fetch). Mirror seams:
    `audio_source()` (controller ingress, `AudioSourceDyn`) and an `AudioOutput` sink the controller
    attaches via `ControllerSession::attach_audio_output` (where received Opus packets go — a WebCodecs
    `AudioDecoder` in the app, a recorder in tests). `SessionTransport::audio_sink`/`audio_source`
    **default to "unsupported"** (a transport without an audio plane simply stays silent), so the
    `IrohSessionTransport` was unchanged at that step. The in-memory loopback overrides both, giving a
    **true end-to-end** host→controller audio path in tests. The real Opus codec is already available
    (`ras-audio-opus`, ADR-080).
  - **iroh audio plane — NOW LANDED, over QUIC datagrams.** The concrete `IrohSessionTransport` now
    implements `audio_sink`/`audio_source`. Audio rides **unreliable QUIC datagrams**, not streams —
    deliberately: real-time output audio wants low latency and no head-of-line blocking; an Opus packet
    is tiny (≈240 B at 96 kbps/20 ms, far under the datagram MTU) and independently decodable, so a lost
    datagram is a brief PLC-covered glitch, never a stall. Datagrams are also a wholly separate QUIC
    mechanism from `accept_uni`, so the audio plane never interferes with the per-frame video streams or
    the control stream. A fixed 36-byte `AudioPacketHeader` (magic `RAU1` + version + per-packet
    `AudioConfig` + `seq` + `captured_at_us`, fail-closed decode) prefixes the Opus bytes in each
    datagram; the receiver skips any foreign/oversized/malformed datagram. **No fragmentation** — one
    Opus packet is one datagram (an oversized packet is a misconfiguration, dropped, not reassembled).
  - **Deferred (OS/on-device):** OS output-audio capture (macOS ScreenCaptureKit audio / CoreAudio tap,
    Windows WASAPI loopback, Linux PipeWire), `AudioConfig` **wire negotiation** (today the config
    travels per-packet in the header rather than being negotiated up front), the "AUDIO SHARED"
    indicator, and JS `AudioDecoder`→`AudioContext` playback.
  - **Verify:** the audio types + `frame_samples` math, the synthetic capture→encode round-trip, the
    `audio.listen` recognized-but-withheld/default-OFF test, **the two loopback spine tests** (end-to-end
    through the transport audio plane: the controller's `AudioOutput` receives packets when
    `audio.listen` is granted, nothing when withheld — Inv 15), **an `AudioPacketHeader` round-trip +
    fail-closed unit test, a real datagram round-trip over two loopback iroh endpoints (packet/seq/config
    intact + seq-gap tolerance), and the full ras-core spine driving the host pump → real iroh datagrams
    → controller output** — all green. Real capture→network→play is the on-device row.

- **ADR-078 · Signed auto-update via Tauri's Ed25519 updater — the free integrity layer, distinct from
  paid OS code-signing · Accepted** (complements ADR-072; `docs/20 §2.4`). An unsigned update channel
  is a supply-chain hole: whoever controls the release host controls what every installed copy runs.
  Tauri's updater verifies each artifact against an **embedded Ed25519 (minisign) public key** before
  applying — a **free** protection we adopt now, orthogonal to the OS-vouches-for-the-installer layer
  (Gatekeeper/SmartScreen) that stays deferred until a sponsor funds certs (ADR-072). *Unsigned by the
  OS ≠ unverified updates* — the two layers are independent, and this ADR closes the integrity one.
  - **Verify-before-apply, always.** The plugin refuses any artifact whose signature doesn't match the
    embedded pubkey; a compromised release file cannot be installed. The private key lives **only** in
    CI secrets (`TAURI_SIGNING_PRIVATE_KEY` + password) and the developer's keystore — **never in the
    repo**.
  - **User-initiated, never silent (Inv 1).** No background auto-replacement. Two Rust commands
    (`check_for_updates`, `install_update`) drive a **two-click** UI: check → then an explicit "Install
    & restart". The machine owner decides when code changes — fitting for a remote-access tool.
  - **Scaffolded now, activated by a one-time key setup.** The plugin, commands, `updater:default`
    capability, `plugins.updater` config (GitHub-releases `latest.json` endpoint), and the CI signing
    env (wired to secrets) are all in place; `bundle.createUpdaterArtifacts` stays **off** and the
    committed `pubkey` is an **empty placeholder**, so keyless CI stays green and no throwaway key ships.
    Activation = generate a key, paste the pubkey, add two secrets, flip the flag (runbook:
    `docs/design/auto-update-runbook.md`). Same posture as ADR-072's deferred OS signing.
  - **Update integrity ≠ transport auth.** This signs *the software*; the session's identity/authority
    model (grants, consent, per-message gate) is unchanged and unrelated.
  - **Verify:** app `cargo check`/`clippy` clean (config parses at `generate_context!`, plugin +
    commands compile); the signature-verified download + install + relaunch is the on-device row (needs
    a provisioned key + a published `latest.json`).

- **ADR-079 · The concrete host clipboard backend is `arboard`, wired but default-inert · Accepted**
  (implements ADR-076's deferred backend). The clipboard **write** seam (`ras_control::ClipboardSink`)
  gets a real cross-platform implementation so a `clipboard.write`-granted push can actually reach the
  OS clipboard.
  - **`arboard`** (`ras-clipboard::ArboardClipboardSink`): NSPasteboard (macOS), Win32 clipboard
    (Windows), X11 selections (Linux). It **only sets** the clipboard — the no-auto-paste rule (ADR-076)
    now holds at the *mechanism*, not just policy. The text is passed straight to the OS, **never
    logged** (Inv 8); errors carry only static messages. `Clipboard::new()` **fails closed** (no display
    ⇒ no sink ⇒ host refuses pushes). The handle lives for the process (Linux/X11 must keep serving the
    selection).
  - **Lean, permissive deps.** `default-features = false` drops arboard's `image-data` (text-only) and
    `wayland-data-control` — Linux stays **X11/Xwayland-only** via pure-Rust `x11rb`, matching
    `ras-input-linux` (no libwayland/system deps). License: arboard is MIT/Apache; its **Windows-only**
    `clipboard-win` + `error-code` are **BSL-1.0** (Boost — mainstream permissive, non-copyleft, GPL-
    compatible; Inv 18 holds), added as **scoped `cargo-deny` exceptions** (never linked off Windows) so
    the global allow-list still matches Inv 18's enumeration.
  - **Wired but default-inert (correct posture).** The app's Share role calls `with_clipboard_sink`, but
    `clipboard.write` stays **withheld by default** (ADR-076), so the sink is never reached until a
    deployment explicitly grants it. Enabling clipboard end-to-end — offering `clipboard.write` in
    policy + a consent, the controller-side "Send clipboard" (reads *its* OS clipboard), a "clipboard
    shared" indicator (Inv 7), and the host→controller `clipboard.read` direction — is the remaining
    app/GUI step.
  - **Verify:** crate builds + `clippy` clean on host **and** cross-compile-checked for
    `x86_64-unknown-linux-gnu` + `x86_64-pc-windows-msvc`; `cargo-deny` clean; a fail-closed
    construction test (Ok with a clipboard, typed Err headless — never panics, never calls `set_text`
    to avoid clobbering CI). App `check`/`clippy` clean. Real OS set + no-paste is the on-device row.

- **ADR-080 · The concrete Opus audio codec is `audiopus` (vendored libopus), unit-tested by
  encode→decode roundtrip · Accepted** (implements ADR-077's deferred codec). The audio seam gets a
  real encoder/decoder so the pipeline can produce/consume Opus.
  - **`ras-audio-opus`**: `OpusEncoder`/`OpusDecoder` implement `ras_media::AudioEncoderBackend` /
    `AudioDecoderBackend` over **`audiopus`** (a safe wrapper; `audiopus_sys` builds **libopus from
    vendored BSD-3 source via cmake** — no system libopus needed). Opus is royalty-free (Inv 18). The
    encoder **buffers sub-frame input** and emits one packet per whole Opus frame (honoring the
    `Ok(None)`-until-ready contract); `set_bitrate` retargets live. **Not** the RustDesk `magnum-opus`
    fork. `unsafe` stays in the external FFI crates — this crate is `unsafe`-free.
  - **Genuinely unit-tested headless** (unlike the clipboard/OS backends): a real **encode→decode
    roundtrip** proves the DSP — a 440 Hz tone survives the codec (decoded frame size matches, peak
    amplitude preserved), plus gap-free `seq`, sub-frame buffering, and unconfigured-errors tests.
  - **cmake-4 fix, workspace-wide.** CMake 4.0 (2025) dropped the pre-3.5 policy that the vendored
    libopus still declares; `.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` (the standard
    fix), inherited by dev + CI automatically. Only affects cmake build scripts (just `audiopus_sys`).
    cmake is preinstalled on all CI runners; libopus builds per-OS natively there, the same model as the
    C-based `ras-media-openh264` (nasm) already in the workspace.
  - **License:** `audiopus`/`audiopus_sys` are **ISC** (already allowed); vendored libopus is **BSD-3**
    (Xiph) — no new `cargo-deny` allowance needed.
  - **Maintenance caveat (RUSTSEC-2026-0150, added later).** `audiopus_sys` is flagged **unmaintained**
    (informational, not a CVE); 0.2.2 is the latest, so no upgrade exists. Its only concrete concern —
    a pre-3.5 CMake policy CMake 4.0 rejects — is **already** mitigated by our
    `CMAKE_POLICY_VERSION_MINIMUM=3.5` above, so libopus builds cleanly. Ignored as a single named
    exception in `deny.toml` (like `paste` RUSTSEC-2024-0436); **follow-up:** evaluate a maintained Opus
    binding once one that vendors cleanly exists. (Real vulnerabilities + yanked crates still fail CI.)
  - **Not yet wired.** Like the audio seam itself, this codec is unconnected until the audio pump lands
    (transport sub-stream + OS capture + `ras-core` pump + `audio.listen` gate + JS playback, ADR-077).
  - **Verify:** builds + roundtrip/bitrate/buffering/error tests green **natively on macOS** (and in the
    full workspace gate); Linux/Windows are the CI-native build gate (a C library can't be cross-built
    from macOS — same honesty as openh264). Real capture→encode→network→decode→play is the on-device row.

- **ADR-081 · Multi-monitor: a signed virtual-desktop `MonitorDef` model + HiDPI descriptor, enumerated
  host-locally and selected by the host owner (not the controller) · Accepted** (`docs/20 §2.2`; display
  cross-device research). The coordinate spine already supports multi-display (normalized-per-display,
  `layout_version`, `CaptureGeometry`); this lands the **enumeration + HiDPI metadata** the research
  ranked as the two missing pieces, off-device.
  - **`MonitorDef` (ras-media)** describes one display in the host's **virtual desktop**: `id`, a
    logical-unit rect with **possibly-negative** `left/top` (a display left of / above the primary — the
    universal convention: RDP `TS_MONITOR_DEF`, RustDesk `DisplayInfo`, Sunshine `offset_x/y`),
    `logical_width/height`, backing `pixel_width/height`, `scale_percent`, and `primary`. Scale is an
    **integer percent** (100/150/200), never a float — the model carries **no float to drift** (the
    research's warning; RustDesk's absolute-host-pixel + float-scale model is its #1 DPI-misalignment
    bug class). The host still resolves normalized→pixels against its *own live* geometry, so a click
    **lands** regardless of DPI; the controller uses the scale only to render **crisply** and to fold
    its own `devicePixelRatio` when normalizing input.
  - **Enumeration is a host-local query, not wire state (Inv 1).** `ScreenCaptureBackend::enumerate_displays()`
    lets the app build a picker so the **host owner** chooses what to share — the controller does **not**
    select or switch displays in this slice (a controller-initiated switch would be a later
    control-message + host-consent addition). Selection is simply the `CaptureOptions.monitor` the app
    passes to `start`. Default empty ("unknown" → share the default display).
  - **HiDPI to the controller** via `captured_display() -> Option<MonitorDef>` → a new additive
    `LifecycleEvent::CaptureDisplay` (logical + pixel dims + scale + primary), emitted at capture start
    **alongside** `CaptureGeometry` (which *places* the host overlay). Metadata only — dimensions/scale,
    never pixels (Inv 8 untouched). A new lifecycle **variant** (the enum is `#[non_exhaustive]`), so it
    is additive — existing consumers are unaffected.
  - **Explicitly NOT doing** host-resolution matching / virtual displays (mutates the owner's display
    config — conflicts with Inv 1; permissive building blocks are uneven per the research). Normalize
    against live geometry instead.
  - **Deferred (on-device):** the real backends' `enumerate_displays`/`captured_display` (macOS
    `SCShareableContent` + `NSScreen.backingScaleFactor`; scap/Windows/Linux equivalents) behind the new
    default methods, the app's display **picker** UI, controller-side crisp-render use of the scale, and
    letterbox-subtraction + mid-session switching (`layout_version` bump).
  - **Verify:** the `MonitorDef` model + `scale_factor` helper, the synthetic backend's two-display
    virtual desktop (primary-first, negative-origin HiDPI secondary, 2× pixel/logical) + active-display
    descriptor, and a `ras-core` loopback test asserting the host emits `CaptureDisplay` with the shared
    display's dims — all green in the full gate. Real per-OS enumeration is the on-device row.

- **ADR-082 · In-session chat is base session communication (no capability), bidirectional, bounded, and
  `Redacted` end-to-end · Accepted** (`docs/20 §3.1`). A simple text channel between the two consented
  peers — useful for the support use-case ("click the button, top-right").
  - **Not a privileged behavior → no capability.** Chat touches no OS/input/screen surface; a live
    session already required local consent (Inv 1). Gating it behind a capability would be
    security-theater (unlike clipboard/input/audio, which *do* reach the OS). It is base session comms,
    like keyframe requests or feedback. (Inv 2 governs *privileged* behaviors; chat is not one.)
  - **Inv 8 is the load-bearing rule.** Chat text is content (users paste anything — a PIN, a link).
    The payload is a [`Redacted`] newtype on the wire (`ControlMsg::ChatMessage`), through the codec, and
    even in the `LifecycleEvent::ChatMessage` that surfaces it — so its `Debug` prints only a byte count
    and it **cannot** leak through any log/trace/crash-dump line. It is **never logged or
    audited-as-content**; only `.reveal()`d at the point of display. This is the sole content-bearing
    lifecycle event, and only because `Redacted` makes it log-safe.
  - **Bounded + fail-closed.** `MAX_CHAT_BYTES = 4 KiB` (chat is short prose, far under
    `MAX_CONTROL_FRAME`); an oversized message is **refused, never truncated** (codec) and dropped
    before send (both `send_chat` APIs). Fail-closed codec + fuzz.
  - **Bidirectional, direction-implicit.** Both `HostSession::send_chat` and
    `ControllerSession::send_chat`; a received `ChatMessage` is always *from the remote peer*, surfaced
    on each side's own lifecycle stream — no direction field needed. The host send reuses a generalized
    **outbound-control channel** (the cursor task now shares it, ADR-073), so one path carries all
    proactive host→controller messages.
  - **Verify:** wire roundtrip / oversize-refused / redacted-in-`Debug` / fuzz (ras-protocol) + a
    `ras-core` loopback test proving chat flows **both** directions (controller→host and host→controller)
    with content intact, each side receiving only the other peer's text. All green. UI is the on-device
    row.

- **ADR-083 · Harden `keyboard.text` (Unicode/IME): `Redacted` end-to-end + control-character rejection;
  stays deny-by-default with its own lease bit · Accepted** (`docs/20 §2.6`; keyboard cross-device
  research). The positional HID path (`KeyEvent`, ADR-067) can't compose CJK/emoji/accents (IME lives
  *above* the keycode layer — no HID usage "is" 你), so the withheld `keyboard.text` capability +
  `InputAction::TextInput` already existed; this makes it **safe** to grant without changing its
  deny-by-default posture. This is the CRD-`TextEvent` / RustDesk-Translate analogue done with the
  invariants enforced at the type layer.
  - **Its own lease bit (Inv 15).** `TextInput` requires the **separate** `keyboard.text` capability —
    a broader "type-anything-into-focus" authority than physical keys (effectively scripting if focus is
    a terminal). A lease that grants `keyboard.key` but **not** `keyboard.text` **denies** a `TextInput`
    at the per-message gate (tested). It stays **out of the default grantable policy** — a deployment
    must explicitly widen policy to offer it.
  - **`Redacted` end-to-end (Inv 8).** `InputAction::TextInput.utf8` is now a [`Redacted`] (was a plain
    `String`) — the field is literal plaintext (passwords/PII typed into focus). Its `Debug` prints only
    a byte count, so it can't leak through a log/trace/crash line at **any** layer (wire type, envelope,
    gate); `.reveal()` is called **only** at the OS-injection boundary (`OsInputSink::text`), never to
    log. Audit records a content-free "text injected" event, never the text.
  - **Control-character rejection (anti-smuggling).** The decoder refuses any payload containing a
    control character (`char::is_control` — C0/C1 + DEL), so `keyboard.text` can't carry a terminal
    escape (`ESC[…`), NUL, or newline/tab navigation. Composed printable Unicode — CJK, emoji (incl. ZWJ
    sequences + variation selectors, which are format chars, not control chars), accents — all passes;
    navigation/shortcuts remain the positional `keyboard.key` path. Length stays bounded by
    `MAX_TEXT_INPUT = 256` (refused, never truncated).
  - **Deferred:** a **rate** bound (chars/sec) as defense-in-depth — it needs a clock in the otherwise
    pure per-message gate, so it is a separate hardening; the per-message length cap bounds burst size
    today. App wiring (a controller that emits composed IME text as `TextInput`) is the on-device row —
    and the mobile controller (§3.6), where soft-keyboard Unicode is unavoidable, depends on this.
  - **Verify:** codec roundtrip (CJK + emoji) / control-char-rejected (ESC/NUL/newline/tab/DEL) /
    printable-passes / redacted-in-`Debug` / oversize-refused / fuzz (ras-protocol), and the per-message
    **own-lease-bit** gate test (`keyboard.key` lease denies `TextInput`; `keyboard.text` lease allows
    it) in `ras-control`. All green.

- **ADR-084 · Persistent paired-controller registry: identity allow-list that skips re-pairing but
  never confers authority · Accepted** (`docs/20 §3.5`, `docs/16 §11`; the foundation for unattended
  access §3.4). After a first **attended, consented** session, the host may persist the controller's
  Ed25519 identity so future sessions from that key skip the pairing prompt — the opposite of a standing
  password.
  - **Skip-pairing ≠ standing authority (the load-bearing rule).** A known controller **still mints a
    fresh, short-lived, endpoint-bound grant** (Inv 3), **still** enforces capabilities per message
    (Inv 15), and **still** honors emergency stop (Inv 4). The registry authenticates *identity* only;
    authority stays the per-session grant's (Inv 9). This is encoded so it can't drift: the pairing
    decision is a bare 2-variant enum (`SkipPairingPrompt` / `RequirePairingPrompt`) carrying **no
    capabilities** — a registry hit governs the *human prompt*, never authorization. Tested.
  - **Local user owns the list (Inv 1); de-listing is a kill-switch.** `PairingRegistry`
    (`pair`/`is_paired`/`get`/`list`/`touch`/`revoke`) with an in-memory MVP impl; a `PairedController`
    record carries the id + a user label + `first_paired_at`/`last_seen_at`. Re-pairing preserves the
    original pairing age; revocation removes the skip-pairing standing (future sessions require a fresh
    attended accept). **Pure** — the crate reads no clock; the caller passes timestamps in (deterministic
    tests).
  - **Structural key-change detection + a human-comparable code.** The registry keys on `ControllerId`
    (= the raw pubkey), so a changed key is structurally a *different* entry — a silently-rotated key is
    never trusted under the old identity. `pairing_code(id)` renders the pubkey as grouped
    **Crockford-base32** (omits `I L O U`) — the eyeball/verbal check shown **alongside** the
    host-displayed QR (host shows, controller scans — the direction that dodges the Signal-QR-hijack
    coached-victim vector, §3.5). No hash step: a `ControllerId` is already a 256-bit uniform key, so
    rendering it directly *is* the Syncthing-style device id.
  - **Scope:** this lands the **pure registry model + decision + code** (verifiable off-device). Deferred:
    a **SQLite-backed** durable impl (restart-survival; adds a store dep — kept out of this pure spine),
    wiring the decision into the app's connect/consent flow + the host-displayed QR, and unattended
    access (§3.4) on top. Replaces the earlier bare `TrustedControllers` set (nothing consumed it yet).
  - **Verify:** pair/lookup/revoke-kill-switch, re-pair-preserves-`first_paired_at` + `touch`,
    decision-governs-prompt-only (+ revocation flips it back), and `pairing_code`
    deterministic/grouped/key-specific/Crockford-only — all green.

- **ADR-085 · Unattended access: a Tier-gated standing pre-authorization that skips the live prompt but
  never the fresh grant · Accepted** (`docs/20 §3.4`, `docs/16`; builds on the pairing registry ADR-084).
  Incumbents ship a **standing password** (RustDesk) or a shared account (TeamViewer — whose 2016
  credential-stuffing wave shows the wrong shape). We build the **opposite**: the host pre-authorizes the
  *issuer*, not a standing session.
  - **Unattended ≠ standing session.** A `Proceed` decision only skips the **live consent click** — every
    connect **still** mints a fresh, short-lived, **endpoint-bound** `SessionGrant` (Inv 3), enforced per
    message (Inv 15) and overridable by emergency stop (Inv 4). So "unattended" *raises* the bar on
    expiry / scope / revocation rather than lowering it. Encoded so it can't drift: `unattended_decision`
    returns `Proceed | RequireAttendedConsent(reason)` and never issues anything — issuance stays the
    `SessionGrantIssuer`'s `requested ∩ policy ∩ ceiling`, so policy can only ever *narrow* the standing
    ceiling.
  - **Tier-16 is the hard cap, checked first.** Unattended above Tier 0 requires an **attested Tier ≥1**
    key store (Inv 16 — no phishable factor recovers a phishing-resistant one). A software-only (Tier 0)
    deployment can **never** do unattended, whatever else is true (tested). Then, fail-closed and ordered:
    paired (Inv 1 — de-listing the key kills unattended) → a standing authorization exists → not expired
    (Inv 3 — never silently permanent; the host renews before expiry, a lapse falls back to attended).
  - **`UnattendedAuthorization`** is a host-local record (controller id + capability **ceiling** + expiry);
    the host trusts its own store (Inv 1). Revocation = drop the record **or** de-list the key (ADR-084) —
    either falls back to attended consent. All facts fed to the pure decision are host-side, never the
    controller's claim.
  - **Scope:** pure decision + model (verifiable off-device), in `ras-grant` (the authorization heart,
    which already sees identity + policy). Deferred: a **signed/portable** authorization form reusing the
    PASETO envelope (control-plane track), wiring the decision into the connect/consent flow + a host UI
    to grant/revoke unattended, and the auto-renew loop. Depends on the pairing registry (ADR-084) and the
    `AssuranceTier` model already in `ras-identity`.
  - **Verify:** the one `Proceed` path (attested + paired + authorized + unexpired, incl. a higher tier);
    Tier-0-is-capped-regardless; and not-paired / not-authorized / expired (with the `now == expiry`
    boundary + one-ms-before proceeds) — all green.

- **ADR-086 · File transfer is a signed catalogue of host-resolved drop targets, never a browse-anywhere
  path · Accepted** (`docs/20 §3.3`, strategy S7, Inv 6). File transfer is *the* danger channel — three
  recent RustDesk CVE classes live on our threat model. So we **reject** the dual-pane browse-anywhere
  file manager (a controller writing an arbitrary host path is exactly Inv 6) and build only the signed
  catalogue.
  - **The controller supplies a target name + a leaf filename + size — never a path.** The vendor
    pre-declares a fixed `DropCatalogue` of named `DropTarget`s, each a **host-chosen** sandbox dir + a
    size cap + an optional extension allow-list. The **host resolves** the destination
    (`dest_dir.join(safe_leaf)`); the controller never chooses where bytes land.
  - **Structurally defends the three CVE classes.** (1) *Path-traversal / zip-slip* (PR #14678):
    `validate_filename` rejects a filename containing any separator (`/`,`\`), `:` (drive / ADS), `..`/`.`,
    NUL/control chars, leading/trailing space or trailing dot, or a reserved Windows device name — so a
    validated name is always a **direct child** leaf (a property test asserts `dir.join(name).parent() ==
    dir` for *every* accepted input). (2) *Capability-bleed into input/capture* (CVE-2026-58056, Inv 15):
    `file.push.<target>` is its own capability namespace; `authorize_file_push` checks **only** that cap —
    never an input/capture cap — and the OS-input gate never maps a file action to a file cap. (3)
    *Symlink-follow write* (CVE-2026-2490): path-string checks are necessary but TOCTOU-prone, so this
    module makes the path **string** provably a safe child leaf, and the (deferred, on-device) write
    backend MUST open with `O_NOFOLLOW`/`openat` — the string guarantee is that write's precondition.
  - **Per-target capability, deny-by-default.** Each target contributes exactly one grantable
    `file.push.<name>` (fine-grained, Inv 15 — never paywalled); nothing is default-on, and a push to one
    target never authorizes another. Per-transfer local confirmation + the size cap round it out (the cap
    is refused, never truncated).
  - **Scope:** the pure catalogue + validator + authorization (in `ras-policy`, which owns capabilities —
    no new crate/dep). Deferred: the wire messages + chunked transfer protocol, the local per-transfer
    confirmation UI, and the `O_NOFOLLOW` write backend (on-device). Only ever in this shape; the
    convenient browse-anywhere version stays rejected (`docs/20 §4`).
  - **Verify:** `validate_filename` rejects every traversal/zip-slip/ADS/reserved-name/control-char case
    and accepts safe leaves (incl. Unicode); `authorize_file_push` is fail-closed + ordered
    (unknown-target / capability-denied / unsafe-filename / too-large / extension-denied) and returns a
    host-resolved child path; the file cap satisfies no input cap; and a **property test** proves every
    accepted name is a direct sandbox child — all green.

- **ADR-087 · Relative-pointer input for trackpad/touch controllers · Accepted** (`docs/20 §3.6`, mobile
  research). A phone has no on-screen cursor to place, so the absolute normalized `PointerMove` is
  unusable there; the mobile-research finding is that a touch controller needs a **relative** pointer
  primitive (a trackpad delta).
  - **`InputAction::PointerMoveRelative { dx, dy }`** — a bounded `i16` pixel delta from the pointer's
    current position, display-independent (no `display_id` / `layout_version`; relative motion needs no
    capture geometry). Closed variant like every other input action (Inv 6 — never a keysym/path). Wire
    oneof 8; fail-closed codec + fuzz.
  - **Same `pointer.move` capability, gated identically (Inv 15).** Relative motion is still cursor
    movement, so `required_cap` maps it to `pointer.move` — a lease without it denies a relative move at
    the per-message gate (tested); no new capability (it is not a broader authority than absolute move).
  - **`OsInputSink::pointer_move_relative` has a default no-op** so backends stay source-compatible; a
    backend that supports it overrides. The **macOS (CGEvent) override has landed** (`ras-input-macos`):
    it reads the *live* cursor position (a null `CGEvent` reports it, so it composes with local motion),
    adds the delta, **clamps to the whole-desktop union** so it can never park off-screen (Inv 6
    fail-safe), and posts a `MouseMoved` that also carries `kCGMouseEventDeltaX/Y` so relative-aware apps
    (games) see the motion — compile+clippy-clean on macOS, the union math unit-tested; live injection is
    the on-device row. The **Linux (XTEST) override has landed too** (`ras-input-linux`): same shape —
    `QueryPointer` for the live position → add delta → clamp to the desktop union → absolute
    `MotionNotify` (XTEST relative motion skipped in favour of the clamped absolute move); cross-compile +
    clippy-clean for `x86_64-unknown-linux-gnu`, union math unit-tested, live XTEST run on-device. The
    **Windows (`SendInput`) override has landed too** (`ras-input-windows`): the simplest of the three —
    `MOUSEEVENTF_MOVE` **without** `ABSOLUTE` is native relative motion (Windows clamps to the virtual
    desktop itself), so it is a one-line direct send; cross-compile + clippy-clean for
    `x86_64-pc-windows-msvc`, live run needs Windows hardware the team lacks. All three OS backends now
    inject relative motion. The **client-side touch-gesture → closed-action translator** (so the host
    only ever sees clicks/wheel/relative-moves — Inv 6) is the remaining follow-up; the mobile controller
    also depends on `keyboard.text` (ADR-083, done).
  - **Verify:** codec roundtrip + fuzz (ras-protocol) and the per-message gate test (`pointer.move`-less
    lease denies it; with it, authorized) in `ras-control` — green.

- **ADR-088 · Audit journal: a per-session SHA-256 hash chain of content-free events, made unforgeable by
  a host-signed head checkpoint · Accepted** (Invariant 10, `docs/06 §12`). `ras-audit` was a stub; this
  implements it as pure data-structure + crypto (no clock, no I/O — persistence is the durable-store
  follow-up).
  - **Two guarantees, together.** (1) *Hash chain → tamper-evidence:* every `AuditEntry` commits to the
    previous entry's hash (`SHA-256(domain ‖ seq ‖ prev_hash ‖ timestamp ‖ event)`), so altering,
    reordering, or removing any **middle** entry breaks `verify()`. The chain **alone** is not unforgeable
    (anyone can recompute a fresh valid chain, or truncate the tail) — so it is paired with (2) *host
    signature → authenticity:* the host signs a `Checkpoint` over the current head with its identity key
    (the `ras-identity` `KeyStore` seam). A verifier pinning the host public key then detects **any**
    rewrite — a forged chain has a different head, so the old signed checkpoint no longer matches and no
    valid new one can be produced without the host key. Domain-separated hashes/signatures + a
    session-id-bound genesis (a chain from another session can't be spliced in).
    - **Verification MUST pin the trusted host key (authenticity is not self-certifying).** `Checkpoint::verify`
      takes the verifier's independently-known host public key and checks the signature under *it* (and that
      the embedded `signer` equals it) — never under the checkpoint's own `signer` field. Verifying under the
      embedded key would be a forgery oracle: an attacker who rewrites the journal can mint their own keypair,
      sign a fresh checkpoint over the rewritten head, stamp in their pubkey, and it would "verify." Authenticity
      comes only from a key the verifier already trusts. (Caught in review; the `signed_checkpoint_round_trips_and_catches_rewrites`
      test now asserts an attacker-key checkpoint is **rejected** under the pinned host key.)
  - **Content-free by construction (Inv 8/11).** `AuditEvent` carries only enum tags + counters
    (generation, a clipboard **byte length**, an `ErrorCode`) — never a pixel, keystroke, clipboard byte,
    typed text, filename/path, or secret. There is *nowhere* to put content; a `content` field is absent
    by design. `ErrorCode` is encoded by its stable `as_str` form, so the chain never depends on enum
    ordering.
  - **Append-only.** `AuditJournal` exposes `append` + read accessors — no edit/remove API; `verify` /
    `verify_chain` recompute from genesis and report the first `ChainBroken { seq }`.
  - **Host-loop wiring — NOW LANDED.** An `AuditSink` DI seam (`ras-core::deps`, `with_audit_sink`)
    receives events **synchronously and losslessly** at each security point — deliberately unlike the
    advisory, bounded, **drops-on-full** `LifecycleEvent` stream (an audit that could drop entries would
    be worthless). The `HostSession` records `ConsentGranted`/`ConsentDenied` at the authorization gate (a
    **refused** connection is audited too), `SessionStarted` once streaming, `ControlLeaseGranted` /
    `ControlLeaseRevoked`, `InputRejected` (the per-message gate, Inv 15), `ClipboardApplied`/`Rejected`,
    `AudioStarted`/`AudioStopped` (only when `audio.listen` gates the pump on), `EmergencyStop` +
    `SessionEnded` on revoke, and `SessionEnded` on graceful stop — each *before* the equivalent lifecycle
    emit. The sink owns the clock + journal + persistence, so `ras-core` stays clock- and I/O-free.
    Loopback-tested: recording sinks over a real journal capture the sequence (incl. the consent
    granted/denied, revoke, and audio-start/stop paths) and the **hash-chain verifies**.
  - **Durable persistence — NOW LANDED (append-only file).** `AuditLog` is a thin, separate layer over
    the pure journal: each `AuditEntry` is written as a `u32`-length-prefixed record (`seq ‖ timestamp ‖
    prev_hash ‖ entry_hash ‖ event`, the `ErrorCode` as its stable 2-byte `to_code`). It is **append-only
    + crash-safe**: `load` reads every complete record and **stops at a partial/undecodable trailing
    record** (a crash mid-append) without failing or corrupting the valid prefix; middle tampering is
    *not* accepted silently — the reloaded entries still run through `verify_chain` (which breaks on any
    altered link) and a signed `Checkpoint` over the head catches a whole-file rewrite. SQLite is **not**
    used (avoids a `-sys` dep); the flat log suffices for the per-session journal. Fully unit-tested with
    a tempfile: persist→reload→verify, torn-tail tolerance, and a same-length event swap caught by the
    chain. Added `ErrorCode::to_code`/`from_code` (stable numeric, matching the wire numbering) to
    `ras-protocol` for the compact round-trippable encoding.
  - **Scope:** the pure journal + chain + signed checkpoint + the append-only `AuditLog` (in the
    new-dep-light `ras-audit`: `sha2` (RustCrypto, MIT/Apache) + the `ras-identity`/`ras-protocol` seams),
    plus the host-loop `AuditSink` wiring above. Deferred: forward-secure key evolution + Merkle-batched
    checkpoints (`docs/06 §12`), and the file-push accept/reject source points (once the file-transfer
    protocol reaches the host loop — the catalogue/validator landed in ADR-086 but is not yet wired).
  - **Verify:** chain links + verifies; append is deterministic + session-bound; content-tamper / reorder
    / middle-removal each break the chain at the right `seq`; a signed checkpoint round-trips and a
    rewritten journal (shorter "clean" history, tampered head, or an attacker-key signature) fails
    against a host-key-pinning verifier; empty journal verifies + checkpoints — all green.

- **ADR-089 · File-transfer authorization wire: an offer → authorize → per-transfer consent → accept/reject
  decision, over the control channel · Accepted** (wires ADR-086's catalogue/validator; `docs/20 §3.3`).
  ADR-086 landed the signed-catalogue *validator* but nothing invoked it over the wire; this lands the
  **security decision half** of file transfer (the dangerous part) and defers the byte streaming.
  - **Wire:** `ControlMsg::FileOffer { target, filename, size }` (controller→host — a target name + a
    **leaf filename** + size, **never a path**), answered by `FileAccept` or `FileReject { code }`
    (proto oneof 17–19; the offer's `target`/`filename` lengths are bounded + fail-closed at decode).
  - **Host flow (`host_handle_file_offer`).** ① `ras_policy::authorize_file_push` against the vendor
    `DropCatalogue` (injected via `with_file_catalogue`; **absent ⇒ no target ⇒ refuse**) + the session
    grant's `file.push.<target>` capability (Inv 15 — never the controller's claim) + the safe-leaf
    `validate_filename` (the traversal/zip-slip CVE-class defense) + the size cap. ② **per-transfer local
    consent** — a new `FileConsent` seam (`with_file_consent`), default `DenyAllFileConsent` (fail-closed:
    no transfer without a live local Allow, Inv 1), awaited outside any lock. The host resolves the
    destination; the controller never chooses where bytes land. Every outcome is **audited** content-free
    (`FilePushAccepted` / `FilePushRejected{code}`, ADR-088) and surfaced as a `FileTransferAccepted` /
    `FileTransferRejected` lifecycle event on both sides. `FilePushError` maps to a stable wire code
    (capability/extension → `CapabilityDenied`; unknown-target/unsafe-filename/too-large →
    `InvalidMessage`).
  - **Scope:** the offer/authorize/consent/accept-reject **decision** + audit (verifiable off-device).
    Deferred: the `FileChunk`/`FileComplete` streaming, a `FileWriteSink` seam, and the `O_NOFOLLOW`/
    `openat` on-device write (ADR-086's third CVE-class defense). Only ever the signed-catalogue shape;
    browse-anywhere stays rejected.
  - **Verify:** wire round-trip + oversize-name-refused + fuzz (ras-protocol); a `ras-core` loopback test
    over all five paths — authorized+consented → accept + audited; consent-denied, capability-withheld,
    a **traversal filename**, and an unknown target → the right reject code + audited — all green.

- **ADR-090 · File-transfer byte streaming: chunk → host-resolved `O_NOFOLLOW` write → complete, size-
  capped · Accepted** (completes ADR-089; `docs/20 §3.3`). ADR-089 landed the offer/authorize/consent
  *decision*; this lands the data path (only the OS write backend stays on-device).
  - **Wire:** `ControlMsg::FileChunk { data }` (sequential; bounded [`MAX_FILE_CHUNK`] = 256 KiB, refused
    at decode) + `FileComplete` (proto oneof 20–21).
  - **Host transfer machine.** On accept, `host_handle_file_offer` opens the **host-resolved** destination
    on an injected `FileWriteSink` (`with_file_write_sink`) and arms one `ActiveTransfer{received,
    declared_size}` (no backend ⇒ the offer is refused — a host that can't write can't receive). Each
    `FileChunk` is written and the running total tracked; a chunk that would **exceed the offered size**,
    or a write error, **aborts** (discard the partial file) — the size-cap DoS defense on the byte path.
    `FileComplete` finalizes **iff** `received == declared_size`, else aborts (no truncated file). A stray
    chunk with no active transfer is ignored; teardown aborts any in-progress transfer. **One transfer at a
    time:** a second `FileOffer` while one is in flight is an out-of-sequence protocol violation, refused
    fail-closed (`InvalidMessage`) **before** authorize/consent — so a malformed/hostile controller can
    neither prompt a wasted consent nor overwrite the active-transfer state and orphan the first partial
    file on disk. **Emergency stop overrides an in-flight offer (Inv 4):** because file consent is awaited
    *inside* the control loop and the loop's join is only best-effort-awaited on stop, a stop can land while
    an offer is parked at its consent prompt. `host_handle_file_offer` therefore checks `stop` **before**
    prompting *and re-checks after* consent returns (refusing with `SessionRevoked` before it ever opens the
    sink — mirroring the control-lease path), and `host_handle_file_chunk` drops any further bytes once
    `stop` is set — so a transfer can never be armed, nor a byte written, on a revoked session.
  - **The `FileWriteSink` seam** carries the ADR-086 **symlink-follow (TOCTOU) defense**: the impl MUST
    `open` with `O_NOFOLLOW`/`openat` and refuse a symlink/existing entry — the safe-leaf path string
    (ADR-086) is the *precondition* that makes that write sound. **The Unix backend has landed**
    (`ras-files::SafeFileWriter`): `O_NOFOLLOW | O_CREAT | O_EXCL` at mode `0600` — a symlink dest is
    refused (`O_NOFOLLOW`), an existing entry is refused (`O_EXCL`, never clobber), `abort` removes the
    partial. Pure `std` + `libc` (no `ras-core` dep — the app wraps it in a `FileWriteSink`), `unsafe`-
    free, and — unusually for an OS backend — **genuinely unit-tested off-device** with real tempfiles
    (write+read-back, existing-file-refused, and a **real symlink refused with the target left
    untouched**). `O_NOFOLLOW` guards only the final component, so the sandbox dir must be host-owned
    (documented). The **Windows backend landed too** (`CreateFileW` + `CREATE_NEW` — the atomic `O_EXCL`
    analogue: refuses any existing entry incl. a symlink/junction, no sharing), behind the same
    `SafeFileWriter` type; `unsafe` is confined to that FFI path (the crate relaxes `unsafe_code` like the
    input backends), the raw `HANDLE` is stored as an `isize` to stay `Send + Sync` (a compile-time
    assertion enforces it), and it is **cross-compile + clippy-clean for `x86_64-pc-windows-msvc`** (the
    live run needs Windows hardware, as everywhere).
  - **Verify:** wire round-trip + oversize-chunk-refused + fuzz (ras-protocol); a `ras-core` loopback test
    with a recording `FileWriteSink` — a full offer→accept→chunks→complete lands the **bytes intact, in
    order**, at the resolved path with `finish` called; an **over-run** (chunk larger than the offered
    size) **aborts** and never finalizes; and a **concurrent second offer** mid-transfer is refused
    (`InvalidMessage`) without re-opening or aborting the in-flight transfer, which still completes intact
    — all green.

- **ADR-091 · Session reconnection: controller-driven re-dial that re-proves the endpoint-bound grant,
  never a new authorization path · Accepted** (backlog X1 — the top production ship-blocker; `docs/21`).
  The session state machine already has `Suspended → TransportRestored → Active`, but nothing fired
  `TransportRestored`: on transport loss the controller went `Active → Suspended`, slept the reconnect
  window, then `ReconnectWindowExpired → Terminated`. A remote-access tool that dies on every Wi-Fi
  blip / NAT rebind is not shippable; every incumbent auto-restores. This adds the missing **re-dial
  driver** — under the strict constraint that reconnection introduces **no new trust**.
  - **The load-bearing security rule: a re-dial is the *existing* handshake over a fresh transport, not
    a resume shortcut.** On reconnect the controller re-establishes the connection to the **same peer**
    and **re-presents its existing `SessionGrant`**; the host runs its **normal, unchanged
    `validate_grant`** (signature under the host key, **sender-constraint against the freshly
    transport-authenticated endpoint**, `not_before ≤ now ≤ expires_at`, version) — identical to a first
    connect. There is **no "resume" code path that trusts the prior session**: authority comes only from
    a grant that still validates *now*. (Inv 3/9/15 hold by construction — we reuse the validator, we do
    not fork it.)
  - **Consequences that fall out of that rule (all fail-closed):**
    - **Grants are short-lived, so the reconnect window is implicitly TTL-bounded.** If the grant expired
      during the outage, the re-dial's `validate_grant` fails `expires_at` → resume is **refused**, and
      the session terminates. You cannot reconnect on a dead grant. (The configured `reconnect_window`
      must be ≤ the grant TTL to be meaningful; a longer window just terminates at TTL.)
    - **No new consent on resume, by design and safely.** The still-valid grant *is* the recorded consent
      (Inv 1 was satisfied when it was issued); re-prompting would be security-theater. An **expired**
      grant, by contrast, gets **no silent renewal** — it requires a fresh `AccessRequest` + consent (a
      new connect), never a resume.
    - **Emergency stop / revoke during suspension wins.** Revocation lives in host-side grant/lease state
      (`revoke_all` bumps the generation; a stop marks the session terminal); a re-dial presenting a
      grant for a revoked session still re-validates the *grant*, but the host's lease generation has
      moved on, so no OS input authorizes (Inv 15/4) — and a host that chose to end the session simply
      is not accepting. The controller cannot re-animate a session the owner killed.
    - **The replay nonce is not re-spent.** Re-dial presents the *grant* (session ALPN, `.with_grant`),
      not a fresh signed `AccessRequest`, so the single-use `AccessRequest` nonce cache is untouched;
      the grant's own `not_before/expires_at` is the freshness bound.
  - **No black screen on resume (ties ADR-060 / backlog X2).** A successful re-dial re-runs the handshake
    (grant → `StreamConfig`), fires `TransportRestored → Active`, re-establishes the video/audio ingest,
    and **forces an IDR** so the controller's decoder resumes on a keyframe — the re-gate mechanism the
    renderer-attach path already uses.
  - **Mechanism.** A `SessionTransport::reconnect()` seam (default *unsupported* so the iroh backend is
    unchanged until its on-device re-dial lands; the loopback implements it for tests). The controller
    control loop, on a transport-loss error, drives the re-dial inside the window: re-establish → re-fetch
    the control channel → re-handshake (which re-validates the grant host-side) → `TransportRestored` +
    forced keyframe → notify the ingest loops to re-fetch their sources → continue. On window expiry →
    `Terminated` (unchanged).
  - **Scope / deferred.** This lands the **driver + seam + coordination + loopback re-dial test** (cut
    mid-stream → `Suspended` → re-dial → `Active` → fresh IDR flows). The concrete **iroh re-dial**
    (`Endpoint::connect` again + real network re-handshake, verified over an actually-impaired link with
    NAT rebind) stays the on-device follow-up, like every other iroh-specific step.

## Contacts, presence & signaling (`docs/design/contacts-and-signaling-design.md`)

- **ADR-092 · Durable, mutual Contacts book (extends the paired-controller registry) · Accepted.**
  - **Context.** Every connection today needs a fresh ticket copy-pasted out of band. `docs/16` already
    names the fix — *"from then on that identity — not the ticket — is the durable trust anchor"* — and
    ADR-084's `PairingRegistry` is its host-side half. Generalize it to a **bidirectional address book**.
  - **Decision.** A `Contact { id (Ed25519 pubkey = EndpointId), label, added_at, last_seen_at, blocked }`
    saved by **both** peers at first pairing (identities are exchanged once, mutually). A `ContactBook`
    seam — `InMemoryContactBook` (MVP) + the durable **`FileContactBook`** (a hand-rolled length-prefixed
    snapshot with **atomic temp+rename** writes and fail-closed decode) — with add / get / list / touch /
    block / remove. **Not SQLite** (superseding the initial note): contacts are a small, low-write,
    query-by-key set, so a file snapshot gives restart-survival with **zero new C/`-sys` dependency** and
    a smaller supply-chain surface (Inv 18), matching the `ras-audit`/`ras-files` precedent. Key-change
    detection is free (id = pubkey; a rotated key is a *new, unverified* entry, surfaced never
    auto-trusted). De-listing / blocking is the kill-switch.
  - **Invariants.** Identity, never authority (Inv 9): a contact hit governs only *finding* a peer and
    *whether the human prompt is pre-filled* — it authorizes nothing. Content-light; no secret (Inv 8).

- **ADR-093 · Ticketless connect to a saved contact via dial-by-EndpointId + discovery · Accepted.**
  - **Context.** iroh 1.0.2 can dial a peer by `EndpointId` alone (no ticket) through its DNS/pkarr
    discovery, *if* the peer is online + discoverable (verified in source; `presets::N0` — already used —
    wires discovery). This is the headline UX win and needs no gossip.
  - **Decision.** Reach a saved contact by dialing its stored `EndpointId`; fallbacks in order:
    discovery-by-id → stored last-known `EndpointAddr` hints → "contact appears offline". The normal
    two-phase connect + consent + grant run unchanged on top.
  - **Invariants.** The dialed connection is still `EndpointId`-authenticated by QUIC/TLS; the grant is
    still fresh, short-lived, endpoint-sender-constrained (Inv 3); consent still required (Inv 1).

- **ADR-094 · Presence + signaling over iroh-gossip 0.101 · Accepted.**
  - **Context.** `iroh-gossip = "0.101.0"` depends on `iroh ^1` → compatible with our pinned `1.0.2`
    (buildable now). Gossip is **best-effort, unordered, no persistence**; payloads are **not**
    author-authenticated (`delivered_from` = forwarding neighbor, not origin); `TopicId` (32 B) is a
    **bearer secret** (anyone who knows it joins); max message ≈ 4 KiB.
  - **Decision.** Presence via **per-contact-pair secret topics** (topic id derived from a shared secret
    established at pairing — unguessable, private to the pair), carrying periodic **signed** "online"
    beacons + `NeighborUp`/`NeighborDown`. **Every gossip payload is signed by the sender identity key
    and verified before use** (`delivered_from` never trusted as author). Gossip is signaling-only
    (≤4 KiB); bulk rides dedicated iroh streams. Owned by a new `ras-signal` crate scoping the
    `iroh-gossip` dependency (**license `cargo-deny`-verified at implementation**).
  - **Invariants.** Deny-by-default: only **saved, non-blocked contacts** are honored (contacts-only —
    strangers refused). Topic secrets + beacons never logged (Inv 8). No offline delivery (see below).

- **ADR-095 · Contact messaging + request-remote-access over a signaling ALPN · Accepted.**
  - **Decision.** A new **signaling ALPN `casual-ras/signal/1`** carries **signed** direct messages
    (reusing the `Redacted` `ChatMessage` body) and **access-request *intents*** between saved contacts.
    A request-intent raises an **incoming-request prompt** (Inv 1 consent, with the focus/notification
    UX already shipped); on Allow, the normal Share/Connect two-phase flow runs. This replaces "text me
    your ticket" **without** replacing consent.
  - **Invariants.** Contact ≠ authorization (Inv 1): the human still clicks Allow; the grant/lease/
    per-message gate (Inv 3/15) and emergency stop (Inv 4) are unchanged. The ALPN is iroh-authenticated
    + app-signed + consent-gated — no new unauthenticated endpoint (Inv 9).
  - **Offline (accepted limitation).** Gossip has no store-and-forward; a message/request to an offline
    contact is queued **locally** and delivered on next presence (best-effort), surfaced honestly. A
    durable **mailbox is out of scope** (it needs an always-on node = backend, conflicting with the
    no-backend-until-Phase-9 stance) — deferred, not built.

## SDK (S1 — extract SDKs from the proven crates)

- **ADR-096 · Start the SDK as a `ras-ffi` C ABI over the proven synchronous core · Accepted.**
  - **Context.** Strategy S1 says: build the reference apps first, then draw the SDK boundary around
    the proven crates and add a C ABI (`cbindgen`) + N-API. The apps exist; this begins the SDK track
    (the actual embeddable product — until now there is none, only two reference apps).
  - **Decision.** A new `crates/ras-ffi` crate (`cdylib` + `staticlib`) exposes a **C ABI**, starting
    with the **pure, synchronous, already-verified** primitives — identity (generate / load-or-create /
    public key / sign), the Crockford contact/pairing code, and connection-ticket parsing — because a
    C ABI over *those* is fully testable off-device, whereas the async session orchestration (a
    callback/runtime FFI model) is the larger follow-up. `cbindgen` generates `casual_ras.h`.
  - **ABI conventions (load-bearing for a stable SDK).** Opaque handles (`Box::into_raw`/`from_raw`)
    with explicit `_free`; **integer status codes** (`0 = OK`) + out-params, never Rust types across
    the boundary; caller-provided byte/char buffers with capacity checks; **every entry point is
    `catch_unwind`-guarded** so a Rust panic can never unwind across the FFI boundary (UB) — it becomes
    an `Internal` status. No secret is ever returned (Inv 8): the identity handle signs + yields the
    *public* key only, exactly like `KeyStore`.
  - **Scope / deferred.** This lands the crate + the synchronous surface + header generation + FFI
    tests. The **session/host/controller SDK** (async, consent callbacks, media), the **N-API** binding,
    `THIRD-PARTY-NOTICES`/SBOM for the SDK artifact, and a real cross-language integration consumer are
    the follow-ups. The SDK is Apache-2.0 (ADR-051) so licensees embed it with no copyleft.
  - **SDK BOUNDARY (load-bearing).** The SDK exposes the **remote-access engine only**: identity,
    authorization (grants/leases/per-message gate), the session (connect / host / view / control /
    consent), transport, and media. It **deliberately excludes** contacts, messaging, presence,
    discovery, and address books — those are **the integrator's product**, not ours: an integrator
    builds their own contact model / user directory / signaling on top of our primitives. Baking a
    contacts opinion into the SDK would force integrators to architect around our choices (the wrong
    place for us to be). `ras-signal` / `ContactBook` / gossip therefore stay **app-only** (the
    reference implementation) and are **never** part of `ras-ffi`. Corollary: the **webapp/browser
    controller** (a remote-access controller over WebRTC/WebTransport, ADR-057) **is** in SDK scope —
    it is the remote-access controller, just on a different transport behind the same `SessionTransport`
    seam.

- **ADR-097 · Viewer annotation markup rendered on the host overlay · Accepted.**
  - **Context.** On-device two-machine testing (issue #5) reported that viewer drawing ("draw…
    nothing works") only appeared on the *controller's* own screen — annotation was a v1 local-only
    canvas, never transmitted. The host had no way to show the viewer's markup.
  - **Decision.** Add a `ControlMsg::Annotate(AnnotateOp)` wire variant (proto oneof field 22) — a
    completed **stroke** (tool + `0xRRGGBB` colour + normalized `0..=65535` points), **undo** (drop
    last), or **clear** (drop all). The host forwards it as `LifecycleEvent::RemoteAnnotation`; the app
    renders it on the existing transparent pointer overlay. Points are normalized to the **video
    content rect** (letterbox-aware, same mapping as the remote pointer) so markup lands on the correct
    host pixels — including a secondary monitor.
  - **Security posture.** Annotation is **display data, not OS input and not a secret** — drawing
    geometry + a colour, exactly like the visual remote pointer (ADR-061). So it carries **no
    capability** (Inv 2 is about *privileged* behaviours; markup touches no OS/input/screen-write
    surface — a live session already required consent). It is **fail-closed on decode**: unknown
    tool/op tags are rejected and the point count is bounded (`MAX_ANNOT_POINTS = 1024`), so a hostile
    or buggy peer cannot force a large allocation; the host also bounds retained strokes (256, oldest
    dropped). It is one-way (controller → host) in this slice.
  - **Scope / deferred.** Wire + core + app + overlay render landed, codec round-trip + a core loopback
    test green. Two-way (host annotating the controller) and the richer cursor model (labeled virtual
    cursors for every participant, touch-style clicks under a control lease) are follow-ups.

## Licensing

- **ADR-051 · Apache-2.0 for the whole repository; reject AGPL/SSPL · Accepted (add full LICENSE +
  counsel sign-off on codec patents before opening the repo).**
  - **Single permissive license — Apache-2.0** across the repo (dropping the earlier open-core/BSL
    plan). Rationale: it is the whole point of an embeddable SDK that customers can ship it in
    proprietary apps with no copyleft obligation; Apache-2.0 adds an explicit **patent grant +
    retaliation clause** and is the Rust-ecosystem norm.
  - **Consequence accepted:** no field-of-use restriction → competitors may also use the code,
    including the fraud subsystem. Differentiation rests on execution, brand, operated
    relays/control-plane, and support — not the license. **MPL-2.0** is the only alternative under
    consideration (file-level weak copyleft: still embeddable, but core-file changes stay open).
  - **AGPL / SSPL rejected** (viral/network copyleft — would force licensees to open-source their
    apps).
  - **Dependency hygiene (hard blocker):** allow MIT / Apache-2.0 / BSD / ISC / Zlib / Unicode-DFS /
    **MPL-2.0**; **deny GPL / LGPL / AGPL / SSPL** as build-breaking via `cargo-deny`. **RustDesk
    (AGPL) is study-only, never linked/vendored** — pull `scrap`/capture/codec crates from permissive
    **upstream crates.io**, never the RustDesk fork. `cargo-about`/`cargo-bundle-licenses` →
    `THIRD-PARTY-NOTICES`; CycloneDX SBOM per release.
  - **Codec patents ≠ copyright:** BSD-2 on `openh264` grants no H.264 patent rights. Prefer OS/GPU
    hardware encoders or a royalty-free default (AV1 via `rav1e`/`dav1d`). Flag for IP counsel.
  - **Contributions:** DCO (`Signed-off-by`); a CLA is optional under Apache-2.0.
  - *Not legal advice — add the full Apache-2.0 text and get counsel sign-off on the codec-patent
    strategy before shipping.*

## Open decisions (tracked, not yet ADRs — see `docs/15 §7`, `docs/16 §6`)
Cool-off durations & gated capability classes per vertical · Apache-2.0 vs MPL-2.0 final call ·
codec strategy (royalty-free vs licensed vs HW-only) · minimum tier binding per vertical · whether to
offer recording at all ·
tamper-resistance vs anti-stalkerware bound · enterprise-console egress scope · concurrent-telephony
detection acceptability · live technical-assumption validation (Chrome 138 UIA, OCR MSIX identity,
`consent.exe` enumeration, FIDO2 ergonomics in Tauri/Rust).
