# Phase 3 — On-Device Verification Checklist (macOS CGEvent input → M4)

> **Purpose.** Everything in Phase 3 that a CI machine cannot exercise: real CGEvent injection, the
> PostEvent-TCC prompt, the Secure-Input drop, multi-monitor coordinate mapping, and the wall-clock
> emergency-stop deadline. The **logic** (lease / generation / seq / capability gate, release-on-stop)
> is already covered green by the pure `ras-control` / `ras-protocol` tests; this file closes the
> **on-device rows** of the §11 security-test matrix in `phase-3-design.md`.
>
> Run this on a macOS login session (not SSH / not a LaunchDaemon — TCC needs a real user session).
> Tick each box and record the evidence noted. A single ✗ blocks the M4 on-device sign-off.

---

## 0. Preconditions

- [ ] Two machines (or two user sessions): a **Share** host on macOS and any **Connect** viewer.
      Share is macOS-only; Connect is cross-platform.
- [ ] Build the app bundle on the host Mac: `cd app && npm run tauri build` (or the debug `.app`).
      Running from a real `.app`/login session is required — TCC does not prompt for a raw `cargo run`
      binary reliably.
- [ ] Host Mac has **Screen Recording** already granted to the app (Phase-1/2 step). Confirm capture
      works view-only first (that isolates any failure here to the *input* path, not capture).
- [ ] **PostEvent access NOT yet granted** for the app (System Settings ▸ Privacy & Security ▸
      *Accessibility is the wrong bucket* — we use the PostEvent/"controlling your computer" bucket).
      Start from the un-granted state so step 2 exercises the prompt honestly.

> Note the TCC bucket: this backend preflights **`CGPreflightPostEventAccess`** and requests
> **`CGRequestPostEventAccess`** — *not* Accessibility, and never UIAccess (Inv 14, ADR-055). If the
> OS surfaces the request under an "Accessibility"-labelled toggle, that is the system's labelling;
> confirm the app is the one being toggled.

---

## 1. Lease consent gate (Inv 1 — the second consent)

- [ ] Connect (viewer) starts a session and clicks **Take control**.
- [ ] The host shows a **separate** control-lease consent panel (distinct from the Phase-2 connect
      consent). Viewing was already live; this prompt is only about *input*.
- [ ] Click **Deny** → the viewer's "Take control" does not engage; no pointer/key events reach the
      host OS. Expected host lifecycle: `ControlLeaseEnded` / no `ControlLeaseGranted`.
- [ ] Repeat, click **Allow** → host emits `ControlLeaseGranted`; the viewer's button shows the armed
      state.
- [ ] Let a control request sit **90 s** with no response → it auto-denies fail-closed (no lease).

**Verifies:** Inv 1 (local user authorizes input, controller cannot self-authorize); the two-tier
grant→lease model (viewing ≠ controlling).

---

## 2. PostEvent TCC prompt & fail-closed refusal

- [ ] On the **first** Allow (from step 1), macOS shows the system PostEvent permission prompt.
- [ ] **Decline** it. Expected: the lease is refused with a clear reason (`input_permitted()` returns
      `false` → host refuses); **no silent no-op injection** — the viewer is told, not left thinking
      it has control. Record the host reason string shown/logged.
- [ ] Grant the permission (System Settings toggle), re-request control, Allow → `input_permitted()`
      now `true`, lease issues.

**Verifies:** §11 row "Ungranted PostEvent TCC → lease refused, no silent no-op"; fail-closed
`OsInputSink::input_permitted`.

---

## 3. Pointer & keyboard injection (Inv 6 — narrow surface)

With a live control lease and PostEvent granted:

- [ ] Move the viewer's pointer over the shared video → the **host** cursor tracks it. Sanity-check
      the mapping: viewer's top-left corner → host's top-left; center → center; bottom-right →
      bottom-right (normalized `u16 → [0,1] →` display points).
- [ ] Left-click a host UI element (e.g. focus a text field) → it responds on the host.
- [ ] Right-click → host context menu appears. Middle-click where observable.
- [ ] Type ASCII letters/digits → they appear in the focused host field. Try a shortcut with a
      modifier (e.g. ⌘A select-all) → the modifier flag is applied (physical-key path, HID→virtual
      keycode + `CGEventFlags`).
- [ ] Scroll (wheel) over a scrollable host view → it scrolls; direction matches (dy vertical).

