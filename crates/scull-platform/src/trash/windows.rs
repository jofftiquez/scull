//! Windows: the volume's Recycle Bin, `<volume>\$Recycle.Bin\<user SID>\`. The item's handle is
//! opened without following reparse points, verified by identity, and renamed into the bin as
//! `$R<id><ext>` (`SetFileInformationByHandle(FileRenameInfo)`, never replacing), after its
//! `$I<id><ext>` record (format 2) was created with `CREATE_NEW`. See the parent module for why
//! this is not `SHFileOperationW` / `IFileOperation` (path-based, and they permanently delete
//! what the bin can't take).
//!
//! # Residual window
//!
//! The *source* has none: the rename acts on the handle whose identity was checked, so the
//! object moved is the planned one (a link is moved as the link itself). The *destination* is
//! a path inside `$Recycle.Bin\<SID>`; that directory is checked (a real directory, not a
//! reparse point, on the item's volume) just before the rename, and only its owner (and
//! administrators) can replace it.

#![allow(unsafe_code)]

use super::format::{self, MAX_NAME_ATTEMPTS};
use super::{TrashError, TrashKind, TrashLocations, TrashOutcome, TrashRequest, measure};
use crate::EntryKind;
use scull_core::{FileIdentity, Restorability};
use std::ffi::{OsStr, OsString, c_void};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem::{MaybeUninit, offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_NOT_SAME_DEVICE, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FileIdInfo,
    FileRenameInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, GetVolumePathNameW,
    SYNCHRONIZE, SetFileInformationByHandle,
};
use windows_sys::Win32::System::SystemServices::{
    IO_REPARSE_TAG_AF_UNIX, IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn unavailable(reason: impl Into<String>) -> TrashError {
    TrashError::Unavailable {
        reason: reason.into(),
    }
}

/// Opens `path` with `access` only. `follow == false`: a reparse point at the final component
/// is opened itself (`FILE_FLAG_OPEN_REPARSE_POINT`), never its target. Directories open too
/// (`FILE_FLAG_BACKUP_SEMANTICS`). Nothing is created.
fn open(path: &Path, access: u32, follow: bool) -> io::Result<File> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if !follow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(flags)
        .open(path)
}

/// A fixed-size output of `GetFileInformationByHandleEx`, zero-initialized then filled.
///
/// # Safety
///
/// `T` must be the structure Windows writes for `class`, made of plain integers only.
unsafe fn query<T: Copy>(file: &File, class: i32) -> io::Result<T> {
    let mut out = MaybeUninit::<T>::zeroed();
    let size = u32::try_from(size_of::<T>()).map_err(io::Error::other)?;
    // SAFETY: `out` is valid for writes of `size` bytes and aligned for `T`; the caller
    // guarantees `T` matches `class`. The handle is open for the call's duration.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            class,
            out.as_mut_ptr().cast(),
            size,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zero-initialized and filled by the successful call; integers only.
    Ok(unsafe { out.assume_init() })
}

/// Volume serial number + file ID, exactly as `Platform::metadata` reports it.
fn identity(file: &File) -> Option<FileIdentity> {
    // SAFETY: FILE_ID_INFO (a u64 and a [u8; 16]) is the documented output of FileIdInfo.
    if let Ok(info) = unsafe { query::<FILE_ID_INFO>(file, FileIdInfo) } {
        let id = u128::from_le_bytes(info.FileId.Identifier);
        if id != 0 && id != u128::MAX {
            return Some(FileIdentity::new(info.VolumeSerialNumber, id));
        }
    }
    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `info` is a valid, aligned, writable BY_HANDLE_FILE_INFORMATION; the handle is
    // open.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, info.as_mut_ptr()) } == 0
    {
        return None;
    }
    // SAFETY: zero-initialized and filled by the successful call; integers only.
    let info = unsafe { info.assume_init() };
    let id = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    (id != 0 && id != u64::MAX)
        .then(|| FileIdentity::new(u64::from(info.dwVolumeSerialNumber), u128::from(id)))
}

