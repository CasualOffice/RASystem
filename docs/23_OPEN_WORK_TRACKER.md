# 23 — Open Work Tracker (post-v0.0.3-alpha)

> Living tracker for the remaining feature gaps, tech debt, and engineering backlog surfaced after the
> v0.0.3-alpha draft. Grading follows Inv 17 / `docs/22`: **☑ = in-environment verified**, **◐ =
> compile/cross-compile/loopback only (off-device)**, **☐ = not started**. Every item carries a
> **fixability** tag so it's honest about what unblocks it:
>
> - `CODE-NOW` — buildable + verifiable off-device (compile/test/loopback) right now.
> - `DEVICE` — needs a real two-machine run or on-device profiling to build/verify correctly.
> - `BIG-NET` — large, networked, effectively unverifiable off-device (high blind-risk).
> - `HW` — needs hardware the team lacks (Windows).
> - `FUND` — needs money (certs) / external provisioning.
> - `FUTURE` — deliberately deferred (scope).
>
> Order within each part is rough priority (top = first).

## Part 2 — Remaining feature gaps (from issue #5)

| # | Item | Fixability | State | Notes |
|---|------|-----------|-------|-------|
| 2.1 | **Two-way annotation** (host → controller) | `CODE-NOW` | ◐ | DONE (off-device). `HostSession::send_annotation` + controller routes inbound `Annotate`→`RemoteAnnotation`; sharer annotation toolbar on the overlay + controller renders host strokes. Loopback-tested; on-device render pending. |
| 2.2 | **Multi-monitor cursor position** | `CODE-NOW`→`DEVICE` | ☐ | Observers normalize over the primary/root display; feed `CaptureGeometry` bounds so a secondary monitor maps right. Code off-device; true multi-monitor is on-device. |
| 2.3 | **Presence / online-dots + "call" a contact** (gossip Phase B/C) | `BIG-NET` | ☐ | Unblocked by ADR-098's always-on endpoint. `ras-signal` engine built; needs gossip wiring + AccessRequestIntent prompt. Unverifiable off-device. |
| 2.4 | **Video lag** (constant offset, Linux host) | `DEVICE` | ☐ | Software OpenH264 encoder (no Linux HW encoder). Needs on-device profiling to attack; guessing regresses. |

## Part 3 — Tech debt

| # | Item | Fixability | State | Notes |
|---|------|-----------|-------|-------|
| 3.1 | **macOS cursor-observer consolidation** | `CODE-NOW` | ◐ | DONE (host-verified). Complete observer moved into `ras-cursor-macos`; `ras-media-macos/cursor.rs` deleted + deps trimmed; app rewired to `ras_cursor_macos::MacCursorObserver`. Workspace clippy + app check green. |
| 3.2 | **Windows cursor position** | `CODE-NOW` (cross-compile) | ◐ | DONE (CI-gated). `Moved` via `GetCursorPos` normalized over the virtual desktop (negative-origin-aware), shape-wins-then-Moved like Linux. Parse-clean + reviewed vs windows-rs 0.58; native compile is on `windows-latest` (ring blocks macOS cross-compile). |
| 3.3 | **libei Wayland input** (unprivileged upgrade) | `BIG-NET` | ☐ | Replaces the `/dev/uinput` udev requirement (reis + ashpd portal). Intricate async handshake; unverifiable off-device. Upgrade from the shipped uinput path. |

## Part 4 — Engineering backlog

| # | Item | Fixability | State | Notes |
|---|------|-----------|-------|-------|
| 4.1 | **Capture stop/restart thread + resource lifecycle** (task #15) | `CODE-NOW`→`DEVICE` | ◐ | DONE (off-device). `ScapCapture::stop` (Linux/Windows) now bounded-joins the capture thread (1.5s) instead of unconditionally detaching it, so `stop_capture()` runs and the OS session releases before the next `start()` — was a real leak-on-fast-restart risk. macOS's `stop_capture_blocking` was already correct (SCK completion handler + `recv_timeout`). 3 new unit tests on the extracted pure timeout logic. On-device: confirm no duplicate-portal-prompt on a real Linux stop→restart cycle. |
| 4.2 | **Concurrent per-frame stream drain** (task #24, receiver-side HOL) | `BIG-NET` | ◐ | DONE (off-device). `VideoSource` drains up to 8 per-frame QUIC streams concurrently (`tokio::task::JoinSet`), strictly frame_id-ordered delivery, a time-based `GAP_GRACE` (120ms) distinguishes a slow concurrent read from a real loss (a count-based watch deadlocks under real loss — caught + fixed pre-landing). 28 tests, repeated for flakiness. On-device: `GAP_GRACE` needs tuning against real WAN/lossy-network RTT/reordering. |
| 4.3 | **Windows on-device run** (task #18) | `HW` | ☐ | Blocked: team has no Windows hardware. CI-compile-gated only. |
| 4.4 | **Adaptive codec / SVC (VP8/VP9)** for low-bandwidth (task #21) | `FUTURE` | ☐ | Deliberately deferred; H.264/WebCodecs is the current coherent path. |

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
