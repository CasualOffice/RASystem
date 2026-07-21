# 22 — Learnings Tracker & Half-Done-Implementation Fix Plan

> **Purpose.** Two jobs in one doc:
> 1. **Record the learnings** from the 2026-07 RustDesk + open-source-ecosystem study-only research
>    sweep (Wayland input, cursor mechanics, capture self-exclusion, video latency) so the techniques
>    aren't lost in a chat transcript.
> 2. **Track the fixes** — turn the "landed but half-done" items (the ones `CLAUDE.md §3` honestly flags
>    as *on-device-only / pending / stubbed*) into concrete, checkbox-tracked tasks with a **Done**
>    definition each.
>
> This does **not** replace `docs/17` (roadmap/milestones), `docs/20` (feature gaps), or `docs/21`
> (production-readiness backlog). It is the **working task tracker** that mirrors their P0/P1 items and
> adds the research-derived rationale. Keep it current as work lands (☐ → ◐ → ☑), and bump `CLAUDE.md §3`
> when an item flips to ☑.
>
> **Study-only reminder (Inv 18).** RustDesk is AGPL — every technique below is described from public API
> docs / architecture, never copied. Clean-room only. Recommended deps are all permissive
> (MIT/Apache/BSD): `reis` (MIT), `ashpd` (MIT), `libei` (MIT), `input-linux` (MIT), OpenH264 (BSD-2).
> Do **not** link or vendor: RustDesk (AGPL), gnome-remote-desktop (GPL-2.0), Sunshine (GPL-3.0),
> x264 (GPL); avoid the `uinput` crate (WTFPL — off-allowlist).

Statuses: ☐ not started · ◐ in progress · ☑ done. Workstream tags per `docs/17` (`CORE`/`NET`/`MED`/
`SEC`/`UI`/`INF`/`QA`, plus `LINUX`/`WIN`/`MAC` platform tags). Fixability: `CODE-NOW` (off-device
today) · `MAC-DEV`/`LINUX-DEV` (needs our own hardware, which we have) · `WIN-HW` (needs Windows
hardware we lack) · `$FUND` (needs money).

---

## Part A — Research learnings (2026-07 study-only sweep)

Full report with citations lives in the workflow transcript; this is the durable distilled version.

### A1. Linux input injection — the Wayland problem

- **RustDesk's mechanism:** kernel **`/dev/uinput`** (below the display server → works on X11 *and*
  every Wayland compositor), driven by a **root systemd service** (`res/rustdesk.service`: `User=root`,
  `ExecStart=/usr/bin/rustdesk --service`; **verified**). A per-user `--server` process sends
  `DataKeyboard`/`DataMouse` IPC to the root daemon, which owns the device. Keyboard built with the
  **`evdev`** crate's `VirtualDeviceBuilder` (**verified**); mouse via `mouce::UInputMouseManager`.
  XTEST (their in-repo `enigo`/`libxdo` fork) is the **X11-only** path. Screen *capture* on Wayland is
  portal + PipeWire, but **input stays uinput** — a split mechanism. Flatpak sandbox can block
  `/dev/uinput`.
- **Ecosystem (the mature answer diverges):** GNOME Remote Desktop and KDE KRdp/KWin converge on
  **libei/EIS through the `org.freedesktop.portal.RemoteDesktop` portal** (`ConnectToEIS()` → fd →
  libei). `libei` (Peter Hutterer/Red Hat, **MIT, verified**) is the unprivileged, consent-gated,
  Wayland-native path. wlroots historically used `zwlr_virtual_pointer_manager_v1` +
  `zwp_virtual_keyboard_manager_v1`. Weston's RDP backend (FreeRDP, Apache-2.0) injects natively
  per-seat. Sunshine (GPL-3.0) uses uinput like RustDesk.
- **Trade-off:** uinput = privileged one-time setup (root or `/dev/uinput` ACL), no per-session consent,
  works everywhere below the compositor. libei/portal = no root, Wayland-native consent, but needs a
  recent compositor (GNOME 45+/recent KDE) with EIS support.
- **➡ Our decision:** add a **libei backend via `reis` (MIT) bootstrapped through `ashpd` (MIT)
  `ConnectToEIS()`** as the Wayland path, keep XTEST (`x11rb`) for X11, keep an optional `input-linux`
  (MIT) uinput fallback for headless/bare-Weston. This is Inv-18 clean, needs **no root**, and the
  portal *is* an OS consent gate (aligns with Inv 1). Do **not** copy RustDesk's root-daemon+uinput
  model as primary — it contradicts our unprivileged posture. See fix **L1**.