/// The kind of the opened entry (attributes and reparse tag), as `Platform::metadata` names it.
fn kind(file: &File) -> io::Result<EntryKind> {
    // SAFETY: FILE_ATTRIBUTE_TAG_INFO (two u32) is the documented output of
    // FileAttributeTagInfo.
    let info = unsafe { query::<FILE_ATTRIBUTE_TAG_INFO>(file, FileAttributeTagInfo) }?;
    let attributes = info.FileAttributes;
    Ok(if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        match info.ReparseTag {
            IO_REPARSE_TAG_SYMLINK => EntryKind::Symlink,
            IO_REPARSE_TAG_MOUNT_POINT => EntryKind::Junction,
            IO_REPARSE_TAG_AF_UNIX => EntryKind::Socket,
            _ => EntryKind::ReparsePoint,
        }
    } else if attributes & FILE_ATTRIBUTE_DEVICE != 0 {
        EntryKind::Other
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        EntryKind::Dir
    } else {
        EntryKind::File
    })
}

/// `path` in verbatim (`\\?\`) form, for APIs that take a full path without length limits.
fn verbatim(path: &Path) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::new();
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(_) => {
                out.extend(r"\\?\".encode_utf16());
                out.extend(p.as_os_str().encode_wide());
            }
            Prefix::UNC(server, share) => {
                out.extend(r"\\?\UNC\".encode_utf16());
                out.extend(server.encode_wide());
                out.push(u16::from(b'\\'));
                out.extend(share.encode_wide());
            }
            _ => return path.as_os_str().encode_wide().collect(),
        },
        _ => return path.as_os_str().encode_wide().collect(),
    }
    for c in components {
        if let Component::Normal(name) = c {
            out.push(u16::from(b'\\'));
            out.extend(name.encode_wide());
        }
    }
    out
}

/// `path` as Explorer shows it (no `\\?\` prefix), for the `$I` record.
fn display_wide(path: &Path) -> Vec<u16> {
    let s = path.as_os_str().encode_wide().collect::<Vec<u16>>();
    let text = String::from_utf16_lossy(&s);
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        let mut out: Vec<u16> = r"\\".encode_utf16().collect();
        out.extend(OsStr::new(rest).encode_wide());
        return out;
    }
    if text.starts_with(r"\\?\") {
        return s[4..].to_vec();
    }
    s
}

/// The root of the volume holding `path` (`C:\`, or a mounted folder).
fn volume_root(path: &Path) -> io::Result<PathBuf> {
    let mut wide = verbatim(path);
    wide.push(0);
    let mut buf = vec![0u16; 32 * 1024];
    let len = u32::try_from(buf.len()).map_err(io::Error::other)?;
    // SAFETY: `wide` is NUL-terminated and outlives the call; `buf` is writable for `len` u16s.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), buf.as_mut_ptr(), len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(OsString::from_wide(&buf[..end])))
}

/// An owned kernel handle (the process token), closed on drop.
struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a token handle from `OpenProcessToken`, owned exclusively here
        // and closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The SID string (`S-1-5-21-…`) of the user this process runs as (the token user), which
/// names the user's folder in every `$Recycle.Bin`.
fn current_user_sid() -> io::Result<String> {
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns an always-valid pseudo-handle; `raw` is a valid
    // out-pointer. The token handle is owned by `Token` and closed on drop.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = Token(raw);
    let mut len = 0u32;
    // SAFETY: a null buffer of length 0 queries the required size (fails by design).
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut len) };
    if (len as usize) < size_of::<TOKEN_USER>() {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buf` is writable for at least `len` bytes and 8-byte aligned (the structure
    // holds a pointer); the token is open with TOKEN_QUERY.
    if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful query wrote a TOKEN_USER at the start of `buf` (large enough,
    // checked above, and aligned); its `Sid` points into `buf`, alive until the end of scope.
    let sid = unsafe { buf.as_ptr().cast::<TOKEN_USER>().read() }.User.Sid;
    let mut out: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is valid (see above); `out` is a valid out-pointer. On success the string
    // is allocated with LocalAlloc and freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut n = 0;
    // SAFETY: on success `out` is a non-null, NUL-terminated wide string; every index up to
    // the terminator is in bounds.
    while unsafe { *out.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` u16s starting at `out` were just read one by one.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(out, n) });
    // SAFETY: `out` was allocated by ConvertSidToStringSidW with LocalAlloc; freed once.
    unsafe { LocalFree(out.cast::<c_void>()) };
    Ok(s)
}

