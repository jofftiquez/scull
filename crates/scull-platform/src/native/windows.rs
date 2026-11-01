//! Windows: handle-based Win32 calls through `windows-sys` (P1-PLAT-02).
//!
//! # How a path is inspected
//!
//! Every query opens a handle with `CreateFileW` and
//! `FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS`, `OPEN_EXISTING`, a share mode
//! of read | write | delete and, for [`metadata`], only `FILE_READ_ATTRIBUTES` access:
//!
//! * a symlink, junction or other reparse point is opened *itself*, never its target
//!   (SAFE-04, T-10);
//! * no data stream is opened or read, so cloud placeholders are never hydrated (PLT-04,
//!   T-52) and nothing is locked: attribute-only access doesn't take part in sharing checks,
//!   so even a file another process holds open exclusively can be inspected;
//! * nothing is created or modified (`OPEN_EXISTING`).
//!
//! The facts come from `GetFileInformationByHandleEx`:
//!
//! | Field | Source |
//! |---|---|
//! | kind, read-only, attributes, mtime | `FileBasicInfo` (+ `FileAttributeTagInfo` for the reparse tag) |
//! | identity | `FileIdInfo` (64-bit volume serial + 128-bit file ID, needed on ReFS); falls back to `GetFileInformationByHandle` (32-bit serial + 64-bit index) where `FileIdInfo` is unsupported (FAT, some network shares) |
//! | logical size, allocated size, link count | `FileStandardInfo` (`EndOfFile`, `AllocationSize`, `NumberOfLinks`) |
//! | allocated size of compressed / sparse files | `FileCompressionInfo` (`CompressedFileSize`, what `GetCompressedFileSizeW` returns), so PRD §9 "use allocated, not logical" holds |
//!
//! # Paths
//!
//! Absolute drive and UNC paths are normalized lexically (`/` → `\`, `.` dropped, `..`
//! applied) and sent in verbatim form (`\\?\C:\…`, `\\?\UNC\server\share\…`), so paths longer
//! than `MAX_PATH` work (PLT-02, FX-WIN-04) and names Win32 would otherwise rewrite, such as
//! reserved device names (`NUL`, `CON`) or trailing dots and spaces, address the entry with
//! exactly that name (FX-WIN-10). Paths that are already verbatim pass through unchanged.
//! Device-namespace paths (`\\.\…`, `\\?\pipe\…`, `\\?\GLOBALROOT\…`) are refused: they name
//! devices and pipes, not filesystem entries, and opening one can have side effects (SAFE-18).
//! 8.3 short names are resolved by the filesystem, so a short-name alias yields the same
//! identity as the long name (SAFE-13, FX-WIN-05).
//!
//! # Directory listings
//!
//! [`read_dir`] checks the kind with attribute-only access, then opens the directory with a
//! no-follow handle, re-checks on that handle that it is a real directory (not a reparse
//! point), and enumerates *that handle* with `FileIdExtdDirectoryInfo` (attributes, reparse
//! tag and the 128-bit file ID of every entry), or `FileFullDirectoryInfo` where the
//! filesystem doesn't support it (FAT, some network shares; the reparse tag is then in
//! `EaSize`). Entry kinds come from the listing itself, so entries are never opened.
//!
//! # Listed entries (#71, FX-FS-15)
//!
//! Opening every entry (`CreateFileW` + four queries + `CloseHandle`) dominated large scans,
//! more so with real-time antivirus. [`entry_metadata`] instead asks the filesystem by name,
//! without a handle: `NtQueryInformationByName(FileStatInformation)` (Windows 10 1709+) gives
//! attributes, sizes, mtime and the link count in one call. The listing's directory entry
//! alone is not enough: it has no link count, and NTFS keeps sizes and times per name, so
//! another name of a hard-linked file can show stale values. The answer is used only when:
//!
//! * the listing gave the entry's file ID and the directory's volume serial (the identity,
//!   128-bit as on ReFS, is the listing's), and the by-name answer has the same file ID (low
//!   64 bits): an entry replaced after the listing, or a name resolved elsewhere, falls back;
//! * neither the listing nor the answer marks it as a reparse point, a device, or a compressed
//!   or sparse file (their on-disk size needs `FileCompressionInfo`), and both agree on
//!   whether it is a directory.
//!
//! Everything else, and every failure of the by-name query, falls back to [`metadata`]: an
//! attribute-only, no-follow open as before. So the result is always what [`metadata`] gives
//! (`tests/entry_metadata.rs`); in particular the link count is never left unknown on the
//! fast path.

#![allow(unsafe_code)]

mod security;

pub(crate) use security::{
    create_dir_owner_only, create_file_owner_only, elevation, known_folder, owner_only_exposures,
    restrict_owner_only,
};

mod volumes;

pub(super) use volumes::{free_space, volume_of, volumes};

/// The account this process acts as, for binding plans (`crate::identity`): the token user's
/// SID string (`S-1-5-21-...`), which survives renaming the account.
pub(super) fn current_user_id() -> io::Result<String> {
    security::current_user_sid()
}

use crate::{
    Capabilities, DirEntry, DirIter, EntryKind, Metadata, Operation, PlatformError, Result,
};
use scull_core::FileIdentity;
use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::{MaybeUninit, offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_STAT_INFORMATION, FileStatInformation, NtQueryInformationByName,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_INVALID_FUNCTION, ERROR_INVALID_LEVEL,
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION,
    HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DEVICE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SPARSE_FILE, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_COMPRESSION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO,
    FILE_ID_EXTD_DIR_INFO, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileAttributeTagInfo, FileBasicInfo, FileCompressionInfo, FileFullDirectoryInfo,
    FileFullDirectoryRestartInfo, FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo,
    FileIdInfo, FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    OPEN_EXISTING, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_AF_UNIX, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};

