//! Casual RAS control: OS-input **leases**, generation tracking, and the per-message enforcement
//! gate (Phase 3, `docs/design/phase-3-design.md`).
//!
//! This crate is **pure and `unsafe`-free**: it owns the single authoritative answer to "who may
//! inject OS input, right now" and re-checks it on **every** event. It never touches the OS — the
//! actual injection is an [`OsInputSink`] implemented by a platform backend (`ras-input-macos`),
//! confined to that FFI crate.
//!
//! # Load-bearing invariants (`CLAUDE.md §5`)
//! - **One active OS-input controller at a time** (Inv 5): there is exactly one [`ControlLease`], and
//!   issuing/transferring bumps the [`Generation`] so any prior holder's in-flight input is instantly
//!   stale — there is never a window where two generations are both valid.
//! - **Per-message, host-side capability enforcement** (Inv 15, ADR-041): [`LeaseManager::authorize_input`]
//!   re-checks capability **and** lease **and** generation **and** sequence on every event, against the
//!   host's **own** live state — never the controller's claim (ADR-069). This closes the RustDesk
//!   CVE-2026-57850 class structurally.
//! - **Narrow, validated input surface** (Inv 6): the gate only ever yields a [`ras_protocol::InputAction`]
//!   from the closed set — never a shell command, path, OS-API name, or keysym.
//! - **Emergency stop overrides everything** (Inv 4): [`LeaseManager::revoke_all`] bumps the generation
//!   and drops the lease unconditionally; it never fails and never blocks.

use getrandom::getrandom;
use ras_policy::CapabilitySet;
use ras_protocol::{ErrorCode, InputAction, InputEnvelope, PointerButton, RasError};

/// Milliseconds since the Unix epoch (host clock). Aliased locally so this crate stays dependency-light.
pub type UnixMillis = u64;

/// A control-lease generation. Monotonic within a session; bumped on issue / transfer / revoke / stop.
pub type Generation = u32;

/// The one canonical error type, aliased (like every crate's) so `?` needs no `From` impls.
pub type InputError = RasError;

/// A 16-byte control-lease identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseId(pub [u8; 16]);

/// Minimum control-lease TTL (`docs/04 §7`). A shorter request is clamped up.
pub const LEASE_MIN_TTL_MS: u64 = 30_000;
/// Maximum control-lease TTL. A longer request is clamped down.
pub const LEASE_MAX_TTL_MS: u64 = 120_000;
/// Default control-lease TTL when the caller does not specify one.
pub const LEASE_DEFAULT_TTL_MS: u64 = 60_000;

/// A single, live OS-input control lease (`docs/04 §7`, `docs/design/phase-3-design.md §2.2`).
#[derive(Debug, Clone)]
pub struct ControlLease {
    /// The lease identifier the controller echoes on every [`InputEnvelope`].
    pub lease_id: LeaseId,
    /// The lease holder's identity (raw controller/endpoint public-key bytes). Informational in the
    /// MVP single-connection model; the generation + transport authentication are what bind input.
    pub holder: [u8; 32],
    /// The capabilities this lease grants — always `requested ∩ grant ∩ consent` (never expands).
    pub capabilities: CapabilitySet,
    /// The session generation at issuance. An input event with any other generation is rejected.
    pub generation: Generation,
    /// When the lease was issued (host clock).
    pub issued_at: UnixMillis,
    /// Absolute expiry; never past the session grant's expiry.
    pub expires_at: UnixMillis,
}

/// Why an input event or lease operation was refused. Distinct variants for precise testing; each maps
/// to a stable wire [`ErrorCode`] via [`ControlError::code`] for the `ControlRevoked`/audit path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlError {
    /// The event's generation ≠ the host's current generation (a prior holder after transfer/stop).
    StaleGeneration,
    /// No lease is active, or the event's `lease_id` does not match the active lease.
    NoActiveLease,
    /// The active lease has expired.
    LeaseExpired,
    /// `seq ≤ last_seen` — a replayed or reordered event.
    ReplayedInput,
    /// The coordinate's `layout_version` ≠ the host's current capture geometry (stale after a monitor
    /// change).
    StaleLayout,
    /// The action's required capability is not in the lease's capability set (Inv 15 / ADR-041).
    CapabilityDenied,
    /// The session grant does not carry `control.request` — no escalation to input past the grant.
    NoGrantToRequest,
    /// The OS entropy source was unavailable when minting a lease id (never on a supported platform).
    EntropyUnavailable,
}

impl ControlError {
    /// The stable wire error code for this refusal (for `ControlMsg::ControlRevoked` / audit).
    #[must_use]
    pub fn code(self) -> ErrorCode {
        match self {
            ControlError::StaleGeneration
            | ControlError::NoActiveLease
            | ControlError::LeaseExpired => ErrorCode::LeaseInvalid,
            ControlError::ReplayedInput => ErrorCode::ReplayDetected,
            ControlError::StaleLayout => ErrorCode::InvalidMessage,
            ControlError::CapabilityDenied | ControlError::NoGrantToRequest => {
                ErrorCode::CapabilityDenied
            }
            ControlError::EntropyUnavailable => ErrorCode::Internal,
        }
    }
}

