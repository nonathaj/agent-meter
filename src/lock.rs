//! Inter-process locks.
//!
//! Two kinds are needed:
//! - [`FileLock`]: an OS advisory lock on a file, used to serialize agent-meter
//!   processes (CLI, TUI and watcher) against each other.
//! - [`DirLock`]: the `mkdir`-based lock protocol used by Claude Code (the
//!   `proper-lockfile` npm package). Holding Claude Code's locks while
//!   swapping credentials keeps it from refreshing or rewriting them mid-swap.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};

/// An exclusive advisory lock on a file, released on drop.
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Blocks until the lock is acquired or `timeout` elapses.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        let file = open_lock_file(path)?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(anyhow!(
                        "timed out after {timeout:?} waiting for lock {} (is another agent-meter busy?)",
                        path.display()
                    ));
                }
                Err(TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
    }

    /// Acquires the lock only if it is free right now.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = open_lock_file(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
        }
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock file {}", path.display()))
}

/// A `proper-lockfile` compatible lock: the lock is a directory whose existence
/// means "held". A holder that died leaves the directory behind; it is treated
/// as abandoned once its mtime is older than `stale`.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
}

impl DirLock {
    pub fn acquire(path: PathBuf, stale: Duration, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if is_stale(&path, stale) {
                        // Another process may reclaim it at the same moment; losing
                        // that race just means we loop and wait again.
                        let _ = fs::remove_dir(&path);
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(anyhow!(
                            "timed out waiting for lock {} (held by a running agent?)",
                            path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // The parent directory does not exist, so nobody can hold a
                    // lock inside it. Create it and retry.
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("creating {}", parent.display()))?;
                    }
                }
                Err(e) => return Err(e).with_context(|| format!("creating lock {}", path.display())),
            }
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn is_stale(path: &Path, stale: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .is_some_and(|age| age > stale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = FileLock::acquire(&path, Duration::from_secs(1)).unwrap();
        assert!(FileLock::try_acquire(&path).unwrap().is_none());
        drop(held);
        assert!(FileLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn dir_lock_times_out_while_held_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.lock");
        let held = DirLock::acquire(path.clone(), Duration::from_secs(60), Duration::ZERO).unwrap();
        assert!(DirLock::acquire(path.clone(), Duration::from_secs(60), Duration::ZERO).is_err());
        drop(held);
        assert!(!path.exists());
        DirLock::acquire(path, Duration::from_secs(60), Duration::ZERO).unwrap();
    }

    #[test]
    fn dir_lock_reclaims_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.lock");
        fs::create_dir(&path).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        DirLock::acquire(path, Duration::from_millis(1), Duration::ZERO).unwrap();
    }
}
