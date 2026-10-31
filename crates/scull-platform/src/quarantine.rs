//! Same-volume moves for quarantine and restore (D-3, SAFE-16; P2-EXEC-05/06).
//!
//! **Only `scull-cleaner`'s Executor may call this** (SAFE-10; `delete` cargo feature).
//!
//! [`move_no_replace`] moves one verified entry into a destination directory with a single
//! atomic rename that **never replaces** anything and **never crosses a volume**:
//!
//! * **Linux:** `renameat2(src_parent_fd, name, dst_dir_fd, new_name, RENAME_NOREPLACE)`.
//! * **macOS:** `renameatx_np(.., RENAME_EXCL)`.
//! * The source is reached like the removal walker reaches its target: the trusted anchor,
//!   then every component below it opened `O_DIRECTORY | O_NOFOLLOW`, then a no-follow
//!   `fstatat` of the entry checked against the expected identity and kind; a mount point is
//!   refused. The destination directory is opened no-follow and checked against its expected
//!   identity. After the rename the entry is looked up again at its new name and must still be
//!   the expected object.
//! * A rename never copies: across volumes the kernel answers `EXDEV`, reported as
//!   [`MoveError::CrossDevice`]. Where the filesystem can't do a no-replace rename the move is
//!   refused ([`MoveError::NoReplaceUnsupported`]); there is no fallback to a plain rename.
//! * **Windows:** the walker's handle discipline (#68): the source is opened relative to its
//!   parent handle, no-follow, with `DELETE` access and verified on that handle; the rename is
//!   `NtSetInformationFile(FileRenameInformationEx, FILE_RENAME_POSIX_SEMANTICS)` **without**
//!   `FILE_RENAME_REPLACE_IF_EXISTS`, on that handle, into the verified destination handle
//!   (classic `FileRenameInformation` with `ReplaceIfExists = FALSE` where the extended class
//!   is unsupported). See `quarantine/windows.rs`. Compiled for Windows; its tests run on
//!   Windows CI.
//!
//! [`quarantine_base`] chooses where a volume's quarantine lives (see its docs).

use crate::EntryKind;
use scull_core::FileIdentity;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Name of the per-volume quarantine directory.
pub const QUARANTINE_DIR_NAME: &str = ".scull-quarantine";

/// One move: the entry, how to reach it, the identity it must have, and where it goes.
#[derive(Debug, Clone, Copy)]
pub struct MoveRequest<'a> {
    /// Trusted directory the source walk starts from (opened as given).
    pub anchor: &'a Path,
    /// Identity the opened anchor must have, if known.
    pub anchor_identity: Option<FileIdentity>,
    /// The entry, relative to the anchor: plain names only, every one but the last a real
    /// directory.
    pub relative: &'a Path,
    /// Identity the entry must have.
    pub expected: FileIdentity,
    /// Kind the entry must have, if known.
    pub expected_kind: Option<EntryKind>,
    /// Destination directory (absolute; its last component is opened no-follow).
    pub dest_dir: &'a Path,
    /// Identity the destination directory must have, if known.
    pub dest_dir_identity: Option<FileIdentity>,
    /// New name inside the destination: one plain name.
    pub dest_name: &'a OsStr,
}

/// A completed move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Moved {
    /// The entry's kind.
    pub kind: EntryKind,
    /// The entry's identity (unchanged by the rename).
    pub identity: FileIdentity,
    /// Identity of the directory the entry was moved out of.
    pub source_parent: FileIdentity,
    /// Identity of the directory the entry was moved into.
    pub dest_dir: FileIdentity,
}