/// A sentinel capability that is never in any catalogue/grant, so requiring it always denies. Used
/// fail-closed for an unrecognized future [`InputAction`] variant (`InputAction` is `#[non_exhaustive]`).
const DENY_UNKNOWN_ACTION: &str = "\u{0}deny-unknown-action";

/// The capability an action requires, or `None` if the action is always permitted (`ReleaseAllKeys`
/// only *clears* state, so it needs no capability — it is a safety operation, never an escalation).
///
/// An unrecognized future action fails **closed**: it requires [`DENY_UNKNOWN_ACTION`], which no grant
/// can hold, so the gate denies it rather than defaulting it allowed.
#[must_use]
pub fn required_cap(action: &InputAction) -> Option<&'static str> {
    match action {
        InputAction::PointerMove { .. } => Some(ras_policy::POINTER_MOVE),
        // Relative motion is still cursor movement — same capability as absolute move (ADR-087).
        InputAction::PointerMoveRelative { .. } => Some(ras_policy::POINTER_MOVE),
        InputAction::PointerButton { .. } => Some(ras_policy::POINTER_CLICK),
        InputAction::PointerWheel { .. } => Some(ras_policy::POINTER_SCROLL),
        InputAction::KeyEvent { .. } => Some(ras_policy::KEYBOARD_KEY),
        InputAction::TextInput { .. } => Some(ras_policy::KEYBOARD_TEXT),
        // Lock-state sync affects what the keyboard produces → gate it on the keyboard capability.
        InputAction::SetLockState { .. } => Some(ras_policy::KEYBOARD_KEY),
        InputAction::ReleaseAllKeys => None,
        _ => Some(DENY_UNKNOWN_ACTION),
    }
}

/// The capture-geometry layout version a coordinate-bearing action was computed against, if any.
///
/// `InputAction` is `#[non_exhaustive]`, so the trailing `_` arm is **forced** by the language — a
/// compile-exhaustive match is impossible from this crate. The wildcard is fail-*permissive* here (an
/// unmatched action skips freshness), unlike [`required_cap`]/[`dispatch`] which fail *closed*. That is
/// still safe: freshness (gate ⑤) runs *before* capability (gate ⑥), and any action this function does
/// not recognize is `required_cap → DENY_UNKNOWN_ACTION` and thus denied at ⑥ regardless. The one thing
/// this asymmetry cannot self-defend is a **new, explicitly-capability-mapped** coordinate action added
/// without a matching arm here — it would inject against a stale layout (a wrong-position click, not an
/// escalation). **Any `InputAction` carrying a `layout_version` MUST be added to this match.** The
/// `coordinate_actions_are_freshness_gated` test pins the current coverage against regression.
fn action_layout_version(action: &InputAction) -> Option<u32> {
    match action {
        InputAction::PointerMove { layout_version, .. }
        | InputAction::PointerButton { layout_version, .. } => Some(*layout_version),
        _ => None,
    }
}

/// The single source of truth for "who may inject OS input, right now" (ADR-069). Host-authoritative:
/// every value the gate compares against is the host's own, never the controller's claim.
#[derive(Debug)]
pub struct LeaseManager {
    active: Option<ControlLease>,
    generation: Generation,
    last_seq: u64,
    layout_version: u32,
    grant_caps: CapabilitySet,
    grant_expiry: UnixMillis,
}

impl LeaseManager {
    /// Create a manager for a session whose grant carries `grant_caps` and expires at `grant_expiry`.
    /// `initial_generation` is the grant's `session_generation`; the first `issue` bumps from it.
    #[must_use]
    pub fn new(
        grant_caps: CapabilitySet,
        grant_expiry: UnixMillis,
        initial_generation: Generation,
    ) -> Self {
        Self {
            active: None,
            generation: initial_generation,
            last_seq: 0,
            layout_version: 0,
            grant_caps,
            grant_expiry,
        }
    }

    /// The current session generation.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// The active lease, if any.
    #[must_use]
    pub fn active_lease(&self) -> Option<&ControlLease> {
        self.active.as_ref()
    }

    /// The host's current capture-geometry layout version (coordinate freshness).
    #[must_use]
    pub fn layout_version(&self) -> u32 {
        self.layout_version
    }

    /// Update the current capture-geometry layout version (call on `CaptureGeometry` change). Input
    /// carrying an older `layout_version` is then dropped as [`ControlError::StaleLayout`].
    pub fn set_layout_version(&mut self, version: u32) {
        self.layout_version = version;
    }

