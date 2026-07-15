# Phase 3 Design — Remote Control & Collaboration (→ M4)

> Scope: PHASE 3 = **safe OS input.** A short-lived, generation-versioned **control lease**; a
> **single OS-input controller at a time** (Inv 5); **per-message, host-side capability + lease +
> generation enforcement** on every injected event (Inv 15, ADR-041 — the RustDesk-CVE fix); a
> **virtual multi-cursor** relay for non-lease participants (visual only, never injected); a real
> **OS input backend** (macOS-lead: CGEvent, unprivileged per-user agent, ADR-055); and the
> **emergency-stop / transfer / disconnect key-state cleanup** path (Inv 4, 14). Input **only** — no
> clipboard, no file transfer, no support-action catalogue (those are later phases). Assurance stays
> **Tier 0**; the hash-chained audit journal (ADR-042) is still Phase 4.
>
> Priority order is **STRICT: Security (1) > Latency (2) > UX (3)**. Input is the sharpest-edged
> capability in the product; where the enforcement path and latency/UX conflict, enforcement wins —
> but the per-message check is an **O(1) set/counter lookup off the video path**, so the cost is a
> handful of nanoseconds per event, never a frame stall (§4).
>
> This is a **design document**: code blocks are compile-*conceptual* Rust with `todo!()` bodies,
> dependency-light. They are the source for the crate trait skeletons. **No implementation lands
> until execution is approved** (CLAUDE.md §9).
>
> **Load-bearing invariants honored throughout** (`CLAUDE.md §5`):
> - Emergency stop overrides **everything** — grant, lease, policy, in-flight input — within ≤250 ms
>   locally (Inv 4). It is never gated behind a lease or capability check.
> - **One active OS-input controller at a time by default** (Inv 5). Everyone else is a *virtual*
>   cursor that **cannot** inject.
> - The input sink accepts **only a narrow, validated set of normalized commands** — never shell
>   commands, executable paths, OS-API names, raw network objects, or controller-supplied file paths
>   (Inv 6).
> - Capability scope is enforced **per message, host-side** — never trust the controller's claim
>   (Inv 15, ADR-041). Every injected event re-checks capability **and** lease **and** generation.
> - **Never build a secure-desktop/UAC input-injection bypass; never request UIAccess** (Inv 14). On
>   macOS injection is deliberately **unprivileged** so it cannot bypass Secure Input (ADR-055).
> - Unknown capabilities are **denied**, never defaulted-on (Inv 2). The input caps
>   (`pointer.move/click/scroll`, `keyboard.key/text`, `control.request/transfer`) are already
>   *recognized-but-withheld* in the catalogue (`ras-policy` `CATALOGUE_V1`); Phase 3 makes them
>   *grantable* behind a lease, not defaulted-on.
> - Secrets never touch logs (Inv 8): typed text, key values, and clipboard are never logged — input
>   payloads are content-bearing and must be redacted in every trace line.
>
> **Prior art this builds on (do not re-litigate):** ADR-041 (per-message capability enforcement),
> ADR-047 (no UIAccess; lean on the OS secure desktop), ADR-048 (SAS-bound emergency stop on
> Windows), ADR-055 (macOS input is an unprivileged per-user agent, PostEvent-TCC-gated), ADR-061
> (the visual `ControlMsg::Pointer`, non-OS-input), and the Phase-2 authorization spine (the grant
> already carries `granted_capabilities` and a `session_generation`; `GrantDecision::Authorized(caps)`
> already flows into the session). This gate **operationalizes** them; the three open choices are
> closed in **ADR-067/068/069** (§0).

---

## 0. What this gate decides (and what's already decided)