/// A refused or failed move. Unless stated otherwise, nothing moved.
#[derive(Debug)]
#[non_exhaustive]
pub enum MoveError {
    /// The request is malformed.
    InvalidRequest {
        /// Why.
        reason: &'static str,
    },
    /// The anchor could not be opened.
    AnchorUnavailable {
        /// The OS error.
        source: io::Error,
    },
    /// The anchor is not the expected directory.
    AnchorChanged {
        /// Expected identity.
        expected: FileIdentity,
        /// Found identity.
        found: FileIdentity,
    },
    /// A component between the anchor and the entry is not a real directory (e.g. a link).
    PathNotTraversable {
        /// The component, relative to the anchor.
        component: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The entry does not exist.
    SourceMissing,
    /// The entry could not be inspected.
    SourceUnavailable {
        /// The OS error.
        source: io::Error,
    },
    /// The entry is not the expected object.
    IdentityMismatch {
        /// Expected identity.
        expected: FileIdentity,
        /// Found identity.
        found: FileIdentity,
    },
    /// The entry is not of the expected kind.
    KindMismatch {
        /// Expected kind.
        expected: EntryKind,
        /// Found kind.
        found: EntryKind,
    },
    /// The entry is a mount point.
    SourceIsMountPoint,
    /// The destination directory could not be opened (or is a link).
    DestinationUnavailable {
        /// The OS error.
        source: io::Error,
    },
    /// The destination directory is not the expected one.
    DestinationChanged {
        /// Expected identity.
        expected: FileIdentity,
        /// Found identity.
        found: FileIdentity,
    },
    /// Something already exists at the destination name; nothing was replaced.
    DestinationExists,
    /// Source and destination are on different volumes (or mounts); a rename can't move it.
    CrossDevice,
    /// The OS refused the rename (permissions, read-only filesystem or parent).
    NotPermitted {
        /// The OS error.
        source: io::Error,
    },
    /// The filesystem can't rename without replacing; a plain rename is never used instead.
    NoReplaceUnsupported {
        /// The OS error.
        source: io::Error,
    },
    /// After the rename, the new name did not refer to the expected object: someone re-pointed
    /// the source name in between. `moved_back` says whether the object was moved back to the
    /// source name (no-replace); if not, it is still at the destination name.
    MovedUnexpected {
        /// What was found at the destination name.
        found: Option<FileIdentity>,
        /// Whether it was moved back.
        moved_back: bool,
    },
    /// Another I/O error.
    Io {
        /// The OS error.
        source: io::Error,
    },
    /// Not implemented on this platform yet.
    Unsupported,
}

impl MoveError {
    /// Whether this means the move is impossible here (permissions, read-only, another
    /// volume, no no-replace rename, no platform support), as opposed to the entry having
    /// changed: the cue to fall back to Trash, then to block (D-3).
    pub fn is_impossible_here(&self) -> bool {
        matches!(
            self,
            MoveError::CrossDevice
                | MoveError::NotPermitted { .. }
                | MoveError::NoReplaceUnsupported { .. }
                | MoveError::DestinationUnavailable { .. }
                | MoveError::Unsupported
        )
    }

