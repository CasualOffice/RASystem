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
- **ADR-064 · MVP `SessionGrant` = PASETO v4.public, not Biscuit · Proposed** (needs sign-off;
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
