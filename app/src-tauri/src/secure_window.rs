//! Exclude Casual RAS's own windows from OS screen capture / recording — the "secure window"
//! affinity that banking apps and password managers use.
//!
//! A remote viewer sees the shared *screen*, and any local recorder (OBS, Zoom, QuickTime, the OS
//! screenshot tool) can capture it too. Sensitive Casual RAS surfaces — the local Allow/Deny consent
//! dialog, a pairing code, in-session chat, a clipboard preview, and (on the Connect side) the
//! *remote* screen feed itself — should never leak into such a capture. We set the per-window OS
//! "do not capture" affinity so those windows render normally to the local user but appear blank to
//! any capture pipeline.
//!
//! Platform support:
//! - **macOS**: `NSWindow.sharingType = NSWindowSharingNone`.
//! - **Windows**: `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` (Windows 10 2004+).
//! - **Linux**: no reliable X11/Wayland primitive exists — a best-effort no-op (documented, not a
//!   silent claim). The Linux build simply remains capturable.
//!
//! **Invariant 7 is preserved.** Excluding a window from *capture* does not hide it from the local
//! user's own eyes: the always-visible "REMOTE … ACTIVE" indicator and the Stop control stay on
//! screen for the human at the machine. Only recordings / remote streams lose them — which is the
//! point (an attacker who is remotely viewing cannot screenshot the indicator away, and a screen
//! recording of a support session never captures the other party's secrets).
//!
//! Best-effort by design: a failure to apply the affinity is swallowed (never crashes the app or
//! blocks a session) — that window is then merely capturable, no worse than an ordinary window.

use tauri::WebviewWindow;

/// Make `window` invisible to screen capture / recording on platforms that support it. Safe to call
/// on a not-yet-shown window (uses the native window handle, which exists once the window is created;
/// unlike GTK realization-sensitive calls, it needs no prior `show()`).
pub fn exclude_from_capture(window: &WebviewWindow) {
    #[cfg(target_os = "macos")]
    macos::exclude(window);
    #[cfg(target_os = "windows")]
    windows_impl::exclude(window);
    #[cfg(target_os = "linux")]
    {
        // No supported X11/Wayland primitive — see the module docs. Intentionally a no-op.
        let _ = window;
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::runtime::AnyObject;
    use tauri::WebviewWindow;

    // NSWindowSharingType::None = 0 — the window is excluded from screen sharing / capture.
    const NS_WINDOW_SHARING_NONE: isize = 0;

    pub(super) fn exclude(window: &WebviewWindow) {
        // `ns_window()` yields the NSWindow pointer once the window exists (created during
        // `Builder::build`, before it is ordered on-screen). No realization requirement.
        let Ok(ptr) = window.ns_window() else {
            return;
        };
        if ptr.is_null() {
            return;
        }
        let ns_window = ptr as *mut AnyObject;
        // SAFETY: `ns_window` is a live NSWindow* owned by Tauri for this window's lifetime;
        // `-setSharingType:` is a standard NSWindow setter taking an NSInteger. We only borrow it to
        // send this one message and do not retain it.
        unsafe {
            let _: () = objc2::msg_send![ns_window, setSharingType: NS_WINDOW_SHARING_NONE];
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use tauri::WebviewWindow;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };

    pub(super) fn exclude(window: &WebviewWindow) {
        // Recover the raw HWND via raw-window-handle so we don't couple to Tauri's internal
        // windows-rs version.
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(w) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(w.hwnd.get() as *mut core::ffi::c_void);
        // SAFETY: `hwnd` is a live top-level window owned by Tauri for this window's lifetime.
        // WDA_EXCLUDEFROMCAPTURE (Windows 10 2004+) renders the window normally to the user but blank
        // to any capture. A pre-2004 OS returns an error, which we ignore (the window stays
        // capturable, as before) — best-effort, never fatal.
        unsafe {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
    }
}
