//! Owner-only directories and files (P1-PLAT-06; DATA-04, SAFE-16, T-33, T-50).
//!
//! scull's database, quarantine folders and plan files must not be readable by other users of
//! a shared machine. These helpers create them with owner-only permissions *at creation*, so
//! there is never a moment where the entry exists with broader access (no create-then-chmod
//! race), and check existing entries.
//!
//! # Unix (Linux, macOS)
//!
//! Directories are created with mode `0700`, files with `0600`, passed to `mkdir(2)` /
//! `open(2)` itself. The umask can only remove bits, so the result is never broader. The
//! check requires no group/other bits and that the entry is owned by the effective user.
//! With POSIX default ACLs (Linux), the creation mode becomes the ACL mask, so a mode of
//! `0600`/`0700` also masks every named ACL entry to nothing. macOS extended ACLs
//! (`chmod +a`) are not inspected.
//!
//! # Windows
//!
//! Entries are created with an explicit security descriptor: owner = the current user, and a
//! *protected* DACL (inheritance from the parent disabled) with exactly two
//! `FILE_ALL_ACCESS` grants:
//!
//! * **the current user** (the token's user SID, not the Administrators group an elevated
//!   token would make the default owner);
//! * **`SYSTEM`** (`S-1-5-18`). SYSTEM is the OS itself and can already bypass any DACL
//!   through its privileges, so excluding it protects against no one (the threat is other
//!   *users*, S6 in the threat model), while OS components that run as SYSTEM without those
//!   privileges (backup, indexing, antivirus in some configurations) would break. User
//!   profiles grant SYSTEM the same way.
//!
//! **Administrators is deliberately not granted**: another administrator is another user. An
//! administrator can still take ownership (`SeTakeOwnershipPrivilege`), but that is an
//! explicit, auditable act rather than ambient read access.
//!
//! On directories both grants are inheritable (`OBJECT_INHERIT | CONTAINER_INHERIT`), so
//! every file created inside later, including ones scull doesn't create through these
//! helpers (SQLite's `-wal` and `-shm` files), gets the same owner-only DACL; the protected
//! flag stops a permissive parent (e.g. a shared volume granting `Everyone`) from leaking in.
//!
//! The check reads the owner and DACL back with `GetNamedSecurityInfoW` and reports a missing
//! (NULL) DACL, any allow entry for another SID, and any allow entry it can't interpret
//! (object or callback entries). Deny entries never broaden access and are ignored.
//!
//! # API
//!
//! * [`create_dir_owner_only`] (recursive), [`create_file_owner_only`] and
//!   [`ensure_owner_only`] report [`PlatformError`]s with the operation and path; they are
//!   re-exported at the crate root.
//! * [`create_dir`], [`create_file`] and [`restrict`] are the single-step `io::Result`
//!   primitives, keeping the raw [`io::ErrorKind`] (e.g. `AlreadyExists`), with the semantics
//!   `scull-storage`'s `OwnerOnly` trait expects.

use crate::{EntryKind, ErrorKind, Operation, PlatformError, Result, native};
use serde::Serialize;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;

/// One way an entry's permissions are broader than owner-only, as found by
/// [`ensure_owner_only`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Exposure {
    /// Unix: the mode grants the group or others some access. `mode` is the permission bits
    /// (`0o7777` mask), e.g. `0o644`.
    Mode {
        /// Permission bits.
        mode: u32,
    },
    /// Owned by an account other than the current (effective) user, which can change the
    /// permissions at will. Unix: the numeric uid. Windows: the owner's SID string.
    Owner {
        /// The owner.
        owner: String,
    },
    /// Windows: an allow entry grants access to another account or group.
    Grant {
        /// The grantee's SID string, e.g. `S-1-1-0` (Everyone).
        sid: String,
        /// The granted access mask.
        mask: u32,
    },
    /// Windows: the entry has no DACL at all, which grants everyone full access.
    NullDacl,
    /// Windows: an allow entry of a type that can't be checked (object, callback or
    /// conditional entries); counted as an exposure to be safe.
    UncheckedAce {
        /// The `ACE_HEADER::AceType`.
        ace_type: u8,
    },
    /// The path is a link (symlink, junction, reparse point), which can be repointed;
    /// scull's private files and folders must be real ones.
    Link {
        /// What was found.
        found: EntryKind,
    },
}

