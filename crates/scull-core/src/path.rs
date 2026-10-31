//! Path representation for serialized output.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// A filesystem path as it appears in plans, JSON output and the database.
///
/// Serialized as a string. Paths that aren't valid Unicode (possible on Unix and Windows) are
/// converted lossily with U+FFFD and flagged by [`FsPath::is_lossy`]; the engine never acts on
/// a path read back from JSON alone. Items are matched by [`crate::ItemId`] and file identity,
/// and candidates are re-derived from the disk before execution (SAFE-05, SAFE-11).
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct FsPath(String);

impl FsPath {
    /// From a native path (lossy if not valid Unicode).
    pub fn from_path(p: &Path) -> Self {
        Self(p.to_string_lossy().into_owned())
    }

    /// The string form.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the string contains U+FFFD, i.e. the original may not round-trip.
    pub fn is_lossy(&self) -> bool {
        self.0.contains('\u{FFFD}')
    }

    /// As a `PathBuf` for display or lookup. Never use this to delete (see type docs).
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

impl From<&Path> for FsPath {
    fn from(p: &Path) -> Self {
        Self::from_path(p)
    }
}

impl From<&str> for FsPath {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl fmt::Display for FsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_path_round_trips() {
        let p = FsPath::from(Path::new("/home/me/proj/target"));
        assert_eq!(p.as_str(), "/home/me/proj/target");
        assert!(!p.is_lossy());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_is_flagged_lossy() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let p = FsPath::from_path(Path::new(OsStr::from_bytes(b"/tmp/\xff\xfe")));
        assert!(p.is_lossy());
    }
}
