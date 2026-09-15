//! Crash-safe file writes and Windows-tolerant file access.
//!
//! Agent CLIs, antivirus scanners and search indexers routinely hold brief
//! handles on the files we touch. On Windows that surfaces as sharing
//! violations, so reads, renames and removals are retried with a short backoff.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How a written file's permissions should be chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Readable by the current user only (0600 on Unix). Used for agent-meter's own
    /// secrets.
    Private,
    /// Keep the permissions of the file being replaced. Used for files owned by
    /// an agent CLI so its expectations (and any sandbox ACLs) stay intact.
    InheritExisting,
}

/// Atomically replaces `path` with `contents`: write to a sibling temp file,
/// flush it to disk, then rename over the target.
pub fn write_atomic(path: &Path, contents: &[u8], mode: Mode) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = temp_sibling(path);
    let result = (|| {
        let mut file = create_new(&tmp, mode, path)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        retry(|| fs::rename(&tmp, path))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Reads a file, retrying transient sharing violations. Returns `Ok(None)` when
/// the file does not exist.
pub fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match retry(|| fs::read(path)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Removes a file, treating "already gone" as success.
pub fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match retry(|| fs::remove_file(path)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Removes a directory tree, treating "already gone" as success.
pub fn remove_dir_all_if_exists(path: &Path) -> io::Result<()> {
    match retry(|| fs::remove_dir_all(path)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Creates a directory (and parents) readable by the current user only.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Returns a path in the same directory as `path` that no other writer will
/// pick, so concurrent writers never share a temp file.
fn temp_sibling(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    let unique = format!(
        ".{name}.agent-meter-{}-{nanos:x}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    path.with_file_name(unique)
}

/// Creates the temp file that will replace `target`, with the permissions
/// `mode` asks for.
#[cfg(unix)]
fn create_new(tmp: &Path, mode: Mode, target: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let bits = match mode {
        Mode::Private => 0o600,
        Mode::InheritExisting => fs::metadata(target)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600),
    };
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(bits)
        .open(tmp)?;
    // The mode above is filtered through the umask, so set the exact bits.
    fs::set_permissions(tmp, fs::Permissions::from_mode(bits))?;
    Ok(file)
}

/// Creates the temp file that will replace `target`.
///
/// Windows has no mode bits: a new file inherits the ACL of its directory,
/// which is what both agent-meter's data directory (under the user's profile)
/// and the agent CLIs' own homes rely on.
#[cfg(not(unix))]
fn create_new(tmp: &Path, _mode: Mode, _target: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(tmp)
}

/// Retries an operation that failed with a transient Windows sharing or access
/// violation. On other platforms the operation runs once.
pub fn retry<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    const ATTEMPTS: u32 = 10;
    let mut delay = Duration::from_millis(2);
    let mut attempt = 1;
    loop {
        match op() {
            Err(e) if attempt < ATTEMPTS && is_transient(&e) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(250));
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn is_transient(e: &io::Error) -> bool {
    // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("file.json");
        write_atomic(&path, b"one", Mode::Private).unwrap();
        write_atomic(&path, b"two", Mode::InheritExisting).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn private_mode_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        write_atomic(&path, b"{}", Mode::Private).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn read_optional_reports_missing_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_optional(&dir.path().join("absent")).unwrap().is_none());
    }
}
