# 19 — Cross-Platform Host Research: Linux & Windows

> **Status:** research / design note (2026-07). No implementation yet. Consolidates a survey of how
> established remote-access products capture the screen, inject input, encode video, and build/package
> on **Linux** and **Windows**, and turns it into a concrete, permissive-only recommended stack for
> Casual RAS's biggest open gaps: **no input backend on either OS**, **software-only OpenH264 encode**,
> and **no verified Linux/Windows runtime** (all CI-compile-gated today).
>
> Every candidate library below is checked against **Invariant 18** (permissive only —
> MIT/Apache-2.0/BSD/ISC/Zlib/MPL; **no GPL/LGPL/AGPL/SSPL linked**). Products whose *own* license is
> copyleft (RustDesk AGPL, Sunshine GPL) are **study-only** — their techniques and their individually
> permissive building blocks are fair to learn from; their trees are never linked or vendored.
>
> Sources are primary where possible (the actual repos/CI, freedesktop.org, Microsoft Learn, crate
> docs). Items marked *research-derived* have not been runtime-verified by us. Feeds `docs/11`
> (Windows host), a future Linux host doc, and refines **ADR-063** (cross-platform sharing).

---

## 1. Executive summary — the two hard problems

Remote-access on Linux and Windows is dominated by two platform problems that every product either
solves the same way or punts on. Casual RAS's invariants happen to line up with where both platforms
are heading.

### 1.1 Linux: Wayland removed the primitives on purpose
X11 gave any client unrestricted screen-scrape (`XShm`/`XDamage`) and input-injection (`XTest`).
Wayland deliberately removed both for security. The sanctioned replacement is compositor-mediated:
**PipeWire + `xdg-desktop-portal` ScreenCast** for capture and **`RemoteDesktop` portal / libei** for
input — each gated by a **per-session user consent dialog** with **uneven per-compositor support**.
The consequence every vendor hits: **unattended / login-screen capture and input are impossible
through the portal** (no session, no human to click Allow). The only bypasses — **DRM/KMS** capture
and **`uinput`** injection — need a privileged helper and deliberately sidestep consent.

**Vendor scorecard for live-Wayland *incoming* control (being controlled):**

| Product | Wayland incoming | How |
|---|---|---|
| TeamViewer | experimental, effectively outgoing-only | recommends disabling Wayland in gdm |
| AnyDesk | **not supported** | "display server not supported" → switch to Xorg |
| Parsec | beta/unreliable | best on X11; Wayland OK as client only |
| Chrome Remote Desktop | **sidesteps it** | runs its own headless **Xvfb** session |
| NoMachine | limited | GNOME-only PipeWire, else DRM framebuffer fallback |
| RustDesk *(AGPL, study-only)* | furthest ahead publicly | PipeWire + portal, restore-token, uinput/RemoteDesktop input |

The industry consensus is clear: **the portal path is correct for the interactive/consented case, and
robust unattended Wayland is a hard, later milestone.** Casual RAS's existing `ras-media-scap`
(PipeWire + portal) is on the right road, and the portal consent model *aligns with Invariants 1 & 7*
(the OS owns consent) rather than fighting them.

### 1.2 Windows: the secure desktop is off-limits (and increasingly enforced)
Windows renders UAC prompts, the lock screen, and the login screen on the **secure desktop**, which a
normal user process cannot capture or inject into. The vendor norm is a **SYSTEM service in Session 0**
plus a **per-session agent**; to reach elevated windows/secure desktop they use **UIAccess** (a signed,
trusted-path manifest flag) or SYSTEM privilege.

**Casual RAS forbids that path (Invariant 14): never build a secure-desktop/UAC injection bypass,
never request UIAccess.** Emergency stop rides the kernel-owned **SAS (Ctrl+Alt+Del)**, which no
user-mode injector can synthesize — which is exactly why it is an un-overridable stop.

