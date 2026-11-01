//! All OS-specific code for scull. No other crate may call OS APIs directly (PLT-11).
//!
//! The rest of the engine asks this crate for filesystem *facts* through the [`Platform`]
//! trait and never touches `std::os::unix` / `std::os::windows` itself:
//!
//! * [`Platform::metadata`]: no-follow metadata for one path ([`Metadata`]): the
//!   [`EntryKind`], path-independent [`FileIdentity`](scull_core::FileIdentity), logical and
//!   allocated size, link count, modification time and read-only flag.
//! * [`Platform::read_dir`]: the entries of one directory, each with its no-follow
//!   [`EntryKind`]. Symlinks, junctions and other reparse points are never followed, and FIFOs,
//!   sockets and device nodes are never opened (SAFE-04, SAFE-18).
//! * [`Platform::capabilities`]: which of these signals this OS build actually provides
//!   ([`Capabilities`]), for `scull doctor` and for lowering confidence when a signal is
//!   missing (PLT-08, PLT-09).
//!
//! Besides the trait, the crate owns the other per-user OS conventions:
//!
//! * [`app_dirs()`]: where scull keeps config, data (database, quarantine metadata), cache and
//!   state on each OS ([`AppDirs`]; Windows data never roams, DATA-04). `SCULL_HOME` overrides
//!   all of them for tests.
//! * [`create_dir_owner_only`], [`create_file_owner_only`], [`ensure_owner_only`]: create and
//!   check entries only the current user can access (Unix `0700`/`0600`; Windows protected
//!   DACL for the user and SYSTEM), for the database and quarantine folders (DATA-04,
//!   SAFE-16).
//! * [`is_elevated`] / [`Privileges`]: whether scull runs as root or an elevated
//!   administrator, and the invoking user under `sudo`, for the executor's refusal and
//!   `scull doctor` (SAFE-14). [`Elevation::Unknown`] blocks destructive operations like
//!   [`Elevation::Elevated`].
//! * [`current_user`] / [`machine_id`]: the account scull runs as (`uid:<n>` / the user SID)
//!   and a random per-installation machine ID kept in the data directory, which plans are
//!   bound to (SAFE-11, PRD §10.4).
//! * [`docker`]: the read-only Docker Engine API transport (Unix socket, Windows named pipe,
//!   loopback TCP): `GET` on allow-listed endpoints only, with a hard deadline (P5-DET-03).
//!
//! Use [`Native`] for the real OS. Other crates can implement [`Platform`] with an in-memory
//! fake in their tests.
//!
//! # Platform support
//!
//! * **Unix (Linux, macOS):** complete for this layer. Identity is `st_dev`/`st_ino`, allocated
//!   size is `st_blocks * 512`, metadata comes from `lstat`.
//! * **Windows:** complete for this layer, through handle-based Win32 calls (`windows-sys`)
//!   on handles opened with `FILE_FLAG_OPEN_REPARSE_POINT` and attribute-only access. Identity
//!   is the volume serial number + 128-bit file ID (`FILE_ID_INFO`), allocated size is
//!   `FILE_STANDARD_INFO::AllocationSize` (compressed/sparse files: `FILE_COMPRESSION_INFO`),
//!   link count is `FILE_STANDARD_INFO::NumberOfLinks`. Reparse points are classified by tag:
//!   [`EntryKind::Symlink`], [`EntryKind::Junction`] (junctions and mount points), and
//!   [`EntryKind::ReparsePoint`] with the raw tag in [`Metadata::reparse_tag`] (cloud
//!   placeholders, app execution aliases, ...). Long paths are sent as `\\?\` paths.
//!   `read_dir` enumerates the no-follow handle it checked, so it is race-free.

pub mod activity;
pub mod app_dirs;
pub mod cloud;
pub mod docker;
pub mod elevation;
mod error;
pub mod identity;
mod native;
pub mod owner_only;
pub mod scheduler;
mod types;
pub mod volumes;

use std::path::Path;

