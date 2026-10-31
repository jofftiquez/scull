//! Volumes, mount points and mount/volume boundaries (P1-PLAT-03; T-12, D-22, PRD §8.3, §9).
//!
//! A candidate that crosses a mount or volume boundary is S3 (§8.3): a bind mount, network
//! mount, Docker volume or Btrfs subvolume inside a `target/` must never be deleted with it
//! (T-12, FX-FS-04, FX-LNX-06). Two signals cover that:
//!
//! * [`is_mount_boundary`]: the parent and child entry live on different volumes (their
//!   [`FileIdentity::volume`](scull_core::FileIdentity) differs). This catches every mounted
//!   filesystem and Btrfs subvolumes (each has its own device number), without any extra
//!   system call: the scanner already has both [`Metadata`] values.
//! * [`mounts_within`]: mount points listed by the OS that lie inside a directory. This also
//!   catches **bind mounts of the same filesystem**, which keep the device number.
//!
//! [`volumes`], [`volume_of`] and [`free_space`] describe the volumes themselves: their
//! [`VolumeKind`] decides whether they are scanned at all (D-22: external drives yes, network
//! drives off by default, Time Machine backup volumes never; see [`VolumeKind::scan_policy`]),
//! [`FsFeatures`] feeds the physical-reclaim estimator (clones, snapshots and compression make
//! predictions less certain, §9), and free space is sampled for measured deltas (PHY-02) and
//! history.
//!
//! # Per OS
//!
//! * **Linux:** `/proc/self/mountinfo` is read and parsed (no process is spawned); the volume
//!   ID is the mount's `major:minor`, which is the `st_dev` of every file on it. Sizes come
//!   from `statvfs`. Network filesystems (`nfs`, `cifs`, `sshfs`, every `fuse.*`, ...) are
//!   never `statvfs`ed while listing, because a stale network mount can block forever (T-53);
//!   neither are `autofs` mount points, which would trigger the automount.
//! * **macOS:** `getfsstat(MNT_NOWAIT)` returns every mount with cached sizes and never blocks
//!   on a network volume. `MNT_LOCAL` and `MNT_RDONLY` give network and read-only volumes;
//!   Time Machine volumes are recognized by name and by their root markers.
//! * **Windows:** `FindFirstVolumeW` / `GetVolumePathNamesForVolumeNameW` enumerate local
//!   volumes with all their mount points (drive letters and mounted folders),
//!   `GetLogicalDriveStringsW` adds mapped network drives. `GetDriveTypeW`,
//!   `GetVolumeInformationW` and `GetDiskFreeSpaceExW` describe each one. The volume ID is
//!   read from a handle on the volume root exactly as [`Metadata::identity`] reads it, so the
//!   two always match. Mapped network drives are listed without being queried (only their
//!   drive type), so a disconnected share can't stall the listing.

pub(crate) mod mountinfo;

use crate::{Metadata, Operation, PlatformError, Result};
use serde::Serialize;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// What kind of storage a volume is. Decides whether it is scanned (D-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VolumeKind {
    /// A local disk volume (internal drive, or a fixed external drive Windows reports as
    /// fixed).
    Local,
    /// Removable or external media: USB sticks, SD cards, optical discs, external drives
    /// (Linux: sysfs `removable` / USB, or mounted under `/media` or `/run/media`; macOS: a
    /// local volume under `/Volumes`; Windows: `DRIVE_REMOVABLE` / `DRIVE_CDROM`).
    Removable,
    /// A network filesystem: NFS, SMB/CIFS, sshfs, WebDAV, every FUSE filesystem on Linux,
    /// mapped network drives on Windows. Off by default: slow, may stall (T-53), and its
    /// contents belong to another machine.
    Network,
    /// A pseudo or virtual filesystem with no disk behind it, or a layered one: `proc`,
    /// `sysfs`, `tmpfs`, `devtmpfs`, `overlay`, `autofs`, macOS `devfs`, Windows RAM disks.
    Virtual,
    /// A backup volume: macOS Time Machine destinations and snapshot mounts. Never scanned
    /// (D-22).
    Backup,
}

impl VolumeKind {
    /// Whether scull scans this volume (D-22).
    pub fn scan_policy(self) -> ScanPolicy {
        match self {
            VolumeKind::Local | VolumeKind::Removable => ScanPolicy::Scan,
            VolumeKind::Network | VolumeKind::Virtual => ScanPolicy::OffByDefault,
            VolumeKind::Backup => ScanPolicy::Never,
        }
    }

