//! Linux (X11) host-side **cursor-shape capture** backend (ADR-073) behind [`ras_core::CursorObserver`],
//! the sibling of `ras-cursor-macos`.
//!
//! The host cursor task polls this observer for the live OS cursor; each reported [`ras_core::CursorFrame`]
//! is forwarded on the already-built cursor-shape channel — a fresh [`ras_core::CursorShape`] the first
//! time an id is seen, else a `CursorCached` reference — and the controller renders it client-side at zero
//! latency (display data, never input; outside Inv 6). Cursor pixels never touch a log: this crate logs
//! nothing, and `CursorShape::Debug` elides the RGBA (handled in `ras-core`).
//!
//! Unlike `ras-cursor-macos`, this backend is **`unsafe`-free**: it uses the pure-Rust `x11rb` client
//! (no C X11 libs) and the XFixes extension, so it keeps the workspace lint defaults. It is deliberately
//! **unprivileged and fail-closed** (like `ras-input-linux`): it connects to `$DISPLAY` as the logged-in
//! user (X11 / Xwayland only, no root). With no reachable X server — or no XFixes — it simply yields no
//! frames (the observer ends), so the host has no host cursor to forward rather than a wrong one.
//!
//! On non-Linux targets the crate is intentionally **empty** so `cargo build --workspace` stays green on
//! macOS/Windows CI (the `x11rb` dependency is `cfg(target_os = "linux")`-gated).

#[cfg(target_os = "linux")]
mod imp;
#[cfg(target_os = "linux")]
pub use imp::X11CursorObserver;
