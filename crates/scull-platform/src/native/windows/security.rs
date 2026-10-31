//! Windows security and per-user folders: the process token's user SID and elevation
//! (P1-PLAT-04), owner-only security descriptors, reading back an entry's owner and DACL, and
//! known folders (P1-PLAT-06).
//!
//! See [`crate::owner_only`] for the DACL design and its justification.

#![allow(unsafe_code)]

use super::{Handle, metadata_from_handle, open, verbatim};
use crate::{DirSource, EntryKind, Exposure};
use std::ffi::{OsString, c_void};
use std::fs::File;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_QUERY,
    TOKEN_USER, TokenElevation, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};

/// SID string of `NT AUTHORITY\SYSTEM` (LocalSystem).
pub(super) const SYSTEM_SID: &str = "S-1-5-18";

// ---------------------------------------------------------------------------------------------
// Owned system allocations and strings
// ---------------------------------------------------------------------------------------------

/// Memory the system allocated with `LocalAlloc` (security descriptors, SID strings), freed
/// with `LocalFree` on drop.
struct LocalBox(*mut c_void);

impl Drop for LocalBox {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` was allocated by the system with `LocalAlloc` (documented for
            // every API that fills a `LocalBox` here), is owned exclusively by this struct and
            // is freed exactly once.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

/// Reads a NUL-terminated UTF-16 string.
///
/// # Safety
///
/// `s` must be non-null and point to a NUL-terminated UTF-16 string valid for reads.
unsafe fn wide_cstr(s: *const u16) -> OsString {
    let mut len = 0;
    // SAFETY: the caller guarantees a NUL terminator, so every index up to it is in bounds.
    while unsafe { *s.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` u16s starting at `s` were just read one by one.
    OsString::from_wide(unsafe { std::slice::from_raw_parts(s, len) })
}

