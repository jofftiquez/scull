//! macOS volumes (P1-PLAT-03): `getfsstat(MNT_NOWAIT)` / `statfs`, `pathconf` for case
//! sensitivity, Time Machine markers by `lstat` only.

use crate::volumes::{
    TIME_MACHINE_ROOT_MARKERS, VolumeInfo, VolumeKind, mac_flags, macos_features, macos_kind,
    resolve_for_volume,
};
use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(crate) fn volumes() -> io::Result<Vec<VolumeInfo>> {
    Ok(all_statfs()?.iter().map(info).collect())
}

pub(crate) fn volume_of(path: &Path) -> io::Result<VolumeInfo> {
    let resolved = resolve_for_volume(path)?;
    // `statfs` follows a symlink at the final component; ask about its directory instead.
    let target = match resolved.parent() {
        Some(parent) if fs::symlink_metadata(&resolved)?.file_type().is_symlink() => parent,
        _ => resolved.as_path(),
    };
    statfs(target).map(|st| info(&st))
}

pub(crate) fn free_space(path: &Path) -> io::Result<u64> {
    statfs(path).map(|st| st.f_bavail.saturating_mul(u64::from(st.f_bsize)))
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Every mount, from the kernel's cached statistics: `MNT_NOWAIT` never contacts a network
/// server, so a stalled share can't block the listing. Unlike `getmntinfo`, `getfsstat` fills
/// a caller-owned buffer, so it is thread-safe.
#[allow(unsafe_code)]
fn all_statfs() -> io::Result<Vec<libc::statfs>> {
    loop {
        // SAFETY: a null buffer with size 0 only asks for the number of mounts.
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
        // Room for mounts that appear between the two calls.
        let capacity = count + 4;
        let bytes = libc::c_int::try_from(capacity * size_of::<libc::statfs>())
            .map_err(|_| io::Error::other("too many mounts"))?;
        let mut buf: Vec<libc::statfs> = Vec::with_capacity(capacity);
        // SAFETY: `buf` has room for `capacity` entries, which is exactly `bytes` bytes; the
        // kernel writes at most that many and returns how many it wrote.
        let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        let got = usize::try_from(got).map_err(|_| io::Error::last_os_error())?;
        if got < capacity {
            // SAFETY: the kernel initialized the first `got` entries (`got` <= capacity).
            unsafe { buf.set_len(got) };
            return Ok(buf);
        }
        // The buffer filled up: mounts were added in between; ask again.
    }
}

/// `statfs(path)`. Follows a symlink, so callers pass resolved paths.
#[allow(unsafe_code)]
fn statfs(path: &Path) -> io::Result<libc::statfs> {
    let c_path = c_path(path)?;
    let mut out = MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `c_path` is NUL-terminated and `out` is valid for writes of one `statfs`; both
    // outlive the call.
    if unsafe { libc::statfs(c_path.as_ptr(), out.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zero-initialized and filled by the successful call; integers and byte arrays.
    Ok(unsafe { out.assume_init() })
}

/// A NUL-terminated `c_char` array as bytes (without the NUL).
fn c_bytes(chars: &[libc::c_char]) -> Vec<u8> {
    chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect()
}

/// `f_fsid.val[0]`: the `dev_t` of the volume (what `st_dev` reports for its files).
#[allow(unsafe_code)]
fn fsid_dev(st: &libc::statfs) -> u64 {
    // SAFETY: `fsid_t` is `#[repr(C)] struct { int32_t val[2]; }` (its field is private in the
    // `libc` crate); transmute checks the sizes match. Every bit pattern is a valid [i32; 2].
    let val: [i32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
    // Same sign extension as `MetadataExt::dev()` (`st_dev as u64`), so the two compare equal.
    val[0] as u64
}

/// `pathconf(_PC_CASE_SENSITIVE)`: 1 case-sensitive, 0 insensitive, -1 unknown.
#[allow(unsafe_code)]
fn case_sensitive(mount_point: &Path) -> Option<bool> {
    let c_path = c_path(mount_point).ok()?;
    // SAFETY: `c_path` is a NUL-terminated string that outlives the call; `pathconf` only
    // reads it.
    match unsafe { libc::pathconf(c_path.as_ptr(), libc::_PC_CASE_SENSITIVE) } {
        1 => Some(true),
        0 => Some(false),
        _ => None,
    }
}

fn info(st: &libc::statfs) -> VolumeInfo {
    let fs_type = String::from_utf8_lossy(&c_bytes(&st.f_fstypename)).into_owned();
    let mount_point = PathBuf::from(OsString::from_vec(c_bytes(&st.f_mntonname)));
    let source = String::from_utf8_lossy(&c_bytes(&st.f_mntfromname)).into_owned();
    let flags = st.f_flags;
    let local = flags & mac_flags::LOCAL != 0;
    // Only local volumes are touched beyond the cached statistics (lstat of root markers,
    // pathconf); a network volume is never contacted while listing.
    let has_backup_marker = local
        && TIME_MACHINE_ROOT_MARKERS
            .iter()
            .any(|m| fs::symlink_metadata(mount_point.join(m)).is_ok());
    let kind = macos_kind(&fs_type, &mount_point, &source, flags, has_backup_marker);
    let volume_id = if local {
        fs::symlink_metadata(&mount_point)
            .map(|md| md.dev())
            .unwrap_or_else(|_| fsid_dev(st))
    } else {
        fsid_dev(st)
    };
    let case = if local && kind != VolumeKind::Virtual {
        case_sensitive(&mount_point)
    } else {
        None
    };
    let block = u64::from(st.f_bsize);
    VolumeInfo {
        source,
        volume_id: Some(volume_id),
        supports: macos_features(&fs_type, case),
        fs_type,
        kind,
        read_only: flags & mac_flags::RDONLY != 0,
        total_bytes: Some(st.f_blocks.saturating_mul(block)),
        free_bytes: Some(st.f_bavail.saturating_mul(block)),
        mount_point,
    }
}

/// Whether `path` (not followed if it is a link) carries a macOS extended ACL (`ls -le`, set
/// with `chmod +a`), of any kind. Permission bits don't show these, so a caller that must know
/// nobody else can write an entry treats any ACL as a possible grant (P5-PRO-02).
#[allow(unsafe_code)]
pub(crate) fn has_extended_acl(path: &Path) -> io::Result<bool> {
    // <sys/acl.h>: `acl_type_t` ACL_TYPE_EXTENDED; `acl_t` is an opaque pointer.
    const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
    unsafe extern "C" {
        fn acl_get_link_np(path: *const libc::c_char, ty: libc::c_int) -> *mut libc::c_void;
        fn acl_free(obj: *mut libc::c_void) -> libc::c_int;
    }
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call; the function
    // does not follow a final symlink and returns either NULL (errno set) or an ACL we own.
    let acl = unsafe { acl_get_link_np(c_path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let e = io::Error::last_os_error();
        // ENOENT: the entry exists but has no extended ACL.
        return match e.raw_os_error() {
            Some(libc::ENOENT) if fs::symlink_metadata(path).is_ok() => Ok(false),
            _ => Err(e),
        };
    }
    // SAFETY: `acl` is a non-NULL ACL returned by `acl_get_link_np`, freed exactly once.
    unsafe { acl_free(acl) };
    Ok(true)
}
