//! The Windows walker's decision tables and parsers: pure functions on raw values, compiled on
//! every OS so they are unit-tested on Linux and macOS too (the walker itself only runs on
//! Windows). The constants are the Windows SDK values; the Windows tests check each one
//! against `windows-sys`.

use super::SkipReason;
use std::io;

/// Raw Windows values used by the decision tables (and, on Windows, by the walker).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod raw {
    // File attributes.
    pub(crate) const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
    pub(crate) const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    pub(crate) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    pub(crate) const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    pub(crate) const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    pub(crate) const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

    // Reparse tags.
    pub(crate) const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    pub(crate) const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
    /// `IO_REPARSE_TAG_CLOUD_1` … `_F` differ from `IO_REPARSE_TAG_CLOUD` in these bits only.
    pub(crate) const IO_REPARSE_TAG_CLOUD_MASK: u32 = 0x0000_F000;
    pub(crate) const IO_REPARSE_TAG_ONEDRIVE: u32 = 0x8000_0021;

    // NTSTATUS values returned by `NtCreateFile` (as `i32`, like `windows-sys`).
    pub(crate) const STATUS_INVALID_PARAMETER: i32 = 0xC000_000D_u32 as i32;
    pub(crate) const STATUS_ACCESS_DENIED: i32 = 0xC000_0022_u32 as i32;
    pub(crate) const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033_u32 as i32;
    pub(crate) const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;
    pub(crate) const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003A_u32 as i32;
    pub(crate) const STATUS_SHARING_VIOLATION: i32 = 0xC000_0043_u32 as i32;
    pub(crate) const STATUS_DELETE_PENDING: i32 = 0xC000_0056_u32 as i32;
    pub(crate) const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC000_00A2_u32 as i32;
    pub(crate) const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BA_u32 as i32;
    pub(crate) const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103_u32 as i32;
    pub(crate) const STATUS_CANNOT_DELETE: i32 = 0xC000_0121_u32 as i32;
    pub(crate) const STATUS_REPARSE_POINT_ENCOUNTERED: i32 = 0xC000_050B_u32 as i32;
    pub(crate) const STATUS_STOPPED_ON_SYMLINK: i32 = 0x8000_002D_u32 as i32;

    // Win32 error codes (`GetLastError`).
    pub(crate) const ERROR_INVALID_FUNCTION: u32 = 1;
    pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
    pub(crate) const ERROR_PATH_NOT_FOUND: u32 = 3;
    pub(crate) const ERROR_ACCESS_DENIED: u32 = 5;
    pub(crate) const ERROR_NO_MORE_FILES: u32 = 18;
    pub(crate) const ERROR_WRITE_PROTECT: u32 = 19;
    pub(crate) const ERROR_SHARING_VIOLATION: u32 = 32;
    pub(crate) const ERROR_LOCK_VIOLATION: u32 = 33;
    pub(crate) const ERROR_NOT_SUPPORTED: u32 = 50;
    pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
    pub(crate) const ERROR_INVALID_LEVEL: u32 = 124;
    pub(crate) const ERROR_DIR_NOT_EMPTY: u32 = 145;
    pub(crate) const ERROR_BUSY: u32 = 170;
    pub(crate) const ERROR_DELETE_PENDING: u32 = 303;
    pub(crate) const ERROR_USER_MAPPED_FILE: u32 = 1224;
    pub(crate) const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;

    /// `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 42, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
    pub(crate) const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
    /// Largest reparse data buffer (`MAXIMUM_REPARSE_DATA_BUFFER_SIZE`).
    pub(crate) const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;
}

use raw::*;

/// Why a path component cannot be opened relative to a directory handle, if it can't.
///
/// A relative `NtCreateFile` name is passed to the filesystem as is, so a component must be a
/// single plain name: no separator (it would walk further, possibly through a reparse point),
/// no `:` (it would name an alternate data stream of the entry, not the entry), no NUL, not
/// `.` or `..`, and short enough for a `UNICODE_STRING`.
pub(crate) fn validate_name(name: &[u16]) -> Result<(), &'static str> {
    const DOT: u16 = b'.' as u16;
    if name.is_empty() {
        return Err("a path component is empty");
    }
    if name == [DOT] || name == [DOT, DOT] {
        return Err("`.` and `..` are not allowed in the relative path");
    }
    if name
        .iter()
        .any(|&c| c == 0 || c == u16::from(b'\\') || c == u16::from(b'/'))
    {
        return Err("a path component contains a separator or a NUL character");
    }
    if name.contains(&u16::from(b':')) {
        return Err("a path component contains `:` (an alternate data stream, not an entry)");
    }
    // UNICODE_STRING lengths are in bytes, in a u16.
    if name.len() > usize::from(u16::MAX) / 2 {
        return Err("a path component is too long");
    }
    Ok(())
}

