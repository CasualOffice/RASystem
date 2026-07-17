//! Signed-catalogue file **push** (ADR-086, `docs/20 §3.3`) — the only sanctioned file transfer.
//!
//! We deliberately do **not** build the dual-pane browse-anywhere file manager: a controller writing an
//! arbitrary host path is exactly what Invariant 6 / strategy S7 forbid. Instead the vendor pre-declares
//! a fixed **catalogue** of named [`DropTarget`]s (each a sandboxed destination + limits); a controller
//! may only push to a catalogued target, supplying **just a leaf filename** — never a path. The **host**
//! resolves the destination, so the controller never chooses where bytes land.
//!
//! This module is **pure** (no filesystem I/O): it validates the request and computes the destination
//! path. It structurally defends the three known RustDesk file-transfer CVE classes:
//! - **path-traversal / zip-slip** (`../`, absolute, drive-letter, UNC in `FileEntry.name` — PR #14678):
//!   [`validate_filename`] rejects every one, so the leaf can only ever name a child of the target dir;
//! - **capability-bleed into input/capture** (CVE-2026-58056, Inv 15): a `file.push.<target>` capability
//!   is its own namespace — [`authorize_file_push`] checks *only* that cap; it never consults or confers
//!   an input/capture capability, and the OS-input gate never maps a file action to a file cap;
//! - **symlink-follow arbitrary write** (CVE-2026-2490): path-string checks are necessary but TOCTOU-prone,
//!   so the *write backend* (deferred, on-device) MUST open with `O_NOFOLLOW` / `openat`; this module
//!   guarantees the path **string** is a safe child leaf, which is the precondition that makes that sound.

use std::path::{Path, PathBuf};

/// Maximum length (bytes) of a controller-supplied filename. Real filenames are short; this bounds the
/// DoS surface and stays well under every OS's `NAME_MAX`.
pub const MAX_FILENAME_LEN: usize = 255;

/// The capability required to push to a catalogued target `name`: `file.push.<name>`. Fine-grained per
/// target (Inv 15 / never paywalled), so granting a push to one target never authorizes another.
#[must_use]
pub fn file_push_capability(target_name: &str) -> String {
    format!("file.push.{target_name}")
}

/// One vendor-declared drop target — a named, sandboxed destination a controller may push files to
/// (`docs/20 §3.3`). The `dest_dir` is chosen by the **host/vendor**, never the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropTarget {
    /// The stable target name (the capability is `file.push.<name>`). Should itself be a simple
    /// identifier; a name with a `.` would nest the capability namespace, so keep it flat.
    pub name: String,
    /// Human description shown in the local per-transfer confirmation prompt.
    pub description: String,
    /// The sandbox directory the host writes into. The resolved path is always a direct child of this.
    pub dest_dir: PathBuf,
    /// Maximum accepted file size (bytes). A larger push is refused, never truncated.
    pub max_bytes: u64,
    /// If `Some`, only these (lowercased, dot-less) extensions are accepted; `None` = any extension.
    pub allowed_extensions: Option<Vec<String>>,
}

/// The vendor's signed catalogue of drop targets. Lookup is exact-by-name; an unknown target is
/// **refused** (fail-closed) — a controller can never invent a destination.
#[derive(Clone, Debug, Default)]
pub struct DropCatalogue {
    targets: Vec<DropTarget>,
}

impl DropCatalogue {
    /// Build from a declared target list.
    #[must_use]
    pub fn new(targets: Vec<DropTarget>) -> Self {
        Self { targets }
    }
    /// The catalogued target with this exact name, if any.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<&DropTarget> {
        self.targets.iter().find(|t| t.name == name)
    }
    /// The capabilities this catalogue makes grantable — one `file.push.<name>` per target. A deployment
    /// still has to explicitly grant them (they are **not** default-on).
    #[must_use]
    pub fn grantable_capabilities(&self) -> Vec<String> {
        self.targets
            .iter()
            .map(|t| file_push_capability(&t.name))
            .collect()
    }
}

