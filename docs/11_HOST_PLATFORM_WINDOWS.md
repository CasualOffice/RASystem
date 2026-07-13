# 11 — Host Platform Deep-Dive: Windows

> Windows 10 22H2 & Windows 11. Grounded July 2026. `[verify]` = confirm on target hardware.
> Covers the process model, OS isolation realities, input injection, and the security-critical
> packaging facts. Priorities: **security → latency → UX**.

## 1. Process model — MVP vs target (and exactly what the MVP shortcut costs)

**Target (production) model** — three processes across a trust boundary:
- **Host service** (LocalSystem or a virtual service account) — owns identity, Iroh endpoint,
  policy, grants, audit; holds `SE_TCB_NAME` to reach user sessions.
- **Session agent** (interactive user session) — capture, encode, consent UI, overlay.
- **Input helper** (minimal privilege) — the *only* code that injects input, behind a narrow
  validated command channel.

**MVP model (decision S4 in `CLAUDE.md`)** — collapse all three into **one user-space process**
(the host Tauri app) to reach a working end-to-end system fast, then split as a hardening phase.
This is exactly why RustDesk ships a "portable" build. **The split is a re-architecture, not a
refactor — so design the IPC boundary and a "which desktop am I on" abstraction now**, even while
both live in one process, so the later split is mechanical.

**What the single-process MVP cannot do — enumerate honestly, do not over-promise:**
- **No SYSTEM service ⇒ cannot capture or inject on the secure desktop.** UAC prompts,
  Ctrl+Alt+Del, the **lock screen**, and the **login screen** all freeze/black-out. The remote user
  is locked out the moment any elevation or lock occurs. This is the biggest UX cliff.
- **Medium-integrity process ⇒ UIPI silently blocks input into elevated windows** (installers,
  admin tools) — no error is returned, so it looks like a bug.
- **Single session only** — no fast-user-switching; the process dies with the user's session.
- **No unattended/pre-login access** (nobody logged in ⇒ no user process ⇒ no host).

## 2. OS isolation realities (why the target model exists)

- **Session 0 isolation:** all services run in non-interactive Session 0 and **cannot** touch the
  interactive desktop. `UI0Detect` (Interactive Services Detection) was **removed in Win10 1803** —
  "allow service to interact with desktop" is dead. The service must launch a per-session agent via
  `WTSQueryUserToken(sessionId)` → `DuplicateTokenEx` → `CreateProcessAsUser` into `WinSta0\Default`.
  Don't rely on `WTSGetActiveConsoleSessionId` alone — enumerate sessions and pick the `WTSActive`
  one `[verify]`.
- **Secure desktop:** the system switches to `WinSta0\Winlogon` for SAS/UAC/lock/logon. Only a
  **SYSTEM** process that `SetThreadDesktop`s onto Winlogon can capture/inject there (and its
  threads must own no windows/hooks, or `SetThreadDesktop` returns `ERROR_BUSY`). Capturing the
  *login screen* is reported flaky even then `[verify]`.
- **Session-change handling:** register `WTSRegisterSessionNotification` →
  `WM_WTSSESSION_CHANGE` (console connect/disconnect, logon/logoff, lock/unlock). **Caveat: this
  does NOT fire on the Default↔Winlogon (UAC/CAD) switch** — detect that by polling
  `OpenInputDesktop` + comparing the desktop name, then reattach (SYSTEM only).
- To send Ctrl+Alt+Del into a session, a Session-0 service calls `sas.dll!SendSAS` (gated by the
  `SoftwareSASGeneration` policy).

## 3. Input injection