/// 100-nanosecond FILETIME ticks per second.
const FILETIME_TICKS_PER_SEC: i64 = 10_000_000;
/// Seconds from the FILETIME epoch (1601-01-01) to the Unix epoch (1970-01-01).
const FILETIME_TO_UNIX_SECS: i64 = 11_644_473_600;
/// Size of the buffer one directory-enumeration call fills.
const DIR_BUF_LEN: usize = 64 * 1024;
const SEP: u16 = b'\\' as u16;

pub(super) fn capabilities() -> Capabilities {
    Capabilities {
        os: std::env::consts::OS,
        file_identity: true,
        allocated_size: true,
        link_count: true,
        mtime: true,
        no_follow_metadata: true,
        // Named pipes and devices live outside the filesystem namespace (and device paths are
        // refused); AF_UNIX sockets are recognized by reparse tag. Nothing is opened for data.
        special_file_kinds: true,
        reparse_points: true,
        race_free_read_dir: true,
        elevation_detection: true,
    }
}

// ---------------------------------------------------------------------------------------------
// Handles and handle queries
// ---------------------------------------------------------------------------------------------

/// An owned Win32 file handle, closed on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle returned by `CreateFileW` that this struct owns
        // exclusively and that has not been closed yet; it is never used after this.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

// SAFETY: a kernel file handle is not tied to the thread that opened it; it may be used and
// closed from any thread. `Handle` hands out no aliasing references to the handle.
unsafe impl Send for Handle {}

/// Opens `path` (NUL-terminated UTF-16) without following a reparse point at its final
/// component, without creating anything, and with the given access only.
fn open(path: &[u16], access: u32) -> io::Result<Handle> {
    if path.last() != Some(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not NUL-terminated",
        ));
    }
    // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call. A null
    // security-attributes pointer and a null template handle are documented as allowed.
    // OPEN_EXISTING never creates a file; FILE_FLAG_OPEN_REPARSE_POINT opens a link itself
    // rather than its target; FILE_FLAG_BACKUP_SEMANTICS is required to open directories.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(handle))
    }
}

/// A fixed-size output structure of `GetFileInformationByHandleEx`.
///
/// # Safety
///
/// `Self` must be the `#[repr(C)]` structure Windows writes for [`HandleInfo::CLASS`], and
/// every bit pattern must be a valid `Self` (plain integers only).
unsafe trait HandleInfo: Copy {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS;
}

// SAFETY: FILE_BASIC_INFO is the documented output of FileBasicInfo; four i64 and one u32.
unsafe impl HandleInfo for FILE_BASIC_INFO {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileBasicInfo;
}

// SAFETY: FILE_ATTRIBUTE_TAG_INFO is the documented output of FileAttributeTagInfo; two u32.
unsafe impl HandleInfo for FILE_ATTRIBUTE_TAG_INFO {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileAttributeTagInfo;
}

// SAFETY: FILE_ID_INFO is the documented output of FileIdInfo; a u64 and a [u8; 16].
unsafe impl HandleInfo for FILE_ID_INFO {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileIdInfo;
}

// SAFETY: FILE_COMPRESSION_INFO is the documented output of FileCompressionInfo; integers and
// a byte array only.
unsafe impl HandleInfo for FILE_COMPRESSION_INFO {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileCompressionInfo;
}

/// `FILE_STANDARD_INFO` with its two `BOOLEAN` fields as `u8`: the `windows-sys` definition
/// uses Rust `bool`, for which a byte other than 0 or 1 would be undefined behavior.
#[repr(C)]
#[derive(Clone, Copy)]
struct StandardInfo {
    allocation_size: i64,
    end_of_file: i64,
    number_of_links: u32,
    /// Layout only; not used.
    _delete_pending: u8,
    /// Layout only; the kind comes from the attributes.
    _directory: u8,
}

// SAFETY: same layout as FILE_STANDARD_INFO (i64, i64, u32, BOOLEAN, BOOLEAN), the documented
// output of FileStandardInfo; all fields are plain integers.
unsafe impl HandleInfo for StandardInfo {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileStandardInfo;
}

fn query<T: HandleInfo>(handle: &Handle) -> io::Result<T> {
    let mut out = MaybeUninit::<T>::zeroed();
    let size = u32::try_from(size_of::<T>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "info struct too large"))?;
    // SAFETY: `out` is valid for writes of `size` bytes and aligned for `T`; `HandleInfo`
    // guarantees `T` is the structure Windows writes for `T::CLASS`. `handle` is open.
    let ok =
        unsafe { GetFileInformationByHandleEx(handle.0, T::CLASS, out.as_mut_ptr().cast(), size) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `out` was zero-initialized and the successful call wrote a `T` into it; every
    // bit pattern is a valid `T` (`HandleInfo` contract).
    Ok(unsafe { out.assume_init() })
}

