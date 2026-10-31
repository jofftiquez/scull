//! Moving one verified item to the platform Trash / Recycle Bin (P2-EXEC-07; PRD §10.6, §9,
//! PLT-01..03, D-3, D-28).
//!
//! **Only `scull-cleaner`'s Executor may call this** (SAFE-10): like [`crate::remove`], the
//! module exists only with the `delete` cargo feature.
//!
//! # Policy (the caller's)
//!
//! Trash is a **fallback**: S1/S2 items go to same-volume quarantine; only where quarantine is
//! impossible does the Executor try the Trash, and where there is no usable Trash on the
//! item's volume the item is **blocked**. Nothing here ever falls back to permanent deletion,
//! and nothing ever copies across volumes: every move is a single same-volume `rename`, so an
//! item is either fully in the Trash or untouched. Trash moves bytes, it does not free them
//! (§9): [`TrashOutcome`] reports bytes *moved*.
//!
//! # Safety properties (every OS)
//!
//! * The item is identified by its [`FileIdentity`] (and, if given, its [`EntryKind`])
//!   immediately before it is moved; a different object at the path is refused
//!   ([`TrashError::IdentityChanged`]). The moved item keeps its identity.
//! * Links are never followed: a symlink, junction or other reparse point is moved as the link
//!   itself, and on Unix no component between the anchor and the item may be a link.
//! * The trash location is on the item's own volume; otherwise [`TrashError::CrossVolume`] or
//!   [`TrashError::Unavailable`] (the caller blocks the item).
//! * Trash directories scull creates are owner-only; existing ones must be real directories
//!   (not links), owned by the current user and not writable by others.
//! * Metadata (the `.trashinfo` / `$I` record) is created exclusively first, then the item is
//!   renamed in without replacing anything; if the rename fails the metadata is removed.
//!
//! # Per OS
//!
//! * **Linux** (freedesktop.org Trash spec 1.0, PLT-03): items on the volume of
//!   `$XDG_DATA_HOME` go to `$XDG_DATA_HOME/Trash` ("home trash"); others to
//!   `$topdir/.Trash/$uid` when `$topdir/.Trash` is a real directory with the sticky bit set,
//!   else `$topdir/.Trash-$uid` (created `0700`). `info/<name>.trashinfo` is written with
//!   `O_EXCL` first, then the item is `renameat2(RENAME_NOREPLACE)`d to `files/<name>`.
//!   A headless machine without a usable Trash on the volume gets
//!   [`TrashError::Unavailable`] (FX-LNX-01).
//! * **macOS** (PLT-01): `~/.Trash` for the home volume, `<volume>/.Trashes/<uid>` for other
//!   volumes, with `renameatx_np(RENAME_EXCL)`. **Decision:** this is the Trash *directory*
//!   convention, not `NSFileManager.trashItemAtURL`. The ObjC API would need the `objc2`
//!   runtime bindings, is path-based (no identity check between our check and its move), and
//!   always uses the real Trash, so it could not be tested inside a fixture. The directory
//!   approach keeps the same fd-relative, identity-checked, never-copying move as Linux and an
//!   injectable root. **Approximation:** Finder's "Put Back" metadata (kept in the Trash's
//!   private `.DS_Store`) is not written, so the item is restored by dragging it out of the
//!   Trash; the original path is in scull's audit log. Opening `~/.Trash` may need Full Disk
//!   Access (TCC); without it the home Trash is [`TrashError::Unavailable`] and the item is
//!   blocked, never deleted.
//! * **Windows** (PLT-02): the volume's Recycle Bin, `<volume>\$Recycle.Bin\<user SID>\`.
//!   **Decision:** the item is moved there directly, as `$R<id><ext>` with its `$I<id><ext>`
//!   record (format 2, Windows 10+), by renaming the identity-verified handle
//!   (`SetFileInformationByHandle(FileRenameInfo)`, `ReplaceIfExists = FALSE`). Not
//!   `SHFileOperationW(FOF_ALLOWUNDO)` / `IFileOperation`: both re-resolve the path (TOCTOU)
//!   and both **permanently delete** an item the Recycle Bin can't take (too large, a volume
//!   without a bin, "don't move files to the Recycle Bin" policy), silently under `FOF_NO_UI`,
//!   which PRD §10.6 forbids. Explorer lists and restores the item from its `$I` record. A
//!   volume without `$Recycle.Bin` is [`TrashError::Unavailable`].
//!
//! # Testing
//!
//! [`TrashLocations`] injects the home trash and the volume top directory, so tests run inside
//! fixture temp directories and never touch the user's real Trash.

