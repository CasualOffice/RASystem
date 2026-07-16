//! Local policy for Casual RAS: the capability catalogue, recognition, and intersection.
//!
//! Invariants (`CLAUDE.md` §5, `docs/04 §8`): **unknown capabilities are always denied** (Inv 2),
//! and a reduced grant **never expands** beyond what was requested, recognized, allowed, and
//! consented. Capabilities are namespaced dotted strings from a versioned, centrally-documented
//! catalogue; recognition drops anything not in it *before* any intersection, so an unrecognized
//! identifier can never survive into a grant.

use std::collections::BTreeSet;

/// A set of capability identifiers (e.g. `"screen.view"`).
///
/// Opaque strings; the catalogue below is the source of truth for which are *recognized*.
pub type CapabilitySet = BTreeSet<String>;

// ── Capability catalogue v1 (`docs/04 §8/§14`) ─────────────────────────────────────────────────
//
// Grantable in Phase 2 (view-only + visual pointer + annotation):
/// View the shared screen.
pub const SCREEN_VIEW: &str = "screen.view";
/// Choose which monitor is shared.
pub const SCREEN_SELECT_MONITOR: &str = "screen.select_monitor";
/// A rendered, non-OS-input "look here" pointer.
pub const POINTER_VIRTUAL: &str = "pointer.virtual";
/// Draw annotations over the shared screen.
pub const ANNOTATION_CREATE: &str = "annotation.create";
//
// Recognized but NOT grantable until Phase 3+ (OS input / clipboard / file / control / recording).
// They are in the catalogue so a request naming them is *understood* (and then denied by policy),
// versus an unknown identifier which is dropped outright.
/// OS pointer move (Phase 3).
pub const POINTER_MOVE: &str = "pointer.move";
/// OS pointer click (Phase 3).
pub const POINTER_CLICK: &str = "pointer.click";
/// OS pointer scroll (Phase 3).
pub const POINTER_SCROLL: &str = "pointer.scroll";
/// OS key input (Phase 3).
pub const KEYBOARD_KEY: &str = "keyboard.key";
/// OS text input (Phase 3).
pub const KEYBOARD_TEXT: &str = "keyboard.text";
/// Read the host clipboard (later).
pub const CLIPBOARD_READ: &str = "clipboard.read";
/// Write the host clipboard (later).
pub const CLIPBOARD_WRITE: &str = "clipboard.write";
/// Upload a file to the host (later).
pub const FILE_UPLOAD: &str = "file.upload";
/// Download a file from the host (later).
pub const FILE_DOWNLOAD: &str = "file.download";
/// Request a catalogued support action (later).
pub const ACTION_REQUEST: &str = "action.request";
/// Request the OS-input control lease (Phase 3).
pub const CONTROL_REQUEST: &str = "control.request";
/// Transfer the OS-input control lease (Phase 3).
pub const CONTROL_TRANSFER: &str = "control.transfer";
/// Invite another participant (later).
pub const SESSION_INVITE: &str = "session.invite";
/// Start session recording (separate, separately-consented product — ADR-052).
pub const RECORDING_START: &str = "recording.start";
/// Stop session recording.
pub const RECORDING_STOP: &str = "recording.stop";

/// The full set of **recognized** capability identifiers (catalogue version 1). Anything not here is
/// unknown and denied. Versioned: a new identifier means a new catalogue version, never a silent add.
pub const CATALOGUE_V1: &[&str] = &[
    SCREEN_VIEW,
    SCREEN_SELECT_MONITOR,
    POINTER_VIRTUAL,
    ANNOTATION_CREATE,
    POINTER_MOVE,
    POINTER_CLICK,
    POINTER_SCROLL,
    KEYBOARD_KEY,
    KEYBOARD_TEXT,
    CLIPBOARD_READ,
    CLIPBOARD_WRITE,
    FILE_UPLOAD,
    FILE_DOWNLOAD,
    ACTION_REQUEST,
    CONTROL_REQUEST,
    CONTROL_TRANSFER,
    SESSION_INVITE,
    RECORDING_START,
    RECORDING_STOP,
];