fn legacy_info(handle: &Handle) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut out = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `out` is a valid, aligned, writable BY_HANDLE_FILE_INFORMATION; `handle` is
    // open.
    let ok = unsafe { GetFileInformationByHandle(handle.0, out.as_mut_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zero-initialized and filled by the successful call; the struct holds only
    // integers, so every bit pattern is valid.
    Ok(unsafe { out.assume_init() })
}

// ---------------------------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------------------------

/// Kind from attributes and, for reparse points, the reparse tag. Never opens anything.
fn kind_from(attributes: u32, reparse_tag: Option<u32>) -> EntryKind {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        match reparse_tag {
            Some(IO_REPARSE_TAG_SYMLINK) => EntryKind::Symlink,
            Some(IO_REPARSE_TAG_MOUNT_POINT) => EntryKind::Junction,
            Some(IO_REPARSE_TAG_AF_UNIX) => EntryKind::Socket,
            // Cloud placeholders, app execution aliases, dedup, unknown or unreadable tags.
            _ => EntryKind::ReparsePoint,
        }
    } else if attributes & FILE_ATTRIBUTE_DEVICE != 0 {
        EntryKind::Other
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

fn filetime_to_unix(ticks: i64) -> Option<i64> {
    // 0 means "not provided by this filesystem".
    (ticks != 0).then(|| ticks.div_euclid(FILETIME_TICKS_PER_SEC) - FILETIME_TO_UNIX_SECS)
}

/// Volume serial + 128-bit file ID; `None` for an ID of 0 or all ones (see [`identity`]).
fn identity_of(volume: u64, file_id: [u8; 16]) -> Option<FileIdentity> {
    let file = u128::from_le_bytes(file_id);
    (file != 0 && file != u128::MAX).then(|| FileIdentity::new(volume, file))
}

/// Volume serial number + file ID. An ID of 0 or all ones is a filesystem saying "no stable
/// ID", so it is reported as unknown rather than as a (colliding) identity.
fn identity(handle: &Handle) -> Option<FileIdentity> {
    if let Ok(info) = query::<FILE_ID_INFO>(handle) {
        let file = u128::from_le_bytes(info.FileId.Identifier);
        if file != 0 && file != u128::MAX {
            return Some(FileIdentity::new(info.VolumeSerialNumber, file));
        }
    }
    let info = legacy_info(handle).ok()?;
    let file = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    (file != 0 && file != u64::MAX)
        .then(|| FileIdentity::new(u64::from(info.dwVolumeSerialNumber), u128::from(file)))
}

fn metadata_from_handle(handle: &Handle) -> io::Result<Metadata> {
    let basic: FILE_BASIC_INFO = query(handle)?;
    let attributes = basic.FileAttributes;
    let reparse_tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // If the tag can't be read the kind stays the protective `ReparsePoint`.
        query::<FILE_ATTRIBUTE_TAG_INFO>(handle)
            .ok()
            .map(|t| t.ReparseTag)
    } else {
        None
    };
    let standard: StandardInfo = query(handle)?;

    let mut allocated_size = u64::try_from(standard.allocation_size).ok();
    let compressed_or_sparse =
        attributes & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE) != 0;
    if compressed_or_sparse && attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        // Bytes actually on disk for a compressed or sparse file.
        if let Some(on_disk) = query::<FILE_COMPRESSION_INFO>(handle)
            .ok()
            .and_then(|c| u64::try_from(c.CompressedFileSize).ok())
        {
            allocated_size = Some(on_disk);
        }
    }

    Ok(Metadata {
        kind: kind_from(attributes, reparse_tag),
        identity: identity(handle),
        logical_size: u64::try_from(standard.end_of_file).unwrap_or(0),
        allocated_size,
        nlink: (standard.number_of_links > 0).then(|| u64::from(standard.number_of_links)),
        mtime_unix: filetime_to_unix(basic.LastWriteTime),
        readonly: attributes & FILE_ATTRIBUTE_READONLY != 0,
        file_attributes: Some(attributes),
        reparse_tag,
        bsd_flags: None,
    })
}

/// Last resort when even an attribute-only open is refused with a sharing violation (system
/// files such as `pagefile.sys`): std falls back to the directory entry. Identity, allocated
/// size, link count and the exact reparse tag are then unknown, and any reparse point is
/// reported as the protective [`EntryKind::ReparsePoint`].
fn metadata_from_std(path: &Path) -> io::Result<Metadata> {
    use std::os::windows::fs::MetadataExt;
    let md = std::fs::symlink_metadata(path)?;
    let attributes = md.file_attributes();
    Ok(Metadata {
        kind: kind_from(attributes, None),
        identity: None,
        logical_size: md.file_size(),
        allocated_size: None,
        nlink: None,
        mtime_unix: i64::try_from(md.last_write_time())
            .ok()
            .and_then(filetime_to_unix),
        readonly: attributes & FILE_ATTRIBUTE_READONLY != 0,
        file_attributes: Some(attributes),
        reparse_tag: None,
        bsd_flags: None,
    })
}

fn metadata_wide(path: &Path, wide: &[u16]) -> io::Result<Metadata> {
    match open(wide, FILE_READ_ATTRIBUTES) {
        Ok(handle) => metadata_from_handle(&handle),
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {
            metadata_from_std(path)
        }
        Err(e) => Err(e),
    }
}

pub(super) fn metadata(path: &Path) -> io::Result<Metadata> {
    metadata_wide(path, &verbatim(path)?)
}

/// Attributes that send an entry to the handle-based [`metadata`] (see the module docs).
const NEEDS_HANDLE: u32 = FILE_ATTRIBUTE_REPARSE_POINT
    | FILE_ATTRIBUTE_DEVICE
    | FILE_ATTRIBUTE_COMPRESSED
    | FILE_ATTRIBUTE_SPARSE_FILE;

