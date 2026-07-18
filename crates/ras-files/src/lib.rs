//! Safe file-write backend for the signed-catalogue file transfer (ADR-090, `docs/20 §3.3`).
//!
//! [`SafeFileWriter`] writes an accepted transfer's bytes to a **host-resolved** destination — a
//! validated leaf inside the target's sandbox (the controller never supplies a path). It opens with
//! `O_NOFOLLOW | O_CREAT | O_EXCL`, so:
//! - a **symlink** at the destination is refused (`O_NOFOLLOW`) — the symlink-follow / TOCTOU CVE-class
//!   defense (CVE-2026-2490) that the safe-leaf path string (ADR-086) is the precondition for;
//! - an **existing** entry is refused (`O_EXCL`) — a push never overwrites/clobbers an existing file.
//!
//! It is deliberately **not** coupled to the heavy `ras-core` crate: the app wraps this in a
//! `ras_core::FileWriteSink` (mapping `io::Error` → the core error). One transfer at a time. Unix-only
//! for now — Windows needs a `CreateFile` + reparse-point check and compiles to an empty lib here, so
//! `cargo build --workspace` stays green on Windows CI. `unsafe`-free (uses `std::os::unix` + `libc`).
//!
//! **Caveat (surfaced honestly):** `O_NOFOLLOW` protects only the **final** path component. The sandbox
//! *directory* must be host-owned and not attacker-writable; a fully hardened impl would resolve each
//! component with `openat`. For the MVP the host-chosen sandbox dir satisfies that.

#[cfg(unix)]
mod unix {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write as _};
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct Open {
        file: File,
        dest: PathBuf,
    }

    /// A one-transfer-at-a-time writer that lands bytes at a host-resolved destination safely (ADR-090).
    /// `open` → `write`* → `finish`; `abort` discards the partial file. `&self` throughout (interior
    /// `Mutex`) so it drops straight into a `ras_core::FileWriteSink`.
    #[derive(Default)]
    pub struct SafeFileWriter {
        state: Mutex<Option<Open>>,
    }

    impl SafeFileWriter {
        /// A fresh writer with no open transfer.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Open `dest` for a new transfer of `_size` bytes. `O_NOFOLLOW` (refuse a symlink) + `O_CREAT |
        /// O_EXCL` (refuse an existing entry) at mode `0600`. Replaces any prior open transfer.
        ///
        /// # Errors
        /// The destination is a symlink or already exists, or any other open failure (permission, ENOENT
        /// on a missing sandbox dir, …).
        pub fn open(&self, dest: &Path, _size: u64) -> io::Result<()> {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true) // O_CREAT | O_EXCL — never follow/overwrite an existing entry
                .custom_flags(libc::O_NOFOLLOW) // the final component may not be a symlink
                .mode(0o600)
                .open(dest)?;
            *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(Open {
                file,
                dest: dest.to_path_buf(),
            });
            Ok(())
        }

        /// Append one chunk to the open transfer.
        ///
        /// # Errors
        /// No transfer is open, or the write fails.
        pub fn write(&self, data: &[u8]) -> io::Result<()> {
            let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match g.as_mut() {
                Some(o) => o.file.write_all(data),
                None => Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "no open transfer",
                )),
            }
        }

        /// Finalize the completed file (`fsync` + close).
        ///
        /// # Errors
        /// No transfer is open, or the flush/close fails.
        pub fn finish(&self) -> io::Result<()> {
            let o = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no open transfer"))?;
            o.file.sync_all()
        }

        /// Abort the transfer and remove the partial file. Idempotent, never fails.
        pub fn abort(&self) {
            if let Some(o) = self.state.lock().unwrap_or_else(|e| e.into_inner()).take() {
                drop(o.file);
                let _ = std::fs::remove_file(&o.dest);
            }
        }
    }
}

#[cfg(unix)]
pub use unix::SafeFileWriter;

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::SafeFileWriter;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ras-files-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_chunks_and_finalizes() {
        let dir = tmp_dir("write");
        let dest = dir.join("out.bin");
        let w = SafeFileWriter::new();
        w.open(&dest, 11).unwrap();
        w.write(b"hello ").unwrap();
        w.write(b"world").unwrap();
        w.finish().unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_an_existing_file() {
        // O_EXCL: a push never overwrites an existing entry.
        let dir = tmp_dir("exists");
        let dest = dir.join("out.bin");
        std::fs::write(&dest, b"original").unwrap();
        let w = SafeFileWriter::new();
        assert!(w.open(&dest, 1).is_err(), "must refuse an existing file");
        assert_eq!(std::fs::read(&dest).unwrap(), b"original", "left untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_a_symlink_destination() {
        // O_NOFOLLOW: the symlink-follow (TOCTOU) CVE-class defense — a symlink at dest is refused, so a
        // push can never be redirected to write through a link the attacker planted.
        let dir = tmp_dir("symlink");
        let target = dir.join("secret.txt");
        std::fs::write(&target, b"do not clobber").unwrap();
        let dest = dir.join("out.bin");
        std::os::unix::fs::symlink(&target, &dest).unwrap();
        let w = SafeFileWriter::new();
        assert!(
            w.open(&dest, 1).is_err(),
            "must refuse a symlink destination"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"do not clobber",
            "the symlink target is never written through"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn abort_removes_the_partial_file() {
        let dir = tmp_dir("abort");
        let dest = dir.join("out.bin");
        let w = SafeFileWriter::new();
        w.open(&dest, 100).unwrap();
        w.write(b"partial").unwrap();
        w.abort();
        assert!(!dest.exists(), "the partial file is removed on abort");
        // Abort is idempotent + a write after abort errors (no open transfer).
        w.abort();
        assert!(w.write(b"x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