/// The capabilities the **MVP host** will actually grant (Phase 2 = view-only + visual pointer +
/// annotation). This is the default host policy; a deployment may narrow it, never widen past the
/// catalogue. Input/clipboard/file/control/recording are recognized but withheld until their phase.
pub const PHASE2_GRANTABLE: &[&str] = &[
    SCREEN_VIEW,
    SCREEN_SELECT_MONITOR,
    POINTER_VIRTUAL,
    ANNOTATION_CREATE,
];

/// The recognized-capability catalogue as a set.
#[must_use]
pub fn catalogue_v1() -> CapabilitySet {
    CATALOGUE_V1.iter().map(|s| (*s).to_string()).collect()
}

/// The capabilities the **MVP host** grants once OS input lands (Phase 3 = the Phase-2 view-only set
/// **plus** OS pointer + physical-key input, gated behind a control lease). `keyboard.text` (arbitrary
/// Unicode injection), clipboard, file transfer, support actions, and recording stay **withheld by
/// default** — a deployment may widen policy up to the catalogue, never past it. Physical
/// `keyboard.key` is the default keyboard path, so shortcuts/keys work without exposing arbitrary
/// text injection by default (`docs/design/phase-3-design.md §2.3/§10`).
pub const PHASE3_GRANTABLE: &[&str] = &[
    // Phase-2 view-only + visual pointer + annotation …
    SCREEN_VIEW,
    SCREEN_SELECT_MONITOR,
    POINTER_VIRTUAL,
    ANNOTATION_CREATE,
    // … plus OS input behind a lease:
    POINTER_MOVE,
    POINTER_CLICK,
    POINTER_SCROLL,
    KEYBOARD_KEY,
    CONTROL_REQUEST,
    CONTROL_TRANSFER,
];

/// The MVP host's default grantable policy (view-only + visual pointer + annotation).
#[must_use]
pub fn phase2_default_policy() -> CapabilitySet {
    PHASE2_GRANTABLE.iter().map(|s| (*s).to_string()).collect()
}

/// The Phase-3 host's default grantable policy: view-only + visual pointer + annotation **plus**
/// OS pointer/physical-key input (each still only usable while the controller holds a lease and only
/// after the per-message host-side gate — `ras-control`, Inv 15). `keyboard.text` and everything past
/// input stay withheld unless a deployment explicitly widens policy.
#[must_use]
pub fn phase3_default_policy() -> CapabilitySet {
    PHASE3_GRANTABLE.iter().map(|s| (*s).to_string()).collect()
}

/// Whether an identifier is in the recognized catalogue.
#[must_use]
pub fn is_recognized(cap: &str) -> bool {
    CATALOGUE_V1.contains(&cap)
}

/// Drop every requested identifier not in the versioned catalogue (default-deny unknown, Inv 2).
///
/// This runs **before** any policy/consent intersection, so an unrecognized capability can never
/// survive into a grant regardless of what a buggy or hostile policy/consent set contains.
#[must_use]
pub fn recognize(requested: &CapabilitySet) -> CapabilitySet {
    requested
        .iter()
        .filter(|c| is_recognized(c))
        .cloned()
        .collect()
}

/// Intersect the controller's `requested` capabilities with what local `policy` allows.
///
/// Only capabilities present in **both** are granted. The result is always a subset of both.
#[must_use]
pub fn intersect(requested: &CapabilitySet, policy: &CapabilitySet) -> CapabilitySet {
    requested.intersection(policy).cloned().collect()
}

/// The capabilities a grant may carry: `recognize(requested) ∩ policy ∩ consented`.
///
/// Recognition first (unknown-denied), then the host policy, then what the local user actually
/// consented to. The result never expands beyond any of the three inputs (property-tested).
#[must_use]
pub fn grantable(
    requested: &CapabilitySet,
    policy: &CapabilitySet,
    consented: &CapabilitySet,
) -> CapabilitySet {
    let recognized = recognize(requested);
    intersect(&intersect(&recognized, policy), consented)
}

/// The direction a clipboard-text push travels, for host-side capability enforcement (ADR-076).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardDirection {
    /// Controller → host: the controller sets the **host's** OS clipboard. The dangerous direction —
    /// the Reverse-RDP / RustDesk clipboard-injection class — gated on [`CLIPBOARD_WRITE`].
    ControllerToHost,
    /// Host → controller: the host shares its **own** clipboard to the controller — gated on
    /// [`CLIPBOARD_READ`].
    HostToController,
}

