//! Windows: same-volume, no-replace moves on handles (P2-EXEC-05/06, #71/#72), mirroring the
//! removal walker (`remove::windows`, #68).
//!
//! * The source is reached like the walker reaches its target: the trusted anchor (opened as
//!   given), then every component below it opened with `NtCreateFile` **relative to the
//!   previous handle**, no-follow (`FILE_OPEN_REPARSE_POINT`, `OBJ_DONT_REPARSE`), and refused
//!   if it is a junction, symbolic link or other reparse point.
//! * The entry itself is opened relative to its parent handle, no-follow, with `DELETE` access
//!   (needed to rename it), and **verified on that handle**: identity (volume serial + file ID),
//!   kind, not a volume mount point, on its parent's volume.
//! * The destination directory is opened by path with `FILE_FLAG_OPEN_REPARSE_POINT`; it must
//!   be a real directory (not a reparse point) with the expected identity, on the entry's
//!   volume.
//! * The rename is `NtSetInformationFile(FileRenameInformationEx)` **on the verified handle**,
//!   with `RootDirectory` = the verified destination handle and `FileName` = the one new name,
//!   `Flags = FILE_RENAME_POSIX_SEMANTICS` and **without** `FILE_RENAME_REPLACE_IF_EXISTS`:
//!   an existing name is never replaced (`STATUS_OBJECT_NAME_COLLISION`), and a rename never
//!   crosses volumes (`STATUS_NOT_SAME_DEVICE`). Where the extended class or the POSIX flag is
//!   not supported (older Windows, FAT), the classic `FileRenameInformation` with
//!   `ReplaceIfExists = FALSE` is used: still never replacing. The NT call is used directly
//!   (rather than `SetFileInformationByHandle(FileRenameInfoEx)`, its Win32 wrapper) so the
//!   destination is resolved relative to the held directory handle, never re-parsed from a
//!   path.
//! * Because the rename acts on the handle whose identity was checked, the object moved is the
//!   planned one (a link is moved as the link itself). The new name is then probed relative to
//!   the destination handle and must have that identity; otherwise the object is moved back to
//!   its source name (no-replace) and the move is reported as [`MoveError::MovedUnexpected`].
//!
//! Compiled and clippy-checked for the Windows target; its tests run on Windows CI.

#![allow(unsafe_code)]

use super::{MoveError, MoveRequest, Moved};
use crate::EntryKind;
use crate::native::windows::{Handle, identity, verbatim};
use crate::remove::plain_components;
use crate::remove::win_rules::{self, raw};
use crate::remove::windows::{Opened, Purpose, attr_tag, inspect, nt_error, open, open_anchor};
use scull_core::FileIdentity;
use std::ffi::OsString;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_RENAME_INFORMATION, FILE_RENAME_POSIX_SEMANTICS, FileRenameInformation,
    FileRenameInformationEx, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, NTSTATUS};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, OPEN_EXISTING,
    SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

/// `STATUS_OBJECT_NAME_COLLISION`: the new name exists (never replaced).
const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = 0xC000_0035_u32 as i32;
/// `STATUS_NOT_SAME_DEVICE`: a rename across volumes.
const STATUS_NOT_SAME_DEVICE: NTSTATUS = 0xC000_00D4_u32 as i32;
/// `STATUS_INVALID_INFO_CLASS`: `FileRenameInformationEx` unknown to this Windows build.
const STATUS_INVALID_INFO_CLASS: NTSTATUS = 0xC000_0003_u32 as i32;
/// `STATUS_NOT_SUPPORTED`: e.g. POSIX rename semantics on FAT.
const STATUS_NOT_SUPPORTED: NTSTATUS = 0xC000_00BB_u32 as i32;
/// `STATUS_ACCESS_DENIED`, `STATUS_MEDIA_WRITE_PROTECTED`, `STATUS_CANNOT_DELETE`: refused by
/// the OS (permissions, read-only volume or attribute).
const NOT_PERMITTED: [NTSTATUS; 3] = [
    raw::STATUS_ACCESS_DENIED,
    raw::STATUS_MEDIA_WRITE_PROTECTED,
    raw::STATUS_CANNOT_DELETE,
];

/// The rename classes, in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameClass {
    /// `FileRenameInformationEx` with `FILE_RENAME_POSIX_SEMANTICS`, no replace.
    PosixNoReplace,
    /// `FileRenameInformation` with `ReplaceIfExists = FALSE`.
    ClassicNoReplace,
}