| Question | Resolution | Where |
|---|---|---|
| Is a lease a signed bearer token or host-authoritative live state? | **Host-authoritative live state.** The on-wire `ControlGranted` is host-signed for forward-compat (the later process split, S4), but per-message enforcement checks the host's **own** generation counter + active-lease table, never the controller's claimed scope (Inv 15). | **ADR-069 (new)**, §5 |
| Input wire shape | A dedicated `ControlMsg::Input(InputEnvelope)` carrying `{lease_id, generation, seq, action}`; the action is a nested oneof (`PointerMove/PointerButton/PointerWheel/KeyEvent/TextInput/ReleaseAllKeys`). Coordinates are **normalized fixed-point `0..=65535`** (matching ADR-061) + `display_id` + `display_layout_version`. Distinct from ADR-061's `Pointer` (which is visual-only, no lease). | **ADR-067 (new)**, §2, refines docs/04 §12 |
| OS input backend | New crate **`ras-input-macos`** (CGEvent via `core-graphics`/`objc2`), unprivileged, gated on `CGPreflightPostEventAccess` (**PostEvent** TCC, *not* Accessibility), Secure-Input-respecting. The `OsInputSink` trait lives in `ras-control` (pure, `unsafe`-free); `unsafe`/FFI is confined to the backend (CONTRIBUTING §5), mirroring `ras-media` / `ras-media-macos`. Linux/Windows backends deferred. | **ADR-068 (new)**, §3.2, §8 |
| Which channel carries input? | The **existing single bidi control stream** (reliable + ordered — clicks/keys must not drop or reorder). Distinct QUIC streams mean input never HOL-blocks video and vice-versa (ADR-060). | §2, §4 |
| Coordinate authority | The **host** maps normalized → device pixels using the capture geometry it already emits (`CaptureGeometry`, the multi-monitor overlay work). The controller never sends pixel coordinates (Inv 6). | §4, §8 |
| Emergency stop on macOS | There is **no** kernel SAS (Ctrl+Alt+Del) on macOS; ADR-048's SAS binding is Windows-specific. On macOS the stop is the always-visible indicator's **Stop** + an optional host-registered global hotkey; it revokes the lease, flushes `ReleaseAllKeys`, and halts injection ≤250 ms. Honest platform caveat, not a regression of Inv 4. | §7 |
| MVP collaboration surface | **One controller + virtual cursors.** The lease/generation machine is designed for N participants (latest-wins, rate-limited relay), but the shipped MVP tests a single OS-input lease-holder plus visual cursors. | §6 |

**ADR-067/068/069 are Proposed in this gate** and must be signed off before Phase-3 code lands. Their
full text is added to `docs/14_DECISIONS_ADR.md` alongside this doc.

---

## 1. Overview — the control-lease flow

Phase 2 ends with an **`Active`, view-only** session: the controller holds a grant whose
`granted_capabilities` are `{screen.view, screen.select_monitor, pointer.virtual, annotation.create}`
and a live `session_generation`. Phase 3 lets a controller **request the OS-input lease** on top of
that grant. The grant is the *coarse* authorization (may this endpoint ask for input at all); the
**lease** is the *fine, revocable, single-holder, generation-versioned* right to inject **right now**.

### 1.1 Flow diagram

```
 CONTROLLER (Tauri)                                     HOST (macOS-lead, one per-user agent — ADR-055)
 ┌───────────────────────────────────────┐             ┌──────────────────────────────────────────────┐
 │ Active session (Phase-2 grant, caps)   │             │ LeaseManager: active = None, generation = G    │
 │        │ user clicks "Request control"  │             │ policy: phase3_default_policy (adds input caps) │
 │        ▼                               │  control     │                                                │
 │ ControlMsg::ControlRequest{caps'}  ═══════════════►   │ ras-control: request()                          │
 │        │                               │  stream      │   ① grant carries control.request? (Inv 15)     │
 │        │                               │             │   ② caps' ⊆ grant.granted_capabilities          │
 │        │                               │             │   ③ LocalConsent: prompt human (Inv 1)          │
 │        │                               │             │      Allow / reduce / Deny                      │
 │ ControlMsg::ControlGranted{lease_id,   │             │   Allow → active = Lease{gen: G+1}, bump gen    │
 │   generation: G+1, caps'', expires} ◄══════════════   │   (any prior holder's gen G is now stale)       │
 │        │ hold lease_id + generation     │             │                                                │
 │        ▼   (per input event)            │             │                                                │
 │ ControlMsg::Input(InputEnvelope{        │  reliable    │ ras-control::authorize_input() — PER MESSAGE:   │
 │   lease_id, generation:G+1, seq:n,      │  ordered     │   ① generation == current?  (else StaleGen)     │
 │   action: PointerButton{..}}) ═══════════════════►    │   ② lease_id == active?     (else NoLease)      │
 │                                        │             │   ③ not expired?            (else LeaseExpired) │
 │                                        │             │   ④ seq strictly increasing? (else Replayed)    │
 │                                        │             │   ⑤ cap(action) ∈ active.caps? (Inv 15)         │
 │                                        │             │      all OK → OsInputSink.inject(normalized)    │
 │                                        │             │   ras-input-macos: normalized → points → CGEvent│
 │  ◄════════════ frames (unchanged) ═════════════════   │                                                │
 │  ⟵ EMERGENCY STOP (always) ⟶  bump generation, ReleaseAllKeys, halt inject ≤250 ms, Bye{Revoked}    │
 └───────────────────────────────────────┘             └──────────────────────────────────────────────┘
```