### A2. Cursor mechanics — no fighting cursors / no double cursor

- **Capture WITHOUT the OS cursor; ship shape + position as separate cached messages; composite a soft
  cursor client-side.** Universal. RustDesk validates our ADR-073 design exactly: proto `CursorData{id,
  hotx, hoty, width, height, colors}` + `CursorPosition{x,y}` + bare `cursor_id` (cache-by-id; resend
  full bitmap only on shape change). Controller draws its own overlay cursor.
- **Capture-cursor-off per OS:** macOS `SCStreamConfiguration.showsCursor = false`; Windows DXGI
  reports the pointer separately (`DXGI_OUTDUPL_FRAME_INFO.PointerPosition` + `GetFramePointerShape`
  on shape change); Linux X11 `xcb_xfixes_get_cursor_image` (XFixes). Baked-in cursor only happens on
  Windows **virtual displays** (no hw cursor plane) — a known RustDesk break.
- **Injection = absolute position by default**, per-connection relative mode for games.
- **Fighting cursors is only partially solved even in RustDesk:** they suppress *self-echo* (don't
  broadcast `CursorPosition` back to the connection that moved within the last **300 ms**), but **true
  local-vs-remote arbitration does not exist** (open RustDesk feature request #15488, "local input takes
  priority, suppress remote 100–300 ms after local activity, like AnyDesk").
- **macOS double-cursor root cause:** `CGWarpMouseCursorPosition` desyncs hardware movement from the
  displayed cursor. Correct pattern uses **balanced stateful pairs**:
  `CGAssociateMouseAndMouseCursorPosition(false)` + `CGDisplayHideCursor` around the move, then
  re-associate/show. RustDesk's real Mac↔Win "two cursors" bug (Discussion #10267, PR #10314) was
  exactly a failure to engage this state.
- **➡ Our decision:** our design is already right (cursor-free capture + ADR-073 channel + Tauri overlay).
  Two disciplines to enforce: **(a)** treat macOS cursor hide/associate as balanced pairs cleaned up on
  **every** teardown path incl. Inv-4 emergency stop; **(b)** make a deliberate local-vs-remote
  arbitration decision (our Inv-5 single-controller lease covers remote-vs-remote; local-vs-remote needs
  an explicit choice). See fixes **C1**, **C2**.

### A3. Excluding the app's own windows from its own capture

| OS | API | Who excludes | Notes |
|---|---|---|---|
| macOS | `SCContentFilter(display:excludingWindows:)` / `(…excludingApplications:exceptingWindows:)` | capturing app | capturer-side filter; pairs with our `excluded_window_ids` |
| Windows | `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` | **the excluded window's own process, on itself** | system-wide; Win10 2004+; WGC/DXGI coverage is strong practice but **not doc-enumerated** — confirm on-device |
| Windows WGC item API | none | — | no item-level exclude param (only `IncludeSecondaryWindows`) |
| Linux PipeWire/portal | **none** | — | **impossible** — picker is compositor/user-owned & opaque (freedesktop issue #1064) |

- **RustDesk:** only code-confirmed self-exclusion is Windows `WDA_EXCLUDEFROMCAPTURE` (PR #6470), used
  for local-privacy blackout not toolbar-hiding. No macOS `SCContentFilter` exclusion; Linux none.
- **➡ Our decision:** our Inv-7 "secure window" work is correct and Linux-no-op is a real platform limit,
  not our gap. macOS: prefer `SCContentFilter` exclusion (capturer-side) to pair with the existing
  `excluded_window_ids`. Windows: overlay/indicator windows must **self-mark** with
  `WDA_EXCLUDEFROMCAPTURE`. Linux: keep no-op; mitigate structurally (don't overlap UI with the shared
  monitor). See fix **X1** (verification only — mechanism already chosen).

### A4. Video latency

- **RustDesk codecs:** VP8/VP9 (libvpx), AV1 (libaom), H.264/H.265 (hardware-first via FFmpeg
  `hwcodec`; **no software H.264** — falls back to VP9). Auto priority H.265>H.264(HW)>AV1>VP9>VP8.
  Linux VAAPI *encode* effectively broken → out-of-the-box Linux lands on **software VP9**.
- **libvpx realtime settings:** `VPX_DL_REALTIME`, VP9 `CPUUSED=7`, `VPX_CBR`, `rc_dropframe_thresh=25`,
  error-resilient, row-MT. **Keyframes:** `kf_mode=VPX_KF_DISABLED` (infinite GOP during live control;
  240-frame GOP only when recording). No explicit "force keyframe now" API found — driven by
  encoder-internal logic + encoder recreation (**our forced-IDR-on-demand is cleaner**).
- **ABR = RTT-driven** (`TestDelay` round-trips; bitrate ratio every 3 s, FPS every 1 s + on lag; delay
  bands), not GCC/bandwidth-estimation. On no-new-frame it **repeats** the previous frame (~10×) to keep
  the pipeline warm.
- **➡ Our decision:** keep our H.264/WebCodecs + OpenH264 (BSD-2) stack — it's a coherent, Inv-18-clean
  choice and our forced-IDR + QUIC-loss-driven `LatencyFirstAbr` is *more* principled than RustDesk's
  RTT-only scheme. Do **not** switch to VP9 to match them (scope creep). Tune OpenH264 for realtime
  (constrained-baseline — already `avc1.42E0xx`, no B-frames, CBR/ABR at target, dropframe under
  congestion). See fix **V1**.

### A5. Cross-OS input synthesis parity — drag + double-click (verified vs enigo, the crate RustDesk ships)

- **What RustDesk actually uses:** RustDesk's `rdev` fork does mouse via **`enigo`** (`en.mouse_down`/
  `mouse_move_to`). So enigo is the real reference (study-only; read, never linked — Inv 18).
- **macOS is the only OS that needs explicit code** (confirmed by reading `enigo/src/macos/macos_impl.rs`):
  - **Drag:** `move_type()` — track pressed buttons, post `kCGEventLeftMouseDragged`/`RightMouseDragged`/
    `OtherMouseDragged` (carrying the held button) instead of `MouseMoved`, else the drag doesn't track.
  - **Double-click:** `nth_button_press()` stamps `kCGMouseEventClickState` — **time-only, same-button,
    unbounded** count (no position slop, no cap). macOS does NOT click-count synthetic events for free.
  - **➡ Ours (`ras-input-macos`) now matches exactly:** `motion_kind()` + `advance_click_count`
    (time+same-button, unbounded). We also **dropped** the old `CGDisplayHideCursor`/dissociate warp
    discipline (it hides the cursor from the baked capture and locks the owner out — Inv 1/4). ADR-100.
- **Windows + Linux need nothing** (confirmed by reading `enigo/src/win/win_impl.rs` +
  `linux/x11rb.rs`): enigo sends only `MOUSEEVENTF_*DOWN/UP` / XTEST press-release and **relies on the
  OS** to aggregate rapid clicks into a double-click and to treat motion-during-hold as a drag. Adding
  clickState/drag-type there would **double-count** — so it's deliberately absent.
  - **➡ Our backends already match:** Windows `SendInput` (move+button combined, wheel signs correct);
    Linux **XTEST** (button at the pointer position — correct because the controller now sends a
    prime-move before each click); Linux **uinput/Wayland** (positions first with its own SYN frame, then
    the button edge — the most robust of the three). Drag + double-click are OS-native on all of them.
- **Shared across all three** (not OS-specific): continuous cursor-follow (controller JS), one baked
  cursor (`show_cursor: true` on SCK/scap), wheel-direction signs. On-device row: the live two-machine
  run (the `q` decode-queue HUD number is the lag diagnostic).

---

## Part B — Half-done implementation fix plan

The rule for this section: an item is **☑ done only when it is verified in the environment it runs in** —
loopback-green / cross-compile-clean is **◐**, not ☑ (per `docs/21`'s grading and Inv 17). Grouped by
priority. Mirrors `docs/21` P0/P1 where noted.

### B0 — P0 ship-blockers (from `docs/21`)

- ☐ **X-RECONNECT** `NET` — **Real session re-dial over iroh, not freeze-then-die.** The state machine
  has `Suspended` + a reconnect window (ADR-091) and the loopback `LoopbackCut.heal()` path is tested,
  but the **iroh concrete re-dial** (`Endpoint` accept/connect, same-peer) is the on-device follow-up.
  *Fixability:* `CODE-NOW` + `LINUX-DEV`/`MAC-DEV` to verify over a real impaired link.
  *Done:* pull the network mid-session → controller `Suspended`→`Active`, video resumes with a fresh
  keyframe, lease/grant survives (or is re-validated in-window), reconnect audited — over a **real**
  lossy/NAT-rebind link, not loopback.
- ☐ **X-BLACKSCREEN** `MED` — **No-black-screen guarantee on connect/reconnect/resume.** Forced IDR +
  capture rebind on every start/resume/resolution change must be *structurally* guaranteed before any
  frame renders. *Fixability:* `CODE-NOW`. *Done:* fault-injection test (decoder reset, transport
  cut+restore, mid-session config change) never renders a stale/garbage frame; first visible frame after
  any resync is a keyframe.
- ☐ **X-INPUT-MAC** `MAC` `SEC` — **On-device macOS input verification.** Live CGEvent injection with the
  PostEvent-TCC prompt, Secure-Input dropping injection in a password field, `release_all` clearing
  modifiers on teardown. *Fixability:* `MAC-DEV`. *Done:* controller keypress → visible host action,
  recorded end-to-end.
- ☐ **X-INPUT-LINUX** `LINUX` `SEC` — **On-device Linux X11 input verification** (XTEST into a real
  X/Xwayland session; fail-closed when no X server). *Fixability:* `LINUX-DEV`. *Done:* as above on Linux.
- ☐ **X-KEYBOARD** `SEC` — **Full keyboard coverage + lock-state + modifier reconciliation verified live**
  (ADR-074): F13–F24, nav/numpad w/ NumLock, HID-usage map, "taps only on mismatch", no stuck modifier
  on focus loss / killed-controller-mid-chord. *Fixability:* `MAC-DEV`+`LINUX-DEV`. *Done:* cross-device
  matrix (Mac↔Linux), Caps/Num stay synced over 100 toggles, no stuck modifier after a killed chord.

### B1 — Linux / Wayland (the research-driven track)

- ☐ **L1** `LINUX` `SEC` — **libei Wayland input backend.** New `OsInputSink` impl over **`reis` (MIT)**
  bootstrapped via **`ashpd` (MIT)** `org.freedesktop.portal.RemoteDesktop` → `ConnectToEIS()`. Behind the
  existing `OsInputSink` seam. Fallback chain: **libei (Wayland) → XTEST (X11) → optional `input-linux`
  uinput (headless/bare-Weston)**. Needs an ADR (next free number after ADR-098). *Fixability:*
  `CODE-NOW` build + `LINUX-DEV` verify. *Done:* injects into a **native Wayland** session (GNOME/KDE)
  through the portal consent dialog, no root, `cargo-deny` clean; falls through to XTEST on X11.
- ☐ **L2** `LINUX` `MED` — **Wayland capture cursor-off + separate cursor path** verified: PipeWire
  screencast without baked cursor, cursor via the ADR-073 channel. *Fixability:* `LINUX-DEV`.
- ☐ **L3** `LINUX` `SEC` — **Decide + document the uinput fallback's permission model** (if we ship it):
  udev rule vs `input` group vs none. Prefer *no* uinput in the default install (portal-only) to keep the
  unprivileged posture; document uinput as an opt-in for headless. *Fixability:* `CODE-NOW` (decision +
  doc).

### B2 — Cursor discipline (research-driven)

- ☐ **C1** `MAC` `MED` — **macOS cursor hide/associate as balanced pairs.** Wrap warp/inject in
  `CGAssociateMouseAndMouseCursorPosition(false)`+`CGDisplayHideCursor`, re-associate/show on **every**
  teardown path including Inv-4 emergency stop / `release_all`. Prevents the RustDesk double-cursor /
  stuck-hidden-cursor bug class. *Fixability:* `CODE-NOW`+`MAC-DEV`. *Done:* emergency stop mid-session
  never leaves the host cursor hidden or dissociated (tested).
- ☐ **C2** `CORE`/`UI` — **Local-vs-remote input arbitration decision.** Inv-5 lease covers
  remote-vs-remote; choose the local-vs-remote policy (recommend AnyDesk-style: local physical input
  momentarily suppresses injected input) for attended simultaneous control. Needs an ADR if it touches
  the enforcement path. *Fixability:* `CODE-NOW` (design) then implement.
- ☐ **C3** `MED`/`UI` — **Host cursor *capture* + controller *render*** for the ADR-073 channel (the
  observer seam + wire exist; OS capture and controller draw are the on-device follow-up).
  *Fixability:* `MAC-DEV`/`LINUX-DEV`.

### B3 — Capture self-exclusion verification (mechanism already chosen)

- ☐ **X1** `MAC`/`WIN`/`LINUX` `MED` — verify the Inv-7 "secure window" exclusion on device: macOS
  `SCContentFilter`/`NSWindowSharingNone` actually drops our overlay from the outbound stream; Windows
  `WDA_EXCLUDEFROMCAPTURE` on our own HWNDs confirmed against **both** WGC and DXGI (doc doesn't
  enumerate them); Linux remains a documented no-op. *Fixability:* `MAC-DEV` now, `WIN-HW` later, Linux
  n/a.

### B4 — Video / encode tuning (research-driven)

- ☐ **V1** `MED` — **OpenH264 realtime tuning pass**: confirm constrained-baseline (`avc1.42E0xx`),
  no B-frames, CBR/ABR at negotiated target, and add a dropframe-under-congestion behavior analogous to
  libvpx `rc_dropframe_thresh`. Validate ABR delay/loss bands **on device**. *Fixability:* `CODE-NOW` +
  `LINUX-DEV` verify.
- ☐ **V2** `MED` — **HW-encoder matrix** (VideoToolbox verified on macOS; NVENC/QSV/AMF + VAAPI still
  ☐ — mirrors `docs/17` Phase 6). *Fixability:* `MAC-DEV` (VT), `WIN-HW`/`LINUX-DEV` (rest).

### B5 — Distribution / trust (funding + hardware gated)

- ☐ **D1** `INF` — **EV / OS code-signing + notarization** (Gatekeeper/SmartScreen). *Fixability:*
  `$FUND` (ADR-072).
- ☐ **D2** `INF` — **Activate signed auto-update** (wired but inert, ADR-078): generate key, provision
  secrets, flip flag; verify a real signed download+install+relaunch on device. *Fixability:* `CODE-NOW`
  (activation) + `MAC-DEV` verify.
- ☐ **D3** `WIN` — **Windows on-device bring-up** (never run at all): input (`SendInput`), capture (WGC),
  file-write (`CreateFileW`/`CREATE_NEW`), secure-window (`WDA_EXCLUDEFROMCAPTURE`). *Fixability:*
  `WIN-HW` (blocked on hardware we lack).

### B6 — Follow-ups already flagged in `CLAUDE.md §3` (lower priority)

- ☐ **F1** `SEC` — SQLite durable store for `PairingRegistry` + wire the pairing decision into
  connect/consent + host-displayed QR (unattended-access unlock, ADR-084/085).
- ☐ **F2** `UI` — App panels for the landed-but-inert features: chat panel, "Send clipboard" +
  indicator, file-transfer confirmation UI, monitor picker (ADR-076/082/086/081).
- ☐ **F3** `MED` — Audio on-device: OS capture (SCK-audio/WASAPI-loopback/PipeWire), "AUDIO SHARED"
  indicator, JS `AudioDecoder`→`AudioContext` playback (ADR-077/080).
- ☐ **F4** `SEC` — macOS global-hotkey emergency stop (baseline Stop button already drives
  `revoke_all`+`release_all`).
- ☐ **F5** `SEC` — `keyboard.text` app IME wiring + rate bound (ADR-083); clipboard grant enablement.

---

## How to use this tracker

1. Pick an item; if it touches a security boundary / wire / priority ordering, **write the ADR first**
   (next free number after ADR-098) per `CLAUDE.md §9`.
2. Implement behind the existing DI seam (`OsInputSink`, `SessionTransport`, etc.) — don't widen the
   surface.
3. Flip ☐→◐ when building, ☐/◐→☑ **only after in-environment verification** (Inv 17).
4. On ☑: bump `CLAUDE.md §3`, tick the mirrored row in `docs/17`/`docs/21`, run the full gate
   (`fmt`/`clippy -D warnings`/`test --all`/`cargo deny check`).

**Cross-refs:** `docs/17` (roadmap) · `docs/19` (cross-platform host research) · `docs/20` (feature gaps)
· `docs/21` (production-readiness backlog) · `docs/14` (ADR log).