/// Renames the object behind `handle` (opened with `DELETE` access) to `name` inside the
/// directory `dest`, never replacing an existing entry.
fn rename_no_replace(handle: &Handle, dest: &Handle, name: &[u16]) -> Result<(), NTSTATUS> {
    match rename_with(handle, dest, name, RenameClass::PosixNoReplace) {
        Err(s)
            if s == STATUS_INVALID_INFO_CLASS
                || s == STATUS_NOT_SUPPORTED
                || s == raw::STATUS_INVALID_PARAMETER =>
        {
            rename_with(handle, dest, name, RenameClass::ClassicNoReplace)
        }
        other => other,
    }
}

fn rename_with(
    handle: &Handle,
    dest: &Handle,
    name: &[u16],
    class: RenameClass,
) -> Result<(), NTSTATUS> {
    let name_bytes = std::mem::size_of_val(name);
    let offset = offset_of!(FILE_RENAME_INFORMATION, FileName);
    let size = (offset + name_bytes).max(size_of::<FILE_RENAME_INFORMATION>());
    // 8-byte aligned and zeroed: no flags (in particular no FILE_RENAME_REPLACE_IF_EXISTS) /
    // ReplaceIfExists = FALSE until set below.
    let mut buf = vec![0u64; size.div_ceil(size_of::<u64>())];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let name_len = u32::try_from(name_bytes).map_err(|_| raw::STATUS_OBJECT_NAME_INVALID)?;
    let len = u32::try_from(size).map_err(|_| raw::STATUS_OBJECT_NAME_INVALID)?;
    // SAFETY: `buf` is at least `size` bytes, zeroed and 8-byte aligned (enough for
    // FILE_RENAME_INFORMATION), and the name fits after `FileName`'s offset by construction.
    // Only raw-pointer writes; no reference to the variable-length tail is created. Writing the
    // `Flags` member of the zeroed union is a plain integer store.
    unsafe {
        if class == RenameClass::PosixNoReplace {
            (*info).Anonymous.Flags = FILE_RENAME_POSIX_SEMANTICS;
        }
        (*info).RootDirectory = dest.as_raw();
        (*info).FileNameLength = name_len;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            buf.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
            name.len(),
        );
    }
    let mut io_status = IO_STATUS_BLOCK::default();
    let class = match class {
        RenameClass::PosixNoReplace => FileRenameInformationEx,
        RenameClass::ClassicNoReplace => FileRenameInformation,
    };
    // SAFETY: `handle` is an open handle with DELETE access that we own; `io_status` is valid
    // for writes; `buf` holds a valid FILE_RENAME_INFORMATION of `len` bytes whose
    // `RootDirectory` is an open directory handle we own, for the duration of the call. The
    // handle is synchronous (FILE_SYNCHRONOUS_IO_NONALERT), so the call completes before
    // returning.
    let status = unsafe {
        NtSetInformationFile(
            handle.as_raw(),
            &mut io_status,
            buf.as_ptr().cast(),
            len,
            class,
        )
    };
    if status < 0 { Err(status) } else { Ok(()) }
}

/// The destination directory, opened by path without following a reparse point at its last
/// component (traverse + attributes only; every share mode, so nobody else is blocked).
fn open_dest(path: &Path) -> io::Result<Handle> {
    let wide = verbatim(path)?;
    // SAFETY: `verbatim` returns a NUL-terminated UTF-16 path that outlives the call. A null
    // security-attributes pointer and a null template handle are documented as allowed.
    // OPEN_EXISTING never creates anything; FILE_FLAG_BACKUP_SEMANTICS is required to open a
    // directory; FILE_FLAG_OPEN_REPARSE_POINT opens a reparse point itself, never its target.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW succeeded: a new open handle that only we own.
    Ok(unsafe { Handle::from_raw(handle) })
}

fn wide_name(name: &std::ffi::OsStr) -> Result<Vec<u16>, MoveError> {
    let wide: Vec<u16> = name.encode_wide().collect();
    win_rules::validate_name(&wide).map_err(|reason| MoveError::InvalidRequest { reason })?;
    Ok(wide)
}

fn not_a_real_dir(attributes: u32) -> bool {
    attributes & raw::FILE_ATTRIBUTE_REPARSE_POINT != 0
        || attributes & raw::FILE_ATTRIBUTE_DIRECTORY == 0
}

