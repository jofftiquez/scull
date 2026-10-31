//! Windows: the removal walker on handles. Every entry is opened with `NtCreateFile` relative
//! to a held directory handle (`FILE_OPEN_REPARSE_POINT`, `OBJ_DONT_REPARSE`), verified on the
//! opened handle, and deleted through that handle with `SetFileInformationByHandle`
//! (`FileDispositionInfoEx`). Nothing is deleted by path. See the parent module for the
//! contract; the decision tables live in `win_rules` (unit-tested on every OS).

#![allow(unsafe_code)]

use super::win_rules::{self, MountPointTarget, OpenFailure, raw};
use super::{
    AbortReason, Anomaly, MAX_DEPTH, MAX_SKIPPED_LISTED, RemoveError, RemoveOutcome, RemoveReport,
    RemoveRequest, SkipReason, SkippedEntry, VERIFY_EVERY, WalkEvent, plain_components,
};
use crate::EntryKind;
use crate::native::windows::{
    Handle, StandardInfo, identity, kind_from, parse_record as parse_full_record, query, verbatim,
};
use scull_core::FileIdentity;
use std::ffi::OsString;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    GetLastError, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
    UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_SPARSE_FILE,
    FILE_ATTRIBUTE_TAG_INFO, FILE_COMPRESSION_INFO, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FileDispositionInfo,
    FileDispositionInfoEx, FileFullDirectoryInfo, FileFullDirectoryRestartInfo,
    FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, GetFileInformationByHandleEx,
    GetFinalPathNameByHandleW, OPEN_EXISTING, SYNCHRONIZE, SetFileInformationByHandle,
    VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};

/// Size of the buffer one directory-enumeration call fills.
const LIST_BUF_LEN: usize = 64 * 1024;
/// Extra attempts after a sharing violation (antivirus scanners briefly open new files).
const SHARING_RETRIES: u32 = 4;
/// Pause between those attempts.
const SHARING_RETRY_DELAY: Duration = Duration::from_millis(50);
/// Restart Manager queries per walk (each takes a moment; a tree full of locked files must not
/// stall the walk). Later locked files are reported without the holder's name.
const RESTART_MANAGER_QUERIES: usize = 16;
/// `STATUS_UNSUCCESSFUL`, for a "successful" open that returned no handle.
const STATUS_UNSUCCESSFUL: NTSTATUS = 0xC000_0001_u32 as i32;

/// Walker limits and test hooks.
pub(crate) struct Tuning {
    /// Deepest level descended to (the target is depth 0).
    pub(crate) max_depth: usize,
    /// Re-verify a directory's place at least every this many entries (at least 1).
    pub(crate) verify_every: usize,
    /// Test only: entries with this name report another volume serial number, to simulate a
    /// volume crossing the test cannot set up.
    #[cfg(test)]
    pub(crate) foreign_volume_name: Option<Vec<u16>>,
    /// Test only: junctions with this name are treated as volume mount points.
    #[cfg(test)]
    pub(crate) volume_mount_name: Option<Vec<u16>>,
    /// Test only: list with `FileFullDirectoryInfo` (no file IDs), the FAT fallback.
    #[cfg(test)]
    pub(crate) force_full_listing: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            max_depth: MAX_DEPTH,
            verify_every: VERIFY_EVERY,
            #[cfg(test)]
            foreign_volume_name: None,
            #[cfg(test)]
            volume_mount_name: None,
            #[cfg(test)]
            force_full_listing: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Opening entries relative to a directory handle
// ---------------------------------------------------------------------------------------------

/// Whether `OBJ_DONT_REPARSE` is accepted by this Windows build (cleared on the first
/// `STATUS_INVALID_PARAMETER`, like `std::fs::remove_dir_all`; see
/// `win_rules::may_be_unsupported_dont_reparse`).
static DONT_REPARSE: AtomicBool = AtomicBool::new(true);

/// What an entry is opened for. Every open is relative to a held directory handle, no-follow
/// (`FILE_OPEN_REPARSE_POINT`), synchronous, and never requests data access to a
/// non-directory: nothing is read, hydrated or locked for I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Attributes and identity only. Shares everything, so it never conflicts with anyone.
    Probe,
    /// An intermediate path component: traverse only.
    Traverse,
    /// A real directory to list and empty: list + traverse access and **no
    /// `FILE_SHARE_DELETE`**, so nobody can rename, move or delete it while it is held (that
    /// needs `DELETE` access to it, which the share mode refuses). No `DELETE` access of our
    /// own: a held `DELETE` would make renames *into* the directory fail for everyone else
    /// (their target-directory open does not share delete).
    Dir,
    /// An emptied directory, re-opened by name to delete it through the new handle (after the
    /// pinning handle is closed; the new handle is identity-checked before the delete).
    DirDelete,
    /// A non-directory (file, link, other reparse point) to delete through this handle.
    Delete {
        /// `FILE_NON_DIRECTORY_FILE`: fail if it is (now) a directory.
        non_directory: bool,
    },
}

impl Purpose {
    /// `(desired access, share mode, create options)`.
    fn params(self) -> (u32, u32, u32) {
        let share_all = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
        match self {
            Purpose::Probe => (FILE_READ_ATTRIBUTES | SYNCHRONIZE, share_all, 0),
            Purpose::Traverse => (
                FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                share_all,
                FILE_DIRECTORY_FILE,
            ),
            Purpose::Dir => (
                FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_DIRECTORY_FILE,
            ),
            Purpose::DirDelete => (
                DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                share_all,
                FILE_DIRECTORY_FILE,
            ),
            Purpose::Delete { non_directory } => (
                DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                share_all,
                if non_directory {
                    FILE_NON_DIRECTORY_FILE
                } else {
                    0
                },
            ),
        }
    }
}