/// Checks that `path` is a real directory (not a reparse point) on volume `volume`.
fn check_dir(path: &Path, volume: u64) -> Result<(), TrashError> {
    let dir = open(path, FILE_READ_ATTRIBUTES, false)
        .map_err(|e| unavailable(format!("{}: {e}", path.display())))?;
    match kind(&dir) {
        Ok(EntryKind::Dir) => {}
        Ok(other) => {
            return Err(unavailable(format!(
                "{} is a {other}, not a real directory; not used",
                path.display()
            )));
        }
        Err(e) => return Err(unavailable(format!("{}: {e}", path.display()))),
    }
    match identity(&dir) {
        Some(id) if id.volume == volume => Ok(()),
        Some(id) => Err(TrashError::CrossVolume {
            item_volume: volume,
            trash_volume: id.volume,
        }),
        None => Err(unavailable(format!(
            "{}: no file identity on this filesystem",
            path.display()
        ))),
    }
}

/// The user's Recycle Bin folder on the item's volume: `$Recycle.Bin` must exist there (the
/// shell creates it; a volume without one has no Recycle Bin), and the user's `<SID>` folder
/// in it is created owner-only when missing.
fn recycle_bin(locations: &TrashLocations, abs: &Path, volume: u64) -> Result<PathBuf, TrashError> {
    let root = match &locations.topdir {
        Some(t) => t.clone(),
        None => volume_root(abs)
            .map_err(|e| unavailable(format!("cannot find the item's volume: {e}")))?,
    };
    let bin_root = root.join("$Recycle.Bin");
    if std::fs::symlink_metadata(&bin_root).is_err() {
        return Err(unavailable(format!(
            "this volume has no Recycle Bin ({} does not exist)",
            bin_root.display()
        )));
    }
    check_dir(&bin_root, volume)?;
    let sid = current_user_sid()
        .map_err(|e| unavailable(format!("cannot determine the user's SID: {e}")))?;
    let bin = bin_root.join(&sid);
    if std::fs::symlink_metadata(&bin).is_err() {
        match crate::owner_only::create_dir(&bin) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(unavailable(format!("cannot create {}: {e}", bin.display())));
            }
        }
    }
    check_dir(&bin, volume)?;
    Ok(bin)
}

