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
    (proto oneof 8–11, fail-closed codec + fuzz); the closed action set later gained
    `InputAction::PointerMoveRelative { dx, dy }` (**ADR-087**, §3.6 mobile) — a bounded `i16` pixel
    delta, display-independent, gated on the **same `pointer.move`** cap at the per-message gate (a
    `pointer.move`-less lease denies it — tested); `OsInputSink::pointer_move_relative` defaults no-op —
    the **macOS CGEvent override landed** (reads live cursor pos → adds delta → clamps to the desktop
    union so it never goes off-screen → posts `MouseMoved` + `kCGMouseEventDeltaX/Y` for games;
    compile/clippy-clean on macOS, union math unit-tested, live injection on-device). The **Linux XTEST
    override landed too** (`QueryPointer` → add delta → clamp to desktop union → absolute `MotionNotify`;
    cross-compile/clippy-clean for linux, union math unit-tested, live run on-device). The **Windows
    `SendInput` override landed too** (`MOUSEEVENTF_MOVE` without `ABSOLUTE` = native relative motion,
    Windows clamps to the virtual desktop itself — a one-line send; cross-compile/clippy-clean for
    windows-msvc, needs Windows hardware to run). **All three OS backends now inject relative motion.**
    The client touch-gesture translator remains;
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
  tree passes `cargo-deny` (Inv 18). A **Windows input backend has also landed** (`ras-input-windows`,
  **ADR-071**): a third `OsInputSink` over **`SendInput`** via `windows-rs`, in-session with **no
  UIAccess** (Inv 14 — cannot drive elevated windows/secure desktop, by design and now OS-enforced by
  Microsoft's Jan-2026 credential-UI hardening), absolute virtual-desktop coords, held-modifier
  reconciliation, `KEYEVENTF_UNICODE` text, tracked-key best-effort `release_all`. `unsafe` confined to
  this FFI crate; **cross-compile-checked + clippy-clean for `x86_64-pc-windows-msvc` from the macOS dev
  machine**, unit-tested (HID→VK table, abs-axis mapping), `cargo-deny`-clean (windows-rs MIT/Apache).
  **Both** the Linux and Windows sinks are **wired into the app's Share role** (`with_input_sink` +
  `set_display_bounds` under the matching `cfg`), so all three platforms inject once built. **Keyboard
  correctness — lock-state sync has landed (ADR-074):** a closed `InputAction::SetLockState { caps_lock,
  num_lock }` carries authoritative lock **state** (not key edges — edge-forwarding guarantees Caps/Num
  drift); the host **slaves** its OS lock keys to it, gated on `keyboard.key` through the same
  per-message `authorize_input` (a pointer-only lease can't flip Caps — tested, Inv 15). Each backend
  reads live OS lock state and taps **only on mismatch** (idempotent): Windows `GetKeyState`+`SendInput`,
  Linux `QueryPointer` mask + XTEST, macOS `CGEventSourceFlagsState` + CapsLock keycode (no NumLock,
  best-effort). `OsInputSink::set_lock_state` has a default no-op (non-breaking); wire+gate+dispatch+all
  three overrides are cross-compile-checked green. **`keyboard.text` (Unicode/IME) hardened (ADR-083):**
  the withheld capability is now **safe to grant** without changing its deny-by-default posture —
  `InputAction::TextInput.utf8` is a `Redacted` (was plain `String`) so typed plaintext (passwords/PII)
  can't leak through a `Debug`/log at any layer (`.reveal()` only at the OS-injection boundary); the
  decoder **rejects control characters** (`char::is_control` — C0/C1+DEL, so no terminal-escape/NUL/
  newline smuggling; composed CJK/emoji/ZWJ/accents pass); it requires its **own lease bit** (a
  `keyboard.key`-only lease denies `TextInput` at the per-message gate — tested, Inv 15) and stays out
  of the default grantable policy. Length bounded `MAX_TEXT_INPUT=256`. Deferred: a rate bound (needs a
  clock in the pure gate) + app IME wiring. The **cursor-shape channel** landed
  (ADR-073: `CursorShape`/`CursorCached`/`CursorHidden` `ControlMsg` variants, fail-closed codec bounded
  at `MAX_CURSOR_DIM=256` + exact `w*h*4` RGBA + hot-spot-inside) — **now with the `ras-core` plumbing
  too**: a host-side `CursorObserver` seam (`with_cursor_observer`) → a host cursor task that **dedups
  host-side** (repeat id → `CursorCached`, else fresh `CursorShape`, id recorded only after a successful
  enqueue so a dropped shape re-sends), re-validates the receiver's bounds, and forwards over the
  reliable control channel via a bounded drop-newest queue (advisory — never backpressures control); and
  a controller-side `CursorSink` seam (`attach_cursor_sink`, `set_shape`/`set_cached`/`hide`). Cursor
  pixels are **display data** on their own sink, **not** the content-free lifecycle events (and
  `CursorShape::Debug` elides the RGBA — Inv 8 hygiene). Loopback-tested (repeat id → cache reference).
  OS cursor **capture** (behind the observer seam) + controller **render** are the on-device follow-up.
  The **clipboard-text security spine**
  landed too (ADR-076): `ControlMsg::ClipboardText` with the payload in a `Redacted` newtype (Inv 8 —
  `Debug` prints only a byte count, so a copied password can't leak to a log), bounded by
  `MAX_CLIPBOARD_BYTES=768 KiB` (refused, never truncated), + the pure host-side per-direction gate
  `ras_policy::clipboard_push_allowed` (controller→host = `clipboard.write`, host→controller =
  `clipboard.read`, both recognized-but-withheld → **default OFF**), with the **no-auto-paste** rule
  (receiver only sets the OS clipboard, never injects a paste) documented on the wire type. The **host
  loop now enforces it end-to-end**: a `ras_control::ClipboardSink` DI seam + `with_clipboard_sink`, the
  loop captures granted caps at auth → gates → (if allowed + backend wired) `set_text` without paste,
  emitting content-free `ClipboardApplied{len}`/`ClipboardRejected{code}`; `send_clipboard_text` is the
  controller push API; two loopback tests (granted reaches sink, withheld refused). The **OS backend
  landed too (ADR-079)**: `ras-clipboard::ArboardClipboardSink` over `arboard` (NSPasteboard/Win32/X11),
  sets-never-pastes, fail-closed, `default-features=false` (text-only, X11-only Linux), BSL-1.0 scoped
  in cargo-deny; wired into Share via `with_clipboard_sink` but **inert until `clipboard.write` is
  granted** (default OFF). **Both clipboard directions now wired (ADR-076):** `HostSession::send_clipboard_text`
  gates the host's push on `clipboard.read` and forwards it over the outbound channel; the controller
  applies it via an attached `ClipboardSink` (`attach_clipboard_sink`, set-never-paste) with the same
  content-free `ClipboardApplied`/`Rejected` outcome. Loopback-tested both ways (granted → controller
  sink receives; withheld → nothing crosses the wire, Inv 15). Enabling the grant + app "Send
  clipboard"/indicator is the follow-up. **In-session chat landed (ADR-082):** `ControlMsg::ChatMessage` with the payload in a `Redacted` newtype
  **end-to-end** (wire + codec + the `LifecycleEvent::ChatMessage` that surfaces it), so chat text — a
  secret in the Inv-8 sense — can never leak to a log/trace; `.reveal()`d only at display. **No
  capability** (base session comms — touches no OS/input/screen surface; a live session already required
  consent — so gating would be security-theater), bounded by `MAX_CHAT_BYTES=4 KiB` (refused, never
  truncated), fail-closed codec + fuzz. **Bidirectional** (`HostSession::send_chat` +
  `ControllerSession::send_chat`; a received message is always from the remote peer, surfaced on each
  side's own lifecycle stream); the host send reuses a **generalized outbound-control channel** the
  cursor task now shares. Loopback-tested both directions; the app chat panel is the on-device follow-up. **Still pending:** the **on-device** GUI run of the real CGEvent injection + PostEvent-TCC prompt +
  Secure-Input drop (macOS); the analogous **Linux on-device** XTEST run (a real X11/Xwayland session);
  the **Windows on-device** `SendInput` run (**needs Windows hardware the team lacks** — stays
  CI-compile-gated on `windows-latest`); a macOS **global-hotkey** emergency stop (baseline stop is the
  always-visible Stop button, which already drives `revoke_all` + `release_all`; no kernel SAS on macOS
  — SAS stays the Windows path); and the Linux **`uinput`/libei** + Windows **Session-0 service/agent
  split (S4)** follow-ups (docs/19 §3/§4). **Controller keyboard app-wiring has landed (ADR-075):**
  the Connect handler forwards the controller's own `getModifierState('CapsLock'/'NumLock')` as
  `SetLockState` on change (state-only — raw lock-key edges are no longer forwarded, so they can't race
  the sync), plus a **Cmd↔Ctrl primary-modifier remap** — a default-OFF, visible "⌘→Ctrl" toggle that
  swaps the Control↔GUI HID usages + Ctrl↔Cmd modifier bits for outgoing input only (controller-side
  policy, no wire/host change, still gated identically — Inv 15). Cross-device follow-ups still open:
  **live on-device lock reconciliation** + the ⌘↔Ctrl on-device check (⌘C→Ctrl+C on a real non-Mac
  host); host cursor **capture** + controller **render** for the cursor-shape channel.
- **Persistent paired-controller registry — pure model landed (ADR-084, §3.5 → unlocks unattended
  access §3.4).** `ras-identity` gains a `PairingRegistry` (`pair`/`is_paired`/`get`/`list`/`touch`/
  `revoke`, in-memory MVP impl) over `PairedController` records (id + user label +
  `first_paired_at`/`last_seen_at`; re-pair preserves the pairing age, revoke = kill-switch). The
  load-bearing rule is **structural**: the pairing decision is a bare 2-variant enum
  (`SkipPairingPrompt`/`RequirePairingPrompt`) with **no capabilities**, so a registry hit governs only
  the *human prompt* — a known controller still mints a fresh grant + per-message enforcement +
  emergency stop (Inv 3/9). Key-change detection is free (keys on `ControllerId` = the pubkey).
  `pairing_code(id)` renders the pubkey as grouped **Crockford-base32** (omits `I L O U`) for the QR-side
  human check. Pure (no clock; caller passes timestamps). Follow-up: **SQLite** durable store, wiring the
  decision into the app connect/consent flow + host-displayed QR, then unattended access.
- **Unattended access — decision model landed (ADR-085, §3.4).** `ras-grant` (the authorization heart)
  gains the pure `unattended_decision(is_paired, tier, authorization, now) → Proceed |
  RequireAttendedConsent(reason)` + the `UnattendedAuthorization` record (controller id + capability
  **ceiling** + expiry). **Structural rule:** a `Proceed` never issues anything — it only skips the *live
  prompt*; issuance stays the `SessionGrantIssuer`'s `requested ∩ policy ∩ ceiling` (policy can only
  narrow), so every connect still mints a fresh, endpoint-bound, per-message-enforced, emergency-stoppable
  `SessionGrant` (Inv 3/4/15). **Fail-closed + ordered:** Tier-16 cap first (software-only Tier 0 can
  *never* do unattended — tested) → paired (Inv 1, de-listing kills it) → authorization exists → not
  expired (Inv 3). All facts host-side, never the controller's claim. Follow-up: a signed/portable
  authorization (PASETO), connect/consent-flow wiring + grant/revoke UI, auto-renew loop.
- **File transfer — signed-catalogue model landed (ADR-086, §3.3, the "danger channel").** We **reject**
  browse-anywhere (Inv 6/S7) and build only the signed catalogue: `ras_policy::file` has
  `DropCatalogue`/`DropTarget` (host-chosen sandbox dir + size cap + optional extension allow-list), a
  `FilePushRequest` carrying **only** target-name + leaf-filename + size (**never a path** — the host
  resolves the destination), and the fail-closed ordered `authorize_file_push` → a host-resolved child
  path. `validate_filename` **structurally defends all three RustDesk CVE classes**: traversal/zip-slip
  (rejects separators/`:`/`..`/control-chars/reserved-Windows-names → provably a direct-child leaf, a
  **property test** asserts `dir.join(name).parent()==dir` over arbitrary input); capability-bleed
  (per-target `file.push.<name>` is its own namespace; checks *only* that cap, never input/capture —
  CVE-2026-58056/Inv 15); symlink-follow (the string is a safe leaf — the precondition for the deferred
  `O_NOFOLLOW` write). Per-target caps deny-by-default. Pure (no I/O, no new crate/dep). **Authorization
  wire landed too (ADR-089):** `ControlMsg::FileOffer{target,filename,size}` (leaf name, never a path) →
  `host_handle_file_offer` runs `authorize_file_push` + a **per-transfer `FileConsent`** prompt (default
  `DenyAllFileConsent`, Inv 1) → `FileAccept`/`FileReject{code}`, audited (`FilePushAccepted`/`Rejected`)
  + surfaced as `FileTransfer{Accepted,Rejected}` lifecycle events both sides. Loopback-tested over all
  five paths (accept, consent-denied, capability-withheld, traversal-filename, unknown-target). **Byte
  streaming landed too (ADR-090):** `ControlMsg::FileChunk`/`FileComplete` (chunk bounded
  `MAX_FILE_CHUNK`) → the host writes each chunk to the resolved dest via an injected `FileWriteSink`
  (`with_file_write_sink`), tracking the total; an **over-run** past the offered size (or a short
  transfer) **aborts** — no oversized/partial file — and `FileComplete` finalizes iff `received == size`.
  Loopback-tested (offer→accept→chunks→complete lands the bytes intact; over-run aborts). **The Unix
  write backend landed too:** `ras-files::SafeFileWriter` opens with `O_NOFOLLOW | O_CREAT | O_EXCL` (mode
  0600) — a symlink dest refused (`O_NOFOLLOW`), an existing entry refused (`O_EXCL`), abort removes the
  partial; pure `std`+`libc` (no ras-core dep — the app wraps it), `unsafe`-free, and **genuinely
  unit-tested off-device** with real tempfiles (write+read-back, existing-refused, a real symlink refused
  with the target untouched). The **Windows backend landed too** (`CreateFileW`+`CREATE_NEW` — atomic
  `O_EXCL`, refuses any existing entry incl. a symlink/junction; `unsafe` confined to that FFI path, raw
  `HANDLE` stored as `isize` to stay `Send+Sync` — a compile-time assertion enforces it;
  cross-compile/clippy-clean for windows-msvc, needs Windows hardware to run). **File transfer is now
  complete on all three platforms at the code level.** Only follow-up: the confirmation UI.
- **Audit journal — Inv 10 implemented (ADR-088).** `ras-audit` (was a stub) is now a per-session
  **SHA-256 hash chain** of **content-free** `AuditEvent`s (enum tags + counters only — never a pixel,
  keystroke, clipboard byte, typed text, path, or secret; a `content` field is absent by construction,
  Inv 8/11), made unforgeable by a **host-signed `Checkpoint`** over the chain head (the `ras-identity`
  `KeyStore` seam). The chain gives tamper-**evidence** (altering/reordering/removing a middle entry
  breaks `verify()`); the signature gives **authenticity** (a rewritten journal has a different head, so
  the old signed checkpoint no longer matches and no valid new one is forgeable without the host key).
  Domain-separated + session-genesis-bound (no cross-session splice); append-only (no edit/remove API).
  Pure (no clock/I/O); `sha2` (RustCrypto MIT/Apache) is the only new dep. Verified: chain links/verifies,
  determinism, content-tamper/reorder/middle-removal each caught at the right `seq`, signed-checkpoint
  round-trip + rewrite/forged-key/tampered-head rejection, empty-journal. **Host-loop wiring landed too:**
  an `AuditSink` DI seam (`with_audit_sink`) receives events **synchronously + losslessly** (unlike the
  advisory, drops-on-full `LifecycleEvent` stream — an audit that drops is worthless); `HostSession`
  records `SessionStarted` / `ControlLeaseGranted`/`Revoked` / `InputRejected` / `ClipboardApplied`/
  `Rejected` / `EmergencyStop` + `SessionEnded`, each before the equivalent lifecycle emit; the sink owns
  clock + journal + persistence so `ras-core` stays clock/I/O-free. Loopback-tested (recorded chain
  verifies). **Durable persistence landed too:** `ras_audit::AuditLog` is an **append-only, length-
  prefixed record file** — crash-safe (`load` stops at a torn trailing record, never corrupting the valid
  prefix) and restart-survivable (reload → `verify_chain` + a signed `Checkpoint` catches any rewrite;
  a same-length event swap breaks the chain — tested). No SQLite (avoids a `-sys` dep); added
  `ErrorCode::to_code`/`from_code` (stable numeric) for the compact round-trippable encoding. **Source
  points now include consent** (`ConsentGranted`/`ConsentDenied` at the authorization gate — a refused
  connection is audited too; tested). Follow-up: Merkle-batched forward-secure checkpoints, the file-push
  source points (once the transfer protocol reaches the host loop).
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
  - **`ras-transport-iroh` — control + video + audio + health planes are concrete, and the loopback→iroh
    swap is wired** (iroh `=1.0.2`, ADR-059/060/077). Real `Endpoint` (bind/id/accept/connect +
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
    **builds ship UNSIGNED (no OS code-signing / notarization) — Gatekeeper/SmartScreen warn — and
    stay that way *until a GitHub sponsor (or equivalent) funds the certificates* (ADR-072).** This is
    only the *OS-vouches-for-the-installer* layer; the **free** Tauri Ed25519 **update-integrity**
    signing is a separate layer, now **wired (ADR-078)** — so unsigned-by-OS ≠ unverified updates.
    The controller `.app`/`.dmg` bundle was built and verified locally on macOS.
  - **Signed auto-update wired (ADR-078), activated by a one-time key setup.** The Tauri updater plugin
    is registered; two Rust commands (`check_for_updates`/`install_update`) drive a **user-initiated,
    two-click** flow (check → explicit "Install & restart" — never silent, Inv 1); `updater:default`
    capability + `plugins.updater` config (GitHub-releases `latest.json` endpoint) + CI signing env
    (`TAURI_SIGNING_PRIVATE_KEY`/password secrets) are in place. Deliberately **inert until
    provisioned**: `bundle.createUpdaterArtifacts` off + committed `pubkey` empty (so keyless CI stays
    green and no throwaway key ships). Activation = generate key → paste pubkey → add secrets → flip the
    flag (**runbook: `docs/design/auto-update-runbook.md`**). App `check`/`clippy` clean; the real
    signature-verified download+install+relaunch is the on-device row.
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
    **Multi-monitor enumeration + HiDPI model landed (ADR-081):** `ras_media::MonitorDef` is the signed
    virtual-desktop display descriptor (logical rect with **negative-capable** `left/top`, backing
    `pixel_width/height`, `scale_percent` as an **integer** — no float to drift), plus two default
    `ScreenCaptureBackend` seams: `enumerate_displays()` (a **host-local** picker query — the owner picks
    what to share, Inv 1; the controller doesn't select/switch in this slice) and `captured_display()` →
    a new additive `LifecycleEvent::CaptureDisplay` carrying the active display's logical+pixel+scale so
    the controller renders crisply (the Rank-2 HiDPI gap; metadata only, never pixels). Synthetic backend
    models a two-display desktop; loopback-tested. Per-OS enumeration + the app picker UI + controller
    crisp-render use stay the on-device follow-up.
  - **Audio pipeline — capability + media seam landed (ADR-077), host→controller output audio.** A
    new `audio.listen` capability (**recognized-but-withheld → default OFF**); `ras-media::audio` defines
    the pipeline as traits + canonical types (`AudioConfig`, `CapturedAudio` interleaved-i16 PCM,
    `EncodedAudio` = one Opus packet + monotonic `seq`, **no keyframes**;
    `AudioCaptureBackend`/`AudioEncoderBackend`/`AudioDecoderBackend`), parallel to the video traits,
    with dependency-free `SyntheticAudioCapture`+`SyntheticAudioEncoder` doubles + a capture→encode
    roundtrip test. Opus is the codec (royalty-free, WebCodecs-native). MVP is **output audio only** —
    no mic, no two-way, **live-only never recorded** (Inv 12); always disclosed by an Inv-7 "AUDIO
    SHARED" indicator when active. The **real Opus codec landed too (ADR-080)**: `ras-audio-opus`
    (`OpusEncoder`/`OpusDecoder` over `audiopus`/vendored-libopus BSD-3, ISC wrapper; sub-frame
    buffering + live `set_bitrate`), verified by a real **encode→decode roundtrip** (a tone survives) —
    not the RustDesk `magnum-opus` fork; `.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM=3.5`
    (the cmake-4 fix for the vendored libopus build). The **host pump + gate landed too**:
    `HostSession::with_audio(capture, encoder)` starts an audio pump thread (mirrors the video media
    thread) **iff the grant carries `audio.listen`** (the Inv-15 host-side audio gate) **and** the
    transport carries an audio plane, re-checks stop between encode/send (Inv 4), joined on teardown.
    The **transport plane + controller ingest landed too**: the egress `AudioSink` is fetched from the
    transport (`SessionTransport::audio_sink()`, symmetric to video — the transport owns the wire, the
    host owns the *right* to be heard, gate before fetch); the mirror `audio_source()` (`AudioSourceDyn`)
    + an `AudioOutput` the controller attaches (`ControllerSession::attach_audio_output`) complete the
    path. Both transport methods **default to "unsupported"** so `IrohSessionTransport` is unchanged (the
    iroh audio sub-stream is the remaining wire follow-up); the in-memory loopback overrides both, giving
    a **true end-to-end** host→controller audio test — the controller's `AudioOutput` receives packets
    when `audio.listen` is granted, **nothing when withheld** (Inv 15). The **iroh audio plane landed
    too, over QUIC datagrams (ADR-077)**: `IrohSessionTransport` implements `audio_sink`/`audio_source`;
    each Opus packet rides one **unreliable QUIC datagram** — deliberately separate from the per-frame
    video uni-streams + control (no HOL blocking; a lost datagram is a PLC-covered glitch, and datagrams
    never touch `accept_uni`), prefixed by a fail-closed 36-byte `AudioPacketHeader` (magic `RAU1` +
    per-packet `AudioConfig` + `seq` + `captured_at_us`). **No fragmentation** — one Opus packet is one
    datagram (≈240 B at 96 kbps/20 ms, far under the datagram MTU; an oversized packet is a
    misconfiguration, dropped not reassembled). Verified by a real datagram round-trip over two loopback
    iroh endpoints **and** the full ras-core spine (host pump → real iroh datagrams → controller output).
    Deferred (OS/on-device): OS capture (SCK-audio / WASAPI-loopback / PipeWire), up-front `AudioConfig`
    negotiation (config travels per-packet today), "AUDIO SHARED" indicator, JS
    `AudioDecoder`→`AudioContext` playback.
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
    ras-identity/         # Ed25519 identities, key storage, paired-controller registry (ADR-084)
    ras-grant/            # access requests, session grants, issuer trait, unattended-access model (ADR-085)
    ras-policy/           # capability intersection, local policy, signed-catalogue file push (ADR-086)
    ras-control/          # control leases, generations, input routing + OsInputSink/ClipboardSink seams
    ras-clipboard/        # cross-platform clipboard write backend (arboard; set-never-paste, ADR-079)
    ras-files/            # safe file-write backend (O_NOFOLLOW|O_EXCL; symlink/clobber refusal, ADR-090)
    ras-media/            # capture/encode/decode traits + pipeline (video + audio, ADR-077)
    ras-audio-opus/       # Opus audio encoder/decoder (audiopus/vendored libopus, ADR-080)
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
| `docs/20_FEATURE_GAPS_AND_ROADMAP.md` | Where we lapse vs incumbents (clipboard/file/audio/multi-monitor/cursor/chat/whiteboard/auto-update/…) + safe designs + priority, from a 5-stream cross-device research sweep |
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
