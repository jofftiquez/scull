//! The sidebar's drives widget (design 01): which mounted volumes to show and how.
//!
//! Read-only: one call to [`scull_platform::volumes()`], which never blocks on network
//! filesystems (their sizes stay unknown).

use schemars::JsonSchema;
use scull_core::{ErrorKind, ScullError};
use scull_platform::{VolumeInfo, VolumeKind};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Volumes smaller than this (EFI partitions, recovery volumes) are left out of the widget.
pub const MIN_SHOWN_BYTES: u64 = 1 << 30;

/// What kind of storage a volume is (mirrors [`VolumeKind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DriveKind {
    /// Internal or fixed disk.
    Local,
    /// Removable or external media.
    Removable,
    /// Network filesystem.
    Network,
    /// Pseudo or virtual filesystem.
    Virtual,
    /// Backup volume (never scanned, D-22).
    Backup,
}

impl DriveKind {
    /// How scull treats a volume of this kind (D-22).
    pub fn scan_policy(self) -> ScanPolicy {
        match self {
            DriveKind::Local | DriveKind::Removable => ScanPolicy::Scan,
            DriveKind::Network | DriveKind::Virtual => ScanPolicy::OffByDefault,
            DriveKind::Backup => ScanPolicy::Never,
        }
    }
}

impl From<VolumeKind> for DriveKind {
    fn from(k: VolumeKind) -> Self {
        match k {
            VolumeKind::Local => DriveKind::Local,
            VolumeKind::Removable => DriveKind::Removable,
            VolumeKind::Network => DriveKind::Network,
            VolumeKind::Backup => DriveKind::Backup,
            // Virtual, and any kind added later: the protective reading (not scanned).
            _ => DriveKind::Virtual,
        }
    }
}

/// Whether scull scans a volume (D-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScanPolicy {
    /// Scanned when connected.
    Scan,
    /// Only on request (network and virtual volumes).
    OffByDefault,
    /// Never (backup volumes).
    Never,
}

/// One volume in the drives widget. Unknown sizes are `null`, never 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Drive {
    /// Where it is mounted (`/`, `/Volumes/Dev SSD`, `C:\`).
    pub mount_point: String,
    /// Short display name.
    pub label: String,
    /// What kind of storage it is.
    pub kind: DriveKind,
    /// Whether scull scans it.
    pub scan_policy: ScanPolicy,
    /// The volume holding the user's home folder (listed first).
    pub holds_home: bool,
    /// Size in bytes, when known.
    pub total_bytes: Option<u64>,
    /// Bytes available to the current user, when known.
    pub free_bytes: Option<u64>,
}

/// The facts [`select_drives`] needs about one mounted volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFacts {
    /// Mount point.
    pub mount_point: PathBuf,
    /// Volume identifier (same for every mount of one volume), when known.
    pub volume_id: Option<u64>,
    /// Kind of storage.
    pub kind: DriveKind,
    /// Size, when known.
    pub total_bytes: Option<u64>,
    /// Free bytes for the user, when known.
    pub free_bytes: Option<u64>,
}

impl From<&VolumeInfo> for VolumeFacts {
    fn from(v: &VolumeInfo) -> Self {
        Self {
            mount_point: v.mount_point.clone(),
            volume_id: v.volume_id,
            kind: v.kind.into(),
            total_bytes: v.total_bytes,
            free_bytes: v.free_bytes,
        }
    }
}

/// `drives`: the mounted volumes worth showing, for the current user.
pub fn drives() -> Result<Vec<Drive>, ScullError> {
    let vols = scull_platform::volumes()
        .map_err(|e| ScullError::new(ErrorKind::Io, format!("cannot list mounted volumes: {e}")))?;
    let facts: Vec<VolumeFacts> = vols.iter().map(VolumeFacts::from).collect();
    Ok(select_drives(
        &facts,
        home_dir().as_deref(),
        MIN_SHOWN_BYTES,
    ))
}

/// The user's home folder, from the environment (`HOME` / `USERPROFILE`).
fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|p| p.is_absolute())
}

