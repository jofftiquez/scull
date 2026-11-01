//! Linux and macOS: the removal walker on `openat` / `fstatat` / `unlinkat` / `getdents`
//! (through `rustix`, so no `unsafe`). See the parent module for the contract.

// `stat` field widths differ per OS and architecture (e.g. macOS `st_dev` is `i32`); the casts
// below mirror `std::os::unix::fs::MetadataExt`, so identities match `Platform::metadata`.
#![allow(clippy::unnecessary_cast)]

use super::{
    AbortReason, Anomaly, FD_BUDGET, MAX_DEPTH, MAX_SKIPPED_LISTED, RemoveError, RemoveOutcome,
    RemoveReport, RemoveRequest, SkipReason, SkippedEntry, VERIFY_EVERY, WalkEvent,
    plain_components,
};
use crate::EntryKind;
use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, Stat, fstat, open, openat, statat, unlinkat,
};
use rustix::io::Errno;
use scull_core::FileIdentity;
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// `st_blocks` is in 512-byte units on Linux and macOS.
const ST_BLOCK_UNIT: u64 = 512;

/// Walker limits. Tests shrink them to exercise the re-open and depth paths on small trees.
pub(crate) struct Tuning {
    /// Directory handles kept open at once (at least 1).
    pub(crate) fd_budget: usize,
    /// Deepest level descended to (the target is depth 0).
    pub(crate) max_depth: usize,
    /// Re-verify a directory's place at least every this many entries (at least 1).
    pub(crate) verify_every: usize,
    /// Test only: entries with this name report a different device, to simulate a mount point
    /// where the test cannot mount one.
    #[cfg(test)]
    pub(crate) foreign_device_name: Option<CString>,
    /// Test only: entries with this name report a different mount ID (a simulated bind mount
    /// of the same filesystem).
    #[cfg(test)]
    pub(crate) foreign_mount_name: Option<CString>,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            fd_budget: FD_BUDGET,
            max_depth: MAX_DEPTH,
            verify_every: VERIFY_EVERY,
            #[cfg(test)]
            foreign_device_name: None,
            #[cfg(test)]
            foreign_mount_name: None,
        }
    }
}

/// The mount ID (`statx` `STATX_MNT_ID`, Linux ≥ 5.8) of `name` in `dirfd` (no-follow), or of
/// `dirfd` itself when `name` is empty. `None` where the kernel or OS cannot tell: then only
/// `st_dev` is compared (recorded in [`RemoveReport::mount_ids_checked`]).
#[cfg(target_os = "linux")]
pub(crate) fn raw_mount_id(dirfd: BorrowedFd<'_>, name: &CStr) -> Option<u64> {
    use rustix::fs::{StatxFlags, statx};
    let flags = if name.is_empty() {
        AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW
    } else {
        AtFlags::SYMLINK_NOFOLLOW
    };
    let stx = retry(|| statx(dirfd, name, flags, StatxFlags::MNT_ID)).ok()?;
    (stx.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stx.stx_mnt_id)
}

/// macOS (and other Unixes) have no per-mount ID in `stat`; bind mounts don't exist there.
#[cfg(not(target_os = "linux"))]
pub(crate) fn raw_mount_id(_dirfd: BorrowedFd<'_>, _name: &CStr) -> Option<u64> {
    None
}

/// [`raw_mount_id`], with the test-only simulated bind mount for the entry `label`.
#[cfg_attr(not(test), allow(unused_variables))]
fn mount_id(tuning: &Tuning, dirfd: BorrowedFd<'_>, name: &CStr, label: &CStr) -> Option<u64> {
    let id = raw_mount_id(dirfd, name)?;
    #[cfg(test)]
    if tuning.foreign_mount_name.as_deref() == Some(label) {
        return Some(id ^ 1);
    }
    Some(id)
}

