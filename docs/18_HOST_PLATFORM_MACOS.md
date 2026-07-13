# 18 — Host Platform Deep-Dive: macOS (development lead)

> macOS is the **development-lead host platform** (ADR-054): it's the team's testable hardware and
> gets us a working end-to-end demo fastest. Windows (`docs/11`) remains the production target,
> ported later behind the same `ScreenCaptureBackend`/`InputBackend` traits. Grounded July 2026.
> `[verify]` = confirm on your macOS version / Apple Silicon. Priorities: **security → latency → UX**.

## 0. The five findings that change the design

1. **Input permission is `PostEvent`, not Accessibility.** Injecting events is gated by TCC
   **`kTCCServicePostEvent`** — a *different* bucket from Accessibility. Gate on
   **`CGPreflightPostEventAccess()`** (and `CGRequestPostEventAccess()`), **not**
   `AXIsProcessTrusted()`. Without the grant, **`CGEventPost` fails silently** — events just vanish,
   no error. `[verify: Apple DTS forums #730441 + CoreGraphics docs]`
2. **Secure input is our fraud-model boundary on macOS — and it's real.** When a password/login/lock
   field holds **secure keyboard entry**, synthetic keystrokes from another process are **dropped** —
   a remote host cannot type credentials into it. **But a process running as root can bypass secure
   input** → therefore keep input injection in the **unprivileged per-user agent, never the root
   daemon** (ADR-055). This both reinforces `docs/15` and dictates the process split direction.
3. **The process split is mandatory on macOS.** A root **LaunchDaemon has no WindowServer
   connection → it cannot capture or inject.** You need a per-user **LaunchAgent** (or a helper
   bootstrapped into `gui/<uid>`) that holds the TCC grants *in the GUI session*. This is the root
   cause of RustDesk's recurring "black screen" bug: the capturing process must be the grant-holder
   in the GUI session.
4. **Secure Enclave is P-256 only** — an **Ed25519 device identity cannot live in the SE.** Either
   make the hardware-backed device key **P-256 (ECDSA/ECDH)**, or accept a **software Ed25519** key
   in the Keychain. (Consistent with `docs/06 §6`, `docs/16 §1`.) `[verify: Trail of Bits + Apple
   CryptoKit]`
5. **One encoder config fixes two problems.** VideoToolbox **low-latency mode**
   (`EnableLowLatencyRateControl`, **no B-frames / IPPP**) is both the latency win on the host **and**
   the fix for the **WKWebView ~3-second H.264 decode bug** on the controller (that bug is
   B-frame/reorder-window driven). Our "B-frames off" rule (`docs/10`, ADR-031) is now doubly
   load-bearing on Apple platforms.

## 1. Screen capture — ScreenCaptureKit (only supported path)

- **Use ScreenCaptureKit (`SCStream` + `SCContentFilter` + `SCStreamConfiguration`).** It is the
  **only supported capture path in 2026** — `CGDisplayStream` is unavailable in macOS 15+
  `[verify: secondary sources FreeRDP/Chromium]`. Frames arrive as `CMSampleBuffer` wrapping an
  **IOSurface-backed `CVPixelBuffer`** → GPU, zero-copy into VideoToolbox.
- **Delta encoding:** `SCStreamFrame` exposes **`.dirtyRects`** (+ contentScale, scaleFactor) — the
  macOS analog of DXGI dirty rects for bandwidth reduction.
- **No separate-cursor API.** Unlike Windows DXGI (which hands you cursor shape/position metadata),
  ScreenCaptureKit has **no separate-cursor path**: either composite the cursor into the frame
  (`showsCursor = true`) or source position yourself. This changes our "cursor out-of-band" plan
  (`docs/10 §2`) on macOS — decide per-platform. `[verify]`
- **Config for low latency:** target FPS via `minimumFrameInterval`, `pixelFormat` BGRA or NV12,
  `queueDepth` small. HDR and protected-content behavior `[verify]`.

## 2. Encode — VideoToolbox (Apple Silicon media engine)

- **`VTCompressionSession`** feeding the `CVPixelBuffer`/IOSurface directly (zero-copy). Apple-Silicon
  gives hardware H.264 for free.
- **Low-latency config (this is the important part):**
  - `kVTVideoEncoderSpecification_EnableLowLatencyRateControl = true`
  - `kVTCompressionPropertyKey_AllowFrameReordering = false` (**no B-frames — IPPP**)
  - `kVTCompressionPropertyKey_ProfileLevel = H264_Main_AutoLevel`, `RealTime = true`
  - `AverageBitRate` (CBR-ish), large `MaxKeyFrameInterval` + **force IDR on demand** via the
    per-frame `kVTEncodeFrameOptionKey_ForceKeyFrame`.
  - Emit **Annex-B** (SPS/PPS in-band) — matches `docs/10` / decoder needs no `description`.
- **Coupling to the controller:** because the WKWebView ~3s decode bug is reorder-driven, the
  **no-B-frames rule is mandatory**, not just a latency preference, whenever the controller may run
  on macOS/WKWebView.

## 3. Input injection — CGEvent

- **`CGEventCreateMouseEvent` / `CGEventCreateKeyboardEvent` + `CGEventPost`** (tap location
  `kCGHIDEventTap` or `kCGSessionEventTap`).