    /// Stable lowercase name, as used in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            VolumeKind::Local => "local",
            VolumeKind::Removable => "removable",
            VolumeKind::Network => "network",
            VolumeKind::Virtual => "virtual",
            VolumeKind::Backup => "backup",
        }
    }
}

impl fmt::Display for VolumeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a volume is scanned (D-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ScanPolicy {
    /// Scanned when connected (local and removable volumes).
    Scan,
    /// Skipped unless the user opts in (network and virtual filesystems).
    OffByDefault,
    /// Never scanned, whatever the configuration (backup volumes).
    Never,
}

/// Filesystem features that change how much space a deletion really frees (PRD §9).
///
/// `None` means unknown: callers treat it like "may be present" and lower confidence; they
/// never treat it as "absent" (PLT-08).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub struct FsFeatures {
    /// Files can share extents: APFS clones, Btrfs/XFS reflinks, ReFS / Dev Drive block
    /// cloning. Deleting a clone may free nothing.
    pub clones: Option<bool>,
    /// The filesystem supports snapshots (APFS local snapshots, Btrfs/ZFS snapshots, NTFS/ReFS
    /// shadow copies). Deleted data may stay pinned until snapshots expire.
    pub snapshots: Option<bool>,
    /// Transparent compression (APFS, HFS+, NTFS, Btrfs, ZFS): allocated size is below the
    /// logical size.
    pub compression: Option<bool>,
    /// Names are case-sensitive. On Windows this is the default behaviour of the volume;
    /// per-directory case sensitivity (WSL) is not reflected.
    pub case_sensitive: Option<bool>,
}

impl FsFeatures {
    const fn new(
        clones: Option<bool>,
        snapshots: Option<bool>,
        compression: Option<bool>,
        case_sensitive: Option<bool>,
    ) -> Self {
        Self {
            clones,
            snapshots,
            compression,
            case_sensitive,
        }
    }
}

/// One mounted volume.
///
/// The same volume mounted at several places (Windows drive letter plus a mounted folder,
/// Linux bind mounts) is reported once per mount point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct VolumeInfo {
    /// Where the volume is mounted: `/`, `/home`, `/Volumes/USB`, `C:\`, `D:\mnt\data\`.
    pub mount_point: PathBuf,
    /// What is mounted, for display: a device (`/dev/sda1`, `/dev/disk3s5`), a share
    /// (`//server/share`, `server:/export`), a Windows volume GUID path
    /// (`\\?\Volume{...}\`), or the filesystem name for pseudo filesystems.
    pub source: String,
    /// Volume identifier, equal to [`FileIdentity::volume`](scull_core::FileIdentity) of
    /// every entry on this volume (Unix `st_dev`; Windows volume serial number). `None`
    /// when it was not queried (Windows mapped network drives in [`volumes`]) or could not
    /// be read.
    pub volume_id: Option<u64>,
    /// Filesystem type as the OS names it: `ext4`, `btrfs`, `nfs4`, `fuse.sshfs`, `apfs`,
    /// `smbfs`, `NTFS`, `ReFS`, `exFAT`. `unknown` when it was not queried.
    pub fs_type: String,
    /// What kind of storage this is.
    pub kind: VolumeKind,
    /// Mounted read-only.
    pub read_only: bool,
    /// Total size in bytes, when known. Not queried for network volumes in [`volumes`].
    pub total_bytes: Option<u64>,
    /// Bytes available to the current user, when known (what [`free_space`] returns).
    pub free_bytes: Option<u64>,
    /// Features that matter for physical reclaim (§9).
    pub supports: FsFeatures,
}

/// Every mounted volume, in the OS's order (Linux: mount order, so a later entry at the same
/// mount point hides an earlier one).
///
/// Never blocks on a network filesystem: sizes of network volumes are left unknown (see the
/// [module docs](self)).
pub fn volumes() -> Result<Vec<VolumeInfo>> {
    crate::native::os_volumes()
        .map_err(|e| PlatformError::from_io(Operation::ListVolumes, Path::new(""), e))
}