/// Cleared when this Windows doesn't know `FileStatInformation` (before 10 1709).
static BY_NAME: AtomicBool = AtomicBool::new(true);
/// Listed entries answered by name, without a handle (tests).
static BY_NAME_HITS: AtomicU64 = AtomicU64::new(0);

/// How many listed entries [`entry_metadata`] answered by name, without opening them (tests).
#[cfg(test)]
fn by_name_hits() -> u64 {
    BY_NAME_HITS.load(Ordering::Relaxed)
}

/// A listed entry's metadata: by name without a handle when the listing allows it (see the
/// module docs), else [`metadata`] of its path.
pub(super) fn entry_metadata(entry: &DirEntry) -> io::Result<Metadata> {
    if let Some(listed) = entry.listed
        && listed.attributes & NEEDS_HANDLE == 0
        && BY_NAME.load(Ordering::Relaxed)
        && let Some(md) = stat_by_name(&entry.path, &listed)
    {
        BY_NAME_HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(md);
    }
    metadata(&entry.path)
}

/// `STATUS_INVALID_INFO_CLASS` / `STATUS_NOT_IMPLEMENTED`: this Windows lacks the class.
const STATUS_INVALID_INFO_CLASS: NTSTATUS = 0xC000_0003_u32 as NTSTATUS;
const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC000_0002_u32 as NTSTATUS;
/// `OBJ_CASE_INSENSITIVE`: Win32 name semantics.
const OBJ_CASE_INSENSITIVE: u32 = 0x40;

/// `FileStatInformation` of `path` by name, accepted only if it is the listed file and needs
/// no handle (see the module docs); `None` sends the entry to [`metadata`].
fn stat_by_name(path: &Path, listed: &crate::types::Listed) -> Option<Metadata> {
    // `\\?\C:\…` (verbatim, NUL-terminated) is the NT path `\??\C:\…`.
    let mut wide = verbatim(path).ok()?;
    wide.pop(); // the NUL
    if wide.get(..4) != Some(&[SEP, SEP, u16::from(b'?'), SEP]) {
        return None;
    }
    wide[1] = u16::from(b'?');
    let bytes = u16::try_from(wide.len() * 2).ok()?;
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: ptr::null_mut(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut io_status = MaybeUninit::<IO_STATUS_BLOCK>::zeroed();
    let mut st = MaybeUninit::<FILE_STAT_INFORMATION>::zeroed();
    // SAFETY: `attributes` and the UNICODE_STRING it points to (and `wide`, its buffer) live
    // until the call returns; `io_status` and `st` are writable, aligned and of the sizes
    // passed. The call queries by name: it creates, opens or changes nothing visible.
    let status = unsafe {
        NtQueryInformationByName(
            &attributes,
            io_status.as_mut_ptr(),
            st.as_mut_ptr().cast(),
            size_of::<FILE_STAT_INFORMATION>() as u32,
            FileStatInformation,
        )
    };
    if status < 0 {
        if status == STATUS_INVALID_INFO_CLASS || status == STATUS_NOT_IMPLEMENTED {
            BY_NAME.store(false, Ordering::Relaxed);
        }
        return None;
    }
    // SAFETY: zero-initialized and filled by the successful call; FILE_STAT_INFORMATION holds
    // only integers, so every bit pattern is valid.
    let st = unsafe { st.assume_init() };
    let same_file = st.FileId as u64 == listed.file_id_low;
    let plain = st.FileAttributes & NEEDS_HANDLE == 0;
    let same_kind = (st.FileAttributes ^ listed.attributes) & FILE_ATTRIBUTE_DIRECTORY == 0;
    if !(same_file && plain && same_kind) {
        return None;
    }
    let attrs = st.FileAttributes;
    Some(Metadata {
        kind: kind_from(attrs, None),
        identity: listed.identity,
        logical_size: u64::try_from(st.EndOfFile).unwrap_or(0),
        allocated_size: u64::try_from(st.AllocationSize).ok(),
        nlink: (st.NumberOfLinks > 0).then(|| u64::from(st.NumberOfLinks)),
        mtime_unix: filetime_to_unix(st.LastWriteTime),
        readonly: attrs & FILE_ATTRIBUTE_READONLY != 0,
        file_attributes: Some(attrs),
        reparse_tag: None,
        bsd_flags: None,
    })
}

// ---------------------------------------------------------------------------------------------
// Directory listing
// ---------------------------------------------------------------------------------------------

pub(super) fn read_dir(path: &Path) -> Result<DirIter<'static>> {
    let wide = verbatim(path).map_err(|e| PlatformError::from_io(Operation::ReadDir, path, e))?;

    // Check with attribute-only access first, so a link or a file is never opened with
    // FILE_LIST_DIRECTORY (= FILE_READ_DATA) access.
    let found = metadata_wide(path, &wide)
        .map_err(|e| PlatformError::from_io(Operation::Metadata, path, e))?
        .kind;
    if found != EntryKind::Dir {
        return Err(PlatformError::not_a_directory(path, found));
    }

    let handle = open(
        &wide,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
    )
    .map_err(|e| PlatformError::from_io(Operation::ReadDir, path, e))?;

    // Re-check on the handle that will be enumerated: if the directory was swapped for a
    // junction or symlink after the check above, the no-follow open got the link itself.
    let basic: FILE_BASIC_INFO =
        query(&handle).map_err(|e| PlatformError::from_io(Operation::ReadDir, path, e))?;
    let attributes = basic.FileAttributes;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || attributes & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        let tag = query::<FILE_ATTRIBUTE_TAG_INFO>(&handle)
            .ok()
            .map(|t| t.ReparseTag);
        return Err(PlatformError::not_a_directory(
            path,
            kind_from(attributes, tag),
        ));
    }

    // The volume serial that identities of listed entries are built with (as `metadata` does
    // with `FileIdInfo`); without it entries are stat'ed through a handle.
    let volume = query::<FILE_ID_INFO>(&handle)
        .ok()
        .map(|i| i.VolumeSerialNumber);

    Ok(Box::new(WinReadDir {
        handle,
        dir: path.to_path_buf(),
        buf: Box::new(DirBuf([0; DIR_BUF_LEN])),
        next: None,
        restart: true,
        done: false,
        extd: true,
        volume,
    }))
}