    /// Grant (or transfer) the single OS-input lease. Bumps the generation — invalidating any prior
    /// holder — clamps capabilities to `requested ∩ grant ∩ consent` (never expands, `ras-policy`),
    /// and clamps expiry to `min(now + ttl, grant_expiry)`. Refuses if the grant itself does not carry
    /// `control.request` (no escalation past the grant, Inv 15).
    ///
    /// # Errors
    /// [`ControlError::NoGrantToRequest`] if the session grant lacks `control.request`.
    pub fn issue(
        &mut self,
        holder: [u8; 32],
        requested: &CapabilitySet,
        consented: &CapabilitySet,
        now: UnixMillis,
        ttl_ms: u64,
    ) -> Result<ControlLease, ControlError> {
        if !self.grant_caps.contains(ras_policy::CONTROL_REQUEST) {
            return Err(ControlError::NoGrantToRequest);
        }
        let capabilities = ras_policy::grantable(requested, &self.grant_caps, consented);
        let ttl = ttl_ms.clamp(LEASE_MIN_TTL_MS, LEASE_MAX_TTL_MS);
        let expires_at = now.saturating_add(ttl).min(self.grant_expiry);
        // Mint the id BEFORE mutating state, so an (essentially impossible) entropy failure leaves the
        // manager untouched rather than half-transitioned.
        let lease_id = LeaseId(fresh_lease_id()?);

        self.generation = self.generation.wrapping_add(1);
        self.last_seq = 0;
        let lease = ControlLease {
            lease_id,
            holder,
            capabilities,
            generation: self.generation,
            issued_at: now,
            expires_at,
        };
        self.active = Some(lease.clone());
        Ok(lease)
    }

    /// Transfer the lease to a new holder. Identical to [`LeaseManager::issue`] — the generation bump
    /// is what makes the departing holder's in-flight input stale — named separately for intent.
    ///
    /// # Errors
    /// As [`LeaseManager::issue`].
    pub fn transfer(
        &mut self,
        to: [u8; 32],
        requested: &CapabilitySet,
        consented: &CapabilitySet,
        now: UnixMillis,
        ttl_ms: u64,
    ) -> Result<ControlLease, ControlError> {
        self.issue(to, requested, consented, now, ttl_ms)
    }

    /// Extend the active lease's expiry (same holder, **no** generation bump). Cannot renew a missing,
    /// mismatched, or already-expired lease.
    ///
    /// # Errors
    /// [`ControlError::NoActiveLease`] / [`ControlError::LeaseExpired`].
    pub fn renew(
        &mut self,
        lease_id: &LeaseId,
        now: UnixMillis,
        ttl_ms: u64,
    ) -> Result<(), ControlError> {
        let grant_expiry = self.grant_expiry;
        let lease = self.active.as_mut().ok_or(ControlError::NoActiveLease)?;
        if lease.lease_id != *lease_id {
            return Err(ControlError::NoActiveLease);
        }
        if now > lease.expires_at {
            return Err(ControlError::LeaseExpired);
        }
        let ttl = ttl_ms.clamp(LEASE_MIN_TTL_MS, LEASE_MAX_TTL_MS);
        lease.expires_at = now.saturating_add(ttl).min(grant_expiry);
        Ok(())
    }

    /// Emergency stop / teardown: bump the generation and drop the active lease. After this, **every**
    /// in-flight input (any generation) is stale at the gate. Never fails, never blocks (Inv 4).
    /// Returns the new generation.
    pub fn revoke_all(&mut self) -> Generation {
        self.generation = self.generation.wrapping_add(1);
        self.active = None;
        self.last_seq = 0;
        self.generation
    }

    /// If an active lease exists **and** has expired (`now > expires_at`), drop it (via [`revoke_all`])
    /// and return `true`; otherwise return `false`. This is the trigger for OS key-state cleanup on
    /// expiry: the per-message gate already refuses input under an expired lease, but that also means the
    /// matching key-**up** can never be delivered, so the caller must flush the OS sink's held keys when
    /// this returns `true` (Inv 4 — a stuck Ctrl/Shift after an idle controller). Fires once: after it
    /// clears the lease, a subsequent call returns `false`. Pure; the caller supplies `now`.
    ///
    /// [`revoke_all`]: Self::revoke_all
    pub fn revoke_if_expired(&mut self, now: UnixMillis) -> bool {
        let expired = self.active.as_ref().is_some_and(|l| now > l.expires_at);
        if expired {
            self.revoke_all();
        }
        expired
    }