/// A controller's request to push one file to a catalogued target. Carries **no path** — only a target
/// name, a leaf filename, and the size. The host resolves where it lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePushRequest {
    /// The catalogued target name.
    pub target: String,
    /// The bare leaf filename (validated by [`validate_filename`]; never a path).
    pub filename: String,
    /// The declared file size in bytes (checked against the target's `max_bytes`).
    pub size: u64,
}

/// Why a file push was refused. Every rejection is fail-closed; each is a stable, content-free reason
/// (never echoes the offending path back — Inv 8-adjacent hygiene).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilePushError {
    /// The target is not in the catalogue.
    UnknownTarget,
    /// The `file.push.<target>` capability is not granted (Inv 15).
    CapabilityDenied,
    /// The filename is empty, over-long, or contains a path separator / `..` / drive letter / control
    /// char / other unsafe construct — i.e. it is not a safe leaf name.
    UnsafeFilename,
    /// The file exceeds the target's `max_bytes`.
    TooLarge,
    /// The file's extension is not in the target's allow-list.
    ExtensionDenied,
}

/// Validate a controller-supplied filename as a **safe leaf** — the core CVE-class defense. Returns the
/// name unchanged on success. Rejects, fail-closed:
/// - empty or longer than [`MAX_FILENAME_LEN`];
/// - any path separator (`/` or `\`) — a filename may not name a directory;
/// - `.` or `..` (traversal);
/// - a NUL or any control character;
/// - a `:` (Windows drive / alternate-data-stream separator) — blocks `C:evil` and `name:stream`;
/// - leading/trailing ASCII whitespace, or a trailing `.` (Windows silently strips these → the file the
///   host thinks it wrote differs from what landed);
/// - a reserved Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`), with or
///   without an extension, case-insensitively.
///
/// A name that passes is guaranteed to be a single component that, joined onto any directory, stays a
/// direct child of it — so [`resolve_destination`] cannot escape the sandbox.
///
/// # Errors
/// [`FilePushError::UnsafeFilename`] on any of the above.
pub fn validate_filename(filename: &str) -> Result<&str, FilePushError> {
    if filename.is_empty() || filename.len() > MAX_FILENAME_LEN {
        return Err(FilePushError::UnsafeFilename);
    }
    if filename == "." || filename == ".." {
        return Err(FilePushError::UnsafeFilename);
    }
    if filename.contains(['/', '\\', ':', '\0']) {
        return Err(FilePushError::UnsafeFilename);
    }
    if filename.chars().any(char::is_control) {
        return Err(FilePushError::UnsafeFilename);
    }
    // Windows strips leading/trailing spaces and trailing dots — reject so the stored name is exact.
    if filename != filename.trim() || filename.ends_with('.') {
        return Err(FilePushError::UnsafeFilename);
    }
    // Reserved device names (the stem before the first `.`), case-insensitive.
    let stem = filename.split('.').next().unwrap_or(filename);
    if is_reserved_windows_name(stem) {
        return Err(FilePushError::UnsafeFilename);
    }
    Ok(filename)
}

fn is_reserved_windows_name(stem: &str) -> bool {
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];
    let upper = stem.to_ascii_uppercase();
    if RESERVED.contains(&upper.as_str()) {
        return true;
    }
    // COM1–COM9 / LPT1–LPT9.
    if let Some(rest) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    {
        return rest.len() == 1 && matches!(rest.as_bytes()[0], b'1'..=b'9');
    }
    false
}

