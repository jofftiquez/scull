//! Owner-only file permissions (DATA-04), injected by the caller.
//!
//! OS permission APIs live only in `scull-platform` (PLT-11), so this crate cannot set Unix
//! modes or Windows ACLs itself. Callers pass an [`OwnerOnly`] implementation, normally the one
//! from `scull-platform`:
//!
//! * **Unix:** files are created with mode `0600`, directories with `0700`;
//!   [`OwnerOnly::restrict`] clears group/other bits on an existing file or directory.
//! * **Windows:** a protected DACL granting access only to the current user's SID (the data
//!   directory must be under non-roaming `%LOCALAPPDATA%`, FX-WIN-12).
//!
//! How the store uses it: the database file is created empty through
//! [`OwnerOnly::create_file`] *before* SQLite opens it. On Unix SQLite creates the `-wal` and
//! `-shm` files with the same mode as the database file, so they are owner-only from their first
//! byte. Existing database, `-wal` and `-shm` files, backups and their directory are tightened
//! with [`OwnerOnly::restrict`] on every open, in case they were created by an older build or
//! copied in.

use std::io;
use std::path::Path;

/// Creates and restricts files and directories so only the current user can access them.
///
/// Implementations must not follow a symlink at `path` when creating (fail instead) and must
/// never widen existing permissions.
pub trait OwnerOnly {
    /// Create directory `path` (its parent exists) accessible only to the current user.
    /// Returns an error if it already exists.
    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Create a new, empty file at `path` accessible only to the current user. Must fail if
    /// anything already exists at `path` (exclusive create), so an attacker-planted file or
    /// symlink is never adopted.
    fn create_file(&self, path: &Path) -> io::Result<()>;

    /// Restrict an existing file or directory to the current user (remove access for group,
    /// others, or other SIDs). Must not follow a symlink at `path`.
    fn restrict(&self, path: &Path) -> io::Result<()>;
}

/// Portable fallback that creates files and directories with the OS defaults and leaves
/// permissions as they are.
///
/// **Not owner-only.** Only for tests and for platforms where `scull-platform` has no
/// implementation yet; production callers must pass the platform implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultPermissions;

impl OwnerOnly for DefaultPermissions {
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }

    fn create_file(&self, path: &Path) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop)
    }

    fn restrict(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}