/// Whether this kernel reports mount IDs (tests use it to decide what to expect).
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn mount_ids_supported() -> bool {
    retry(|| {
        open(
            c"/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
    })
    .ok()
    .and_then(|fd| raw_mount_id(fd.as_fd(), c""))
    .is_some()
}

/// Flags for every directory the walker opens below the anchor: never follow a link at the
/// final component, fail on anything that is not a directory, never block (a FIFO swapped in
/// cannot hang the walk), never become a controlling terminal, never leak into children.
pub(crate) fn dir_flags() -> OFlags {
    OFlags::RDONLY
        | OFlags::DIRECTORY
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK
        | OFlags::NOCTTY
        | OFlags::CLOEXEC
}

/// Retries a call interrupted by a signal (`EINTR`).
pub(crate) fn retry<T>(mut f: impl FnMut() -> rustix::io::Result<T>) -> rustix::io::Result<T> {
    loop {
        match f() {
            Err(Errno::INTR) => continue,
            other => return other,
        }
    }
}

pub(crate) fn io_err(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

pub(crate) fn device(st: &Stat) -> u64 {
    st.st_dev as u64
}

/// The device of the entry `name` (tests may simulate a mount point by name).
#[cfg_attr(not(test), allow(unused_variables))]
fn device_of(tuning: &Tuning, st: &Stat, name: &CStr) -> u64 {
    #[cfg(test)]
    if tuning.foreign_device_name.as_deref() == Some(name) {
        return device(st) ^ 1;
    }
    device(st)
}

pub(crate) fn identity(st: &Stat) -> FileIdentity {
    FileIdentity::new(device(st), u128::from(st.st_ino as u64))
}

pub(crate) fn kind(st: &Stat) -> EntryKind {
    kind_of(FileType::from_raw_mode(st.st_mode as _))
}

/// The listing's `d_type`, when the filesystem provides one.
fn listed_kind(ft: FileType) -> Option<EntryKind> {
    (ft != FileType::Unknown).then(|| kind_of(ft))
}

fn kind_of(ft: FileType) -> EntryKind {
    match ft {
        FileType::RegularFile => EntryKind::File,
        FileType::Directory => EntryKind::Dir,
        FileType::Symlink => EntryKind::Symlink,
        FileType::Fifo => EntryKind::Fifo,
        FileType::Socket => EntryKind::Socket,
        FileType::CharacterDevice => EntryKind::CharDevice,
        FileType::BlockDevice => EntryKind::BlockDevice,
        _ => EntryKind::Other,
    }
}

fn skip_reason(e: Errno) -> SkipReason {
    match e {
        Errno::ACCESS | Errno::PERM => SkipReason::PermissionDenied,
        Errno::ROFS => SkipReason::ReadOnlyFilesystem,
        Errno::BUSY => SkipReason::Busy,
        Errno::NOTEMPTY | Errno::EXIST => SkipReason::NotEmpty,
        _ => SkipReason::Io,
    }
}

/// One listed name and the kind the listing reported for it (`d_type`), if any.
type Listed = (CString, Option<EntryKind>);

/// Every name in the directory behind `fd`, except `.` and `..`, with its `d_type`. Read in
/// full before anything in the directory is removed, so removal never disturbs the listing.
fn list(fd: &OwnedFd) -> rustix::io::Result<Vec<Listed>> {
    // `read_from` reads through its own descriptor, opened as `openat(fd, ".")`: the same,
    // already verified directory.
    let mut dir = retry(|| Dir::read_from(fd))?;
    let mut names = Vec::new();
    while let Some(entry) = dir.read() {
        let entry = entry?;
        let name = entry.file_name();
        if name != c"." && name != c".." {
            names.push((name.to_owned(), listed_kind(entry.file_type())));
        }
    }
    Ok(names)
}

/// Whether the directory behind `fd` is still the entry `name` (identity `id`) of the directory
/// `parent_id`: `..` is re-opened from the held handle and must be the recorded parent, and the
/// name in it must still refer to `id`. POSIX removes by name, not by identity, so this is how
/// the walker learns that a directory it holds was moved (possibly out of the candidate).
fn still_in_place(fd: &OwnedFd, name: &CStr, id: FileIdentity, parent_id: FileIdentity) -> bool {
    let Ok(parent) = retry(|| openat(fd, c"..", dir_flags(), Mode::empty())) else {
        return false;
    };
    let parent_ok = retry(|| fstat(&parent)).is_ok_and(|st| identity(&st) == parent_id);
    parent_ok
        && retry(|| statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW))
            .is_ok_and(|st| identity(&st) == id)
}

/// One directory on the walk stack.
struct Frame {
    /// Its name in its parent.
    name: CString,
    /// Device + inode, as verified on the opened handle.
    id: FileIdentity,
    /// Device + inode of its parent directory.
    parent_id: FileIdentity,
    /// The handle; `None` while closed to stay within the descriptor budget.
    fd: Option<OwnedFd>,
    /// Names not processed yet, with their listed kind.
    pending: Vec<Listed>,
    /// Something below was kept, so this directory is kept too.
    incomplete: bool,
    /// Entries processed since its place was last verified; `None`: verify before the next.
    since_verified: Option<usize>,
}

/// What happened to one entry.
enum Step {
    /// Unlinked.
    Removed,
    /// Gone before it could be removed.
    Vanished,
    /// Kept (recorded in the report); its parent is kept too.
    Kept,
    /// A directory to descend into.
    Descend(Frame),
}

/// Walk state other than the frame stack.
struct Walk<'o, 't> {
    report: RemoveReport,
    /// Path of the current entry relative to the target.
    rel: PathBuf,
    root_device: u64,
    /// The target's mount ID, when the OS reports one (Linux).
    root_mount: Option<u64>,
    observer: Option<&'o mut dyn FnMut(&WalkEvent<'_>)>,
    tuning: &'t Tuning,
}

