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
    }
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