/// Pick and order the drives to show:
///
/// * virtual filesystems are left out; so are local and removable volumes known to be smaller
///   than `min_bytes` (EFI and recovery partitions);
/// * a volume mounted in several places is shown once, at its shortest mount point;
/// * known macOS auxiliary mounts are hidden when the system root is shown;
/// * the volume holding `home` comes first, then the other scanned drives, then the ones scull
///   doesn't scan (backup, network), each group by mount point.
pub fn select_drives(vols: &[VolumeFacts], home: Option<&Path>, min_bytes: u64) -> Vec<Drive> {
    let has_system_root = vols.iter().any(|v| {
        v.kind == DriveKind::Local
            && v.mount_point == Path::new("/")
            && v.total_bytes.is_none_or(|t| t >= min_bytes)
    });
    let mut kept: Vec<&VolumeFacts> = Vec::new();
    for v in vols {
        if v.kind == DriveKind::Virtual {
            continue;
        }
        let small = v.total_bytes.is_some_and(|t| t < min_bytes);
        if small && v.kind.scan_policy() == ScanPolicy::Scan {
            continue;
        }
        // macOS auxiliary mounts belong to the system installation, not separate user drives.
        if has_system_root
            && v.kind == DriveKind::Local
            && v.mount_point.parent() == Some(Path::new("/System/Volumes"))
            && matches!(
                v.mount_point.file_name().and_then(|name| name.to_str()),
                Some(
                    "Data"
                        | "Preboot"
                        | "Update"
                        | "VM"
                        | "Recovery"
                        | "xarts"
                        | "iSCPreboot"
                        | "Hardware"
                )
            )
        {
            continue;
        }
        let same = v
            .volume_id
            .and_then(|id| kept.iter().position(|k| k.volume_id == Some(id)));
        match same {
            Some(i) => {
                if depth(&v.mount_point) < depth(&kept[i].mount_point) {
                    kept[i] = v;
                }
            }
            None => kept.push(v),
        }
    }
    // The volume holding home: the longest mount point that contains it.
    let home_mount = home.and_then(|h| {
        kept.iter()
            .filter(|v| h.starts_with(&v.mount_point))
            .max_by_key(|v| depth(&v.mount_point))
            .map(|v| v.mount_point.clone())
    });
    let mut out: Vec<Drive> = kept
        .into_iter()
        .map(|v| Drive {
            mount_point: v.mount_point.display().to_string(),
            label: label(&v.mount_point),
            kind: v.kind,
            scan_policy: v.kind.scan_policy(),
            holds_home: home_mount.as_ref() == Some(&v.mount_point),
            total_bytes: v.total_bytes,
            free_bytes: v.free_bytes,
        })
        .collect();
    out.sort_by(|a, b| {
        let rank = |d: &Drive| match (d.holds_home, d.scan_policy) {
            (true, _) => 0,
            (false, ScanPolicy::Scan) => 1,
            _ => 2,
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.mount_point.cmp(&b.mount_point))
    });
    out
}

fn depth(p: &Path) -> usize {
    p.components().count()
}

