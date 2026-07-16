# CLAUDE.md — Operating Guide for Casual RAS

> This file is the single source of truth for how anyone (human or AI agent) works in this
> repository. Read it fully before proposing changes. If a change would contradict anything
> under **Non-Negotiable Invariants**, stop and raise it instead of implementing it.

---

## 1. What this project is

**Casual RAS** (Casual Remote Access System) is an **embeddable, white-label remote-access
platform**. Software vendors embed it into their own applications to add secure screen
viewing, remote control, multi-user collaboration, and approved support actions — without
sending their users to a separate branded remote-desktop product.

It is **not** primarily a standalone remote-desktop app. The end products are:
a native **host runtime** (embedded in the customer's app / on the controlled machine),
a **controller app** (the technician/support side), a shared **Rust core**, and — later —
**SDKs** extracted from that core.

Transport is peer-to-peer over **Iroh/QUIC** (encrypted, NAT-traversing, relay-fallback).
Authorization in the MVP is **host-issued**: the host validates a signed access request,
gets local consent, and issues a short-lived signed **session grant**. A future server can
replace only the grant *issuer* without changing the host validator or the wire protocol.

---

## 2. Priorities — the ordering is a decision rule, not a slogan

**1. Security → 2. Latency → 3. UX.**

When two of these conflict, the higher one wins. Concretely:

- **Security beats latency.** Never skip consent, grant validation, capability checks, lease
  checks, or audit writes to shave milliseconds. Do not cache authorization decisions past
  their signed expiry to "go faster."
- **Security beats UX.** Never hide active remote control, remove the emergency stop, or
  suppress an OS permission prompt to make onboarding smoother.
- **Latency beats UX.** Prefer a responsive local cursor and fast frame path over richer but
  slower UI. A stalled video must never freeze the controller's own pointer or the stop button.

If you believe a specific case justifies inverting the order, that is an architecture decision:
write an ADR (see `docs/14_DECISIONS_ADR.md`) and get sign-off. Do not invert it silently.

---

## 3. Current status

- **Phase 0 complete — Milestone M0 reached.** The design doc set is done and the Cargo workspace
  skeleton builds clean. **Phases 1 and 2 are implemented and green (M1 media/transport landed, M3
  authorization reached);** the design gates (`docs/design/phase-1-design.md`, `phase-2-design.md`)
  are written and their spines built. **Phase 3 (M4) enforcement core is implemented and CI-green**
  (leases + per-message gate + macOS input backend + orchestration); the app UI + on-device input
  verification are the remaining steps (see below).
- **Live progress tracker:** `docs/17_ROADMAP_AND_MILESTONES.md` (per-phase ☐/◐/☑ checkboxes) is the
  single source of truth for what's done; spike measurements are recorded in
  `docs/design/phase-S-design.md §4.1`. Keep both current as work lands.
- **Phase S (risk spike) — mostly measured, one item pending.** WebCodecs bet is **GO**: measured on
  Chrome (e2e 7.1/10.5 ms) *and* Safari/WebKit (e2e 4.0/5.0 ms, 60 fps, 0 drops) — Safari is the
  WKWebView engine, so the macOS-lead controller render path is validated and the native-surface
  PIVOT is off the table. **macOS capture→encode is GO** (`spike/macos-capture`, on-device run): SCK
  delivers a frame-accurate 16.67 ms/60 fps cadence on change (coalesces static frames — a bandwidth
  feature), pixel extraction costs ~20–40 µs/frame, and VideoToolbox H.264 **encode latency is ~11 ms
  med / ~13 ms p95** at 60 fps with a cleanly-decoding Annex-B stream (`ffprobe`-verified). Uses the
  pure-Rust **`objc2`** bindings (no Swift bridge), the family the real `ras-media-macos` backend
  should adopt. **Still pending (blocks the M1 go/no-go ADR):** the iroh network-matrix probe (needs a
  Mac↔Linux two-machine run) and the minor rVFC compositor-penalty delta. The media go/no-go is
  independently cleared, so the **real macOS media backend has landed** (`ras-media-macos`, see below);
  only the concrete **iroh transport** stays stubbed behind its trait until the network go/no-go.
- **Phase 2 (identity/pairing/authorization → M3) — IMPLEMENTED, M3 reached.** "No frames without
  authorization" is live: persistent Ed25519 identities (`ras-identity`, `KeyStore` seam, Tier 0
  `SoftwareKeyStore`), rotating single-use connection tickets + a bounded TTL-swept nonce cache
  (`ras-bootstrap`), signed `AccessRequest`s and sender-constrained **PASETO v4.public** `SessionGrant`s
  with an ordered validation matrix (`ras-grant`, hand-rolled PASETO envelope over `ed25519-dalek`,
  byte-verified against the official v4 vectors — ADR-064/065/066, all **Accepted**), the real
  `GrantSessionValidator` filling the Phase-1 §5.5 auth seam (`ras-core`, sender-constraint enforced at
  the moment iroh proves the peer endpoint), a separate **bootstrap ALPN** (`casual-ras/bootstrap/1`)
  in `ras-transport-iroh`, and the **unified app's two-phase Connect** (bootstrap → signed
  `AccessRequest` → grant → session ALPN with `.with_grant`) + real host-side local Allow/Deny consent
  (Invariant 1). The M3 security-test matrix is green (`docs/design/phase-2-design.md §9.1`): ticket
  replay/expiry/stale-generation, request/grant signature+endpoint+host+expiry+nonce, unknown-capability
  drop + reduced-never-expands property, plus never-panic decoder fuzz — every crate suite passing.
  **Pending:** on-device GUI runtime verification of the two-phase flow (Tauri/WebView + Screen-Recording
  TCC — developer step).