    /// Stable code for reports and the audit log.
    pub fn code(&self) -> &'static str {
        match self {
            MoveError::InvalidRequest { .. } => "invalid_request",
            MoveError::AnchorUnavailable { .. } => "anchor_unavailable",
            MoveError::AnchorChanged { .. } => "anchor_changed",
            MoveError::PathNotTraversable { .. } => "path_not_traversable",
            MoveError::SourceMissing => "missing",
            MoveError::SourceUnavailable { .. } => "source_unavailable",
            MoveError::IdentityMismatch { .. } => "identity_changed",
            MoveError::KindMismatch { .. } => "kind_changed",
            MoveError::SourceIsMountPoint => "mount_point",
            MoveError::DestinationUnavailable { .. } => "destination_unavailable",
            MoveError::DestinationChanged { .. } => "destination_changed",
            MoveError::DestinationExists => "destination_exists",
            MoveError::CrossDevice => "cross_device",
            MoveError::NotPermitted { .. } => "not_permitted",
            MoveError::NoReplaceUnsupported { .. } => "no_replace_unsupported",
            MoveError::MovedUnexpected { .. } => "moved_unexpected",
            MoveError::Io { .. } => "io",
            MoveError::Unsupported => "unsupported_platform",
        }
    }
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveError::InvalidRequest { reason } => write!(f, "invalid move request: {reason}"),
            MoveError::AnchorUnavailable { source } => {
                write!(f, "cannot open the anchor directory: {source}")
            }
            MoveError::AnchorChanged { .. } => f.write_str("the anchor directory was replaced"),
            MoveError::PathNotTraversable { component, source } => write!(
                f,
                "{} is not a real directory (links are never followed): {source}",
                component.display()
            ),
            MoveError::SourceMissing => f.write_str("the entry no longer exists"),
            MoveError::SourceUnavailable { source } => {
                write!(f, "cannot inspect the entry: {source}")
            }
            MoveError::IdentityMismatch { .. } => {
                f.write_str("the entry is not the expected object (identity changed)")
            }
            MoveError::KindMismatch { expected, found } => {
                write!(f, "the entry is a {found}, expected a {expected}")
            }
            MoveError::SourceIsMountPoint => f.write_str("the entry is a mount point"),
            MoveError::DestinationUnavailable { source } => {
                write!(f, "cannot open the destination directory: {source}")
            }
            MoveError::DestinationChanged { .. } => {
                f.write_str("the destination directory was replaced")
            }
            MoveError::DestinationExists => {
                f.write_str("something already exists at the destination; nothing was replaced")
            }
            MoveError::CrossDevice => {
                f.write_str("source and destination are on different volumes")
            }
            MoveError::NotPermitted { source } => write!(f, "the OS refused the move: {source}"),
            MoveError::NoReplaceUnsupported { source } => write!(
                f,
                "this filesystem cannot rename without replacing ({source}); a plain rename is \
                 never used instead"
            ),
            MoveError::MovedUnexpected { moved_back, .. } => write!(
                f,
                "the name was re-pointed during the move; the object {}",
                if *moved_back {
                    "was moved back"
                } else {
                    "remains at the destination"
                }
            ),
            MoveError::Io { source } => write!(f, "I/O error: {source}"),
            MoveError::Unsupported => {
                f.write_str("same-volume moves are not implemented on this platform yet")
            }
        }
    }
}

impl std::error::Error for MoveError {}

/// Moves one verified entry into `dest_dir` under `dest_name` with an atomic, same-volume,
/// no-replace rename. See the module docs.
pub fn move_no_replace(req: &MoveRequest<'_>) -> Result<Moved, MoveError> {
    imp::move_no_replace(req)
}

/// Where the quarantine of the volume holding `item_parent` lives: the **shallowest** directory
/// among `item_parent` and its ancestors that is on the same device as `item_parent`, is a real
/// directory (not a link) and is **owned by the current user** (e.g. the home directory on a
/// single-volume system, or the user's own folder on a shared data volume). The quarantine is
/// `<base>/.scull-quarantine/<operation-id>/`, created owner-only, so it is per user and never
/// shared with another account even on a shared volume (FX-EXEC-16).
///
/// `Ok(None)` when there is no such directory (e.g. every directory up to the item belongs to
/// someone else): quarantine is impossible there, and the caller falls back to Trash, then
/// blocks (D-3). Nothing is created here.
pub fn quarantine_base(item_parent: &Path) -> io::Result<Option<PathBuf>> {
    imp::quarantine_base(item_parent)
}