impl fmt::Display for Exposure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exposure::Mode { mode } => write!(f, "mode {mode:04o} grants group/other access"),
            Exposure::Owner { owner } => write!(f, "owned by {owner}, not the current user"),
            Exposure::Grant { sid, mask } => write!(f, "grants {sid} access (mask {mask:#x})"),
            Exposure::NullDacl => f.write_str("has no DACL (everyone has full access)"),
            Exposure::UncheckedAce { ace_type } => {
                write!(f, "has an access entry of unchecked type {ace_type}")
            }
            Exposure::Link { found } => write!(f, "is a {found}, not a real file or folder"),
        }
    }
}

/// Creates `path` and any missing parents, restricting **only the directories this call
/// creates** to the current user (see the [module docs](self) for the per-OS meaning).
///
/// Existing ancestors are never modified, so calling this for `~/.local/share/scull` never
/// changes the permissions of `~/.local`. If `path` already exists it must be a real
/// directory (not a symlink or junction to one) and is left as it is; use
/// [`ensure_owner_only`] to check it. A missing parent created concurrently by someone else is
/// used as found.
///
/// # Errors
///
/// [`Operation::CreateDir`] with [`ErrorKind::NotADirectory`] if `path` (or a parent this
/// call would create) exists as something other than a directory, or the I/O error.
pub fn create_dir_owner_only(path: &Path) -> Result<()> {
    let op = Operation::CreateDir;
    let mut missing = Vec::new();
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break; // relative path: the current directory exists
        }
        match native::os_metadata(ancestor) {
            Ok(md) => {
                // An existing ancestor may be a link (`~/.local` symlinked to another disk is
                // common), but the directory requested must be a real one.
                if ancestor == path && md.kind != EntryKind::Dir {
                    return Err(PlatformError::with_kind(
                        op,
                        path,
                        ErrorKind::NotADirectory { found: md.kind },
                    ));
                }
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => missing.push(ancestor),
            Err(e) => return Err(PlatformError::from_io(op, ancestor, e)),
        }
    }
    for dir in missing.into_iter().rev() {
        match native::create_dir_owner_only(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let found = native::os_metadata(dir)
                    .map_err(|e| PlatformError::from_io(op, dir, e))?
                    .kind;
                if found != EntryKind::Dir {
                    return Err(PlatformError::with_kind(
                        op,
                        dir,
                        ErrorKind::NotADirectory { found },
                    ));
                }
            }
            Err(e) => return Err(PlatformError::from_io(op, dir, e)),
        }
    }
    Ok(())
}

/// Creates a new file at `path`, readable and writable only by the current user, and opens it
/// for reading and writing.
///
/// `create_new` semantics: fails with [`io::ErrorKind::AlreadyExists`] (inside
/// [`ErrorKind::Io`]) if anything exists at `path`, including a symlink, which is never
/// followed. The permissions are applied by the creating call itself (Unix mode `0600`;
/// Windows owner-only security descriptor), never by a later `chmod`. The parent directory
/// must exist.
pub fn create_file_owner_only(path: &Path) -> Result<File> {
    native::create_file_owner_only(path)
        .map_err(|e| PlatformError::from_io(Operation::CreateFile, path, e))
}

/// Opens the existing regular file at `path` for reading **without following a link** at its
/// last component (Unix `O_NOFOLLOW`; Windows `FILE_FLAG_OPEN_REPARSE_POINT`, a reparse point is
/// refused), and returns it with the identity read from the opened handle. Used to read a file
/// scull wrote itself (the headroom reserve's marker) knowing it is that object.
pub fn open_file_no_follow(path: &Path) -> Result<(File, Option<scull_core::FileIdentity>)> {
    native::open_file_no_follow(path)
        .map_err(|e| PlatformError::from_io(Operation::OpenFile, path, e))
}

