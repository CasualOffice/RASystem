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
| 2.1 | **Two-way annotation** (host → controller) | `CODE-NOW` | ☐ | Mirror of ADR-097 (controller→host exists). Bidirectional `Annotate`; testable at codec + core loopback. |
| 2.2 | **Multi-monitor cursor position** | `CODE-NOW`→`DEVICE` | ☐ | Observers normalize over the primary/root display; feed `CaptureGeometry` bounds so a secondary monitor maps right. Code off-device; true multi-monitor is on-device. |
| 2.3 | **Presence / online-dots + "call" a contact** (gossip Phase B/C) | `BIG-NET` | ☐ | Unblocked by ADR-098's always-on endpoint. `ras-signal` engine built; needs gossip wiring + AccessRequestIntent prompt. Unverifiable off-device. |
| 2.4 | **Video lag** (constant offset, Linux host) | `DEVICE` | ☐ | Software OpenH264 encoder (no Linux HW encoder). Needs on-device profiling to attack; guessing regresses. |

## Part 3 — Tech debt

| # | Item | Fixability | State | Notes |
|---|------|-----------|-------|-------|
| 3.1 | **macOS cursor-observer consolidation** | `CODE-NOW` | ☐ | Observer lives in `ras-media-macos` (dup name w/ dead `ras-cursor-macos`); move to `ras-cursor-macos` for symmetry with Linux/Windows. Host-compilable → verifiable. |
| 3.2 | **Windows cursor position** | `CODE-NOW` (cross-compile) | ☐ | `ras-cursor-windows` is shape-only; add `Moved` via `GetCursorPos` (parallel to the Linux XFixes / macOS mouseLocation work). |
| 3.3 | **libei Wayland input** (unprivileged upgrade) | `BIG-NET` | ☐ | Replaces the `/dev/uinput` udev requirement (reis + ashpd portal). Intricate async handshake; unverifiable off-device. Upgrade from the shipped uinput path. |

## Part 4 — Engineering backlog

| # | Item | Fixability | State | Notes |
|---|------|-----------|-------|-------|
| 4.1 | **Capture stop/restart thread + resource lifecycle** (task #15) | `CODE-NOW`→`DEVICE` | ☐ | Clean teardown/re-init of the capture thread across stop→restart; partly host-verifiable (macOS). |
| 4.2 | **Concurrent per-frame stream drain** (task #24, receiver-side HOL) | `BIG-NET` | ☐ | Viewer drains per-frame QUIC uni-streams serially → a stalled frame HOL-blocks the arrived ones. Real latency win, but networked/unverifiable. |
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
