//! Emergency headroom (PRO-06, off by default): what a reserve file is, where it lives, how big
//! it may be, and what state it is in. **Read-only**: this module reads scull's manifest and the
//! file's no-follow metadata, and never writes, creates or removes anything. Creating and
//! releasing the reserve is `scull_cleaner::headroom` (the only crate that changes the disk).
//!
//! # The reserve
//!
//! * **Where:** only `<scull data dir>/headroom/reserve.bin`, in scull's own data directory,
//!   so it is on the volume that directory is on. Nothing outside the data directory is ever
//!   created or released.
//! * **What:** a regular file with real allocated blocks. It is filled with bytes that don't
//!   compress, and its allocated size is checked after writing. A sparse, compressed or
//!   deduplicated file would reserve nothing, so creation refuses one.
//! * **Who:** owner-only permissions, like the rest of scull's data.
//! * **Record:** `<data dir>/headroom.json` (the [`HeadroomManifest`]) records the file's
//!   identity, size, allocated bytes, modification time and a **marker**: 32 random bytes
//!   written as the file's first bytes when it was created. scull releases only a file that the
//!   manifest describes: same no-follow identity, kind, size and modification time, **and** the
//!   same marker, read through a handle opened without following links whose identity is
//!   checked again ([`marker_mismatch`]). Identity alone is not enough: filesystems reuse inode
//!   numbers right away, so a file put in the reserve's place can have the old identity.
//! * **Size:** [`DEFAULT_SIZE`] (2 GiB) unless asked otherwise, at least [`MIN_SIZE`], at most
//!   [`MAX_SIZE`] and at most [`MAX_SHARE_PERCENT`] % of the volume. Creation refuses when free
//!   space would drop below the forecast reserve (`[proactive] reserve_gib`).

use schemars::JsonSchema;
use scull_core::FileIdentity;
use scull_platform::{EntryKind, Native, Platform};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// Folder of the reserve inside scull's data directory.
pub const HEADROOM_DIR: &str = "headroom";
/// The reserve file's name inside [`HEADROOM_DIR`].
pub const RESERVE_FILE: &str = "reserve.bin";
/// The manifest's file name in scull's data directory.
pub const MANIFEST_FILE: &str = "headroom.json";
/// Manifest format version (2: the marker and modification time; a version-1 manifest has
/// neither and is refused).
pub const MANIFEST_VERSION: u32 = 2;
/// Bytes of the random marker at the start of the reserve.
pub const MARKER_LEN: usize = 32;
/// Size when none is asked for (PRD: "e.g. 2–5 GB").
pub const DEFAULT_SIZE: u64 = 2 * GIB;
/// Smallest reserve accepted.
pub const MIN_SIZE: u64 = 256 * MIB;
/// Largest reserve accepted.
pub const MAX_SIZE: u64 = 10 * GIB;
/// A reserve never takes more than this share of its volume.
pub const MAX_SHARE_PERCENT: u64 = 5;
/// The allocated size must be at least this share of the logical size (per mille). Filesystems
/// that report a little less (metadata accounting) still pass; compressed, sparse or
/// deduplicated files don't.
pub const MIN_ALLOCATED_PER_MILLE: u64 = 980;

/// The reserve's path relative to the data directory.
pub fn reserve_relative() -> PathBuf {
    Path::new(HEADROOM_DIR).join(RESERVE_FILE)
}

/// What scull recorded when it created the reserve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadroomManifest {
    /// Format version ([`MANIFEST_VERSION`]).
    pub version: u32,
    /// The reserve, relative to the data directory (always [`reserve_relative`]).
    pub relative: String,
    /// Its no-follow identity when created.
    pub identity: FileIdentity,
    /// Its logical size (bytes).
    pub size_bytes: u64,
    /// Its allocated size when created (bytes): the space it really holds.
    pub allocated_bytes: u64,
    /// The volume it is on (mount point).
    pub volume_id: String,
    /// When it was created (Unix seconds).
    pub created_at: i64,
    /// Its modification time when created (Unix seconds), when the OS reports one.
    pub mtime_unix: Option<i64>,
    /// The random marker written as its first [`MARKER_LEN`] bytes, lowercase hex.
    pub marker: String,
}

/// What the reserve looks like now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeadroomState {
    /// No reserve (the default).
    None,
    /// The reserve exists and matches its manifest: it can be released.
    Ready,
    /// The manifest is there but the file is gone (released outside scull, or a release
    /// whose record is missing). Releasing only removes the manifest.
    Missing,
    /// Something at the path differs from the manifest (identity, kind or size): scull will
    /// not release it.
    Changed,
    /// The manifest can't be read: scull will not release anything.
    Unreadable,
    /// A file is at the reserve's path but there is no manifest: scull didn't record it and
    /// will not release it.
    Unrecorded,
}

