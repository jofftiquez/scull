//! Plain data returned by [`crate::Platform`].

use scull_core::FileIdentity;
use serde::Serialize;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

/// What a filesystem entry is, determined without following links or opening it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Real directory (not a link to one).
    Dir,
    /// Symbolic link. On Windows: a reparse point tagged `IO_REPARSE_TAG_SYMLINK` (file or
    /// directory symlink). Never followed.
    Symlink,
    /// Windows junction or volume mount point (reparse tag `IO_REPARSE_TAG_MOUNT_POINT`). Like a
    /// directory symlink, but creatable without privilege, so common in cloned repositories
    /// and tool caches. Never followed (SAFE-04, T-10).
    Junction,
    /// Any other Windows reparse point: cloud-sync placeholders (OneDrive, `IO_REPARSE_TAG_CLOUD*`),
    /// app execution aliases, deduplicated files, and tags scull doesn't know. The raw tag is in
    /// [`Metadata::reparse_tag`]. Never followed, opened for data or hydrated (PLT-04).
    ReparsePoint,
    /// Named pipe. Never opened (SAFE-18).
    Fifo,
    /// Unix domain socket. Never opened (SAFE-18).
    Socket,
    /// Block device node. Never opened (SAFE-18).
    BlockDevice,
    /// Character device node. Never opened (SAFE-18).
    CharDevice,
    /// Anything else the OS reports (e.g. Solaris doors). Treated like a special file.
    Other,
}

impl EntryKind {
    /// Symlinks, junctions and other reparse points: entries that point somewhere else (or
    /// whose content lives elsewhere) and are never followed.
    pub fn is_link(self) -> bool {
        matches!(
            self,
            EntryKind::Symlink | EntryKind::Junction | EntryKind::ReparsePoint
        )
    }

    /// FIFOs, sockets, devices and unknown kinds: entries that must never be opened (SAFE-18).
    pub fn is_special(self) -> bool {
        matches!(
            self,
            EntryKind::Fifo
                | EntryKind::Socket
                | EntryKind::BlockDevice
                | EntryKind::CharDevice
                | EntryKind::Other
        )
    }

    /// Stable lowercase name, as used in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
            EntryKind::Symlink => "symlink",
            EntryKind::Junction => "junction",
            EntryKind::ReparsePoint => "reparse_point",
            EntryKind::Fifo => "fifo",
            EntryKind::Socket => "socket",
            EntryKind::BlockDevice => "block_device",
            EntryKind::CharDevice => "char_device",
            EntryKind::Other => "other",
        }
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// No-follow metadata for one path (`lstat` semantics).
///
/// Optional fields are `None` when this OS build can't provide the signal; see
/// [`Capabilities`]. Callers must treat `None` as "unknown", never as zero or "unique".
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Metadata {
    /// What the entry is.
    pub kind: EntryKind,
    /// Path-independent identity (Unix: `st_dev` + `st_ino`).
    pub identity: Option<FileIdentity>,
    /// Logical size in bytes (`st_size` / end-of-file). For a symlink this is the length of
    /// the link itself, not of its target.
    pub logical_size: u64,
    /// Bytes allocated on disk (Unix: `st_blocks * 512`). Smaller than `logical_size` for
    /// sparse or compressed files.
    pub allocated_size: Option<u64>,
    /// Number of hard links (`st_nlink`). Greater than 1 means removing this path may not free
    /// its blocks (PHY-03).
    pub nlink: Option<u64>,
    /// Last modification time in whole seconds since the Unix epoch (negative before 1970).
    pub mtime_unix: Option<i64>,
    /// Whether the entry is read-only (Unix: no write permission bit set for anyone; Windows:
    /// `FILE_ATTRIBUTE_READONLY`).
    pub readonly: bool,
    /// Windows only: the raw `FILE_ATTRIBUTE_*` bits (e.g. `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`
    /// or `FILE_ATTRIBUTE_OFFLINE` on cloud placeholders). `None` on Unix.
    pub file_attributes: Option<u32>,
    /// Windows only: the raw reparse tag (`IO_REPARSE_TAG_*`) when the entry is a reparse point,
    /// e.g. to classify cloud placeholders (PLT-04). `None` for ordinary entries and on Unix.
    pub reparse_tag: Option<u32>,
    /// macOS only: the raw BSD file flags (`st_flags`, see `chflags(2)`), e.g. `SF_DATALESS`
    /// on iCloud / File Provider files whose contents were evicted (PLT-04). `None` on Linux
    /// and Windows.
    pub bsd_flags: Option<u32>,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DirEntry {
    /// Full path: the listed directory joined with [`DirEntry::file_name`].
    pub path: PathBuf,
    /// The entry's name within its directory.
    pub file_name: OsString,
    /// The entry's no-follow kind. Obtained without opening the entry.
    pub kind: EntryKind,
    /// Windows only: the raw reparse tag when the entry is a reparse point (see
    /// [`Metadata::reparse_tag`]). `None` for ordinary entries and on Unix.
    pub reparse_tag: Option<u32>,
}

/// Which signals this OS build provides, for `scull doctor` (PLT-09) and for downgrading
/// confidence or blocking actions when a safety signal is missing (PLT-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub struct Capabilities {
    /// `std::env::consts::OS`, e.g. `linux`, `macos`, `windows`.
    pub os: &'static str,
    /// [`Metadata::identity`] is available (SAFE-05, SAFE-13, PHY-03).
    pub file_identity: bool,
    /// [`Metadata::allocated_size`] is available (PRD §9).
    pub allocated_size: bool,
    /// [`Metadata::nlink`] is available (PHY-03).
    pub link_count: bool,
    /// [`Metadata::mtime_unix`] is available.
    pub mtime: bool,
    /// Metadata and listings never follow symlinks (SAFE-04). Always true; listed so
    /// `scull doctor` shows it.
    pub no_follow_metadata: bool,
    /// FIFOs, sockets and devices are told apart without opening them (SAFE-18).
    pub special_file_kinds: bool,
    /// Windows reparse points are detected and told apart by tag: symlinks, junctions and
    /// mount points, and other reparse points with their raw tag (placeholders, ...).
    pub reparse_points: bool,
    /// `read_dir` opens the directory with a no-follow call and checks the opened handle, so a
    /// directory swapped for a link between the check and the open can't redirect the listing.
    /// True on Windows (no-follow handle, kind re-checked on the handle, entries enumerated
    /// from that handle). False on Unix for now: the check uses `lstat` and the open uses
    /// `std::fs::read_dir`. Listing is read-only, so this race can at worst list a directory
    /// outside the scan root; candidates are revalidated by identity before deletion (SAFE-05)
    /// and deletion uses its own handle-relative operations (SAFE-04).
    pub race_free_read_dir: bool,
    /// [`crate::is_elevated`] can tell root/administrator from an ordinary user (SAFE-14).
    /// True on every supported OS; when it is false, or the check returns
    /// [`crate::Elevation::Unknown`], destructive operations are refused.
    pub elevation_detection: bool,
}