    /// **The per-message enforcement gate** (Inv 15, ADR-041). O(1): integer compares + one set lookup,
    /// no allocation, no I/O, no crypto. Returns the normalized action to inject **only** if the
    /// generation, lease, expiry, sequence, layout, and capability checks all pass — in that order,
    /// first failure wins. On success, advances `last_seq`.
    ///
    /// Every value compared against is the host's own (`self.generation`, `self.active`, `self.last_seq`,
    /// `self.layout_version`, `lease.capabilities`); the controller's claims must match, never authorize.
    ///
    /// # Errors
    /// A [`ControlError`] naming the first failed check; nothing reaches the OS sink.
    pub fn authorize_input<'a>(
        &mut self,
        env: &'a InputEnvelope,
        now: UnixMillis,
    ) -> Result<&'a InputAction, ControlError> {
        // ① generation (the transfer/stop fix — Inv 5)
        if env.generation != self.generation {
            return Err(ControlError::StaleGeneration);
        }
        let lease = self.active.as_ref().ok_or(ControlError::NoActiveLease)?;
        // ② lease identity
        if env.lease_id != lease.lease_id.0 {
            return Err(ControlError::NoActiveLease);
        }
        // ③ expiry
        if now > lease.expires_at {
            return Err(ControlError::LeaseExpired);
        }
        // ④ sequence (replay / reorder)
        if env.seq <= self.last_seq {
            return Err(ControlError::ReplayedInput);
        }
        // ⑤ coordinate freshness
        if let Some(lv) = action_layout_version(&env.action) {
            if lv != self.layout_version {
                return Err(ControlError::StaleLayout);
            }
        }
        // ⑥ capability (Inv 15 / ADR-041)
        if let Some(cap) = required_cap(&env.action) {
            if !lease.capabilities.contains(cap) {
                return Err(ControlError::CapabilityDenied);
            }
        }
        self.last_seq = env.seq;
        Ok(&env.action)
    }
}

/// The narrow, validated OS-input surface (Inv 6). A platform backend (`ras-input-macos`) implements
/// it; `ras-control` calls it only with a **normalized** action the gate already authorized — never a
/// pixel, path, OS-API name, or keysym. Normalized coordinates are fractions `0.0..=1.0` of a display.
pub trait OsInputSink: Send + Sync {
    /// Move the OS pointer to a normalized position on a display.
    ///
    /// # Errors
    /// Backend/OS failure (permission missing, injection refused).
    fn pointer_move(&self, display: u32, nx: f32, ny: f32) -> Result<(), InputError>;
    /// Press or release a pointer button at a normalized position.
    ///
    /// # Errors
    /// Backend/OS failure.
    fn pointer_button(
        &self,
        display: u32,
        nx: f32,
        ny: f32,
        button: PointerButton,
        down: bool,
    ) -> Result<(), InputError>;
    /// Scroll by notched deltas.
    ///
    /// # Errors
    /// Backend/OS failure.
    fn pointer_wheel(&self, dx: i16, dy: i16) -> Result<(), InputError>;
    /// Move the OS pointer by a relative pixel delta from its current position (ADR-087, §3.6 — the
    /// trackpad/touch controller path). Default is a **no-op** so existing backends stay source-compatible;
    /// a backend that supports relative motion (CGEvent / XTEST / `SendInput` with `MOUSEEVENTF_MOVE`)
    /// overrides it. Gated identically to [`Self::pointer_move`] (the `pointer.move` capability).
    ///
    /// # Errors
    /// Backend/OS failure.
    fn pointer_move_relative(&self, _dx: i16, _dy: i16) -> Result<(), InputError> {
        Ok(())
    }
    /// Press or release a physical key by USB-HID usage + modifier bitset.
    ///
    /// # Errors
    /// Backend/OS failure.
    fn key(&self, hid_usage: u16, down: bool, modifiers: u8) -> Result<(), InputError>;
    /// Type layout-independent Unicode text (the `keyboard.text` capability).
    ///
    /// # Errors
    /// Backend/OS failure.
    fn text(&self, utf8: &str) -> Result<(), InputError>;
    /// Release every key/button the sink currently holds down (key-state cleanup). Idempotent.
    ///
    /// # Errors
    /// Backend/OS failure.
    fn release_all(&self) -> Result<(), InputError>;
    /// Slave the OS CapsLock/NumLock **state** to the controller's (not an edge — see
    /// [`InputAction::SetLockState`]). Default is a no-op for backends/test doubles that don't sync
    /// lock state; the real platform backends override it. Idempotent (only toggles on a mismatch).
    ///
    /// # Errors
    /// Backend/OS failure.
    fn set_lock_state(&self, caps_lock: bool, num_lock: bool) -> Result<(), InputError> {
        let _ = (caps_lock, num_lock);
        Ok(())
    }
    /// Whether OS input is permitted **without** prompting (macOS: `CGPreflightPostEventAccess`).
    /// Fail-closed: a backend that cannot inject returns `false`, and the host refuses the lease.
    fn input_permitted(&self) -> bool;
}

