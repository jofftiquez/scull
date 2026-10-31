//! Linux (freedesktop.org Trash spec 1.0) and macOS (`~/.Trash`, `.Trashes/<uid>`): an
//! identity-checked, fd-relative, no-replace `rename` into a trash directory on the item's own
//! volume. See the parent module for the contract.

// `geteuid` and `localtime_r` (libc) are the only unsafe calls; each has a SAFETY comment.
#![allow(unsafe_code)]
// `stat` field widths differ per OS and architecture; the casts mirror
// `std::os::unix::fs::MetadataExt`, so identities match `Platform::metadata`.
#![allow(clippy::unnecessary_cast)]

use super::format::{self, LocalTime, MAX_NAME_ATTEMPTS};
use super::{
    MovedSize, TrashError, TrashKind, TrashLocations, TrashOutcome, TrashRequest, measure,
    plain_components,
};
use crate::EntryKind;
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, RenameFlags, Stat, fstat, fsync, mkdirat, open, openat,
    renameat_with, statat, unlinkat,
};
use rustix::io::Errno;
use scull_core::{FileIdentity, Restorability};
use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Ancestors examined when looking for the volume's top directory.
const MAX_CLIMB: usize = 4096;

/// The sticky bit (`S_ISVTX`).
const STICKY: u32 = 0o1000;

fn retry<T>(mut f: impl FnMut() -> rustix::io::Result<T>) -> rustix::io::Result<T> {
    loop {
        match f() {
            Err(Errno::INTR) => continue,
            other => return other,
        }
    }
}

fn io_err(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

fn device(st: &Stat) -> u64 {
    st.st_dev as u64
}

fn identity(st: &Stat) -> FileIdentity {
    FileIdentity::new(device(st), u128::from(st.st_ino as u64))
}

fn mode(st: &Stat) -> u32 {
    st.st_mode as u32
}

fn kind(st: &Stat) -> EntryKind {
    match FileType::from_raw_mode(st.st_mode as _) {
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

fn euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, cannot fail and has no side effects (POSIX).
    unsafe { libc::geteuid() }
}

/// Flags for every directory opened below a trusted starting point: never follow a link at
/// the final component, fail on non-directories, never block, never leak into children.
fn dir_flags() -> OFlags {
    OFlags::RDONLY
        | OFlags::DIRECTORY
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK
        | OFlags::NOCTTY
        | OFlags::CLOEXEC
}

/// Flags for a trusted starting point given by path (anchor, home trash parent).
fn trusted_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOCTTY | OFlags::CLOEXEC
}

/// Flags for walking up through `..`. Linux: `O_PATH` needs no read permission on ancestors
/// and still works as the directory of `*at` calls.
fn climb_flags() -> OFlags {
    #[cfg(target_os = "linux")]
    return OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    return dir_flags();
}

/// The mount ID (`statx` `STATX_MNT_ID`) of the directory behind `fd`, where the kernel
/// reports one: a bind mount shares `st_dev` with its source.
#[cfg(target_os = "linux")]
fn mount_id(fd: BorrowedFd<'_>) -> Option<u64> {
    use rustix::fs::{StatxFlags, statx};
    let stx = retry(|| {
        statx(
            fd,
            c"",
            AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW,
            StatxFlags::MNT_ID,
        )
    })
    .ok()?;
    (stx.stx_mask & StatxFlags::MNT_ID.bits() != 0).then_some(stx.stx_mnt_id)
}

#[cfg(not(target_os = "linux"))]
fn mount_id(_fd: BorrowedFd<'_>) -> Option<u64> {
    None
}

fn mounts_differ(a: Option<u64>, b: Option<u64>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a != b)
}

fn cstring(bytes: &[u8]) -> Result<CString, TrashError> {
    CString::new(bytes).map_err(|_| TrashError::InvalidRequest {
        reason: "a path component contains a NUL byte",
    })
}