/// Enumeration buffer. `FILE_ID_EXTD_DIR_INFO` / `FILE_FULL_DIR_INFO` records are 8-byte
/// aligned.
#[repr(C, align(8))]
struct DirBuf([u8; DIR_BUF_LEN]);

struct WinReadDir {
    handle: Handle,
    dir: PathBuf,
    buf: Box<DirBuf>,
    /// Offset of the next unread record in `buf`; `None` when `buf` must be refilled.
    next: Option<usize>,
    /// The next fill is the first one (restarts the scan).
    restart: bool,
    done: bool,
    /// Enumerating with `FileIdExtdDirectoryInfo` (else `FileFullDirectoryInfo`).
    extd: bool,
    /// The directory's volume serial (`FileIdInfo`), for the identity of listed entries.
    volume: Option<u64>,
}

impl WinReadDir {
    /// Fills `buf` with the next batch of records. `Ok(false)` at the end of the directory.
    fn fill(&mut self) -> io::Result<bool> {
        let first = self.restart;
        self.restart = false;
        let class = match (self.extd, first) {
            (true, true) => FileIdExtdDirectoryRestartInfo,
            (true, false) => FileIdExtdDirectoryInfo,
            (false, true) => FileFullDirectoryRestartInfo,
            (false, false) => FileFullDirectoryInfo,
        };
        // SAFETY: `self.buf` is an owned, 8-byte-aligned, writable buffer of exactly
        // DIR_BUF_LEN bytes that outlives the call; `self.handle` is an open directory handle
        // with FILE_LIST_DIRECTORY access.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                self.handle.0,
                class,
                self.buf.0.as_mut_ptr().cast(),
                DIR_BUF_LEN as u32,
            )
        };
        if ok != 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        let unsupported = [
            ERROR_INVALID_PARAMETER,
            ERROR_INVALID_LEVEL,
            ERROR_NOT_SUPPORTED,
            ERROR_INVALID_FUNCTION,
        ];
        match err.raw_os_error() {
            // This filesystem has no FileIdExtdDirectoryInfo: list it the classic way.
            Some(code) if first && self.extd && unsupported.iter().any(|&c| code == c as i32) => {
                self.extd = false;
                self.restart = true;
                self.fill()
            }
            Some(code) if code == ERROR_NO_MORE_FILES as i32 => Ok(false),
            // A first query that matches nothing (an empty volume root has no `.`/`..`).
            Some(code) if first && code == ERROR_FILE_NOT_FOUND as i32 => Ok(false),
            _ => Err(err),
        }
    }

    fn entry(&self, record: Record) -> DirEntry {
        let reparse_tag =
            (record.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(record.reparse_tag);
        let listed = match (self.volume, record.file_id) {
            (Some(volume), Some(id)) => Some(crate::types::Listed {
                identity: identity_of(volume, id),
                file_id_low: u64::from_le_bytes(id[..8].try_into().unwrap_or([0; 8])),
                attributes: record.attributes,
            }),
            _ => None,
        };
        DirEntry {
            path: self.dir.join(&record.name),
            kind: kind_from(record.attributes, reparse_tag),
            file_name: record.name,
            reparse_tag,
            at: None,
            listed,
        }
    }
}

impl Iterator for WinReadDir {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            let Some(offset) = self.next else {
                match self.fill() {
                    Ok(true) => self.next = Some(0),
                    Ok(false) => self.done = true,
                    Err(e) => {
                        self.done = true;
                        return Some(Err(PlatformError::from_io(
                            Operation::ReadDirEntry,
                            &self.dir,
                            e,
                        )));
                    }
                }
                continue;
            };
            match parse_record(&self.buf.0, offset, self.extd) {
                Ok(record) => {
                    self.next = (record.next_entry_offset != 0)
                        .then(|| offset + record.next_entry_offset as usize);
                    if record.name == "." || record.name == ".." {
                        continue;
                    }
                    return Some(Ok(self.entry(record)));
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(PlatformError::from_io(
                        Operation::ReadDirEntry,
                        &self.dir,
                        e,
                    )));
                }
            }
        }
    }
}

/// The fields of one `FILE_ID_EXTD_DIR_INFO` / `FILE_FULL_DIR_INFO` record that a listing
/// needs.
struct Record {
    next_entry_offset: u32,
    attributes: u32,
    /// The reparse tag when the entry is a reparse point: `ReparsePointTag`, or `EaSize` in a
    /// `FILE_FULL_DIR_INFO` record ([MS-FSCC] 2.4.14).
    reparse_tag: u32,
    /// The 128-bit file ID (`FILE_ID_EXTD_DIR_INFO` only).
    file_id: Option<[u8; 16]>,
    name: OsString,
}