use crate::EntryKind;
use scull_core::{FileIdentity, Restorability};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

mod format;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// Where the Trash lives. [`TrashLocations::from_env`] for the real one; tests inject
/// directories inside their fixture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrashLocations {
    /// The home trash directory: Linux `$XDG_DATA_HOME/Trash` (default
    /// `~/.local/share/Trash`), macOS `~/.Trash`; unused on Windows. Its parent must exist; the
    /// directory itself (and Linux `files/`, `info/`) is created owner-only when missing. Used
    /// for items on the same volume as its parent. `None`: no home trash (a headless account).
    pub home: Option<PathBuf>,
    /// The top directory of the item's volume, where per-volume trash directories live. `None`:
    /// the item's mount point (Windows: its volume root). When set it must be an ancestor of the
    /// item on the same volume (verified by identity), so a test can point it at its fixture.
    pub topdir: Option<PathBuf>,
}

impl TrashLocations {
    /// The current user's real Trash, from the environment (`XDG_DATA_HOME`, `HOME`).
    pub fn from_env() -> Self {
        Self::from_vars(|k| std::env::var_os(k))
    }

    /// [`TrashLocations::from_env`] with an injected environment.
    pub fn from_vars(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Self {
        let absolute = |k: &str| {
            var(k)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute() && !p.as_os_str().is_empty())
        };
        let home = if cfg!(target_os = "macos") {
            absolute("HOME").map(|h| h.join(".Trash"))
        } else if cfg!(windows) {
            None
        } else {
            absolute("XDG_DATA_HOME")
                .or_else(|| absolute("HOME").map(|h| h.join(".local").join("share")))
                .map(|d| d.join("Trash"))
        };
        Self { home, topdir: None }
    }
}

/// One move to the Trash: the item, how to reach it, the identity it must have, and where the
/// Trash is.
#[derive(Debug, Clone, Copy)]
pub struct TrashRequest<'a> {
    /// Trusted, absolute directory the walk starts from (the scan root), opened as given and
    /// checked against [`TrashRequest::anchor_identity`] when set.
    pub anchor: &'a Path,
    /// Identity the opened anchor must have, if known.
    pub anchor_identity: Option<FileIdentity>,
    /// The item relative to the anchor: plain names only (no root, `.` or `..`).
    pub relative: &'a Path,
    /// Device + inode (Windows: volume + file ID) the item must have (SAFE-05).
    pub expected: FileIdentity,
    /// The kind the item must be, if known.
    pub expected_kind: Option<EntryKind>,
    /// Where the Trash is.
    pub locations: &'a TrashLocations,
}

/// Which trash directory received the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrashKind {
    /// The user's home trash (Linux `$XDG_DATA_HOME/Trash`, macOS `~/.Trash`).
    Home,
    /// A shared, administrator-created per-volume trash, in the user's own sub-directory
    /// (Linux `$topdir/.Trash/$uid`, macOS `<volume>/.Trashes/<uid>`).
    VolumeShared,
    /// The user's own per-volume trash (Linux `$topdir/.Trash-$uid`).
    VolumeUser,
    /// The volume's Windows Recycle Bin (`$Recycle.Bin\<SID>`).
    RecycleBin,
}

impl TrashKind {
    /// Stable lowercase code.
    pub fn as_str(self) -> &'static str {
        match self {
            TrashKind::Home => "home",
            TrashKind::VolumeShared => "volume_shared",
            TrashKind::VolumeUser => "volume_user",
            TrashKind::RecycleBin => "recycle_bin",
        }
    }
}

/// Size of what was moved, measured (no-follow, on the item's volume) just before the move.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MovedSize {
    /// Entries moved (the item and everything below it).
    pub entries: u64,
    /// Sum of logical sizes of the non-directories.
    pub logical_bytes: u64,
    /// Sum of allocated sizes of the non-directories: the bytes that stay on the volume, now in
    /// the Trash (§9: **moved, not freed**).
    pub allocated_bytes: u64,
}

