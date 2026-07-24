# 23 — Open Work Tracker (post-v0.0.3-alpha)

> Living tracker for the remaining feature gaps, tech debt, and engineering backlog surfaced after the
> v0.0.3-alpha draft. Grading follows Inv 17 / `docs/22`: **☑ = in-environment verified**, **◐ =
> compile/cross-compile/loopback only (off-device)**, **☐ = not started**. Every item carries a
> **fixability** tag so it's honest about what unblocks it:
>
> - `CODE-NOW` — buildable + verifiable off-device (compile/test/loopback) right now.
> - `DEVICE` — needs a real two-machine run or on-device profiling to build/verify correctly.
> - `BIG-NET` — large, networked, effectively unverifiable off-device (high blind-risk).
> - `HW` — needs hardware the team lacks (none currently — Windows hardware access was confirmed
>   2026-07; kept as a tag for any future case).
> - `FUND` — needs money (certs) / external provisioning.
> - `FUTURE` — deliberately deferred (scope).
>
> Order within each part is rough priority (top = first).

## Part 2 — Remaining feature gaps (from issue #5)

| # | Item | Fixability | State | Notes |
|---|------|-----------|-------|-------|
| 2.1 | **Two-way annotation** (host → controller) | `CODE-NOW` | ◐ | DONE (off-device). `HostSession::send_annotation` + controller routes inbound `Annotate`→`RemoteAnnotation`; sharer annotation toolbar on the overlay + controller renders host strokes. Loopback-tested; on-device render pending. |
| 2.2 | **Multi-monitor selection + cursor position** | `CODE-NOW`→`DEVICE` | ◐ | DONE (off-device) on **macOS + Windows**. macOS: real `enumerate_displays`/`captured_display`, a Share-view picker (`list_displays`/`select_display`), `HostSessionConfig` threaded with the real choice. **Bug found + fixed**: `start()` was matching the picked id against a raw array index into the unsorted `SCShareableContent.displays()`, while `enumerate_displays()` reports the real `CGDirectDisplayID` (sorted primary-first) — picking a non-primary display silently captured the wrong one or fell back to primary every time; `start()` now matches on the real id (`find_display_by_id`). **Windows**: real per-monitor enumeration + selection now wired in `ras-media-scap` — `CaptureOptions.monitor` threads through to the capture thread and resolves against a fresh `scap::get_all_targets()` (`Target` is `!Send` on Windows, so the lookup runs on the consuming thread), `enumerate_displays()` returns real geometry via `GetMonitorInfoW`+`GetDpiForMonitor` (scap's own geometry helpers are private to that crate, so this reimplements the same GDI calls). **Linux stays an honest single "Display 1" fallback by design, not by gap**: reading scap's source confirmed `get_all_targets()` always returns empty there — the xdg-desktop-portal picks interactively at capture start, not programmatically, so there is no API a picker could act on; building one would silently do nothing. On-device: a real secondary-monitor pick + cursor-position check on both macOS and Windows. |
| 2.3 | **Presence / online-dots** (gossip) | shipped | ☑ | Landed earlier this session (always-on endpoint + gossip, three-state dots) — see the July 23 entries above. |
| 2.3b | **"Call" a contact** | `CODE-NOW`→`DEVICE` | ◐ | DONE (off-device). Discovered `SignalPayload::AccessRequestIntent` (ADR-095) already existed fully at the protocol layer with a NO-OP receiving arm in the app — wired it instead of building new: `call_contact` command, a real local-attention prompt (unminimize/focus/notify) on receipt, a one-click "Call" button per contact, a dismissible global banner whose "Share now" only navigates to Share (no consent bypass, Inv 1/9 untouched). On-device: a real two-machine call→share round trip. |
| 2.4 | **Video lag** (constant offset, Linux host) | `DEVICE` | ☐ | Software OpenH264 encoder (no Linux HW encoder). Needs on-device profiling to attack; guessing regresses. |

## Part 3 — Tech debt

| # | Item | Fixability | State | Notes |
|---|------|-----------|-------|-------|
| 3.1 | **macOS cursor-observer consolidation** | `CODE-NOW` | ◐ | DONE (host-verified). Complete observer moved into `ras-cursor-macos`; `ras-media-macos/cursor.rs` deleted + deps trimmed; app rewired to `ras_cursor_macos::MacCursorObserver`. Workspace clippy + app check green. |
| 3.2 | **Windows cursor position** | `CODE-NOW` (cross-compile) | ◐ | DONE (CI-gated). `Moved` via `GetCursorPos` normalized over the virtual desktop (negative-origin-aware), shape-wins-then-Moved like Linux. Parse-clean + reviewed vs windows-rs 0.58; native compile is on `windows-latest` (ring blocks macOS cross-compile). |
| 3.3 | **libei Wayland input** (unprivileged upgrade) | `BIG-NET` | ◐ | DONE (off-device). `ras-input-linux::libei` — a third `OsInputSink` over the XDG Desktop Portal RemoteDesktop interface (`reis` for the libei/EIS wire protocol + `ashpd` for the D-Bus portal handshake, both MIT), replacing the `/dev/uinput` udev group requirement with the unprivileged portal path. Runs on a dedicated OS thread with its own single-threaded Tokio runtime, bridging the sync `OsInputSink` calls to the async portal/libei handshake. **Regression caught and fixed before landing**: `best_input_sink()` originally preferred libei the instant its thread spawned (`has_session()`), true almost immediately regardless of whether a real RemoteDesktop portal exists — so any Linux box without one would always pick libei, always fail, and never fall back to the working uinput/XTEST path. Fixed with a bounded `probe_available` (400ms) that waits for a real ready/failed signal before preferring libei over the proven fallback. Cross-compile/clippy-clean for `x86_64-unknown-linux-gnu`, unit-tested (including the probe timing itself). On-device: the real portal consent dialog + a live Wayland compositor round trip. |

## Part 4 — Engineering backlog

| # | Item | Fixability | State | Notes |
|---|------|-----------|-------|-------|
| 4.1 | **Capture stop/restart thread + resource lifecycle** (task #15) | `CODE-NOW`→`DEVICE` | ◐ | DONE (off-device). `ScapCapture::stop` (Linux/Windows) now bounded-joins the capture thread (1.5s) instead of unconditionally detaching it, so `stop_capture()` runs and the OS session releases before the next `start()` — was a real leak-on-fast-restart risk. macOS's `stop_capture_blocking` was already correct (SCK completion handler + `recv_timeout`). 3 new unit tests on the extracted pure timeout logic. On-device: confirm no duplicate-portal-prompt on a real Linux stop→restart cycle. |
| 4.2 | **Concurrent per-frame stream drain** (task #24, receiver-side HOL) | `BIG-NET` | ◐ | DONE (off-device). `VideoSource` drains up to 8 per-frame QUIC streams concurrently (`tokio::task::JoinSet`), strictly frame_id-ordered delivery, a gap-detection grace window distinguishes a slow concurrent read from a real loss (a count-based watch deadlocks under real loss — caught + fixed pre-landing). The grace window is now **adaptive** (`adaptive_gap_grace`, 2.5× the live QUIC RTT sampled off the connection, clamped 60–400ms) instead of the fixed 120ms it shipped with, so it self-tunes to the real link instead of needing hand-tuning against WAN RTT. 28+ tests, repeated for flakiness. On-device: confirm the clamp bounds hold under real lossy/high-RTT WAN conditions. |
| 4.3 | **Windows on-device run** (task #18) | `HW`→`DEVICE` | ◐ | The team now has Windows hardware access (confirmed 2026-07-24) and has run the app on it — the Share/Connect two-machine flow worked, with the same bugs seen cross-platform as on macOS/Linux (now fixed). Windows is no longer purely CI-compile-gated; it's a normal on-device-verification target like macOS/Linux going forward. |
| 4.4 | **Adaptive codec / SVC (VP8/VP9)** for low-bandwidth (task #21) | `CODE-NOW`→`DEVICE` | ◐ | DONE (off-device). No longer deferred — VP9 3-layer temporal SVC in `ras-media-vpx` (encoder capability, 15 tests) fully wired to `LatencyFirstAbr` (a second, independent bandwidth response alongside bitrate, deliberately more conservative thresholds so it only sheds under genuinely bad conditions, 5 new ABR tests) end-to-end through `HostInner`→`MediaSignals`→`media_pump`, exactly mirroring the existing bitrate-ABR plumbing. No-op for H.264 (trait default). On-device: real-network tuning of the shed thresholds against actual observed bandwidth variance. |

## Solve order (this pass)

Solving the `CODE-NOW` items first (buildable + verifiable off-device), gating hard + verifying on-disk
before each push (lesson from the cursor workflow that mis-placed observers):

1. **3.1** macOS cursor-observer consolidation (host-verifiable cleanup).
2. **3.2** Windows cursor position (cross-compile-verifiable).
3. **2.2** Multi-monitor cursor bounds plumbing (feed capture geometry to the observers).
4. **2.1** Two-way annotation (codec + core loopback tests).

`DEVICE` / `BIG-NET` / `HW` / `FUND` / `FUTURE` items are queued behind the on-device test + explicit
go-aheads (presence/call and libei are the big ones; both need a device in the loop to de-risk).

## Input + cursor rollback (v0.0.4-alpha, ADR-100)

On-device testing showed the soft-cursor + sharer-annotation direction (2.1 host-side draw) *regressed*
the experience — reverted to the simple proven model:

| Symptom (on-device) | Root cause | Fix | State |
|---|---|---|---|
| White screen on Mac; hidden context-menu/files | Sharer-annotation made the overlay opaque + interactive | Removed sharer annotation; overlay always transparent + click-through | ◐ (fixed, on-device pending) |
| Confusing multi-cursor | Client soft-cursor overlay | One cursor baked into the video (`showsCursor=true`); soft-cursor unwired | ◐ |
| Clicks intermittent; keyboard dead | Touch/tap model — click didn't focus the target app | Continuous cursor-follow (controller-side) | ◐ |
| Double-click / drag broken (macOS) | No `kCGMouseEventClickState`; `MouseMoved` during a hold | `advance_click_count` + `motion_kind` (`*MouseDragged`), **validated vs enigo/RustDesk** | ◐ |
| Cursor could vanish / owner locked out | macOS warp hide+dissociate (PR #10314) vs baked cursor | Removed the hide/dissociate (Inv 1/4); `begin_warp`/`end_warp` = no-op seams | ◐ |
| Lag (constant offset, Linux viewer) | Decoder tolerated ~100 ms backlog | Guard 6→3 (~50 ms) + `q` (decode-queue depth) in the HUD for profiling | ◐ + `DEVICE` |

**2.1 note:** the *viewer→host* annotation + remote pointer are kept; only the *host→controller* (sharer)
annotation was removed. All the above are compile + macOS-unit verified; the live two-machine run is the
on-device confirmation (the `q` HUD number is the lag diagnostic).