/// Renames the entry behind `file` to `dest` (a full path), never replacing an existing entry.
fn rename_handle(file: &File, dest: &Path) -> io::Result<()> {
    let name = verbatim(dest);
    let name_bytes = name.len() * size_of::<u16>();
    let offset = offset_of!(FILE_RENAME_INFO, FileName);
    let size = (offset + name_bytes + size_of::<u16>()).max(size_of::<FILE_RENAME_INFO>());
    // 8-byte aligned and zeroed: ReplaceIfExists = FALSE, RootDirectory = NULL, NUL-terminated.
    let mut buf = vec![0u64; size.div_ceil(size_of::<u64>())];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buf` is at least `size` bytes, aligned for FILE_RENAME_INFO, and the name (plus
    // its terminator) fits after `FileName`'s offset by construction. Raw-pointer writes only;
    // no reference to the variable-length tail is created.
    unsafe {
        (*info).FileNameLength = u32::try_from(name_bytes).map_err(io::Error::other)?;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            buf.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
    }
    let len = u32::try_from(size).map_err(io::Error::other)?;
    // SAFETY: the handle is open with DELETE access; `buf` holds a valid FILE_RENAME_INFO of
    // `len` bytes for the duration of the call.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileRenameInfo,
            buf.as_ptr().cast(),
            len,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn is_os_error(e: &io::Error, code: u32) -> bool {
    e.raw_os_error() == i32::try_from(code).ok()
}

pub(super) fn move_to_trash(req: &TrashRequest<'_>) -> Result<TrashOutcome, TrashError> {
    let names = super::plain_components(req.relative)?;

    // 1. The anchor (trusted, opened as given).
    let anchor = open(req.anchor, FILE_READ_ATTRIBUTES, true).map_err(|source| {
        TrashError::AnchorUnavailable {
            anchor: req.anchor.to_path_buf(),
            source,
        }
    })?;
    if let Some(expected) = req.anchor_identity {
        let found = identity(&anchor).unwrap_or(FileIdentity::new(0, 0));
        if found != expected {
            return Err(TrashError::AnchorChanged { expected, found });
        }
    }

    // 2. Every component between the anchor and the item must be a real directory, never a
    //    junction or symlink. (The item handle below is verified by identity regardless.)
    let mut walked = PathBuf::new();
    let mut here = req.anchor.to_path_buf();
    for name in &names[..names.len() - 1] {
        walked.push(name);
        here.push(name);
        let not_traversable = |source| TrashError::PathNotTraversable {
            component: walked.clone(),
            source,
        };
        let dir = open(&here, FILE_READ_ATTRIBUTES, false).map_err(not_traversable)?;
        match kind(&dir) {
            Ok(EntryKind::Dir) => {}
            Ok(other) => {
                return Err(not_traversable(io::Error::other(format!(
                    "is a {other}; never followed"
                ))));
            }
            Err(e) => return Err(not_traversable(e)),
        }
    }

    // 3. The item, opened no-follow with DELETE access, verified by identity and kind.
    let abs = req.anchor.join(req.relative);
    let item = open(&abs, DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, false).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            TrashError::TargetMissing
        } else {
            TrashError::TargetUnavailable { source: e }
        }
    })?;
    let found = identity(&item).ok_or_else(|| TrashError::TargetUnavailable {
        source: io::Error::other("no file identity on this filesystem"),
    })?;
    if found != req.expected {
        return Err(TrashError::IdentityChanged {
            expected: req.expected,
            found,
        });
    }
    let item_kind = kind(&item).map_err(|source| TrashError::TargetUnavailable { source })?;
    if let Some(expected) = req.expected_kind
        && expected != item_kind
    {
        return Err(TrashError::KindMismatch {
            expected,
            found: item_kind,
        });
    }
    let (seen, moved) = measure(&abs).map_err(|source| TrashError::TargetUnavailable { source })?;
    if seen != Some(req.expected) {
        return Err(TrashError::IdentityChanged {
            expected: req.expected,
            found: seen.unwrap_or(FileIdentity::new(0, 0)),
        });
    }

    // 4. The Recycle Bin folder on the item's volume.
    let bin = recycle_bin(req.locations, &abs, found.volume)?;

    // 5. `$I` record (CREATE_NEW), then rename the verified handle to `$R`.
    let ext: String = abs
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let now = SystemTime::now();
    let record = format::recycle_bin_record(
        moved.logical_bytes,
        format::filetime(now),
        &display_wide(&abs),
    );
    let seed = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
        ^ (u64::from(std::process::id()) << 32);
    for attempt in 0..u64::from(MAX_NAME_ATTEMPTS) {
        let id = format::recycle_id(seed.wrapping_add(attempt));
        let info_path = bin.join(format!("$I{id}{ext}"));
        let dest = bin.join(format!("$R{id}{ext}"));
        if std::fs::symlink_metadata(&dest).is_ok() {
            continue;
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&info_path)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(&record).and_then(|()| f.sync_all()) {
                    drop(f);
                    let _ = std::fs::remove_file(&info_path);
                    return Err(unavailable(format!(
                        "cannot write {}: {e}",
                        info_path.display()
                    )));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(unavailable(format!(
                    "cannot write a record in {}: {e}",
                    bin.display()
                )));
            }
        }
        match rename_handle(&item, &dest) {
            Ok(()) => {
                return Ok(TrashOutcome {
                    kind: TrashKind::RecycleBin,
                    trash_dir: bin,
                    location: Some(dest),
                    info_file: Some(info_path),
                    moved,
                    restorable_via: Restorability::ViaTrash,
                });
            }
            Err(e) => {
                let _ = std::fs::remove_file(&info_path);
                if is_os_error(&e, ERROR_ALREADY_EXISTS) || is_os_error(&e, ERROR_FILE_EXISTS) {
                    continue;
                }
                if is_os_error(&e, ERROR_NOT_SAME_DEVICE) {
                    return Err(TrashError::CrossVolume {
                        item_volume: found.volume,
                        trash_volume: found.volume,
                    });
                }
                return Err(TrashError::MoveFailed { source: e });
            }
        }
    }
    Err(unavailable(format!(
        "no free name in {} after {MAX_NAME_ATTEMPTS} attempts",
        bin.display()
    )))
}
