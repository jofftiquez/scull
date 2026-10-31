//! The removal walker: fd-relative, no-follow deletion of one verified tree (P2-EXEC-01,
//! SAFE-04, SAFE-05, SAFE-18, T-10, T-11).
//!
//! **Only `scull-cleaner`'s Executor may call this** (SAFE-10). The module exists only with the
//! `delete` cargo feature, which no other crate enables (checked by
//! `crates/scull-cleaner/tests/delete_feature.rs`). It lives here because it is OS code
//! (PLT-11); the policy (which items, which tier, lock, journal, audit) lives in the Executor.
//!
//! # What it does (Unix)
//!
//! 1. Opens the trusted **anchor** directory (e.g. the scan root), then walks to the target's
//!    parent one component at a time with `openat(O_DIRECTORY | O_NOFOLLOW)`, so no component
//!    below the anchor can be a symlink: a link in the path is refused, never followed.
//! 2. `fstatat(AT_SYMLINK_NOFOLLOW)` on the target and compares its device + inode with the
//!    expected [`FileIdentity`] (SAFE-05). A mismatch refuses the whole request; nothing is
//!    removed. A target that is a mount point (another device than its parent directory or, on
//!    Linux, another `statx` mount ID: bind mounts) is refused too. A single non-directory
//!    target with other hard links is refused (removing one name frees nothing), and is
//!    re-checked by identity immediately before its `unlinkat`. A directory target is opened
//!    no-follow and its *opened handle* is checked against the same identity.
//! 3. Removes the tree bottom-up, iteratively (no recursion, so depth cannot overflow the
//!    stack), always relative to directory handles it holds:
//!    * every directory is listed in full first, recording each name's `d_type`;
//!    * every entry is `fstatat`'d no-follow before it is touched; its kind must match the
//!      listing and its device must equal the target's (a different device is a mount point:
//!      **never crossed**);
//!    * sub-directories are opened with `openat(O_DIRECTORY | O_NOFOLLOW | O_NONBLOCK)`; the
//!      opened handle must have the device + inode the `fstatat` saw and, on Linux, the
//!      target's mount ID (a bind mount of the same filesystem is never crossed either; where
//!      the kernel has no mount IDs only `st_dev` is compared, see
//!      [`RemoveReport::mount_ids_checked`]);
//!    * before the first removal in a directory, after returning from a sub-directory, and at
//!      least every [`VERIFY_EVERY`] entries, the walker checks the directory is still in
//!      place: `..` re-opened from its handle is the recorded parent and the name in that
//!      parent still refers to it; otherwise it aborts ([`Anomaly::DirectoryMoved`]);
//!    * files, symlinks, FIFOs, sockets and device nodes are removed with `unlinkat(.., 0)`:
//!      unlinking a symlink removes the link, never its target, and special files are never
//!      opened (SAFE-18);
//!    * emptied directories are removed with `unlinkat(.., AT_REMOVEDIR)` after re-checking
//!      that the name still refers to the directory that was emptied.
//!
//! Nothing is ever `chmod`ed, `chown`ed or followed, and no path string is ever re-resolved
//! after the anchor: every operation is relative to a held, verified handle.
//!
//! # Errors versus reports
//!
//! * `Err(`[`RemoveError`]`)`: the request was **refused before anything was removed**
//!   (invalid request, unusable anchor, link in the path, target missing, identity mismatch).
//! * `Ok(`[`RemoveReport`]`)`: the walk ran. [`RemoveOutcome::Complete`] means the target is
//!   gone. [`RemoveOutcome::Incomplete`] means some entries could not be removed (each listed in
//!   [`RemoveReport::skipped`] with a reason, e.g. permission denied in a read-only directory),
//!   and their ancestors were kept. [`RemoveOutcome::Aborted`] means the walk saw an
//!   [`Anomaly`] (an entry replaced or turned into a link mid-walk, a mount point, a directory
//!   moved) and **stopped at once**: nothing further was touched, never "best effort" (SAFE-05).
//!
//! # Threat model notes (T-10, T-11)
//!
//! Every removal happens inside a directory whose handle was opened no-follow from the anchor
//! and verified, so a link swapped in anywhere (before or during the walk) can at worst be
//! unlinked itself; its target is never reached.
//!
//! ## Residual window
//!
//! POSIX has no "unlink by identity": `unlinkat` removes whatever entry a *name* refers to in a
//! directory handle at that instant. Two windows therefore remain, and are made as small as
//! the API allows rather than claimed closed:
//!
//! * **A held directory moved elsewhere.** If someone renames a directory the walker is
//!   emptying (or one of its ancestors) out of the candidate, the walker's handle follows it.
//!   Between two place checks, i.e. for at most [`VERIFY_EVERY`] entries, removals land at the
//!   new location, on entries whose names were listed while the directory was in the
//!   candidate, including entries someone created there under those names in the meantime. The
//!   next check (or the climb back) aborts the walk. A check only covers the directory and its
//!   parent; an ancestor moved higher up is caught when the walk climbs back to it.
//! * **A name re-pointed between check and unlink.** Between the no-follow `fstatat` of an
//!   entry (or the final re-check of a single-file target) and its `unlinkat`, the name can be
//!   pointed at another object in the same directory (a `rename` over it). That object is then
//!   unlinked by name: never followed, and only an entry that someone placed inside the
//!   candidate directory itself.
//!
//! Both need write access to the candidate (or its parent) during the run; neither can make
//! the walker follow a link or cross a mount.
//!
//! # Windows
//!
//! Not implemented yet: [`remove_tree`] returns [`RemoveError::Unsupported`]. The handle-based
//! Windows walker (`FILE_FLAG_OPEN_REPARSE_POINT`, long paths, locked files) is P2-EXEC-02, #68.

