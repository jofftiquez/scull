//! The real OS implementation of [`Platform`]. Per-OS details live in `unix.rs` / `windows.rs`.

use crate::{Capabilities, DirIter, Metadata, Operation, Platform, PlatformError, Result};
use std::borrow::Cow;
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