/// The host's OS-clipboard **write** seam (ADR-076). A platform backend sets the OS clipboard to the
/// given text and **must never inject a paste keystroke** — the no-auto-paste rule that severs the
/// clipboard-hijack→RCE chain (Reverse-RDP / RustDesk CVE class). This is deliberately *not* part of
/// [`OsInputSink`]: setting the clipboard is not OS input and is gated by a separate session capability
/// ([`ras_policy::clipboard_push_allowed`]), which the caller checks *before* invoking this. Object-safe
/// for DI. Distinct from OS input so a host may allow clipboard sync without allowing input, or vice
/// versa.
pub trait ClipboardSink: Send + Sync {
    /// Set the OS clipboard to `text` (plain UTF-8). **Never** pastes. `text` is a secret — never log
    /// it (Inv 8).
    ///
    /// # Errors
    /// Backend/OS failure (clipboard unavailable, write refused).
    fn set_text(&self, text: &str) -> Result<(), InputError>;
}

/// Dispatch an already-authorized [`InputAction`] to an [`OsInputSink`], normalizing the fixed-point
/// `0..=65535` coordinates to `0.0..=1.0` fractions. Only ever called with a gate-approved action.
///
/// # Errors
/// Propagates the sink's [`InputError`].
pub fn dispatch(sink: &dyn OsInputSink, action: &InputAction) -> Result<(), InputError> {
    fn norm(v: u16) -> f32 {
        f32::from(v) / f32::from(u16::MAX)
    }
    match action {
        InputAction::PointerMove {
            display_id, nx, ny, ..
        } => sink.pointer_move(*display_id, norm(*nx), norm(*ny)),
        InputAction::PointerButton {
            display_id,
            nx,
            ny,
            button,
            down,
            ..
        } => sink.pointer_button(*display_id, norm(*nx), norm(*ny), *button, *down),
        InputAction::PointerWheel { dx, dy } => sink.pointer_wheel(*dx, *dy),
        InputAction::PointerMoveRelative { dx, dy } => sink.pointer_move_relative(*dx, *dy),
        InputAction::KeyEvent {
            hid_usage,
            down,
            modifiers,
        } => sink.key(*hid_usage, *down, *modifiers),
        // `reveal()` at the OS-injection boundary is the one sanctioned use — typing, never logging.
        InputAction::TextInput { utf8 } => sink.text(utf8.reveal()),
        InputAction::ReleaseAllKeys => sink.release_all(),
        InputAction::SetLockState {
            caps_lock,
            num_lock,
        } => sink.set_lock_state(*caps_lock, *num_lock),
        // Fail-closed: an unrecognized future action is never injected (it is also denied at the gate
        // by `required_cap`, so this is belt-and-braces).
        _ => Err(RasError::fatal(
            ErrorCode::InputFailed,
            "unknown input action",
        )),
    }
}