pub(super) fn move_no_replace(req: &MoveRequest<'_>) -> Result<Moved, MoveError> {
    let names = plain_components(req.relative)
        .map_err(|_| MoveError::InvalidRequest {
            reason: "the relative path must be plain names",
        })?
        .into_iter()
        .map(wide_name)
        .collect::<Result<Vec<_>, _>>()?;
    let Some((name, intermediate)) = names.split_last() else {
        return Err(MoveError::InvalidRequest {
            reason: "the relative path is empty",
        });
    };
    let dest_name = match plain_components(Path::new(req.dest_name)).as_deref() {
        Ok([one]) => wide_name(one)?,
        _ => {
            return Err(MoveError::InvalidRequest {
                reason: "the destination name must be one plain name",
            });
        }
    };
    if !req.dest_dir.is_absolute() {
        return Err(MoveError::InvalidRequest {
            reason: "the destination directory must be absolute",
        });
    }

    // 1. The anchor (trusted, opened as given), then every component no-follow, relative to
    //    the previous handle; a reparse point is never traversed.
    let mut parent =
        open_anchor(req.anchor).map_err(|source| MoveError::AnchorUnavailable { source })?;
    if let Some(expected) = req.anchor_identity {
        let found = identity(&parent).unwrap_or(FileIdentity::new(0, 0));
        if found != expected {
            return Err(MoveError::AnchorChanged { expected, found });
        }
    }
    let mut walked = PathBuf::new();
    for n in intermediate {
        walked.push(OsString::from_wide(n));
        let not_traversable = |source| MoveError::PathNotTraversable {
            component: walked.clone(),
            source,
        };
        let next = open(&parent, n, Purpose::Traverse).map_err(|s| not_traversable(nt_error(s)))?;
        let (attributes, _) = attr_tag(&next).map_err(not_traversable)?;
        if not_a_real_dir(attributes) {
            return Err(not_traversable(io::Error::other(
                "a junction, symbolic link or other reparse point is never traversed",
            )));
        }
        parent = next;
    }
    let parent_id = identity(&parent).ok_or_else(|| MoveError::SourceUnavailable {
        source: io::Error::other("the source directory reports no stable file ID"),
    })?;

    // 2. The entry: opened relative to its parent, no-follow, with DELETE access, and verified
    //    on that very handle.
    let entry = match open(&parent, name, Purpose::Move) {
        Ok(h) => h,
        Err(s) if win_rules::is_not_found(s) => return Err(MoveError::SourceMissing),
        Err(s) if NOT_PERMITTED.contains(&s) => {
            return Err(MoveError::NotPermitted {
                source: nt_error(s),
            });
        }
        Err(s) => {
            return Err(MoveError::SourceUnavailable {
                source: nt_error(s),
            });
        }
    };
    let Opened { id, kind, .. } =
        inspect(&entry).map_err(|source| MoveError::SourceUnavailable { source })?;
    if id != req.expected {
        return Err(MoveError::IdentityMismatch {
            expected: req.expected,
            found: id,
        });
    }
    if let Some(expected) = req.expected_kind
        && expected != kind
    {
        return Err(MoveError::KindMismatch {
            expected,
            found: kind,
        });
    }
    // Another volume than its directory, or a volume mount point: never moved.
    if parent_id.volume != id.volume
        || (kind == EntryKind::Junction
            && crate::remove::windows::is_volume_mount_point_handle(&entry))
    {
        return Err(MoveError::SourceIsMountPoint);
    }

    // 3. The destination directory: a real directory, the expected one, on the entry's volume.
    let dest =
        open_dest(req.dest_dir).map_err(|source| MoveError::DestinationUnavailable { source })?;
    let (dest_attributes, _) =
        attr_tag(&dest).map_err(|source| MoveError::DestinationUnavailable { source })?;
    if not_a_real_dir(dest_attributes) {
        return Err(MoveError::DestinationUnavailable {
            source: io::Error::other(
                "the destination is not a real directory (a reparse point is never followed)",
            ),
        });
    }
    let dest_id = identity(&dest).ok_or_else(|| MoveError::DestinationUnavailable {
        source: io::Error::other("the destination reports no stable file ID"),
    })?;
    if let Some(expected) = req.dest_dir_identity
        && dest_id != expected
    {
        return Err(MoveError::DestinationChanged {
            expected,
            found: dest_id,
        });
    }
    if dest_id.volume != id.volume {
        return Err(MoveError::CrossDevice);
    }

    // 4. The rename, on the verified handle, relative to the verified destination, never
    //    replacing.
    match rename_no_replace(&entry, &dest, &dest_name) {
        Ok(()) => {}
        Err(s) if s == STATUS_OBJECT_NAME_COLLISION => return Err(MoveError::DestinationExists),
        Err(s) if s == STATUS_NOT_SAME_DEVICE => return Err(MoveError::CrossDevice),
        Err(s) if win_rules::is_not_found(s) || s == raw::STATUS_DELETE_PENDING => {
            return Err(MoveError::SourceMissing);
        }
        Err(s) if NOT_PERMITTED.contains(&s) => {
            return Err(MoveError::NotPermitted {
                source: nt_error(s),
            });
        }
        Err(s) if s == STATUS_INVALID_INFO_CLASS || s == STATUS_NOT_SUPPORTED => {
            return Err(MoveError::NoReplaceUnsupported {
                source: nt_error(s),
            });
        }
        Err(s) => {
            return Err(MoveError::Io {
                source: nt_error(s),
            });
        }
    }

    // 5. The new name must refer to the object that was checked.
    let now = open(&dest, &dest_name, Purpose::Probe)
        .ok()
        .and_then(|h| identity(&h));
    match now {
        Some(found) if found == req.expected => Ok(Moved {
            kind,
            identity: req.expected,
            source_parent: parent_id,
            dest_dir: dest_id,
        }),
        found => {
            let moved_back = rename_no_replace(&entry, &parent, name).is_ok();
            Err(MoveError::MovedUnexpected { found, moved_back })
        }
    }
}

