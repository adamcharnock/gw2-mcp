//! Per-bottle file lock used to ensure exactly one in-bottle Mumble Link
//! holder runs per bottle, even when multiple `gw2-mcp` instances are
//! started concurrently against the same bottle.
//!
//! The lock is BSD-style advisory `flock(2)` on a per-bottle file:
//! `<bottle>/drive_c/users/Public/gw2-mcp/holder.lock`. The leader
//! holds the lock for the lifetime of its process; the kernel releases
//! it automatically on exit (clean OR crash), which gives us
//! lossless leader re-election with no stale-pid-file cleanup logic.
//!
//! The lockfile body is the leader's `session_token` (hex). The *next*
//! leader reads it to identify orphans from the previous leader — a
//! fingerprint-scoped sweep that cannot false-positive on another
//! gw2-mcp's live holder.
//!
//! On non-macOS targets this module is a no-op stub so callers don't
//! need cfg gymnastics.

use std::path::Path;

#[cfg(target_os = "macos")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "macos")]
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};

#[cfg(target_os = "macos")]
use fs2::FileExt as _;

/// Owns the open file descriptor + advisory exclusive `flock`.
///
/// `Drop` closes the fd, which atomically releases the lock — this is
/// the property that lets the next gw2-mcp promote itself when the
/// current leader dies (cleanly or otherwise).
pub struct HolderLock {
    #[cfg(target_os = "macos")]
    file: File,
}

impl HolderLock {
    /// Try to take the exclusive lock at `path` without blocking.
    ///
    /// Creates the file if it doesn't exist. Returns `Ok(Some(lock))`
    /// on success, `Ok(None)` if another process already holds it,
    /// and `Err` only for I/O errors that aren't "lock contention"
    /// (e.g. EACCES on the lockfile path).
    #[cfg(target_os = "macos")]
    pub fn try_acquire(path: &Path) -> std::io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        // `fs2::FileExt::try_lock_exclusive` is a safe wrapper around
        // `flock(LOCK_EX | LOCK_NB)`. The lock is released when `file`
        // drops (via the kernel's per-fd flock semantics), so storing
        // the `File` in our struct keeps it held for the lifetime of
        // `HolderLock` and no longer.
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn try_acquire(_path: &Path) -> std::io::Result<Option<Self>> {
        Ok(None)
    }

    /// Read the session token previously written by another leader, if
    /// any. Returns `Ok(None)` for an empty/missing-content lockfile —
    /// the common first-run case.
    #[cfg(target_os = "macos")]
    pub fn read_previous_token(&mut self) -> std::io::Result<Option<String>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut buf = String::new();
        self.file.read_to_string(&mut buf)?;
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_owned()))
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn read_previous_token(&mut self) -> std::io::Result<Option<String>> {
        Ok(None)
    }

    /// Overwrite the lockfile with the new leader's session token.
    /// Truncates first so a longer-then-shorter sequence doesn't leave
    /// trailing garbage.
    #[cfg(target_os = "macos")]
    pub fn write_token(&mut self, token: &str) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.set_len(0)?;
        self.file.write_all(token.as_bytes())?;
        self.file.sync_data()?;
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    pub fn write_token(&mut self, _token: &str) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn try_acquire_creates_missing_file_and_parent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/holder.lock");
        let lock = HolderLock::try_acquire(&path)
            .expect("acquire succeeds")
            .expect("got the lock");
        drop(lock);
        assert!(path.exists(), "lockfile was created on first acquire");
    }

    #[test]
    fn second_acquire_returns_none_while_first_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("holder.lock");
        let first = HolderLock::try_acquire(&path).unwrap().unwrap();
        let second = HolderLock::try_acquire(&path).unwrap();
        assert!(
            second.is_none(),
            "contended acquire returns None, not an error"
        );
        drop(first);
        let third = HolderLock::try_acquire(&path).unwrap();
        assert!(third.is_some(), "lock is reacquirable after drop");
    }

    #[test]
    fn token_roundtrips_through_lockfile() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("holder.lock");
        let mut lock = HolderLock::try_acquire(&path).unwrap().unwrap();
        assert_eq!(
            lock.read_previous_token().unwrap(),
            None,
            "fresh lockfile has no token"
        );
        lock.write_token("deadbeefcafebabe").unwrap();
        // Re-read via the same handle to confirm seek/write/read offsets
        // are consistent (the supervisor reads BEFORE writing on take-over).
        assert_eq!(
            lock.read_previous_token().unwrap().as_deref(),
            Some("deadbeefcafebabe")
        );
    }

    #[test]
    fn next_leader_sees_previous_token() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("holder.lock");
        {
            let mut first = HolderLock::try_acquire(&path).unwrap().unwrap();
            first.write_token("aaaaaaaabbbbbbbb").unwrap();
        }
        let mut second = HolderLock::try_acquire(&path).unwrap().unwrap();
        assert_eq!(
            second.read_previous_token().unwrap().as_deref(),
            Some("aaaaaaaabbbbbbbb"),
            "previous leader's token is visible after lock release"
        );
    }

    #[test]
    fn empty_lockfile_reads_as_no_token() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("holder.lock");
        std::fs::write(&path, "   \n").unwrap();
        let mut lock = HolderLock::try_acquire(&path).unwrap().unwrap();
        assert_eq!(lock.read_previous_token().unwrap(), None);
    }
}