/// The reserve's status (`scull headroom status`, `scull status`, the desktop).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadroomStatus {
    /// What it looks like.
    pub state: HeadroomState,
    /// The reserve's path.
    pub path: String,
    /// The manifest, when it could be read.
    pub manifest: Option<HeadroomManifest>,
    /// Bytes releasing it would free now (its allocated size), when [`HeadroomState::Ready`].
    pub releasable_bytes: Option<u64>,
    /// Why it isn't ready, in one sentence.
    pub detail: Option<String>,
}

/// The reserve's status in `data_dir`. Read-only: reads the manifest and the no-follow
/// metadata of the reserve's path.
pub fn status(data_dir: &Path) -> HeadroomStatus {
    let path = data_dir.join(reserve_relative());
    let mk = |state, manifest, releasable_bytes, detail: Option<String>| HeadroomStatus {
        state,
        path: path.display().to_string(),
        manifest,
        releasable_bytes,
        detail,
    };
    let native = Native::new();
    let file = native.metadata(&path);
    let manifest = match read_manifest(data_dir) {
        Ok(m) => m,
        Err(e) => return mk(HeadroomState::Unreadable, None, None, Some(e)),
    };
    let Some(manifest) = manifest else {
        return match file {
            Ok(_) => mk(
                HeadroomState::Unrecorded,
                None,
                None,
                Some("a file is at the reserve's path but scull has no record of it".into()),
            ),
            Err(_) => mk(HeadroomState::None, None, None, None),
        };
    };
    match file {
        Err(e) if e.is_not_found() => mk(
            HeadroomState::Missing,
            Some(manifest),
            None,
            Some("the reserve file is gone".into()),
        ),
        Err(e) => mk(
            HeadroomState::Changed,
            Some(manifest),
            None,
            Some(e.to_string()),
        ),
        Ok(md) => match mismatch(&manifest, md.kind, md.identity, md.logical_size)
            .or_else(|| mtime_mismatch(&manifest, md.mtime_unix))
            .or_else(|| marker_mismatch(&path, &manifest))
        {
            Some(why) => mk(HeadroomState::Changed, Some(manifest), None, Some(why)),
            None => {
                let bytes = md.allocated_size.unwrap_or(manifest.allocated_bytes);
                mk(HeadroomState::Ready, Some(manifest), Some(bytes), None)
            }
        },
    }
}

/// Why what is at the path is not the reserve the manifest describes, if it isn't.
pub fn mismatch(
    manifest: &HeadroomManifest,
    kind: EntryKind,
    identity: Option<FileIdentity>,
    logical_size: u64,
) -> Option<String> {
    if manifest.relative != reserve_relative().to_string_lossy() {
        return Some("the manifest names another path".into());
    }
    if kind != EntryKind::File {
        return Some(format!("it is a {}, not the reserve file", kind.as_str()));
    }
    match identity {
        Some(id) if id == manifest.identity => {}
        Some(_) => return Some("it is not the file scull created (identity differs)".into()),
        None => return Some("its identity can't be read".into()),
    }
    if logical_size != manifest.size_bytes {
        return Some(format!(
            "its size changed ({logical_size} bytes, recorded {})",
            manifest.size_bytes
        ));
    }
    None
}