/// The string form (`S-1-5-…`) of a SID.
///
/// # Safety
///
/// `sid` must point to a valid SID.
unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut out: *mut u16 = ptr::null_mut();
    // SAFETY: the caller guarantees `sid` is valid; `out` is a valid out-pointer. On success
    // the system allocates the string with LocalAlloc.
    if unsafe { ConvertSidToStringSidW(sid, &mut out) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = LocalBox(out.cast());
    // SAFETY: on success `out` is a non-null, NUL-terminated wide string, freed by `owned`
    // after this read.
    let s = unsafe { wide_cstr(out) };
    drop(owned);
    Ok(s.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------------------------
// Process token
// ---------------------------------------------------------------------------------------------

/// This process's access token, opened for queries only.
pub(super) fn process_token() -> io::Result<Handle> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is always valid and needs no
    // closing; `token` is a valid out-pointer. The returned token handle is owned by `Handle`
    // and closed on drop.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(token))
}

/// A variable-length token information buffer, 8-byte aligned (the structures hold pointers).
pub(super) fn token_information(
    token: &Handle,
    class: TOKEN_INFORMATION_CLASS,
) -> io::Result<Vec<u64>> {
    let mut len = 0u32;
    // SAFETY: a null buffer of length 0 is the documented way to query the required size;
    // `len` is a valid out-pointer. The call fails with ERROR_INSUFFICIENT_BUFFER by design.
    unsafe { GetTokenInformation(token.0, class, ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buf` is writable for at least `len` bytes and 8-byte aligned; `token` is open
    // with TOKEN_QUERY.
    let ok = unsafe { GetTokenInformation(token.0, class, buf.as_mut_ptr().cast(), len, &mut len) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf)
}

/// The SID string of the user this process runs as (the token user, not the token's default
/// owner, which is the Administrators group for an elevated administrator).
pub(super) fn current_user_sid() -> io::Result<String> {
    let token = process_token()?;
    let buf = token_information(&token, TokenUser)?;
    if buf.len() * size_of::<u64>() < size_of::<TOKEN_USER>() {
        return Err(io::Error::other("TOKEN_USER buffer too small"));
    }
    // SAFETY: the successful TokenUser query wrote a TOKEN_USER at the start of `buf`, which
    // is large enough (checked) and 8-byte aligned. Its `Sid` points into `buf`, which stays
    // alive until after `sid_string` returns.
    unsafe {
        let user = buf.as_ptr().cast::<TOKEN_USER>().read();
        sid_string(user.User.Sid)
    }
}

/// `TokenElevation` of the process token; [`Elevation::Unknown`](crate::Elevation::Unknown) if
/// the token can't be opened or queried.
pub(crate) fn elevation() -> crate::Elevation {
    let query = || -> io::Result<bool> {
        let token = process_token()?;
        let buf = token_information(&token, TokenElevation)?;
        if buf.len() * size_of::<u64>() < size_of::<TOKEN_ELEVATION>() {
            return Err(io::Error::other("TOKEN_ELEVATION buffer too small"));
        }
        // SAFETY: the successful TokenElevation query wrote a TOKEN_ELEVATION (one u32) at the
        // start of `buf`, which is large enough (checked) and suitably aligned.
        let info = unsafe { buf.as_ptr().cast::<TOKEN_ELEVATION>().read() };
        Ok(info.TokenIsElevated != 0)
    };
    match query() {
        Ok(true) => crate::Elevation::Elevated,
        Ok(false) => crate::Elevation::NotElevated,
        Err(_) => crate::Elevation::Unknown,
    }
}

// ---------------------------------------------------------------------------------------------
// Owner-only creation
// ---------------------------------------------------------------------------------------------

/// The SDDL of an owner-only security descriptor: owner = `user`, protected DACL (no
/// inheritance from the parent) granting full control to `user` and SYSTEM; inheritable to
/// files and subfolders when `dir`.
pub(super) fn owner_only_sddl(user: &str, dir: bool) -> String {
    let inherit = if dir { "OICI" } else { "" };
    format!("O:{user}D:P(A;{inherit};FA;;;{user})(A;{inherit};FA;;;SY)")
}

fn owner_only_descriptor(dir: bool) -> io::Result<LocalBox> {
    let sddl: Vec<u16> = owner_only_sddl(&current_user_sid()?, dir)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is a NUL-terminated UTF-16 string that outlives the call; `sd` is a valid
    // out-pointer; the size out-pointer may be null. On success the system allocates the
    // descriptor with LocalAlloc, owned by the returned `LocalBox`.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalBox(sd))
}

fn attributes(sd: &LocalBox) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    }
}

/// `CreateDirectoryW` with the owner-only descriptor applied at creation. Not recursive.
pub(crate) fn create_dir_owner_only(path: &Path) -> io::Result<()> {
    let wide = verbatim(path)?;
    let sd = owner_only_descriptor(true)?;
    let sa = attributes(&sd);
    // SAFETY: `wide` is NUL-terminated; `sa` and the descriptor it points to (owned by `sd`)
    // outlive the call.
    if unsafe { CreateDirectoryW(wide.as_ptr(), &sa) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `CreateFileW(CREATE_NEW)` with the owner-only descriptor applied at creation.
/// `FILE_FLAG_OPEN_REPARSE_POINT` makes an existing symlink at `path` a name collision rather
/// than something to follow.
pub(crate) fn create_file_owner_only(path: &Path) -> io::Result<File> {
    let wide = verbatim(path)?;
    let sd = owner_only_descriptor(false)?;
    let sa = attributes(&sd);
    // SAFETY: `wide` is NUL-terminated; `sa` and its descriptor outlive the call; a null
    // template handle is allowed. CREATE_NEW never opens or truncates an existing file.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &sa,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid file handle that nothing else owns; `File` takes ownership
    // and closes it on drop.
    Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
}

/// Replaces the DACL of an existing file or directory with the owner-only one (protected, user
/// and SYSTEM; inheritable on a directory, and Windows re-propagates it to the inherited
/// entries of everything inside), and makes the current user the owner when the handle can
/// be opened with `WRITE_OWNER` (e.g. a file an elevated process created, owned by the
/// Administrators group). Without that right the owner is left as it is.
///
/// The entry is opened with a no-follow handle and its kind checked on that handle, so a
/// link is refused rather than having its target modified.
pub(crate) fn restrict_owner_only(path: &Path) -> io::Result<()> {
    let wide = verbatim(path)?;
    let base = FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC;
    let (handle, set_owner) = match open(&wide, base | WRITE_OWNER) {
        Ok(h) => (h, true),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            (open(&wide, base)?, false)
        }
        Err(e) => return Err(e),
    };
    let kind = metadata_from_handle(&handle)?.kind;
    if kind != EntryKind::File && kind != EntryKind::Dir {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to restrict a {kind}: only real files and folders are restricted"),
        ));
    }
    let sd = owner_only_descriptor(kind == EntryKind::Dir)?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut owner: PSID = ptr::null_mut();
    // SAFETY: `sd.0` is a valid self-relative descriptor built above; the out-pointers are
    // valid. `dacl` and `owner` point into `sd`, which outlives their use below.
    let ok = unsafe {
        GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) != 0
            && GetSecurityDescriptorOwner(sd.0, &mut owner, &mut defaulted) != 0
    };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || dacl.is_null() || owner.is_null() {
        return Err(io::Error::other("owner-only descriptor is incomplete"));
    }
    let mut info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    if set_owner {
        info |= OWNER_SECURITY_INFORMATION;
    } else {
        owner = ptr::null_mut();
    }
    // SAFETY: `handle` is open with WRITE_DAC (and WRITE_OWNER when `set_owner`); `dacl` and
    // `owner` are valid (see above) or null when not set; null group and SACL pointers leave
    // those parts unchanged.
    let err = unsafe {
        SetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            info,
            owner,
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Reading back owner and DACL
// ---------------------------------------------------------------------------------------------

/// One entry of a DACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ace {
    /// `ACE_HEADER::AceType`.
    pub(super) ace_type: u8,
    /// `ACE_HEADER::AceFlags` (inheritance flags).
    pub(super) flags: u8,
    /// Access mask.
    pub(super) mask: u32,
    /// Grantee, for plain allow and deny entries.
    pub(super) sid: Option<String>,
}

/// Owner and DACL of an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Security {
    /// Owner SID string.
    pub(super) owner: Option<String>,
    /// `None` for a NULL DACL (everyone has full access).
    pub(super) dacl: Option<Vec<Ace>>,
    /// `SE_DACL_PROTECTED`: the DACL does not inherit from the parent.
    pub(super) protected: bool,
}

/// Owner and DACL of `path` itself, read through a no-follow handle opened with
/// `READ_CONTROL` only.
pub(super) fn security(path: &Path) -> io::Result<Security> {
    let handle = open(&verbatim(path)?, READ_CONTROL)?;
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `handle` is open with READ_CONTROL; the out-pointers are valid; the group and
    // SACL out-pointers may be null. On success `owner` and `dacl` point into `sd`, which the
    // system allocates with LocalAlloc and which `_sd` frees after the last use below.
    let err = unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    let _sd = LocalBox(sd);

    let owner = if owner.is_null() {
        None
    } else {
        // SAFETY: a non-null owner from a successful call is a valid SID inside `sd`.
        Some(unsafe { sid_string(owner) }?)
    };

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `sd` is a valid descriptor (successful call above); out-pointers are valid.
    if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let dacl = if dacl.is_null() {
        None
    } else {
        // SAFETY: a non-null DACL from a successful call is a valid ACL inside `sd`.
        Some(unsafe { read_acl(dacl) }?)
    };

    Ok(Security {
        owner,
        dacl,
        protected: control & SE_DACL_PROTECTED != 0,
    })
}

/// # Safety
///
/// `acl` must point to a valid ACL that stays alive during the call.
unsafe fn read_acl(acl: *const ACL) -> io::Result<Vec<Ace>> {
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `acl` is valid (caller); `info` is a writable ACL_SIZE_INFORMATION of the size
    // passed.
    let ok = unsafe {
        GetAclInformation(
            acl,
            (&raw mut info).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut aces = Vec::with_capacity(info.AceCount as usize);
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `acl` is valid and `index` < AceCount; `ace` is a valid out-pointer.
        if unsafe { GetAce(acl, index, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE starts with an ACE_HEADER; read unaligned to assume nothing.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        let ace_type = u32::from(header.AceType);
        let plain = ace_type == ACCESS_ALLOWED_ACE_TYPE || ace_type == ACCESS_DENIED_ACE_TYPE;
        // Every access ACE type (allow, deny, object, callback) has its mask right after the
        // header; plain allow and deny ACEs (same layout) then hold the SID.
        let mask = if usize::from(header.AceSize) >= offset_of!(ACCESS_ALLOWED_ACE, SidStart) {
            // SAFETY: the ACE is at least as long as header + mask (checked against AceSize).
            unsafe {
                ace.cast::<u8>()
                    .add(offset_of!(ACCESS_ALLOWED_ACE, Mask))
                    .cast::<u32>()
                    .read_unaligned()
            }
        } else {
            0
        };
        let sid = if plain && usize::from(header.AceSize) > offset_of!(ACCESS_ALLOWED_ACE, SidStart)
        {
            // SAFETY: a plain allow/deny ACE holds a SID starting at `SidStart`, inside the
            // valid ACL.
            let sid = unsafe {
                ace.cast::<u8>()
                    .add(offset_of!(ACCESS_ALLOWED_ACE, SidStart))
            };
            // SAFETY: `sid` points to the ACE's SID (see above).
            Some(unsafe { sid_string(sid.cast()) }?)
        } else {
            None
        };
        aces.push(Ace {
            ace_type: header.AceType,
            flags: header.AceFlags,
            mask,
            sid,
        });
    }
    Ok(aces)
}

/// How `security` is broader than owner-only for `user`: owner other than `user`, NULL DACL,
/// allow entries for SIDs other than `user` and SYSTEM, and allow entries of types that carry
/// no plain SID. Deny entries never broaden access.
pub(super) fn exposures(security: &Security, user: &str) -> Vec<Exposure> {
    let mut out = Vec::new();
    match &security.owner {
        Some(owner) if owner == user => {}
        Some(owner) => out.push(Exposure::Owner {
            owner: owner.clone(),
        }),
        None => out.push(Exposure::Owner {
            owner: "(none)".to_owned(),
        }),
    }
    let Some(aces) = &security.dacl else {
        out.push(Exposure::NullDacl);
        return out;
    };
    for ace in aces {
        match u32::from(ace.ace_type) {
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE => match ace.sid.as_deref() {
                _ if ace.mask == 0 => {}
                Some(sid) if sid == user || sid == SYSTEM_SID => {}
                sid => out.push(Exposure::Grant {
                    sid: sid.unwrap_or("(unknown)").to_owned(),
                    mask: ace.mask,
                }),
            },
            _ => out.push(Exposure::UncheckedAce {
                ace_type: ace.ace_type,
            }),
        }
    }
    out
}

pub(crate) fn owner_only_exposures(path: &Path) -> io::Result<Vec<Exposure>> {
    Ok(exposures(&security(path)?, &current_user_sid()?))
}

// ---------------------------------------------------------------------------------------------
// Known folders
// ---------------------------------------------------------------------------------------------

pub(crate) fn known_folder(source: DirSource) -> Option<PathBuf> {
    let id = match source {
        DirSource::RoamingAppData => &FOLDERID_RoamingAppData,
        DirSource::LocalAppData => &FOLDERID_LocalAppData,
        _ => return None,
    };
    let mut out: *mut u16 = ptr::null_mut();
    // SAFETY: `id` is a valid GUID; a null token means the current user; `out` is a valid
    // out-pointer. KF_FLAG_DEFAULT never creates the folder.
    let hr = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, ptr::null_mut(), &mut out) };
    let path = if hr >= 0 && !out.is_null() {
        // SAFETY: on success `out` is a non-null, NUL-terminated wide string.
        Some(PathBuf::from(unsafe { wide_cstr(out) }))
    } else {
        None
    };
    // SAFETY: the caller must free `out` with CoTaskMemFree whether or not the call succeeded;
    // CoTaskMemFree accepts null. `out` is not used afterwards.
    unsafe { CoTaskMemFree(out.cast_const().cast()) };
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{CONTAINER_INHERIT_ACE, INHERITED_ACE, OBJECT_INHERIT_ACE};
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    const EVERYONE: &str = "S-1-1-0";

    fn sids(aces: &[Ace]) -> Vec<String> {
        let mut v: Vec<String> = aces.iter().filter_map(|a| a.sid.clone()).collect();
        v.sort();
        v
    }

    /// Asserts the exact owner-only DACL: two full-control allow entries, user and SYSTEM.
    fn assert_owner_only(sec: &Security, user: &str, flags: u8) {
        assert_eq!(sec.owner.as_deref(), Some(user), "owner");
        let aces = sec.dacl.as_ref().expect("DACL present");
        assert_eq!(aces.len(), 2, "ACE count: {aces:?}");
        let mut want = vec![user.to_owned(), SYSTEM_SID.to_owned()];
        want.sort();
        assert_eq!(sids(aces), want);
        for ace in aces {
            assert_eq!(u32::from(ace.ace_type), ACCESS_ALLOWED_ACE_TYPE, "{ace:?}");
            assert_eq!(ace.mask, FILE_ALL_ACCESS, "{ace:?}");
            assert_eq!(ace.flags, flags, "{ace:?}");
        }
        assert!(
            exposures(sec, user).is_empty(),
            "{:?}",
            exposures(sec, user)
        );
    }

    #[test]
    fn current_user_sid_is_a_sid() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert_ne!(sid, SYSTEM_SID);
    }

    #[test]
    fn sddl_shape() {
        assert_eq!(
            owner_only_sddl("S-1-5-21-1", true),
            "O:S-1-5-21-1D:P(A;OICI;FA;;;S-1-5-21-1)(A;OICI;FA;;;SY)"
        );
        assert_eq!(
            owner_only_sddl("S-1-5-21-1", false),
            "O:S-1-5-21-1D:P(A;;FA;;;S-1-5-21-1)(A;;FA;;;SY)"
        );
    }

    #[test]
    fn created_dir_has_protected_inheritable_owner_only_dacl() {
        let tmp = tempfile::tempdir().unwrap();
        let user = current_user_sid().unwrap();
        let dir = tmp.path().join("private");
        create_dir_owner_only(&dir).unwrap();
        let sec = security(&dir).unwrap();
        assert!(sec.protected, "DACL must not inherit from the parent");
        assert_owner_only(
            &sec,
            &user,
            (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8,
        );
    }

    #[test]
    fn created_file_has_protected_owner_only_dacl() {
        let tmp = tempfile::tempdir().unwrap();
        let user = current_user_sid().unwrap();
        let file = tmp.path().join("private.db");
        drop(create_file_owner_only(&file).unwrap());
        let sec = security(&file).unwrap();
        assert!(sec.protected);
        assert_owner_only(&sec, &user, 0);
    }

    #[test]
    fn files_created_inside_inherit_owner_only() {
        // Like SQLite's -wal/-shm files: created by someone else's code inside our folder.
        let tmp = tempfile::tempdir().unwrap();
        let user = current_user_sid().unwrap();
        let dir = tmp.path().join("private");
        create_dir_owner_only(&dir).unwrap();
        let file = dir.join("db-wal");
        std::fs::write(&file, b"x").unwrap();
        let sec = security(&file).unwrap();
        let aces = sec.dacl.as_ref().unwrap();
        assert_eq!(aces.len(), 2, "{aces:?}");
        let mut want = vec![user.clone(), SYSTEM_SID.to_owned()];
        want.sort();
        assert_eq!(sids(aces), want);
        assert!(aces.iter().all(|a| a.flags & INHERITED_ACE as u8 != 0));
        // The owner is the token's default owner (Administrators when elevated), so only the
        // DACL is asserted here.
        assert!(
            exposures(&sec, &user)
                .iter()
                .all(|e| matches!(e, Exposure::Owner { .. })),
            "{:?}",
            exposures(&sec, &user)
        );
    }

    #[test]
    fn restrict_replaces_an_inherited_dacl() {
        let tmp = tempfile::tempdir().unwrap();
        let user = current_user_sid().unwrap();
        let file = tmp.path().join("plain.db");
        std::fs::write(&file, b"x").unwrap();
        restrict_owner_only(&file).unwrap();
        let sec = security(&file).unwrap();
        assert!(sec.protected);
        let aces = sec.dacl.as_ref().unwrap();
        assert_eq!(aces.len(), 2, "{aces:?}");
        let mut want = vec![user.clone(), SYSTEM_SID.to_owned()];
        want.sort();
        assert_eq!(sids(aces), want);
        assert!(
            aces.iter()
                .all(|a| a.flags == 0 && a.mask == FILE_ALL_ACCESS)
        );
        // The user has full control over a file it just created, so ownership moves to it
        // (from Administrators when elevated, as on CI).
        assert_eq!(sec.owner.as_deref(), Some(user.as_str()));
        assert!(exposures(&sec, &user).is_empty());
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
    }

    #[test]
    fn restrict_on_a_dir_propagates_to_children() {
        let tmp = tempfile::tempdir().unwrap();
        let user = current_user_sid().unwrap();
        let dir = tmp.path().join("plain-dir");
        std::fs::create_dir(&dir).unwrap();
        let child = dir.join("child");
        std::fs::write(&child, b"x").unwrap();
        restrict_owner_only(&dir).unwrap();
        let sec = security(&dir).unwrap();
        assert!(sec.protected);
        let aces = sec.dacl.as_ref().unwrap();
        assert_eq!(aces.len(), 2, "{aces:?}");
        let inherit = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
        assert!(aces.iter().all(|a| a.flags == inherit), "{aces:?}");
        let child_aces = security(&child).unwrap().dacl.unwrap();
        let mut want = vec![user, SYSTEM_SID.to_owned()];
        want.sort();
        assert_eq!(sids(&child_aces), want, "{child_aces:?}");
    }

    #[test]
    fn exposure_rules() {
        let user = "S-1-5-21-9";
        let allow = |sid: &str, mask| Ace {
            ace_type: ACCESS_ALLOWED_ACE_TYPE as u8,
            flags: 0,
            mask,
            sid: Some(sid.to_owned()),
        };
        let sec = |owner: &str, dacl| Security {
            owner: Some(owner.to_owned()),
            dacl,
            protected: true,
        };
        assert!(
            exposures(
                &sec(user, Some(vec![allow(user, 1), allow(SYSTEM_SID, 1)])),
                user
            )
            .is_empty()
        );
        assert_eq!(exposures(&sec(user, None), user), vec![Exposure::NullDacl]);
        assert_eq!(
            exposures(&sec("S-1-5-32-544", Some(vec![])), user),
            vec![Exposure::Owner {
                owner: "S-1-5-32-544".into()
            }]
        );
        assert_eq!(
            exposures(
                &sec(
                    user,
                    Some(vec![allow(EVERYONE, 0x1200a9), allow(EVERYONE, 0)])
                ),
                user
            ),
            vec![Exposure::Grant {
                sid: EVERYONE.into(),
                mask: 0x1200a9
            }]
        );
        let deny = Ace {
            ace_type: ACCESS_DENIED_ACE_TYPE as u8,
            flags: 0,
            mask: 1,
            sid: Some(EVERYONE.into()),
        };
        assert!(exposures(&sec(user, Some(vec![deny])), user).is_empty());
        let object = Ace {
            ace_type: 5,
            flags: 0,
            mask: 1,
            sid: None,
        };
        assert_eq!(
            exposures(&sec(user, Some(vec![object])), user),
            vec![Exposure::UncheckedAce { ace_type: 5 }]
        );
    }

    #[test]
    fn known_folders_resolve_and_differ() {
        let roaming = known_folder(DirSource::RoamingAppData).expect("RoamingAppData");
        let local = known_folder(DirSource::LocalAppData).expect("LocalAppData");
        assert!(roaming.is_absolute() && local.is_absolute());
        assert_ne!(roaming, local, "data must not land in the roaming folder");
        assert_eq!(known_folder(DirSource::Home), None);
    }
}
