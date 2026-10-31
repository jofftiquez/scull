//! Windows volumes (P1-PLAT-03): volume GUID enumeration, drive types, filesystem names and
//! flags, free space, and the volume serial number exactly as [`super::identity`] reads it.

use super::{legacy_info, open, query, verbatim};
use crate::volumes::{
    FsFeatures, VolumeInfo, VolumeKind, resolve_for_volume, win, windows_features, windows_kind,
};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use windows_sys::Win32::Foundation::{
    ERROR_MORE_DATA, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE, MAX_PATH,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ID_INFO, FILE_READ_ATTRIBUTES, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, GetVolumePathNamesForVolumeNameW,
};

/// Room for a volume GUID path, `\\?\Volume{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}\` (49
/// characters) plus the NUL.
const GUID_PATH_LEN: u32 = 64;
/// Room for a filesystem name (`NTFS`, `ReFS`, `exFAT`); the documented maximum is MAX_PATH.
const FS_NAME_LEN: u32 = MAX_PATH + 1;

/// A `FindFirstVolumeW` search handle, closed on drop.
struct FindVolume(HANDLE);

impl Drop for FindVolume {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid search handle from `FindFirstVolumeW`, owned exclusively
        // by this struct and not closed yet; it is never used after this.
        unsafe {
            FindVolumeClose(self.0);
        }
    }
}

fn wide_nul(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn until_nul(buf: &[u16]) -> &[u16] {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    &buf[..end]
}

fn buffer_len(buf: &[u16]) -> u32 {
    u32::try_from(buf.len()).unwrap_or(u32::MAX)
}

pub(crate) fn volumes() -> io::Result<Vec<VolumeInfo>> {
    let mut out = Vec::new();
    let mut name = [0u16; GUID_PATH_LEN as usize];
    // SAFETY: `name` is valid for writes of GUID_PATH_LEN UTF-16 units.
    let handle = unsafe { FindFirstVolumeW(name.as_mut_ptr(), GUID_PATH_LEN) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let search = FindVolume(handle);
    loop {
        let guid = until_nul(&name).to_vec();
        for mount in mount_points(&guid) {
            // A volume that can't be described (card reader without media, a volume being
            // dismounted) is left out rather than failing the whole list.
            if let Ok(info) = describe(&mount, Some(&guid)) {
                out.push(info);
            }
        }
        // SAFETY: `search.0` is an open search handle; `name` is valid for GUID_PATH_LEN
        // UTF-16 units.
        if unsafe { FindNextVolumeW(search.0, name.as_mut_ptr(), GUID_PATH_LEN) } == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(err);
        }
    }
    drop(search);

    // Mapped network drives have no volume GUID. Only their drive type is asked for: a
    // disconnected share could stall GetVolumeInformationW / GetDiskFreeSpaceExW (T-53).
    for drive in logical_drives()? {
        if drive_type(&drive) == win::DRIVE_REMOTE
            && !out.iter().any(|v| same_root(&v.mount_point, &drive))
        {
            out.push(VolumeInfo {
                source: drive.display().to_string(),
                mount_point: drive,
                volume_id: None,
                fs_type: String::from("unknown"),
                kind: VolumeKind::Network,
                read_only: false,
                total_bytes: None,
                free_bytes: None,
                supports: FsFeatures::default(),
            });
        }
    }
    Ok(out)
}

pub(crate) fn volume_of(path: &Path) -> io::Result<VolumeInfo> {
    let root = volume_root(path)?;
    describe(&root, None)
}

pub(crate) fn free_space(path: &Path) -> io::Result<u64> {
    let root = volume_root(path)?;
    disk_free(&wide_nul(root.as_os_str())).map(|(available, _)| available)
}

/// The root of the volume holding `path` (`C:\`, `C:\mnt\data\`, `\\server\share\`), via
/// `GetVolumePathNameW`. A link at the final component is not followed: its directory is
/// used.
fn volume_root(path: &Path) -> io::Result<PathBuf> {
    let resolved = resolve_for_volume(path)?;
    let target = match resolved.parent() {
        Some(parent) if super::metadata(&resolved)?.kind.is_link() => parent,
        _ => resolved.as_path(),
    };
    let wide = verbatim(target)?;
    let mut buf = vec![0u16; wide.len().max(MAX_PATH as usize) + 1];
    // SAFETY: `wide` is NUL-terminated and `buf` is valid for writes of `buf.len()` units;
    // both outlive the call.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), buf.as_mut_ptr(), buffer_len(&buf)) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(strip_verbatim(&OsString::from_wide(until_nul(&buf))))
}

/// `\\?\C:\` → `C:\`, `\\?\UNC\server\share\` → `\\server\share\`; volume GUID paths are kept.
fn strip_verbatim(path: &OsStr) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\")
        && rest.as_bytes().get(1) == Some(&b':')
    {
        return PathBuf::from(rest);
    }
    PathBuf::from(path)
}

fn same_root(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

/// Every mount point (drive letters, mounted folders) of a volume GUID path.
fn mount_points(guid: &[u16]) -> Vec<PathBuf> {
    let guid = guid
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let mut buf = vec![0u16; MAX_PATH as usize];
    loop {
        let mut needed = 0u32;
        // SAFETY: `guid` is NUL-terminated; `buf` is valid for writes of `buf.len()` units;
        // `needed` is a valid out pointer. All outlive the call.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                guid.as_ptr(),
                buf.as_mut_ptr(),
                buffer_len(&buf),
                &mut needed,
            )
        };
        if ok != 0 {
            return split_multi_sz(&buf);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_MORE_DATA as i32) && needed as usize > buf.len() {
            buf.resize(needed as usize, 0);
            continue;
        }
        return Vec::new();
    }
}