- **Phase 3 (remote control & collaboration → M4) — enforcement core IMPLEMENTED; app + on-device
  verification pending.** The design gate `docs/design/phase-3-design.md` is signed off
  (**ADR-067/068/069 Accepted**) and the bottom-up crate work has landed and is CI-green:
  - `ras-policy` `phase3_default_policy` (OS input becomes grantable behind a lease; `keyboard.text`/
    clipboard/file/recording still withheld);
  - `ras-protocol` **OS-input wire** (ADR-067): `InputEnvelope{lease_id, generation, seq, action}` +
    the closed `InputAction` set + `ControlRequest/Granted/Revoked/Input` `ControlMsg` variants
    (proto oneof 8–11, fail-closed codec + fuzz);
  - `ras-control` **`LeaseManager`** + the **O(1) per-message gate** `authorize_input` (generation →
    lease → expiry → seq → layout → capability), host-authoritative (ADR-069, the RustDesk-CVE fix,
    Inv 15) — pure, `unsafe`-free, 16 tests covering the M4 matrix at the logic layer;
  - `ras-input-macos` (ADR-068): unprivileged **CGEvent** `OsInputSink`, PostEvent-TCC-gated (not
    Accessibility), Secure-Input-respecting, tracked-key `release_all`, empty off-macOS;
  - `ras-core` wiring: `OsInputSink` + `ControlConsent` DI seams (fail-closed default), `LeaseManager`
    seeded at `Active`, `ControlRequest`→consent→issue and `Input`→gate→sink in the host loop,
    `revoke_all`+`release_all` on emergency stop / teardown (Inv 4), content-free lifecycle events,
    and an end-to-end loopback test.
  - **app** wiring: the bootstrap request + host issuer now use `phase3_default_policy` (so the grant
    ceiling can include input); a **second** control-lease consent (`LocalConsent` → `ControlConsent`,
    Inv 1) gates injection; Share builds a macOS `CgEventSink` (`with_input_sink`) fed capture geometry
    and surfaces a "REMOTE CONTROL ACTIVE" indicator; Connect has a "Take control" button + forwards
    the viewer's pointer/keyboard/wheel as `Input` (normalized to the video rect, JS→USB-HID map,
    monotonic seq) when it holds the lease. App `check`/`clippy`/`fmt` clean.
  macOS is the lead input platform (ADR-054/055). A **Linux X11 input backend has landed**
  (`ras-input-linux`, **ADR-070**): a second `OsInputSink` over the X11 **XTEST** extension via the
  **pure-Rust `x11rb`** (so it is `unsafe`-free, unlike the CGEvent crate), deliberately unprivileged
  (connects to `$DISPLAY` as the user — X11/Xwayland only, no root/uinput, fail-closed when no X server
  so the host refuses the lease), HID→evdev(+8) keycode map, held-modifier reconciliation (X11 has no
  per-event flag), tracked-key best-effort `release_all` (Inv 4), and the same normalized→pixel
  geometry seam. Empty off-Linux; **cross-compile-checked + clippy-clean for `x86_64-unknown-linux-gnu`
  from the macOS dev machine** and unit-tested (HID table, coord clamp, non-finite guard); its x11rb
  tree passes `cargo-deny` (Inv 18). **Still pending:** the **on-device** GUI run of the real CGEvent
  injection + PostEvent-TCC prompt + Secure-Input drop (same constraint as every prior macOS backend)
  and the analogous **Linux on-device** XTEST run (a real X11/Xwayland session); app wiring of the
  Linux sink into Share's `make_backends`; a macOS **global-hotkey** emergency stop (baseline stop is
  the always-visible Stop button, which already drives `revoke_all` + `release_all`; no kernel SAS on
  macOS — SAS stays the Windows path); the Linux **`uinput`/libei** follow-up backends (docs/19 §3);
  and the **Windows** input backend (parallel port of `ras-input-macos`, `windows-rs SendInput`, no
  UIAccess — docs/19 §4).
