//! Windows, std-only for now.
//!
//! Provided: entry kinds with reparse-point detection, logical size, mtime, read-only flag.
//!
//! Remaining for P1-PLAT-02 (#32), which needs handle-based Win32 calls not exposed by stable
//! std:
//! * file identity: volume serial number + 128-bit file ID (`GetFileInformationByHandleEx`
//!   with `FileIdInfo`);
//! * allocated size (`GetCompressedFileSizeW` / `FILE_STANDARD_INFO::AllocationSize`);
//! * hard-link count (`FILE_STANDARD_INFO::NumberOfLinks`);
//! * telling junctions and other mount points apart from symlinks by reparse tag
//!   (`IO_REPARSE_TAG_MOUNT_POINT` vs `IO_REPARSE_TAG_SYMLINK`);
//! * a no-follow, handle-relative directory open.
//!
//! Until then these are `None` and reported unavailable in [`Capabilities`], so callers lower
//! confidence or block (PLT-08) rather than assume.

use crate::{Capabilities, EntryKind, Metadata};
use std::borrow::Cow;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// `FILE_ATTRIBUTE_REPARSE_POINT` from `winnt.h`.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

pub(super) fn capabilities() -> Capabilities {
    Capabilities {
        os: std::env::consts::OS,
        file_identity: false,
        allocated_size: false,
        link_count: false,
        mtime: true,
        no_follow_metadata: true,
        special_file_kinds: false,
        reparse_points: true,
        race_free_read_dir: false,
    }
}

/// Kind from the attributes and std's file type. std reports name-surrogate reparse points
/// (symlinks, junctions) as symlinks; any other reparse point is a [`EntryKind::ReparsePoint`].
fn kind(md: &fs::Metadata) -> EntryKind {
    let ft = md.file_type();
    if ft.is_symlink() {
        EntryKind::Symlink
    } else if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        EntryKind::ReparsePoint
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn unix_seconds(t: SystemTime) -> Option<i64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).ok(),
        Err(e) => i64::try_from(e.duration().as_secs()).ok().map(|s| -s),
    }
}

pub(super) fn convert(md: &fs::Metadata) -> Metadata {
    Metadata {
        kind: kind(md),
        identity: None,
        logical_size: md.file_size(),
        allocated_size: None,
        nlink: None,
        mtime_unix: md.modified().ok().and_then(unix_seconds),
        readonly: md.permissions().readonly(),
    }
}

/// On Windows `DirEntry::metadata` comes from the `FindNextFileW` data: no extra open, links
/// not followed.
pub(super) fn entry_kind(entry: &fs::DirEntry) -> io::Result<EntryKind> {
    entry.metadata().map(|md| kind(&md))
}

pub(super) fn path_bytes(path: &Path) -> Cow<'_, [u8]> {
    Cow::Owned(
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect(),
    )
}
