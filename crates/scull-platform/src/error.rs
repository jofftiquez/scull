//! Error type for platform calls.

use crate::{EntryKind, Exposure};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The platform call that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Operation {
    /// [`crate::Platform::metadata`].
    Metadata,
    /// Opening a directory in [`crate::Platform::read_dir`].
    ReadDir,
    /// Reading one entry during a directory listing.
    ReadDirEntry,
    /// [`crate::create_dir_owner_only`].
    CreateDir,
    /// [`crate::create_file_owner_only`].
    CreateFile,
    /// [`crate::ensure_owner_only`].
    CheckPermissions,
    /// [`crate::volumes()`].
    ListVolumes,
    /// [`crate::volume_of`].
    VolumeOf,
    /// [`crate::free_space`].
    FreeSpace,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Operation::Metadata => "read metadata of",
            Operation::ReadDir => "list directory",
            Operation::ReadDirEntry => "read directory entry in",
            Operation::CreateDir => "create directory",
            Operation::CreateFile => "create file",
            Operation::CheckPermissions => "check permissions of",
            Operation::ListVolumes => "list mounted volumes",
            Operation::VolumeOf => "find the volume of",
            Operation::FreeSpace => "query free space of",
        })
    }
}

/// What went wrong.
#[derive(Debug)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The path does not exist.
    NotFound(io::Error),
    /// The OS refused access (permissions, macOS TCC / Full Disk Access, ...).
    PermissionDenied(io::Error),
    /// [`crate::Platform::read_dir`] was given something that is not a real directory, e.g. a
    /// symlink or junction, which is never followed; or
    /// [`crate::create_dir_owner_only`] found something other than a directory in the way.
    NotADirectory {
        /// What was found at the path instead.
        found: EntryKind,
    },
    /// [`crate::ensure_owner_only`]: the entry is accessible by more than the current user.
    NotOwnerOnly {
        /// Every way the permissions are broader than owner-only (never empty).
        exposures: Vec<Exposure>,
    },
    /// Any other OS error.
    Io(io::Error),
}

/// Error from a [`crate::Platform`] call: the operation, the path and the cause.
#[derive(Debug)]
pub struct PlatformError {
    op: Operation,
    path: PathBuf,
    kind: ErrorKind,
}

impl PlatformError {
    /// Build an error from an I/O error, classifying common kinds.
    pub fn from_io(op: Operation, path: &Path, err: io::Error) -> Self {
        let kind = match err.kind() {
            io::ErrorKind::NotFound => ErrorKind::NotFound(err),
            io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied(err),
            _ => ErrorKind::Io(err),
        };
        Self {
            op,
            path: path.to_path_buf(),
            kind,
        }
    }

    /// `read_dir` refused a path that is not a real directory.
    pub fn not_a_directory(path: &Path, found: EntryKind) -> Self {
        Self {
            op: Operation::ReadDir,
            path: path.to_path_buf(),
            kind: ErrorKind::NotADirectory { found },
        }
    }

    /// An error with an explicit kind.
    pub(crate) fn with_kind(op: Operation, path: &Path, kind: ErrorKind) -> Self {
        Self {
            op,
            path: path.to_path_buf(),
            kind,
        }
    }

    /// The operation that failed.
    pub fn operation(&self) -> Operation {
        self.op
    }

    /// The path involved.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What went wrong.
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    /// Whether the path did not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self.kind, ErrorKind::NotFound(_))
    }

    /// Whether access was denied.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self.kind, ErrorKind::PermissionDenied(_))
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.as_os_str().is_empty() {
            write!(f, "cannot {}: ", self.op)?;
        } else {
            write!(f, "cannot {} {}: ", self.op, self.path.display())?;
        }
        match &self.kind {
            ErrorKind::NotFound(e) | ErrorKind::PermissionDenied(e) | ErrorKind::Io(e) => {
                write!(f, "{e}")
            }
            ErrorKind::NotADirectory { found } => {
                write!(
                    f,
                    "not a directory (found {found}; links are never followed)"
                )
            }
            ErrorKind::NotOwnerOnly { exposures } => {
                f.write_str("not owner-only: ")?;
                for (i, e) in exposures.iter().enumerate() {
                    if i > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{e}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PlatformError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ErrorKind::NotFound(e) | ErrorKind::PermissionDenied(e) | ErrorKind::Io(e) => Some(e),
            ErrorKind::NotADirectory { .. } | ErrorKind::NotOwnerOnly { .. } => None,
        }
    }
}