**Verifies:** the CGEvent injection path end-to-end; HID→virtual-keycode table; modifier-bit mapping;
the closed action set only (no shell/path/keysym ever crosses — Inv 6).

---

## 4. Secure-Input drop (the honest boundary)

- [ ] On the host, focus a **password field** (login window, Keychain prompt, or a browser password
      box that asserts Secure Input).
- [ ] From the viewer, attempt to type into it → **nothing is injected** into the secure field.
- [ ] Confirm this is surfaced honestly (not presented as success). This is the documented fraud-model
      boundary (docs/18 §0), not a bug.

**Verifies:** Inv 14 (no secure-desktop bypass); the deliberately-unprivileged PostEvent posture.

---

## 5. Emergency stop within the deadline (Inv 4 — load-bearing)

- [ ] Hold a key **down** on the viewer (e.g. press-and-hold a letter, or a modifier) so the host has
      a key logically down, and start a continuous pointer drag.
- [ ] Hit the always-visible **Stop** button on the host **mid-input**.
- [ ] Observe: injection ceases; the held key/button is **released** (no stuck key — the drag stops,
      the modifier clears); the viewer's subsequent input no longer reaches the host OS.
- [ ] Time it: the stop-to-input-halt should be **≤ 250 ms** locally. (Coarse stopwatch / screen
      recording is acceptable evidence for the alpha.)
- [ ] After stop, re-check: the viewer sending more `Input` gets it dropped (generation bumped →
      `StaleGeneration`); the host is not injecting.

**Verifies:** Inv 4 (emergency stop overrides grant/lease/in-flight input, ≤250 ms); `revoke_all` +
`release_all` on stop. Also exercises the **best-effort `release_all`** fix (commit `7ce9f94`) — the
key-state cleanup must clear *every* held key even if an individual `CGEventSource` hiccups; confirm
no key stays stuck after stop.

---

## 6. Transfer / disconnect key-state cleanup

- [ ] With a key held down via the lease, **disconnect** the viewer abruptly (close its window / kill
      the network). Expected: the host runs `release_all` on teardown → no stuck keys/buttons.
- [ ] (If a second viewer is available) transfer control to it → the first holder's in-flight input is
      dropped (`StaleGeneration`), and any keys it held are released before the new lease is active.

**Verifies:** §11 "Keys left down on transfer/disconnect/stop → no stuck key"; Inv 5 (one OS-input
controller at a time).

---

## 7. Multi-monitor coordinate mapping

- [ ] On a host with **two displays**, share the **secondary** (non-primary) monitor.
- [ ] Confirm the pointer overlay covers exactly the shared display (from `captured_bounds` /
      `CaptureGeometry`), and injected pointer coordinates land on the **shared** display, not the
      primary. Corners map correctly (no offset by the primary's origin).
- [ ] Change the display arrangement mid-session → coordinates computed against the old
      `layout_version` are dropped (`StaleLayout`) until the viewer picks up the new geometry.

**Verifies:** multi-monitor `to_point` mapping; the `layout_version` freshness gate; the non-finite
coordinate guard (commit `7ce9f94`) is defense-in-depth here (never triggers on the real path).

---

## 8. Secret-hygiene spot check (Inv 8)

- [ ] Run the host with `RUST_LOG`/tracing at its most verbose and drive a full control session
      (type some text, use modifiers).
- [ ] `grep` the logs / any crash dump for the typed text, key values, and pointer coordinates →
      **none present**. Only content-free lifecycle events (`ControlLeaseGranted{generation}`,
      `InputRejected{code}`, …) should appear.

**Verifies:** Inv 8 (typed text / key values / pixels never logged). This is a grep gate, runnable
here and worth re-confirming on-device once real injection is flowing.

---

## Sign-off

- [ ] All boxes above ticked, evidence captured (screen recording for §5 timing; log grep for §8).
- [ ] Any deviation filed as an issue and linked from `docs/17` Phase 3 ③.
- [ ] On green: flip the Phase 3 / M4 status in `CLAUDE.md §3` and `docs/17` from "app + on-device
      verification pending" to **on-device verified**, and mark the §11 on-device rows done.

> Still explicitly **out of scope** for this checklist (tracked separately in `docs/17`): the macOS
> global-hotkey emergency stop (the always-visible Stop button is the verified baseline here), and the
> Windows input backend (needs Windows hardware).