/// 16 random bytes for a fresh lease id. Errors only if the OS entropy source is unavailable (never
/// on a supported platform) — we propagate rather than issue a zeroed / predictable id.
fn fresh_lease_id() -> Result<[u8; 16], ControlError> {
    let mut id = [0u8; 16];
    getrandom(&mut id).map_err(|_| ControlError::EntropyUnavailable)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn caps(items: &[&str]) -> CapabilitySet {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// A full input policy: the grant carries control.request + all the input caps.
    fn full_grant() -> CapabilitySet {
        caps(&[
            ras_policy::CONTROL_REQUEST,
            ras_policy::POINTER_MOVE,
            ras_policy::POINTER_CLICK,
            ras_policy::POINTER_SCROLL,
            ras_policy::KEYBOARD_KEY,
        ])
    }

    fn mgr() -> LeaseManager {
        // Grant expires far in the future; generation starts at 1.
        LeaseManager::new(full_grant(), 10_000_000, 1)
    }

    fn move_env(lease: &ControlLease, gen: Generation, seq: u64) -> InputEnvelope {
        InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: gen,
            seq,
            action: InputAction::PointerMove {
                display_id: 0,
                nx: 100,
                ny: 200,
                layout_version: 0,
            },
        }
    }

    #[test]
    fn issue_bumps_generation_and_installs_one_lease() {
        let mut m = mgr();
        assert_eq!(m.generation(), 1);
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        assert_eq!(m.generation(), 2);
        assert_eq!(lease.generation, 2);
        assert!(m.active_lease().is_some());
        // Capabilities are clamped to grant ∩ consent (never expands past the grant).
        assert!(lease.capabilities.contains(ras_policy::POINTER_MOVE));
    }

    #[test]
    fn happy_path_authorizes_and_advances_seq() {
        let mut m = mgr();
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        assert!(m.authorize_input(&move_env(&lease, g, 1), 1001).is_ok());
        assert!(m.authorize_input(&move_env(&lease, g, 2), 1002).is_ok());
    }

    #[test]
    fn no_lease_rejects_input() {
        let mut m = mgr();
        let env = InputEnvelope {
            lease_id: [0u8; 16],
            generation: 1,
            seq: 1,
            action: InputAction::ReleaseAllKeys,
        };
        assert_eq!(
            m.authorize_input(&env, 1).unwrap_err(),
            ControlError::NoActiveLease
        );
    }

    #[test]
    fn stale_generation_after_transfer_is_rejected() {
        // The M4 exit criterion: old-lease input rejected after transfer.
        let mut m = mgr();
        let a = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let old_gen = m.generation();
        // Transfer to a new holder bumps the generation.
        let _b = m
            .transfer(
                [2u8; 32],
                &full_grant(),
                &full_grant(),
                1500,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        assert_ne!(m.generation(), old_gen);
        // A's in-flight input at the old generation is now stale.
        let err = m
            .authorize_input(&move_env(&a, old_gen, 5), 1600)
            .unwrap_err();
        assert_eq!(err, ControlError::StaleGeneration);
    }

    #[test]
    fn wrong_lease_id_is_rejected() {
        let mut m = mgr();
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        let mut env = move_env(&lease, g, 1);
        env.lease_id = [9u8; 16];
        assert_eq!(
            m.authorize_input(&env, 1001).unwrap_err(),
            ControlError::NoActiveLease
        );
    }

    #[test]
    fn expired_lease_is_rejected() {
        let mut m = mgr();
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_MIN_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        // now well past issued_at + TTL
        let err = m
            .authorize_input(&move_env(&lease, g, 1), 1000 + LEASE_MIN_TTL_MS + 1)
            .unwrap_err();
        assert_eq!(err, ControlError::LeaseExpired);
    }

    #[test]
    fn replayed_or_reordered_seq_is_rejected() {
        let mut m = mgr();
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        assert!(m.authorize_input(&move_env(&lease, g, 5), 1001).is_ok());
        // Same seq → replay.
        assert_eq!(
            m.authorize_input(&move_env(&lease, g, 5), 1002)
                .unwrap_err(),
            ControlError::ReplayedInput
        );
        // Lower seq → reorder.
        assert_eq!(
            m.authorize_input(&move_env(&lease, g, 4), 1003)
                .unwrap_err(),
            ControlError::ReplayedInput
        );
    }

    #[test]
    fn capability_outside_the_lease_is_denied() {
        // Grant only pointer caps; a KeyEvent must be denied (Inv 15).
        let grant = caps(&[ras_policy::CONTROL_REQUEST, ras_policy::POINTER_MOVE]);
        let mut m = LeaseManager::new(grant.clone(), 10_000_000, 1);
        let lease = m
            .issue([1u8; 32], &grant, &grant, 1000, LEASE_DEFAULT_TTL_MS)
            .unwrap();
        let g = m.generation();
        let key = InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: g,
            seq: 1,
            action: InputAction::KeyEvent {
                hid_usage: 0x04,
                down: true,
                modifiers: 0,
            },
        };
        assert_eq!(
            m.authorize_input(&key, 1001).unwrap_err(),
            ControlError::CapabilityDenied
        );
        // …but a pointer move (granted) is fine.
        assert!(m.authorize_input(&move_env(&lease, g, 2), 1002).is_ok());
    }

    #[test]
    fn keyboard_text_requires_its_own_capability() {
        // `keyboard.text` is a broader "type-anything-into-focus" authority than physical keys, so it
        // has its own lease bit (ADR-082-adjacent, §2.6): a lease that grants `keyboard.key` but NOT
        // `keyboard.text` must still deny a `TextInput` (Inv 15, per-message, host-side).
        let text = |lease: &ControlLease, gen, seq| InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: gen,
            seq,
            action: InputAction::TextInput {
                utf8: ras_protocol::Redacted("你好".into()),
            },
        };

        let phys_only = caps(&[
            ras_policy::CONTROL_REQUEST,
            ras_policy::POINTER_MOVE,
            ras_policy::KEYBOARD_KEY,
        ]);
        let mut m = LeaseManager::new(phys_only.clone(), 10_000_000, 1);
        let lease = m
            .issue(
                [1u8; 32],
                &phys_only,
                &phys_only,
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        assert_eq!(
            m.authorize_input(&text(&lease, g, 1), 1001).unwrap_err(),
            ControlError::CapabilityDenied,
            "a physical-key lease must not be able to inject arbitrary Unicode text"
        );

        // With `keyboard.text` in the lease, the same TextInput is authorized.
        let with_text = caps(&[ras_policy::CONTROL_REQUEST, ras_policy::KEYBOARD_TEXT]);
        let mut m2 = LeaseManager::new(with_text.clone(), 10_000_000, 1);
        let lease2 = m2
            .issue(
                [1u8; 32],
                &with_text,
                &with_text,
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g2 = m2.generation();
        assert!(m2.authorize_input(&text(&lease2, g2, 1), 1001).is_ok());
    }

    #[test]
    fn relative_pointer_move_is_gated_on_pointer_move() {
        // Relative motion (ADR-087) is still cursor movement → same `pointer.move` cap as absolute move.
        // A lease without `pointer.move` denies it; with it, it's authorized.
        let rel = InputAction::PointerMoveRelative { dx: 3, dy: -4 };
        assert_eq!(required_cap(&rel), Some(ras_policy::POINTER_MOVE));

        let no_move = caps(&[ras_policy::CONTROL_REQUEST, ras_policy::POINTER_CLICK]);
        let mut m = LeaseManager::new(no_move.clone(), 10_000_000, 1);
        let lease = m
            .issue([1u8; 32], &no_move, &no_move, 1000, LEASE_DEFAULT_TTL_MS)
            .unwrap();
        let g = m.generation();
        let env = |gen, seq| InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: gen,
            seq,
            action: rel.clone(),
        };
        assert_eq!(
            m.authorize_input(&env(g, 1), 1001).unwrap_err(),
            ControlError::CapabilityDenied
        );

        let with_move = caps(&[ras_policy::CONTROL_REQUEST, ras_policy::POINTER_MOVE]);
        let mut m2 = LeaseManager::new(with_move.clone(), 10_000_000, 1);
        let lease2 = m2
            .issue(
                [1u8; 32],
                &with_move,
                &with_move,
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g2 = m2.generation();
        let ok_env = InputEnvelope {
            lease_id: lease2.lease_id.0,
            generation: g2,
            seq: 1,
            action: rel,
        };
        assert!(m2.authorize_input(&ok_env, 1001).is_ok());
    }

    #[test]
    fn set_lock_state_is_gated_on_the_keyboard_capability() {
        // Lock-state sync changes what the keyboard types, so it needs keyboard.key (not free like
        // ReleaseAllKeys). A pointer-only lease must not be able to flip CapsLock.
        let action = InputAction::SetLockState {
            caps_lock: true,
            num_lock: false,
        };
        assert_eq!(required_cap(&action), Some(ras_policy::KEYBOARD_KEY));

        let grant = caps(&[ras_policy::CONTROL_REQUEST, ras_policy::POINTER_MOVE]);
        let mut m = LeaseManager::new(grant.clone(), 10_000_000, 1);
        let lease = m
            .issue([1u8; 32], &grant, &grant, 1000, LEASE_DEFAULT_TTL_MS)
            .unwrap();
        let env = InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: m.generation(),
            seq: 1,
            action,
        };
        assert_eq!(
            m.authorize_input(&env, 1001).unwrap_err(),
            ControlError::CapabilityDenied
        );
    }

    #[test]
    fn release_all_keys_is_always_allowed_even_with_empty_caps() {
        // A lease with no input caps at all still admits ReleaseAllKeys (it only clears state).
        let grant = caps(&[ras_policy::CONTROL_REQUEST]);
        let mut m = LeaseManager::new(grant.clone(), 10_000_000, 1);
        let lease = m
            .issue([1u8; 32], &grant, &grant, 1000, LEASE_DEFAULT_TTL_MS)
            .unwrap();
        let g = m.generation();
        let rel = InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: g,
            seq: 1,
            action: InputAction::ReleaseAllKeys,
        };
        assert!(m.authorize_input(&rel, 1001).is_ok());
    }

    #[test]
    fn revoke_all_bumps_generation_and_makes_input_stale() {
        // Emergency stop: after revoke_all, in-flight input is stale (Inv 4).
        let mut m = mgr();
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        let new_gen = m.revoke_all();
        assert_ne!(new_gen, g);
        assert!(m.active_lease().is_none());
        assert_eq!(
            m.authorize_input(&move_env(&lease, g, 1), 1001)
                .unwrap_err(),
            ControlError::StaleGeneration
        );
    }

    #[test]
    fn revoke_if_expired_fires_once_on_an_expired_lease() {
        // The OS key-state-cleanup trigger (Inv 4): an expired lease is swept exactly once so the caller
        // can flush held keys — the gate already refuses input, so the matching key-up never arrives.
        let grant = full_grant();
        let mut m = LeaseManager::new(grant.clone(), 10_000_000, 1);
        let lease = m
            .issue([1u8; 32], &grant, &grant, 1000, LEASE_DEFAULT_TTL_MS)
            .unwrap();
        // expires_at = min(1000 + 60_000, grant_expiry) = 61_000.
        assert!(
            !m.revoke_if_expired(61_000),
            "not expired at the boundary (matches gate ③: now > expires_at)"
        );
        assert!(m.active_lease().is_some());
        // Past expiry → swept, returns true (the flush trigger).
        assert!(m.revoke_if_expired(61_001), "expired lease must be swept");
        assert!(m.active_lease().is_none());
        // Fires once — a cleared lease is not re-swept (so the flush happens exactly once).
        assert!(
            !m.revoke_if_expired(70_000),
            "a cleared lease is not re-swept"
        );
        // Input under the swept lease is stale at the gate (defense in depth).
        assert_eq!(
            m.authorize_input(&move_env(&lease, lease.generation, 1), 61_002)
                .unwrap_err(),
            ControlError::StaleGeneration
        );
    }

    #[test]
    fn issue_refuses_when_grant_lacks_control_request() {
        // A view-only grant (no control.request) can never yield an input lease (no escalation).
        let grant = caps(&[ras_policy::SCREEN_VIEW]);
        let mut m = LeaseManager::new(grant.clone(), 10_000_000, 1);
        assert_eq!(
            m.issue([1u8; 32], &grant, &grant, 1000, LEASE_DEFAULT_TTL_MS)
                .unwrap_err(),
            ControlError::NoGrantToRequest
        );
    }

    #[test]
    fn issued_caps_never_expand_past_grant_or_consent() {
        // Controller requests keyboard.key; grant allows it; but consent withheld it → not granted.
        let grant = full_grant();
        let mut m = LeaseManager::new(grant.clone(), 10_000_000, 1);
        let requested = caps(&[ras_policy::POINTER_MOVE, ras_policy::KEYBOARD_KEY]);
        let consented = caps(&[ras_policy::POINTER_MOVE]); // user consented to less
        let lease = m
            .issue(
                [1u8; 32],
                &requested,
                &consented,
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        assert!(lease.capabilities.contains(ras_policy::POINTER_MOVE));
        assert!(!lease.capabilities.contains(ras_policy::KEYBOARD_KEY));
    }

    #[test]
    fn expiry_is_clamped_to_the_grant() {
        // A max-TTL lease cannot outlive a grant that expires sooner.
        let mut m = LeaseManager::new(full_grant(), 1000 + 5000, 1); // grant expires at 6000
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_MAX_TTL_MS,
            )
            .unwrap();
        assert_eq!(lease.expires_at, 6000);
    }

    #[test]
    fn stale_layout_coordinate_is_rejected() {
        let mut m = mgr();
        m.set_layout_version(3);
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        // Coordinate computed against layout 0, but current is 3 → stale.
        let stale = InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: g,
            seq: 1,
            action: InputAction::PointerMove {
                display_id: 0,
                nx: 1,
                ny: 1,
                layout_version: 0,
            },
        };
        assert_eq!(
            m.authorize_input(&stale, 1001).unwrap_err(),
            ControlError::StaleLayout
        );
    }

    /// Pins the freshness-coverage contract of `action_layout_version` against regression: **both**
    /// coordinate-bearing actions (`PointerMove`, `PointerButton`) are gate-⑤ freshness-checked, while
    /// display-independent relative motion (`PointerMoveRelative`) is correctly exempt. If a future edit
    /// drops a coordinate action from `action_layout_version`, its `StaleLayout` assertion here fails.
    #[test]
    fn coordinate_actions_are_freshness_gated() {
        let mut m = mgr();
        m.set_layout_version(3);
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        // A PointerButton computed against layout 0 while current is 3 → stale (coordinate-bearing).
        let stale_button = InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: g,
            seq: 1,
            action: InputAction::PointerButton {
                display_id: 0,
                nx: 1,
                ny: 1,
                button: PointerButton::Left,
                down: true,
                layout_version: 0,
            },
        };
        assert_eq!(
            m.authorize_input(&stale_button, 1001).unwrap_err(),
            ControlError::StaleLayout,
            "PointerButton must be freshness-gated"
        );
        // A stale layout must NOT block relative motion — it is display-independent (no layout_version).
        // (The failed button above did not advance last_seq, so seq 1 is still valid here.)
        let rel = InputEnvelope {
            lease_id: lease.lease_id.0,
            generation: g,
            seq: 1,
            action: InputAction::PointerMoveRelative { dx: 5, dy: -5 },
        };
        assert!(
            m.authorize_input(&rel, 1002).is_ok(),
            "relative motion is display-independent and must not be blocked by a stale layout"
        );
    }

    #[test]
    fn renew_extends_expiry_without_bumping_generation() {
        let mut m = mgr();
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_MIN_TTL_MS,
            )
            .unwrap();
        let g = m.generation();
        m.renew(
            &lease.lease_id,
            1000 + LEASE_MIN_TTL_MS - 1,
            LEASE_MAX_TTL_MS,
        )
        .unwrap();
        // Same generation → the holder's input still authorizes.
        assert_eq!(m.generation(), g);
        assert!(m.active_lease().unwrap().expires_at > lease.expires_at);
    }

    #[test]
    fn cannot_renew_an_expired_lease() {
        let mut m = mgr();
        let lease = m
            .issue(
                [1u8; 32],
                &full_grant(),
                &full_grant(),
                1000,
                LEASE_MIN_TTL_MS,
            )
            .unwrap();
        let err = m
            .renew(
                &lease.lease_id,
                1000 + LEASE_MIN_TTL_MS + 1,
                LEASE_DEFAULT_TTL_MS,
            )
            .unwrap_err();
        assert_eq!(err, ControlError::LeaseExpired);
    }
}