type Abort = (PathBuf, Anomaly);

impl Walk<'_, '_> {
    fn observe(&mut self, event: WalkEvent<'_>) {
        if let Some(observer) = self.observer.as_mut() {
            observer(&event);
        }
    }

    /// Reports the current entry (`self.rel`) to the observer, if any.
    fn observe_current(&mut self, make: impl FnOnce(&std::path::Path) -> WalkEvent<'_>) {
        if let Some(observer) = self.observer.as_mut() {
            observer(&make(&self.rel));
        }
    }

    fn abort(&self, anomaly: Anomaly) -> Abort {
        (self.rel.clone(), anomaly)
    }

    fn keep(&mut self, kind: Option<EntryKind>, reason: SkipReason, detail: String) {
        self.report.skipped_total += 1;
        if self.report.skipped.len() < MAX_SKIPPED_LISTED {
            self.report.skipped.push(SkippedEntry {
                relative: self.rel.clone(),
                kind,
                reason,
                detail,
            });
        }
    }

    fn keep_errno(&mut self, kind: Option<EntryKind>, e: Errno) {
        self.keep(kind, skip_reason(e), io_err(e).to_string());
    }

    fn device_of(&self, st: &Stat, name: &CStr) -> u64 {
        device_of(self.tuning, st, name)
    }

    fn count_removed(&mut self, kind: EntryKind, st: &Stat) {
        let r = &mut self.report.removed;
        match kind {
            EntryKind::File => r.files += 1,
            EntryKind::Symlink => r.symlinks += 1,
            _ => r.special += 1,
        }
        r.logical_bytes = r.logical_bytes.saturating_add(st.st_size as u64);
        r.allocated_bytes = r
            .allocated_bytes
            .saturating_add((st.st_blocks as u64).saturating_mul(ST_BLOCK_UNIT));
    }