use crate::EntryKind;
use scull_core::FileIdentity;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;

/// Directory handles the walker keeps open at once. Deeper levels have their handle closed and
/// re-opened through `..` (identity-checked) when the walk climbs back, so very deep trees never
/// exhaust the descriptor limit (macOS defaults to 256 per process).
pub const FD_BUDGET: usize = 32;

/// Deepest level the walker descends to below the target. Deeper directories are reported as
/// [`SkipReason::TooDeep`] (and their ancestors kept), never silently ignored.
pub const MAX_DEPTH: usize = 1 << 16;

/// While emptying a directory, the walker re-verifies that it is still at its name in its
/// parent (and that `..` is still that parent) before its first removal, after returning from
/// a sub-directory, and at least once every this many entries. See "Residual window" in the
/// module docs.
pub const VERIFY_EVERY: usize = 64;

/// Skipped entries listed individually in a [`RemoveReport`]; the rest are only counted in
/// [`RemoveReport::skipped_total`].
pub const MAX_SKIPPED_LISTED: usize = 1000;

/// One removal: the target, how to reach it, and the identity it must have.
#[derive(Debug, Clone, Copy)]
pub struct RemoveRequest<'a> {
    /// Trusted directory the walk starts from, typically the scan root. Opened as given (a
    /// link *at* the anchor is resolved like any user-chosen root), and checked against
    /// [`RemoveRequest::anchor_identity`] when that is set.
    pub anchor: &'a Path,
    /// Identity the opened anchor must have, if known.
    pub anchor_identity: Option<FileIdentity>,
    /// The target, relative to the anchor: one or more plain names (no root, prefix, `.` or
    /// `..`). Every component but the last must be a real directory, never a link.
    pub relative: &'a Path,
    /// Device + inode (Windows: volume + file ID) the target must have (SAFE-05).
    pub expected: FileIdentity,
    /// The kind the target must be, if known. [`RemoveRequest::expected`] already pins the
    /// object, so this is an extra check, not a requirement.
    pub expected_kind: Option<EntryKind>,
}

/// A point in the walk, reported to an observer (see [`remove_tree_observed`]).
///
/// For fault-injection tests (FX-RACE-02): an observer can rename, swap or relink entries at
/// exactly these points. The walker's safety never depends on what an observer does; it
/// re-verifies everything after each call.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum WalkEvent<'a> {
    /// A directory was opened no-follow and its handle verified; it has not been listed yet.
    DirOpened {
        /// Path relative to the target (empty for the target itself).
        relative: &'a Path,
        /// 0 for the target, 1 for its children, ...
        depth: usize,
    },
    /// A directory was listed (its names are now pending); nothing in it was removed yet.
    DirListed {
        /// Path relative to the target (empty for the target itself).
        relative: &'a Path,
        /// 0 for the target, 1 for its children, ...
        depth: usize,
    },
    /// An entry was examined with a no-follow `stat` and is about to be removed (or, for a
    /// directory, opened).
    EntryChecked {
        /// Path relative to the target (empty for the target itself).
        relative: &'a Path,
        /// What the `stat` saw.
        kind: EntryKind,
    },
}

