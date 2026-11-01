//! Linux and macOS: `lstat` fields via `std::os::unix::fs::MetadataExt`.

use crate::{
    Capabilities, DirEntry, DirIter, EntryKind, Metadata, Operation, PlatformError, Result,
};
use scull_core::FileIdentity;
use std::borrow::Cow;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

/// `st_blocks` is in 512-byte units on Linux and macOS, regardless of the filesystem block
/// size.
const ST_BLOCK_UNIT: u64 = 512;

pub(super) fn capabilities() -> Capabilities {
    Capabilities {
        os: std::env::consts::OS,
        file_identity: true,
        allocated_size: true,
        link_count: true,
        mtime: true,
        no_follow_metadata: true,
        special_file_kinds: true,
        reparse_points: false,
        race_free_read_dir: false,
        elevation_detection: true,
    }
}

fn kind(ft: fs::FileType) -> EntryKind {
    if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else if ft.is_fifo() {
        EntryKind::Fifo
    } else if ft.is_socket() {
        EntryKind::Socket
    } else if ft.is_block_device() {
        EntryKind::BlockDevice
    } else if ft.is_char_device() {
        EntryKind::CharDevice
    } else {
        EntryKind::Other
    }
}

fn convert(md: &fs::Metadata) -> Metadata {
    Metadata {
        kind: kind(md.file_type()),
        identity: Some(FileIdentity::new(md.dev(), u128::from(md.ino()))),
        logical_size: md.size(),
        allocated_size: Some(md.blocks().saturating_mul(ST_BLOCK_UNIT)),
        nlink: Some(md.nlink()),
        mtime_unix: Some(md.mtime()),
        readonly: md.permissions().readonly(),
        file_attributes: None,
        reparse_tag: None,
        bsd_flags: bsd_flags(md),
    }
}

/// `st_flags` (macOS): `SF_DATALESS` and friends, read from the same `lstat`.
#[cfg(target_os = "macos")]
fn bsd_flags(md: &fs::Metadata) -> Option<u32> {
    use std::os::macos::fs::MetadataExt as _;
    Some(md.st_flags())
}

/// Linux has no BSD file flags in `stat`.
#[cfg(not(target_os = "macos"))]
fn bsd_flags(_md: &fs::Metadata) -> Option<u32> {
    None
}

/// `lstat`: the final component is never followed and special files are never opened.
pub(super) fn metadata(path: &Path) -> io::Result<Metadata> {
    fs::symlink_metadata(path).map(|md| convert(&md))
}

pub(super) fn read_dir(path: &Path) -> Result<DirIter<'static>> {
    // `std::fs::read_dir` follows a symlink at `path`, so check first and refuse links.
    let found = metadata(path)
        .map_err(|e| PlatformError::from_io(Operation::Metadata, path, e))?
        .kind;
    if found != EntryKind::Dir {
        return Err(PlatformError::not_a_directory(path, found));
    }
    let dir = path.to_path_buf();
    let rd = fs::read_dir(path).map_err(|e| PlatformError::from_io(Operation::ReadDir, path, e))?;
    Ok(Box::new(rd.map(move |entry| {
        let entry = entry.map_err(|e| PlatformError::from_io(Operation::ReadDirEntry, &dir, e))?;
        let entry_path = entry.path();
        // `DirEntry::file_type` uses `d_type` from `readdir` and falls back to `lstat`: never
        // follows, never opens.
        let kind = entry
            .file_type()
            .map(kind)
            .map_err(|e| PlatformError::from_io(Operation::ReadDirEntry, &entry_path, e))?;
        Ok(DirEntry {
            file_name: entry.file_name(),
            path: entry_path,
            kind,
            reparse_tag: None,
        })
    })))
}

pub(super) fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    Cow::Borrowed(path.as_os_str().as_bytes())
}

// ---------------------------------------------------------------------------------------------
// Owner-only entries (P1-PLAT-06)
// ---------------------------------------------------------------------------------------------

/// Owner-only directory mode.
const DIR_MODE: u32 = 0o700;
/// Owner-only file mode.
const FILE_MODE: u32 = 0o600;

/// The effective user ID of this process.
#[allow(unsafe_code)]
pub(super) fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, cannot fail and has no side effects (POSIX).
    unsafe { libc::geteuid() }
}

/// The account this process acts as, for binding plans (`crate::identity`): the effective
/// user ID as `uid:<n>`. Numeric, so renaming the account doesn't change it.
pub(super) fn current_user_id() -> io::Result<String> {
    Ok(format!("uid:{}", effective_uid()))
}