### 1.2 Prose walkthrough

1. **Request.** The lease-less controller sends `ControlRequest{capabilities}` on the control stream.
   The host first checks the **grant** actually carries `control.request` (Inv 15 — a controller that
   was only granted view-only can never escalate to input by asking); then that the requested input
   caps are a subset of the grant's `granted_capabilities`.
2. **Consent (Invariant 1).** Requesting **OS input** is a distinct, higher-stakes act than viewing,
   so it re-prompts the local human (§6 of Phase 2's consent contract, reused): who, what input caps,
   duration, always-present Stop. Deny/timeout ⇒ `ControlRevoked{ConsentDenied}`, no lease.
3. **Grant lease.** On Allow the host **bumps the session generation** (`G → G+1`) and installs the
   single active lease `{lease_id, generation: G+1, caps'', expires_at}`. Bumping the generation is
   what makes a *prior* holder's in-flight input instantly stale (Inv 5) — there is never a window
   where two generations are both valid.
4. **Inject (per message).** Each input event rides `ControlMsg::Input(InputEnvelope)` carrying the
   lease id, generation, a strictly-increasing `seq`, and the normalized action. The host runs the
   **five ordered per-message checks** (§4/§5) and, only if all pass, hands the *normalized* action to
   the `OsInputSink`, which maps it to device pixels and injects a CGEvent. The controller never sends
   pixels, key names as strings, or anything but the closed action set (Inv 6).
5. **Transfer.** A second controller's `ControlRequest` (with consent) **transfers** the lease:
   generation bumps again, the old lease is revoked, and a `ReleaseAllKeys` is injected on the host to
   clear any keys the departing controller left down (§7). Old-generation input arriving after the
   bump is rejected (`StaleGeneration`) — the M4 exit criterion.
6. **Emergency stop / disconnect / expiry.** Any of these bumps the generation, flushes
   `ReleaseAllKeys`, halts the input sink **before its next event**, and (for stop) drives the
   existing Phase-1 `Revoke → Revoked` edge. This overrides a valid lease and a valid grant (Inv 4).

---

## 2. Canonical types & crate homes

Everything wire-facing is protobuf in `proto/` (source of truth); the hand-rolled enum stays the
public API and the codec maps between them (as in Phase 2). New/extended types:

### 2.1 Input wire (`ras-protocol`) — refines docs/04 §12/§13, **ADR-067**

```rust
/// One OS-input event, bound to the lease that authorizes it. Rides the control stream.
pub struct InputEnvelope {
    pub lease_id: [u8; 16],
    pub generation: u32,     // MUST equal the host's current generation, else rejected (Inv 5)
    pub seq: u64,            // strictly increasing per lease; host rejects <= last_seen (replay/reorder)
    pub action: InputAction,
}

pub enum InputAction {
    /// Normalized fixed-point 0..=65535 == 0.0..=1.0 of `display_id`'s logical bounds (ADR-061 model).
    PointerMove   { display_id: u32, nx: u16, ny: u16, layout_version: u32 },
    PointerButton { display_id: u32, nx: u16, ny: u16, layout_version: u32, button: PointerButton, down: bool },
    PointerWheel  { dx: i16, dy: i16 },                      // notched deltas, clamped
    /// Physical key by USB-HID usage (layout-independent) + explicit modifier bitset. Never a keysym.
    KeyEvent      { hid_usage: u16, down: bool, modifiers: u8 },
    /// UTF-8 text for the `keyboard.text` cap ONLY — layout-independent Unicode entry, never shortcuts.
    TextInput     { utf8: String },                          // bounded length; redacted in logs (Inv 8)
    /// Explicit key-state flush — host injects on transfer/disconnect/stop; also controller-sendable.
    ReleaseAllKeys,
}

pub enum PointerButton { Left, Right, Middle }               // closed set (Inv 6)
```

`ControlMsg` gains four variants (proto oneof fields **8–11**, additive to the existing 1–7):

```rust
pub enum ControlMsg {
    // … existing: Hello, StreamConfig, KeyframeRequest, Feedback, AuthEnvelope, Bye, Pointer …
    ControlRequest { capabilities: CapabilitySetWire },      // controller asks for the input lease
    ControlGranted { lease_id: [u8; 16], generation: u32, capabilities: CapabilitySetWire,
                     expires_at: u64, signature: Bytes },    // host → controller; host-signed (ADR-069)
    ControlRevoked { code: ErrorCode },                      // host → controller; revoke/transfer/deny
    Input(InputEnvelope),
}
```

*Why a distinct `Input` variant and not a richer `Pointer`?* ADR-061's `Pointer` is **visual only**,
carries no lease, and is deliberately *outside* Invariants 6/14. Mixing OS input into it would blur
that boundary. Keeping them separate means the host can route `Pointer` to the overlay and `Input`
to the enforcement gate with **no ambiguity** about which path enforces Inv 15. (§6 keeps using
`Pointer` for virtual cursors unchanged.)

### 2.2 Control lease (`ras-control`) — docs/04 §7

```rust
pub struct LeaseId(pub [u8; 16]);
pub type Generation = u32;

pub struct ControlLease {
    pub lease_id: LeaseId,
    pub session_id: SessionId,
    pub holder: ControllerId,
    pub capabilities: CapabilitySet,   // ⊆ grant.granted_capabilities ∩ consented
    pub generation: Generation,        // the session generation at issuance
    pub issued_at: UnixMillis,
    pub expires_at: UnixMillis,        // 30..=120 s, and never past the grant's expiry (§5)
}
```

### 2.3 Capabilities (`ras-policy`) — extend, not replace

The input/control caps already exist in `CATALOGUE_V1` as *recognized-but-withheld*
(`POINTER_MOVE`, `POINTER_CLICK`, `POINTER_SCROLL`, `KEYBOARD_KEY`, `KEYBOARD_TEXT`,
`CONTROL_REQUEST`, `CONTROL_TRANSFER`). Phase 3 adds a **`phase3_default_policy()`** that unions the
Phase-2 grantable set with the input caps a host is willing to grant *by default* — leaving
`keyboard.text`, clipboard, file, and recording still withheld unless a deployment widens policy:

```rust
pub const PHASE3_GRANTABLE: &[&str] = &[
    // Phase-2 view-only + visual pointer + annotation …
    SCREEN_VIEW, SCREEN_SELECT_MONITOR, POINTER_VIRTUAL, ANNOTATION_CREATE,
    // … plus OS input behind a lease:
    POINTER_MOVE, POINTER_CLICK, POINTER_SCROLL, KEYBOARD_KEY, CONTROL_REQUEST, CONTROL_TRANSFER,
];
```

A tiny **action→capability** map is the per-message enforcement key (§4):

```rust
fn required_cap(a: &InputAction) -> &'static str {
    match a {
        PointerMove{..}                 => POINTER_MOVE,
        PointerButton{..}               => POINTER_CLICK,
        PointerWheel{..}                => POINTER_SCROLL,
        KeyEvent{..}                    => KEYBOARD_KEY,
        TextInput{..}                   => KEYBOARD_TEXT,
        ReleaseAllKeys                  => /* always allowed: it only *clears* state */ SAFE,
    }
}
```

---

## 3. Crate interfaces (conceptual, `todo!()` bodies)

### 3.1 `ras-control` — leases, generations, and the per-message gate (the heart of Phase 3)

`ras-control` stays **pure and `unsafe`-free**: it owns the authoritative lease state and the
enforcement logic; it never touches the OS. It is the one place Inv 15 is enforced.

```rust
/// The single source of truth for "who may inject, right now". Host-authoritative (ADR-069):
/// enforcement reads THIS, never the controller's claimed lease/generation/scope.
pub struct LeaseManager {
    active: Option<ControlLease>,
    generation: Generation,          // monotonic; bumped on issue / transfer / revoke / stop
    last_seq: u64,                   // per-active-lease replay guard
    grant_caps: CapabilitySet,       // the session grant's granted_capabilities (the ceiling)
    grant_expiry: UnixMillis,
}

impl LeaseManager {
    /// Grant/transfer the single lease. Bumps the generation (invalidating any prior holder), clamps
    /// caps to `requested ∩ grant_caps ∩ consented` and expiry to `min(now+MAX, grant_expiry)`.
    pub fn issue(&mut self, holder: ControllerId, requested: &CapabilitySet,
                 consented: &CapabilitySet, now: UnixMillis) -> Result<ControlLease, ControlError> { todo!() }

    pub fn renew(&mut self, lease_id: &LeaseId, now: UnixMillis) -> Result<(), ControlError> { todo!() }

    /// Emergency stop / teardown: bump generation, clear active. After this, EVERY in-flight input
    /// (any generation) is stale. Never fails, never blocks (Inv 4).
    pub fn revoke_all(&mut self) -> Generation { todo!() }

    /// THE PER-MESSAGE GATE. O(1). Ordered checks (§5); returns the normalized action to inject only
    /// if lease + generation + expiry + seq + capability ALL hold. Content-free error on any failure.
    pub fn authorize_input(&mut self, env: &InputEnvelope, now: UnixMillis)
        -> Result<&InputAction, ControlError> { todo!() }
}
```

### 3.2 `ras-control::OsInputSink` (trait) + `ras-input-macos` (backend) — **ADR-068**

The trait is pure and lives in `ras-control`; the OS backend is a new FFI crate (`unsafe` confined,
empty on non-macOS so Linux CI stays green — the `ras-media-macos` pattern).

```rust
/// The narrow, validated input surface (Inv 6). Takes ONLY normalized coordinates + the closed
/// action set — never a pixel, a path, an OS-API name, or a keysym string.
pub trait OsInputSink: Send + Sync {
    fn pointer_move(&self, display: u32, nx: f32, ny: f32) -> Result<(), InputError>;
    fn pointer_button(&self, display: u32, nx: f32, ny: f32, b: PointerButton, down: bool) -> Result<(), InputError>;
    fn pointer_wheel(&self, dx: i16, dy: i16) -> Result<(), InputError>;
    fn key(&self, hid_usage: u16, down: bool, modifiers: u8) -> Result<(), InputError>;
    fn text(&self, utf8: &str) -> Result<(), InputError>;            // keyboard.text only
    /// Release every key this sink currently believes is down. Idempotent. Injected on stop/transfer.
    fn release_all(&self) -> Result<(), InputError>;
    /// Preflight the OS permission WITHOUT prompting (macOS: CGPreflightPostEventAccess). Fail-closed.
    fn input_permitted(&self) -> bool;
}
```

`ras-input-macos::CgEventSink`:
- Maps `(display_id, nx, ny)` → global points using the capture geometry the host already emits
  (`LifecycleEvent::CaptureGeometry`) — Retina scale + multi-display origin offsets. **Host-side
  mapping only** (Inv 6).
- `CGEventCreateMouseEvent` / `CGEventCreateKeyboardEvent` + `CGEventPost(kCGHIDEventTap, …)`;
  `CGEventKeyboardSetUnicodeString` for `text` (layout-independent).
- **Tracks the set of currently-pressed keys/buttons** so `release_all` is exact (docs/18 §3).
- Gates on `CGPreflightPostEventAccess` (**PostEvent** TCC bucket, *not* Accessibility); a first run
  calls `CGRequestPostEventAccess`. If ungranted, `input_permitted()` is `false` and the host refuses
  the lease with a clear reason (CGEventPost fails *silently* otherwise — docs/18 §0).
- **Deliberately unprivileged** (ADR-055): it therefore *cannot* inject into a Secure-Input field
  (password/login). That is a feature, not a bug — the fraud-model boundary (docs/18 §0).

Linux (`uinput`/libei) and Windows (`SendInput` ABSOLUTE|VIRTUALDESK, PMv2 manifest — docs/11 §3)
backends are **deferred**; the trait makes them additive.

### 3.3 `ras-core` — route input through the gate to the sink

`ras-core`'s host session already reaches `Active` with `GrantDecision::Authorized(caps)`. Phase 3
wires a `LeaseManager` (seeded with `caps` + grant expiry) and, on each control-stream
`ControlMsg::Input`, calls `authorize_input` → on `Ok`, dispatches to the `OsInputSink`; on `Err`,
drops the event and emits a content-free `LifecycleEvent::InputRejected{code}` (audit-ready, Phase 4).
`ControlRequest` drives consent → `LeaseManager::issue` → `ControlGranted`. This is **additive** to
the Phase-1/2 state machine: no renamed states, `Active` unchanged, emergency stop still the
`Revoke → Revoked` edge (now also calling `revoke_all` + `release_all`, §7).

---

## 4. The input hot path & per-message enforcement (ADR-041, Inv 15)

Every injected event passes **one** function — `LeaseManager::authorize_input` — before it can reach
the OS. It is the single choke point where Inv 15 lives, and it is cheap:

```
authorize_input(env, now):
  ① env.generation == self.generation          else Err(StaleGeneration)   -- Inv 5, the transfer fix
  ② Some(l) = self.active & env.lease_id == l   else Err(NoActiveLease)
  ③ now <= l.expires_at                          else Err(LeaseExpired)
  ④ env.seq > self.last_seq                       else Err(ReplayedInput)    -- reorder/replay guard
  ⑤ required_cap(&env.action) ∈ l.capabilities   else Err(CapabilityDenied)  -- Inv 15 / ADR-041
  → self.last_seq = env.seq; Ok(&env.action)
```

- All five checks are **integer compares + one `BTreeSet` lookup** — O(1), no allocation, no I/O, no
  crypto. Nanoseconds. It runs on the **control** task, never on the per-frame video path (ADR-060:
  distinct QUIC streams, no HOL blocking). Latency invariant (priority 2) is untouched.
- **Host-authoritative (ADR-069):** every value compared against is the host's own
  (`self.generation`, `self.active`, `self.last_seq`, `l.capabilities`). The controller's
  `env.generation`/`env.lease_id` are *claims* that must match; they are never trusted as authority.
  This is precisely the RustDesk CVE-2026-57850 class (client-asserted scope) closed structurally.
- **Coordinates never arrive as pixels.** `nx/ny` are normalized `0..=65535`; the host maps them to
  device pixels *after* authorization, using its own capture geometry. A controller cannot aim at a
  pixel it can't see, cannot target another display it wasn't granted, and cannot inject a path or an
  OS-API string — the action set is closed (Inv 6).
- **`layout_version` staleness:** a `PointerMove/Button` whose `layout_version` ≠ the host's current
  `CaptureGeometry` version is dropped (`StaleLayout`) — after a monitor change, coordinates from the
  old layout are meaningless (docs/04 §12).

---

## 5. Lease/generation state machine & validation order

Mirrors docs/03 §10; the generation is the load-bearing anti-race primitive.

```
        ControlRequest+consent            ControlRequest(other)+consent
NoLease ─────────────────────► Active ───────────────────────────────► Active'
  ▲         issue: gen G+1     (holder A,   transfer: gen G+2,          (holder B,
  │                             gen G+1)     revoke A, ReleaseAllKeys    gen G+2)
  │                                │             on host
  └──── revoke_all (stop / expiry / disconnect / Deny): gen bump, ReleaseAllKeys, active=None ───┘
```

**Ordered lease-issue checks (fail-closed):** ① grant carries `control.request` (Inv 15 — no
escalation past the grant) → ② requested input caps ⊆ `grant.granted_capabilities` → ③ local consent
Allow (Inv 1) → ④ clamp caps to `requested ∩ grant_caps ∩ consented` (`ras-policy::grantable`, never
expands) → ⑤ clamp `expires_at = min(now + LEASE_MAX, grant.expires_at)` → **then** bump generation +
install. A lease can only ever be **narrower** than the grant and **shorter-lived** than the session.

**Concrete bounds (this gate sets):** lease TTL **30–120 s** (docs/04 §7), default **60 s**,
renewable while the grant is live; renewal never extends past the grant. One live lease, always.

**Replay/generation state (host, in-memory — MVP is attended-only, per Phase-2 Q-GEN-STORE):**

```
session_generation : u32                    -- bumped on issue / transfer / revoke / stop
active_lease       : Option<ControlLease>   -- the single OS-input holder (Inv 5)
last_input_seq     : u64                     -- per-lease monotonic replay guard; resets on generation bump
pressed_keys       : set (in the OsInputSink) -- for exact ReleaseAllKeys (§7)
```

---

## 6. Virtual multi-cursor relay (Inv 5)

Non-lease participants (and the lease-holder's *own* cursor, for the overlay) are **visual only**,
via the existing `ControlMsg::Pointer` (ADR-061) — no lease, no injection, explicitly outside
Invariants 6/14. Phase 3 generalizes the single remote pointer to **N** participants:

- Each participant streams normalized `Pointer{x, y, visible}` (fixed-point `0..=65535`).
- The host relay is **latest-state-wins per participant, rate-limited** (docs/03 §7) — a stalled or
  flooding participant cannot back up the control stream or delay input authorization.
- The host draws them on the existing transparent, click-through overlay (distinct colors/labels),
  positioned by `CaptureGeometry` (the multi-monitor overlay work already in the app).
- **Only the lease-holder's `Input` events inject** (Inv 5). A virtual-cursor participant that has no
  lease and sends an `Input` envelope is rejected at the §4 gate (`NoActiveLease`) — belt and braces.

**MVP surface:** one lease-holder + virtual cursors. The relay is written for N but the shipped tests
cover single-controller + one or two virtual cursors.

---

## 7. Emergency stop, transfer & key-state cleanup (Inv 4, 14)

**Emergency stop is unchanged in spirit** (Phase-1 `HostSession::emergency_stop`), extended to input:

1. `LeaseManager::revoke_all()` — bump generation, drop the active lease. Every subsequent `Input`
   (any generation) now fails `StaleGeneration` at the gate, before touching the OS.
2. `OsInputSink::release_all()` — inject key/button-up for everything the sink believes is down, so a
   controller cannot leave a key or mouse button stuck (docs/03 §6, docs/18 §3).
3. The existing `Revoke → Revoked` edge halts the media pump and flushes `Bye{SessionRevoked}` — the
   Phase-1 path, ≤250 ms locally, idempotent, non-downgradable.

Steps 1–2 also run on **transfer**, **grant expiry**, **disconnect/transport loss**, and **consent
Deny** — anywhere the current holder loses the lease, its keys are released first.

**Ordering guarantee (Inv 4):** `revoke_all` bumps the generation *before* `release_all` injects, and
the input task checks the generation *before* every injection — so there is no window where a
post-stop event lands. Stop is never gated behind a lease or capability check.

**Platform honesty (Inv 14 caveat):** ADR-048's SAS binding (Ctrl+Alt+Del) is **Windows-specific**;
macOS has no kernel SAS. On macOS the emergency stop is the always-visible indicator's **Stop**
button plus an optional host-registered **global hotkey** (`CGEventTap`/`NSEvent` monitor), driving
the same `emergency_stop`. This is a genuine platform difference, documented not hidden — we do **not**
claim a kernel-guaranteed stop on macOS, and we still never build a Secure-Input bypass (ADR-055).

---

## 8. macOS input backend specifics (`ras-input-macos`, ADR-068)

| Concern | Decision (docs/18) |
|---|---|
| Permission | **PostEvent** TCC (`kTCCServicePostEvent`), *not* Accessibility. Preflight `CGPreflightPostEventAccess`; request `CGRequestPostEventAccess`. `CGEventPost` fails **silently** if ungranted — so `input_permitted()` gates lease issuance. |
| API | `CGEventCreateMouseEvent` / `CGEventCreateKeyboardEvent` + `CGEventPost(kCGHIDEventTap, …)`; `CGEventKeyboardSetUnicodeString` for `keyboard.text`. |
| Coordinates | Normalized → **points** with Retina scale + multi-display origin offset, from the host's `CaptureGeometry`. |
| Keys | Physical **USB-HID usage** on the wire → macOS virtual keycode in the backend; explicit modifier bitset; `pressed_keys` tracked for `release_all`. |
| Privilege | **Unprivileged per-user LaunchAgent** (ADR-055). Root has no WindowServer and could bypass Secure Input — neither is wanted. |
| Secure Input | Synthetic keystrokes are (correctly) dropped by the OS in password/secure fields. We respect this and surface it honestly; we never try to defeat it. |
| Bindings | Pure-Rust `objc2` + `core-graphics` (permissive), no Swift bridge — the `ras-media-macos` family. `enigo` (MIT) is an acceptable higher-level fallback if raw CGEvent proves fiddly, but raw CGEvent is preferred for the tracked-key/`release_all` precision. `cargo-deny` must clear any new dep (Inv 18). |

---

## 9. What stays stubbed after Phase 3

- **Clipboard, file transfer, support-action catalogue** — later phases; their caps stay
  recognized-but-withheld (`clipboard.*`, `file.*`, `action.request`).
- **Linux/Windows OS-input backends** — deferred; `OsInputSink` makes them additive (docs/11 §3 has
  the Windows recipe).
- **Hash-chained signed audit journal** (`ras-audit`, ADR-042) — Phase 4. Phase 3 emits the
  content-free `InputRejected`/lease lifecycle events it will consume; input **payloads** (typed text,
  key values) are never in those events (Inv 8).
- **Assurance Tier ≥1** (TPM/Keychain-sealed keys, attestation) — unchanged from Phase 2; MVP is
  Tier 0.
- **Restart-surviving lease/generation state** — MVP is attended-only, in-memory (Phase-2
  Q-GEN-STORE); a host restart ends the session anyway.

---

## 10. Open questions — resolutions

- **Q-LEASE-TOKEN — RESOLVED (ADR-069).** The lease is **host-authoritative live state**, not a
  trusted bearer token. `ControlGranted` is host-signed for the *future* process split (S4), but MVP
  enforcement reads the host's own generation/active-lease/seq — the controller's claims must match,
  never authorize.
- **Q-INPUT-COORDS — RESOLVED (ADR-067).** Normalized fixed-point `0..=65535` + `display_id` +
  `layout_version` on the wire; host maps to pixels. Refines docs/04 §12's "float" to the ADR-061
  fixed-point encoding for wire efficiency and one coordinate model across visual + OS-input pointers.
- **Q-STOP-MACOS — RESOLVED (§7).** No kernel SAS on macOS; stop = always-visible Stop + optional
  host global hotkey, same `emergency_stop`, ≤250 ms, honestly caveated. ADR-048's SAS stays the
  Windows path.
- **Q-TEXT-CAP — MVP default.** `keyboard.text` (Unicode `TextInput`) stays **withheld by default**
  (not in `PHASE3_GRANTABLE`); a deployment may widen policy. Physical `keyboard.key` is the default
  keyboard path, so shortcuts/keys work without exposing arbitrary Unicode injection by default.
- **Q-MULTI-CONTROLLER — MVP default.** One OS-input lease + N virtual cursors. Genuine concurrent
  multi-injection is explicitly **not** a goal (Inv 5); it is not a deferred feature but a rejected
  one for the core.

---

## 11. Security test matrix (exit criteria)

Every row is required before **M4** (`docs/07` Phase 3, `docs/17` Phase 3 ③). Unit + property + fuzz +
integration.

| Attack / property | Expected | Layer |
|---|---|---|
| Two controllers inject concurrently | second is a virtual cursor only; its `Input` → `NoActiveLease` | `ras-control` (integration) |
| Old-generation input after transfer | `StaleGeneration`, dropped before the OS sink | `ras-control` |
| Replayed / reordered input (`seq ≤ last`) | `ReplayedInput`, dropped | `ras-control` |
| Expired lease input | `LeaseExpired`, dropped | `ras-control` |
| Input action outside the lease's caps (e.g. `KeyEvent` with only `pointer.*`) | `CapabilityDenied` (Inv 15) | `ras-control` (property) |
| `ControlRequest` from a grant lacking `control.request` | refused, no lease (no escalation past grant) | `ras-control` + `ras-core` |
| Lease caps ⊄ grant caps | clamped by `grantable`; lease never exceeds grant | `ras-policy` (property) |
| Coordinate with stale `layout_version` | `StaleLayout`, dropped | `ras-control` |
| Emergency stop during active input | generation bumped + `ReleaseAllKeys` + halt ≤250 ms; overrides lease (Inv 4) | `ras-core` |
| Keys left down on transfer/disconnect/stop | `release_all` clears them; no stuck key | `ras-control` + `ras-input-macos` |
| Input payload in a log/trace line | **never present** (typed text / key values redacted) | grep gate (Inv 8) |
| Ungranted PostEvent TCC | lease refused with a clear reason; no silent no-op inject | `ras-input-macos` (on-device) |
| Fuzz: `decode` of `InputEnvelope` / `ControlMsg` input variants | never panics; fail-closed | `ras-protocol` (fuzz) |

**On-device (developer step, not a CI row):** the actual CGEvent injection, PostEvent-TCC prompt,
Secure-Input drop, and multi-monitor coordinate mapping need a macOS login session with the
permission granted — the same on-device constraint as every prior media/app change. The **logic**
(lease/generation/seq/cap gate, release-on-transfer) is fully covered by the pure `ras-control` tests
above.

---

## 12. Execution sequence

1. `ras-policy`: add `PHASE3_GRANTABLE` + `phase3_default_policy()` + `required_cap` map (extend; the
   caps already exist in `CATALOGUE_V1`).
2. `ras-protocol`: `InputEnvelope` + `InputAction` + the four `ControlMsg` variants + `proto/` oneof
   fields 8–11; codec map + bounds (coord range, text length, closed button/action sets) + fuzz.
3. `ras-control`: `LeaseManager` (issue/renew/transfer/`revoke_all`) + `authorize_input` (the §4
   gate) + the `OsInputSink` trait. Pure, `unsafe`-free; unit + property + fuzz.
4. `ras-input-macos` (new crate, ADR-068): `CgEventSink` implementing `OsInputSink` over CGEvent;
   empty on non-macOS; `--example` on-device smoke.
5. `ras-core`: seed a `LeaseManager` at `Active`; route `ControlRequest`→consent→`issue`→
   `ControlGranted`; route `Input`→gate→sink; wire `revoke_all`+`release_all` into `emergency_stop`
   and every lease-loss path; emit content-free lifecycle events.
6. App: "Request control" UI + the input-caps consent panel (reuse the Phase-2 consent window);
   forward viewer pointer/keyboard as `Input` when it holds the lease, as `Pointer` otherwise; the
   macOS global-hotkey stop (§7).
7. Security test matrix (§11); property/fuzz on the gate + codec; `cargo-deny` on any new dep.

**Exit → M4:** no two controllers inject concurrently by default · old-lease input rejected after
transfer · emergency stop halts input within target time · virtual cursors stay responsive during
video loss · every §11 row green.