/// An item now in the Trash.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TrashOutcome {
    /// Which trash.
    pub kind: TrashKind,
    /// The trash directory used (Linux: the one holding `files/` and `info/`).
    pub trash_dir: PathBuf,
    /// Where the item is now, when known (Linux `files/<name>`, macOS `.Trash/<name>`,
    /// Windows `$R…`).
    pub location: Option<PathBuf>,
    /// The metadata record written for it (Linux `.trashinfo`, Windows `$I…`); `None` on macOS.
    pub info_file: Option<PathBuf>,
    /// The moved size. Trash moves bytes, it never frees them (§9).
    pub moved: MovedSize,
    /// How to get it back: always [`Restorability::ViaTrash`] (the platform Trash, outside
    /// scull's control).
    pub restorable_via: Restorability,
}

impl TrashOutcome {
    /// Bytes moved into the Trash (allocated size): reported as "moved", never as "freed".
    pub fn moved_bytes(&self) -> u64 {
        self.moved.allocated_bytes
    }
}

/// Why an item was not moved. Except for [`TrashError::ReplacedDuringMove`], **nothing was
/// moved**; in no case was anything deleted.
#[derive(Debug)]
#[non_exhaustive]
pub enum TrashError {
    /// The request itself is invalid (relative anchor, `..`, empty path, NUL byte).
    InvalidRequest {
        /// What is wrong.
        reason: &'static str,
    },
    /// The anchor could not be opened as a directory.
    AnchorUnavailable {
        /// The anchor.
        anchor: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// The opened anchor is not the expected directory.
    AnchorChanged {
        /// Expected identity.
        expected: FileIdentity,
        /// Found identity.
        found: FileIdentity,
    },
    /// A component between the anchor and the item is a link (refused, never followed), is
    /// missing or is not accessible.
    PathNotTraversable {
        /// The anchor-relative path up to and including the failing component.
        component: PathBuf,
        /// The OS error.
        source: io::Error,
    },
    /// Nothing exists at the item's path.
    TargetMissing,
    /// The item could not be examined.
    TargetUnavailable {
        /// The OS error.
        source: io::Error,
    },
    /// The item is a different object than planned (SAFE-05).
    IdentityChanged {
        /// Expected identity.
        expected: FileIdentity,
        /// Found identity.
        found: FileIdentity,
    },
    /// The item is not of the expected kind.
    KindMismatch {
        /// Expected kind.
        expected: EntryKind,
        /// Found kind.
        found: EntryKind,
    },
    /// The item is a mount point; it is never moved.
    TargetIsMountPoint,
    /// The only trash available is on another volume: moving there would be a copy, which is
    /// never done. The caller blocks the item.
    CrossVolume {
        /// The item's volume.
        item_volume: u64,
        /// The trash's volume.
        trash_volume: u64,
    },
    /// There is no usable Trash for this item (none on its volume, a check failed, or the
    /// filesystem lacks a no-replace rename). The caller blocks the item.
    Unavailable {
        /// Why, for the report.
        reason: String,
    },
    /// The rename itself failed (e.g. permission denied in a read-only parent). Nothing was
    /// moved and the metadata record was removed.
    MoveFailed {
        /// The OS error.
        source: io::Error,
    },
    /// After the move, the trashed entry was not the planned object: the name was re-pointed
    /// between the last check and the rename. `put_back`: it was moved back to where it came
    /// from; otherwise it stays in the Trash (restorable, not deleted).
    ReplacedDuringMove {
        /// Identity of what was moved.
        found: FileIdentity,
        /// Whether it was moved back.
        put_back: bool,
    },
}

impl TrashError {
    /// Whether the item must be **blocked** because no Trash can take it (PRD §10.6: then
    /// blocked, never permanently deleted).
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self,
            TrashError::Unavailable { .. } | TrashError::CrossVolume { .. }
        )
    }
}