/// Why the modification time says the file was rewritten since it was created, if it does.
pub fn mtime_mismatch(manifest: &HeadroomManifest, mtime_unix: Option<i64>) -> Option<String> {
    match (manifest.mtime_unix, mtime_unix) {
        (Some(recorded), Some(now)) if recorded != now => Some(format!(
            "it was modified after scull created it (mtime {now}, recorded {recorded})"
        )),
        _ => None,
    }
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why the file at `path` is not the one scull created, judged by its content: it is opened
/// without following a link (`scull_platform::open_file_no_follow`), the handle's identity must
/// be the recorded one, and its first [`MARKER_LEN`] bytes must be the recorded marker.
/// Read-only.
pub fn marker_mismatch(path: &Path, manifest: &HeadroomManifest) -> Option<String> {
    use std::io::Read;
    if manifest.marker.len() != MARKER_LEN * 2 {
        return Some("the record has no valid marker".into());
    }
    let (mut f, id) = match scull_platform::open_file_no_follow(path) {
        Ok(x) => x,
        Err(e) => return Some(format!("it can't be opened without following links ({e})")),
    };
    if id != Some(manifest.identity) {
        return Some("the opened file is not the one scull created (identity differs)".into());
    }
    let mut head = [0u8; MARKER_LEN];
    if f.read_exact(&mut head).is_err() {
        return Some("it is too short to hold scull's marker".into());
    }
    if hex(&head) != manifest.marker {
        return Some("it is not the file scull created (marker differs)".into());
    }
    None
}

/// The manifest in `data_dir`: `Ok(None)` when there is none, `Err` when it can't be read or
/// parsed (or is a link, which is never followed).
pub fn read_manifest(data_dir: &Path) -> Result<Option<HeadroomManifest>, String> {
    let path = data_dir.join(MANIFEST_FILE);
    match Native::new().metadata(&path) {
        Err(e) if e.is_not_found() => return Ok(None),
        Err(e) => return Err(e.to_string()),
        Ok(md) if md.kind != EntryKind::File => {
            return Err(format!(
                "{MANIFEST_FILE} is a {}, not a file",
                md.kind.as_str()
            ));
        }
        Ok(md) if md.logical_size > 64 * 1024 => {
            return Err(format!("{MANIFEST_FILE} is too large to be scull's"));
        }
        Ok(_) => {}
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let m: HeadroomManifest =
        serde_json::from_str(&text).map_err(|e| format!("{MANIFEST_FILE}: {e}"))?;
    if m.version != MANIFEST_VERSION {
        return Err(format!(
            "{MANIFEST_FILE} has version {}, this scull reads {MANIFEST_VERSION}",
            m.version
        ));
    }
    Ok(Some(m))
}

/// Why a reserve of `size` bytes can't be created on a volume of `total` bytes with `free`
/// bytes free, keeping `reserve` bytes free (the forecast reserve), if it can't.
pub fn size_refusal(size: u64, total: u64, free: u64, reserve: u64) -> Option<String> {
    size_refusal_within(size, total, free, reserve, MIN_SIZE, MAX_SIZE)
}

/// [`size_refusal`] with explicit size limits (tests use small reserves).
pub fn size_refusal_within(
    size: u64,
    total: u64,
    free: u64,
    reserve: u64,
    min: u64,
    max: u64,
) -> Option<String> {
    let gib = |b: u64| format!("{:.1} GiB", b as f64 / GIB as f64);
    if size < min {
        return Some(format!("a reserve must be at least {}", gib(min)));
    }
    if size > max {
        return Some(format!("a reserve is at most {}", gib(max)));
    }
    if size > total / 100 * MAX_SHARE_PERCENT {
        return Some(format!(
            "a reserve takes at most {MAX_SHARE_PERCENT}% of its volume ({} here)",
            gib(total / 100 * MAX_SHARE_PERCENT)
        ));
    }
    if free < reserve {
        return Some(format!(
            "free space ({}) is already below the {} reserve: free up space first",
            gib(free),
            gib(reserve)
        ));
    }
    if free - reserve < size {
        return Some(format!(
            "a {} reserve would leave {} free, below the {} reserve",
            gib(size),
            gib(free.saturating_sub(size)),
            gib(reserve)
        ));
    }
    None
}

/// Whether `allocated` bytes really hold a reserve of `size` bytes (not sparse, compressed or
/// deduplicated).
pub fn really_allocated(size: u64, allocated: u64) -> bool {
    allocated.saturating_mul(1000) >= size.saturating_mul(MIN_ALLOCATED_PER_MILLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> HeadroomManifest {
        HeadroomManifest {
            version: MANIFEST_VERSION,
            relative: reserve_relative().to_string_lossy().into_owned(),
            identity: FileIdentity::new(1, 2),
            size_bytes: GIB,
            allocated_bytes: GIB,
            volume_id: "/".into(),
            created_at: 5,
            mtime_unix: Some(5),
            marker: "ab".repeat(MARKER_LEN),
        }
    }

    #[test]
    fn sizes_are_capped_and_never_eat_the_reserve() {
        let t = 500 * GIB;
        assert_eq!(size_refusal(2 * GIB, t, 100 * GIB, 10 * GIB), None);
        assert!(
            size_refusal(MIN_SIZE - 1, t, 100 * GIB, 10 * GIB)
                .unwrap()
                .contains("at least")
        );
        assert!(
            size_refusal(11 * GIB, t, 100 * GIB, 10 * GIB)
                .unwrap()
                .contains("at most 10.0")
        );
        // 5% of a 40 GiB volume is 2 GiB.
        assert!(
            size_refusal(3 * GIB, 40 * GIB, 30 * GIB, GIB)
                .unwrap()
                .contains("5%")
        );
        let below = size_refusal(2 * GIB, t, 8 * GIB, 10 * GIB).unwrap();
        assert!(below.contains("already below"), "{below}");
        let would = size_refusal(2 * GIB, t, 11 * GIB, 10 * GIB).unwrap();
        assert!(would.contains("would leave 9.0 GiB free"), "{would}");
        assert_eq!(size_refusal(2 * GIB, t, 12 * GIB, 10 * GIB), None);
    }

    #[test]
    fn allocation_must_be_real() {
        assert!(really_allocated(GIB, GIB));
        assert!(really_allocated(GIB, GIB / 1000 * 985));
        assert!(!really_allocated(GIB, 4096), "sparse");
        assert!(!really_allocated(GIB, GIB / 2), "compressed");
    }

    #[test]
    fn mismatches_are_named() {
        let m = manifest();
        let id = Some(m.identity);
        assert_eq!(mismatch(&m, EntryKind::File, id, GIB), None);
        assert!(
            mismatch(&m, EntryKind::Symlink, id, GIB)
                .unwrap()
                .contains("symlink")
        );
        assert!(
            mismatch(&m, EntryKind::File, Some(FileIdentity::new(1, 3)), GIB)
                .unwrap()
                .contains("identity")
        );
        assert!(mismatch(&m, EntryKind::File, None, GIB).is_some());
        assert!(
            mismatch(&m, EntryKind::File, id, 7)
                .unwrap()
                .contains("size")
        );
        let other = HeadroomManifest {
            relative: "../elsewhere".into(),
            ..m
        };
        assert!(
            mismatch(&other, EntryKind::File, id, GIB)
                .unwrap()
                .contains("another path")
        );
    }

    #[test]
    fn the_marker_is_read_through_a_no_follow_handle_and_compared() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("reserve.bin");
        let mut body = vec![0x5au8; 4096];
        body[..MARKER_LEN].copy_from_slice(&[3u8; MARKER_LEN]);
        std::fs::write(&p, &body).unwrap();
        let id = Native::new().metadata(&p).unwrap().identity.unwrap();
        let m = HeadroomManifest {
            identity: id,
            marker: hex(&[3u8; MARKER_LEN]),
            ..manifest()
        };
        assert_eq!(marker_mismatch(&p, &m), None);
        let other = HeadroomManifest {
            marker: hex(&[4u8; MARKER_LEN]),
            ..m.clone()
        };
        assert!(
            marker_mismatch(&p, &other)
                .unwrap()
                .contains("marker differs")
        );
        let foreign = HeadroomManifest {
            identity: FileIdentity::new(id.volume, id.file + 1),
            ..m.clone()
        };
        assert!(marker_mismatch(&p, &foreign).unwrap().contains("identity"));
        let bad = HeadroomManifest {
            marker: "zz".into(),
            ..m.clone()
        };
        assert!(
            marker_mismatch(&p, &bad)
                .unwrap()
                .contains("no valid marker")
        );
        std::fs::write(&p, b"short").unwrap();
        assert!(marker_mismatch(&p, &m).is_some());
        #[cfg(unix)]
        {
            let target = dir.path().join("target.bin");
            std::fs::write(&target, &body).unwrap();
            std::fs::remove_file(&p).unwrap();
            std::os::unix::fs::symlink(&target, &p).unwrap();
            let linked = HeadroomManifest {
                identity: Native::new().metadata(&target).unwrap().identity.unwrap(),
                ..m.clone()
            };
            assert!(
                marker_mismatch(&p, &linked)
                    .unwrap()
                    .contains("without following links")
            );
        }
        assert_eq!(mtime_mismatch(&m, Some(5)), None);
        assert!(mtime_mismatch(&m, Some(6)).unwrap().contains("modified"));
        assert_eq!(mtime_mismatch(&m, None), None);
    }

    #[test]
    fn status_without_anything_is_none_and_reads_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let s = status(dir.path());
        assert_eq!(s.state, HeadroomState::None);
        assert!(s.manifest.is_none());
        assert!(
            !dir.path().join(HEADROOM_DIR).exists(),
            "status never creates anything"
        );
    }

    #[test]
    fn unrecorded_unreadable_and_missing_states() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(HEADROOM_DIR)).unwrap();
        std::fs::write(dir.path().join(reserve_relative()), b"x").unwrap();
        assert_eq!(status(dir.path()).state, HeadroomState::Unrecorded);
        std::fs::write(dir.path().join(MANIFEST_FILE), b"{not json").unwrap();
        assert_eq!(status(dir.path()).state, HeadroomState::Unreadable);
        let m = manifest();
        std::fs::write(
            dir.path().join(MANIFEST_FILE),
            serde_json::to_string(&m).unwrap(),
        )
        .unwrap();
        // The file there is not the recorded one.
        assert_eq!(status(dir.path()).state, HeadroomState::Changed);
        std::fs::remove_file(dir.path().join(reserve_relative())).unwrap();
        let s = status(dir.path());
        assert_eq!(s.state, HeadroomState::Missing);
        assert_eq!(s.manifest, Some(m));
    }
}