/// Whether an entry is a cloud-sync placeholder or would be recalled from remote storage when
/// accessed: OneDrive / Cloud Files placeholders (`IO_REPARSE_TAG_CLOUD*`, the legacy OneDrive
/// tag) and anything with the offline or recall-on-open / recall-on-data-access attributes.
/// The walker never opens, hydrates or removes these (PLT-04, T-52): removing a placeholder
/// would delete the file from the cloud as well.
pub(crate) fn is_cloud_placeholder(attributes: u32, reparse_tag: Option<u32>) -> bool {
    let recall = FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    if attributes & recall != 0 {
        return true;
    }
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        return false;
    }
    matches!(
        reparse_tag,
        Some(tag) if tag & !IO_REPARSE_TAG_CLOUD_MASK == IO_REPARSE_TAG_CLOUD
            || tag == IO_REPARSE_TAG_ONEDRIVE
    )
}

/// Whether the read-only attribute is set. The walker never clears it (and never asks the OS
/// to ignore it): such entries are kept and reported.
pub(crate) fn is_read_only(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_READONLY != 0
}

/// What a failed relative open means for the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenFailure {
    /// The entry is gone (or being deleted by someone else).
    Vanished,
    /// The entry is no longer of the kind it was listed or checked as (a directory became a
    /// file or a reparse point, or the reverse): an anomaly, the walk stops.
    KindChanged,
    /// The entry is kept, for this reason.
    Skip(SkipReason),
}

/// The meaning of an `NtCreateFile` failure (`status`) when opening an entry by name.
pub(crate) fn open_failure(status: i32) -> OpenFailure {
    match status {
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_DELETE_PENDING => {
            OpenFailure::Vanished
        }
        // FILE_DIRECTORY_FILE on a non-directory, FILE_NON_DIRECTORY_FILE on a directory, or a
        // reparse point met where none may be (OBJ_DONT_REPARSE).
        STATUS_NOT_A_DIRECTORY
        | STATUS_FILE_IS_A_DIRECTORY
        | STATUS_REPARSE_POINT_ENCOUNTERED
        | STATUS_STOPPED_ON_SYMLINK => OpenFailure::KindChanged,
        STATUS_ACCESS_DENIED | STATUS_CANNOT_DELETE => {
            OpenFailure::Skip(SkipReason::PermissionDenied)
        }
        STATUS_SHARING_VIOLATION => OpenFailure::Skip(SkipReason::Busy),
        STATUS_MEDIA_WRITE_PROTECTED => OpenFailure::Skip(SkipReason::ReadOnlyFilesystem),
        _ => OpenFailure::Skip(SkipReason::Io),
    }
}

/// Whether `status` means "not found" (the target is missing).
pub(crate) fn is_not_found(status: i32) -> bool {
    matches!(
        status,
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_DELETE_PENDING
    )
}

/// Whether `status` is a sharing violation (another process holds the entry open without
/// sharing the access the walker needs). Retried briefly, then reported, never forced.
pub(crate) fn is_sharing_violation(status: i32) -> bool {
    status == STATUS_SHARING_VIOLATION
}

/// Whether `status` may mean that `OBJ_DONT_REPARSE` is unknown to this Windows build (older
/// Windows 10); the open is then retried without it, like `std::fs::remove_dir_all`. Safety
/// does not depend on the flag: every name is a single component opened with
/// `FILE_OPEN_REPARSE_POINT`, and every handle is checked after the open.
pub(crate) fn may_be_unsupported_dont_reparse(status: i32) -> bool {
    status == STATUS_INVALID_PARAMETER
}

/// Whether a name was refused as invalid by the filesystem (a request error, not a skip).
pub(crate) fn is_invalid_name(status: i32) -> bool {
    status == STATUS_OBJECT_NAME_INVALID
}