/// The volume `path` lives on.
///
/// `path` must exist. A symlink or junction at its final component is not followed: the
/// volume reported is the one holding the link itself. Earlier components are resolved.
/// On Linux, a Btrfs subvolume that is not mounted on its own is reported as part of the
/// mount that contains it (its `st_dev` still differs; use [`is_mount_boundary`]).
pub fn volume_of(path: &Path) -> Result<VolumeInfo> {
    crate::native::os_volume_of(path)
        .map_err(|e| PlatformError::from_io(Operation::VolumeOf, path, e))
}

/// Bytes available to the current user on the volume holding `path` (Unix `statvfs`/`statfs`
/// `f_bavail`; Windows `GetDiskFreeSpaceExW`, honouring quotas). Used for measured
/// free-space deltas (PHY-02) and history samples.
///
/// `path` must exist. Unlike [`volumes`], this queries network volumes too; call it only for
/// a path the user chose.
pub fn free_space(path: &Path) -> Result<u64> {
    crate::native::os_free_space(path)
        .map_err(|e| PlatformError::from_io(Operation::FreeSpace, path, e))
}

/// Whether going from `parent` to `child` (an entry directly inside it) crosses a mount or
/// volume boundary (T-12, §8.3: a candidate containing one is S3).
///
/// True when the two volume IDs differ, **or when either identity is unknown**: a boundary
/// that can't be ruled out is treated as one (PLT-08). Bind mounts of the same filesystem
/// keep the volume ID; find those with [`mounts_within`].
pub fn is_mount_boundary(parent: &Metadata, child: &Metadata) -> bool {
    match (parent.identity, child.identity) {
        (Some(p), Some(c)) => p.volume != c.volume,
        _ => true,
    }
}

/// The volumes from `volumes` whose mount point lies strictly inside `dir`, e.g. bind mounts,
/// network mounts or mounted folders under a candidate (T-12).
///
/// Comparison is by path components, after stripping Windows verbatim prefixes and ignoring
/// ASCII case on Windows. Pass `dir` in canonical form (as the OS lists mount points: no
/// symlinks, no `..`).
pub fn mounts_within<'a>(dir: &Path, volumes: &'a [VolumeInfo]) -> Vec<&'a VolumeInfo> {
    let base = comparable(dir);
    volumes
        .iter()
        .filter(|v| {
            let mount = comparable(&v.mount_point);
            mount.len() > base.len() && mount.starts_with(&base)
        })
        .collect()
}

/// Index of the volume in `volumes` whose mount point is the longest prefix of `path`; the
/// last one wins among equal mount points (it hides the earlier ones).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn longest_mount_prefix<'a, I>(path: &Path, mount_points: I) -> Option<usize>
where
    I: IntoIterator<Item = &'a Path>,
{
    let target = comparable(path);
    let mut best: Option<(usize, usize)> = None;
    for (i, mount) in mount_points.into_iter().enumerate() {
        let mount = comparable(mount);
        if target.starts_with(&mount) && best.is_none_or(|(_, len)| mount.len() >= len) {
            best = Some((i, mount.len()));
        }
    }
    best.map(|(i, _)| i)
}