impl fmt::Display for TrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrashError::InvalidRequest { reason } => write!(f, "invalid Trash request: {reason}"),
            TrashError::AnchorUnavailable { anchor, source } => {
                write!(f, "cannot open anchor {}: {source}", anchor.display())
            }
            TrashError::AnchorChanged { expected, found } => {
                write!(f, "anchor changed (expected {expected}, found {found})")
            }
            TrashError::PathNotTraversable { component, source } => write!(
                f,
                "cannot traverse {} without following a link: {source}",
                component.display()
            ),
            TrashError::TargetMissing => f.write_str("item does not exist"),
            TrashError::TargetUnavailable { source } => write!(f, "item unavailable: {source}"),
            TrashError::IdentityChanged { expected, found } => write!(
                f,
                "item is a different object than planned (expected {expected}, found {found})"
            ),
            TrashError::KindMismatch { expected, found } => {
                write!(f, "item is a {found}, expected a {expected}")
            }
            TrashError::TargetIsMountPoint => f.write_str("item is a mount point; never moved"),
            TrashError::CrossVolume {
                item_volume,
                trash_volume,
            } => write!(
                f,
                "the Trash is on another volume ({trash_volume:x}, item on {item_volume:x}); \
                 never copied across volumes, item blocked"
            ),
            TrashError::Unavailable { reason } => {
                write!(f, "no usable Trash for this item ({reason}); item blocked")
            }
            TrashError::MoveFailed { source } => {
                write!(f, "moving the item to the Trash failed: {source}")
            }
            TrashError::ReplacedDuringMove { found, put_back } => write!(
                f,
                "the item was replaced during the move ({found} was moved instead; {})",
                if *put_back {
                    "moved back"
                } else {
                    "left in the Trash"
                }
            ),
        }
    }
}

impl std::error::Error for TrashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TrashError::AnchorUnavailable { source, .. }
            | TrashError::PathNotTraversable { source, .. }
            | TrashError::TargetUnavailable { source }
            | TrashError::MoveFailed { source } => Some(source),
            _ => None,
        }
    }
}

/// Moves the item of `req` to the platform Trash (see the module docs).
pub fn move_to_trash(req: &TrashRequest<'_>) -> Result<TrashOutcome, TrashError> {
    if !req.anchor.is_absolute() {
        return Err(TrashError::InvalidRequest {
            reason: "the anchor must be an absolute path",
        });
    }
    #[cfg(unix)]
    return unix::move_to_trash(req);
    #[cfg(windows)]
    return windows::move_to_trash(req);
}

/// The relative path as plain names, or why it is not acceptable.
fn plain_components(relative: &Path) -> Result<Vec<&std::ffi::OsStr>, TrashError> {
    use std::path::Component;
    let mut out = Vec::new();
    for c in relative.components() {
        match c {
            Component::Normal(name) => out.push(name),
            _ => {
                return Err(TrashError::InvalidRequest {
                    reason: "the item path must be plain names relative to the anchor",
                });
            }
        }
    }
    if out.is_empty() {
        return Err(TrashError::InvalidRequest {
            reason: "the relative path is empty (the anchor itself is never moved)",
        });
    }
    Ok(out)
}

/// Measures `path` (no-follow) and everything below it on the same volume, through the
/// read-only [`Platform`](crate::Platform) calls. Entries on another volume (mount points) are
/// not descended into. Returns the root's identity as seen by this walk, so the caller can
/// check it is the planned object.
fn measure(path: &Path) -> io::Result<(Option<FileIdentity>, MovedSize)> {
    use crate::{Native, Platform};
    let to_io = |e: crate::PlatformError| io::Error::other(e.to_string());
    let root = Native.metadata(path).map_err(to_io)?;
    let volume = root.identity.map(|i| i.volume);
    let mut size = MovedSize::default();
    let mut add = |m: &crate::Metadata| {
        size.entries += 1;
        if m.kind != EntryKind::Dir {
            size.logical_bytes = size.logical_bytes.saturating_add(m.logical_size);
            size.allocated_bytes = size
                .allocated_bytes
                .saturating_add(m.allocated_size.unwrap_or(m.logical_size));
        }
    };
    add(&root);
    let mut stack = Vec::new();
    if root.kind == EntryKind::Dir {
        stack.push(path.to_path_buf());
    }
    while let Some(dir) = stack.pop() {
        let Ok(entries) = Native.read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path;
            let Ok(m) = Native.metadata(&child) else {
                continue;
            };
            if m.identity.map(|i| i.volume) != volume {
                continue;
            }
            add(&m);
            if m.kind == EntryKind::Dir {
                stack.push(child);
            }
        }
    }
    Ok((root.identity, size))
}