This is now also the platform direction: Microsoft's **January 2026 credential-UI hardening** restricts
credential/secure-desktop input to trusted local (physical) sources, explicitly targeting the
UIAccess remote-support vector — TeamViewer/AnyDesk/RDP can no longer type into UAC/credential dialogs.
**So Invariant 14 is aligned with where Windows is going**, not a self-imposed handicap. It must be
*documented to users*: UAC prompts and the login screen are not remotely controllable by design.

---

## 2. What the field actually uses (capture · input · encode · license)

| Product | Win capture | Linux capture | Win input | Linux input | SW encode | HW encode | Own license |
|---|---|---|---|---|---|---|---|
| **RustDesk** | DXGI Desktop Duplication (+GDI fallback) | X11 fb · Wayland portal+PipeWire via **GStreamer** | `SendInput`; SYSTEM svc + `SendSAS` | libxdo (X11) · **uinput helper** · RemoteDesktop portal | **VP8/VP9 (libvpx BSD-3), AV1 (libaom BSD-2)**, libyuv | own `hwcodec` = FFmpeg (**unlicensed, GPL-flavor**) | **AGPL-3.0** |
| **Sunshine** | DXGI DDA **+ WGC** (`display_wgc`) | KMS/DRM · X11 SHM · wlroots dmabuf · KWin · portal+PipeWire | `SendInput` + `CreateSyntheticPointerDevice` + **ViGEm** | **inputtino (MIT)** = uinput/evdev/uhid | **libx264 (GPL)** | NVENC SDK direct; else FFmpeg (AMF/QSV/VAAPI/MF/VT) | **GPL-3.0** |
| **TeamViewer** | proprietary | portal/PipeWire (partial) | SYSTEM service | — | proprietary delta/tile codec | — | proprietary |
| **AnyDesk** | proprietary | X11 (no Wayland incoming) | installed-mode elevation | — | **DeskRT** (GUI-aware, not H.264/JPEG) | — | proprietary |
| **Parsec** | DXGI DDA family, zero-copy | X11 (Wayland beta) | SYSTEM service; VDD (IddCx) | — | — | NVENC/QSV/AMF, H.264/HEVC | proprietary |
| **Chrome Remote Desktop** | WebRTC capturer | **own Xvfb session** (X11) | WebRTC DataChannel input | (X11 in its session) | **VP8/VP9 over WebRTC** | — | proprietary |
| **NoMachine** | NX / screen-encode | PipeWire (GNOME) / DRM fb | — | — | H.264/VP8 | NVENC/QSV/AMF | proprietary |

**Reusable, individually-permissive building blocks surfaced by the survey** (usable independently of
the copyleft products that ship them): **inputtino (MIT)**, **ViGEmClient (BSD-3)**, **libvpx (BSD-3)**,
**libaom (BSD-2)**, **libyuv (BSD-3)**, **enigo/rdev (MIT)**, **libei/`reis` (MIT)**, **`ashpd` (MIT)**,
**`cros-libva`/`cros-codecs` (BSD-3)**.

### Notable architecture lessons
- **Parsec BUD** (co-designed UDP transport + codec, ~97% NAT traversal, congestion signals feed the
  encoder) is the closest analogue to the Iroh/QUIC latency-first goal. Casual RAS already has
  `LatencyFirstAbr` + windowed-loss `HealthObserver`; the lesson is to keep tightening that loop.
- **AnyDesk DeskRT** is the counterexample to "just use H.264": GUI content (flat regions, sharp edges,
  repetition) compresses far better with a desktop-aware codec than a video codec. Our H.264/OpenH264
  path is pragmatic but note the quality/bandwidth ceiling — desktop-aware coding is where incumbents
  win.
- **Chrome Remote Desktop** validates the deferred webapp-controller track (ADR-057): WebRTC + VP8 over
  a DataChannel, P2P via ICE — and validates sidestepping Wayland via a dedicated virtual session for
  headless hosts.
- **Restore-token**: RustDesk and Sunshine both persist the portal `restore_token` to avoid
  re-prompting the local user every session. **We should do the same** in the `ras-media-scap`/portal
  path (see §3).

---

## 3. Recommended Linux stack

