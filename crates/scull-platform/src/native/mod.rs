//! The real OS implementation of [`Platform`]. Per-OS details live in `unix.rs` / `windows.rs`.

use crate::{
    Capabilities, DirEntry, DirIter, EntryKind, Metadata, Operation, Platform, PlatformError,
    Result,
};
use std::borrow::Cow;
use std::fs;
use std::path::Path;

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
        // `symlink_metadata` is `lstat` on Unix and opens with FILE_FLAG_OPEN_REPARSE_POINT on
        // Windows: the final component is never followed and special files are never opened.
        let md = fs::symlink_metadata(path)
            .map_err(|e| PlatformError::from_io(Operation::Metadata, path, e))?;
        Ok(os::convert(&md))
    }

    fn read_dir<'a>(&'a self, path: &Path) -> Result<DirIter<'a>> {
        // `std::fs::read_dir` follows a symlink at `path`, so check first and refuse links.
        let kind = self.metadata(path)?.kind;
        if kind != EntryKind::Dir {
            return Err(PlatformError::not_a_directory(path, kind));
        }
        let dir = path.to_path_buf();
        let rd =
            fs::read_dir(path).map_err(|e| PlatformError::from_io(Operation::ReadDir, path, e))?;
        Ok(Box::new(rd.map(move |entry| {
            let entry =
                entry.map_err(|e| PlatformError::from_io(Operation::ReadDirEntry, &dir, e))?;
            let entry_path = entry.path();
            let kind = os::entry_kind(&entry)
                .map_err(|e| PlatformError::from_io(Operation::ReadDirEntry, &entry_path, e))?;
            Ok(DirEntry {
                file_name: entry.file_name(),
                path: entry_path,
                kind,
            })
        })))
    }
}

/// The platform-native encoding of a path: raw bytes on Unix, UTF-16LE on Windows.
///
/// This is the encoding [`scull_core::ItemId::derive`] expects, so two different paths never
/// hash alike even when their lossy display strings match.
pub fn native_path_bytes(path: &Path) -> Cow<'_, [u8]> {
    os::path_bytes(path)
}