/// How a walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RemoveOutcome {
    /// The target and everything below it were removed.
    Complete,
    /// The walk finished but some entries were kept (see [`RemoveReport::skipped`]); the target
    /// and the ancestors of kept entries still exist.
    Incomplete,
    /// The walk stopped at an [`Anomaly`]; nothing was touched after it.
    Aborted,
}

/// What was removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RemovedCounts {
    /// Regular files unlinked.
    pub files: u64,
    /// Directories removed (including the target, when it was one).
    pub dirs: u64,
    /// Symbolic links unlinked (the links themselves; targets untouched).
    pub symlinks: u64,
    /// FIFOs, sockets and device nodes unlinked (never opened).
    pub special: u64,
    /// Sum of `st_size` of the unlinked non-directories. A file with other hard links keeps its
    /// data, so this is an upper bound for freed bytes.
    pub logical_bytes: u64,
    /// Sum of allocated bytes (`st_blocks * 512`) of the unlinked non-directories.
    pub allocated_bytes: u64,
}

/// Why one entry was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SkipReason {
    /// The OS refused (e.g. a read-only directory, `EACCES`/`EPERM`). Never escalated: the
    /// walker does not change permissions.
    PermissionDenied,
    /// The filesystem is mounted read-only.
    ReadOnlyFilesystem,
    /// The entry is in use (`EBUSY`).
    Busy,
    /// The directory was not empty when it was removed (entries appeared during the walk, or an
    /// entry below it was kept).
    NotEmpty,
    /// Deeper than [`MAX_DEPTH`].
    TooDeep,
    /// Any other OS error.
    Io,
}

impl SkipReason {
    /// Stable lowercase code.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::PermissionDenied => "permission_denied",
            SkipReason::ReadOnlyFilesystem => "read_only_filesystem",
            SkipReason::Busy => "busy",
            SkipReason::NotEmpty => "not_empty",
            SkipReason::TooDeep => "too_deep",
            SkipReason::Io => "io",
        }
    }
}

/// One kept entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SkippedEntry {
    /// Path relative to the target.
    pub relative: PathBuf,
    /// The entry's kind, when known.
    pub kind: Option<EntryKind>,
    /// Why it was kept.
    pub reason: SkipReason,
    /// The OS error text.
    pub detail: String,
}

/// An unexpected condition that stops the walk (SAFE-05: never "best effort").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Anomaly {
    /// An entry was replaced between being examined and being opened or removed (different
    /// device + inode).
    IdentityChanged {
        /// What was examined.
        expected: FileIdentity,
        /// What was found.
        found: FileIdentity,
    },
    /// An entry changed kind between being examined and being opened or removed, e.g. a
    /// directory swapped for a symlink (`ELOOP` from the no-follow open). Never followed.
    KindChanged {
        /// What was examined.
        expected: EntryKind,
        /// What was found, when known.
        found: Option<EntryKind>,
    },
    /// An entry lives on another device than the target: a mount point (or Btrfs subvolume).
    /// Never crossed.
    MountCrossing {
        /// The target's device.
        root_device: u64,
        /// The entry's device.
        found_device: u64,
    },
    /// A directory the walker emptied is no longer at its name in its parent, or its parent's
    /// handle could not be re-verified: it was moved or removed during the walk.
    DirectoryMoved,
    /// A directory is on the target's device but another mount (Linux `statx` mount ID): a
    /// bind mount of the same filesystem. Never crossed.
    MountIdCrossing {
        /// The target's mount ID.
        root_mount_id: u64,
        /// The directory's mount ID.
        found_mount_id: u64,
    },
}

impl fmt::Display for Anomaly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Anomaly::IdentityChanged { expected, found } => {
                write!(
                    f,
                    "entry replaced during the walk ({expected} became {found})"
                )
            }
            Anomaly::KindChanged { expected, found } => match found {
                Some(found) => write!(f, "entry changed from {expected} to {found}"),
                None => write!(
                    f,
                    "entry is no longer a {expected} (a link is never followed)"
                ),
            },
            Anomaly::MountCrossing {
                root_device,
                found_device,
            } => write!(
                f,
                "mount point: device {found_device:x} differs from the target's {root_device:x}; never crossed"
            ),
            Anomaly::DirectoryMoved => {
                f.write_str("a directory was moved or removed during the walk")
            }
            Anomaly::MountIdCrossing {
                root_mount_id,
                found_mount_id,
            } => write!(
                f,
                "mount point on the same device (mount {found_mount_id}, target on {root_mount_id}); never crossed"
            ),
        }
    }
}