/// Path components in a form that compares like the OS compares mount points.
fn comparable(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for component in path.components() {
        let part = match component {
            Component::Prefix(prefix) => {
                let raw = prefix.as_os_str().to_string_lossy();
                let stripped = raw
                    .strip_prefix(r"\\?\UNC\")
                    .map(|rest| format!(r"\\{rest}"))
                    .or_else(|| raw.strip_prefix(r"\\?\").map(str::to_owned))
                    .unwrap_or_else(|| raw.into_owned());
                stripped.replace('/', "\\")
            }
            Component::RootDir => String::from("/"),
            Component::Normal(name) => name.to_string_lossy().into_owned(),
            Component::CurDir => continue,
            Component::ParentDir => String::from(".."),
        };
        out.push(if cfg!(windows) {
            part.to_ascii_lowercase()
        } else {
            part
        });
    }
    out
}

/// The path whose volume `volume_of` reports: `path` resolved to an absolute path without
/// symlinks, except that a link at the final component is kept (its parent is resolved).
pub(crate) fn resolve_for_volume(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let md = crate::native::os_metadata(&absolute)?;
    if md.kind.is_link() {
        match (absolute.parent(), absolute.file_name()) {
            (Some(parent), Some(name)) => Ok(std::fs::canonicalize(parent)?.join(name)),
            _ => Ok(absolute),
        }
    } else {
        std::fs::canonicalize(&absolute)
    }
}

// ---------------------------------------------------------------------------------------------
// Classification (pure functions, unit-tested on every OS)
// ---------------------------------------------------------------------------------------------

/// Linux: the kind of a mount from its filesystem type and mount point.
/// `removable_device` is the sysfs verdict for the backing block device.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_kind(fs_type: &str, mount_point: &Path, removable_device: bool) -> VolumeKind {
    const NETWORK: &[&str] = &[
        "nfs",
        "nfs4",
        "cifs",
        "smb3",
        "smbfs",
        "ncpfs",
        "sshfs",
        "9p",
        "afs",
        "ceph",
        "glusterfs",
        "lustre",
        "davfs",
        "fuse",
        "gfs2",
        "ocfs2",
        "beegfs",
        "gpfs",
        "orangefs",
    ];
    const VIRTUAL: &[&str] = &[
        "proc",
        "sysfs",
        "tmpfs",
        "ramfs",
        "devtmpfs",
        "devpts",
        "cgroup",
        "cgroup2",
        "securityfs",
        "debugfs",
        "tracefs",
        "pstore",
        "bpf",
        "mqueue",
        "hugetlbfs",
        "configfs",
        "fusectl",
        "binfmt_misc",
        "autofs",
        "efivarfs",
        "nsfs",
        "rpc_pipefs",
        "nfsd",
        "selinuxfs",
        "overlay",
        "aufs",
        "shiftfs",
        "rootfs",
        "devfs",
    ];
    if NETWORK.contains(&fs_type) || fs_type.starts_with("fuse.") {
        // Every FUSE filesystem (`fuse.sshfs`, `fuse.rclone`, `fuse.gvfsd-fuse`, ...) may be
        // backed by the network and may stall; `fuseblk` (ntfs-3g, exfat-fuse on a local
        // block device) is not matched here.
        VolumeKind::Network
    } else if VIRTUAL.contains(&fs_type) {
        VolumeKind::Virtual
    } else if removable_device
        || mount_point.starts_with("/media")
        || mount_point.starts_with("/run/media")
    {
        VolumeKind::Removable
    } else {
        VolumeKind::Local
    }
}

/// Linux: features by filesystem type.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn linux_features(fs_type: &str) -> FsFeatures {
    let (t, f) = (Some(true), Some(false));
    match fs_type {
        "btrfs" | "bcachefs" => FsFeatures::new(t, t, t, t),
        // Reflinks are on by default since xfsprogs 5.1 but can't be confirmed without an
        // ioctl; reporting them makes the estimate more cautious, never less.
        "xfs" => FsFeatures::new(t, f, f, t),
        // OpenZFS 2.2+ has block cloning; older pools don't.
        "zfs" => FsFeatures::new(None, t, t, t),
        "ext2" | "ext3" | "ext4" | "jfs" | "reiserfs" => FsFeatures::new(f, f, f, t),
        "f2fs" => FsFeatures::new(f, f, t, t),
        "tmpfs" | "ramfs" => FsFeatures::new(f, f, f, t),
        "vfat" | "msdos" | "exfat" | "fat" => FsFeatures::new(f, f, f, f),
        "ntfs" | "ntfs3" => FsFeatures::new(f, f, t, f),
        "iso9660" | "udf" | "squashfs" => FsFeatures::new(f, f, None, None),
        _ => FsFeatures::default(),
    }
}

/// macOS `statfs` flag bits (values from `<sys/mount.h>`), so classification can be tested
/// on every OS.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod mac_flags {
    /// `MNT_RDONLY`.
    pub(crate) const RDONLY: u32 = 0x0000_0001;
    /// `MNT_LOCAL`.
    pub(crate) const LOCAL: u32 = 0x0000_1000;
}

/// Names of files at the root of a Time Machine destination (HFS+ `Backups.backupdb`; the
/// marker that hides backup disks from the Finder).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const TIME_MACHINE_ROOT_MARKERS: &[&str] =
    &["Backups.backupdb", ".com.apple.timemachine.donotpresent"];

/// Whether a macOS mount is a Time Machine volume by its names: snapshot mounts under
/// `/Volumes/.timemachine`, `com.apple.TimeMachine.*` mounts (local snapshots, network backup
/// bundles).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn is_time_machine_mount(mount_point: &Path, source: &str) -> bool {
    let mount = mount_point.to_string_lossy();
    mount.starts_with("/Volumes/.timemachine")
        || mount.contains("com.apple.TimeMachine")
        || source.contains("com.apple.TimeMachine")
}

