# Phase 3 — On-Device Verification Checklist (Linux XTEST input → M4)

> The Linux analogue of `phase-3-on-device-verification.md` (macOS). Everything in the Linux input
> path a CI machine cannot exercise: real XTEST injection, the X11-vs-Wayland reach, and the
> emergency-stop wall clock. The **logic** (lease/generation/seq/capability gate, release-on-stop) is
> already green in the pure `ras-control` tests, and the `ras-input-linux` crate + its pure-logic
> tables cross-compile-check + unit-test clean. This file closes the **on-device rows** on your Linux
> box (**ADR-070**, `docs/19 §3`).
>
> Run on an **X11 or Xwayland** login session. Tick each box. A single ✗ blocks the Linux on-device
> sign-off.

---

## 0. Preconditions & the X11/Wayland reach (read first)

- [ ] Build the app on Linux: `cd app && npm run tauri build` (needs the WebKitGTK 4.1 + PipeWire dev
      stack — see `docs/19 §6`). A Share host on Linux + any Connect viewer.
- [ ] **Know your session type:** `echo $XDG_SESSION_TYPE`.
      - `x11` → XTEST drives the whole desktop. Full pass expected.
      - `wayland` → the backend connects to **Xwayland**, so injected input reaches **X11 clients
        only**, not native-Wayland windows. This is the documented v1 limit (the `uinput`/libei
        follow-ups fix it). Verify against an X11 app (e.g. an xterm) and note the Wayland gap.
- [ ] Confirm screen capture works view-only first (isolates any failure to the *input* path).

---

## 1. Automated pointer self-check (no human eye)

- [ ] Run: `cargo run -p ras-input-linux --example pointer_roundtrip` from a terminal in the session.
      It injects a move to the screen centre through the real `X11InputSink` and reads the cursor back
      via `QueryPointer`, asserting it landed within 2 px.
      - Exit `0` = injection verified (pointer mapping row closed mechanically).
      - Exit `2` = no reachable X server (headless / pure-Wayland with no Xwayland) — expected there;
        it confirms the **fail-closed** branch (`input_permitted()` false ⇒ host refuses the lease).
      - Exit `1` = a real bug (cursor didn't land).

**Verifies:** the XTEST pointer path end-to-end + the fail-closed permission contract.

---

## 2. Lease consent gate (Inv 1) — shared with macOS §1

- [ ] Viewer clicks **Take control** → host shows the **second**, input-specific consent panel.
- [ ] **Deny** → no pointer/key events reach the host. **Allow** → `ControlLeaseGranted`; button arms.
- [ ] Leave a request unanswered 90 s → auto-denies fail-closed.

**Verifies:** Inv 1 (the local user authorizes input; viewing ≠ controlling).

---

## 3. Pointer & keyboard injection (Inv 6)

With a live lease (on an X11 session, or against an X11 app under Xwayland):

- [ ] Move the viewer's pointer over the shared video → the host cursor tracks it; corners map right
      (normalized `u16 → [0,1] →` root-window pixels).
- [ ] Left / right / middle click land on the host.
- [ ] Type ASCII letters/digits into a focused host field; a modifier shortcut (e.g. Ctrl+A) applies
      the modifier (held-modifier reconciliation → the physical modifier keycode is held around the
      key). `release_all` must leave no modifier stuck.
- [ ] Scroll (wheel) over a scrollable host view; direction matches (buttons 4/5 vertical, 6/7
      horizontal).
- [ ] Confirm **`TextInput` is refused** (the `keyboard.text` cap is withheld by
      `phase3_default_policy`; XTEST v1 returns InputFailed rather than mis-typing).

**Verifies:** the XTEST injection path; HID→evdev(+8) keycode map; the closed action set only (Inv 6).

---

## 4. Emergency stop within the deadline (Inv 4) — shared with macOS §5

- [ ] Hold a key/modifier down on the viewer + start a continuous drag; hit the host **Stop**
      mid-input.
- [ ] Injection ceases; held keys/buttons are **released** (no stuck key — exercises the best-effort
      `release_all`); further viewer input is dropped (generation bumped → `StaleGeneration`).
- [ ] Stop-to-halt ≤ **250 ms** (stopwatch / screen recording is acceptable evidence).

**Verifies:** Inv 4 (emergency stop overrides grant/lease/in-flight input); `revoke_all` + `release_all`.

---

## 5. Secret hygiene (Inv 8) — shared with macOS §8

- [ ] Run the host at max `RUST_LOG`/tracing, drive a full control session (type text, use modifiers),
      then `grep` the logs / any crash dump for the typed text, key values, and coordinates →
      **none present**. Only content-free events (`ControlLeaseGranted{generation}`, `InputRejected{code}`).

---

## Sign-off

- [ ] Boxes ticked; §1 exit `0`; §4 timing + §5 grep captured.
- [ ] On green: mark the Linux on-device rows done and note the session type tested (x11 vs Xwayland).

> **Out of scope for this checklist** (tracked in `docs/19 §3`): native-Wayland reach (needs the
> `uinput` privileged-helper and/or the `ashpd`+`reis` libei consented path), and unattended/greeter
> access (needs DRM/KMS + a privileged helper — a deliberately separate, audited posture, not the
> interactive default).