/// How an entry of `kind` is opened for removal.
fn purpose_for(kind: EntryKind) -> Purpose {
    match kind {
        EntryKind::Dir => Purpose::Dir,
        EntryKind::File => Purpose::Delete {
            non_directory: true,
        },
        // Links and other reparse points may carry the directory attribute (junctions,
        // directory symlinks): no directory option either way; the kind is checked on the
        // opened handle.
        _ => Purpose::Delete {
            non_directory: false,
        },
    }
}

/// One `NtCreateFile` of the single component `name` relative to the directory `parent`.
fn nt_open(
    parent: &Handle,
    name: &[u16],
    access: u32,
    share: u32,
    options: u32,
) -> Result<Handle, NTSTATUS> {
    // `win_rules::validate_name` has already bounded the length; this only avoids a panic.
    let len = u16::try_from(name.len() * 2).map_err(|_| raw::STATUS_OBJECT_NAME_INVALID)?;
    let object_name = UNICODE_STRING {
        Length: len,
        MaximumLength: len,
        // NtCreateFile only reads the name; the cast to `*mut` is what the struct requires.
        Buffer: name.as_ptr().cast_mut(),
    };
    let dont_reparse = DONT_REPARSE.load(Ordering::Relaxed);
    let object = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw(),
        ObjectName: &object_name,
        Attributes: if dont_reparse { OBJ_DONT_REPARSE } else { 0 },
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let status = match nt_create(&object, access, share, options) {
        Ok(handle) => return Ok(handle),
        Err(status) => status,
    };
    if dont_reparse && win_rules::may_be_unsupported_dont_reparse(status) {
        // Retry once without the flag; remember that it is unsupported only if that retry
        // succeeds (an unrelated STATUS_INVALID_PARAMETER must not clear it).
        return nt_open_without_dont_reparse(parent, &object_name, access, share, options)
            .inspect(|_| DONT_REPARSE.store(false, Ordering::Relaxed));
    }
    Err(status)
}

/// [`nt_open`] without `OBJ_DONT_REPARSE`, for Windows builds that reject the flag.
fn nt_open_without_dont_reparse(
    parent: &Handle,
    object_name: &UNICODE_STRING,
    access: u32,
    share: u32,
    options: u32,
) -> Result<Handle, NTSTATUS> {
    let object = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw(),
        ObjectName: object_name,
        Attributes: 0,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    nt_create(&object, access, share, options)
}

/// The `NtCreateFile` call shared by [`nt_open`] and [`nt_open_without_dont_reparse`].
fn nt_create(
    object: &OBJECT_ATTRIBUTES,
    access: u32,
    share: u32,
    options: u32,
) -> Result<Handle, NTSTATUS> {
    let mut io_status = IO_STATUS_BLOCK::default();
    let mut raw_handle: HANDLE = ptr::null_mut();
    // SAFETY: `raw_handle` and `io_status` are valid for writes; `object` is a fully
    // initialized OBJECT_ATTRIBUTES whose `ObjectName` points to a UNICODE_STRING (and its
    // buffer) that outlive the call, and whose `RootDirectory` is an open directory handle we
    // own. A null allocation size and EA buffer (length 0) are documented as allowed.
    // FILE_OPEN never creates anything; FILE_OPEN_REPARSE_POINT opens a reparse point itself
    // instead of its target; FILE_SYNCHRONOUS_IO_NONALERT (with SYNCHRONIZE access) makes every
    // later call on the handle synchronous.
    let status = unsafe {
        NtCreateFile(
            &mut raw_handle,
            access,
            object,
            &mut io_status,
            ptr::null(),
            0,
            share,
            FILE_OPEN,
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(status);
    }
    if raw_handle.is_null() || raw_handle == INVALID_HANDLE_VALUE {
        return Err(STATUS_UNSUCCESSFUL);
    }
    // SAFETY: NtCreateFile succeeded, so `raw_handle` is a new open handle that only we own;
    // `Handle` closes it exactly once.
    Ok(unsafe { Handle::from_raw(raw_handle) })
}

/// Opens `name` in `parent` for `purpose`, retrying a sharing violation briefly (never
/// forcing anything: a file still held after the retries is reported).
fn open(parent: &Handle, name: &[u16], purpose: Purpose) -> Result<Handle, NTSTATUS> {
    let (access, share, options) = purpose.params();
    let mut attempts = 0;
    loop {
        match nt_open(parent, name, access, share, options) {
            Err(s) if win_rules::is_sharing_violation(s) && attempts < SHARING_RETRIES => {
                attempts += 1;
                std::thread::sleep(SHARING_RETRY_DELAY);
            }
            other => return other,
        }
    }
}

/// The anchor, opened as given (a reparse point *at* the anchor is resolved, like a user-chosen
/// root; Unix opens it with a plain `open`).
fn open_anchor(path: &Path) -> io::Result<Handle> {
    let wide = verbatim(path)?;
    // SAFETY: `verbatim` returns a NUL-terminated UTF-16 path that outlives the call. A null
    // security-attributes pointer and a null template handle are documented as allowed.
    // OPEN_EXISTING never creates anything; FILE_FLAG_BACKUP_SEMANTICS is required to open a
    // directory. The access is traverse + attributes only.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW succeeded: a new open handle that only we own.
    Ok(unsafe { Handle::from_raw(handle) })
}

/// An NTSTATUS as an `io::Error` (through its Win32 error code).
fn nt_error(status: NTSTATUS) -> io::Error {
    // SAFETY: a pure conversion of a value; no pointers.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
}

// ---------------------------------------------------------------------------------------------
// Handle queries
// ---------------------------------------------------------------------------------------------

/// What an opened handle refers to.
struct Opened {
    id: FileIdentity,
    attributes: u32,
    /// The reparse tag, for reparse points.
    tag: Option<u32>,
    kind: EntryKind,
}

/// Attributes and reparse tag of the handle (one `FileAttributeTagInfo` query).
fn attr_tag(handle: &Handle) -> io::Result<(u32, Option<u32>)> {
    let info: FILE_ATTRIBUTE_TAG_INFO = query(handle)?;
    let attributes = info.FileAttributes;
    let tag = (attributes & raw::FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.ReparseTag);
    Ok((attributes, tag))
}

fn inspect(handle: &Handle) -> io::Result<Opened> {
    let (attributes, tag) = attr_tag(handle)?;
    let id = identity(handle).ok_or_else(no_file_id)?;
    Ok(Opened {
        id,
        attributes,
        tag,
        kind: kind_from(attributes, tag),
    })
}

fn no_file_id() -> io::Error {
    io::Error::other("the filesystem reports no stable file ID for this entry")
}

/// Logical and allocated size of a non-directory (allocated: on-disk bytes of compressed and
/// sparse files, like `Platform::metadata`). Zero when unreadable: the counts are a report,
/// never a decision.
fn sizes(handle: &Handle, attributes: u32) -> (u64, u64) {
    let Ok(standard) = query::<StandardInfo>(handle) else {
        return (0, 0);
    };
    let logical = u64::try_from(standard.end_of_file).unwrap_or(0);
    let mut allocated = u64::try_from(standard.allocation_size).unwrap_or(0);
    if attributes & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE) != 0
        && let Some(on_disk) = query::<FILE_COMPRESSION_INFO>(handle)
            .ok()
            .and_then(|c| u64::try_from(c.CompressedFileSize).ok())
    {
        allocated = on_disk;
    }
    (logical, allocated)
}