fn malformed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "malformed directory enumeration record",
    )
}

fn read_u32(bytes: &[u8], at: usize) -> io::Result<u32> {
    let end = at.checked_add(4).ok_or_else(malformed)?;
    let b: [u8; 4] = bytes
        .get(at..end)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(malformed)?;
    Ok(u32::from_le_bytes(b))
}

/// Decodes the record at `offset` (`FILE_ID_EXTD_DIR_INFO` when `extd`, else
/// `FILE_FULL_DIR_INFO`) with bounds-checked byte reads (no pointer casts).
fn parse_record(buf: &[u8], offset: usize, extd: bool) -> io::Result<Record> {
    let rec = buf.get(offset..).ok_or_else(malformed)?;
    let (next_entry_offset, attributes, name_len, reparse_tag, file_id, name_at) = if extd {
        let id_at = offset_of!(FILE_ID_EXTD_DIR_INFO, FileId);
        let id: [u8; 16] = rec
            .get(id_at..id_at + 16)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(malformed)?;
        (
            read_u32(rec, offset_of!(FILE_ID_EXTD_DIR_INFO, NextEntryOffset))?,
            read_u32(rec, offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes))?,
            read_u32(rec, offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength))? as usize,
            read_u32(rec, offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag))?,
            Some(id),
            offset_of!(FILE_ID_EXTD_DIR_INFO, FileName),
        )
    } else {
        (
            read_u32(rec, offset_of!(FILE_FULL_DIR_INFO, NextEntryOffset))?,
            read_u32(rec, offset_of!(FILE_FULL_DIR_INFO, FileAttributes))?,
            read_u32(rec, offset_of!(FILE_FULL_DIR_INFO, FileNameLength))? as usize,
            read_u32(rec, offset_of!(FILE_FULL_DIR_INFO, EaSize))?,
            None,
            offset_of!(FILE_FULL_DIR_INFO, FileName),
        )
    };
    let name_end = name_at.checked_add(name_len).ok_or_else(malformed)?;
    // The next record must start after this one's name, which also guarantees progress.
    if !name_len.is_multiple_of(2)
        || (next_entry_offset != 0 && (next_entry_offset as usize) < name_end)
    {
        return Err(malformed());
    }
    let units: Vec<u16> = rec
        .get(name_at..name_end)
        .ok_or_else(malformed)?
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(Record {
        next_entry_offset,
        attributes,
        reparse_tag,
        file_id,
        name: OsString::from_wide(&units),
    })
}

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

fn nul_terminated(s: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = s.encode_wide().collect();
    if wide.contains(&0) {
        return Err(invalid("path contains a NUL character"));
    }
    wide.push(0);
    Ok(wide)
}

/// `\\?\Volume{GUID}\…` names a volume, not a device: allowed.
fn is_volume_guid(name: &OsStr) -> bool {
    name.to_str().is_some_and(|n| {
        n.len() > 7
            && n.get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("Volume{"))
    })
}

/// The NUL-terminated verbatim form of `path` (see the module docs).
fn verbatim(path: &Path) -> io::Result<Vec<u16>> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "empty path"));
    }
    let absolute: Cow<'_, Path> = if path.is_absolute() {
        Cow::Borrowed(path)
    } else if path.has_root() || matches!(path.components().next(), Some(Component::Prefix(_))) {
        // `\foo` (current drive) or `C:foo` (per-drive current directory): only Win32 knows.
        Cow::Owned(std::path::absolute(path)?)
    } else {
        // Plain relative path: join lexically so reserved names and trailing dots survive.
        Cow::Owned(std::env::current_dir()?.join(path))
    };

    let mut components = absolute.components();
    let mut out: Vec<u16> = Vec::with_capacity(absolute.as_os_str().len() + 8);
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) => {
                out.extend(r"\\?\".encode_utf16());
                out.push(u16::from(letter));
                out.push(u16::from(b':'));
            }
            Prefix::UNC(server, share) => {
                out.extend(r"\\?\UNC\".encode_utf16());
                out.extend(server.encode_wide());
                out.push(SEP);
                out.extend(share.encode_wide());
            }
            Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..) => {
                return nul_terminated(absolute.as_os_str());
            }
            Prefix::Verbatim(name) if is_volume_guid(name) => {
                return nul_terminated(absolute.as_os_str());
            }
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
                return Err(invalid(
                    "device-namespace paths name devices or pipes, not filesystem entries",
                ));
            }
        },
        _ => return Err(invalid("path could not be made absolute")),
    }

    let mut parts: Vec<&OsStr> = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => parts.push(name),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out.push(SEP);
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push(SEP);
        }
        out.extend(part.encode_wide());
    }
    if out.contains(&0) {
        return Err(invalid("path contains a NUL character"));
    }
    out.push(0);
    Ok(out)
}