/// macOS: the kind of a mount. `has_backup_marker` is whether a
/// [`TIME_MACHINE_ROOT_MARKERS`] entry exists at its root.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_kind(
    fs_type: &str,
    mount_point: &Path,
    source: &str,
    flags: u32,
    has_backup_marker: bool,
) -> VolumeKind {
    const NETWORK: &[&str] = &["smbfs", "nfs", "afpfs", "webdav", "ftp", "cifs"];
    const VIRTUAL: &[&str] = &["devfs", "autofs", "fdesc", "nullfs", "lifs"];
    if has_backup_marker || is_time_machine_mount(mount_point, source) {
        VolumeKind::Backup
    } else if VIRTUAL.contains(&fs_type) {
        VolumeKind::Virtual
    } else if NETWORK.contains(&fs_type) || flags & mac_flags::LOCAL == 0 {
        VolumeKind::Network
    } else if mount_point.starts_with("/Volumes") {
        // Local volumes the user attaches (external drives, SD cards, disk images) are
        // mounted under /Volumes; the system's own volumes are at / and /System/Volumes/*.
        VolumeKind::Removable
    } else {
        VolumeKind::Local
    }
}

/// macOS: features by filesystem type; `case_sensitive` comes from
/// `pathconf(_PC_CASE_SENSITIVE)`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn macos_features(fs_type: &str, case_sensitive: Option<bool>) -> FsFeatures {
    let (t, f) = (Some(true), Some(false));
    match fs_type {
        "apfs" => FsFeatures::new(t, t, t, case_sensitive),
        "hfs" => FsFeatures::new(f, f, t, case_sensitive),
        "msdos" | "exfat" => FsFeatures::new(f, f, f, f),
        "devfs" | "autofs" => FsFeatures::new(f, f, f, case_sensitive),
        _ => FsFeatures::new(None, None, None, case_sensitive),
    }
}

/// Windows `GetDriveTypeW` results and `GetVolumeInformationW` flags (values from the
/// Windows SDK), so classification can be tested on every OS.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod win {
    /// `DRIVE_UNKNOWN`.
    #[allow(dead_code)] // matched by the `_` arm of `windows_kind`; named for tests
    pub(crate) const DRIVE_UNKNOWN: u32 = 0;
    /// `DRIVE_NO_ROOT_DIR`.
    pub(crate) const DRIVE_NO_ROOT_DIR: u32 = 1;
    /// `DRIVE_REMOVABLE`.
    pub(crate) const DRIVE_REMOVABLE: u32 = 2;
    /// `DRIVE_FIXED`.
    #[allow(dead_code)] // matched by the `_` arm of `windows_kind`; named for tests
    pub(crate) const DRIVE_FIXED: u32 = 3;
    /// `DRIVE_REMOTE`.
    pub(crate) const DRIVE_REMOTE: u32 = 4;
    /// `DRIVE_CDROM`.
    pub(crate) const DRIVE_CDROM: u32 = 5;
    /// `DRIVE_RAMDISK`.
    pub(crate) const DRIVE_RAMDISK: u32 = 6;
    /// `FILE_FILE_COMPRESSION`.
    pub(crate) const FILE_FILE_COMPRESSION: u32 = 0x0000_0010;
    /// `FILE_READ_ONLY_VOLUME`.
    pub(crate) const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;
    /// `FILE_SUPPORTS_BLOCK_REFCOUNTING` (ReFS and Dev Drive block cloning).
    pub(crate) const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
}

/// Windows: the kind of a volume from `GetDriveTypeW`. `None` for a path with no volume
/// behind it (`DRIVE_NO_ROOT_DIR`).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_kind(drive_type: u32) -> Option<VolumeKind> {
    match drive_type {
        win::DRIVE_NO_ROOT_DIR => None,
        win::DRIVE_REMOVABLE | win::DRIVE_CDROM => Some(VolumeKind::Removable),
        win::DRIVE_REMOTE => Some(VolumeKind::Network),
        win::DRIVE_RAMDISK => Some(VolumeKind::Virtual),
        // DRIVE_FIXED, and DRIVE_UNKNOWN treated as an ordinary local disk.
        _ => Some(VolumeKind::Local),
    }
}