/// The volume of the entry `name` (tests may simulate a foreign volume by name).
#[cfg_attr(not(test), allow(unused_variables))]
fn volume_of(tuning: &Tuning, volume: u64, name: &[u16]) -> u64 {
    #[cfg(test)]
    if tuning.foreign_volume_name.as_deref() == Some(name) {
        return volume ^ 1;
    }
    volume
}

/// Whether the junction behind `handle` is a volume mount point (or anything the walker does
/// not recognize as a plain directory junction): `FSCTL_GET_REPARSE_POINT` on the handle.
/// Unreadable reparse data counts as a mount point (the protective answer).
#[cfg_attr(not(test), allow(unused_variables))]
fn is_volume_mount_point(tuning: &Tuning, handle: &Handle, name: &[u16]) -> bool {
    #[cfg(test)]
    if tuning.volume_mount_name.as_deref() == Some(name) {
        return true;
    }
    let mut buf = vec![0u8; raw::MAXIMUM_REPARSE_DATA_BUFFER_SIZE];
    let mut returned = 0u32;
    // SAFETY: `buf` is writable for `buf.len()` bytes and outlives the call; no input buffer
    // (null, length 0); `returned` is writable; no OVERLAPPED (the handle is synchronous).
    // FSCTL_GET_REPARSE_POINT only reads the reparse point's data (FILE_ANY_ACCESS).
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw(),
            raw::FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return true;
    }
    let n = (returned as usize).min(buf.len());
    win_rules::mount_point_target(&buf[..n]) == MountPointTarget::Volume
}

/// The path of an open handle (`\\?\C:\…`), for the Restart Manager query only.
fn final_path(handle: &Handle) -> Option<Vec<u16>> {
    let mut buf = vec![0u16; 512];
    for _ in 0..3 {
        let len = u32::try_from(buf.len()).ok()?;
        // SAFETY: `buf` is writable for `len` UTF-16 units; `handle` is open.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw(),
                buf.as_mut_ptr(),
                len,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            buf.truncate(n);
            return Some(buf);
        }
        // Too small: `n` is the size needed, including the NUL.
        buf.resize(n + 1, 0);
    }
    None
}

/// Marks the entry behind `handle` (opened with `DELETE` access) for deletion: POSIX semantics
/// where supported (the name disappears at once; `Ok(true)`), the classic disposition
/// otherwise (it disappears when the last handle is closed; `Ok(false)`). The read-only
/// attribute is never ignored. Use [`delete_and_close`], which also confirms a classic delete.
fn dispose(handle: &Handle) -> Result<bool, u32> {
    let ex = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: `ex` is a valid FILE_DISPOSITION_INFO_EX that outlives the call and the size is
    // its size; `handle` is open with DELETE access.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.as_raw(),
            FileDispositionInfoEx,
            ptr::from_ref(&ex).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    // SAFETY: reads the calling thread's last-error value; no arguments.
    let code = unsafe { GetLastError() };
    if !win_rules::disposition_ex_unsupported(code) {
        return Err(code);
    }
    let classic = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: as above, with a valid FILE_DISPOSITION_INFO.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle.as_raw(),
            FileDispositionInfo,
            ptr::from_ref(&classic).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok != 0 {
        Ok(false)
    } else {
        // SAFETY: as above.
        Err(unsafe { GetLastError() })
    }
}