pub use app_dirs::{
    APP_NAME, AppDirs, AppDirsError, DirLayout, DirSource, HOME_OVERRIDE_ENV, app_dirs,
};
pub use cloud::{CloudProvider, CloudState, SyncMechanism, SyncedRoot, SyncedRoots};
pub use elevation::{Elevation, Privileges, is_elevated};
pub use error::{ErrorKind, Operation, PlatformError};
pub use identity::{MACHINE_ID_FILE, current_user, machine_id};
pub use native::{Native, native_path_bytes};
pub use owner_only::{Exposure, create_dir_owner_only, create_file_owner_only, ensure_owner_only};
pub use types::{Capabilities, DirEntry, EntryKind, Metadata};
pub use volumes::{
    FsFeatures, ScanPolicy, VolumeInfo, VolumeKind, free_space, is_mount_boundary, mounts_within,
    volume_of, volumes,
};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Result alias for this crate.
pub type Result<T, E = PlatformError> = std::result::Result<T, E>;

/// Iterator over the entries of one directory, as returned by [`Platform::read_dir`].
///
/// Each item is fallible on its own: one unreadable entry does not end the listing.
pub type DirIter<'a> = Box<dyn Iterator<Item = Result<DirEntry>> + Send + 'a>;

/// OS facts the engine needs, behind one interface per OS (PLT-07, PLT-11).
///
/// # Invariants every implementation must keep
///
/// 1. **Never follow links.** Neither method resolves a symlink, junction or other reparse
///    point, at the final path component or in a directory listing. A link is reported as
///    [`EntryKind::Symlink`], [`EntryKind::Junction`] or [`EntryKind::ReparsePoint`] with the
///    link's own metadata.
/// 2. **Never open special files.** FIFOs, sockets and block/character devices are identified
///    from metadata only (SAFE-18). Opening a FIFO can block forever; opening a device can
///    have side effects.
/// 3. **Never hydrate cloud placeholders** (PLT-04): metadata-only calls, no reads of file
///    contents.
/// 4. **Read-only.** Nothing here creates, modifies or deletes anything. Deletion belongs to
///    the executor in `scull-cleaner` (SAFE-10).
/// 5. **Don't guess.** A signal the OS can't provide is `None` and reported as unavailable in
///    [`Platform::capabilities`]; callers then lower confidence or block (PLT-08).
pub trait Platform: Send + Sync {
    /// Which signals this platform provides.
    fn capabilities(&self) -> Capabilities;

    /// Metadata of `path` itself, without following a link at its final component
    /// (`lstat` semantics).
    ///
    /// Errors carry the path and [`Operation::Metadata`]; use [`PlatformError::kind`] to tell
    /// "not found" from "permission denied" (PLT-01: report inaccessible areas, don't fail).
    fn metadata(&self, path: &Path) -> Result<Metadata>;

    /// Lists the directory at `path`.
    ///
    /// `path` itself must be a real directory: if it is a symlink, junction or other reparse
    /// point the call fails with [`ErrorKind::NotADirectory`] instead of following it. Entries
    /// are reported in OS order, without `.` and `..`, each with its no-follow kind.
    fn read_dir<'a>(&'a self, path: &Path) -> Result<DirIter<'a>>;

    /// Metadata of an entry from [`Platform::read_dir`]: exactly what [`Platform::metadata`]
    /// returns for its path (the final component is never followed; same kind, identity,
    /// sizes and times), but possibly faster. On Unix the entry is stat'ed relative to its
    /// listed directory (`fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)`), so the kernel doesn't
    /// resolve the whole path again for every entry, and a parent swapped for a link after the
    /// listing can't redirect the call. On Windows a plain file or directory is queried by name
    /// without opening it (`NtQueryInformationByName(FileStatInformation)`), with the identity
    /// from the listing (`FileIdExtdDirectoryInfo`) and only if the answer has the listed file
    /// ID; reparse points, compressed and sparse files, and anything else are opened as by
    /// [`Platform::metadata`]. On macOS the listing itself (`getattrlistbulk`) supplies the
    /// metadata of a regular file with one link that is neither compressed nor dataless, as it
    /// was when the listing read it (checked against `fstatat` once per listing); everything
    /// else is stat'ed as above. The default is [`Platform::metadata`] of the path.
    ///
    /// **Scan only.** The result may describe the entry as it was listed, so an entry removed
    /// or replaced since can still be reported. Never use it to decide whether something may
    /// be changed: revalidation, the executor, quarantine/restore and purges re-check with
    /// [`Platform::metadata`] or handle-based checks. `scripts/ci/platform_boundary.py` fails
    /// on calls outside `scull-scanner`.
    fn entry_metadata(&self, entry: &DirEntry) -> Result<Metadata> {
        self.metadata(&entry.path)
    }
}