fn unavailable(reason: impl Into<String>) -> TrashError {
    TrashError::Unavailable {
        reason: reason.into(),
    }
}

/// The current local time (`localtime_r`), or UTC if the conversion fails.
fn local_now() -> LocalTime {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let t = secs as libc::time_t;
    // SAFETY: `libc::tm` holds integers and (on some OSs) a `*const c_char` zone name; all-zero
    // is a valid value for every field (null pointer).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` and `tm` are valid, live, properly aligned values for the duration of the
    // call; `localtime_r` is the thread-safe variant and writes only into `tm`.
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return format::utc_time(secs);
    }
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1).clamp(1, 12) as u32,
        day: tm.tm_mday.clamp(1, 31) as u32,
        hour: tm.tm_hour.clamp(0, 23) as u32,
        minute: tm.tm_min.clamp(0, 59) as u32,
        second: tm.tm_sec.clamp(0, 60) as u32,
    }
}

/// A trash directory, opened and checked.
struct TrashDir {
    kind: TrashKind,
    /// The trash directory (display).
    path: PathBuf,
    /// Where items go: Linux `files/`, macOS the trash directory itself.
    files: OwnedFd,
    files_path: PathBuf,
    /// Linux `info/`.
    info: Option<(OwnedFd, PathBuf)>,
    /// For a per-volume trash: the top directory the `.trashinfo` `Path` is relative to.
    topdir: Option<PathBuf>,
}

/// Checks an existing trash directory: owned by the current user and not writable by others.
fn check_private(st: &Stat, shown: &Path) -> Result<(), String> {
    if st.st_uid as u32 != euid() {
        return Err(format!(
            "{} is owned by another user; not used",
            shown.display()
        ));
    }
    if mode(st) & 0o022 != 0 {
        return Err(format!(
            "{} is writable by other users; not used",
            shown.display()
        ));
    }
    Ok(())
}

/// Opens the directory `name` in `parent`, creating it owner-only (`0700`) when missing. It
/// must be a real directory (never a link), owned by the current user, not writable by others.
fn private_dir(parent: BorrowedFd<'_>, name: &CStr, shown: &Path) -> Result<OwnedFd, String> {
    match retry(|| mkdirat(parent, name, Mode::from_raw_mode(0o700))) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(format!("cannot create {}: {}", shown.display(), io_err(e))),
    }
    let fd = retry(|| openat(parent, name, dir_flags(), Mode::empty())).map_err(|e| {
        format!(
            "cannot open {} as a real directory: {}",
            shown.display(),
            io_err(e)
        )
    })?;
    let st = retry(|| fstat(&fd)).map_err(|e| format!("{}: {}", shown.display(), io_err(e)))?;
    check_private(&st, shown)?;
    Ok(fd)
}

/// Linux: `files/` and `info/` of a trash directory.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn spec_layout(
    trash: OwnedFd,
    path: PathBuf,
    kind: TrashKind,
    topdir: Option<PathBuf>,
) -> Result<TrashDir, String> {
    let files_path = path.join("files");
    let info_path = path.join("info");
    let files = private_dir(trash.as_fd(), c"files", &files_path)?;
    let info = private_dir(trash.as_fd(), c"info", &info_path)?;
    Ok(TrashDir {
        kind,
        path,
        files,
        files_path,
        info: Some((info, info_path)),
        topdir,
    })
}