pub(super) fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    Cow::Owned(
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Native, Platform};
    use std::error::Error;
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    type TestResult = std::result::Result<(), Box<dyn Error>>;

    fn verbatim_string(path: &str) -> io::Result<String> {
        let mut wide = verbatim(Path::new(path))?;
        assert_eq!(wide.pop(), Some(0));
        Ok(String::from_utf16_lossy(&wide))
    }

    #[test]
    fn verbatim_normalizes_drive_and_unc_paths() -> TestResult {
        assert_eq!(verbatim_string(r"C:\a\b")?, r"\\?\C:\a\b");
        assert_eq!(verbatim_string(r"C:/a/./b/../c/")?, r"\\?\C:\a\c");
        assert_eq!(verbatim_string(r"C:\")?, r"\\?\C:\");
        assert_eq!(
            verbatim_string(r"\\srv\share\x\y")?,
            r"\\?\UNC\srv\share\x\y"
        );
        // Reserved names and trailing dots are kept, not rewritten to devices (FX-WIN-10).
        assert_eq!(verbatim_string(r"C:\x\NUL")?, r"\\?\C:\x\NUL");
        assert_eq!(verbatim_string(r"C:\x\foo. ")?, r"\\?\C:\x\foo. ");
        // Already verbatim: unchanged.
        assert_eq!(verbatim_string(r"\\?\C:\x\..\y")?, r"\\?\C:\x\..\y");
        assert_eq!(
            verbatim_string(r"\\?\UNC\srv\share\x")?,
            r"\\?\UNC\srv\share\x"
        );
        let vol = r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\x";
        assert_eq!(verbatim_string(vol)?, vol);
        Ok(())
    }

    #[test]
    fn verbatim_makes_relative_paths_absolute() -> TestResult {
        let cwd = std::env::current_dir()?;
        let got = verbatim_string("rel\\NUL")?;
        assert!(got.starts_with(r"\\?\"), "{got}");
        assert!(got.ends_with(r"\rel\NUL"), "{got}");
        let cwd_name = cwd.file_name().ok_or("cwd has no name")?;
        assert!(got.contains(&*cwd_name.to_string_lossy()), "{got}");
        Ok(())
    }

    #[test]
    fn verbatim_refuses_device_namespace() {
        for p in [
            r"\\.\pipe\x",
            r"\\.\C:",
            r"\\?\pipe\x",
            r"\\?\GLOBALROOT\Device\Null",
        ] {
            let err = verbatim(Path::new(p)).err();
            assert!(
                matches!(&err, Some(e) if e.kind() == io::ErrorKind::InvalidInput),
                "{p}: {err:?}"
            );
        }
        assert_eq!(
            verbatim(Path::new("")).err().map(|e| e.kind()),
            Some(io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn kinds_from_reparse_tags() {
        let rp = FILE_ATTRIBUTE_REPARSE_POINT;
        let dir = FILE_ATTRIBUTE_DIRECTORY;
        assert_eq!(kind_from(0, None), EntryKind::File);
        assert_eq!(kind_from(dir, None), EntryKind::Dir);
        assert_eq!(
            kind_from(rp | dir, Some(IO_REPARSE_TAG_MOUNT_POINT)),
            EntryKind::Junction
        );
        assert_eq!(
            kind_from(rp | dir, Some(IO_REPARSE_TAG_SYMLINK)),
            EntryKind::Symlink
        );
        assert_eq!(
            kind_from(rp, Some(IO_REPARSE_TAG_SYMLINK)),
            EntryKind::Symlink
        );
        assert_eq!(
            kind_from(rp, Some(IO_REPARSE_TAG_AF_UNIX)),
            EntryKind::Socket
        );
        // Cloud placeholders, app execution aliases, unknown and unreadable tags.
        for tag in [
            Some(windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_CLOUD_6),
            Some(windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_ONEDRIVE),
            Some(windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_APPEXECLINK),
            Some(0x8000_7777),
            None,
        ] {
            assert_eq!(kind_from(rp, tag), EntryKind::ReparsePoint, "{tag:?}");
            assert!(kind_from(rp | dir, tag).is_link());
        }
        assert_eq!(kind_from(FILE_ATTRIBUTE_DEVICE, None), EntryKind::Other);
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_unix(0), None);
        assert_eq!(
            filetime_to_unix(FILETIME_TO_UNIX_SECS * 10_000_000),
            Some(0)
        );
        assert_eq!(
            filetime_to_unix((FILETIME_TO_UNIX_SECS + 1_600_000_000) * 10_000_000 + 9_999_999),
            Some(1_600_000_000)
        );
        assert_eq!(
            filetime_to_unix((FILETIME_TO_UNIX_SECS - 1) * 10_000_000 + 1),
            Some(-1)
        );
    }

    #[test]
    fn parse_record_rejects_truncated_and_non_advancing_records() {
        let name_at = offset_of!(FILE_FULL_DIR_INFO, FileName);
        let mut buf = vec![0u8; name_at + 4];
        buf[offset_of!(FILE_FULL_DIR_INFO, FileNameLength)] = 4;
        buf[name_at] = b'a';
        buf[name_at + 2] = b'b';
        let rec = parse_record(&buf, 0, false).ok();
        assert_eq!(rec.map(|r| r.name), Some(OsString::from("ab")));
        // Name runs past the buffer.
        buf[offset_of!(FILE_FULL_DIR_INFO, FileNameLength)] = 6;
        assert!(parse_record(&buf, 0, false).is_err());
        // Next record would overlap this one.
        buf[offset_of!(FILE_FULL_DIR_INFO, FileNameLength)] = 4;
        buf[0] = 8;
        assert!(parse_record(&buf, 0, false).is_err());
        assert!(parse_record(&buf, buf.len() + 1, false).is_err());
    }

    #[test]
    fn parse_record_reads_the_file_id_and_reparse_tag_of_an_extended_record() {
        let name_at = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);
        let mut buf = vec![0u8; name_at + 2];
        buf[offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength)] = 2;
        buf[offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes)] = 0x10; // directory
        let tag_at = offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag);
        buf[tag_at..tag_at + 4].copy_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        let id_at = offset_of!(FILE_ID_EXTD_DIR_INFO, FileId);
        for (i, b) in buf[id_at..id_at + 16].iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        buf[name_at] = b'z';
        let rec = parse_record(&buf, 0, true).unwrap();
        assert_eq!(rec.name, OsString::from("z"));
        assert_eq!(rec.attributes, 0x10);
        assert_eq!(rec.reparse_tag, IO_REPARSE_TAG_SYMLINK);
        let id = rec.file_id.unwrap();
        assert_eq!(id[0], 1);
        assert_eq!(id[15], 16);
        assert!(parse_record(&buf[..name_at], 0, true).is_err());
    }

    /// #71: listed entries are answered by name, without a handle, with exactly what the
    /// handle-based `metadata` gives (including a hard link's count).
    #[test]
    fn listed_entries_are_stated_by_name_like_metadata() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let d = tmp.path();
        std::fs::write(d.join("a.txt"), b"hello")?;
        std::fs::write(d.join("big.bin"), vec![1u8; 200_000])?;
        std::fs::create_dir(d.join("sub"))?;
        std::fs::hard_link(d.join("a.txt"), d.join("link.txt"))?;
        let before = by_name_hits();
        let mut n = 0;
        for e in read_dir(d)? {
            let e = e?;
            assert!(
                e.listed.is_some(),
                "{}: NTFS lists file IDs",
                e.path.display()
            );
            let fast = entry_metadata(&e)?;
            assert_eq!(fast, metadata(&e.path)?, "{}", e.path.display());
            if e.file_name == "link.txt" {
                assert_eq!(fast.nlink, Some(2));
            }
            n += 1;
        }
        assert_eq!(n, 4);
        assert!(by_name_hits() >= before + 4, "answered by name");
        Ok(())
    }

    /// An entry replaced after the listing is not answered by name (its file ID changed): the
    /// result is the new file's, from a handle, never a mix.
    #[test]
    fn an_entry_replaced_after_the_listing_falls_back_to_a_handle() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, b"old")?;
        let entry = read_dir(tmp.path())?.next().ok_or("no entry")??;
        let listed = entry.listed.ok_or("no listing data")?;
        std::fs::remove_file(&f)?;
        std::fs::write(&f, b"replaced!")?;
        let md = entry_metadata(&entry)?;
        assert_eq!(md, metadata(&f)?);
        assert_ne!(md.identity, listed.identity);
        assert_eq!(md.logical_size, 9);
        // Gone entirely: not found, like `metadata`.
        std::fs::remove_file(&f)?;
        assert_eq!(
            entry_metadata(&entry).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        Ok(())
    }

    /// Skips a test whose OS feature is unavailable, except on CI (`CI` is set), where the
    /// feature is expected and a silent skip would hide a regression. Set
    /// `SCULL_TEST_ALLOW_SKIP=1` to allow the skip on a CI runner that lacks the feature.
    fn skip(reason: &str) -> TestResult {
        if std::env::var_os("CI").is_some() && std::env::var_os("SCULL_TEST_ALLOW_SKIP").is_none() {
            return Err(
                format!("{reason} (required on CI; set SCULL_TEST_ALLOW_SKIP=1 to skip)").into(),
            );
        }
        eprintln!("skipping: {reason}");
        Ok(())
    }

    /// `GetShortPathNameW`, or `None` when the volume has no 8.3 name for `path`.
    fn short_path(path: &Path) -> Option<PathBuf> {
        let wide = nul_terminated(path.as_os_str()).ok()?;
        let mut buf = vec![0u16; 4096];
        // SAFETY: `wide` is NUL-terminated; `buf` is writable for `buf.len()` UTF-16 units.
        let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) }
            as usize;
        if n == 0 || n >= buf.len() {
            return None;
        }
        buf.truncate(n);
        Some(PathBuf::from(OsString::from_wide(&buf)))
    }

    /// FX-WIN-05 (platform part, SAFE-13): an 8.3 alias resolves to the same identity as the
    /// long name, so identity comparison can't be bypassed with a short name.
    #[test]
    fn fx_win_05_short_name_alias_has_same_identity() -> TestResult {
        let tmp = tempfile::tempdir()?;
        let long_dir = tmp.path().join("a protected folder with a long name");
        std::fs::create_dir(&long_dir)?;
        std::fs::write(long_dir.join("file.txt"), b"x")?;

        let Some(short_dir) = short_path(&long_dir) else {
            return skip("GetShortPathNameW failed");
        };
        if short_dir.file_name() == long_dir.file_name() {
            return skip("8.3 short names are disabled on this volume");
        }
        eprintln!("{} -> {}", long_dir.display(), short_dir.display());

        let long_md = Native.metadata(&long_dir)?;
        let short_md = Native.metadata(&short_dir)?;
        assert!(long_md.identity.is_some());
        assert_eq!(long_md.identity, short_md.identity);
        assert_eq!(short_md.kind, EntryKind::Dir);
        assert_eq!(
            Native.metadata(&long_dir.join("file.txt"))?.identity,
            Native.metadata(&short_dir.join("file.txt"))?.identity
        );
        Ok(())
    }
}
