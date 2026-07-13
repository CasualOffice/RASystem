//! Local policy for Casual RAS: capability intersection and (later) consent/duration rules.
//!
//! Invariant (`CLAUDE.md` §5.2, `docs/04 §8`): unknown capabilities are always denied, and a
//! reduced grant never expands beyond what was requested or what policy allows.

use std::collections::BTreeSet;

/// A set of capability identifiers (e.g. `"screen.view"`).
///
/// Opaque strings for now; a typed, versioned registry lands with the protobuf message set.
pub type CapabilitySet = BTreeSet<String>;

/// Intersect the controller's `requested` capabilities with what local `policy` allows.
///
/// Only capabilities present in **both** are granted. A requested capability absent from `policy`
/// — including any unknown/unrecognized identifier — is denied. The result is always a subset of
/// both `requested` and `policy`.
#[must_use]
pub fn intersect(requested: &CapabilitySet, policy: &CapabilitySet) -> CapabilitySet {
    requested.intersection(policy).cloned().collect()
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
}