/// The skip reason for a Win32 error from a delete (`SetFileInformationByHandle`) or a query.
pub(crate) fn skip_reason(code: u32) -> SkipReason {
    match code {
        ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD => SkipReason::PermissionDenied,
        ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION | ERROR_BUSY | ERROR_USER_MAPPED_FILE => {
            SkipReason::Busy
        }
        ERROR_WRITE_PROTECT => SkipReason::ReadOnlyFilesystem,
        ERROR_DIR_NOT_EMPTY => SkipReason::NotEmpty,
        _ => SkipReason::Io,
    }
}

/// Whether a Win32 error from a delete means the entry was already gone.
pub(crate) fn is_gone(code: u32) -> bool {
    matches!(
        code,
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_DELETE_PENDING
    )
}

/// Whether `FileDispositionInfoEx` (POSIX delete semantics, Windows 10 1709+ on NTFS) is not
/// supported here, so the classic `FileDispositionInfo` must be used instead. Both delete the
/// entry the handle refers to; neither ignores the read-only attribute.
pub(crate) fn disposition_ex_unsupported(code: u32) -> bool {
    matches!(
        code,
        ERROR_INVALID_PARAMETER
            | ERROR_NOT_SUPPORTED
            | ERROR_INVALID_FUNCTION
            | ERROR_INVALID_LEVEL
    )
}

/// Whether a directory-enumeration information class is not supported by the filesystem (e.g.
/// `FileIdExtdDirectoryInfo` on FAT), so the walker lists with `FileFullDirectoryInfo` instead.
pub(crate) fn listing_class_unsupported(code: u32) -> bool {
    matches!(
        code,
        ERROR_INVALID_PARAMETER
            | ERROR_NOT_SUPPORTED
            | ERROR_INVALID_FUNCTION
            | ERROR_INVALID_LEVEL
    )
}

/// Where a mount-point reparse point (`IO_REPARSE_TAG_MOUNT_POINT`) leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountPointTarget {
    /// A directory junction: the substitute name is a drive-letter path (`\??\C:\…`).
    Directory,
    /// A volume mount point (`\??\Volume{GUID}\`), or a target the walker does not recognize
    /// as a plain directory. Never crossed or removed: removing it would unmount a volume.
    Volume,
}

/// Classifies the `REPARSE_DATA_BUFFER` returned by `FSCTL_GET_REPARSE_POINT` for a
/// mount-point reparse point. Anything that cannot be parsed, or is not a mount point, is
/// [`MountPointTarget::Volume`]: the protective answer.
pub(crate) fn mount_point_target(buf: &[u8]) -> MountPointTarget {
    match mount_point_substitute(buf) {
        Some(name) if is_drive_path(&name) => MountPointTarget::Directory,
        _ => MountPointTarget::Volume,
    }
}

fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
    let b = buf.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    let b = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The substitute name of a mount-point `REPARSE_DATA_BUFFER` ([MS-FSCC] 2.1.2.5):
/// `ReparseTag` u32 @0, `ReparseDataLength` u16 @4, `Reserved` u16 @6,
/// `SubstituteNameOffset` u16 @8, `SubstituteNameLength` u16 @10, `PrintNameOffset` u16 @12,
/// `PrintNameLength` u16 @14, `PathBuffer` @16 (offsets and lengths in bytes, relative to
/// `PathBuffer`). Bounds-checked; `None` when malformed or not a mount point.
pub(crate) fn mount_point_substitute(buf: &[u8]) -> Option<Vec<u16>> {
    const PATH_BUFFER: usize = 16;
    if u32_at(buf, 0)? != IO_REPARSE_TAG_MOUNT_POINT {
        return None;
    }
    let data_len = usize::from(u16_at(buf, 4)?);
    // The reparse data (everything after the 8-byte header) must be inside the buffer.
    let data_end = 8usize.checked_add(data_len)?;
    let buf = buf.get(..data_end)?;
    let off = usize::from(u16_at(buf, 8)?);
    let len = usize::from(u16_at(buf, 10)?);
    if !off.is_multiple_of(2) || !len.is_multiple_of(2) {
        return None;
    }
    let start = PATH_BUFFER.checked_add(off)?;
    let bytes = buf.get(start..start.checked_add(len)?)?;
    Some(
        bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect(),
    )
}