/// The shallowest directory among `item_parent` and its ancestors that is a real directory
/// (not a reparse point), on the same volume as `item_parent`, and **owned by the current
/// user** (the token user SID): see [`super::quarantine_base`].
pub(super) fn quarantine_base(item_parent: &Path) -> io::Result<Option<PathBuf>> {
    use crate::{Native, Platform};
    let md = Native
        .metadata(item_parent)
        .map_err(|e| io::Error::other(e.to_string()))?;
    let Some(volume) = md.identity.map(|i| i.volume) else {
        return Ok(None);
    };
    let mut chain: Vec<&Path> = item_parent.ancestors().collect();
    chain.reverse();
    for dir in chain {
        let Ok(m) = Native.metadata(dir) else {
            continue;
        };
        if m.kind == EntryKind::Dir
            && m.identity.is_some_and(|i| i.volume == volume)
            && crate::native::windows::owned_by_current_user(dir).unwrap_or(false)
        {
            return Ok(Some(dir.to_path_buf()));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Windows quarantine/restore moves (#71/#72), in `scull-fixtures` trees with a canary
    //! (and its junction target), invariants checked. Run on Windows CI.

    use super::super::{MoveError, MoveRequest, move_no_replace, quarantine_base};
    use crate::{EntryKind, Native, Platform};
    use scull_core::FileIdentity;
    use scull_fixtures::{Fixture, InvariantInputs};
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn id(p: &Path) -> FileIdentity {
        Native.metadata(p).unwrap().identity.unwrap()
    }

    fn fixture() -> Fixture {
        let mut fx = Fixture::new().unwrap();
        fx.file("proj/cand/sub/f.txt", "data")
            .unwrap()
            .dir("q")
            .unwrap();
        fx.checkpoint().unwrap();
        fx
    }

    fn req<'a>(
        fx: &'a Fixture,
        dest: &'a Path,
        expected: FileIdentity,
        name: &'a str,
    ) -> MoveRequest<'a> {
        MoveRequest {
            anchor: fx.root(),
            anchor_identity: Some(id(fx.root())),
            relative: Path::new("proj/cand"),
            expected,
            expected_kind: Some(EntryKind::Dir),
            dest_dir: dest,
            dest_dir_identity: Some(id(dest)),
            dest_name: OsStr::new(name),
        }
    }

    fn check(fx: &Fixture, allowed: &[&str]) {
        let mut inputs = InvariantInputs::new();
        for a in allowed {
            inputs = inputs.allow(*a);
        }
        let report = fx.check_invariants(&inputs);
        println!("{report}");
        report.assert_ok();
    }

    #[test]
    fn moves_by_handle_keeping_identity_and_back() {
        let fx = fixture();
        let dest = fx.root().join("q");
        let before = id(&fx.root().join("proj/cand"));
        let m = move_no_replace(&req(&fx, &dest, before, "0001")).unwrap();
        assert_eq!(m.identity, before);
        assert_eq!(m.kind, EntryKind::Dir);
        assert_eq!(m.source_parent, id(&fx.root().join("proj")));
        assert_eq!(m.dest_dir, id(&dest));
        assert!(!fx.root().join("proj/cand").exists());
        assert_eq!(id(&dest.join("0001")), before, "a rename, not a copy");
        assert_eq!(fs::read(dest.join("0001/sub/f.txt")).unwrap(), b"data");
        // Restore: the same primitive, back into the original directory object.
        let proj = fx.root().join("proj");
        let back = MoveRequest {
            anchor: &dest,
            anchor_identity: Some(id(&dest)),
            relative: Path::new("0001"),
            expected: before,
            expected_kind: Some(EntryKind::Dir),
            dest_dir: &proj,
            dest_dir_identity: Some(m.source_parent),
            dest_name: OsStr::new("cand"),
        };
        move_no_replace(&back).unwrap();
        assert_eq!(id(&fx.root().join("proj/cand")), before);
        check(&fx, &[]);
    }

    #[test]
    fn never_replaces_an_existing_destination() {
        let fx = fixture();
        let dest = fx.root().join("q");
        fs::write(dest.join("0001"), b"keep me").unwrap();
        let before = id(&fx.root().join("proj/cand"));
        let e = move_no_replace(&req(&fx, &dest, before, "0001")).unwrap_err();
        assert!(matches!(e, MoveError::DestinationExists), "{e}");
        assert_eq!(fs::read(dest.join("0001")).unwrap(), b"keep me");
        assert!(fx.root().join("proj/cand/sub/f.txt").is_file());
        check(&fx, &["q/0001"]);
    }

    #[test]
    fn refuses_a_replaced_entry_or_a_junction_in_the_path() {
        let mut fx = fixture();
        let dest = fx.root().join("q");
        let wrong = id(&fx.root().join("proj"));
        let e = move_no_replace(&req(&fx, &dest, wrong, "0001")).unwrap_err();
        assert!(matches!(e, MoveError::IdentityMismatch { .. }), "{e}");

        // `proj` replaced by a junction to the canary's junction target: never traversed.
        let before = id(&fx.root().join("proj/cand"));
        fs::rename(fx.root().join("proj"), fx.root().join("real")).unwrap();
        let target = fx.canary().junction_target.clone();
        if fx.junction("proj", &target).unwrap().is_created() {
            let e = move_no_replace(&req(&fx, &dest, before, "0001")).unwrap_err();
            assert!(matches!(e, MoveError::PathNotTraversable { .. }), "{e}");
            fs::remove_dir(fx.root().join("proj")).unwrap();
        }
        fs::rename(fx.root().join("real"), fx.root().join("proj")).unwrap();
        assert!(fx.root().join("proj/cand/sub/f.txt").is_file());
        assert_eq!(
            fs::read(fx.canary().junction_target.join("keep.txt")).unwrap(),
            b"junction target content\n"
        );
        check(&fx, &[]);
    }

    #[test]
    fn refuses_a_destination_that_is_a_junction_or_was_replaced() {
        let mut fx = fixture();
        let dest = fx.root().join("q");
        let before = id(&fx.root().join("proj/cand"));
        let mut r = req(&fx, &dest, before, "0001");
        r.dest_dir_identity = Some(id(fx.root()));
        assert!(matches!(
            move_no_replace(&r).unwrap_err(),
            MoveError::DestinationChanged { .. }
        ));
        let link: PathBuf = fx.root().join("qlink");
        if fx.junction("qlink", &dest).unwrap().is_created() {
            let r = MoveRequest {
                dest_dir: &link,
                dest_dir_identity: None,
                ..req(&fx, &dest, before, "0001")
            };
            assert!(matches!(
                move_no_replace(&r).unwrap_err(),
                MoveError::DestinationUnavailable { .. }
            ));
        }
        assert!(fx.root().join("proj/cand/sub/f.txt").is_file());
        check(&fx, &["qlink"]);
    }

    #[test]
    fn quarantine_base_is_an_owned_directory_on_the_volume() {
        let fx = fixture();
        let parent = fx.root().join("proj");
        if let Some(base) = quarantine_base(&parent).unwrap() {
            assert!(parent.starts_with(&base), "{base:?}");
            assert!(crate::native::windows::owned_by_current_user(&base).unwrap());
        }
        check(&fx, &[]);
    }
}
