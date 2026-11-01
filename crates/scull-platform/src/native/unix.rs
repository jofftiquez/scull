//! Linux and macOS: `lstat` fields via `std::os::unix::fs::MetadataExt`.

use crate::types::EntryAt;
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
use std::sync::Arc;

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
    let checked =
        metadata(path).map_err(|e| PlatformError::from_io(Operation::Metadata, path, e))?;
    if checked.kind != EntryKind::Dir {
        return Err(PlatformError::not_a_directory(path, checked.kind));
    }
    let dir = path.to_path_buf();
    let rd = fs::read_dir(path).map_err(|e| PlatformError::from_io(Operation::ReadDir, path, e))?;
    // The same directory, opened once more for `fstatat` of its entries ([`entry_metadata`]).
    // The listing owns it; entries hold it weakly, so it lives exactly as long as the listing.
    let stat_dir = checked.identity.and_then(|id| open_for_stat(path, id));
    // macOS: names, kinds and plain files' metadata in batches (#91).
    #[cfg(target_os = "macos")]
    if let Some((d, id)) = stat_dir.as_ref().zip(checked.identity)
        && let Some(it) = bulk::list(path, Arc::clone(d), id)
    {
        return Ok(it);
    }
    let at = stat_dir
        .as_ref()
        .zip(checked.identity)
        .map(|(d, id)| EntryAt {
            dir: Arc::downgrade(d),
            dir_id: id,
        });
    Ok(Box::new(rd.map(move |entry| {
        // Keeps the descriptor open while the listing is iterated.
        let _owner = &stat_dir;
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
            at: at.clone(),
            #[cfg(target_os = "macos")]
            listed: None,
        })
    })))
}

/// `path` opened `O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC` for `fstatat`, if it is
/// still the directory `expect` (checked on the descriptor). `None` (entries then use their
/// paths) when it can't be opened or was swapped since it was checked.
fn open_for_stat(path: &Path, expect: FileIdentity) -> Option<Arc<fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let md = file.metadata().ok()?;
    let same = FileIdentity::new(md.dev(), u128::from(md.ino())) == expect;
    same.then(|| Arc::new(file))
}

/// A listed entry's metadata: `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)` relative to its
/// listed directory while the listing is alive, else `lstat` of its path. Both give the same
/// values ([`convert_stat`] mirrors [`convert`]). On macOS a plain file's metadata may come
/// from the listing itself ([`bulk`]), as it was when the listing read it.
pub(super) fn entry_metadata(entry: &DirEntry) -> io::Result<Metadata> {
    #[cfg(target_os = "macos")]
    if let Some(md) = &entry.listed {
        return Ok(md.clone());
    }
    match entry.at.as_ref().and_then(|a| a.dir.upgrade()) {
        Some(dir) => fstatat_nofollow(&dir, &entry.file_name).map(|st| convert_stat(&st)),
        None => metadata(&entry.path),
    }
}

/// `fstatat(dir, name, AT_SYMLINK_NOFOLLOW)`: never follows a link at `name`, never opens it.
#[allow(unsafe_code)]
fn fstatat_nofollow(dir: &fs::File, name: &std::ffi::OsStr) -> io::Result<libc::stat> {
    use std::os::fd::AsRawFd;
    let c = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `dir` is an open directory descriptor for the duration of the call; `c` is a
    // NUL-terminated name that outlives it; `st` points to writable memory for one `stat`.
    let r = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstatat` returned 0, so it filled in the whole `stat`.
    Ok(unsafe { st.assume_init() })
}

/// [`convert`] for a raw `stat`, with the same conversions std's `MetadataExt` applies, so a
/// `fstatat` result equals the `lstat` one field for field.
#[allow(clippy::unnecessary_cast)] // field widths differ between Linux and macOS
fn convert_stat(st: &libc::stat) -> Metadata {
    Metadata {
        kind: kind_of_mode(st.st_mode),
        identity: Some(FileIdentity::new(
            st.st_dev as u64,
            u128::from(st.st_ino as u64),
        )),
        logical_size: st.st_size as u64,
        allocated_size: Some((st.st_blocks as u64).saturating_mul(ST_BLOCK_UNIT)),
        nlink: Some(st.st_nlink as u64),
        mtime_unix: Some(st.st_mtime as i64),
        readonly: (st.st_mode as u32) & 0o222 == 0,
        file_attributes: None,
        reparse_tag: None,
        bsd_flags: stat_bsd_flags(st),
    }
}

/// The entry kind encoded in a `st_mode`.
fn kind_of_mode(mode: libc::mode_t) -> EntryKind {
    let fmt = mode & libc::S_IFMT;
    if fmt == libc::S_IFLNK {
        EntryKind::Symlink
    } else if fmt == libc::S_IFDIR {
        EntryKind::Dir
    } else if fmt == libc::S_IFREG {
        EntryKind::File
    } else if fmt == libc::S_IFIFO {
        EntryKind::Fifo
    } else if fmt == libc::S_IFSOCK {
        EntryKind::Socket
    } else if fmt == libc::S_IFBLK {
        EntryKind::BlockDevice
    } else if fmt == libc::S_IFCHR {
        EntryKind::CharDevice
    } else {
        EntryKind::Other
    }
}

#[cfg(target_os = "macos")]
fn stat_bsd_flags(st: &libc::stat) -> Option<u32> {
    Some(st.st_flags)
}

#[cfg(not(target_os = "macos"))]
fn stat_bsd_flags(_st: &libc::stat) -> Option<u32> {
    None
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

/// `fchmod(fd, mode & 0700)` on a descriptor opened with `O_NOFOLLOW | O_NONBLOCK`: a symlink
/// fails to open instead of being followed, and nothing blocks. Only regular files and
/// directories are opened (checked with `lstat` first, SAFE-18), and the opened descriptor
/// must be the entry that was checked (same device and inode), so a swap in between is an
/// error. Permissions are only ever narrowed. Needs read permission on the entry.
///
/// An entry that is already owner-only is **not opened**: closing any descriptor of a file
/// releases every POSIX record lock this process holds on it (`fcntl` semantics), and SQLite
/// keeps its database and `-shm` locks that way. `scull-storage` restricts those files while
/// its own connection may be open, and a lost `-shm` lock lets another process reset the
/// shared memory under that connection (which then dies of `SIGBUS`).
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
    if checked.mode() & 0o7777 & !0o700 == 0 {
        return Ok(());
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
mod bulk;
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