/// Where and why a walk was aborted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AbortReason {
    /// Path relative to the target.
    pub relative: PathBuf,
    /// What was seen.
    pub anomaly: Anomaly,
}

/// The result of a walk that started (see the module docs for `Err` versus `Ok`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoveReport {
    /// How the walk ended.
    pub outcome: RemoveOutcome,
    /// What was removed.
    pub removed: RemovedCounts,
    /// Kept entries, up to [`MAX_SKIPPED_LISTED`].
    pub skipped: Vec<SkippedEntry>,
    /// Number of kept entries, including those not listed.
    pub skipped_total: u64,
    /// Entries that disappeared on their own between listing and removal.
    pub vanished: u64,
    /// Set when [`RemoveReport::outcome`] is [`RemoveOutcome::Aborted`].
    pub aborted: Option<AbortReason>,
    /// Whether mount IDs were compared besides devices (Linux with `statx` `STATX_MNT_ID`,
    /// kernel ≥ 5.8). When false, a bind mount of the same filesystem is not detected by the
    /// walker; the engine's mount checks (PRD §8.3) still classify such candidates S3.
    pub mount_ids_checked: bool,
}

#[cfg_attr(not(unix), allow(dead_code))]
impl RemoveReport {
    fn new() -> Self {
        Self {
            outcome: RemoveOutcome::Incomplete,
            removed: RemovedCounts::default(),
            skipped: Vec::new(),
            skipped_total: 0,
            vanished: 0,
            aborted: None,
            mount_ids_checked: false,
        }
    }
}

/// Why a request was refused. **Nothing was removed.**
#[derive(Debug)]
#[non_exhaustive]
pub enum RemoveError {
    /// The request itself is invalid (empty relative path, `..`, absolute path, NUL byte).
    InvalidRequest {
        /// What is wrong.
        reason: &'static str,
    },
    /// The anchor could not be opened as a directory.
    AnchorUnavailable {
        /// The anchor.
        anchor: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The opened anchor is not the expected directory.
    AnchorChanged {
        /// Expected identity.
        expected: FileIdentity,
        /// Found identity.
        found: FileIdentity,
    },
    /// A component between the anchor and the target is not a real directory (e.g. a symlink,
    /// refused rather than followed), is missing or is not accessible.
    PathNotTraversable {
        /// The anchor-relative path up to and including the failing component.
        component: PathBuf,
        /// The OS error (`ELOOP` / `ENOTDIR` for a link).
        source: io::Error,
    },
    /// Nothing exists at the target path.
    TargetMissing,
    /// The target could not be examined, opened or listed (e.g. permission denied).
    TargetUnavailable {
        /// The OS error.
        source: io::Error,
    },
    /// The target is a different object than planned (SAFE-05, FX-RACE-01).
    IdentityMismatch {
        /// Expected identity.
        expected: FileIdentity,
        /// Found identity.
        found: FileIdentity,
    },
    /// The target is not of the expected kind.
    KindMismatch {
        /// Expected kind.
        expected: EntryKind,
        /// Found kind.
        found: EntryKind,
    },
    /// The target is a mount point (or a Btrfs subvolume): another device than the directory
    /// holding it or, on Linux, another mount ID (a bind mount). It is never emptied.
    TargetIsMountPoint {
        /// Device of the directory holding the target.
        parent_device: u64,
        /// Device of the target.
        target_device: u64,
        /// Mount ID of the directory holding the target, where the OS reports one.
        parent_mount_id: Option<u64>,
        /// Mount ID of the target, where the OS reports one.
        target_mount_id: Option<u64>,
    },
    /// A single non-directory target has other hard links: removing this name would free
    /// nothing (another link keeps the data) and the other name may be the user's.
    HardLinked {
        /// Its link count.
        nlink: u64,
    },
    /// The target changed between being examined and being opened (identity or kind).
    TargetChanged {
        /// What changed.
        anomaly: Anomaly,
    },
    /// This OS has no removal walker yet (Windows: P2-EXEC-02, #68).
    Unsupported,
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoveError::InvalidRequest { reason } => {
                write!(f, "invalid removal request: {reason}")
            }
            RemoveError::AnchorUnavailable { anchor, source } => {
                write!(f, "cannot open anchor {}: {source}", anchor.display())
            }
            RemoveError::AnchorChanged { expected, found } => {
                write!(f, "anchor changed (expected {expected}, found {found})")
            }
            RemoveError::PathNotTraversable { component, source } => write!(
                f,
                "cannot traverse {} without following a link: {source}",
                component.display()
            ),
            RemoveError::TargetMissing => f.write_str("target does not exist"),
            RemoveError::TargetUnavailable { source } => write!(f, "target unavailable: {source}"),
            RemoveError::IdentityMismatch { expected, found } => write!(
                f,
                "target is a different object than planned (expected {expected}, found {found})"
            ),
            RemoveError::KindMismatch { expected, found } => {
                write!(f, "target is a {found}, expected a {expected}")
            }
            RemoveError::TargetIsMountPoint {
                parent_device,
                target_device,
                parent_mount_id,
                target_mount_id,
            } => write!(
                f,
                "target is a mount point (device {target_device:x}, mount {target_mount_id:?}; \
                 its directory: device {parent_device:x}, mount {parent_mount_id:?}); never emptied"
            ),
            RemoveError::HardLinked { nlink } => write!(
                f,
                "target has {nlink} hard links: removing one name frees nothing; refused"
            ),
            RemoveError::TargetChanged { anomaly } => write!(f, "target changed: {anomaly}"),
            RemoveError::Unsupported => {
                f.write_str("removal is not implemented on this OS yet (P2-EXEC-02, #68)")
            }
        }
    }
}