    /// Handles one entry of the directory `dirfd` (identity `parent_id`, at depth `depth - 1`);
    /// `self.rel` already names it; `listed` is the kind its listing reported, if any.
    fn entry(
        &mut self,
        dirfd: BorrowedFd<'_>,
        parent_id: FileIdentity,
        (name, listed): &Listed,
        depth: usize,
    ) -> Result<Step, Abort> {
        let name = name.as_c_str();
        let listed = *listed;
        let st = match retry(|| statat(dirfd, name, AtFlags::SYMLINK_NOFOLLOW)) {
            Ok(st) => st,
            Err(Errno::NOENT) => return Ok(Step::Vanished),
            Err(e) => {
                self.keep_errno(None, e);
                return Ok(Step::Kept);
            }
        };
        let found_device = self.device_of(&st, name);
        if found_device != self.root_device {
            return Err(self.abort(Anomaly::MountCrossing {
                root_device: self.root_device,
                found_device,
            }));
        }
        let kind = kind(&st);
        if let Some(listed) = listed
            && listed != kind
        {
            // Replaced since the directory was listed: stop.
            return Err(self.abort(Anomaly::KindChanged {
                expected: listed,
                found: Some(kind),
            }));
        }
        self.observe_current(|relative| WalkEvent::EntryChecked { relative, kind });

        if kind != EntryKind::Dir {
            // Files, symlinks (the link itself), FIFOs, sockets, devices: unlinked by name in
            // the held directory, never opened (SAFE-18).
            return match retry(|| unlinkat(dirfd, name, AtFlags::empty())) {
                Ok(()) => {
                    self.count_removed(kind, &st);
                    Ok(Step::Removed)
                }
                Err(Errno::NOENT) => Ok(Step::Vanished),
                // Linux: the name now refers to a directory.
                Err(Errno::ISDIR) => Err(self.abort(Anomaly::KindChanged {
                    expected: kind,
                    found: Some(EntryKind::Dir),
                })),
                Err(e) => {
                    self.keep_errno(Some(kind), e);
                    Ok(Step::Kept)
                }
            };
        }

        if depth > self.tuning.max_depth {
            self.keep(
                Some(kind),
                SkipReason::TooDeep,
                format!("deeper than {} levels", self.tuning.max_depth),
            );
            return Ok(Step::Kept);
        }
        let fd = match retry(|| openat(dirfd, name, dir_flags(), Mode::empty())) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(Step::Vanished),
            // Replaced by a symlink (ELOOP; EMLINK on some BSDs) or a non-directory (ENOTDIR)
            // since the `stat`: never followed, and the walk stops.
            Err(Errno::LOOP | Errno::MLINK | Errno::NOTDIR) => {
                return Err(self.abort(Anomaly::KindChanged {
                    expected: EntryKind::Dir,
                    found: None,
                }));
            }
            Err(e) => {
                self.keep_errno(Some(kind), e);
                return Ok(Step::Kept);
            }
        };
        let opened = match retry(|| fstat(&fd)) {
            Ok(opened) => opened,
            Err(e) => {
                self.keep_errno(Some(kind), e);
                return Ok(Step::Kept);
            }
        };
        if identity(&opened) != identity(&st) {
            return Err(self.abort(Anomaly::IdentityChanged {
                expected: identity(&st),
                found: identity(&opened),
            }));
        }
        // Same device is not enough: a bind mount of the same filesystem shares `st_dev`.
        if let Some(root_mount) = self.root_mount {
            match mount_id(self.tuning, fd.as_fd(), c"", name) {
                Some(found) if found == root_mount => {}
                Some(found) => {
                    return Err(self.abort(Anomaly::MountIdCrossing {
                        root_mount_id: root_mount,
                        found_mount_id: found,
                    }));
                }
                None => {
                    self.keep(
                        Some(kind),
                        SkipReason::Io,
                        "mount ID unavailable for this directory".into(),
                    );
                    return Ok(Step::Kept);
                }
            }
        }
        self.observe_current(|relative| WalkEvent::DirOpened { relative, depth });
        match list(&fd) {
            Ok(pending) => {
                self.observe_current(|relative| WalkEvent::DirListed { relative, depth });
                Ok(Step::Descend(Frame {
                    name: name.to_owned(),
                    id: identity(&opened),
                    parent_id,
                    fd: Some(fd),
                    pending,
                    incomplete: false,
                    since_verified: None,
                }))
            }
            Err(e) => {
                self.keep_errno(Some(kind), e);
                Ok(Step::Kept)
            }
        }
    }

    /// Removes the emptied directory `done` from `parent`, after checking the name still refers
    /// to it. Returns whether it was removed.
    fn remove_dir(&mut self, parent: BorrowedFd<'_>, done: &Frame) -> Result<bool, Abort> {
        let st = match retry(|| statat(parent, &done.name, AtFlags::SYMLINK_NOFOLLOW)) {
            Ok(st) => st,
            // We held it a moment ago: it was moved (possibly out of the candidate) or removed
            // behind our back. Stop.
            Err(Errno::NOENT) => return Err(self.abort(Anomaly::DirectoryMoved)),
            Err(e) => {
                self.keep_errno(Some(EntryKind::Dir), e);
                return Ok(false);
            }
        };
        if identity(&st) != done.id {
            return Err(self.abort(Anomaly::IdentityChanged {
                expected: done.id,
                found: identity(&st),
            }));
        }
        match retry(|| unlinkat(parent, &done.name, AtFlags::REMOVEDIR)) {
            Ok(()) => {
                self.report.removed.dirs += 1;
                Ok(true)
            }
            Err(Errno::NOENT) => Err(self.abort(Anomaly::DirectoryMoved)),
            Err(Errno::NOTDIR) => Err(self.abort(Anomaly::KindChanged {
                expected: EntryKind::Dir,
                found: None,
            })),
            Err(e) => {
                self.keep_errno(Some(EntryKind::Dir), e);
                Ok(false)
            }
        }
    }
}

