//! OS-conditional code of the harness (with the I-6 network guard in `net`, which spawns the
//! OS sandbox and socket-listing tools). Everything here uses `std` alone: no `libc`, no
//! `windows-sys`, no `unsafe`.
//!
//! This is a deliberate, test-only exception to PLT-11 ("no OS APIs outside
//! `scull-platform`"): the harness must be able to build and verify trees independently of the
//! code under test, so it does not route through `scull-platform`. Keep this module small.
//!
//! Gaps that `std` cannot fill on stable Rust, to be closed by #32 (P1-PLAT-02):
//! * **Windows file identity** (volume serial number + file ID): `MetadataExt::file_index` and
//!   `volume_serial_number` are unstable, so snapshots record `identity: None` on Windows and
//!   hardlink identity cannot be asserted there yet.
//! * **Windows junctions** are created by spawning `cmd /C mklink /J` (no privilege needed);
//!   a native `FSCTL_SET_REPARSE_POINT` implementation belongs in `scull-platform`.

use scull_core::FileIdentity;
use std::fs::Metadata;
use std::io;
use std::path::Path;

/// Path-independent identity of the object `m` describes, where `std` exposes it.
#[cfg(unix)]
pub(crate) fn identity(m: &Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FileIdentity::new(m.dev(), u128::from(m.ino())))
}

/// Path-independent identity: not available from stable `std` on this OS (see module docs).
#[cfg(not(unix))]
pub(crate) fn identity(_m: &Metadata) -> Option<FileIdentity> {
    None
}

/// Full permission bits (`st_mode & 0o7777`) where available.
#[cfg(unix)]
pub(crate) fn mode(m: &Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(m.permissions().mode() & 0o7777)
}

/// Full permission bits: Unix only.
#[cfg(not(unix))]
pub(crate) fn mode(_m: &Metadata) -> Option<u32> {
    None
}

/// Create a symbolic link at `link` pointing to `target`.
#[cfg(unix)]
pub(crate) fn symlink(target: &Path, link: &Path, _target_is_dir: bool) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Create a symbolic link at `link` pointing to `target` (Windows distinguishes file and
/// directory links).
///
/// A relative target is stored with `\` separators: Windows keeps the target text as given
/// and doesn't resolve `/` in a relative link target, so `../a/b` would make a dangling link.
#[cfg(windows)]
pub(crate) fn symlink(target: &Path, link: &Path, target_is_dir: bool) -> io::Result<()> {
    let native: std::path::PathBuf;
    let target = if target.is_relative() {
        native = target.components().collect();
        native.as_path()
    } else {
        target
    };
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// Symbolic links are not supported on this target.
#[cfg(not(any(unix, windows)))]
pub(crate) fn symlink(_target: &Path, _link: &Path, _target_is_dir: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported on this target",
    ))
}

/// Whether a symlink-creation error means "not permitted here" (e.g. Windows without Developer
/// Mode or admin rights), so the step should be skipped rather than failed.
pub(crate) fn is_symlink_unsupported(e: &io::Error) -> bool {
    // ERROR_PRIVILEGE_NOT_HELD (1314) on Windows.
    const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
    matches!(
        e.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
    ) || (cfg!(windows) && e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD))
}

/// Create an NTFS junction at `link` pointing to the directory `target`, via `mklink /J`.
#[cfg(windows)]
pub(crate) fn junction(target: &Path, link: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    // Windows paths cannot contain `"`, so plain quoting is unambiguous for cmd.exe.
    let out = std::process::Command::new(shell)
        .raw_arg(format!(
            "/D /C mklink /J \"{}\" \"{}\"",
            link.display(),
            target.display()
        ))
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "mklink /J failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Junctions exist only on Windows.
#[cfg(not(windows))]
pub(crate) fn junction(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "junctions exist only on Windows",
    ))
}

/// Set the modification time of `path` itself, never following a link: files and directories
/// through `File::set_modified`; links and special files (FIFOs, sockets: opening a FIFO can
/// block forever) through `touch -h -r <reference>` (stable `std` has no `lutimes`), where
/// `reference` already carries the wanted time.
#[cfg(unix)]
pub(crate) fn set_mtime_nofollow(
    path: &Path,
    kind: std::fs::FileType,
    mtime: std::time::SystemTime,
    reference: &Path,
) -> io::Result<()> {
    if !kind.is_file() && !kind.is_dir() {
        let out = std::process::Command::new("touch")
            .arg("-h")
            .arg("-r")
            .arg(reference)
            .arg(path)
            .output()?;
        return if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "touch -h failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        };
    }
    let f = if kind.is_dir() {
        std::fs::File::open(path)?
    } else {
        std::fs::File::options().write(true).open(path)?
    };
    f.set_modified(mtime)
}

/// Set the modification time of `path` itself, never following a link: opened with
/// `FILE_WRITE_ATTRIBUTES` only, `FILE_FLAG_BACKUP_SEMANTICS` (needed for directories) and,
/// for links and junctions, `FILE_FLAG_OPEN_REPARSE_POINT`.
#[cfg(windows)]
pub(crate) fn set_mtime_nofollow(
    path: &Path,
    kind: std::fs::FileType,
    mtime: std::time::SystemTime,
    _reference: &Path,
) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if kind.is_symlink() {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let f = std::fs::File::options()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(flags)
        .open(path)?;
    f.set_modified(mtime)
}

/// Setting modification times is not supported on this target.
#[cfg(not(any(unix, windows)))]
pub(crate) fn set_mtime_nofollow(
    _path: &Path,
    _kind: std::fs::FileType,
    _mtime: std::time::SystemTime,
    _reference: &Path,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "setting modification times is not supported on this target",
    ))
}