/// The extension (lowercased, dot-less) of a validated leaf name, or `None` if it has none.
fn extension_of(filename: &str) -> Option<String> {
    Path::new(filename)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

/// Authorize a file push, host-side and fail-closed (ADR-086). Ordered checks — target exists →
/// capability granted (Inv 15) → filename is a safe leaf → size within limit → extension allowed — and,
/// on success, returns the **host-resolved** destination path (`target.dest_dir` joined with the safe
/// leaf; provably a direct child, no escape). This function **only** consults the file-push capability:
/// it never reads or grants an input/capture capability, so a file authorization can never bleed into
/// OS input (CVE-2026-58056 / Inv 15). It performs no I/O; the caller still writes with `O_NOFOLLOW`.
///
/// # Errors
/// A specific [`FilePushError`] for the first failing check.
pub fn authorize_file_push(
    catalogue: &DropCatalogue,
    granted: &crate::CapabilitySet,
    req: &FilePushRequest,
) -> Result<PathBuf, FilePushError> {
    let target = catalogue
        .lookup(&req.target)
        .ok_or(FilePushError::UnknownTarget)?;
    if !granted.contains(&file_push_capability(&target.name)) {
        return Err(FilePushError::CapabilityDenied);
    }
    let safe = validate_filename(&req.filename)?;
    if req.size > target.max_bytes {
        return Err(FilePushError::TooLarge);
    }
    if let Some(allowed) = &target.allowed_extensions {
        let ext = extension_of(safe).ok_or(FilePushError::ExtensionDenied)?;
        if !allowed.iter().any(|a| a.eq_ignore_ascii_case(&ext)) {
            return Err(FilePushError::ExtensionDenied);
        }
    }
    Ok(resolve_destination(target, safe))
}

/// Join a **validated** leaf name onto a target's sandbox directory. Because `safe_leaf` has already
/// passed [`validate_filename`] (no separators, not `..`, not absolute), the result is always a direct
/// child of `target.dest_dir` — this cannot escape the sandbox.
#[must_use]
pub fn resolve_destination(target: &DropTarget, safe_leaf: &str) -> PathBuf {
    target.dest_dir.join(safe_leaf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The load-bearing security property: `validate_filename` never panics on arbitrary input, and
        /// **any** name it accepts, joined onto a sandbox dir, is a **direct child** of that dir — it can
        /// never traverse out. This is the guarantee `resolve_destination` relies on.
        #[test]
        fn validated_names_are_always_direct_children(s in ".*") {
            match validate_filename(&s) {
                Err(_) => {} // fine — rejection is always safe
                Ok(name) => {
                    let dir = Path::new("/sandbox/incoming");
                    let joined = dir.join(name);
                    prop_assert_eq!(joined.parent(), Some(dir), "escaped the sandbox: {:?}", name);
                    prop_assert!(joined.starts_with(dir));
                }
            }
        }
    }

    fn caps(items: &[&str]) -> crate::CapabilitySet {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn catalogue() -> DropCatalogue {
        DropCatalogue::new(vec![
            DropTarget {
                name: "config_bundle".into(),
                description: "Deploy a config bundle".into(),
                dest_dir: PathBuf::from("/var/lib/app/incoming"),
                max_bytes: 1_000,
                allowed_extensions: Some(vec!["zip".into(), "json".into()]),
            },
            DropTarget {
                name: "logs".into(),
                description: "Upload logs".into(),
                dest_dir: PathBuf::from("/var/lib/app/logs"),
                max_bytes: 10_000,
                allowed_extensions: None,
            },
        ])
    }

    #[test]
    fn validate_filename_rejects_the_cve_traversal_classes() {
        for bad in [
            "",
            "..",
            ".",
            "../etc/passwd",
            "..\\..\\windows\\system32",
            "/etc/passwd",
            "\\\\host\\share\\x",
            "C:evil.exe",
            "name:stream",
            "with/slash",
            "with\\backslash",
            "nul\0byte",
            "esc\x1b[2J",
            " leading",
            "trailing ",
            "trailing.",
            "CON",
            "con.txt",
            "LPT1",
            "com9.log",
            "AUX.dat",
        ] {
            assert_eq!(
                validate_filename(bad),
                Err(FilePushError::UnsafeFilename),
                "must reject {bad:?}"
            );
        }
        // Over-long name.
        assert_eq!(
            validate_filename(&"a".repeat(MAX_FILENAME_LEN + 1)),
            Err(FilePushError::UnsafeFilename)
        );
        // Safe leaf names pass unchanged (incl. Unicode + internal dots).
        for ok in [
            "report.pdf",
            "config_bundle.zip",
            "2026-07-report.json",
            "café.txt",
            "COMET.txt",
        ] {
            assert_eq!(validate_filename(ok), Ok(ok));
        }
    }

    #[test]
    fn authorize_is_fail_closed_and_host_resolves_the_path() {
        let cat = catalogue();
        let granted = caps(&["file.push.config_bundle", "file.push.logs"]);

        // Happy path → host-resolved child of the sandbox dir.
        let dest = authorize_file_push(
            &cat,
            &granted,
            &FilePushRequest {
                target: "config_bundle".into(),
                filename: "c.zip".into(),
                size: 500,
            },
        )
        .unwrap();
        assert_eq!(dest, PathBuf::from("/var/lib/app/incoming/c.zip"));
        assert!(
            dest.starts_with("/var/lib/app/incoming"),
            "must stay in the sandbox"
        );

        // Unknown target → refused (a controller can't invent a destination).
        assert_eq!(
            authorize_file_push(
                &cat,
                &granted,
                &FilePushRequest {
                    target: "nope".into(),
                    filename: "x.zip".into(),
                    size: 1
                }
            ),
            Err(FilePushError::UnknownTarget)
        );

        // Even a valid target + safe name is denied without the per-target capability (Inv 15).
        assert_eq!(
            authorize_file_push(
                &cat,
                &caps(&["file.push.logs"]), // has logs, not config_bundle
                &FilePushRequest {
                    target: "config_bundle".into(),
                    filename: "c.zip".into(),
                    size: 1
                }
            ),
            Err(FilePushError::CapabilityDenied)
        );

        // Traversal filename → UnsafeFilename even with the capability.
        assert_eq!(
            authorize_file_push(
                &cat,
                &granted,
                &FilePushRequest {
                    target: "logs".into(),
                    filename: "../../etc/passwd".into(),
                    size: 1
                }
            ),
            Err(FilePushError::UnsafeFilename)
        );

        // Oversized → TooLarge.
        assert_eq!(
            authorize_file_push(
                &cat,
                &granted,
                &FilePushRequest {
                    target: "config_bundle".into(),
                    filename: "c.zip".into(),
                    size: 1_001
                }
            ),
            Err(FilePushError::TooLarge)
        );

        // Extension not on the allow-list → ExtensionDenied; `logs` (no restriction) accepts anything.
        assert_eq!(
            authorize_file_push(
                &cat,
                &granted,
                &FilePushRequest {
                    target: "config_bundle".into(),
                    filename: "c.exe".into(),
                    size: 1
                }
            ),
            Err(FilePushError::ExtensionDenied)
        );
        assert!(authorize_file_push(
            &cat,
            &granted,
            &FilePushRequest {
                target: "logs".into(),
                filename: "anything.bin".into(),
                size: 1
            }
        )
        .is_ok());
    }

    #[test]
    fn file_capability_is_its_own_namespace_never_input() {
        // Capability-bleed defense (CVE-2026-58056 / Inv 15): a granted file.push.* cap is a distinct
        // string and satisfies no input/capture capability — the two never overlap.
        let granted = caps(&["file.push.logs"]);
        assert!(!granted.contains(crate::KEYBOARD_KEY));
        assert!(!granted.contains(crate::POINTER_MOVE));
        assert!(!granted.contains(crate::SCREEN_VIEW));
        assert_eq!(file_push_capability("logs"), "file.push.logs");
        // The catalogue's grantable caps are exactly the per-target push caps — nothing else.
        assert_eq!(
            catalogue().grantable_capabilities(),
            vec![
                "file.push.config_bundle".to_string(),
                "file.push.logs".to_string()
            ]
        );
    }
}