/// Host-side, per-message clipboard authorization (Inv 15): may a clipboard-text push in this
/// `direction` proceed, given the session's already-`granted` capabilities? The peer's claim is
/// never trusted — only the host's granted set decides.
///
/// This is **authorization only**. The load-bearing *no-auto-paste* rule — the receiver populates the
/// OS clipboard but never injects a paste keystroke — is a receiver-side invariant enforced where the
/// clipboard is actually set (the OS clipboard backend), not here (ADR-076).
#[must_use]
pub fn clipboard_push_allowed(direction: ClipboardDirection, granted: &CapabilitySet) -> bool {
    let cap = match direction {
        ClipboardDirection::ControllerToHost => CLIPBOARD_WRITE,
        ClipboardDirection::HostToController => CLIPBOARD_READ,
    };
    granted.contains(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> CapabilitySet {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn unknown_capabilities_are_denied() {
        let requested = set(&["screen.view", "keyboard.key", "totally.unknown"]);
        let policy = set(&["screen.view", "pointer.virtual"]);
        let granted = intersect(&requested, &policy);
        assert_eq!(granted, set(&["screen.view"]));
        assert!(!granted.contains("totally.unknown"));
        assert!(!granted.contains("keyboard.key"));
    }

    #[test]
    fn reduced_grant_never_expands() {
        let requested = set(&["screen.view"]);
        let policy = set(&["screen.view", "keyboard.key", "pointer.click"]);
        let granted = intersect(&requested, &policy);
        assert!(granted.is_subset(&requested));
        assert!(granted.is_subset(&policy));
    }

    #[test]
    fn recognize_drops_unknown_but_keeps_known_non_grantable() {
        // `keyboard.key` is recognized (in the catalogue) even though it is not grantable in Phase 2;
        // `made.up` is not recognized and is dropped.
        let requested = set(&["screen.view", "keyboard.key", "made.up", "x.y.z"]);
        let recognized = recognize(&requested);
        assert_eq!(recognized, set(&["screen.view", "keyboard.key"]));
    }

    #[test]
    fn grantable_never_expands_past_any_input() {
        let requested = set(&["screen.view", "pointer.virtual", "keyboard.key", "made.up"]);
        let policy = phase2_default_policy();
        let consented = set(&["screen.view", "keyboard.key"]); // user consented to less
        let granted = grantable(&requested, &policy, &consented);
        // Only screen.view survives: recognized ∩ policy (view-only) ∩ consent.
        assert_eq!(granted, set(&["screen.view"]));
        assert!(granted.is_subset(&recognize(&requested)));
        assert!(granted.is_subset(&policy));
        assert!(granted.is_subset(&consented));
    }

    #[test]
    fn grantable_withholds_input_even_if_requested_and_consented() {
        // Phase-2 policy withholds OS input regardless of request/consent (Phase-3 gate).
        let requested = set(&["screen.view", "keyboard.key", "pointer.click"]);
        let consented = requested.clone(); // user said yes to everything asked
        let granted = grantable(&requested, &phase2_default_policy(), &consented);
        assert_eq!(granted, set(&["screen.view"]));
        assert!(!granted.contains("keyboard.key"));
        assert!(!granted.contains("pointer.click"));
    }

    #[test]
    fn an_unknown_cap_cannot_survive_a_permissive_policy_and_consent() {
        // Even if a (buggy) policy and consent both name an unknown id, recognition drops it first.
        let requested = set(&["made.up"]);
        let policy = set(&["made.up", "screen.view"]);
        let consented = set(&["made.up"]);
        assert!(grantable(&requested, &policy, &consented).is_empty());
    }

    #[test]
    fn catalogue_and_default_policy_are_consistent() {
        // Everything grantable must be recognized.
        let cat = catalogue_v1();
        for cap in phase2_default_policy() {
            assert!(
                cat.contains(&cap),
                "grantable cap {cap} must be in the catalogue"
            );
        }
    }

    #[test]
    fn phase3_default_policy_grants_os_input_but_still_withholds_text_and_beyond() {
        let policy = phase3_default_policy();
        // OS pointer + physical key input become grantable in Phase 3 …
        for cap in [
            SCREEN_VIEW,
            POINTER_VIRTUAL,
            ANNOTATION_CREATE,
            POINTER_MOVE,
            POINTER_CLICK,
            POINTER_SCROLL,
            KEYBOARD_KEY,
            CONTROL_REQUEST,
            CONTROL_TRANSFER,
        ] {
            assert!(policy.contains(cap), "phase 3 should grant {cap}");
        }
        // … but arbitrary Unicode text, clipboard, file, actions, recording stay withheld by default.
        for cap in [
            KEYBOARD_TEXT,
            CLIPBOARD_READ,
            CLIPBOARD_WRITE,
            FILE_UPLOAD,
            FILE_DOWNLOAD,
            ACTION_REQUEST,
            SESSION_INVITE,
            RECORDING_START,
            RECORDING_STOP,
        ] {
            assert!(
                !policy.contains(cap),
                "phase 3 must withhold {cap} by default"
            );
        }
    }

    #[test]
    fn phase3_policy_is_a_superset_of_phase2_and_within_the_catalogue() {
        let cat = catalogue_v1();
        let p2 = phase2_default_policy();
        let p3 = phase3_default_policy();
        // Never a regression: Phase 3 grants everything Phase 2 did, and more.
        assert!(p2.is_subset(&p3));
        // Never past the catalogue (unknown-denied holds for the policy set itself).
        assert!(p3.is_subset(&cat));
    }

    #[test]
    fn phase3_grantable_still_drops_unknown_and_never_expands() {
        // A request naming an unknown id plus a withheld cap: recognition + policy still contain it.
        let requested = set(&["screen.view", "keyboard.key", "keyboard.text", "made.up"]);
        let consented = requested.clone();
        let granted = grantable(&requested, &phase3_default_policy(), &consented);
        // keyboard.key survives (grantable now); keyboard.text withheld by policy; made.up unrecognized.
        assert_eq!(granted, set(&["keyboard.key", "screen.view"]));
        assert!(granted.is_subset(&recognize(&requested)));
        assert!(granted.is_subset(&phase3_default_policy()));
    }

    #[test]
    fn clipboard_push_gated_per_direction_and_default_denied() {
        // No clipboard caps granted → both directions refused (default OFF).
        let none = set(&["screen.view", "keyboard.key"]);
        assert!(!clipboard_push_allowed(
            ClipboardDirection::ControllerToHost,
            &none
        ));
        assert!(!clipboard_push_allowed(
            ClipboardDirection::HostToController,
            &none
        ));

        // write authorizes only controller→host; read only host→controller — never crosswired.
        let write = set(&[CLIPBOARD_WRITE]);
        assert!(clipboard_push_allowed(
            ClipboardDirection::ControllerToHost,
            &write
        ));
        assert!(!clipboard_push_allowed(
            ClipboardDirection::HostToController,
            &write
        ));

        let read = set(&[CLIPBOARD_READ]);
        assert!(clipboard_push_allowed(
            ClipboardDirection::HostToController,
            &read
        ));
        assert!(!clipboard_push_allowed(
            ClipboardDirection::ControllerToHost,
            &read
        ));
    }

    #[test]
    fn clipboard_caps_are_recognized_but_withheld_by_default_policy() {
        // Both clipboard caps are in the catalogue …
        assert!(is_recognized(CLIPBOARD_READ) && is_recognized(CLIPBOARD_WRITE));
        // … but neither is grantable under the Phase-3 default policy (default OFF, docs/20 §2.3).
        let p3 = phase3_default_policy();
        assert!(!p3.contains(CLIPBOARD_READ) && !p3.contains(CLIPBOARD_WRITE));
        // So even a controller that requests + consents to them gets nothing.
        let requested = set(&[CLIPBOARD_READ, CLIPBOARD_WRITE, "screen.view"]);
        let granted = grantable(&requested, &p3, &requested);
        assert!(!granted.contains(CLIPBOARD_READ) && !granted.contains(CLIPBOARD_WRITE));
    }
}