/// The home trash, if the item is on its volume. `Ok(None)`: no home trash (its parent does
/// not exist) or another volume, so a per-volume trash applies.
fn home_trash(
    home: &Path,
    item_dev: u64,
    item_mount: Option<u64>,
) -> Result<Option<TrashDir>, TrashError> {
    let (Some(parent_path), Some(name)) = (home.parent(), home.file_name()) else {
        return Ok(None);
    };
    let Ok(parent) = retry(|| open(parent_path, trusted_flags(), Mode::empty())) else {
        return Ok(None);
    };
    match retry(|| fstat(&parent)) {
        Ok(st) if device(&st) == item_dev => {}
        _ => return Ok(None),
    }
    let name = cstring(name.as_bytes())?;
    let trash = private_dir(parent.as_fd(), &name, home).map_err(|r| {
        if cfg!(target_os = "macos") {
            unavailable(format!(
                "{r} (macOS may require Full Disk Access to use ~/.Trash)"
            ))
        } else {
            unavailable(r)
        }
    })?;
    #[cfg(target_os = "macos")]
    let dir = TrashDir {
        kind: TrashKind::Home,
        path: home.to_path_buf(),
        files: trash,
        files_path: home.to_path_buf(),
        info: None,
        topdir: None,
    };
    #[cfg(not(target_os = "macos"))]
    let dir = spec_layout(trash, home.to_path_buf(), TrashKind::Home, None).map_err(unavailable)?;
    // The trash may itself be a separate mount: then it is not on the item's volume.
    match retry(|| fstat(&dir.files)) {
        Ok(st)
            if device(&st) == item_dev
                && !mounts_differ(item_mount, mount_id(dir.files.as_fd())) =>
        {
            Ok(Some(dir))
        }
        _ => Ok(None),
    }
}

/// The top directory of the item's volume, opened by walking up from the item's parent, so it
/// is verified to be an ancestor on the same volume (and, on Linux, the same mount).
fn volume_top(
    locations: &TrashLocations,
    parent: &OwnedFd,
    abs: &Path,
    item_dev: u64,
) -> Result<(OwnedFd, PathBuf), TrashError> {
    let top_path = match &locations.topdir {
        Some(t) => t.clone(),
        None => {
            crate::volume_of(abs)
                .map_err(|e| unavailable(format!("cannot find the item's volume: {e}")))?
                .mount_point
        }
    };
    let top_st = retry(|| rustix::fs::stat(&top_path))
        .map_err(|e| unavailable(format!("{}: {}", top_path.display(), io_err(e))))?;
    if device(&top_st) != item_dev {
        return Err(TrashError::CrossVolume {
            item_volume: item_dev,
            trash_volume: device(&top_st),
        });
    }
    let top_id = identity(&top_st);
    let item_mount = mount_id(parent.as_fd());
    let mut cur = retry(|| openat(parent, c".", climb_flags(), Mode::empty()))
        .map_err(|e| TrashError::TargetUnavailable { source: io_err(e) })?;
    for _ in 0..MAX_CLIMB {
        let Ok(st) = retry(|| fstat(&cur)) else { break };
        if device(&st) != item_dev || mounts_differ(item_mount, mount_id(cur.as_fd())) {
            break;
        }
        if identity(&st) == top_id {
            return Ok((cur, top_path));
        }
        let Ok(up) = retry(|| openat(&cur, c"..", climb_flags(), Mode::empty())) else {
            break;
        };
        match retry(|| fstat(&up)) {
            Ok(up_st) if identity(&up_st) != identity(&st) => cur = up,
            _ => break, // the filesystem root
        }
    }
    Err(unavailable(format!(
        "{} is not an ancestor of the item on the item's volume",
        top_path.display()
    )))
}