/// Re-opens the parent of the directory behind `child` through `..` and checks it is still the
/// directory recorded as `expected`. `..` of a held directory is its current parent, so a match
/// proves the tree above was not rearranged.
fn reopen_parent(child: &OwnedFd, expected: FileIdentity) -> Option<OwnedFd> {
    let fd = retry(|| openat(child, c"..", dir_flags(), Mode::empty())).ok()?;
    let st = retry(|| fstat(&fd)).ok()?;
    (identity(&st) == expected).then_some(fd)
}

pub(super) fn remove_tree(
    req: &RemoveRequest<'_>,
    observer: Option<&mut dyn FnMut(&WalkEvent<'_>)>,
    tuning: &Tuning,
) -> Result<RemoveReport, RemoveError> {
    let names = plain_components(req.relative)?
        .into_iter()
        .map(|n| CString::new(n.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RemoveError::InvalidRequest {
            reason: "a path component contains a NUL byte",
        })?;
    let Some((target, intermediate)) = names.split_last() else {
        return Err(RemoveError::InvalidRequest {
            reason: "the relative path is empty",
        });
    };

    // 1. The anchor (trusted, opened as given), then every component below it no-follow.
    let anchor_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOCTTY | OFlags::CLOEXEC;
    let mut parent = retry(|| open(req.anchor, anchor_flags, Mode::empty())).map_err(|e| {
        RemoveError::AnchorUnavailable {
            anchor: req.anchor.to_path_buf(),
            source: io_err(e),
        }
    })?;
    if let Some(expected) = req.anchor_identity {
        let st = retry(|| fstat(&parent)).map_err(|e| RemoveError::AnchorUnavailable {
            anchor: req.anchor.to_path_buf(),
            source: io_err(e),
        })?;
        if identity(&st) != expected {
            return Err(RemoveError::AnchorChanged {
                expected,
                found: identity(&st),
            });
        }
    }
    let mut walked = PathBuf::new();
    for name in intermediate {
        walked.push(OsStr::from_bytes(name.as_bytes()));
        parent = retry(|| openat(&parent, name.as_c_str(), dir_flags(), Mode::empty())).map_err(
            |e| RemoveError::PathNotTraversable {
                component: walked.clone(),
                source: io_err(e),
            },
        )?;
    }

    // 2. The target: no-follow stat, identity and kind (SAFE-05).
    let st = match retry(|| statat(&parent, target.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)) {
        Ok(st) => st,
        Err(Errno::NOENT) => return Err(RemoveError::TargetMissing),
        Err(e) => return Err(RemoveError::TargetUnavailable { source: io_err(e) }),
    };
    if identity(&st) != req.expected {
        return Err(RemoveError::IdentityMismatch {
            expected: req.expected,
            found: identity(&st),
        });
    }
    // A target on another device than the directory holding it is itself a mount point (or a
    // Btrfs subvolume): never emptied (PRD §8.3: crossing a mount boundary is S3).
    let parent_st = retry(|| fstat(&parent))
        .map_err(|e| RemoveError::TargetUnavailable { source: io_err(e) })?;
    let target_device = device_of(tuning, &st, target);
    // Linux: a bind mount of the same filesystem keeps `st_dev`; compare mount IDs too.
    let parent_mount = mount_id(tuning, parent.as_fd(), c"", c"");
    let target_mount = mount_id(tuning, parent.as_fd(), target, target);
    let mounts_differ = matches!((parent_mount, target_mount), (Some(p), Some(t)) if p != t);
    if device(&parent_st) != target_device || mounts_differ {
        return Err(RemoveError::TargetIsMountPoint {
            parent_device: device(&parent_st),
            target_device,
            parent_mount_id: parent_mount,
            target_mount_id: target_mount,
        });
    }
    let root_kind = kind(&st);
    if let Some(expected) = req.expected_kind
        && expected != root_kind
    {
        return Err(RemoveError::KindMismatch {
            expected,
            found: root_kind,
        });
    }
    // A single non-directory with other hard links: removing this name frees nothing (another
    // link keeps the data) and the other name may be the user's. Refused.
    let nlink = st.st_nlink as u64;
    if root_kind != EntryKind::Dir && nlink > 1 {
        return Err(RemoveError::HardLinked { nlink });
    }

    let mut walk = Walk {
        report: RemoveReport::new(),
        rel: PathBuf::new(),
        root_device: device(&st),
        root_mount: target_mount.filter(|_| parent_mount.is_some()),
        observer,
        tuning,
    };
    walk.report.mount_ids_checked = walk.root_mount.is_some();
    walk.observe(WalkEvent::EntryChecked {
        relative: &PathBuf::new(),
        kind: root_kind,
    });

    // A single non-directory: unlink it (a symlink target is removed as the link only).
    if root_kind != EntryKind::Dir {
        // Re-check right before the unlink: POSIX unlinks by name, not by identity, so this
        // narrows (but cannot close) the window in which the name could be re-pointed.
        let now = retry(|| statat(&parent, target.as_c_str(), AtFlags::SYMLINK_NOFOLLOW));
        match now {
            Ok(now) if identity(&now) == req.expected => {}
            Ok(now) => {
                return Err(RemoveError::TargetChanged {
                    anomaly: Anomaly::IdentityChanged {
                        expected: req.expected,
                        found: identity(&now),
                    },
                });
            }
            Err(Errno::NOENT) => return Err(RemoveError::TargetMissing),
            Err(e) => return Err(RemoveError::TargetUnavailable { source: io_err(e) }),
        }
        match retry(|| unlinkat(&parent, target.as_c_str(), AtFlags::empty())) {
            Ok(()) => {
                walk.count_removed(root_kind, &st);
                walk.report.outcome = RemoveOutcome::Complete;
            }
            Err(Errno::NOENT) => walk.report.vanished += 1,
            Err(Errno::ISDIR) => {
                return Err(RemoveError::TargetChanged {
                    anomaly: Anomaly::KindChanged {
                        expected: root_kind,
                        found: Some(EntryKind::Dir),
                    },
                });
            }
            Err(e) => walk.keep_errno(Some(root_kind), e),
        }
        return Ok(walk.report);
    }

    // 3. A directory: open no-follow and verify the handle, then walk.
    let root_fd = match retry(|| openat(&parent, target.as_c_str(), dir_flags(), Mode::empty())) {
        Ok(fd) => fd,
        Err(Errno::LOOP | Errno::MLINK | Errno::NOTDIR) => {
            return Err(RemoveError::TargetChanged {
                anomaly: Anomaly::KindChanged {
                    expected: EntryKind::Dir,
                    found: None,
                },
            });
        }
        Err(Errno::NOENT) => return Err(RemoveError::TargetMissing),
        Err(e) => return Err(RemoveError::TargetUnavailable { source: io_err(e) }),
    };
    let opened = retry(|| fstat(&root_fd))
        .map_err(|e| RemoveError::TargetUnavailable { source: io_err(e) })?;
    if identity(&opened) != req.expected {
        return Err(RemoveError::TargetChanged {
            anomaly: Anomaly::IdentityChanged {
                expected: req.expected,
                found: identity(&opened),
            },
        });
    }
    walk.observe(WalkEvent::DirOpened {
        relative: &PathBuf::new(),
        depth: 0,
    });
    let pending =
        list(&root_fd).map_err(|e| RemoveError::TargetUnavailable { source: io_err(e) })?;
    walk.observe(WalkEvent::DirListed {
        relative: &PathBuf::new(),
        depth: 0,
    });

    let frames = vec![Frame {
        name: target.clone(),
        id: req.expected,
        parent_id: identity(&parent_st),
        fd: Some(root_fd),
        pending,
        incomplete: false,
        since_verified: None,
    }];
    if let Err((relative, anomaly)) = run(&mut walk, frames, parent.as_fd()) {
        walk.report.outcome = RemoveOutcome::Aborted;
        walk.report.aborted = Some(AbortReason { relative, anomaly });
    }
    Ok(walk.report)
}

/// The iterative walk. `frames[0]` is the target; `target_parent` holds it.
fn run(
    walk: &mut Walk<'_, '_>,
    mut frames: Vec<Frame>,
    target_parent: BorrowedFd<'_>,
) -> Result<(), Abort> {
    let budget = walk.tuning.fd_budget.max(1);
    let verify_every = walk.tuning.verify_every.max(1);
    // Frames at index >= first_open have their handle open; those below were closed to stay
    // within the budget (closed from the shallow end, re-opened from the deep end).
    let mut first_open = 0usize;

    loop {
        let depth = frames.len();
        let Some(top) = frames.last_mut() else {
            return Ok(());
        };

        if let Some(listed) = top.pending.pop() {
            let Some(dirfd) = top.fd.as_ref() else {
                // Unreachable by construction (the top frame is always open); stop rather than
                // guess.
                return Err(walk.abort(Anomaly::DirectoryMoved));
            };
            // Before the first removal in a directory, after coming back from a sub-directory,
            // and every `verify_every` entries: is it still where it was, inside the candidate?
            // (POSIX removes by name; a held directory moved elsewhere would otherwise keep
            // being emptied at its new location.)
            let due = top.since_verified.is_none_or(|n| n >= verify_every);
            if due {
                if !still_in_place(dirfd, &top.name, top.id, top.parent_id) {
                    return Err(walk.abort(Anomaly::DirectoryMoved));
                }
                top.since_verified = Some(0);
            }
            walk.rel.push(OsStr::from_bytes(listed.0.as_bytes()));
            let step = walk.entry(dirfd.as_fd(), top.id, &listed, depth)?;
            top.since_verified = top.since_verified.map(|n| n + 1);
            match step {
                Step::Descend(frame) => {
                    // `walk.rel` keeps the directory's name until it is popped.
                    frames.push(frame);
                    while frames.len() - first_open > budget {
                        frames[first_open].fd = None;
                        first_open += 1;
                    }
                }
                Step::Removed => {
                    walk.rel.pop();
                }
                Step::Vanished => {
                    walk.report.vanished += 1;
                    walk.rel.pop();
                }
                Step::Kept => {
                    top.incomplete = true;
                    walk.rel.pop();
                }
            }
            continue;
        }

        // The top directory is processed: remove it from its parent.
        let Some(done) = frames.pop() else {
            return Ok(());
        };
        let parent_index = frames.len().saturating_sub(1);
        let removed = match frames.last_mut() {
            Some(parent) => {
                if parent.fd.is_none() {
                    let Some(child) = done.fd.as_ref() else {
                        return Err(walk.abort(Anomaly::DirectoryMoved));
                    };
                    match reopen_parent(child, parent.id) {
                        Some(fd) => parent.fd = Some(fd),
                        None => return Err(walk.abort(Anomaly::DirectoryMoved)),
                    }
                    first_open = parent_index;
                }
                let Some(parent_fd) = parent.fd.as_ref() else {
                    return Err(walk.abort(Anomaly::DirectoryMoved));
                };
                let parent_fd = parent_fd.as_fd();
                if done.incomplete {
                    false
                } else {
                    walk.remove_dir(parent_fd, &done)?
                }
            }
            None => {
                if done.incomplete {
                    false
                } else {
                    walk.remove_dir(target_parent, &done)?
                }
            }
        };
        drop(done);
        match frames.last_mut() {
            Some(parent) => {
                if !removed {
                    parent.incomplete = true;
                }
                // Resuming the parent: re-verify its place before its next removal.
                parent.since_verified = None;
                walk.rel.pop();
            }
            None => {
                walk.report.outcome = if removed {
                    RemoveOutcome::Complete
                } else {
                    RemoveOutcome::Incomplete
                };
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
