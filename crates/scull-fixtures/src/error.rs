//! Error type for fixture building and snapshotting.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T, E = FixtureError> = std::result::Result<T, E>;

/// Why a fixture operation failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum FixtureError {
    /// A filesystem call failed.
    Io {
        /// What the harness was doing, e.g. "create file".
        op: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A builder was given a path that is not a plain relative path inside the fixture root
    /// (absolute, `..`, a drive prefix, empty), or that would be written through a symlink.
    /// The harness refuses these so it can never write outside its temporary directory.
    UnsafePath {
        /// The offending path as given.
        path: PathBuf,
        /// Why it was refused.
        reason: &'static str,
    },
    /// An external tool (e.g. `git`) could not be run or exited unsuccessfully.
    Tool {
        /// The command line, for diagnostics.
        command: String,
        /// Exit status or spawn error.
        status: String,
        /// Captured standard error.
        stderr: String,
    },
}

impl FixtureError {
    pub(crate) fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            op,
            path: path.into(),
            source,
        }
    }

    pub(crate) fn unsafe_path(path: impl Into<PathBuf>, reason: &'static str) -> Self {
        Self::UnsafePath {
            path: path.into(),
            reason,
        }
    }
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, path, source } => {
                write!(f, "fixture: {op} {}: {source}", path.display())
            }
            Self::UnsafePath { path, reason } => {
                write!(f, "fixture: refusing path {:?}: {reason}", path)
            }
            Self::Tool {
                command,
                status,
                stderr,
            } => write!(f, "fixture: `{command}` failed ({status}): {stderr}"),
        }
    }
}

impl std::error::Error for FixtureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