/// Linux: `$topdir/.Trash/$uid` when `$topdir/.Trash` is a real directory with the sticky bit,
/// else `$topdir/.Trash-$uid` (freedesktop Trash spec, "Trash directories").
#[cfg(not(target_os = "macos"))]
fn volume_trash(top: OwnedFd, top_path: PathBuf) -> Result<TrashDir, TrashError> {
    let uid = euid().to_string();
    let uid_c = cstring(uid.as_bytes())?;
    let mut reasons = Vec::new();
    let shared_path = top_path.join(".Trash");
    match retry(|| statat(&top, c".Trash", AtFlags::SYMLINK_NOFOLLOW)) {
        Ok(st) if kind(&st) == EntryKind::Dir && mode(&st) & STICKY != 0 => {
            let opened = retry(|| openat(&top, c".Trash", dir_flags(), Mode::empty()))
                .ok()
                .filter(|fd| retry(|| fstat(fd)).is_ok_and(|o| identity(&o) == identity(&st)));
            match opened {
                Some(shared) => {
                    let user_path = shared_path.join(&uid);
                    match private_dir(shared.as_fd(), &uid_c, &user_path).and_then(|fd| {
                        spec_layout(
                            fd,
                            user_path,
                            TrashKind::VolumeShared,
                            Some(top_path.clone()),
                        )
                    }) {
                        Ok(dir) => return Ok(dir),
                        Err(r) => reasons.push(r),
                    }
                }
                None => reasons.push(format!(
                    "{} changed while it was checked; not used",
                    shared_path.display()
                )),
            }
        }
        Ok(st) if kind(&st) == EntryKind::Symlink => reasons.push(format!(
            "{} is a symbolic link; not used",
            shared_path.display()
        )),
        Ok(_) => reasons.push(format!(
            "{} is not a directory with the sticky bit set; not used",
            shared_path.display()
        )),
        Err(Errno::NOENT) => {}
        Err(e) => reasons.push(format!("{}: {}", shared_path.display(), io_err(e))),
    }
    let name = format!(".Trash-{uid}");
    let user_path = top_path.join(&name);
    let name_c = cstring(name.as_bytes())?;
    private_dir(top.as_fd(), &name_c, &user_path)
        .and_then(|fd| spec_layout(fd, user_path, TrashKind::VolumeUser, Some(top_path)))
        .map_err(|r| {
            reasons.push(r);
            unavailable(reasons.join("; "))
        })
}

/// macOS: `<volume>/.Trashes/<uid>`. `.Trashes` must be a real directory with the sticky bit;
/// it is created (`d-wx-wx-wt`, as Finder does) when missing.
#[cfg(target_os = "macos")]
fn volume_trash(top: OwnedFd, top_path: PathBuf) -> Result<TrashDir, TrashError> {
    let shared_path = top_path.join(".Trashes");
    let examine = || retry(|| statat(&top, c".Trashes", AtFlags::SYMLINK_NOFOLLOW));
    let st = match examine() {
        Ok(st) => st,
        Err(Errno::NOENT) => {
            create_trashes(&top, &shared_path)?;
            examine()
                .map_err(|e| unavailable(format!("{}: {}", shared_path.display(), io_err(e))))?
        }
        Err(e) => {
            return Err(unavailable(format!(
                "{}: {}",
                shared_path.display(),
                io_err(e)
            )));
        }
    };
    if kind(&st) != EntryKind::Dir || mode(&st) & STICKY == 0 {
        return Err(unavailable(format!(
            "{} is not a real directory with the sticky bit set; not used",
            shared_path.display()
        )));
    }
    let uid = euid().to_string();
    let user_path = shared_path.join(&uid);
    // `.Trashes` is normally not readable (mode 1333), so it is never opened: the user's
    // directory is reached through it, opened no-follow, and its `..` must be the `.Trashes`
    // examined above.
    let rel = cstring(format!(".Trashes/{uid}").as_bytes())?;
    match retry(|| mkdirat(&top, rel.as_c_str(), Mode::from_raw_mode(0o700))) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => {
            return Err(unavailable(format!(
                "cannot create {}: {}",
                user_path.display(),
                io_err(e)
            )));
        }
    }
    let fd = retry(|| openat(&top, rel.as_c_str(), dir_flags(), Mode::empty())).map_err(|e| {
        unavailable(format!(
            "cannot open {} as a real directory: {}",
            user_path.display(),
            io_err(e)
        ))
    })?;
    let fd_st = retry(|| fstat(&fd))
        .map_err(|e| unavailable(format!("{}: {}", user_path.display(), io_err(e))))?;
    check_private(&fd_st, &user_path).map_err(unavailable)?;
    match retry(|| statat(&fd, c"..", AtFlags::SYMLINK_NOFOLLOW)) {
        Ok(up) if identity(&up) == identity(&st) => {}
        _ => {
            return Err(unavailable(format!(
                "{} changed while it was checked; not used",
                shared_path.display()
            )));
        }
    }
    Ok(TrashDir {
        kind: TrashKind::VolumeShared,
        path: user_path.clone(),
        files: fd,
        files_path: user_path,
        info: None,
        topdir: None,
    })
}