/// `mkdir(path, 0700)`: the mode is applied by the creating call; the umask can only narrow
/// it. Not recursive.
pub(super) fn create_dir_owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(DIR_MODE).create(path)
}

/// `open(path, O_RDWR | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600)`. `O_EXCL` with
/// `O_CREAT` already fails on any existing entry, including a symlink; `O_NOFOLLOW` says so
/// explicitly.
pub(super) fn create_file_owner_only(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(FILE_MODE)
        .open(path)
}

/// `open(path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)`, then `fstat` on the descriptor:
/// a symlink at `path` fails (`ELOOP`), anything but a regular file is refused, and the
/// identity comes from the opened descriptor (not from a second path lookup).
pub(super) fn open_file_no_follow(path: &Path) -> io::Result<(fs::File, Option<FileIdentity>)> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let md = f.metadata()?;
    if !md.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok((f, Some(FileIdentity::new(md.dev(), u128::from(md.ino())))))
}

/// `fchmod(fd, mode & 0700)` on a descriptor opened with `O_NOFOLLOW | O_NONBLOCK`: a symlink
/// fails to open instead of being followed, and nothing blocks. Only regular files and
/// directories are opened (checked with `lstat` first, SAFE-18), and the opened descriptor
/// must be the entry that was checked (same device and inode), so a swap in between is an
/// error. Permissions are only ever narrowed. Needs read permission on the entry.
pub(super) fn restrict_owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let checked = fs::symlink_metadata(path)?;
    let found = kind(checked.file_type());
    if found != EntryKind::File && found != EntryKind::Dir {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to restrict a {found}: only real files and directories are restricted"
            ),
        ));
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let md = file.metadata()?;
    if md.dev() != checked.dev() || md.ino() != checked.ino() {
        return Err(io::Error::other(
            "the entry changed while it was being restricted",
        ));
    }
    let mode = md.mode() & 0o7777;
    let narrowed = mode & 0o700; // owner bits only: drops group, other, setuid/setgid, sticky
    if narrowed != mode {
        file.set_permissions(fs::Permissions::from_mode(narrowed))?;
    }
    Ok(())
}

pub(super) fn owner_only_exposures(path: &Path) -> io::Result<Vec<crate::Exposure>> {
    use crate::Exposure;
    let md = fs::symlink_metadata(path)?;
    let mut exposures = Vec::new();
    let mode = md.mode() & 0o7777;
    if mode & 0o077 != 0 {
        exposures.push(Exposure::Mode { mode });
    }
    if md.uid() != effective_uid() {
        exposures.push(Exposure::Owner {
            owner: md.uid().to_string(),
        });
    }
    Ok(exposures)
}

/// Whether `path` carries an extended ACL that permission bits don't show: macOS ACLs. On Linux
/// a POSIX ACL's mask is reflected in the group bits, which [`owner_only_exposures`] checks.
#[cfg(target_os = "macos")]
pub(super) fn has_extended_acl(path: &Path) -> io::Result<bool> {
    macos::has_extended_acl(path)
}

/// See the macOS version.
#[cfg(not(target_os = "macos"))]
pub(super) fn has_extended_acl(path: &Path) -> io::Result<bool> {
    fs::symlink_metadata(path).map(|_| false)
}

/// Known folders are a Windows concept.
pub(super) fn known_folder(_source: crate::DirSource) -> Option<std::path::PathBuf> {
    None
}

/// Root is effective UID 0 (`sudo`, `su`, `doas`, setuid-root). Cannot fail.
pub(super) fn elevation() -> crate::Elevation {
    if effective_uid() == 0 {
        crate::Elevation::Elevated
    } else {
        crate::Elevation::NotElevated
    }
}

// ---------------------------------------------------------------------------------------------
// Volumes (P1-PLAT-03)
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(super) use linux::{free_space, volume_of, volumes};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::{free_space, volume_of, volumes};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod other_unix {
    //! Other Unixes are not supported targets; volume queries report "unsupported" so callers
    //! treat every boundary and size as unknown.
    use std::io;
    use std::path::Path;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "volume queries are implemented for Linux, macOS and Windows only",
        )
    }

    pub(crate) fn volumes() -> io::Result<Vec<crate::VolumeInfo>> {
        Err(unsupported())
    }

    pub(crate) fn volume_of(_path: &Path) -> io::Result<crate::VolumeInfo> {
        Err(unsupported())
    }

    pub(crate) fn free_space(_path: &Path) -> io::Result<u64> {
        Err(unsupported())
    }
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) use other_unix::{free_space, volume_of, volumes};
