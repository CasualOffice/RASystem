//! Windows host-side **cursor-shape capture** backend (ADR-073) behind [`ras_core::CursorObserver`].
//!
//! The host cursor task polls this observer for the live OS cursor; each reported [`CursorFrame`] is
//! forwarded on the already-built cursor-shape channel — a fresh [`ras_core::CursorShape`] the first
//! time an id is seen, else a `CursorCached` reference — and the controller renders it client-side at
//! zero latency (display data, never input; outside Inv 6). Cursor pixels never touch a log:
//! `CursorShape::Debug` elides the RGBA (handled in `ras-core`), and this crate logs nothing.
//!
//! This is an FFI-bearing platform crate (CONTRIBUTING §5): the workspace's `unsafe_code = deny` is
//! relaxed to `allow` **here only**, with `unsafe` confined behind the safe [`ras_core::CursorObserver`]
//! trait surface — no raw pointers/handles escape. It uses the Microsoft `windows` (windows-rs)
//! bindings (the same family/version as `ras-input-windows`).
//!
//! On non-Windows targets the crate is intentionally **empty** so `cargo build --workspace` stays green
//! on macOS/Linux CI (the `windows` dependency is `cfg(target_os = "windows")`-gated).

#[cfg(target_os = "windows")]
mod imp;
#[cfg(target_os = "windows")]
pub use imp::WinCursorObserver;