#[cfg(target_os = "macos")]
fn create_trashes(top: &OwnedFd, shown: &Path) -> Result<(), TrashError> {
    use rustix::fs::fchmod;
    match retry(|| mkdirat(top, c".Trashes", Mode::from_raw_mode(0o700))) {
        Ok(()) => {}
        Err(Errno::EXIST) => return Ok(()),
        Err(e) => {
            return Err(unavailable(format!(
                "cannot create {}: {}",
                shown.display(),
                io_err(e)
            )));
        }
    }
    let fd = retry(|| openat(top, c".Trashes", dir_flags(), Mode::empty()))
        .map_err(|e| unavailable(format!("{}: {}", shown.display(), io_err(e))))?;
    retry(|| fchmod(&fd, Mode::from_raw_mode(0o1333)))
        .map_err(|e| unavailable(format!("{}: {}", shown.display(), io_err(e))))
}

/// Chooses the trash directory for an item on device `item_dev`.
fn select_trash(
    locations: &TrashLocations,
    parent: &OwnedFd,
    abs: &Path,
    item_dev: u64,
) -> Result<TrashDir, TrashError> {
    if let Some(home) = &locations.home
        && let Some(dir) = home_trash(home, item_dev, mount_id(parent.as_fd()))?
    {
        return Ok(dir);
    }
    let (top, top_path) = volume_top(locations, parent, abs, item_dev)?;
    volume_trash(top, top_path)
}

/// The no-follow stat of the item, checked against the request (identity, kind, not a mount
/// point).
fn check_target(
    parent: &OwnedFd,
    target: &CStr,
    req: &TrashRequest<'_>,
) -> Result<Stat, TrashError> {
    let st = match retry(|| statat(parent, target, AtFlags::SYMLINK_NOFOLLOW)) {
        Ok(st) => st,
        Err(Errno::NOENT) => return Err(TrashError::TargetMissing),
        Err(e) => return Err(TrashError::TargetUnavailable { source: io_err(e) }),
    };
    if identity(&st) != req.expected {
        return Err(TrashError::IdentityChanged {
            expected: req.expected,
            found: identity(&st),
        });
    }
    if let Some(expected) = req.expected_kind
        && expected != kind(&st)
    {
        return Err(TrashError::KindMismatch {
            expected,
            found: kind(&st),
        });
    }
    let parent_st =
        retry(|| fstat(parent)).map_err(|e| TrashError::TargetUnavailable { source: io_err(e) })?;
    if device(&parent_st) != device(&st) {
        return Err(TrashError::TargetIsMountPoint);
    }
    if kind(&st) == EntryKind::Dir
        && let Ok(fd) = retry(|| {
            openat(
                parent,
                target,
                climb_flags() | OFlags::NOFOLLOW,
                Mode::empty(),
            )
        })
        && mounts_differ(mount_id(parent.as_fd()), mount_id(fd.as_fd()))
    {
        return Err(TrashError::TargetIsMountPoint);
    }
    Ok(st)
}

