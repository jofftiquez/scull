//! The per-user execution lock (SAFE-09, P2-EXEC-04): an OS advisory file lock that at most
//! one scull executor (CLI, desktop or MCP) holds at a time.
//!
//! * **Unix:** `flock(LOCK_EX | LOCK_NB)` on the lock file (Linux, macOS), through
//!   [`std::fs::File::try_lock`].
//! * **Windows:** `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)`, through the
//!   same std call.
//!
//! The lock belongs to the open file, so the OS releases it when the holder closes it, exits
//! or is killed: a stale lock can't outlive its process and there is no PID file to clean up.
//! Scans never take it.
//!
//! The lock file lives in the user's state directory ([`exec_lock_path`]), is created
//! owner-only (`0600` / protected DACL), and is never followed if something replaced it with a
//! link: opening fails instead (`O_NOFOLLOW` / `FILE_FLAG_OPEN_REPARSE_POINT`).

use crate::app_dirs::AppDirs;
use crate::owner_only::{create_dir_owner_only, create_file, ensure_owner_only};
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Name of the lock file inside the state directory.
pub const EXEC_LOCK_FILE: &str = "execution.lock";

/// How often a waiting acquirer retries.
const POLL: Duration = Duration::from_millis(25);

/// Where the per-user execution lock lives: `<state>/execution.lock`.
pub fn exec_lock_path(dirs: &AppDirs) -> PathBuf {
    dirs.state.join(EXEC_LOCK_FILE)
}

/// A held exclusive lock. Dropping it (or the process ending, however it ends) releases it.
#[derive(Debug)]
pub struct ExclusiveLock {
    file: File,
    path: PathBuf,
}

impl ExclusiveLock {
    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        // Closing the file releases the lock anyway; unlocking first makes it immediate even
        // if a duplicated handle were still open somewhere.
        let _ = self.file.unlock();
    }
}

/// Tries once to take the exclusive lock at `path` (creating the file, and its directory,
/// owner-only if needed). `Ok(None)` when another holder has it.
///
/// # Errors
///
/// The lock file can't be created or opened (e.g. a link or a directory is in the way, or it
/// is accessible to other users), or the OS refused the lock for another reason.
pub fn try_lock_exclusive(path: &Path) -> io::Result<Option<ExclusiveLock>> {
    let file = open_lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(ExclusiveLock {
            file,
            path: path.to_path_buf(),
        })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Takes the exclusive lock at `path`, waiting up to `timeout` for the current holder to
/// release it. `Ok(None)` when it is still held after `timeout` (a zero timeout tries once).
///
/// # Errors
///
/// As [`try_lock_exclusive`].
pub fn lock_exclusive(path: &Path, timeout: Duration) -> io::Result<Option<ExclusiveLock>> {
    let deadline = Instant::now().checked_add(timeout);
    loop {
        if let Some(lock) = try_lock_exclusive(path)? {
            return Ok(Some(lock));
        }
        let now = Instant::now();
        match deadline {
            Some(d) if now < d => std::thread::sleep(POLL.min(d - now)),
            Some(_) => return Ok(None),
            // A timeout beyond `Instant`'s range waits indefinitely.
            None => std::thread::sleep(POLL),
        }
    }
}

/// Creates the lock file owner-only, or opens the existing one without following a link, and
/// checks it is a regular file only the current user can access.
fn open_lock_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_dir_owner_only(parent).map_err(io::Error::other)?;
    }
    let file = match create_file(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => open_existing_no_follow(path)?,
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    ensure_owner_only(path).map_err(io::Error::other)?;
    Ok(file)
}

#[cfg(unix)]
fn open_existing_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(windows)]
fn open_existing_no_follow(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if file.metadata()?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is a link; it is never followed", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_existing_no_follow(_path: &Path) -> io::Result<File> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join(EXEC_LOCK_FILE);
        let first = try_lock_exclusive(&path).unwrap().expect("first lock");
        assert!(try_lock_exclusive(&path).unwrap().is_none());
        assert!(
            lock_exclusive(&path, Duration::from_millis(60))
                .unwrap()
                .is_none()
        );
        ensure_owner_only(&path).unwrap();
        drop(first);
        assert!(try_lock_exclusive(&path).unwrap().is_some());
    }

    #[test]
    fn waiting_acquirer_gets_the_lock_once_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(EXEC_LOCK_FILE);
        let first = try_lock_exclusive(&path).unwrap().unwrap();
        let p = path.clone();
        let waiter = std::thread::spawn(move || lock_exclusive(&p, Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(100));
        drop(first);
        assert!(waiter.join().unwrap().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_at_the_lock_path_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"not a lock").unwrap();
        let path = dir.path().join(EXEC_LOCK_FILE);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(try_lock_exclusive(&path).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"not a lock");
    }

    #[test]
    fn a_directory_at_the_lock_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(EXEC_LOCK_FILE);
        std::fs::create_dir(&path).unwrap();
        assert!(try_lock_exclusive(&path).is_err());
    }
}
