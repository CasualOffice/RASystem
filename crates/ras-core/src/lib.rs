//! Casual RAS core: session orchestration and state machines (skeleton).
//!
//! Ties together identity, grants, policy, control, media, audit, and transport. The session and
//! control state machines from `docs/03 §10` land here across Phases 1–4. This crate re-exports the
//! subsystem crates so downstream consumers (and, later, the FFI/SDK layer) have one entry point.

pub use ras_audit as audit;
pub use ras_control as control;
pub use ras_grant as grant;
pub use ras_identity as identity;
pub use ras_media as media;
pub use ras_policy as policy;
pub use ras_protocol as protocol;
pub use ras_transport_iroh as transport;

/// The runtime version (from `Cargo.toml`).
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn subsystems_are_wired() {
        // Exercises the re-exports so the crate graph is verified at build time.
        assert_eq!(super::protocol::PROTOCOL_VERSION, 1);
        assert!(!super::version().is_empty());
    }
}