/// A `REG_MULTI_SZ`-style list: NUL-separated strings ending with an empty one.
fn split_multi_sz(buf: &[u16]) -> Vec<PathBuf> {
    buf.split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(|s| PathBuf::from(OsString::from_wide(s)))
        .collect()
}

/// Drive roots (`C:\`, `Z:\`) from `GetLogicalDriveStringsW`.
fn logical_drives() -> io::Result<Vec<PathBuf>> {
    // SAFETY: a zero length with a null buffer only asks for the required size.
    let needed = unsafe { GetLogicalDriveStringsW(0, ptr::null_mut()) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u16; needed as usize + 1];
    // SAFETY: `buf` is valid for writes of `buf.len()` units.
    let written = unsafe { GetLogicalDriveStringsW(buffer_len(&buf), buf.as_mut_ptr()) };
    if written == 0 || written as usize > buf.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(split_multi_sz(&buf))
}

fn drive_type(root: &Path) -> u32 {
    let wide = wide_nul(root.as_os_str());
    // SAFETY: `wide` is a NUL-terminated string that outlives the call.
    unsafe { GetDriveTypeW(wide.as_ptr()) }
}

/// (bytes available to the caller, total bytes) of the volume at `root` (NUL-terminated).
fn disk_free(root: &[u16]) -> io::Result<(u64, u64)> {
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `root` is NUL-terminated; the three out pointers are valid for writes.
    let ok = unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut available, &mut total, &mut free) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((available, total))
}

/// The volume serial number as [`super::identity`] reports it for entries on the volume:
/// the 64-bit `FILE_ID_INFO::VolumeSerialNumber`, or the 32-bit legacy serial where
/// `FileIdInfo` is unsupported. Read from a handle on the volume root, opened with
/// attribute-only access.
fn handle_volume_serial(root: &Path) -> Option<u64> {
    let handle = open(&verbatim(root).ok()?, FILE_READ_ATTRIBUTES).ok()?;
    if let Ok(info) = query::<FILE_ID_INFO>(&handle) {
        return Some(info.VolumeSerialNumber);
    }
    legacy_info(&handle)
        .ok()
        .map(|info| u64::from(info.dwVolumeSerialNumber))
}

/// The volume GUID path of a mount point, for display.
fn volume_guid(root_w: &[u16]) -> Option<String> {
    let mut buf = [0u16; GUID_PATH_LEN as usize];
    // SAFETY: `root_w` is NUL-terminated; `buf` is valid for writes of GUID_PATH_LEN units.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(root_w.as_ptr(), buf.as_mut_ptr(), GUID_PATH_LEN)
    };
    (ok != 0).then(|| String::from_utf16_lossy(until_nul(&buf)))
}

/// Describes the volume mounted at `root` (with trailing backslash).
fn describe(root: &Path, guid: Option<&[u16]>) -> io::Result<VolumeInfo> {
    let root_w = wide_nul(root.as_os_str());
    let kind = windows_kind(drive_type(root)).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no volume is mounted at this path")
    })?;

    let mut serial = 0u32;
    let mut flags = 0u32;
    let mut fs_name = [0u16; FS_NAME_LEN as usize];
    // SAFETY: `root_w` is NUL-terminated; the serial and flags out pointers are valid; the
    // volume-name buffer is null with size 0 and the maximum-component-length pointer is
    // null, both documented as optional; `fs_name` is valid for FS_NAME_LEN units.
    let ok = unsafe {
        GetVolumeInformationW(
            root_w.as_ptr(),
            ptr::null_mut(),
            0,
            &mut serial,
            ptr::null_mut(),
            &mut flags,
            fs_name.as_mut_ptr(),
            FS_NAME_LEN,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let fs_type = String::from_utf16_lossy(until_nul(&fs_name));
    let sizes = disk_free(&root_w).ok();
    let source = guid
        .map(String::from_utf16_lossy)
        .or_else(|| volume_guid(&root_w))
        .unwrap_or_else(|| root.display().to_string());

    Ok(VolumeInfo {
        mount_point: root.to_path_buf(),
        source,
        volume_id: handle_volume_serial(root).or(Some(u64::from(serial))),
        supports: windows_features(&fs_type, flags),
        fs_type,
        kind,
        read_only: flags & win::FILE_READ_ONLY_VOLUME != 0,
        total_bytes: sizes.map(|(_, total)| total),
        free_bytes: sizes.map(|(available, _)| available),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_prefixes() {
        assert_eq!(
            strip_verbatim(OsStr::new(r"\\?\C:\")),
            PathBuf::from(r"C:\")
        );
        assert_eq!(
            strip_verbatim(OsStr::new(r"\\?\UNC\srv\share\")),
            PathBuf::from(r"\\srv\share\")
        );
        assert_eq!(
            strip_verbatim(OsStr::new(r"D:\mnt\")),
            PathBuf::from(r"D:\mnt\")
        );
        let guid = r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\";
        assert_eq!(strip_verbatim(OsStr::new(guid)), PathBuf::from(guid));
    }

    #[test]
    fn splits_multi_sz() {
        let buf: Vec<u16> = "C:\\\0D:\\mnt\\\0\0garbage".encode_utf16().collect();
        assert_eq!(
            split_multi_sz(&buf),
            [PathBuf::from(r"C:\"), PathBuf::from(r"D:\mnt\")]
        );
        assert!(split_multi_sz(&[0, 0]).is_empty());
    }
}