#[cfg(unix)]
mod imp {
    use super::{MoveError, MoveRequest, Moved};
    use crate::remove::plain_components;
    use crate::remove::unix::{device, dir_flags, identity, io_err, kind, raw_mount_id, retry};
    use rustix::fs::{
        AtFlags, Mode, OFlags, RenameFlags, fstat, open, openat, renameat_with, statat,
    };
    use rustix::io::Errno;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    pub(super) fn move_no_replace(req: &MoveRequest<'_>) -> Result<Moved, MoveError> {
        let names = plain_components(req.relative)
            .map_err(|_| MoveError::InvalidRequest {
                reason: "the relative path must be plain names",
            })?
            .into_iter()
            .map(|n| CString::new(n.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| MoveError::InvalidRequest {
                reason: "a path component contains a NUL byte",
            })?;
        let Some((name, intermediate)) = names.split_last() else {
            return Err(MoveError::InvalidRequest {
                reason: "the relative path is empty",
            });
        };
        let dest_name = match plain_components(Path::new(req.dest_name)).as_deref() {
            Ok([one]) => CString::new(one.as_bytes()).map_err(|_| MoveError::InvalidRequest {
                reason: "the destination name contains a NUL byte",
            })?,
            _ => {
                return Err(MoveError::InvalidRequest {
                    reason: "the destination name must be one plain name",
                });
            }
        };
        if !req.dest_dir.is_absolute() {
            return Err(MoveError::InvalidRequest {
                reason: "the destination directory must be absolute",
            });
        }

        // Source: the anchor as given, then every component no-follow.
        let anchor_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOCTTY | OFlags::CLOEXEC;
        let mut parent = retry(|| open(req.anchor, anchor_flags, Mode::empty()))
            .map_err(|e| MoveError::AnchorUnavailable { source: io_err(e) })?;
        if let Some(expected) = req.anchor_identity {
            let st = retry(|| fstat(&parent))
                .map_err(|e| MoveError::AnchorUnavailable { source: io_err(e) })?;
            if identity(&st) != expected {
                return Err(MoveError::AnchorChanged {
                    expected,
                    found: identity(&st),
                });
            }
        }
        let mut walked = PathBuf::new();
        for n in intermediate {
            walked.push(std::ffi::OsStr::from_bytes(n.as_bytes()));
            parent = retry(|| openat(&parent, n.as_c_str(), dir_flags(), Mode::empty())).map_err(
                |e| MoveError::PathNotTraversable {
                    component: walked.clone(),
                    source: io_err(e),
                },
            )?;
        }
        let st = match retry(|| statat(&parent, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)) {
            Ok(st) => st,
            Err(Errno::NOENT) => return Err(MoveError::SourceMissing),
            Err(e) => return Err(MoveError::SourceUnavailable { source: io_err(e) }),
        };
        if identity(&st) != req.expected {
            return Err(MoveError::IdentityMismatch {
                expected: req.expected,
                found: identity(&st),
            });
        }
        let found_kind = kind(&st);
        if let Some(expected) = req.expected_kind
            && expected != found_kind
        {
            return Err(MoveError::KindMismatch {
                expected,
                found: found_kind,
            });
        }
        let parent_st = retry(|| fstat(&parent))
            .map_err(|e| MoveError::SourceUnavailable { source: io_err(e) })?;
        let parent_mount = raw_mount_id(parent.as_fd(), c"");
        let entry_mount = raw_mount_id(parent.as_fd(), name.as_c_str());
        if device(&parent_st) != device(&st)
            || matches!((parent_mount, entry_mount), (Some(p), Some(e)) if p != e)
        {
            return Err(MoveError::SourceIsMountPoint);
        }

        // Destination: opened no-follow and checked.
        let dest = retry(|| open(req.dest_dir, dir_flags(), Mode::empty()))
            .map_err(|e| MoveError::DestinationUnavailable { source: io_err(e) })?;
        let dest_st = retry(|| fstat(&dest))
            .map_err(|e| MoveError::DestinationUnavailable { source: io_err(e) })?;
        if let Some(expected) = req.dest_dir_identity
            && identity(&dest_st) != expected
        {
            return Err(MoveError::DestinationChanged {
                expected,
                found: identity(&dest_st),
            });
        }
        if device(&dest_st) != device(&st) {
            return Err(MoveError::CrossDevice);
        }

        // The rename: atomic, same volume, never replacing.
        match retry(|| {
            renameat_with(
                &parent,
                name.as_c_str(),
                &dest,
                dest_name.as_c_str(),
                RenameFlags::NOREPLACE,
            )
        }) {
            Ok(()) => {}
            Err(Errno::EXIST | Errno::NOTEMPTY) => return Err(MoveError::DestinationExists),
            Err(Errno::XDEV) => return Err(MoveError::CrossDevice),
            Err(Errno::NOENT) => return Err(MoveError::SourceMissing),
            Err(e @ (Errno::ACCESS | Errno::PERM | Errno::ROFS)) => {
                return Err(MoveError::NotPermitted { source: io_err(e) });
            }
            Err(e @ (Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP)) => {
                return Err(MoveError::NoReplaceUnsupported { source: io_err(e) });
            }
            Err(e) => return Err(MoveError::Io { source: io_err(e) }),
        }

        // The new name must refer to the object that was checked.
        let now = retry(|| statat(&dest, dest_name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)).ok();
        match now.as_ref().map(identity) {
            Some(id) if id == req.expected => Ok(Moved {
                kind: found_kind,
                identity: req.expected,
                source_parent: identity(&parent_st),
                dest_dir: identity(&dest_st),
            }),
            found => {
                let moved_back = retry(|| {
                    renameat_with(
                        &dest,
                        dest_name.as_c_str(),
                        &parent,
                        name.as_c_str(),
                        RenameFlags::NOREPLACE,
                    )
                })
                .is_ok();
                Err(MoveError::MovedUnexpected { found, moved_back })
            }
        }
    }

    pub(super) fn quarantine_base(item_parent: &Path) -> io::Result<Option<PathBuf>> {
        let dev = std::fs::symlink_metadata(item_parent)?.dev();
        let uid = crate::native::effective_uid();
        let mut chain: Vec<&Path> = item_parent.ancestors().collect();
        chain.reverse();
        for dir in chain {
            let Ok(md) = std::fs::symlink_metadata(dir) else {
                continue;
            };
            if md.file_type().is_dir() && md.dev() == dev && md.uid() == uid {
                return Ok(Some(dir.to_path_buf()));
            }
        }
        Ok(None)
    }
}

#[cfg(windows)]
#[path = "quarantine/windows.rs"]
mod imp;

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::{MoveError, MoveRequest, Moved};
    use std::io;
    use std::path::{Path, PathBuf};