Build **Wayland-first (consented)**, keep **X11** as a compatibility backend, treat **unattended
(DRM/KMS + uinput)** as a separately-provisioned, audited posture — never the interactive default.

| Concern | Recommendation | License | Notes |
|---|---|---|---|
| **Capture (Wayland, consented)** | Keep **`scap`** (portal ScreenCast + PipeWire). **Add `restore_token` persistence** (persist_mode 2, store token in the identity keystore keyed by host+display) to stop re-prompting reconnects. | MIT/Apache ✅ | Token is single-use — store the fresh one returned on each `Start`. Detect ScreenCast interface `version` at runtime and degrade if <4. |
| **Capture (X11 legacy)** | **`x11rb`** + XShm + **XDamage** (dirty-rect only — never full-frame `XGetImage`, it's a known GPU-readback bottleneck). | MIT/Apache ✅ | |
| **Capture (unattended/greeter)** | *Later, opt-in only:* **DRM/KMS** via `libdrm`/`gbm` in a privileged helper. Needs a virtual display when headless (vkms / EDID dongle). | libdrm MIT ✅ | **Deliberate consent-bypass — gate behind explicit unattended provisioning + audit; never the interactive path (Inv 1/7).** |
| **Input (Wayland, consented)** | **`ashpd`** (`RemoteDesktop` + `ConnectToEIS`) → **`reis`** libei client. | MIT ✅ | `reis` is **pre-1.0** ("API subject to change, lacks some checks libei has") — pin exact version, expect churn. GNOME/KDE ship EIS; wlroots partial. |
| **Input (portable, robust)** | **inputtino (MIT)** — virtual input over **uinput/evdev/uhid**, **X11/Wayland-agnostic** (goes below the display server), fail-closed on permission errors. *This is the single best find for the Linux input gap.* | MIT ✅ | C++ lib (Sunshine's). Either FFI-wrap it (confine `unsafe` per CONTRIBUTING §5, like `ras-media-macos`) or port its approach onto the `uinput` Rust crate. Needs a udev `uaccess` rule for `/dev/uinput` (privileged-helper / install step). |
| **Input (X11)** | **`x11rb`** XTest, or **`enigo`** X11 backend (its stable path). | MIT/Apache ✅ | `enigo`'s Wayland/libei backends are experimental behind feature flags — not production yet. |
| **Encode (default)** | Keep **OpenH264** software (current `ras-media-openh264`). | BSD-2 ✅ | |
| **Encode (HW, Intel/AMD)** | **`cros-libva` / `cros-codecs`** VAAPI encoder. | BSD-3 ✅ | Both **0.0.x** — verify H.264 **encode** entrypoints on target GPUs (decode is more mature than encode). Needs libva ≥ 1.20. |
| **Encode (HW, NVIDIA)** | NVENC via self-bound Video Codec SDK — *later, on demand only.* | NVIDIA SDK BSD-3-style ✅ | Adds real surface; only if a customer needs it. |
| **Portal D-Bus** | **`zbus`** (via `ashpd`) — avoids the C `libdbus` dep. | MIT ✅ | |
| **⚠️ Do NOT link** | **FFmpeg/x264** for HW encode. | LGPL / GPL ❌ | The whole reason Sunshine is GPL and RustDesk's `hwcodec` is unusable. If ever needed, isolate FFmpeg as a *separate CLI process*, never linked — and even then avoid `--enable-gpl`. |

**Build order for the missing Linux input backend** (all permissive): **X11 XTest** now → **inputtino
(uinput) privileged helper** for robustness + unattended → **`ashpd`+`reis` libei** for the
Wayland-consented sandbox-friendly path. There is **no drop-in production Linux input backend today** —
this is genuinely new work, not a crate swap.

---

## 4. Recommended Windows stack

Windows is the eventual production target; the team has no Windows hardware, so this stays
**CI-compile-gated** until a runner or device is available. Build a first-party `ras-media-windows` +
`ras-input-windows` mirroring the macOS crates.

| Concern | Recommendation | License | Notes |
|---|---|---|---|
| **Screen capture** | **WGC** default (via `scap` now; migrate to first-party on **`windows-rs`** or **`windows-capture`** for zero-copy + border-off on Win11). DXGI DDA as the latency/dirty-rect escape hatch (via `windows-rs` or **`dxcapture`**). | MIT / MIT-Apache ✅ | WGC interop min OS is **Win10 1903** (not 1803); yellow border only removable on **Win11 20348+** (`IsBorderRequired`). `scap` outputs **CPU BGRA** → breaks zero-copy; a native backend is needed to keep the `ID3D11Texture2D` on the GPU. |
| **❌ Never** | **`dxgcap`** | **AGPL-3.0** ❌ | DISQUALIFIED by Inv 18. Use `windows-capture`/`dxcapture`/`windows-rs` for DXGI instead. |
| **Input injection** | First-party **`ras-input-windows`** on raw **`windows-rs` `SendInput`** (or `enigo` for a quick first pass). **In-session, no UIAccess.** | MIT-Apache / MIT ✅ | Parity with `ras-input-macos`: absolute-coord normalization to captured geometry, tracked-key `release_all` (Inv 4), Secure-Input/secure-desktop **no-op** (Inv 14). |
| **Gamepad (if ever)** | **ViGEmClient (BSD-3)** + `CreateSyntheticPointerDevice` for touch/pen. | BSD-3 ✅ | Needs the ViGEmBus driver (upstream EOL Aug 2025; LizardByte fork maintained). Out of MVP scope. |
| **Hardware encode** | First-party **Media Foundation MFT** encoder on **`windows-rs`** (enumerate `MFT_ENUM_FLAG_HARDWARE`; bind D3D11 device; feed captured NV12 texture via `MFCreateDXGISurfaceBuffer`; `MF_LOW_LATENCY`). Vendor-neutral (NVIDIA/AMD/Intel). | MIT OR Apache-2.0 ✅ | Windows analogue of VideoToolbox in `ras-media-macos`. Zero-copy, no GPL taint. NVENC/AMF/oneVPL only as later tuned backends. |
| **Software fallback** | **`ras-media-openh264`** (existing). | BSD-2 ✅ | |
| **⚠️ Do NOT link** | **FFmpeg** with `--enable-gpl`/x264. | GPL ❌ | `h264_mf`/`nvenc`/`qsv`/`amf` are LGPL-clean but x264 taints the whole link. Direct Media Foundation sidesteps it — preferred. |
| **Build target** | **`x86_64-pc-windows-msvc`** | — | Production ABI; required for Windows SDK + WebView2 + Tauri. Follow RustDesk's MSVC+vcpkg model, **not** Sunshine's MinGW. |
| **CI / packaging** | **GitHub Actions `windows-latest`** for compile-gate + release. `cargo-xwin` only for local cross-*checks*. | — | Cross-*packaging* the Tauri app from mac/Linux is **not reliable** (WebView2, NSIS/WiX, C system deps). Matches the no-hardware constraint. |
| **Installer** | **NSIS** now (cross-buildable); **MSI/WiX** later for enterprise. Embed WebView2 **offline installer** (`webviewInstallMode="offlineInstaller"`, +~127 MB) for embedded/white-label offline deploys. | — | Pin **Tauri ≥ 2.11.1** (Origin-Confusion CVE). |
| **Signing** | **EV cert on HSM (Azure Key Vault)** — hardening phase. | — | Immediate SmartScreen reputation; keys off build machines. Alpha is unsigned (SmartScreen warns), consistent with the macOS notarization deferral. |

### The Windows service/session split (S4 hardening, not MVP)
Production Windows needs **two processes**: a **Session-0 SYSTEM service** (always-on, owns the network
endpoint, survives logout, reaches pre-logon) + a **per-session agent** in the interactive session
(does capture/input/consent — those need an interactive desktop). The service launches the agent via
`WTSGetActiveConsoleSessionId` → `WTSQueryUserToken` (needs `SeTcbPrivilege`) → `CreateProcessAsUser`
with `lpDesktop = "WinSta0\\Default"`, and tracks `WM_WTSSESSION_CHANGE` (logon/logoff/lock/unlock/RDP)
+ desktop switches. **Today Casual RAS is single-process (MVP, S4)** — fine for on-demand,
user-initiated support where a user is already logged in. The later split must add: the service +
token-launch, session/desktop-change tracking, an **audited content-free IPC boundary** (control/verdict
enums only — never pixels/keystrokes/secrets, Inv 8/11), and preservation of host-authoritative
`authorize_input` + `revoke_all`/`release_all` across the process boundary — while continuing to refuse
secure-desktop injection (Inv 14).

---

## 5. The Invariant-18 license verdict (the gate that decides everything)

The survey's most important output is a clean permissive path for every layer — and a short list of
traps to never link:

**✅ Permissive — safe to link:** `scap`, `windows-capture`, `dxcapture`, `windows-rs`, `x11rb`,
`ashpd`, `reis`, `enigo`, **inputtino**, `uinput`, **ViGEmClient**, `cros-libva`/`cros-codecs`,
`openh264`, libvpx/libaom/libyuv (all MIT/Apache/BSD).

**❌ Forbidden — never in the linked graph:**
- **`dxgcap`** — AGPL-3.0.
- **RustDesk `hwcodec`** — no SPDX license + bundles GPL-flavor FFmpeg; unverifiable ⇒ fails `cargo-deny`.
- **libx264 / FFmpeg built `--enable-gpl`** — GPL (the reason Sunshine is GPL).
- **FFmpeg core** — LGPL; even LGPL is out as a *linked* dep (Inv 18). Isolate as a separate CLI process only, if ever.
- The **RustDesk (AGPL)** and **Sunshine (GPL)** trees themselves — study-only, never vendored.

The MVP already sits on the right side of this line (OpenH264 BSD-2, `scap` MIT). Every recommended
next step keeps it there.

---

## 6. Build & CI notes (cross-cutting)

- **Linux glibc floor:** glibc is forward-*incompatible* — a binary built on newer glibc fails on older
  distros (`GLIBC_2.xx not found`). **Build in an Ubuntu 22.04 container** (the oldest base that still
  ships **WebKitGTK 4.1** for Tauri v2). This is the same lesson PR #2 already applied to the release
  workflow (ubuntu-22.04 pin) — extend it to the host build. AppImage does **not** fix glibc.
- **Linux packaging:** ship **.deb/.rpm** (so the installer can drop the **`/dev/uinput` udev
  `uaccess` rule** and register a privileged input helper). **Avoid Flatpak/Snap for the full host** —
  the sandbox blocks `uinput` and DRM/KMS (portal capture works *because* it's mediated, but unattended
  input does not). A portal-only view/consented-input build *can* be Flatpak'd. Tauri v2 emits
  AppImage + .deb + .rpm natively.
- **Windows build:** MSVC + `windows-latest` runners are the realistic path; you cannot reliably
  cross-package the Tauri Windows app from mac/Linux. `cargo-xwin` is useful for a local
  `cargo check --target x86_64-pc-windows-msvc` compile-gate only.
- **System dev-deps (Linux host):** `libpipewire-0.3-dev`, `libdbus-1-dev` (or pure-Rust `zbus`),
  `libclang` (bindgen), `libva-dev` (+driver), X11 headers (`libxcb`/`libxtst`/`libxdamage`),
  `libdrm-dev`/`libgbm-dev` (if DRM), `nasm` (openh264), and the Tauri WebKitGTK 4.1 stack.

---

## 7. Concrete next steps for the open gaps

Ordered by value and by what's verifiable without new hardware. (Design-phase: each real backend
needs its own ADR before code, per CLAUDE.md §9.)

1. **`ras-input-windows` (needs an ADR):** raw `windows-rs` `SendInput`, in-session, no UIAccess,
   mirroring `ras-input-macos` (normalized coords → pixels post-authorization, tracked-key
   `release_all`, secure-desktop no-op). Compile-gated on `windows-latest`; runtime-verified when a
   Windows device/runner exists. This closes the largest single gap and is symmetric with work already
   done on macOS.
2. **`ras-input-linux` (needs an ADR):** start with **X11 XTest** (`x11rb`) for X sessions; add an
   **inputtino/`uinput` privileged-helper** backend (udev `uaccess` rule) for robustness + eventual
   unattended; add the **`ashpd`+`reis` libei** consented-Wayland path last. All permissive.
3. **`restore_token` persistence** in the `ras-media-scap` portal path — small, high-UX-value, removes
   the re-prompt on every reconnect. Verifiable on the dev Linux machine.
4. **Hardware encode** (later, on demand): Media Foundation MFT on Windows (`windows-rs`); VAAPI via
   `cros-codecs` on Linux. Both permissive; keep OpenH264 as the universal fallback.
5. **Document the platform limits to users** (Inv 17 honesty): UAC prompts / Windows login screen and
   the Linux greeter are **not** remotely controllable by design — and, on Windows, increasingly not by
   OS enforcement (Jan-2026 hardening). Add this to `docs/11` and any public claims.

---

## 8. Invariant call-outs

- **Inv 1 & 7 (local owner / honest consent):** the Wayland **portal consent dialog** is a feature, not
  a bug — the OS owns consent. Keep the consented path as the default; treat DRM/KMS + uinput unattended
  as a separately-provisioned, audited posture.
- **Inv 14 (no secure-desktop bypass / no UIAccess):** *validated by the platform.* Microsoft's
  Jan-2026 credential-UI hardening now enforces at the OS level what Inv 14 already mandated. Build the
  Windows input backend **without** UIAccess; emergency stop stays on the kernel SAS path.
- **Inv 18 (permissive only):** a clean permissive stack exists for every layer (§5). The only traps
  are `dxgcap` (AGPL), RustDesk `hwcodec` (unlicensed/GPL-FFmpeg), and x264/FFmpeg-GPL — all avoidable.
- **Inv 6 (narrow validated input):** inputtino and `SendInput` map onto the existing `OsInputSink`
  seam and the host-authoritative `authorize_input` gate — the platform backends stay dumb; the
  `ras-control` gate stays the single authority.
- **S4 (process split):** the Windows Session-0 service + session-agent split and the Linux privileged
  input helper are the same hardening story — a later, audited trust boundary carrying only content-free
  messages.

---

## Sources

**Open-source (repos / CI / DeepWiki):**
- [RustDesk LICENCE (AGPL-3.0)](https://raw.githubusercontent.com/rustdesk/rustdesk/master/LICENCE) · [scrap platform dispatch](https://github.com/rustdesk/rustdesk/blob/master/libs/scrap/src/common/mod.rs) · [vcpkg.json](https://raw.githubusercontent.com/rustdesk/rustdesk/master/vcpkg.json) · [DeepWiki: capture/encode](https://deepwiki.com/rustdesk/rustdesk/5.1-video-capture-and-encoding) · [DeepWiki: input](https://deepwiki.com/rustdesk/rustdesk/4.1-input-service) · [DeepWiki: Wayland](https://deepwiki.com/rustdesk/rustdesk/6.3.1-wayland-support)
- [rustdesk-org/hwcodec (no SPDX license)](https://github.com/rustdesk-org/hwcodec)
- [Sunshine CMakeLists (GPL-3.0)](https://raw.githubusercontent.com/LizardByte/Sunshine/master/CMakeLists.txt) · [video.cpp encoder tables + libx264 fallback](https://raw.githubusercontent.com/LizardByte/Sunshine/master/src/video.cpp) · [windows/input.cpp (SendInput+ViGEm)](https://raw.githubusercontent.com/LizardByte/Sunshine/master/src/platform/windows/input.cpp) · [ci-windows.yml (MSYS2/MinGW)](https://raw.githubusercontent.com/LizardByte/Sunshine/master/.github/workflows/ci-windows.yml)
- [inputtino (games-on-whales, MIT)](https://github.com/games-on-whales/inputtino) · [ViGEmBus fork (BSD-3)](https://github.com/LizardByte/Virtual-Gamepad-Emulation-Bus)

**Linux platform:**
- [ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html) · [RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html) · [restore-token impl](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.impl.portal.ScreenCast.html) · [ConnectToEIS PR #762](https://github.com/flatpak/xdg-desktop-portal/pull/762)
- [libei 1.0 (Phoronix)](https://www.phoronix.com/news/libei-1.0-Emulated-Input) · [who-t: libei](http://who-t.blogspot.com/2020/08/libei-library-to-support-emulated-input.html) · [reis (MIT)](https://github.com/ids1024/reis) · [ashpd (MIT)](https://github.com/bilelmoussaoui/ashpd) · [enigo](https://github.com/enigo-rs/enigo) · [uinput crate](https://crates.io/crates/uinput)
- [cros-libva (BSD-3)](https://github.com/chromeos/cros-libva) · [cros-codecs](https://docs.rs/cros-codecs) · [FFmpeg legal/licensing](https://www.ffmpeg.org/legal.html)
- [GNOME Remote Desktop (headless/greeter)](https://github.com/GNOME/gnome-remote-desktop) · [ReFrame DRM/KMS](https://github.com/AlynxZhou/reframe) · [Tauri v2 Debian](https://v2.tauri.app/distribute/debian/)

**Windows platform:**
- [WGC vs DXGI Desktop Duplication (OBS)](https://obsproject.com/forum/threads/windows-graphics-capture-vs-dxgi-desktop-duplication.149320/) · [CreateForWindow — min Win10 1903 (MS Learn)](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createforwindow) · [Desktop Duplication API (MS Learn)](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)
- [scap (MIT)](https://github.com/CapSoftware/scap) · [windows-capture](https://docs.rs/windows-capture) · [dxgcap — AGPL-3.0](https://crates.io/crates/dxgcap)
- [UIAccess secure-locations policy (MS Learn)](https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-10/security/threat-protection/security-policy-settings/user-account-control-only-elevate-uiaccess-applications-that-are-installed-in-secure-locations) · [Windows credential-UI hardening 2026](https://windowsnews.ai/article/windows-credential-ui-hardened-how-microsofts-2026-security-update-blocks-remote-input.401133) · [Project Zero: Administrator Protection / UIAccess](https://projectzero.google/2026/02/windows-administrator-protection.html)
- [WTSQueryUserToken (MS Learn)](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken) · [Media Foundation H.264 MFT (gist)](https://gist.github.com/KeloCube/0e56ba7f2c5729223483147eb35d9cc7) · [IMFTransform (windows-rs docs)](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Media/MediaFoundation/struct.IMFTransform.html) · [cargo-xwin](https://github.com/rust-cross/cargo-xwin) · [Tauri Windows signing](https://v2.tauri.app/distribute/sign/windows/) · [Tauri Windows installer](https://v2.tauri.app/distribute/windows-installer/)

**Commercial:**
- [TeamViewer Wayland (Community)](https://community.teamviewer.com/English/discussion/122410/teamviewer-support-on-wayland-experimental-state) · [AnyDesk UAC/elevation](https://support.anydesk.com/docs/administrative-privileges-and-elevation-uac) · [AnyDesk Wayland "not supported"](https://www.airdroid.com/remote-support/anydesk-remote-display-server-not-supported/) · [Parsec Windows service/secure-desktop](https://support.parsec.app/hc/en-us/articles/32381199341716-Parsec-App-for-Windows) · [Parsec BUD protocol](https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007) · [CRD Linux (Xvfb)](https://docs.cloud.google.com/architecture/chrome-desktop-remote-on-compute-engine) · [NoMachine Wayland KB](https://kb.nomachine.com/AR04R01083)

> **Confidence notes:** proprietary codec internals (TeamViewer, AnyDesk DeskRT, Parsec BUD) are from
> vendor material — directional, not spec-grade. `reis` and `cros-libva`/`cros-codecs` are **pre-1.0**
> — pin exact versions, expect churn, verify encode-entrypoint coverage before committing. RustDesk
> source line numbers (partly from DeepWiki's index) may drift on `master`; mechanisms are corroborated.