/// What [`delete_and_close`] achieved.
enum Deleted {
    /// The entry is gone.
    Gone,
    /// Deleted by someone else first (or already being deleted).
    AlreadyGone,
    /// The delete did not take effect (yet): kept, for this reason.
    Kept(SkipReason, String),
}

/// Deletes the entry behind `handle` (the verified entry `id`, named `name` in `parent`) and
/// closes the handle. After a classic (non-POSIX) delete, which only takes effect when the last
/// handle closes, the name is probed again so that an entry another process keeps open (delete
/// pending) or that is otherwise still there is reported as kept, never counted as removed.
fn delete_and_close(handle: Handle, parent: &Handle, name: &[u16], id: FileIdentity) -> Deleted {
    let posix = match dispose(&handle) {
        Ok(posix) => posix,
        Err(code) if win_rules::is_gone(code) => return Deleted::AlreadyGone,
        Err(code) => {
            return Deleted::Kept(
                win_rules::skip_reason(code),
                io::Error::from_raw_os_error(code as i32).to_string(),
            );
        }
    };
    drop(handle);
    if posix {
        return Deleted::Gone;
    }
    match open(parent, name, Purpose::Probe) {
        Err(status) if status == raw::STATUS_DELETE_PENDING => Deleted::Kept(
            SkipReason::Busy,
            "delete pending: another process still has it open".into(),
        ),
        Err(status) if win_rules::is_not_found(status) => Deleted::Gone,
        Ok(probe) if identity(&probe) == Some(id) => Deleted::Kept(
            SkipReason::Io,
            "still present after its delete was requested".into(),
        ),
        // Another object at the name, or a probe failure: ours is no longer there.
        _ => Deleted::Gone,
    }
}

/// Whether the directory `id` is still the entry `name` of the held directory `parent`. Held
/// directories are pinned (no `FILE_SHARE_DELETE`), so this is defense in depth.
fn still_in_place(parent: &Handle, name: &[u16], id: FileIdentity) -> bool {
    open(parent, name, Purpose::Probe)
        .ok()
        .and_then(|h| identity(&h))
        .is_some_and(|found| found == id)
}

// ---------------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------------

/// One listed entry.
struct Listed {
    name: Vec<u16>,
    attributes: u32,
    tag: Option<u32>,
    /// The file ID the listing reported (`FileIdExtdDirectoryInfo`), when it has one.
    file_id: Option<u128>,
}

/// Enumeration buffer; records are 8-byte aligned.
#[repr(C, align(8))]
struct ListBuf([u8; LIST_BUF_LEN]);

/// Every entry of the directory behind `dir` (except `.` and `..`), read in full before
/// anything in it is removed. File IDs come with the listing where the filesystem supports
/// `FileIdExtdDirectoryInfo` (NTFS, ReFS); elsewhere (FAT) `FileFullDirectoryInfo` is used.
#[cfg_attr(not(test), allow(unused_variables))]
fn list(dir: &Handle, tuning: &Tuning) -> io::Result<Vec<Listed>> {
    #[cfg(test)]
    if tuning.force_full_listing {
        return list_with(dir, false);
    }
    match list_with(dir, true) {
        Err(e)
            if e.raw_os_error()
                .is_some_and(|c| win_rules::listing_class_unsupported(c as u32)) =>
        {
            list_with(dir, false)
        }
        other => other,
    }
}

fn list_with(dir: &Handle, with_ids: bool) -> io::Result<Vec<Listed>> {
    let mut buf = Box::new(ListBuf([0; LIST_BUF_LEN]));
    let mut out = Vec::new();
    let mut first = true;
    loop {
        let class = match (with_ids, first) {
            (true, true) => FileIdExtdDirectoryRestartInfo,
            (true, false) => FileIdExtdDirectoryInfo,
            (false, true) => FileFullDirectoryRestartInfo,
            (false, false) => FileFullDirectoryInfo,
        };
        // SAFETY: `buf` is an owned, 8-byte-aligned, writable buffer of exactly LIST_BUF_LEN
        // bytes that outlives the call; `dir` is an open, synchronous directory handle with
        // FILE_LIST_DIRECTORY access.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.as_raw(),
                class,
                buf.0.as_mut_ptr().cast(),
                LIST_BUF_LEN as u32,
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error().map(|c| c as u32) {
                Some(raw::ERROR_NO_MORE_FILES) => break,
                // A first query that matches nothing (an empty volume root has no `.`/`..`).
                Some(raw::ERROR_FILE_NOT_FOUND) if first => break,
                _ => return Err(err),
            }
        }
        first = false;
        let mut offset = 0usize;
        loop {
            let (next, listed) = if with_ids {
                let r = win_rules::parse_extd_record(&buf.0, offset)?;
                let tag = (r.attributes & raw::FILE_ATTRIBUTE_REPARSE_POINT != 0)
                    .then_some(r.reparse_tag);
                (
                    r.next_entry_offset,
                    Listed {
                        name: r.name,
                        attributes: r.attributes,
                        tag,
                        file_id: win_rules::usable_file_id(r.file_id),
                    },
                )
            } else {
                let r = parse_full_record(&buf.0, offset)?;
                // `EaSize` holds the reparse tag for reparse points ([MS-FSCC] 2.4.14).
                let tag = (r.attributes & raw::FILE_ATTRIBUTE_REPARSE_POINT != 0)
                    .then_some(r.ea_size_or_tag);
                (
                    r.next_entry_offset,
                    Listed {
                        name: r.name.encode_wide().collect(),
                        attributes: r.attributes,
                        tag,
                        file_id: None,
                    },
                )
            };
            if !win_rules::is_dot_or_dotdot(&listed.name) {
                out.push(listed);
            }
            if next == 0 {
                break;
            }
            offset = offset
                .checked_add(next as usize)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "listing overflow"))?;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------------------------