pub(super) fn move_to_trash(req: &TrashRequest<'_>) -> Result<TrashOutcome, TrashError> {
    let names = plain_components(req.relative)?
        .into_iter()
        .map(|n| cstring(n.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    let Some((target, intermediate)) = names.split_last() else {
        return Err(TrashError::InvalidRequest {
            reason: "the relative path is empty",
        });
    };

    // 1. The anchor (trusted, opened as given), then every component below it no-follow.
    let mut parent = retry(|| open(req.anchor, trusted_flags(), Mode::empty())).map_err(|e| {
        TrashError::AnchorUnavailable {
            anchor: req.anchor.to_path_buf(),
            source: io_err(e),
        }
    })?;
    if let Some(expected) = req.anchor_identity {
        let st = retry(|| fstat(&parent)).map_err(|e| TrashError::AnchorUnavailable {
            anchor: req.anchor.to_path_buf(),
            source: io_err(e),
        })?;
        if identity(&st) != expected {
            return Err(TrashError::AnchorChanged {
                expected,
                found: identity(&st),
            });
        }
    }
    let mut walked = PathBuf::new();
    for name in intermediate {
        walked.push(OsStr::from_bytes(name.as_bytes()));
        parent = retry(|| openat(&parent, name.as_c_str(), dir_flags(), Mode::empty())).map_err(
            |e| TrashError::PathNotTraversable {
                component: walked.clone(),
                source: io_err(e),
            },
        )?;
    }

    // 2. The item: identity, kind, not a mount point (SAFE-05).
    let st = check_target(&parent, target, req)?;
    let item_dev = device(&st);
    let abs = req.anchor.join(req.relative);

    // 3. What will be moved (advisory size; the walk must see the same object).
    let (seen, moved) = measure(&abs).map_err(|source| TrashError::TargetUnavailable { source })?;
    if seen != Some(req.expected) {
        return Err(TrashError::IdentityChanged {
            expected: req.expected,
            found: seen.unwrap_or(FileIdentity::new(0, 0)),
        });
    }

    // 4. A trash directory on the item's volume.
    let trash = select_trash(req.locations, &parent, &abs, item_dev)?;

    // 5. Record, then no-replace rename.
    move_into(&parent, target, req.expected, item_dev, &abs, trash, moved)
}

/// Writes `contents` to a new file `name` in `dir`, created exclusively (`O_EXCL`, never
/// following a link). `Ok(false)`: the name is taken.
fn write_new(dir: &OwnedFd, name: &CStr, contents: &[u8]) -> io::Result<bool> {
    let flags = OFlags::WRONLY
        | OFlags::CREATE
        | OFlags::EXCL
        | OFlags::NOFOLLOW
        | OFlags::NOCTTY
        | OFlags::CLOEXEC;
    let fd = match retry(|| openat(dir, name, flags, Mode::from_raw_mode(0o600))) {
        Ok(fd) => fd,
        Err(Errno::EXIST) => return Ok(false),
        Err(e) => return Err(io_err(e)),
    };
    let mut file = File::from(fd);
    let written = file.write_all(contents).and_then(|()| file.sync_all());
    if let Err(e) = written {
        let _ = retry(|| unlinkat(dir, name, AtFlags::empty()));
        return Err(e);
    }
    Ok(true)
}

fn move_into(
    parent: &OwnedFd,
    target: &CStr,
    expected: FileIdentity,
    item_dev: u64,
    abs: &Path,
    trash: TrashDir,
    moved: MovedSize,
) -> Result<TrashOutcome, TrashError> {
    let trash_dev = retry(|| fstat(&trash.files))
        .map(|st| device(&st))
        .map_err(|e| unavailable(format!("{}: {}", trash.files_path.display(), io_err(e))))?;
    if trash_dev != item_dev {
        return Err(TrashError::CrossVolume {
            item_volume: item_dev,
            trash_volume: trash_dev,
        });
    }
    let original: &Path = trash
        .topdir
        .as_deref()
        .and_then(|top| abs.strip_prefix(top).ok())
        .filter(|rel| !rel.as_os_str().is_empty())
        .unwrap_or(abs);
    let record = format::trash_info(original.as_os_str().as_bytes(), local_now());
    let sep = if cfg!(target_os = "macos") {
        b' '
    } else {
        b'.'
    };

    for n in 1..=MAX_NAME_ATTEMPTS {
        let name = cstring(&format::numbered_name(target.to_bytes(), n, sep))?;

        // 1. The metadata record, exclusively (freedesktop: "atomically, O_EXCL").
        let info_name = match &trash.info {
            Some((info_fd, info_path)) => {
                let mut bytes = name.as_bytes().to_vec();
                bytes.extend_from_slice(b".trashinfo");
                let info_name = cstring(&bytes)?;
                match write_new(info_fd, &info_name, record.as_bytes()) {
                    Ok(true) => Some((info_fd, info_path, info_name)),
                    Ok(false) => continue,
                    Err(e) => {
                        return Err(unavailable(format!(
                            "cannot write a record in {}: {e}",
                            info_path.display()
                        )));
                    }
                }
            }
            None => None,
        };
        let drop_record = || {
            if let Some((fd, _, n)) = &info_name {
                let _ = retry(|| unlinkat(*fd, n.as_c_str(), AtFlags::empty()));
            }
        };

        // 2. Last identity check, then a rename that never replaces anything.
        match retry(|| statat(parent, target, AtFlags::SYMLINK_NOFOLLOW)) {
            Ok(now) if identity(&now) == expected => {}
            Ok(now) => {
                drop_record();
                return Err(TrashError::IdentityChanged {
                    expected,
                    found: identity(&now),
                });
            }
            Err(Errno::NOENT) => {
                drop_record();
                return Err(TrashError::TargetMissing);
            }
            Err(e) => {
                drop_record();
                return Err(TrashError::TargetUnavailable { source: io_err(e) });
            }
        }
        let renamed = retry(|| {
            renameat_with(
                parent,
                target,
                &trash.files,
                name.as_c_str(),
                RenameFlags::NOREPLACE,
            )
        });
        match renamed {
            Ok(()) => {}
            Err(Errno::EXIST) => {
                // Taken in `files/` (an orphan without a record): try the next name.
                drop_record();
                continue;
            }
            Err(Errno::XDEV) => {
                drop_record();
                return Err(TrashError::CrossVolume {
                    item_volume: item_dev,
                    trash_volume: trash_dev,
                });
            }
            Err(Errno::NOENT) => {
                drop_record();
                return Err(TrashError::TargetMissing);
            }
            Err(e @ (Errno::INVAL | Errno::NOSYS | Errno::NOTSUP)) => {
                drop_record();
                return Err(unavailable(format!(
                    "the no-replace rename was refused ({}): the filesystem may lack \
                     RENAME_NOREPLACE / RENAME_EXCL, or the Trash is inside the item",
                    io_err(e)
                )));
            }
            Err(e) => {
                drop_record();
                return Err(TrashError::MoveFailed { source: io_err(e) });
            }
        }

        // 3. The moved entry must be the planned object (it keeps its identity).
        match retry(|| statat(&trash.files, name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)) {
            Ok(now) if identity(&now) == expected => {}
            other => {
                let found = other.map_or(FileIdentity::new(0, 0), |s| identity(&s));
                let put_back = retry(|| {
                    renameat_with(
                        &trash.files,
                        name.as_c_str(),
                        parent,
                        target,
                        RenameFlags::NOREPLACE,
                    )
                })
                .is_ok();
                if put_back {
                    drop_record();
                }
                return Err(TrashError::ReplacedDuringMove { found, put_back });
            }
        }
        // Best effort durability of the two directory entries.
        let _ = fsync(&trash.files);
        let _ = fsync(parent);

        let shown = OsStr::from_bytes(name.as_bytes());
        return Ok(TrashOutcome {
            kind: trash.kind,
            trash_dir: trash.path.clone(),
            location: Some(trash.files_path.join(shown)),
            info_file: info_name
                .as_ref()
                .map(|(_, p, n)| p.join(OsStr::from_bytes(n.as_bytes()))),
            moved,
            restorable_via: Restorability::ViaTrash,
        });
    }
    Err(unavailable(format!(
        "no free name in {} after {MAX_NAME_ATTEMPTS} attempts",
        trash.files_path.display()
    )))
}