/// `\??\X:` or `\??\X:\…`: an NT path to a drive-letter directory.
fn is_drive_path(name: &[u16]) -> bool {
    let prefix: Vec<u16> = r"\??\".encode_utf16().collect();
    name.strip_prefix(prefix.as_slice())
        .is_some_and(drive_rooted)
}

/// `X:` or `X:\…`.
fn drive_rooted(rest: &[u16]) -> bool {
    match rest {
        [letter, colon, tail @ ..] => {
            let letter_ok = u8::try_from(*letter).is_ok_and(|l| l.is_ascii_alphabetic());
            letter_ok
                && *colon == u16::from(b':')
                && tail.first().is_none_or(|&c| c == u16::from(b'\\'))
        }
        _ => false,
    }
}

/// A path as `GetFinalPathNameByHandleW` returns it (`\\?\C:\…`, `\\?\UNC\srv\share\…`),
/// rewritten in plain form for Restart Manager when it fits in `MAX_PATH`; otherwise unchanged.
pub(crate) fn plain_form(path: &[u16]) -> Vec<u16> {
    const MAX_PATH: usize = 260;
    let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    let out: Vec<u16> = if let Some(rest) = path.strip_prefix(unc.as_slice()) {
        r"\\".encode_utf16().chain(rest.iter().copied()).collect()
    } else if let Some(rest) = path.strip_prefix(verbatim.as_slice())
        && drive_rooted(rest)
    {
        rest.to_vec()
    } else {
        return path.to_vec();
    };
    if out.len() < MAX_PATH {
        out
    } else {
        path.to_vec()
    }
}

/// One record of a `FileIdExtdDirectoryInfo` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtdRecord {
    pub(crate) next_entry_offset: u32,
    pub(crate) attributes: u32,
    pub(crate) reparse_tag: u32,
    pub(crate) file_id: u128,
    pub(crate) name: Vec<u16>,
}

/// Field offsets of `FILE_ID_EXTD_DIR_INFO` (checked against `windows-sys` on Windows).
pub(crate) mod extd {
    pub(crate) const NEXT_ENTRY_OFFSET: usize = 0;
    pub(crate) const FILE_ATTRIBUTES: usize = 56;
    pub(crate) const FILE_NAME_LENGTH: usize = 60;
    pub(crate) const REPARSE_POINT_TAG: usize = 68;
    pub(crate) const FILE_ID: usize = 72;
    pub(crate) const FILE_NAME: usize = 88;
}

fn malformed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "malformed directory enumeration record",
    )
}

/// Decodes the `FILE_ID_EXTD_DIR_INFO` record at `offset` with bounds-checked byte reads (no
/// pointer casts). The next record must start after this one's name, which also guarantees
/// progress.
pub(crate) fn parse_extd_record(buf: &[u8], offset: usize) -> io::Result<ExtdRecord> {
    let rec = buf.get(offset..).ok_or_else(malformed)?;
    let next_entry_offset = u32_at(rec, extd::NEXT_ENTRY_OFFSET).ok_or_else(malformed)?;
    let attributes = u32_at(rec, extd::FILE_ATTRIBUTES).ok_or_else(malformed)?;
    let name_len = u32_at(rec, extd::FILE_NAME_LENGTH).ok_or_else(malformed)? as usize;
    let reparse_tag = u32_at(rec, extd::REPARSE_POINT_TAG).ok_or_else(malformed)?;
    let id_bytes: [u8; 16] = rec
        .get(extd::FILE_ID..extd::FILE_ID + 16)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(malformed)?;
    let name_end = extd::FILE_NAME
        .checked_add(name_len)
        .ok_or_else(malformed)?;
    if !name_len.is_multiple_of(2)
        || (next_entry_offset != 0 && (next_entry_offset as usize) < name_end)
    {
        return Err(malformed());
    }
    let name = rec
        .get(extd::FILE_NAME..name_end)
        .ok_or_else(malformed)?
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(ExtdRecord {
        next_entry_offset,
        attributes,
        reparse_tag,
        file_id: u128::from_le_bytes(id_bytes),
        name,
    })
}

/// Whether a listed file ID is a real one: 0 and all ones mean "no stable ID".
pub(crate) fn usable_file_id(id: u128) -> Option<u128> {
    (id != 0 && id != u128::MAX).then_some(id)
}