/// One directory on the walk stack. Its handle stays open (pinned) until it is removed.
struct Frame {
    /// Its name in its parent.
    name: Vec<u16>,
    /// Volume + file ID, as verified on the opened handle.
    id: FileIdentity,
    /// The pinning handle (list + traverse access, no `FILE_SHARE_DELETE`).
    handle: Handle,
    /// Entries not processed yet.
    pending: Vec<Listed>,
    /// Something below was kept, so this directory is kept too.
    incomplete: bool,
    /// Entries processed since its place was last verified; `None`: verify before the next.
    since_verified: Option<usize>,
}

/// What happened to one entry.
enum Step {
    /// Deleted.
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
    /// The target's volume serial number.
    root_volume: u64,
    observer: Option<&'o mut dyn FnMut(&WalkEvent<'_>)>,
    tuning: &'t Tuning,
    /// Restart Manager queries left.
    rm_budget: usize,
}

type Abort = (PathBuf, Anomaly);

impl Walk<'_, '_> {
    fn observe(&mut self, event: WalkEvent<'_>) {
        if let Some(observer) = self.observer.as_mut() {
            observer(&event);
        }
    }

    /// Reports the current entry (`self.rel`) to the observer, if any.
    fn observe_current(&mut self, make: impl FnOnce(&Path) -> WalkEvent<'_>) {
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

    fn keep_io(&mut self, kind: Option<EntryKind>, e: &io::Error) {
        let reason = e
            .raw_os_error()
            .map_or(SkipReason::Io, |c| win_rules::skip_reason(c as u32));
        self.keep(kind, reason, e.to_string());
    }

    /// Keeps entries the walker never removes: cloud placeholders and recall-on-access entries
    /// (PLT-04), the read-only attribute (never cleared or ignored), device entries (SAFE-18).
    fn keep_protected(
        &mut self,
        kind: EntryKind,
        attributes: u32,
        tag: Option<u32>,
    ) -> Option<Step> {
        let (reason, detail) = if win_rules::is_cloud_placeholder(attributes, tag) {
            (
                SkipReason::CloudPlaceholder,
                "cloud-sync placeholder or recall-on-access entry: never opened, hydrated or \
                 removed (PLT-04)",
            )
        } else if win_rules::is_read_only(attributes) {
            (
                SkipReason::ReadOnlyAttribute,
                "the read-only attribute is set; scull never clears or ignores it",
            )
        } else if kind == EntryKind::Other {
            (SkipReason::Io, "device entry: never opened (SAFE-18)")
        } else {
            return None;
        };
        self.keep(Some(kind), reason, detail.to_owned());
        Some(Step::Kept)
    }

    /// The processes holding the locked file `name` in `dir`, from Restart Manager, as text
    /// appended to the skip detail (FX-WIN-03). The path is resolved from the held handle for
    /// this query only; nothing is ever deleted by path.
    fn holders(&mut self, dir: &Handle, name: &[u16]) -> String {
        if self.rm_budget == 0 {
            return "; holder not looked up (Restart Manager query limit reached)".into();
        }
        self.rm_budget -= 1;
        let Some(dir_path) = final_path(dir) else {
            return "; holder unknown (the directory's path is unavailable)".into();
        };
        let mut full = dir_path;
        full.push(u16::from(b'\\'));
        full.extend_from_slice(name);
        let path = PathBuf::from(OsString::from_wide(&win_rules::plain_form(&full)));
        match crate::activity::processes_holding(&path) {
            Ok(hits) if hits.is_empty() => {
                "; Restart Manager names no process (a service or another session may hold it)"
                    .into()
            }
            Ok(hits) => {
                let names: Vec<String> = hits
                    .iter()
                    .map(|h| match &h.name {
                        Some(n) => format!("{n} (pid {})", h.pid),
                        None => format!("pid {}", h.pid),
                    })
                    .collect();
                format!("; held open by {}", names.join(", "))
            }
            Err(e) => format!("; Restart Manager: {e}"),
        }
    }

    fn count_removed(&mut self, kind: EntryKind, logical: u64, allocated: u64) {
        let r = &mut self.report.removed;
        match kind {
            EntryKind::File => r.files += 1,
            EntryKind::Symlink | EntryKind::Junction | EntryKind::ReparsePoint => r.symlinks += 1,
            _ => r.special += 1,
        }
        r.logical_bytes = r.logical_bytes.saturating_add(logical);
        r.allocated_bytes = r.allocated_bytes.saturating_add(allocated);
    }

    /// A failed open of the entry `name` (listed as `kind`) in `dir`.
    fn open_failed(
        &mut self,
        dir: &Handle,
        name: &[u16],
        kind: EntryKind,
        status: NTSTATUS,
    ) -> Result<Step, Abort> {
        match win_rules::open_failure(status) {
            OpenFailure::Vanished => Ok(Step::Vanished),
            // A directory became a file or a link (or the reverse) since the listing: never
            // followed, and the walk stops.
            OpenFailure::KindChanged => Err(self.abort(Anomaly::KindChanged {
                expected: kind,
                found: None,
            })),
            OpenFailure::Skip(reason) => {
                let mut detail = nt_error(status).to_string();
                if reason == SkipReason::Busy && kind != EntryKind::Dir {
                    detail.push_str(&self.holders(dir, name));
                }
                self.keep(Some(kind), reason, detail);
                Ok(Step::Kept)
            }
        }
    }

    /// Handles one listed entry of the held directory `dir`; `self.rel` already names it.
    fn entry(&mut self, dir: &Handle, listed: &Listed, depth: usize) -> Result<Step, Abort> {
        let listed_kind = kind_from(listed.attributes, listed.tag);
        if let Some(step) = self.keep_protected(listed_kind, listed.attributes, listed.tag) {
            return Ok(step);
        }
        if listed_kind == EntryKind::Dir && depth > self.tuning.max_depth {
            self.keep(
                Some(listed_kind),
                SkipReason::TooDeep,
                format!("deeper than {} levels", self.tuning.max_depth),
            );
            return Ok(Step::Kept);
        }

        let handle = match open(dir, &listed.name, purpose_for(listed_kind)) {
            Ok(h) => h,
            Err(status) => return self.open_failed(dir, &listed.name, listed_kind, status),
        };
        let opened = match inspect(&handle) {
            Ok(o) => o,
            Err(e) => {
                self.keep_io(Some(listed_kind), &e);
                return Ok(Step::Kept);
            }
        };
        let found_volume = volume_of(self.tuning, opened.id.volume, &listed.name);
        if found_volume != self.root_volume {
            return Err(self.abort(Anomaly::MountCrossing {
                root_device: self.root_volume,
                found_device: found_volume,
            }));
        }
        if opened.kind != listed_kind {
            // Replaced since the listing (e.g. a directory swapped for a junction, which the
            // no-follow open returned as itself): stop.
            return Err(self.abort(Anomaly::KindChanged {
                expected: listed_kind,
                found: Some(opened.kind),
            }));
        }
        if let Some(listed_id) = listed.file_id
            && listed_id != opened.id.file
        {
            return Err(self.abort(Anomaly::IdentityChanged {
                expected: FileIdentity::new(self.root_volume, listed_id),
                found: opened.id,
            }));
        }
        // Attributes may have changed since the listing: decide on the opened handle.
        if let Some(step) = self.keep_protected(opened.kind, opened.attributes, opened.tag) {
            return Ok(step);
        }
        if opened.kind == EntryKind::Junction
            && is_volume_mount_point(self.tuning, &handle, &listed.name)
        {
            return Err(self.abort(Anomaly::VolumeMountPoint));
        }
        self.observe_current(|relative| WalkEvent::EntryChecked {
            relative,
            kind: opened.kind,
        });

        if opened.kind != EntryKind::Dir {
            // Files, symlinks, junctions and other reparse points: deleted through the handle
            // just verified, as themselves (a junction's target is never reached).
            let (logical, allocated) = sizes(&handle, opened.attributes);
            return match delete_and_close(handle, dir, &listed.name, opened.id) {
                Deleted::Gone => {
                    self.count_removed(opened.kind, logical, allocated);
                    Ok(Step::Removed)
                }
                Deleted::AlreadyGone => Ok(Step::Vanished),
                Deleted::Kept(reason, detail) => {
                    self.keep(Some(opened.kind), reason, detail);
                    Ok(Step::Kept)
                }
            };
        }

        self.observe_current(|relative| WalkEvent::DirOpened { relative, depth });
        match list(&handle, self.tuning) {
            Ok(pending) => {
                self.observe_current(|relative| WalkEvent::DirListed { relative, depth });
                Ok(Step::Descend(Frame {
                    name: listed.name.clone(),
                    id: opened.id,
                    handle,
                    pending,
                    incomplete: false,
                    since_verified: None,
                }))
            }
            Err(e) => {
                self.keep_io(Some(EntryKind::Dir), &e);
                Ok(Step::Kept)
            }
        }
    }

    /// Deletes the emptied directory `done`: its pinning handle is closed (our own share mode
    /// would refuse a `DELETE` open), the name is re-opened in `parent` with delete access, and
    /// the new handle must be the same real directory before it is deleted through it.
    /// Returns whether it was removed.
    fn remove_dir(&mut self, parent: &Handle, done: Frame) -> Result<bool, Abort> {
        let Frame {
            name, id, handle, ..
        } = done;
        drop(handle);
        let handle = match open(parent, &name, Purpose::DirDelete) {
            Ok(h) => h,
            Err(status) => {
                return match win_rules::open_failure(status) {
                    // We held it a moment ago: it was moved or removed behind our back. Stop.
                    OpenFailure::Vanished => Err(self.abort(Anomaly::DirectoryMoved)),
                    OpenFailure::KindChanged => Err(self.abort(Anomaly::KindChanged {
                        expected: EntryKind::Dir,
                        found: None,
                    })),
                    OpenFailure::Skip(reason) => {
                        self.keep(Some(EntryKind::Dir), reason, nt_error(status).to_string());
                        Ok(false)
                    }
                };
            }
        };
        let opened = match inspect(&handle) {
            Ok(o) => o,
            Err(e) => {
                self.keep_io(Some(EntryKind::Dir), &e);
                return Ok(false);
            }
        };
        if opened.id != id {
            return Err(self.abort(Anomaly::IdentityChanged {
                expected: id,
                found: opened.id,
            }));
        }
        if opened.kind != EntryKind::Dir {
            return Err(self.abort(Anomaly::KindChanged {
                expected: EntryKind::Dir,
                found: Some(opened.kind),
            }));
        }
        match delete_and_close(handle, parent, &name, id) {
            Deleted::Gone => {
                self.report.removed.dirs += 1;
                Ok(true)
            }
            // Identity-checked a moment ago, then deleted by someone else: gone either way.
            Deleted::AlreadyGone => {
                self.report.vanished += 1;
                Ok(true)
            }
            Deleted::Kept(reason, detail) => {
                self.keep(Some(EntryKind::Dir), reason, detail);
                Ok(false)
            }
        }
    }
}

pub(super) fn remove_tree(
    req: &RemoveRequest<'_>,
    observer: Option<&mut dyn FnMut(&WalkEvent<'_>)>,
    tuning: &Tuning,
) -> Result<RemoveReport, RemoveError> {
    let names = plain_components(req.relative)?
        .into_iter()
        .map(|n| {
            let wide: Vec<u16> = n.encode_wide().collect();
            win_rules::validate_name(&wide)
                .map(|()| wide)
                .map_err(|reason| RemoveError::InvalidRequest { reason })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some((target, intermediate)) = names.split_last() else {
        return Err(RemoveError::InvalidRequest {
            reason: "the relative path is empty",
        });
    };

    // 1. The anchor (trusted, opened as given), then every component below it no-follow,
    //    relative to the previous handle.
    let anchor_unavailable = |source| RemoveError::AnchorUnavailable {
        anchor: req.anchor.to_path_buf(),
        source,
    };
    let mut parent = open_anchor(req.anchor).map_err(anchor_unavailable)?;
    let (anchor_attributes, _) = attr_tag(&parent).map_err(anchor_unavailable)?;
    if anchor_attributes & raw::FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(anchor_unavailable(io::Error::new(
            io::ErrorKind::NotADirectory,
            "the anchor is not a directory",
        )));
    }
    if let Some(expected) = req.anchor_identity {
        let found = identity(&parent).ok_or_else(|| anchor_unavailable(no_file_id()))?;
        if found != expected {
            return Err(RemoveError::AnchorChanged { expected, found });
        }
    }
    let mut walked = PathBuf::new();
    for name in intermediate {
        walked.push(OsString::from_wide(name));
        let not_traversable = |source| RemoveError::PathNotTraversable {
            component: walked.clone(),
            source,
        };
        let next =
            open(&parent, name, Purpose::Traverse).map_err(|s| not_traversable(nt_error(s)))?;
        // FILE_OPEN_REPARSE_POINT opened a junction or directory symlink as itself: refused,
        // never traversed.
        let (attributes, _) = attr_tag(&next).map_err(not_traversable)?;
        if attributes & raw::FILE_ATTRIBUTE_REPARSE_POINT != 0
            || attributes & raw::FILE_ATTRIBUTE_DIRECTORY == 0
        {
            return Err(not_traversable(io::Error::other(
                "a junction, symbolic link or other reparse point is never traversed",
            )));
        }
        parent = next;
    }

    // 2. The target: attributes-only open, identity, volume, kind (SAFE-05).
    let probe = match open(&parent, target, Purpose::Probe) {
        Ok(h) => h,
        Err(s) if win_rules::is_not_found(s) => return Err(RemoveError::TargetMissing),
        Err(s) if win_rules::is_invalid_name(s) => {
            return Err(RemoveError::InvalidRequest {
                reason: "the filesystem refused a name in the relative path",
            });
        }
        Err(s) => {
            return Err(RemoveError::TargetUnavailable {
                source: nt_error(s),
            });
        }
    };
    let info = inspect(&probe).map_err(|source| RemoveError::TargetUnavailable { source })?;
    if info.id != req.expected {
        return Err(RemoveError::IdentityMismatch {
            expected: req.expected,
            found: info.id,
        });
    }
    let parent_id = identity(&parent).ok_or_else(|| RemoveError::TargetUnavailable {
        source: no_file_id(),
    })?;
    // A target on another volume than its directory, or a volume mount point, is never
    // emptied or removed (PRD §8.3: crossing a mount boundary is S3).
    let target_volume = volume_of(tuning, info.id.volume, target);
    if parent_id.volume != target_volume
        || (info.kind == EntryKind::Junction && is_volume_mount_point(tuning, &probe, target))
    {
        return Err(RemoveError::TargetIsMountPoint {
            parent_device: parent_id.volume,
            target_device: target_volume,
            parent_mount_id: None,
            target_mount_id: None,
        });
    }
    if let Some(expected) = req.expected_kind
        && expected != info.kind
    {
        return Err(RemoveError::KindMismatch {
            expected,
            found: info.kind,
        });
    }
    // A single non-directory with other hard links: removing this name frees nothing and the
    // other name may be the user's. Refused.
    if info.kind != EntryKind::Dir {
        let standard = query::<StandardInfo>(&probe)
            .map_err(|source| RemoveError::TargetUnavailable { source })?;
        let nlink = u64::from(standard.number_of_links);
        if nlink > 1 {
            return Err(RemoveError::HardLinked { nlink });
        }
    }
    drop(probe);

    let mut walk = Walk {
        report: RemoveReport::new(),
        rel: PathBuf::new(),
        root_volume: info.id.volume,
        observer,
        tuning,
        rm_budget: RESTART_MANAGER_QUERIES,
    };
    if walk
        .keep_protected(info.kind, info.attributes, info.tag)
        .is_some()
    {
        return Ok(walk.report);
    }
    walk.observe(WalkEvent::EntryChecked {
        relative: Path::new(""),
        kind: info.kind,
    });

    // A single non-directory: re-open it for deletion, check the new handle is still the
    // planned object, delete through it (a link is removed as the link only).
    if info.kind != EntryKind::Dir {
        let handle = match open(&parent, target, purpose_for(info.kind)) {
            Ok(h) => h,
            Err(s) => match win_rules::open_failure(s) {
                OpenFailure::Vanished => return Err(RemoveError::TargetMissing),
                OpenFailure::KindChanged => {
                    return Err(RemoveError::TargetChanged {
                        anomaly: Anomaly::KindChanged {
                            expected: info.kind,
                            found: None,
                        },
                    });
                }
                OpenFailure::Skip(reason) => {
                    let mut detail = nt_error(s).to_string();
                    if reason == SkipReason::Busy {
                        detail.push_str(&walk.holders(&parent, target));
                    }
                    walk.keep(Some(info.kind), reason, detail);
                    return Ok(walk.report);
                }
            },
        };
        let now = inspect(&handle).map_err(|source| RemoveError::TargetUnavailable { source })?;
        if now.id != req.expected {
            return Err(RemoveError::TargetChanged {
                anomaly: Anomaly::IdentityChanged {
                    expected: req.expected,
                    found: now.id,
                },
            });
        }
        if now.kind != info.kind {
            return Err(RemoveError::TargetChanged {
                anomaly: Anomaly::KindChanged {
                    expected: info.kind,
                    found: Some(now.kind),
                },
            });
        }
        if walk
            .keep_protected(now.kind, now.attributes, now.tag)
            .is_some()
        {
            return Ok(walk.report);
        }
        let (logical, allocated) = sizes(&handle, now.attributes);
        match delete_and_close(handle, &parent, target, req.expected) {
            Deleted::Gone => {
                walk.count_removed(now.kind, logical, allocated);
                walk.report.outcome = RemoveOutcome::Complete;
            }
            Deleted::AlreadyGone => walk.report.vanished += 1,
            Deleted::Kept(reason, detail) => walk.keep(Some(now.kind), reason, detail),
        }
        return Ok(walk.report);
    }

    // 3. A directory: open it pinned (no FILE_SHARE_DELETE), verify the handle, walk.
    let root = match open(&parent, target, Purpose::Dir) {
        Ok(h) => h,
        Err(s) => {
            return Err(match win_rules::open_failure(s) {
                OpenFailure::Vanished => RemoveError::TargetMissing,
                OpenFailure::KindChanged => RemoveError::TargetChanged {
                    anomaly: Anomaly::KindChanged {
                        expected: EntryKind::Dir,
                        found: None,
                    },
                },
                OpenFailure::Skip(_) => RemoveError::TargetUnavailable {
                    source: nt_error(s),
                },
            });
        }
    };
    let opened = inspect(&root).map_err(|source| RemoveError::TargetUnavailable { source })?;
    if opened.id != req.expected {
        return Err(RemoveError::TargetChanged {
            anomaly: Anomaly::IdentityChanged {
                expected: req.expected,
                found: opened.id,
            },
        });
    }
    if opened.kind != EntryKind::Dir {
        return Err(RemoveError::TargetChanged {
            anomaly: Anomaly::KindChanged {
                expected: EntryKind::Dir,
                found: Some(opened.kind),
            },
        });
    }
    if walk
        .keep_protected(opened.kind, opened.attributes, opened.tag)
        .is_some()
    {
        return Ok(walk.report);
    }
    walk.observe(WalkEvent::DirOpened {
        relative: Path::new(""),
        depth: 0,
    });
    let pending =
        list(&root, tuning).map_err(|source| RemoveError::TargetUnavailable { source })?;
    walk.observe(WalkEvent::DirListed {
        relative: Path::new(""),
        depth: 0,
    });

    let frames = vec![Frame {
        name: target.clone(),
        id: req.expected,
        handle: root,
        pending,
        incomplete: false,
        since_verified: None,
    }];
    if let Err((relative, anomaly)) = run(&mut walk, frames, &parent) {
        walk.report.outcome = RemoveOutcome::Aborted;
        walk.report.aborted = Some(AbortReason { relative, anomaly });
    }
    Ok(walk.report)
}

/// The iterative walk. `frames[0]` is the target; `target_parent` holds it. Every frame's
/// handle stays open until the frame is removed.
fn run(
    walk: &mut Walk<'_, '_>,
    mut frames: Vec<Frame>,
    target_parent: &Handle,
) -> Result<(), Abort> {
    let verify_every = walk.tuning.verify_every.max(1);
    loop {
        let depth = frames.len();
        let Some((top, rest)) = frames.split_last_mut() else {
            return Ok(());
        };

        if let Some(listed) = top.pending.pop() {
            // Before the first removal in a directory, after coming back from a
            // sub-directory, and every `verify_every` entries: is it still where it was?
            // (Held directories are pinned; this is defense in depth.)
            let due = top.since_verified.is_none_or(|n| n >= verify_every);
            if due {
                let parent = rest.last().map_or(target_parent, |f| &f.handle);
                if !still_in_place(parent, &top.name, top.id) {
                    return Err(walk.abort(Anomaly::DirectoryMoved));
                }
                top.since_verified = Some(0);
            }
            walk.rel.push(OsString::from_wide(&listed.name));
            let step = walk.entry(&top.handle, &listed, depth)?;
            top.since_verified = top.since_verified.map(|n| n + 1);
            match step {
                Step::Descend(frame) => {
                    // `walk.rel` keeps the directory's name until it is popped.
                    frames.push(frame);
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
        let removed = if done.incomplete {
            drop(done);
            false
        } else {
            let parent = frames.last().map_or(target_parent, |f| &f.handle);
            walk.remove_dir(parent, done)?
        };
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
