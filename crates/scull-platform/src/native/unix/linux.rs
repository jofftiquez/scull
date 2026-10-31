//! Linux volumes (P1-PLAT-03): `/proc/self/mountinfo` + `statvfs`, sysfs for removable media.

use crate::volumes::mountinfo::{self, MountEntry};
use crate::volumes::{
    VolumeInfo, VolumeKind, linux_features, linux_kind, longest_mount_prefix, resolve_for_volume,
};
use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const MOUNTINFO: &str = "/proc/self/mountinfo";

fn mount_table() -> io::Result<Vec<MountEntry>> {
    let raw = fs::read(MOUNTINFO)?;
    let table = mountinfo::parse(&raw);
    if table.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no mounts could be parsed from /proc/self/mountinfo",
        ));
    }
    Ok(table)
}

pub(crate) fn volumes() -> io::Result<Vec<VolumeInfo>> {
    Ok(mount_table()?.iter().map(info).collect())
}

pub(crate) fn volume_of(path: &Path) -> io::Result<VolumeInfo> {
    let resolved = resolve_for_volume(path)?;
    let table = mount_table()?;
    let index = longest_mount_prefix(
        &resolved,
        table
            .iter()
            .map(|e| Path::new(OsStr::from_bytes(&e.mount_point))),
    )
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no mount in /proc/self/mountinfo contains this path",
        )
    })?;
    Ok(info(&table[index]))
}

pub(crate) fn free_space(path: &Path) -> io::Result<u64> {
    statvfs(path).map(|(_, available)| available)
}

fn info(entry: &MountEntry) -> VolumeInfo {
    let mount_point = PathBuf::from(OsStr::from_bytes(&entry.mount_point));
    let kind = linux_kind(
        &entry.fs_type,
        &mount_point,
        removable_block_device(entry.major, entry.minor),
    );
    // `statvfs` on a stale network mount can block forever, and on an autofs mount point
    // it triggers the automount (T-53).
    let sizes = if kind == VolumeKind::Network || entry.fs_type == "autofs" {
        None
    } else {
        statvfs(&mount_point).ok()
    };
    VolumeInfo {
        source: String::from_utf8_lossy(&entry.source).into_owned(),
        volume_id: Some(entry.dev()),
        fs_type: entry.fs_type.clone(),
        kind,
        read_only: entry.read_only,
        total_bytes: sizes.map(|(total, _)| total),
        free_bytes: sizes.map(|(_, available)| available),
        supports: linux_features(&entry.fs_type),
        mount_point,
    }
}

/// Whether the block device `major:minor` is removable media or on USB, from sysfs. A
/// partition's flags live on its parent disk. Major 0 is an anonymous device (pseudo
/// filesystems, Btrfs, overlay, NFS): never a block device.
fn removable_block_device(major: u32, minor: u32) -> bool {
    if major == 0 {
        return false;
    }
    let Ok(dev) = fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")) else {
        return false;
    };
    let flag = |dir: &Path| {
        fs::read(dir.join("removable")).is_ok_and(|v| v.trim_ascii() == b"1".as_slice())
    };
    dev.to_string_lossy().contains("/usb") || flag(&dev) || dev.parent().is_some_and(flag)
}

/// `statvfs(path)`: (total bytes, bytes available to unprivileged users).
#[allow(unsafe_code)]
#[allow(clippy::useless_conversion)] // field types differ between 32- and 64-bit targets
fn statvfs(path: &Path) -> io::Result<(u64, u64)> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut out = MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c_path` is a valid NUL-terminated string and `out` is valid for writes of one
    // `statvfs`; both outlive the call. `statvfs` only reads the path and fills the struct.
    if unsafe { libc::statvfs(c_path.as_ptr(), out.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zero-initialized and filled by the successful call; plain integers only.
    let st = unsafe { out.assume_init() };
    let unit = if st.f_frsize != 0 {
        u64::from(st.f_frsize)
    } else {
        u64::from(st.f_bsize)
    };
    Ok((
        u64::from(st.f_blocks).saturating_mul(unit),
        u64::from(st.f_bavail).saturating_mul(unit),
    ))
}