/// Whether a listed name is `.` or `..` (never processed).
pub(crate) fn is_dot_or_dotdot(name: &[u16]) -> bool {
    const DOT: u16 = b'.' as u16;
    name == [DOT] || name == [DOT, DOT]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn names_must_be_single_plain_components() {
        for bad in [
            "",
            ".",
            "..",
            "a\\b",
            "a/b",
            "a:b",
            "file.txt:stream",
            "x\0y",
            "C:",
        ] {
            assert!(validate_name(&w(bad)).is_err(), "{bad:?}");
        }
        // Reserved device names, trailing dots and spaces are plain names to the NT API
        // (FX-WIN-10); `...` is a name, not `..`.
        for good in ["NUL", "CON.txt", "foo.", "bar ", "...", "a b", "ü", ".git"] {
            assert!(validate_name(&w(good)).is_ok(), "{good:?}");
        }
        assert!(validate_name(&vec![u16::from(b'a'); 40_000]).is_err());
    }

    #[test]
    fn cloud_placeholders_and_recall_attributes_are_recognized() {
        let rp = FILE_ATTRIBUTE_REPARSE_POINT;
        assert!(is_cloud_placeholder(rp, Some(IO_REPARSE_TAG_CLOUD)));
        for n in 1..=0xF_u32 {
            // IO_REPARSE_TAG_CLOUD_1 … _F.
            assert!(
                is_cloud_placeholder(rp, Some(IO_REPARSE_TAG_CLOUD | (n << 12))),
                "{n}"
            );
        }
        assert!(is_cloud_placeholder(rp, Some(IO_REPARSE_TAG_ONEDRIVE)));
        assert!(is_cloud_placeholder(
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
            None
        ));
        assert!(is_cloud_placeholder(FILE_ATTRIBUTE_RECALL_ON_OPEN, None));
        assert!(is_cloud_placeholder(FILE_ATTRIBUTE_OFFLINE, None));
        // Symlinks, junctions, plain files and directories are not placeholders.
        assert!(!is_cloud_placeholder(rp, Some(0xA000_000C)));
        assert!(!is_cloud_placeholder(rp, Some(IO_REPARSE_TAG_MOUNT_POINT)));
        assert!(!is_cloud_placeholder(0, None));
        assert!(!is_cloud_placeholder(FILE_ATTRIBUTE_DIRECTORY, None));
        // A tag without the reparse attribute is ignored (it is EaSize then).
        assert!(!is_cloud_placeholder(0, Some(IO_REPARSE_TAG_CLOUD)));
        assert!(is_read_only(
            FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_DIRECTORY
        ));
        assert!(!is_read_only(FILE_ATTRIBUTE_DIRECTORY));
    }

    #[test]
    fn open_failures_map_to_vanished_anomaly_or_skip() {
        use OpenFailure::*;
        for s in [
            STATUS_OBJECT_NAME_NOT_FOUND,
            STATUS_OBJECT_PATH_NOT_FOUND,
            STATUS_DELETE_PENDING,
        ] {
            assert_eq!(open_failure(s), Vanished, "{s:#x}");
            assert!(is_not_found(s));
        }
        for s in [
            STATUS_NOT_A_DIRECTORY,
            STATUS_FILE_IS_A_DIRECTORY,
            STATUS_REPARSE_POINT_ENCOUNTERED,
            STATUS_STOPPED_ON_SYMLINK,
        ] {
            assert_eq!(open_failure(s), KindChanged, "{s:#x}");
            assert!(!is_not_found(s));
        }
        assert_eq!(
            open_failure(STATUS_SHARING_VIOLATION),
            Skip(SkipReason::Busy)
        );
        assert!(is_sharing_violation(STATUS_SHARING_VIOLATION));
        assert!(!is_sharing_violation(STATUS_ACCESS_DENIED));
        assert_eq!(
            open_failure(STATUS_ACCESS_DENIED),
            Skip(SkipReason::PermissionDenied)
        );
        assert_eq!(
            open_failure(STATUS_CANNOT_DELETE),
            Skip(SkipReason::PermissionDenied)
        );
        assert_eq!(
            open_failure(STATUS_MEDIA_WRITE_PROTECTED),
            Skip(SkipReason::ReadOnlyFilesystem)
        );
        assert_eq!(open_failure(0xC000_0001_u32 as i32), Skip(SkipReason::Io));
        assert!(may_be_unsupported_dont_reparse(STATUS_INVALID_PARAMETER));
        assert!(!may_be_unsupported_dont_reparse(STATUS_ACCESS_DENIED));
        assert!(is_invalid_name(STATUS_OBJECT_NAME_INVALID));
    }

    #[test]
    fn win32_errors_map_to_skip_reasons() {
        assert_eq!(
            skip_reason(ERROR_ACCESS_DENIED),
            SkipReason::PermissionDenied
        );
        assert_eq!(
            skip_reason(ERROR_PRIVILEGE_NOT_HELD),
            SkipReason::PermissionDenied
        );
        for c in [
            ERROR_SHARING_VIOLATION,
            ERROR_LOCK_VIOLATION,
            ERROR_BUSY,
            ERROR_USER_MAPPED_FILE,
        ] {
            assert_eq!(skip_reason(c), SkipReason::Busy, "{c}");
        }
        assert_eq!(
            skip_reason(ERROR_WRITE_PROTECT),
            SkipReason::ReadOnlyFilesystem
        );
        assert_eq!(skip_reason(ERROR_DIR_NOT_EMPTY), SkipReason::NotEmpty);
        assert_eq!(skip_reason(9999), SkipReason::Io);
        for c in [
            ERROR_FILE_NOT_FOUND,
            ERROR_PATH_NOT_FOUND,
            ERROR_DELETE_PENDING,
        ] {
            assert!(is_gone(c));
        }
        assert!(!is_gone(ERROR_ACCESS_DENIED));
        for c in [
            ERROR_INVALID_PARAMETER,
            ERROR_NOT_SUPPORTED,
            ERROR_INVALID_FUNCTION,
            ERROR_INVALID_LEVEL,
        ] {
            assert!(disposition_ex_unsupported(c));
            assert!(listing_class_unsupported(c));
        }
        // Access denied (e.g. read-only) never triggers the fallback.
        assert!(!disposition_ex_unsupported(ERROR_ACCESS_DENIED));
        assert!(!disposition_ex_unsupported(ERROR_SHARING_VIOLATION));
    }

    /// A mount-point REPARSE_DATA_BUFFER with the given substitute and print names.
    fn mount_point_buf(substitute: &str, print: &str) -> Vec<u8> {
        let sub: Vec<u8> = substitute
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let prn: Vec<u8> = print.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut path = sub.clone();
        path.extend([0, 0]);
        let print_off = path.len();
        path.extend(&prn);
        path.extend([0, 0]);
        let data_len = 8 + path.len();
        let mut b = Vec::new();
        b.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        b.extend((data_len as u16).to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b.extend(0u16.to_le_bytes()); // SubstituteNameOffset
        b.extend((sub.len() as u16).to_le_bytes());
        b.extend((print_off as u16).to_le_bytes());
        b.extend((prn.len() as u16).to_le_bytes());
        b.extend(path);
        b
    }

    #[test]
    fn mount_point_reparse_data_tells_junctions_from_volume_mount_points() {
        let j = mount_point_buf(r"\??\C:\Users\me\canary", r"C:\Users\me\canary");
        assert_eq!(
            mount_point_substitute(&j),
            Some(w(r"\??\C:\Users\me\canary"))
        );
        assert_eq!(mount_point_target(&j), MountPointTarget::Directory);
        assert_eq!(
            mount_point_target(&mount_point_buf(r"\??\d:", "")),
            MountPointTarget::Directory
        );
        let v = mount_point_buf(r"\??\Volume{01234567-89ab-cdef-0123-456789abcdef}\", "");
        assert_eq!(mount_point_target(&v), MountPointTarget::Volume);
        // Anything unrecognized is treated as a volume (protective).
        for odd in [
            r"\Device\HarddiskVolume3\x",
            r"\??\UNC\srv\share",
            r"\??\C",
            r"C:\x",
        ] {
            assert_eq!(
                mount_point_target(&mount_point_buf(odd, "")),
                MountPointTarget::Volume,
                "{odd}"
            );
        }
        // Truncated, wrong tag, odd offsets, names running past the data: protective.
        assert_eq!(
            mount_point_target(&j[..j.len() - 6]),
            MountPointTarget::Volume
        );
        assert_eq!(mount_point_target(&[]), MountPointTarget::Volume);
        let mut symlink_tag = j.clone();
        symlink_tag[..4].copy_from_slice(&0xA000_000Cu32.to_le_bytes());
        assert_eq!(mount_point_substitute(&symlink_tag), None);
        let mut odd_off = j.clone();
        odd_off[8] = 1;
        assert_eq!(mount_point_substitute(&odd_off), None);
        let mut long = j.clone();
        long[10..12].copy_from_slice(&0x7FF0u16.to_le_bytes());
        assert_eq!(mount_point_substitute(&long), None);
    }

    #[test]
    fn plain_form_for_restart_manager() {
        let s = |p: &str| String::from_utf16_lossy(&plain_form(&w(p)));
        assert_eq!(s(r"\\?\C:\Users\me\f.txt"), r"C:\Users\me\f.txt");
        assert_eq!(s(r"\\?\UNC\srv\share\f"), r"\\srv\share\f");
        // Volume GUID paths and other verbatim forms are left alone.
        let vol = r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\f";
        assert_eq!(s(vol), vol);
        // Too long for MAX_PATH: unchanged (verbatim keeps it addressable).
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(s(&long), long);
    }

    fn extd_record(name: &str, next: u32, attributes: u32, tag: u32, id: u128) -> Vec<u8> {
        let name: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut b = vec![0u8; extd::FILE_NAME];
        b[extd::NEXT_ENTRY_OFFSET..4].copy_from_slice(&next.to_le_bytes());
        b[extd::FILE_ATTRIBUTES..extd::FILE_ATTRIBUTES + 4]
            .copy_from_slice(&attributes.to_le_bytes());
        b[extd::FILE_NAME_LENGTH..extd::FILE_NAME_LENGTH + 4]
            .copy_from_slice(&(name.len() as u32).to_le_bytes());
        b[extd::REPARSE_POINT_TAG..extd::REPARSE_POINT_TAG + 4].copy_from_slice(&tag.to_le_bytes());
        b[extd::FILE_ID..extd::FILE_ID + 16].copy_from_slice(&id.to_le_bytes());
        b.extend(name);
        b
    }

    #[test]
    fn extd_records_are_parsed_with_bounds_checks() {
        let first = extd_record("ab", 96, FILE_ATTRIBUTE_DIRECTORY, 0, 0x1234_5678_9abc);
        let mut buf = first.clone();
        buf.resize(96, 0);
        buf.extend(extd_record(
            "j",
            0,
            FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT,
            IO_REPARSE_TAG_MOUNT_POINT,
            7,
        ));
        let a = parse_extd_record(&buf, 0).unwrap();
        assert_eq!(a.name, w("ab"));
        assert_eq!(a.file_id, 0x1234_5678_9abc);
        assert_eq!(a.next_entry_offset, 96);
        let b = parse_extd_record(&buf, 96).unwrap();
        assert_eq!(b.name, w("j"));
        assert_eq!(b.reparse_tag, IO_REPARSE_TAG_MOUNT_POINT);
        assert_eq!(b.next_entry_offset, 0);
        // Name past the end, odd name length, overlapping next record, offset out of range.
        assert!(parse_extd_record(&first[..first.len() - 1], 0).is_err());
        let mut odd = first.clone();
        odd[extd::FILE_NAME_LENGTH] = 3;
        assert!(parse_extd_record(&odd, 0).is_err());
        let mut overlap = first.clone();
        overlap[0] = 8;
        assert!(parse_extd_record(&overlap, 0).is_err());
        assert!(parse_extd_record(&buf, buf.len() + 1).is_err());
        assert!(parse_extd_record(&[0u8; 10], 0).is_err());
    }

    #[test]
    fn file_ids_and_dot_names() {
        assert_eq!(usable_file_id(0), None);
        assert_eq!(usable_file_id(u128::MAX), None);
        assert_eq!(usable_file_id(5), Some(5));
        assert!(is_dot_or_dotdot(&w(".")));
        assert!(is_dot_or_dotdot(&w("..")));
        assert!(!is_dot_or_dotdot(&w("...")));
        assert!(!is_dot_or_dotdot(&w(".git")));
    }
}
