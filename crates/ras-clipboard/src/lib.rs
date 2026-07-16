//! Cross-platform host clipboard backend (ADR-079).
//!
//! A [`ras_control::ClipboardSink`] over [`arboard`]: it **sets** the host's OS clipboard and **never
//! pastes** — the no-auto-paste rule (ADR-076) enforced at the mechanism, not just by policy. It is
//! only ever reached after the host-side capability gate (`clipboard.write`, Inv 15) approves an
//! inbound push, so this crate performs no authorization itself. The clipboard text is a secret and is
//! passed straight to the OS — never logged (Inv 8).

use std::sync::Mutex;

use ras_control::{ClipboardSink, InputError};
use ras_protocol::ErrorCode;

/// A [`ClipboardSink`] backed by [`arboard`]. Holds the clipboard handle for the process lifetime: on
/// Linux/X11 the handle owns and *serves* the CLIPBOARD selection, so it must outlive each `set_text`
/// (dropping it can drop the offered content). `arboard::Clipboard` is not `Sync`, so a [`Mutex`]
/// guards it — clipboard pushes are rare and non-latency-critical, so the lock is free.
pub struct ArboardClipboardSink {
    inner: Mutex<arboard::Clipboard>,
}

impl ArboardClipboardSink {
    /// Open the OS clipboard. **Fails closed** if it is unavailable (e.g. no display / no clipboard
    /// server): the host then wires no sink and refuses clipboard pushes, rather than pretending.
    ///
    /// # Errors
    /// The platform clipboard could not be opened.
    pub fn new() -> Result<Self, InputError> {
        let clipboard = arboard::Clipboard::new()
            .map_err(|_| InputError::fatal(ErrorCode::InputFailed, "clipboard unavailable"))?;
        Ok(Self {
            inner: Mutex::new(clipboard),
        })
    }
}

impl ClipboardSink for ArboardClipboardSink {
    fn set_text(&self, text: &str) -> Result<(), InputError> {
        // Set the OS clipboard only — no paste is ever injected (ADR-076). Never log `text` (Inv 8);
        // the error carries a static message, never the content.
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .set_text(text)
            .map_err(|_| InputError::fatal(ErrorCode::InputFailed, "clipboard set failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opening the clipboard must never panic — it returns `Ok` where a clipboard exists (dev macOS)
    /// or a typed `Err` in a headless CI environment (no X server). Deliberately does **not** call
    /// `set_text` (that would clobber the CI machine's clipboard). Verifies the fail-closed contract.
    #[test]
    fn new_is_fail_closed_and_never_panics() {
        match ArboardClipboardSink::new() {
            Ok(_) => {}
            Err(e) => assert_eq!(e.code, ErrorCode::InputFailed),
        }
    }
}