    pub(super) fn move_no_replace(_req: &MoveRequest<'_>) -> Result<Moved, MoveError> {
        Err(MoveError::Unsupported)
    }

    pub(super) fn quarantine_base(_item_parent: &Path) -> io::Result<Option<PathBuf>> {
        Ok(None)
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{Native, Platform};
    use std::fs;

    fn id(p: &Path) -> FileIdentity {
        Native.metadata(p).unwrap().identity.unwrap()
    }

    struct Tree {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        dest: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::create_dir_all(root.join("proj/cand/sub")).unwrap();
        fs::write(root.join("proj/cand/sub/f.txt"), b"data").unwrap();
        fs::create_dir(root.join("q")).unwrap();
        let dest = root.join("q");
        Tree {
            _tmp: tmp,
            root,
            dest,
        }
    }

    fn req<'a>(t: &'a Tree, expected: FileIdentity, name: &'a str) -> MoveRequest<'a> {
        MoveRequest {
            anchor: &t.root,
            anchor_identity: Some(id(&t.root)),
            relative: Path::new("proj/cand"),
            expected,
            expected_kind: Some(EntryKind::Dir),
            dest_dir: &t.dest,
            dest_dir_identity: Some(id(&t.dest)),
            dest_name: OsStr::new(name),
        }
    }

    #[test]
    fn moves_atomically_keeping_identity() {
        let t = tree();
        let before = id(&t.root.join("proj/cand"));
        let m = move_no_replace(&req(&t, before, "0001")).unwrap();
        assert_eq!(m.identity, before);
        assert_eq!(m.kind, EntryKind::Dir);
        assert_eq!(m.source_parent, id(&t.root.join("proj")));
        assert!(!t.root.join("proj/cand").exists());
        assert_eq!(id(&t.dest.join("0001")), before, "a rename, not a copy");
        assert_eq!(fs::read(t.dest.join("0001/sub/f.txt")).unwrap(), b"data");
    }

    #[test]
    fn never_replaces_an_existing_destination() {
        let t = tree();
        fs::write(t.dest.join("0001"), b"keep me").unwrap();
        let before = id(&t.root.join("proj/cand"));
        let e = move_no_replace(&req(&t, before, "0001")).unwrap_err();
        assert!(matches!(e, MoveError::DestinationExists), "{e}");
        assert_eq!(fs::read(t.dest.join("0001")).unwrap(), b"keep me");
        assert!(t.root.join("proj/cand/sub/f.txt").is_file());
    }

    #[test]
    fn refuses_a_replaced_entry_or_a_link_in_the_path() {
        let t = tree();
        let wrong = id(&t.root.join("proj"));
        let e = move_no_replace(&req(&t, wrong, "0001")).unwrap_err();
        assert!(matches!(e, MoveError::IdentityMismatch { .. }), "{e}");

        let before = id(&t.root.join("proj/cand"));
        fs::rename(t.root.join("proj"), t.root.join("real")).unwrap();
        std::os::unix::fs::symlink(t.root.join("real"), t.root.join("proj")).unwrap();
        let e = move_no_replace(&req(&t, before, "0001")).unwrap_err();
        assert!(matches!(e, MoveError::PathNotTraversable { .. }), "{e}");
        assert!(t.root.join("real/cand/sub/f.txt").is_file());
    }

    #[test]
    fn refuses_a_destination_that_is_a_link_or_was_replaced() {
        let t = tree();
        let before = id(&t.root.join("proj/cand"));
        let mut r = req(&t, before, "0001");
        r.dest_dir_identity = Some(id(&t.root));
        assert!(matches!(
            move_no_replace(&r).unwrap_err(),
            MoveError::DestinationChanged { .. }
        ));
        let link = t.root.join("qlink");
        std::os::unix::fs::symlink(&t.dest, &link).unwrap();
        r.dest_dir = &link;
        r.dest_dir_identity = None;
        assert!(matches!(
            move_no_replace(&r).unwrap_err(),
            MoveError::DestinationUnavailable { .. }
        ));
        assert!(t.root.join("proj/cand/sub/f.txt").is_file());
    }

    #[test]
    fn read_only_parent_is_impossible_here_and_nothing_moves() {
        use std::os::unix::fs::PermissionsExt;
        if crate::native::effective_uid() == 0 {
            return; // root ignores directory permissions
        }
        let t = tree();
        let before = id(&t.root.join("proj/cand"));
        let proj = t.root.join("proj");
        fs::set_permissions(&proj, fs::Permissions::from_mode(0o555)).unwrap();
        let e = move_no_replace(&req(&t, before, "0001")).unwrap_err();
        fs::set_permissions(&proj, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(e.is_impossible_here(), "{e}");
        assert!(t.root.join("proj/cand/sub/f.txt").is_file());
    }

    #[test]
    fn quarantine_base_is_the_shallowest_directory_owned_by_the_user() {
        use std::os::unix::fs::MetadataExt;
        let t = tree();
        let item_parent = t.root.join("proj");
        let base = quarantine_base(&item_parent).unwrap().unwrap();
        assert!(item_parent.starts_with(&base), "{base:?}");
        let uid = crate::native::effective_uid();
        assert_eq!(fs::metadata(&base).unwrap().uid(), uid);
        // Every shallower ancestor on the same device is someone else's.
        let dev = fs::metadata(&item_parent).unwrap().dev();
        for a in base.ancestors().skip(1) {
            let md = fs::symlink_metadata(a).unwrap();
            assert!(md.dev() != dev || md.uid() != uid, "{a:?}");
        }
    }
}
