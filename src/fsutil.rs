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

/// The longest path Windows accepts without the verbatim prefix, counting the
/// terminator it does not show you.
#[cfg(windows)]
const MAX_PATH: usize = 260;

/// A path with its symlinks resolved, written the way the rest of the system
/// writes it.
///
/// `canonicalize` is how a directory is checked for being the same directory
/// as another under a different name, which is worth doing before one of them
/// is trusted. On Windows it answers with a verbatim path — `\\?\C:\Users\…`
/// — naming the same place in a form nobody types, several Windows programs
/// refuse to open, and no path a user gave us will ever compare equal to.
pub fn canonical(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path).map(plain)
}

/// Drops a verbatim prefix where dropping it cannot change which file is
/// meant. Everywhere but Windows there is nothing to drop.
#[cfg(not(windows))]
pub fn plain(path: PathBuf) -> PathBuf {
    path
}

#[cfg(windows)]
pub fn plain(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path;
    };
    let head = match prefix.kind() {
        Prefix::VerbatimDisk(letter) => format!("{}:\\", letter as char),
        // `\\?\UNC\server\share` is `\\server\share` written the long way.
        Prefix::VerbatimUNC(server, share) => {
            format!(r"\\{}\{}", server.to_string_lossy(), share.to_string_lossy())
        }
        // Already plain, or a device path that means nothing without it.
        _ => return path,
    };

    let rest = components.as_path();
    // The verbatim form is the only way to name some of these, so dropping it
    // would name a different file or none at all.
    let needs_the_prefix = rest.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name.ends_with('.') || name.ends_with(' ') || is_device_name(&name)
    });
    let dropped = Path::new(&head).join(rest);
    if needs_the_prefix || dropped.as_os_str().len() >= MAX_PATH {
        return path;
    }
    dropped
}

/// Names that mean a device rather than a file, whatever directory they are
/// written in and whatever is put after a dot.
///
/// The numbered ones start at one: `COM0` is an ordinary name, and a file
/// called that would be lost by treating it as a device.
#[cfg(windows)]
fn is_device_name(name: &str) -> bool {
    const DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "COM", "LPT"];
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    DEVICES.iter().any(|device| {
        stem.eq_ignore_ascii_case(device)
            || (matches!(*device, "COM" | "LPT")
                && stem.len() == device.len() + 1
                && stem[..device.len()].eq_ignore_ascii_case(device)
                && matches!(stem.as_bytes()[device.len()], b'1'..=b'9'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `canonicalize` is used to tell whether two names are the same
    /// directory. On Windows its answer is a form nobody else writes, and a
    /// path a user typed will never compare equal to it.
    #[test]
    fn a_canonical_path_is_written_the_way_everything_else_writes_it() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = canonical(dir.path()).unwrap();
        assert!(
            !canonical.to_string_lossy().starts_with(r"\\?\"),
            "{canonical:?} is not a path anything else will match"
        );
        // Still the same directory, and still absolute.
        assert!(canonical.is_absolute());
        assert!(canonical.is_dir());
        // And asking twice is stable, which is what the comparisons rely on.
        assert_eq!(canonical, self::canonical(&canonical).unwrap());
    }

    /// Some names can only be written with the prefix, so dropping it would
    /// name a different file, or none.
    #[cfg(windows)]
    #[test]
    fn a_prefix_that_is_load_bearing_is_kept() {
        let kept = |path: &str| {
            let out = plain(PathBuf::from(path));
            assert_eq!(out, PathBuf::from(path), "{path} lost a prefix it needed");
        };
        let dropped = |path: &str, want: &str| {
            assert_eq!(plain(PathBuf::from(path)), PathBuf::from(want));
        };

        dropped(r"\\?\C:\Users\dev\project", r"C:\Users\dev\project");
        dropped(r"\\?\UNC\server\share\file", r"\\server\share\file");
        // Already plain, and left alone.
        dropped(r"C:\Users\dev", r"C:\Users\dev");

        // A trailing dot or space is only reachable through the prefix.
        kept(r"\\?\C:\Users\dev\odd.");
        kept(r"\\?\C:\Users\dev\odd ");
        // So is a file named after a device.
        kept(r"\\?\C:\Users\dev\NUL");
        kept(r"\\?\C:\Users\dev\com1.txt");
        // And anything too long to be named without it.
        kept(&format!(r"\\?\C:\{}", "n".repeat(300)));
        // A name that merely starts like a device is an ordinary name.
        dropped(r"\\?\C:\Users\dev\communication", r"C:\Users\dev\communication");
        dropped(r"\\?\C:\Users\dev\com0", r"C:\Users\dev\com0");
    }

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
