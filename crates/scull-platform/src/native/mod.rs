//! The real OS implementation of [`Platform`]. Per-OS details live in `unix.rs` / `windows.rs`.

use crate::{
    Capabilities, DirIter, DirSource, Exposure, Metadata, Operation, Platform, PlatformError,
    Result,
};
use std::borrow::Cow;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as os;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

#[cfg(not(any(unix, windows)))]
compile_error!("scull-platform supports Unix (Linux, macOS) and Windows only");

/// The real operating system.
///
/// Stateless and cheap to construct; share one or create as needed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Native;

impl Native {
    /// The native platform.
    pub const fn new() -> Self {
        Native
    }
}

impl Platform for Native {
    fn capabilities(&self) -> Capabilities {
        os::capabilities()
    }

    fn metadata(&self, path: &Path) -> Result<Metadata> {
        // Unix: `lstat`. Windows: a handle opened with FILE_FLAG_OPEN_REPARSE_POINT and only
        // FILE_READ_ATTRIBUTES access. Either way the final component is never followed and
        // no data is read.
        os::metadata(path).map_err(|e| PlatformError::from_io(Operation::Metadata, path, e))
    }

    fn read_dir<'a>(&'a self, path: &Path) -> Result<DirIter<'a>> {
        os::read_dir(path)
    }
}

/// The platform-native encoding of a path: raw bytes on Unix, UTF-16LE on Windows.
///
/// This is the encoding [`scull_core::ItemId::derive`] expects, so two different paths never
/// hash alike even when their lossy display strings match.
pub fn native_path_bytes(path: &Path) -> Cow<'_, [u8]> {
    os::path_bytes(path)
}

// Crate-internal entry points for the OS-independent modules (`app_dirs`, `owner_only`).

/// No-follow metadata (see [`Platform::metadata`]), as a raw I/O result.
pub(crate) fn os_metadata(path: &Path) -> io::Result<Metadata> {
    os::metadata(path)
}

/// Creates one directory (parent must exist) with owner-only permissions.
pub(crate) fn create_dir_owner_only(path: &Path) -> io::Result<()> {
    os::create_dir_owner_only(path)
}

/// Creates a new file (`create_new`) with owner-only permissions, opened read-write.
pub(crate) fn create_file_owner_only(path: &Path) -> io::Result<File> {
    os::create_file_owner_only(path)
}

/// Narrows an existing file or directory to owner-only, without following a link.
pub(crate) fn restrict_owner_only(path: &Path) -> io::Result<()> {
    os::restrict_owner_only(path)
}

/// How `path` (already known not to be a link) is broader than owner-only.
pub(crate) fn owner_only_exposures(path: &Path) -> io::Result<Vec<Exposure>> {
    os::owner_only_exposures(path)
}

/// Whether this process is elevated (root / elevated administrator token).
pub(crate) fn elevation() -> crate::Elevation {
    os::elevation()
}

/// A Windows known folder (`RoamingAppData`, `LocalAppData`); `None` on other OSs, for other
/// sources, or when the shell can't provide it.
pub(crate) fn known_folder(source: DirSource) -> Option<PathBuf> {
    os::known_folder(source)
}