/// Creates one directory at `path` (the parent must exist) with owner-only permissions at
/// creation. Fails with [`io::ErrorKind::AlreadyExists`] if anything exists at `path`,
/// including a symlink, which is never followed.
///
/// The `io::Result` primitive behind [`create_dir_owner_only`], for callers that need the
/// raw error kind (e.g. `scull-storage`'s `OwnerOnly` adapter).
pub fn create_dir(path: &Path) -> io::Result<()> {
    native::create_dir_owner_only(path)
}

/// Exclusively creates a new file at `path` with owner-only permissions at creation (Unix
/// `O_CREAT | O_EXCL | O_NOFOLLOW`, mode `0600`), opened for reading and writing. Fails with
/// [`io::ErrorKind::AlreadyExists`] if anything exists at `path`, including a symlink.
///
/// The `io::Result` primitive behind [`create_file_owner_only`].
pub fn create_file(path: &Path) -> io::Result<File> {
    native::create_file_owner_only(path)
}

/// Narrows an existing file or directory to owner-only, never widening anything and never
/// following a link.
///
/// * **Unix:** `mode & 0o700` (group, other, setuid/setgid and sticky bits cleared), applied
///   with `fchmod` on a descriptor opened `O_NOFOLLOW | O_NONBLOCK` and verified to be the
///   entry that was checked. Requires read permission on the entry.
/// * **Windows:** replaces the DACL with the protected owner-only one (current user and
///   SYSTEM; inheritable on a directory, re-propagated to the inherited entries inside),
///   through a no-follow handle, and makes the current user the owner if the handle can be
///   opened with `WRITE_OWNER` (otherwise the owner is left unchanged).
///
/// Symlinks, junctions, other reparse points and special files (FIFOs, sockets, devices) are
/// refused with [`io::ErrorKind::InvalidInput`] (or the OS's no-follow error) and left
/// untouched.
pub fn restrict(path: &Path) -> io::Result<()> {
    native::restrict_owner_only(path)
}

/// Checks that `path` is accessible only by the current user (and, on Windows, `SYSTEM`).
///
/// The path itself is inspected without following a link; a link is reported as
/// [`Exposure::Link`]. Nothing is modified.
///
/// # Errors
///
/// [`Operation::CheckPermissions`] with [`ErrorKind::NotOwnerOnly`] listing every
/// [`Exposure`] found, or the I/O error if the permissions can't be read.
pub fn ensure_owner_only(path: &Path) -> Result<()> {
    let op = Operation::CheckPermissions;
    let kind = native::os_metadata(path)
        .map_err(|e| PlatformError::from_io(op, path, e))?
        .kind;
    let exposures = if kind.is_link() {
        vec![Exposure::Link { found: kind }]
    } else {
        native::owner_only_exposures(path).map_err(|e| PlatformError::from_io(op, path, e))?
    };
    if exposures.is_empty() {
        Ok(())
    } else {
        Err(PlatformError::with_kind(
            op,
            path,
            ErrorKind::NotOwnerOnly { exposures },
        ))
    }
}

/// Whether `path` (not followed) carries an extended ACL its permission bits don't show:
/// macOS ACLs (`chmod +a`, `ls -le`), of any kind, allow or deny. `false` on Linux (a POSIX
/// ACL's mask shows in the group bits, see [`ensure_owner_only`]) and on Windows (the DACL is
/// what [`ensure_owner_only`] reads). For callers that must be sure no other account can write
/// an entry (the auto-policy config check, P5-PRO-02) and so treat any such ACL as a grant.
///
/// # Errors
///
/// [`Operation::CheckPermissions`] with the I/O error (e.g. the path does not exist).
pub fn has_extended_acl(path: &Path) -> Result<bool> {
    native::has_extended_acl(path)
        .map_err(|e| PlatformError::from_io(Operation::CheckPermissions, path, e))
}