impl std::error::Error for RemoveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RemoveError::AnchorUnavailable { source, .. }
            | RemoveError::PathNotTraversable { source, .. }
            | RemoveError::TargetUnavailable { source } => Some(source),
            _ => None,
        }
    }
}

/// Removes the target of `req` and everything below it, without following any link or
/// crossing any mount (see the module docs).
pub fn remove_tree(req: &RemoveRequest<'_>) -> Result<RemoveReport, RemoveError> {
    imp(req, None)
}

/// [`remove_tree`] with an observer called at each [`WalkEvent`] (fault-injection tests).
pub fn remove_tree_observed(
    req: &RemoveRequest<'_>,
    observer: &mut dyn FnMut(&WalkEvent<'_>),
) -> Result<RemoveReport, RemoveError> {
    imp(req, Some(observer))
}

#[cfg(unix)]
fn imp(
    req: &RemoveRequest<'_>,
    observer: Option<&mut dyn FnMut(&WalkEvent<'_>)>,
) -> Result<RemoveReport, RemoveError> {
    unix::remove_tree(req, observer, &unix::Tuning::default())
}

#[cfg(not(unix))]
fn imp(
    _req: &RemoveRequest<'_>,
    _observer: Option<&mut dyn FnMut(&WalkEvent<'_>)>,
) -> Result<RemoveReport, RemoveError> {
    Err(RemoveError::Unsupported)
}

/// The relative path as plain names, or why it is not acceptable.
#[cfg_attr(not(unix), allow(dead_code))]
fn plain_components(relative: &Path) -> Result<Vec<&std::ffi::OsStr>, RemoveError> {
    use std::path::Component;
    let mut out = Vec::new();
    for c in relative.components() {
        match c {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                return Err(RemoveError::InvalidRequest {
                    reason: "`..` is not allowed in the relative path",
                });
            }
            Component::CurDir => {
                return Err(RemoveError::InvalidRequest {
                    reason: "`.` is not allowed in the relative path",
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(RemoveError::InvalidRequest {
                    reason: "the target path must be relative to the anchor",
                });
            }
        }
    }
    if out.is_empty() {
        return Err(RemoveError::InvalidRequest {
            reason: "the relative path is empty (the anchor itself is never removed)",
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_must_be_plain_names() {
        for bad in ["", ".", "..", "a/../b", "./a", "/abs"] {
            assert!(
                matches!(
                    plain_components(Path::new(bad)),
                    Err(RemoveError::InvalidRequest { .. })
                ),
                "{bad:?}"
            );
        }
        assert_eq!(
            plain_components(Path::new("a/b")).map(|v| v.len()).ok(),
            Some(2)
        );
    }
}