- **`SendInput` with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`** — normalized 0..65535 over
  the **whole virtual desktop**. Never relative motion (pointer acceleration makes it inexact).
  Batch move+button into one atomic `SendInput` array. `SendInput` returning 0 = blocked by another
  thread.
- **Normalized 0..1 → physical pixel recipe** (this is what makes mixed-DPI correct):
  1. Host process is **Per-Monitor-V2 DPI-aware** (declare in the **manifest** — MS recommends
     manifest over the programmatic API). A non-PMv2 process gets *virtualized* metrics and clicks
     drift.
  2. Controller sends per-monitor `(u,v)∈[0,1]` + monitor id; resolve that monitor's physical rect
     (`GetMonitorInfo`); `x = rect.left + u*(rect.right−rect.left)`.
  3. Map that virtual-desktop pixel to 0..65535:
     `nx = (x − SM_XVIRTUALSCREEN) * 65535 / SM_CXVIRTUALSCREEN` (monitors left/above have negative
     coords — handle them). This is RustDesk's verified formula.
- **Keyboard:** two forms (mirrors `docs/04 §13`). **Unicode text** via `KEYEVENTF_UNICODE`
  (`wVk=0`, `wScan`=UTF-16 unit; chars above U+FFFF need surrogate-pair events) — types any
  character regardless of layout. **Scan codes** (`KEYEVENTF_SCANCODE`) for shortcuts/modifiers/apps
  that inspect raw key state.
- **Stuck keys:** track every pressed key/button; on disconnect/lease-change send `KEYUP`/`*UP` for
  each held. Verify with `GetAsyncKeyState` high bit (0x8000); note it returns 0 on the secure/lock
  desktop.
- **UIPI limit:** a medium-IL host cannot drive an elevated window/UAC prompt/Task Manager, and the
  block is **silent** (no return value, no `GetLastError`). Cross-integrity requires UIAccess
  (signed + installed in a secure location) or running elevated/SYSTEM.
- **Crates:** `enigo` (MIT; RustDesk vendors a fork wired for VIRTUALDESK), raw `windows-rs`.
  Note: don't assume an input lib handles DPI — RustDesk's enigo fork relies on the host manifest.

## 4. Code signing — a security requirement, not cosmetics

Defender classifies RustDesk as **`Win64/RemoteAdmin.RustDesk.A` (PUA)**; unsigned installers trip
SmartScreen; many AVs flag remote-access tools as riskware. **Authenticode-sign everything, ideally
with an EV certificate**, build reputation, and submit to Microsoft's re-evaluation portal.
Prior-art incident (AnyDesk 2024): a **stolen code-signing cert** let attackers sign malware that
looked authentic — so **keep signing keys in an HSM/TPM, off the build/production machines, and use
short-lived signing + revocation** (see `docs/06`). Without signing, Casual RAS is both flagged as
malware (adoption failure) and easier to impersonate (security failure).

## 5. Caveats summary
- Single-process MVP is blind on the secure desktop and to elevated windows (§1).
- DXGI `ACCESS_LOST` on every desktop/mode transition — build the re-acquire loop (`docs/10`).
- UIPI silently drops input into higher-integrity targets (§3).
- `WM_WTSSESSION_CHANGE` doesn't fire on the UAC/CAD desktop switch — poll `OpenInputDesktop` (§2).
- Per-Monitor-V2 manifest is mandatory for click accuracy (§3).
- Unsigned/unreputable binaries are SmartScreen/AV-blocked (§4).

## 6. Decisions & open validation
- **ADR:** DXGI-primary capture; single-process MVP with a pre-designed IPC/desktop-context
  boundary; SendInput ABSOLUTE|VIRTUALDESK + PMv2 manifest; EV code signing from first external
  build.
- **Spike must validate:** QUIC/UDP under Defender Firewall; DXGI capture + SendInput on mixed-DPI
  multi-monitor; behavior across lock/unlock and fast-user-switch (documenting the MVP cliffs).

## 7. Sources
learn.microsoft.com: Interactive Services, Window Stations, Desktops, WTSQueryUserToken,
CreateProcessAsUser, WTSRegisterSessionNotification, WM_WTSSESSION_CHANGE, SendSAS, SendInput,
KEYBDINPUT, The Virtual Screen, High-DPI dev, GetAsyncKeyState, SetProcessDpiAwarenessContext ·
github.com/rustdesk (enigo fork, FAQ, Defender PUA discussions) · Sunshine #2119, LookingGlass #263.