- **What exists:**
  - Phase 0: dependency-free crate skeletons under `crates/`; `deny.toml` license gate;
    `.github/workflows/ci.yml`; `proto/casual_ras.proto` placeholder.
  - Phase 1 spine (verified `cargo test`, no iroh/OS/GPU): canonical cross-crate types + error
    taxonomy (`ras-protocol`/`ras-media`), the pure session state machine, DI seams (`ras-core::deps`),
    typed lifecycle events (`ras-core::event`), the no-op auth seam (`AllowAllValidator` behind
    `insecure-no-auth`), and the **host + controller orchestrators** (`ras-core::session`). Exercised
    end-to-end by a synthetic capture/encode double (`ras-media::synthetic`) over an in-memory
    loopback transport (`ras-core::testkit`) in a `#[tokio::test]` (streaming + keyframe round-trip +
    teardown). `ras-core` now depends on `tokio` + `async-trait` (design-sanctioned, permissive).
    The **emergency-stop / revoke runtime path (Invariant 4)** is implemented and loopback-tested:
    `HostSession::emergency_stop` takes the audit-distinct `Revoke → Revoked` edge, halts the media
    pump before its next send (no post-revoke frame leak), and flushes a bounded `Bye{SessionRevoked}`
    so the controller ends `Revoked` — verified ≤250 ms local, idempotent, non-downgradable. Teardown
    now has **three separable paths (ADR-056)** via the new `ErrorCode::NormalClosure` wire code:
    clean `Bye{NormalClosure}` → `Terminated` (prompt), `Bye{SessionRevoked}` → `Revoked` (host only),
    and a missing `Bye` → `Suspended` (transport loss). The testkit gained a `LoopbackCut` fault
    handle to exercise the last path honestly.
  - **Real macOS media backend (`ras-media-macos`), on-device verified.** Implements the `ras-media`
    traits: `ScreenCaptureBackend` (ScreenCaptureKit push-delegate → latest-frame pull adapter) and
    `VideoEncoderBackend` (VideoToolbox H.264 — realtime, no B-frames, Baseline, ∞-GOP with
    forced-IDR-on-demand, ABR `set_bitrate`), through the real `PlatformSurface` seam (**ADR-058**:
    a tagged borrowed GPU-surface pointer the paired same-platform encoder recovers fail-closed, so
    `ras-media` stays `unsafe`-free while `unsafe` is confined to this FFI crate per CONTRIBUTING §5).
    Pure-Rust `objc2` bindings (no Swift bridge); the crate is **empty on non-macOS** so Linux CI stays
    green. Driven end-to-end through the traits by `--example capture_encode`: first-frame keyframe,
    gap-free monotonic ids, Annex-B + in-band SPS/PPS on every IDR, `ffprobe`-clean h264, ~8 ms encode.
  - **Unified desktop app (`app/`, Tauri v2), one binary does both roles (ADR-062), builds clean.**
    A home screen offers **Share this screen** (agent) and **Connect to a screen** (viewer); nobody
    installs two apps. The video path: Rust pushes each encoded access unit as the canonical
    `ras_core::frame_channel` blob (24-byte `RAS1` header + Annex-B) over a **binary** Tauri `Channel`;
    the webview decodes with WebCodecs `VideoDecoder` → `<canvas>`, gates on the first IDR, and drives
    forced-IDR-on-demand (`request_keyframe`). Both roles are the real `ras-core` orchestrators
    (`ControllerSession` / `HostSession`) over `IrohSessionTransport` behind the `SessionTransport`
    seam. Built with `ras-core` `default-features = false`, so the Share role uses the **real
    `LocalConsent` `GrantValidator`** (Invariant 1) and the `insecure-no-auth` `AllowAllValidator` is
    **not linked** — the old loopback self-mirror is dropped with it. **Connect is decode-only →
    macOS/Linux/Windows; Share needs a capture backend → macOS-only** for now (`start_sharing` reports
    "not available on this platform yet" off macOS, Connect still works). Static frontend via
    `withGlobalTauri` (no bundler); `core:default` capability on main + transparent overlay windows;
    CSP set; always-visible indicators (Invariant 7). Kept **out of the root workspace** (heavy WebView
    deps); the GUI run is an on-device step (login session + Screen-Recording TCC). The `.app`/`.dmg`
    bundle was built + verified locally on macOS.
  - **`ras-transport-iroh` — control + video + health planes are concrete, and the loopback→iroh
    swap is wired** (iroh `=1.0.2`, ADR-059/060). Real `Endpoint` (bind/id/accept/connect +
    `connect_direct` for same-network dials), `Session` (`remote()` = peer's authenticated
    `EndpointId`; `close(code)` → QUIC app-close code), and `ControlChannel` running the fuzzed
    `FramedControlChannel` codec over iroh's `(RecvStream, SendStream)` — ALPN `casual-ras/1`. The
    **host opens** the single bidi control stream (and every video uni-stream); the controller only
    dials the connection (ADR-059 amended — the original controller-opens draft deadlocked over real
    QUIC because the host speaks first, a bug the pre-wired loopback masked and the two-endpoint iroh
    run surfaced). The **`PerFrameStream` video path** (ADR-060): host `VideoSink` opens one uni QUIC
    stream per frame (bounded drop-at-source channel → sheds under congestion, no latency build-up),
    controller `VideoSource` reads each to FIN and reconstructs the `EncodedFrame` from a 44-byte
    per-frame header carrying the whole `StreamConfig` (a res/bitrate change arrives atomically with
    its IDR), synthesizing a `FrameDropped` on any `frame_id` gap. Distinct per-frame streams never
    HOL-block each other or control (the latency invariant); decode is fail-closed, `read_to_end`
    bounded (8 MiB). A **`HealthObserver`** derives `ConnHealth` on demand from live QUIC stats
    (rtt/bandwidth/path from the selected `PathStats`; cumulative loss from `ConnectionStats`;
    non-blocking, never awaits I/O). The **`IrohSessionTransport: SessionTransport` adapter** (in
    `ras-core`) makes the swap transparent — **the full spine runs end-to-end over two real iroh
    endpoints with no orchestrator/wire change** (`spine_runs_over_real_iroh_transport`). Verified by
    **hermetic tests** (control round-trip asserting peer identity — Invariant 9; a real
    per-frame-stream video exchange with gap detection + live health read; a header round-trip /
    fail-closed-decode unit test; the full-spine iroh e2e). Transport authenticates identity, never
    authority. `cargo-deny` gates iroh's transitive tree via scoped permissive exceptions
    (Unlicense/CDLA-Permissive-2.0 wasm/relay helpers) — Invariant 18 holds.
  - **Alpha two-machine app is usable (view-only + remote pointer).** A **connection ticket**
    (`EndpointAddr::to_ticket`, `CASUALRAS1:<hex>`, fail-closed decode) carries id + direct addrs +
    relay; `Endpoint::online`/`addr`/`connect` dial across NAT (direct + relay, discovery-by-id
    fallback). The **unified `app/` (Tauri, ADR-062)** does both ends from one binary. **Connect**
    (viewer): `connect_to_host(ticket)` / `disconnect` — platform-independent (viewer only decodes) —
    plus viewer-side annotation and a **remote pointer** (its cursor over the shared screen streams to
    the host as `ControlMsg::Pointer` → `LifecycleEvent::RemotePointer`, ADR-061; normalized,
    best-effort, **not OS input** so outside Invariants 6/14). **Share** (agent, macOS-only):
    `start_sharing` / `stop_sharing` publish a ticket and accept one viewer over `IrohSessionTransport`
    serving real `ras-media-macos` capture, with an always-on `REMOTE VIEWING ACTIVE` indicator +
    Stop (Invariant 7) and a transparent, click-through, always-on-top **overlay** drawing the viewer's
    remote pointer on the host's screen. It enforces **real local Allow/Deny consent (Invariant 1)**: a
    `LocalConsent` implements `ras-core`'s `GrantValidator` — a connecting viewer is held in the
    handshake (no pixels) until the local user clicks Allow; Deny or 90 s of silence refuses
    fail-closed. Built with `ras-core` `default-features = false`, so the `insecure-no-auth`
    `AllowAllValidator` is **not even linked** (the old loopback self-mirror is dropped with it). A
    headless `ras-host` (workspace CLI) remains for no-GUI shares. Verified: the app `cargo
    check`/`clippy` clean and its `.app`/`.dmg` bundle builds on macOS; pointer path has a loopback e2e
    (`controller_pointer_reaches_host…`) + codec round-trip.
  - **GitHub release builds are wired** (`.github/workflows/release.yml`): on a `v*` tag (or manual
    dispatch → draft) `tauri-action` bundles the **controller** on macOS/Linux/Windows (dmg / AppImage
    + deb / NSIS — it is decode-only so it ships everywhere today) and the **host** on macOS (dmg).
    Both apps now carry a real bundle config (branded 1024px icon set, `bundle.active`, category);
    builds are unsigned in the alpha (Gatekeeper/SmartScreen warn — EV signing is a hardening-phase
    step). The controller `.app`/`.dmg` bundle was built and verified locally on macOS.
  - **Cross-platform sharing implemented (ADR-063) — Share now targets macOS + Linux + Windows.** A
    shared **software encoder `ras-media-openh264`** (`VideoEncoderBackend`): CPU BGRA → I420 →
    Annex-B with in-band SPS/PPS on every IDR, forced-IDR-on-demand; permissive Cisco **BSD-2**
    (openh264 `=0.8.1`, clears RUSTSEC-2025-0008 which is a *decode*-only overflow we never hit);
    **unit-tested + built locally on macOS** (keyframe SPS/PPS/IDR, row-padding/odd-dim, fail-close).
    A cross-platform **capture `ras-media-scap`** (`ScreenCaptureBackend`) over the permissive `scap`
    crate — **PipeWire+portal (Linux), Windows.Graphics.Capture (Windows)**, SCK (macOS) — drains
    scap's blocking pull on a thread into a latest-frame slot with a condvar-timeout `next_frame`
    (Ok(None) on a static screen, no pump stall); frames normalize to CPU BGRA over the new
    `SurfaceKind::CpuBgra` seam. The unified app's `make_backends()` picks hardware SCK+VideoToolbox on
    macOS and scap+OpenH264 on Linux/Windows. **Verification honesty:** the encoder is verified
    locally; the **Linux/Windows capture paths compile only on their own OS, so CI is the compile gate
    there and on-device runtime verification is pending** (CI installs nasm + PipeWire/dbus/libclang).
    Windows needed a transitive pin: `scap 0.0.8` calls `windows-capture`'s 5-arg `Settings::new`, but
    `windows-capture 1.5.0` grew it to 8 args in a *minor* release, so `ras-media-scap` pins
    `windows-capture = "=1.4.4"` (Windows-only, not used directly) to keep scap compiling.
  - **Runtime ABR is wired on the software (OpenH264) path too.** `ras-media-openh264` now builds the
    encoder in bitrate rate-control mode at the negotiated `target_bitrate_bps` (it previously ran at
    OpenH264's ~120 kbps quality-mode default, ignoring the target) and `set_bitrate` retargets the
    **live** encoder keyframe-free via `SetOption(ENCODER_OPTION_BITRATE)` through `openh264-sys2`
    (BSD-2) — the safe wrapper exposes no bitrate setter. So the `LatencyFirstAbr` in `ras-core` now
    actually adapts both backends. Unit-verified: after a runtime `set_bitrate` drop the encoder emits
    substantially smaller access units for the same content (no reconfigure, no IDR).
  - **ABR loss estimate is now windowed, not cumulative.** `HealthObserver` remembers the previous
    `(sent, lost)` datagram counters and reports `loss_fraction` over the interval since the last
    read (`windowed_loss`), so a burst of loss no longer stays baked into the lifetime average and
    permanently depresses the bitrate — the ABR raises it again once the link recovers. The adapter
    holds one persistent `HealthObserver` so the window survives across the 500 ms ticks. Pure math
    unit-tested (recovery-after-burst, idle-interval, clamping).
  - **Multi-monitor remote-pointer overlay wired.** The macOS capture backend reports the shared
    display's global bounds (`SCDisplay.frame`, logical points) via the new
    `ScreenCaptureBackend::captured_bounds`; `HostSession` emits them as `LifecycleEvent::CaptureGeometry`;
    the app positions + sizes the pointer overlay to cover exactly that display (macOS points map 1:1
    to Tauri `Logical*`, and the pointer is normalized, so it lands right even on a secondary monitor,
    not just the primary — replacing the old `maximized`-on-primary overlay). Fail-safe: no bounds →
    default overlay. Compiles clean; the multi-monitor behavior is an on-device verification step.
  - Still stubbed / deferred (`todo!()` or additive): iroh **reset-on-stale + FEC** and the
    `DatagramFec` video alternative (behind `StreamConfig::video_transport`),
    **hardware encoders + Wayland DMA-buf zero-copy** (Linux/Windows use the
    software OpenH264 path), the **Phase-2 grant/lease/capability
    model** (consent is now real local Allow/Deny, but authorization is still coarse — no signed
    grants/leases, no capability scoping, no TPM tiers), and EV
    code-signing/notarization of the release bundles. **(Excluding the host's own overlay/indicator
    windows from macOS capture is now done — `CaptureOptions::excluded_window_ids` → `SCWindow` via
    CGWindowID; the app supplies the ids from each Tauri window's `NSWindow.windowNumber`.)**
- **Build/verify commands** (all green as of M0):
  - `cargo build --workspace`
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test --all`
  - `cargo deny check` (license gate: allows MIT/Apache/BSD/ISC/Zlib/MPL; denies GPL/LGPL/AGPL/SSPL)
  - `cargo bench -p ras-core --bench hot_paths` (hand-rolled hot-path micro-bench + loose sanity
    ceiling; no criterion — runs in CI as a gross-regression smoke check)
- **Deviation resolved** (`docs/design/phase-0-design.md §8`): the deferred protobuf codegen is now
  wired. `crates/ras-protocol/build.rs` compiles `proto/casual_ras.proto` with **`protox`** (pure-Rust,
  no system `protoc`, no network, no vendored binary) + `prost-build` into `OUT_DIR`; `ras-protocol::codec`
  maps `ControlMsg` ⇄ the generated wire types (non-breaking — the hand-rolled enum stays the public
  API) with length-prefixed framing + a `MAX_CONTROL_FRAME` DoS guard. Generated code is never committed
  or hand-edited.

---

## 4. Strategy decisions already made (do not re-litigate without an ADR)

| # | Decision | Rationale |
|---|----------|-----------|
| S1 | **App-first, extract SDKs later.** Build two working reference apps (host + controller) that share Rust crates *directly*, then draw the SDK boundary around the proven crates and add C ABI / N-API. | You cannot validate an SDK surface without a real consumer. SDK-first produces the wrong ABI. |
| S2 | **Controller = Tauri v2** (Rust core + React/TS webview) — **native first**. A **browser/webapp controller over WebRTC** (public STUN → self-hosted TURN) is a *deferred* SDK/embedding track, not the MVP (ADR-057). | Core is already Rust; Tauri reuses the crates in-process with no ABI, fastest iteration. The webapp track reuses the transport-agnostic core behind the DI seams; WebRTC is the only browser transport that keeps P2P. |
| S3 | **Video render path = WebCodecs → canvas/WebGL** in the webview for the MVP. Rust pushes encoded H.264 chunks to JS via Tauri v2's binary `Channel`; `VideoDecoder` decodes; render to canvas. Native-surface fallback reserved for when latency won't close (notably macOS/WKWebView). | Single clean data path, fastest to a working demo. |
| S4 | **Collapse the host process model for the MVP** into one user-space process (capture + encode + Iroh + consent + input). Re-separate into system service + session agent + privileged input helper as a dedicated hardening phase, once the end-to-end system works. | The 3-process split is production security hardening, not functionality. Separating later is mechanical, and we'll know the real boundary messages. **This is a temporary MVP posture — the security story is not complete until it is separated.** |
| S5 | **macOS is the development-lead host platform; Windows remains the production target** (Linux last). | Team is on Mac+Linux — lead on what's testable (ScreenCaptureKit/VideoToolbox/CGEvent); Windows is a port when hardware/CI is available. Architecture is platform-abstracted so this is a scheduling choice (ADR-054, amends ADR-010). |
| S6 | **Rust shared core**, protobuf wire protocol for high-frequency channels, CBOR only for portable tickets. | Cross-platform, performant, versionable. |
| S7 | **No arbitrary shell / no generic filesystem browsing.** Support actions are a signed catalogue with strict argument schemas. | Attack-surface reduction; enterprise/regulated buyers. |
| S8 | **Fraud/harm-prevention is a first-class, on-device, privacy-safe subsystem** — friction + containment against coached-victim scams, strong prevention against remote attackers, honest about its limits. | Differentiator for regulated verticals; over-claiming is a liability. |
| S9 | **Licensing: Apache-2.0 for the whole repo; reject AGPL/SSPL** (MPL-2.0 is the only alternative under consideration). *Add full LICENSE + codec-patent counsel sign-off before opening the repo.* | Permissive embedding is the point of an SDK; Apache adds a patent grant. Trade-off: no license-based moat — differentiation is execution/brand/hosted, not the license. |

See `docs/14_DECISIONS_ADR.md` for the full ADR log and the reasoning behind each.

---

## 5. Non-Negotiable Invariants (security-critical — must never regress)

These are load-bearing for the product's security promise. A change that weakens any of them is
rejected by default, regardless of latency or UX benefit:

1. **The local user is the final owner of the machine.** A controller *requests*; it never
   self-authorizes.
2. **Every privileged behavior is an explicit, named capability.** Unknown capabilities are
   **denied**, never defaulted-on.
3. **Grants and leases are short-lived, signed, and bound** to host + controller + endpoint
   identities. Expired or endpoint-mismatched grants are rejected.
4. **Emergency stop always overrides everything** — grant, lease, policy, in-flight input — and
   takes effect within the target time (≤250 ms locally).
5. **One active OS-input controller at a time by default.** Everyone else is a *virtual* cursor
   that cannot inject input.
6. **The input helper accepts only a narrow, validated set** of normalized input commands. Never
   shell commands, executable paths, OS API names, raw network objects, or controller-supplied
   file paths.
7. **Consent is honest and unspoofable.** Active remote control is always visible; recording is
   always disclosed; the stop control is always present. White-labeling may not hide these.
8. **Secrets never touch logs or crash dumps**: private keys, grant/token contents, clipboard
   data, typed text, file contents, screen pixels.
9. **Transport encryption is necessary but not sufficient** — authorization is enforced by the
   host, not by the transport layer. Iroh gives us a secure pipe, not permission.
10. **Audit is append-only and hash-chained** per session and signed by the host identity.
    Security-sensitive events are recorded.

**Fraud & harm-prevention invariants** (see `docs/15`, `docs/16`):

11. **The fraud-protection subsystem is a pure on-device `content → verdict` function.** Content
    (URLs, titles, field labels, clipboard/key values, pixels) never crosses a process or network
    boundary — only content-free verdict enums do. A `content` field is forbidden in verdict/console
    payloads **at compile time**. No per-URL cloud lookups (the lookup *is* the exfiltration).
12. **The fraud analyzer is inert unless a host-authorized remote session grant is live.** Zero
    content at rest: no screenshots, no keystroke logs, no session recording in the fraud subsystem.
13. **Every enforcement action is a pause with a one-action local-user recovery.** Resume authority
    belongs only to the local user on a controller-blind channel; the controller can never resume.
14. **Never build a secure-desktop/UAC input-injection bypass; never request UIAccess.** The
    emergency stop rides the kernel-owned SAS (Ctrl+Alt+Del) path and overrides any active grant.
15. **Enforce capability scope per message, host-side** — never trust the controller's claimed
    scope (RustDesk CVE-2026-57850 class). Capabilities are fine-grained and never paywalled in the
    core.
16. **A deployment may advertise assurance Tier ≥1 only if TPM-backed key storage is attested**;
    software-fallback installs are capped at Tier 0. No phishable factor recovers a
    phishing-resistant one.
17. **Public protection claims must distinguish prevent (remote-attacker) vs deter (coached victim)
    vs cannot-stop.** Never claim to "prevent scams," "detect credential capture," or offer
    "tamper-resistant" or machine-level protection (`docs/15 §6`).
18. **No GPL/LGPL/AGPL/SSPL in the linked dependency graph** (MIT/Apache-2.0/BSD/ISC/Zlib/**MPL-2.0**
    are fine; `cargo-deny` fails the build on denied licenses). The project itself is **Apache-2.0**.
    RustDesk (AGPL) is study-only, never linked or vendored; pull `scrap`/capture/codec crates from
    permissive upstreams, never the RustDesk fork.

If you're unsure whether something touches an invariant, assume it does and flag it.

---

## 6. Target tech stack

**Native core / host:** Rust, Tokio, **Iroh 1.x (pin exact, no `unstable-*`)**, Prost/Protobuf,
`tracing`, SQLite (rusqlite/SQLx), **libsodium Ed25519**, grant format **Biscuit** (or PASETO
v4.public), platform crates (`windows-rs`). Capture **DXGI Desktop Duplication** (`scrap`/upstream);
input **`enigo`** (upstream MIT) / raw `windows-rs`; encode Media Foundation → NVENC/AMF/oneVPL,
software fallback **OpenH264 (`libloading`) — never x264/GPL**. FEC via `nanors`. C ABI (`cbindgen`)
+ N-API — *deferred to the SDK phase*.

**Controller:** **Tauri v2 (pin ≥ 2.11.1** — Origin-Confusion CVE), React + TypeScript UI,
WebCodecs `VideoDecoder`, canvas/WebGL rendering; deny-by-default capabilities + Isolation + strict
CSP; remote feed to canvas only.

**Supply chain:** `cargo-deny` license gate (**deny GPL/LGPL/AGPL/SSPL as build-breaking**);
`cargo-about`/`cargo-bundle-licenses` → `THIRD-PARTY-NOTICES`; CycloneDX SBOM per release; EV
code-signing with keys in HSM/TPM off build machines.

**Host consent UI (MVP):** small Tauri v2 window (React) so both apps share one UI stack.

**Backend:** none for the MVP. A future control plane (issuer/audit/relay directory) is
explicitly out of scope until Phase 9.

Exact crate choices and versions are pinned in `docs/09`–`docs/12` once research lands. Do not
introduce a new significant dependency without noting it in the relevant doc and, if it touches a
security boundary, an ADR.

---

## 7. Target repository structure

Not yet created. When execution starts, follow this layout (adapted from `docs/02_ARCHITECTURE.md`):

```text
casual-ras/
  crates/                 # shared Rust core (the future SDK internals)
    ras-core/             # session orchestration, state machines
    ras-protocol/         # protobuf messages, framing, versioning
    ras-identity/         # Ed25519 identities, key storage
    ras-grant/            # access requests, session grants, issuer trait
    ras-policy/           # capability intersection, local policy
    ras-control/          # control leases, generations, input routing
    ras-media/            # capture/encode/decode traits + pipeline
    ras-media-macos/      # macOS backend: ScreenCaptureKit + VideoToolbox (FFI; unsafe confined here)
    ras-audit/            # hash-chained signed audit journal
    ras-transport-iroh/   # Iroh endpoint, ALPN routing, relay
    ras-host/             # headless host CLI (no-GUI share)
    ras-ffi/              # C ABI (SDK phase only)
  app/                    # unified Tauri v2 desktop app — both roles in one binary (ADR-062)
    src-tauri/            #   connect_to_host/disconnect + start_sharing/stop_sharing/respond_consent
    ui/                   #   home (share/connect) + WebCodecs viewer + pointer overlay + consent
  proto/                  # .proto sources (source of truth for the wire)
  docs/                   # architecture + design docs
  examples/               # integration samples (later)
```

---

## 8. Where to find things (doc map)

| Doc | Contents |
|-----|----------|
| `README.md` | Public overview, vision, quick architecture, doc index |
| `CLAUDE.md` | **This file** — operating rules, invariants, decisions |
| `CONTRIBUTING.md` | Workflow, standards, review & testing gates |
| `SKILLS.md` | Engineering skill map + reusable playbooks |
| `docs/01_PRD.md` | Product requirements |
| `docs/02_ARCHITECTURE.md` | Components, boundaries, process model |
| `docs/03_HLD.md` | Runtime flows and state machines |
| `docs/04_PROTOCOL_AND_TOKEN_SPEC.md` | Wire protocol, grants, leases, capabilities |
| `docs/05_SDK_SPECIFICATION.md` | Host/controller/React SDK surfaces (later phase) |
| `docs/06_SECURITY_AND_THREAT_MODEL.md` | Assets, actors, threats, mitigations |
| `docs/07_IMPLEMENTATION_PHASES.md` | Delivery phases and exit criteria |
| `docs/08_TEST_AND_RELEASE_PLAN.md` | Verification, performance, release strategy |
| `docs/09_TRANSPORT_IROH.md` | Iroh/QUIC deep-dive + caveats |
| `docs/10_MEDIA_PIPELINE.md` | Capture → encode → transport → decode → render |
| `docs/11_HOST_PLATFORM_WINDOWS.md` | Windows host internals & OS isolation |
| `docs/12_CONTROLLER_TAURI.md` | Controller architecture & video path |
| `docs/13_RISK_REGISTER_AND_CAVEATS.md` | Consolidated risks with severity + mitigation + validation |
| `docs/14_DECISIONS_ADR.md` | Architecture Decision Records (incl. licensing) |
| `docs/15_FRAUD_AND_HARM_PREVENTION.md` | Anti-scam / harmful-action-blocking design + honest limits |
| `docs/16_ACCESS_AND_ENROLLMENT_MODEL.md` | Per-device keys + authenticator security tiers |
| `docs/17_ROADMAP_AND_MILESTONES.md` | Milestones + phase-wise task plan with per-phase design gates |
| `docs/18_HOST_PLATFORM_MACOS.md` | macOS host deep-dive — dev-lead platform (ADR-054/055) |
| `docs/19_CROSS_PLATFORM_HOST_RESEARCH.md` | Linux/Windows capture·input·encode·build survey + permissive recommended stack (Inv 18 license verdicts) |
| `docs/design/phase-<n>-design.md` | Per-phase design notes (written at each phase's design gate) |

---

## 9. How to work in this repo (for AI agents especially)

- **Design before code.** We are in the design phase. Produce/adjust docs; do not write
  implementation code until the user approves execution.
- **Match the surrounding code and docs** in tone, structure, and naming.
- **Keep the wire protocol in `proto/` as the source of truth.** Never hand-edit generated code.
- **Any decision that affects a security boundary, the wire protocol, or the priority ordering
  requires an ADR** in `docs/14`.
- **Never introduce code that logs a secret** (see Invariant 8) — this includes debug/trace lines.
- **Prefer explicit, typed errors** with stable machine-readable codes (see the error model in
  `docs/04`). No silent failures on a security path.
- **Flag, don't guess.** If a fact about Iroh/Windows/WebCodecs/crypto is uncertain, mark it and
  ask for validation rather than asserting it.
- **Cost/scope awareness:** this is a large multi-year system. Keep the MVP surface ruthlessly
  small (Windows host + Tauri controller, view-only then single control lease). Resist scope creep
  into P1+ features.

---

## 10. Definition of done (for any change, once we're building)

- Meets the Non-Negotiable Invariants.
- Has tests appropriate to its layer (unit / property / fuzz / integration — see `CONTRIBUTING.md`).
- Security-sensitive changes have a second reviewer and, where relevant, an updated threat model.
- Docs updated (including this file's status section and any affected ADR).
- No secret-leaking logs; no new unauthenticated local endpoints; no new capability that isn't in
  the registry in `docs/04`.