/// Display name: the last path component (`Dev SSD`), the drive (`C:`) for a Windows drive
/// root, and `System` for the Unix root.
fn label(mount: &Path) -> String {
    let s = mount.display().to_string();
    let trimmed = s.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return "System".to_owned();
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return trimmed.to_ascii_uppercase();
    }
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(trimmed)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn vol(mount: &str, id: Option<u64>, kind: DriveKind, total: Option<u64>) -> VolumeFacts {
        VolumeFacts {
            mount_point: PathBuf::from(mount),
            volume_id: id,
            kind,
            total_bytes: total,
            free_bytes: total.map(|t| t / 4),
        }
    }

    #[test]
    fn home_volume_first_then_scanned_then_not_scanned() {
        let vols = [
            vol(
                "/Volumes/Backup",
                Some(9),
                DriveKind::Backup,
                Some(2000 * GIB),
            ),
            vol(
                "/Volumes/Dev SSD",
                Some(2),
                DriveKind::Removable,
                Some(1000 * GIB),
            ),
            vol("/", Some(1), DriveKind::Local, Some(494 * GIB)),
            vol("/dev", None, DriveKind::Virtual, None),
            vol("//server/share", None, DriveKind::Network, None),
        ];
        let d = select_drives(&vols, Some(Path::new("/Users/ada")), GIB);
        let mounts: Vec<&str> = d.iter().map(|d| d.mount_point.as_str()).collect();
        assert_eq!(
            mounts,
            ["/", "/Volumes/Dev SSD", "//server/share", "/Volumes/Backup"]
        );
        assert!(d[0].holds_home && !d[1].holds_home);
        assert_eq!(d[0].label, "System");
        assert_eq!(d[1].label, "Dev SSD");
        assert_eq!(d[3].scan_policy, ScanPolicy::Never);
        assert_eq!(d[2].scan_policy, ScanPolicy::OffByDefault);
        // Unknown stays unknown (never 0).
        assert_eq!(d[2].total_bytes, None);
        assert_eq!(d[2].free_bytes, None);
    }

    #[test]
    fn separate_home_partition_holds_home() {
        let vols = [
            vol("/", Some(1), DriveKind::Local, Some(100 * GIB)),
            vol("/home", Some(2), DriveKind::Local, Some(900 * GIB)),
        ];
        let d = select_drives(&vols, Some(Path::new("/home/ada")), GIB);
        assert_eq!(d[0].mount_point, "/home");
        assert!(d[0].holds_home);
        assert_eq!(d[0].label, "home");
    }

    #[test]
    fn bind_mounts_shown_once_at_the_shortest_path() {
        let vols = [
            vol(
                "/var/lib/docker",
                Some(1),
                DriveKind::Local,
                Some(500 * GIB),
            ),
            vol("/", Some(1), DriveKind::Local, Some(500 * GIB)),
            vol("/snap/x", Some(1), DriveKind::Local, Some(500 * GIB)),
        ];
        let d = select_drives(&vols, None, GIB);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].mount_point, "/");
    }

    #[test]
    fn macos_system_volumes_shown_once_as_system() {
        let mut vols = vec![vol("/", Some(1), DriveKind::Local, Some(500 * GIB))];
        for (i, name) in [
            "Data",
            "Preboot",
            "Update",
            "VM",
            "Recovery",
            "xarts",
            "iSCPreboot",
            "Hardware",
        ]
        .iter()
        .enumerate()
        {
            vols.push(vol(
                &format!("/System/Volumes/{name}"),
                Some(i as u64 + 2),
                DriveKind::Local,
                Some(500 * GIB),
            ));
        }

        for home in ["/Users/ada", "/System/Volumes/Data/Users/ada"] {
            let d = select_drives(&vols, Some(Path::new(home)), GIB);
            assert_eq!(d.len(), 1);
            assert_eq!(d[0].mount_point, "/");
            assert_eq!(d[0].label, "System");
            assert!(d[0].holds_home);
            assert_eq!(d[0].total_bytes, Some(500 * GIB));
        }
    }

    #[test]
    fn equal_size_independent_and_special_mounts_stay_visible() {
        let mounts = [
            ("/", DriveKind::Local),
            ("/Volumes/Preboot", DriveKind::Removable),
            ("/Volumes/Other", DriveKind::Local),
            ("/System/Volumes/Projects", DriveKind::Local),
            ("/System/Volumes/Data/External", DriveKind::Local),
            (
                "/Library/Developer/CoreSimulator/Volumes/iOS_23E254a",
                DriveKind::Local,
            ),
            ("/System/Volumes/VM", DriveKind::Network),
            ("/System/Volumes/Preboot", DriveKind::Backup),
        ];
        let vols: Vec<_> = mounts
            .iter()
            .enumerate()
            .map(|(i, (mount, kind))| vol(mount, Some(i as u64 + 1), *kind, Some(500 * GIB)))
            .collect();
        let d = select_drives(&vols, None, GIB);
        assert_eq!(d.len(), mounts.len());
        for (mount, _) in mounts {
            assert!(d.iter().any(|drive| drive.mount_point == mount));
        }
    }

    #[test]
    fn macos_data_mount_kept_without_system_root() {
        let vols = [vol(
            "/System/Volumes/Data",
            Some(2),
            DriveKind::Local,
            Some(500 * GIB),
        )];
        let d = select_drives(
            &vols,
            Some(Path::new("/System/Volumes/Data/Users/ada")),
            GIB,
        );
        assert_eq!(d.len(), 1);
        assert!(d[0].holds_home);
    }

    #[test]
    fn small_partitions_hidden_unknown_sizes_kept() {
        let vols = [
            vol("/boot/efi", Some(3), DriveKind::Local, Some(512 << 20)),
            vol("/mnt/usb", Some(4), DriveKind::Removable, None),
        ];
        let d = select_drives(&vols, None, GIB);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].mount_point, "/mnt/usb");
    }

    #[test]
    fn windows_labels() {
        assert_eq!(label(Path::new("C:\\")), "C:");
        assert_eq!(label(Path::new("d:")), "D:");
        assert_eq!(label(Path::new("D:\\mnt\\data\\")), "data");
        assert_eq!(label(Path::new("/")), "System");
    }

    #[test]
    fn drive_serializes_snake_case_with_nulls() {
        let d = select_drives(&[vol("/net", None, DriveKind::Network, None)], None, GIB);
        let v = serde_json::to_value(&d[0]).unwrap();
        assert_eq!(v["kind"], "network");
        assert_eq!(v["scan_policy"], "off_by_default");
        assert!(v["total_bytes"].is_null());
        assert_eq!(v["holds_home"], false);
    }

    #[test]
    fn real_machine_listing_is_read_only_and_well_formed() {
        // Reads the mount table only (no file is opened for writing).
        if let Ok(list) = drives() {
            for d in list {
                assert!(!d.mount_point.is_empty());
                assert!(d.kind != DriveKind::Virtual);
            }
        }
    }
}