/// Windows: features from the filesystem name and `GetVolumeInformationW` flags.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn windows_features(fs_name: &str, flags: u32) -> FsFeatures {
    let has = |bit: u32| Some(flags & bit != 0);
    match fs_name.to_ascii_uppercase().as_str() {
        // NTFS and ReFS support Volume Shadow Copy snapshots; ReFS (and Dev Drive, which is
        // ReFS) reports block cloning through FILE_SUPPORTS_BLOCK_REFCOUNTING.
        "NTFS" | "REFS" => FsFeatures::new(
            has(win::FILE_SUPPORTS_BLOCK_REFCOUNTING),
            Some(true),
            has(win::FILE_FILE_COMPRESSION),
            Some(false),
        ),
        "FAT" | "FAT32" | "EXFAT" => {
            FsFeatures::new(Some(false), Some(false), Some(false), Some(false))
        }
        _ => FsFeatures::new(
            has(win::FILE_SUPPORTS_BLOCK_REFCOUNTING),
            None,
            has(win::FILE_FILE_COMPRESSION),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::FileIdentity;

    fn md(identity: Option<FileIdentity>) -> Metadata {
        Metadata {
            kind: crate::EntryKind::Dir,
            identity,
            logical_size: 0,
            allocated_size: None,
            nlink: None,
            mtime_unix: None,
            readonly: false,
            file_attributes: None,
            reparse_tag: None,
        }
    }

    fn vol(mount: &str) -> VolumeInfo {
        VolumeInfo {
            mount_point: PathBuf::from(mount),
            source: String::new(),
            volume_id: None,
            fs_type: String::from("ext4"),
            kind: VolumeKind::Local,
            read_only: false,
            total_bytes: None,
            free_bytes: None,
            supports: FsFeatures::default(),
        }
    }

    #[test]
    fn boundary_by_volume_id() {
        let a = md(Some(FileIdentity::new(1, 10)));
        let a2 = md(Some(FileIdentity::new(1, 11)));
        let b = md(Some(FileIdentity::new(2, 10)));
        assert!(!is_mount_boundary(&a, &a2));
        assert!(is_mount_boundary(&a, &b));
        // Unknown identity: can't rule a boundary out, so it counts as one (PLT-08).
        assert!(is_mount_boundary(&a, &md(None)));
        assert!(is_mount_boundary(&md(None), &a));
    }

    #[test]
    fn scan_policy_follows_d22() {
        assert_eq!(VolumeKind::Local.scan_policy(), ScanPolicy::Scan);
        assert_eq!(VolumeKind::Removable.scan_policy(), ScanPolicy::Scan);
        assert_eq!(VolumeKind::Network.scan_policy(), ScanPolicy::OffByDefault);
        assert_eq!(VolumeKind::Virtual.scan_policy(), ScanPolicy::OffByDefault);
        assert_eq!(VolumeKind::Backup.scan_policy(), ScanPolicy::Never);
    }

    #[cfg(unix)]
    #[test]
    fn mounts_within_finds_nested_mounts_only() {
        let vols = [
            vol("/"),
            vol("/home"),
            vol("/home/u/proj/target/cache"),
            vol("/home/u/proj/target"),
            vol("/home/u/projector"),
        ];
        let found: Vec<_> = mounts_within(Path::new("/home/u/proj"), &vols)
            .into_iter()
            .map(|v| v.mount_point.clone())
            .collect();
        assert_eq!(
            found,
            [
                PathBuf::from("/home/u/proj/target/cache"),
                PathBuf::from("/home/u/proj/target")
            ]
        );
        // The directory's own mount point is not "within" it.
        assert!(mounts_within(Path::new("/home/u/proj/target"), &vols).len() == 1);
    }

    /// T-12 / FX-FS-04 / FX-LNX-06 (platform part): a bind mount of the same Btrfs filesystem
    /// keeps the device number, so the identity check alone misses it; the mount table finds
    /// it inside the candidate. The NFS mount and the subvolume differ by device as well.
    #[cfg(unix)]
    #[test]
    fn bind_mount_inside_candidate_found_by_mount_table() {
        let table = mountinfo::parse(
            b"29 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
              31 29 0:35 /@home /home rw - btrfs /dev/nvme0n1p2 rw,subvol=/@home\n\
              43 31 0:35 /@home/u/src /home/u/proj/target/src rw - btrfs /dev/nvme0n1p2 rw\n\
              44 31 0:52 / /home/u/proj/target/nfs rw - nfs4 srv:/x rw\n",
        );
        let vols: Vec<VolumeInfo> = table
            .iter()
            .map(|e| VolumeInfo {
                volume_id: Some(e.dev()),
                kind: linux_kind(&e.fs_type, Path::new("/"), false),
                ..vol(&String::from_utf8_lossy(&e.mount_point))
            })
            .collect();
        let inside = mounts_within(Path::new("/home/u/proj/target"), &vols);
        let points: Vec<_> = inside.iter().map(|v| v.mount_point.clone()).collect();
        assert_eq!(
            points,
            [
                PathBuf::from("/home/u/proj/target/src"),
                PathBuf::from("/home/u/proj/target/nfs")
            ]
        );
        // The bind mount shares /home's volume ID: only the table reveals it.
        assert_eq!(inside[0].volume_id, vols[1].volume_id);
        assert_eq!(inside[1].kind, VolumeKind::Network);
        assert_ne!(inside[1].volume_id, vols[1].volume_id);
    }

    #[cfg(windows)]
    #[test]
    fn mounts_within_ignores_case_and_verbatim_prefix() {
        let vols = [vol(r"C:\"), vol(r"C:\Dev\Proj\mnt\")];
        let found = mounts_within(Path::new(r"\\?\c:\dev\proj"), &vols);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mount_point, PathBuf::from(r"C:\Dev\Proj\mnt\"));
        assert_eq!(
            longest_mount_prefix(
                Path::new(r"\\?\C:\dev\proj\mnt\x"),
                vols.iter().map(|v| v.mount_point.as_path())
            ),
            Some(1)
        );
    }

    #[cfg(unix)]
    #[test]
    fn longest_prefix_prefers_deepest_then_latest() {
        let mounts = ["/", "/home", "/home/u", "/home", "/homeless"];
        let pick = |p: &str| longest_mount_prefix(Path::new(p), mounts.iter().map(Path::new));
        assert_eq!(pick("/home/u/x"), Some(2));
        // Two mounts at /home: the later one hides the earlier.
        assert_eq!(pick("/home/v"), Some(3));
        assert_eq!(pick("/homeless/a"), Some(4));
        assert_eq!(pick("/etc"), Some(0));
        assert_eq!(
            longest_mount_prefix(Path::new("/x"), std::iter::empty()),
            None
        );
    }

    #[test]
    fn linux_kinds() {
        let p = Path::new;
        for t in [
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
            "sshfs",
            "fuse.sshfs",
            "fuse.rclone",
            "9p",
        ] {
            assert_eq!(
                linux_kind(t, p("/mnt/x"), false),
                VolumeKind::Network,
                "{t}"
            );
        }
        for t in [
            "proc", "sysfs", "tmpfs", "devtmpfs", "overlay", "autofs", "cgroup2",
        ] {
            assert_eq!(linux_kind(t, p("/x"), false), VolumeKind::Virtual, "{t}");
        }
        assert_eq!(linux_kind("ext4", p("/"), false), VolumeKind::Local);
        assert_eq!(
            linux_kind("fuseblk", p("/mnt/win"), false),
            VolumeKind::Local
        );
        assert_eq!(
            linux_kind("vfat", p("/mnt/usb"), true),
            VolumeKind::Removable
        );
        assert_eq!(
            linux_kind("exfat", p("/run/media/u/SD"), false),
            VolumeKind::Removable
        );
        assert_eq!(
            linux_kind("ext4", p("/media/u/disk"), false),
            VolumeKind::Removable
        );
        // Network wins over the removable hint.
        assert_eq!(
            linux_kind("nfs", p("/media/share"), false),
            VolumeKind::Network
        );
    }

    #[test]
    fn linux_feature_table() {
        assert_eq!(linux_features("btrfs").clones, Some(true));
        assert_eq!(linux_features("btrfs").snapshots, Some(true));
        assert_eq!(linux_features("xfs").clones, Some(true));
        assert_eq!(linux_features("ext4").clones, Some(false));
        assert_eq!(linux_features("ext4").case_sensitive, Some(true));
        assert_eq!(linux_features("vfat").case_sensitive, Some(false));
        assert_eq!(linux_features("zfs").snapshots, Some(true));
        assert_eq!(linux_features("nfs4"), FsFeatures::default());
    }

    #[test]
    fn macos_kinds() {
        use mac_flags::{LOCAL, RDONLY};
        let p = Path::new;
        assert_eq!(
            macos_kind("apfs", p("/"), "/dev/disk3s1s1", LOCAL | RDONLY, false),
            VolumeKind::Local
        );
        assert_eq!(
            macos_kind(
                "apfs",
                p("/System/Volumes/Data"),
                "/dev/disk3s5",
                LOCAL,
                false
            ),
            VolumeKind::Local
        );
        assert_eq!(
            macos_kind("devfs", p("/dev"), "devfs", LOCAL, false),
            VolumeKind::Virtual
        );
        assert_eq!(
            macos_kind("smbfs", p("/Volumes/share"), "//u@nas/share", 0, false),
            VolumeKind::Network
        );
        assert_eq!(
            macos_kind("nfs", p("/net/x"), "x:/export", LOCAL, false),
            VolumeKind::Network
        );
        assert_eq!(
            macos_kind("exfat", p("/Volumes/SD"), "/dev/disk5s1", LOCAL, false),
            VolumeKind::Removable
        );
        // Time Machine: by root marker, snapshot mount path, or source name.
        assert_eq!(
            macos_kind("hfs", p("/Volumes/Backup"), "/dev/disk6s2", LOCAL, true),
            VolumeKind::Backup
        );
        assert_eq!(
            macos_kind(
                "apfs",
                p("/Volumes/.timemachine/ABCD/2026-01-01-000000.backup"),
                "/dev/disk7s1",
                LOCAL | RDONLY,
                false
            ),
            VolumeKind::Backup
        );
        assert_eq!(
            macos_kind(
                "apfs",
                p("/Volumes/com.apple.TimeMachine.localsnapshots/Backups.backupdb/Mac"),
                "com.apple.TimeMachine.2026-01-01-000000.local@/dev/disk3s5",
                LOCAL | RDONLY,
                false
            ),
            VolumeKind::Backup
        );
    }

    #[test]
    fn macos_feature_table() {
        let apfs = macos_features("apfs", Some(false));
        assert_eq!(apfs.clones, Some(true));
        assert_eq!(apfs.snapshots, Some(true));
        assert_eq!(apfs.case_sensitive, Some(false));
        assert_eq!(macos_features("hfs", Some(true)).clones, Some(false));
        assert_eq!(macos_features("smbfs", None), FsFeatures::default());
    }

    #[test]
    fn windows_drive_types() {
        use win::*;
        assert_eq!(windows_kind(DRIVE_FIXED), Some(VolumeKind::Local));
        assert_eq!(windows_kind(DRIVE_UNKNOWN), Some(VolumeKind::Local));
        assert_eq!(windows_kind(DRIVE_REMOVABLE), Some(VolumeKind::Removable));
        assert_eq!(windows_kind(DRIVE_CDROM), Some(VolumeKind::Removable));
        assert_eq!(windows_kind(DRIVE_REMOTE), Some(VolumeKind::Network));
        assert_eq!(windows_kind(DRIVE_RAMDISK), Some(VolumeKind::Virtual));
        assert_eq!(windows_kind(DRIVE_NO_ROOT_DIR), None);
    }

    #[test]
    fn windows_feature_flags() {
        use win::*;
        let ntfs = windows_features("NTFS", FILE_FILE_COMPRESSION);
        assert_eq!(ntfs.clones, Some(false));
        assert_eq!(ntfs.compression, Some(true));
        assert_eq!(ntfs.snapshots, Some(true));
        assert_eq!(ntfs.case_sensitive, Some(false));
        let refs = windows_features("ReFS", FILE_SUPPORTS_BLOCK_REFCOUNTING);
        assert_eq!(refs.clones, Some(true));
        assert_eq!(refs.compression, Some(false));
        let fat = windows_features("FAT32", 0);
        assert_eq!(fat.clones, Some(false));
        assert_eq!(fat.snapshots, Some(false));
        assert_eq!(windows_features("exFAT", 0).compression, Some(false));
        assert_eq!(windows_features("UDF", 0).snapshots, None);
    }

    #[test]
    fn volume_info_serializes() -> std::result::Result<(), serde_json::Error> {
        let mut v = vol("/");
        v.kind = VolumeKind::Network;
        let json = serde_json::to_value(&v)?;
        assert_eq!(json["kind"], "network");
        assert_eq!(json["supports"]["clones"], serde_json::Value::Null);
        Ok(())
    }
}