- **Permission (see finding #1):** preflight **`CGPreflightPostEventAccess()`**; request via
  `CGRequestPostEventAccess()`. **`AXIsProcessTrusted()` is NOT the right gate for injection** —
  that's Accessibility, a different bucket. Silent failure if ungranted.
- **Coordinates:** global display coordinates in **points**; map normalized 0..1 → points accounting
  for **Retina backing scale** and multi-display arrangement (displays can have negative origins).
- **Text vs keycodes:** `CGEventKeyboardSetUnicodeString` for arbitrary Unicode text (layout-
  independent); virtual keycodes for shortcuts/modifiers. Track pressed keys; release on
  transfer/disconnect.
- **Secure input (finding #2):** synthetic keys are dropped while secure keyboard entry is active
  (password fields) — surface this to the fraud engine as the macOS analog of the Windows
  `IsPassword` context (`docs/15 §2.1`). Keep injection **unprivileged** so we *don't* accidentally
  gain the root bypass.
- `[verify]` macOS 26 "Tahoe" reports of `CGXSenderCanSynthesizeEvents` dropping synthetic modifiers
  come from a reverse-engineering blog, **not Apple** — do not freeze design on it; test on target.

## 4. Security & permission model (the macOS analog of Windows isolation)

- **Two TCC permissions, both user-granted prompts, both in the GUI session:** **Screen Recording**
  (capture) and **PostEvent** (injection). The **process holding the grant must be the one
  capturing/injecting** (finding #3).
- **Sequoia re-prompts Screen Recording ~monthly** for ordinary apps → pursue Apple's
  **`com.apple.developer.persistent-content-capture`** entitlement (built for VNC/remote-desktop
  apps) to avoid the monthly nag. `[verify: entitlement availability/approval]`
- **Secure input mode** blocks synthetic keystrokes into secure fields (finding #2) — a *feature* for
  our threat model; do not try to defeat it.
- **Lock screen / loginwindow / FileVault pre-boot:** a background daemon cannot reach these; whether
  locked-session content is silently capturable is **undocumented** `[verify]`. Treat lock/login like
  the Windows secure desktop: capture/injection unavailable, session continues (ADR-047 analog).

## 5. Process model & packaging

- **LaunchAgent (per-user, GUI session)** holds capture + injection + their TCC grants — this is the
  privileged-for-input component but runs **unprivileged (user), by design** (finding #2/#3). A root
  **LaunchDaemon**, if used at all (e.g., for identity/audit/update), **cannot capture or inject** and
  must delegate to the agent over XPC/Unix socket.
- **This maps cleanly onto our target split** (`docs/11 §1`): the macOS "input helper" equivalent is
  the per-user agent; keep it unprivileged. The MVP single-process posture (ADR-023) = one per-user
  agent process.
- **Packaging:** hardened runtime + **code signing + notarization** (Developer ID); required TCC
  usage-description entitlements; distribute as a signed/notarized **PKG**. `apple-codesign`
  (permissive Rust) can sign/notarize in CI.
- **Key storage:** Keychain; hardware-backed device key via Secure Enclave is **P-256 only**
  (finding #4) — decide P-256-hardware vs software-Ed25519.

## 6. Rust crate stack (all permissive — ADR-051 clean)
`screencapturekit` (capture) · `objc2` + `objc2-video-toolbox` / `core-media` / `core-video`
(VideoToolbox + frame types) · `enigo` and/or `core-graphics` (CGEvent input) · `security-framework`
(Keychain) · `apple-codesign` (CI signing/notarization). `[verify licenses per version]`

## 7. Caveats summary
- Injection gate is **PostEvent**, not Accessibility; `CGEventPost` fails **silently** if ungranted.
- **No separate-cursor API** in ScreenCaptureKit — rethink out-of-band cursor on macOS.
- Root **LaunchDaemon can't capture/inject** (no WindowServer) — must be a per-user agent.
- Keep injection **unprivileged** (root bypasses secure input — don't want that power).
- **No-B-frames is mandatory** (latency + WKWebView decode bug), not optional.
- **Secure Enclave = P-256 only** — Ed25519 can't be SE-backed.
- Sequoia re-prompts Screen Recording monthly without the persistent-content-capture entitlement.
- `CGDisplayStream` gone in macOS 15 — ScreenCaptureKit only.

## 8. Decisions & open validation
- **ADR-055:** input injection lives in the **unprivileged per-user agent** (never root) — secure
  input must remain effective against us. **ADR-047 analog:** no attempt to capture/inject on
  lock/login screen. **ADR-031 reinforced:** B-frames off is mandatory on Apple platforms.
- **Spike (Phase S) must validate on the Mac:** ScreenCaptureKit→VideoToolbox capture→encode
  latency; WebCodecs decode in **Safari/WKWebView** with no-B-frame Annex-B (does the ~3s bug
  vanish?); PostEvent permission flow; multi-display coordinate mapping.
- **Open:** persistent-content-capture entitlement approval; locked-session capture behavior;
  cursor-out-of-band approach without a separate-cursor API; P-256 vs software-Ed25519 device key.

## 9. Sources
Apple: ScreenCaptureKit (SCStream/SCContentFilter/SCStreamConfiguration), VideoToolbox
(VTCompressionSession, EnableLowLatencyRateControl, AllowFrameReordering, ForceKeyFrame), Core
Graphics (CGEventPost, CGPreflightPostEventAccess/CGRequestPostEventAccess), TCC / persistent-
content-capture entitlement, CryptoKit / Secure Enclave (P-256) · Apple DTS forums #730441 · Trail of
Bits (SE) · WebKit/WebCodecs decode-latency reports · Rust crates: screencapturekit, objc2 family,
enigo, core-graphics, security-framework, apple-codesign. (Full cited write-up:
`scratchpad`/research artifact; some Apple key names cross-checked against SDK-header mirrors as the
live docs are JS-rendered. Uncertainty flags inline as `[verify]`.)
