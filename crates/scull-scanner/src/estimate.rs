//! Physical reclaim estimate with confidence levels (PRD §9, PHY-01, PHY-02).
//!
//! Logical size routinely overstates what removing an item frees. [`estimate`] turns a
//! scanned [`DirSummary`] plus facts about its volume ([`EstimateContext`], supplied by the
//! caller) into core [`Sizes`]: logical, allocated and reclaimable bytes, where the
//! reclaimable figure carries a [`Confidence`] and the bytes pinned by hard links elsewhere.
//! [`assess`] returns the same plus [`EstimateNote`]s explaining every downgrade, and
//! [`ActionReclaim`] splits the estimate by what an [`Action`] does with the bytes: freed now,
//! or only moved (quarantine, Trash) until a purge.
//!
//! Everything here is pure: no I/O, no clock, no platform calls.
//!
//! # Rules (PRD §9)
//!
//! The confidence starts at the scan's [`DirSummary::reclaim_confidence`] and only ever gets
//! weaker ([`Confidence`] orders `High < Medium < Low < Unknown`; the weakest rule wins):
//!
//! | PRD §9 situation | Bytes | Confidence |
//! |---|---|---|
//! | Hard links, all identified | `unique_allocated` (links elsewhere: `shared_bytes`, frees ~0) | unchanged (High) |
//! | Hard links without identity / unreadable parts | as scanned | Low (from the scan) |
//! | CoW clones possible (APFS, Btrfs, XFS, ReFS, ZFS) | unchanged | Low |
//! | Volume filesystem unknown | unchanged | at most Medium |
//! | Snapshots present or possible | unchanged | Low |
//! | Snapshots unknown | unchanged | at most Medium |
//! | Transparent compression, sparse files | allocated, never logical | unchanged |
//! | Trash / quarantine | see [`ActionReclaim`]: moved, freed only after purge | unchanged |
//! | VM disk images (bytes inside Docker Desktop / WSL2 disks) | unchanged (in-VM bytes) | Unknown |
//! | Allocated size unknown (capability missing or per entry) | best guess | Unknown |
//! | macOS purgeable space | unchanged (affects measuring, not estimating) | unchanged, noted |
//!
//! High therefore requires a known non-CoW filesystem *and* snapshots known to be absent: the
//! caller must positively know there is nothing that can pin bytes.

use crate::DirSummary;
use scull_core::{Action, Confidence, Reclaimable, Sizes};

/// The filesystem type of the volume an item lives on, as far as reclaim is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum VolumeFs {
    /// Apple APFS: clones (`clonefile`), local snapshots, compression, purgeable space.
    Apfs,
    /// Apple HFS+.
    HfsPlus,
    /// Linux Btrfs: reflinks, snapshots, compression.
    Btrfs,
    /// XFS: reflinks.
    Xfs,
    /// OpenZFS: snapshots, compression, block cloning (2.2+).
    Zfs,
    /// ext2/3/4.
    Ext4,
    /// Windows NTFS: compression (no block cloning).
    Ntfs,
    /// Windows ReFS / Dev Drive: block cloning.
    Refs,
    /// FAT32 / exFAT.
    Fat,
    /// tmpfs / ramfs.
    Tmpfs,
    /// A filesystem scull knows by name but has no reclaim facts for.
    Other,
    /// Not determined (default).
    #[default]
    Unknown,
}

/// Whether a filesystem can share extents between files (CoW clones, reflinks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloneSupport {
    /// The filesystem never shares extents.
    No,
    /// Clones are possible; whether a given file is cloned can't be determined cheaply.
    Possible,
    /// Not known for this filesystem.
    Unknown,
}

impl VolumeFs {
    /// Parse a filesystem type name as reported by the OS (`statfs` `f_fstypename`,
    /// `/proc/mounts`, `GetVolumeInformationW`), case-insensitively. Unrecognized names give
    /// [`VolumeFs::Other`], an empty name [`VolumeFs::Unknown`].
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "" => VolumeFs::Unknown,
            "apfs" => VolumeFs::Apfs,
            "hfs" | "hfs+" | "hfsplus" => VolumeFs::HfsPlus,
            "btrfs" => VolumeFs::Btrfs,
            "xfs" => VolumeFs::Xfs,
            "zfs" => VolumeFs::Zfs,
            "ext2" | "ext3" | "ext4" => VolumeFs::Ext4,
            "ntfs" | "ntfs3" => VolumeFs::Ntfs,
            "refs" => VolumeFs::Refs,
            "vfat" | "fat" | "fat32" | "msdos" | "exfat" => VolumeFs::Fat,
            "tmpfs" | "ramfs" => VolumeFs::Tmpfs,
            _ => VolumeFs::Other,
        }
    }

    /// Whether files on this filesystem may share extents (PRD §9 "CoW clones / reflinks").
    pub fn clones(self) -> CloneSupport {
        match self {
            VolumeFs::Apfs | VolumeFs::Btrfs | VolumeFs::Xfs | VolumeFs::Refs | VolumeFs::Zfs => {
                CloneSupport::Possible
            }
            VolumeFs::HfsPlus
            | VolumeFs::Ext4
            | VolumeFs::Ntfs
            | VolumeFs::Fat
            | VolumeFs::Tmpfs => CloneSupport::No,
            VolumeFs::Other | VolumeFs::Unknown => CloneSupport::Unknown,
        }
    }
}

/// What is known about filesystem snapshots on the item's volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Snapshots {
    /// Known: no snapshot can pin the item's blocks.
    Absent,
    /// Not determined (default).
    #[default]
    Unknown,
    /// The volume supports snapshots and some may exist (e.g. Time Machine local snapshots
    /// enabled, a Btrfs subvolume with a snapshot policy).
    Possible,
    /// Snapshots exist on the volume: deleted data stays pinned until they expire.
    Present,
}

/// Facts about the item's volume the estimator can't get from the scan; supplied by callers
/// (the platform layer for the filesystem type and snapshots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct EstimateContext {
    /// Filesystem type of the volume.
    pub fs: VolumeFs,
    /// Snapshot state of the volume.
    pub snapshots: Snapshots,
    /// Whether the platform reports allocated sizes at all
    /// ([`scull_platform::Capabilities::allocated_size`]). `false` makes every estimate
    /// [`Confidence::Unknown`].
    pub allocated_size_supported: bool,
    /// The bytes live inside a VM disk image (Docker Desktop, WSL2): pruning them frees space
    /// inside the VM, and the host file may not shrink until it is compacted.
    pub inside_vm_image: bool,
}

impl Default for EstimateContext {
    /// Nothing known: filesystem and snapshots unknown, allocated sizes supported.
    fn default() -> Self {
        Self {
            fs: VolumeFs::Unknown,
            snapshots: Snapshots::Unknown,
            allocated_size_supported: true,
            inside_vm_image: false,
        }
    }
}

impl EstimateContext {
    /// A context for a volume of type `fs` with snapshot state `snapshots`.
    pub fn new(fs: VolumeFs, snapshots: Snapshots) -> Self {
        Self {
            fs,
            snapshots,
            ..Self::default()
        }
    }
}

/// Why an estimate is what it is; one per rule that applied, for explanations in previews
/// ("shared with store: frees ~0 until `pnpm store prune`", "space may not be freed until
/// snapshots expire").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EstimateNote {
    /// These bytes belong to files with hard links outside the item: removing it frees ~0 for
    /// them while the other links remain (e.g. until `pnpm store prune`).
    SharedWithLinks {
        /// Allocated bytes pinned by the other links.
        bytes: u64,
    },
    /// Some multi-linked files could not be attributed (no identity or link count).
    UnattributedLinks,
    /// Part of the item could not be read; its bytes are missing from the estimate.
    Incomplete,
    /// The filesystem supports CoW clones / reflinks: shared extents won't be freed and can't
    /// be determined; rely on the measured delta.
    ClonesPossible(VolumeFs),
    /// The volume's filesystem type is unknown, so clones can't be ruled out.
    FilesystemUnknown,
    /// Snapshots exist: space may not be freed until they expire.
    SnapshotsPresent,
    /// Snapshots may exist: space may not be freed until they expire.
    SnapshotsPossible,
    /// Whether snapshots exist is unknown.
    SnapshotsUnknown,
    /// Allocated is below logical (transparent compression or sparse files); the estimate
    /// uses allocated bytes.
    CompressedOrSparse {
        /// `logical - allocated`.
        saved: u64,
    },
    /// The bytes are inside a VM disk image: freed inside the VM; the host file may not shrink
    /// without compaction, so host bytes freed are never claimed without measuring.
    InsideVmImage,
    /// Some allocated sizes are unknown (platform capability missing or per entry).
    AllocatedUnknown,
    /// APFS purgeable space makes reported free space fluctuate; noted when measuring.
    PurgeableSpace,
}

/// An estimate and its explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Estimate {
    /// Logical, allocated and reclaimable bytes with confidence.
    pub sizes: Sizes,
    /// Every rule that shaped the estimate, in a stable order.
    pub notes: Vec<EstimateNote>,
}

/// Estimate the physically reclaimable bytes of a scanned subtree (PRD §9). Pure.
///
/// `reclaimable.bytes` is what removing the item *frees once it is gone*: the allocated
/// bytes of files whose every link is inside it. What an action frees right away (nothing,
/// for quarantine and Trash, until a purge) is [`ActionReclaim::for_action`].
pub fn estimate(summary: &DirSummary, ctx: &EstimateContext) -> Sizes {
    assess(summary, ctx).sizes
}

/// [`estimate`] plus the notes explaining it.
pub fn assess(summary: &DirSummary, ctx: &EstimateContext) -> Estimate {
    let t = &summary.totals;
    let mut confidence = summary.reclaim_confidence;
    let mut notes = Vec::new();
    let mut weaken = |to: Confidence| confidence = confidence.max(to);

    // Hard links (PHY-03): attributed by the scan.
    if summary.shared_allocated > 0 {
        notes.push(EstimateNote::SharedWithLinks {
            bytes: summary.shared_allocated,
        });
    }
    if !summary.complete {
        notes.push(EstimateNote::Incomplete);
    } else if summary.reclaim_confidence != Confidence::High && t.allocated_unknown == 0 {
        notes.push(EstimateNote::UnattributedLinks);
    }

    // CoW clones / reflinks.
    match ctx.fs.clones() {
        CloneSupport::No => {}
        CloneSupport::Possible => {
            weaken(Confidence::Low);
            notes.push(EstimateNote::ClonesPossible(ctx.fs));
        }
        CloneSupport::Unknown => {
            weaken(Confidence::Medium);
            notes.push(EstimateNote::FilesystemUnknown);
        }
    }

    // Snapshots.
    match ctx.snapshots {
        Snapshots::Absent => {}
        Snapshots::Unknown => {
            weaken(Confidence::Medium);
            notes.push(EstimateNote::SnapshotsUnknown);
        }
        Snapshots::Possible => {
            weaken(Confidence::Low);
            notes.push(EstimateNote::SnapshotsPossible);
        }
        Snapshots::Present => {
            weaken(Confidence::Low);
            notes.push(EstimateNote::SnapshotsPresent);
        }
    }

    // Compression and sparse files: the scan's allocated sizes already account for them.
    let allocated_known = ctx.allocated_size_supported && t.allocated_unknown == 0;
    if allocated_known && t.logical > t.allocated {
        notes.push(EstimateNote::CompressedOrSparse {
            saved: t.logical - t.allocated,
        });
    }

    if ctx.inside_vm_image {
        weaken(Confidence::Unknown);
        notes.push(EstimateNote::InsideVmImage);
    }
    if !allocated_known {
        weaken(Confidence::Unknown);
        notes.push(EstimateNote::AllocatedUnknown);
    }
    if ctx.fs == VolumeFs::Apfs {
        notes.push(EstimateNote::PurgeableSpace);
    }

    Estimate {
        sizes: Sizes {
            logical: t.logical,
            allocated: t.allocated,
            reclaimable: Reclaimable {
                bytes: summary.unique_allocated,
                confidence,
                shared_bytes: summary.shared_allocated,
            },
        },
        notes,
    }
}

/// What an [`Action`] does with an item's bytes (PRD §9 "Trash / quarantine", PHY-02):
/// freed right away, or moved and freed only when the quarantine / Trash is purged.
///
/// Mapping from [`Sizes`]:
///
/// | Action | `freed_now` | `moved` | `freed_after_purge` | confidence |
/// |---|---|---|---|---|
/// | `Delete`, `GitWorktreeRemove`, `GitWorktreePrune` | `reclaimable.bytes` | 0 | 0 | as estimated |
/// | `ToolCommand` | `reclaimable.bytes` | 0 | 0 | at most Medium (the tool decides what goes) |
/// | `Quarantine`, `Trash` | 0 | `allocated` | `reclaimable.bytes` | as estimated |
/// | `None` | 0 | 0 | 0 | High |
/// | anything newer | 0 | 0 | 0 | Unknown |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ActionReclaim {
    /// Bytes freed on the volume when the action completes.
    pub freed_now: u64,
    /// Bytes moved (same volume) into quarantine or the Trash: not freed yet.
    pub moved: u64,
    /// Bytes freed later, when the quarantine or Trash is purged.
    pub freed_after_purge: u64,
    /// How far to trust the figures.
    pub confidence: Confidence,
}

impl ActionReclaim {
    /// Split `sizes` (from [`estimate`]) by what `action` does. Pure.
    pub fn for_action(sizes: &Sizes, action: &Action) -> Self {
        let r = &sizes.reclaimable;
        let freed = |confidence| Self {
            freed_now: r.bytes,
            moved: 0,
            freed_after_purge: 0,
            confidence,
        };
        let nothing = |confidence| Self {
            freed_now: 0,
            moved: 0,
            freed_after_purge: 0,
            confidence,
        };
        match action {
            Action::Delete | Action::GitWorktreeRemove | Action::GitWorktreePrune => {
                freed(r.confidence)
            }
            Action::ToolCommand { .. } => freed(r.confidence.max(Confidence::Medium)),
            Action::Quarantine | Action::Trash => Self {
                freed_now: 0,
                moved: sizes.allocated,
                freed_after_purge: r.bytes,
                confidence: r.confidence,
            },
            Action::None => nothing(Confidence::High),
            _ => nothing(Confidence::Unknown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Totals;
    use std::path::PathBuf;

    const KB: u64 = 1024;

    /// A finished subtree: `logical`/`allocated` bytes, `shared` of them pinned elsewhere.
    fn summary(logical: u64, allocated: u64, shared: u64) -> DirSummary {
        let totals = Totals {
            entries: 10,
            files: 9,
            dirs: 1,
            logical,
            allocated,
            ..Totals::default()
        };
        DirSummary {
            path: PathBuf::from("proj/target"),
            depth: 1,
            identity: None,
            mtime: Some(0),
            newest_mtime: Some(0),
            totals,
            complete: true,
            unique_allocated: allocated - shared,
            shared_allocated: shared,
            reclaim_confidence: Confidence::High,
        }
    }

    /// Ext4 without snapshots: nothing can pin bytes.
    fn certain() -> EstimateContext {
        EstimateContext::new(VolumeFs::Ext4, Snapshots::Absent)
    }

    fn conf(s: &DirSummary, ctx: &EstimateContext) -> Confidence {
        estimate(s, ctx).reclaimable.confidence
    }

    #[test]
    fn high_when_nothing_is_shared_or_unknown() {
        let s = summary(100 * KB, 120 * KB, 0);
        let e = assess(&s, &certain());
        assert_eq!(e.sizes.logical, 100 * KB);
        assert_eq!(e.sizes.allocated, 120 * KB);
        assert_eq!(
            e.sizes.reclaimable,
            Reclaimable {
                bytes: 120 * KB,
                confidence: Confidence::High,
                shared_bytes: 0
            }
        );
        assert!(e.notes.is_empty(), "{:?}", e.notes);
        for fs in [
            VolumeFs::Ext4,
            VolumeFs::Ntfs,
            VolumeFs::HfsPlus,
            VolumeFs::Fat,
            VolumeFs::Tmpfs,
        ] {
            let ctx = EstimateContext::new(fs, Snapshots::Absent);
            assert_eq!(conf(&s, &ctx), Confidence::High, "{fs:?}");
        }
    }

    #[test]
    fn hardlinks_attributed_once_shared_bytes_free_nothing() {
        // PRD §9 row 1: pnpm node_modules hard-linked into the store.
        let s = summary(390 * KB, 400 * KB, 380 * KB);
        let e = assess(&s, &certain());
        assert_eq!(e.sizes.reclaimable.bytes, 20 * KB);
        assert_eq!(e.sizes.reclaimable.shared_bytes, 380 * KB);
        assert_eq!(
            e.sizes.allocated,
            400 * KB,
            "allocated still shows everything"
        );
        assert_eq!(
            e.sizes.reclaimable.confidence,
            Confidence::High,
            "fully attributed links are certain"
        );
        assert_eq!(
            e.notes,
            vec![EstimateNote::SharedWithLinks { bytes: 380 * KB }]
        );
    }

    #[test]
    fn hardlinks_without_identity_are_low() {
        let mut s = summary(500 * KB, 400 * KB, 100 * KB);
        s.reclaim_confidence = Confidence::Low; // the scan couldn't identify some links
        let e = assess(&s, &certain());
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::Low);
        assert!(e.notes.contains(&EstimateNote::UnattributedLinks));
        // Even the best volume context can't raise it.
        assert_eq!(conf(&s, &certain()), Confidence::Low);
    }

    #[test]
    fn incomplete_scan_is_low_and_noted() {
        let mut s = summary(10 * KB, 12 * KB, 0);
        s.complete = false;
        s.reclaim_confidence = Confidence::Low;
        let e = assess(&s, &certain());
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::Low);
        assert_eq!(e.notes, vec![EstimateNote::Incomplete]);
    }

    #[test]
    fn cow_clones_possible_are_low() {
        // PRD §9 row 2: APFS clones, Btrfs/XFS reflinks, ReFS/Dev Drive block cloning.
        let s = summary(100 * KB, 100 * KB, 0);
        for fs in [
            VolumeFs::Apfs,
            VolumeFs::Btrfs,
            VolumeFs::Xfs,
            VolumeFs::Refs,
            VolumeFs::Zfs,
        ] {
            let e = assess(&s, &EstimateContext::new(fs, Snapshots::Absent));
            assert_eq!(e.sizes.reclaimable.confidence, Confidence::Low, "{fs:?}");
            assert_eq!(e.sizes.reclaimable.bytes, 100 * KB, "bytes unchanged");
            assert!(e.notes.contains(&EstimateNote::ClonesPossible(fs)));
        }
    }

    #[test]
    fn unknown_filesystem_is_at_most_medium() {
        let s = summary(100 * KB, 100 * KB, 0);
        for fs in [VolumeFs::Unknown, VolumeFs::Other] {
            let e = assess(&s, &EstimateContext::new(fs, Snapshots::Absent));
            assert_eq!(e.sizes.reclaimable.confidence, Confidence::Medium, "{fs:?}");
            assert_eq!(e.notes, vec![EstimateNote::FilesystemUnknown]);
        }
        // And never raises a weaker one.
        let mut low = s.clone();
        low.reclaim_confidence = Confidence::Low;
        assert_eq!(conf(&low, &EstimateContext::default()), Confidence::Low);
    }

    #[test]
    fn snapshots_present_or_possible_are_low() {
        // PRD §9 row 3: deleted data stays pinned by snapshots.
        let s = summary(100 * KB, 100 * KB, 0);
        let present = assess(
            &s,
            &EstimateContext::new(VolumeFs::Ext4, Snapshots::Present),
        );
        assert_eq!(present.sizes.reclaimable.confidence, Confidence::Low);
        assert_eq!(present.notes, vec![EstimateNote::SnapshotsPresent]);
        let possible = assess(
            &s,
            &EstimateContext::new(VolumeFs::Ntfs, Snapshots::Possible),
        );
        assert_eq!(possible.sizes.reclaimable.confidence, Confidence::Low);
        assert_eq!(possible.notes, vec![EstimateNote::SnapshotsPossible]);
        let unknown = assess(
            &s,
            &EstimateContext::new(VolumeFs::Ext4, Snapshots::Unknown),
        );
        assert_eq!(unknown.sizes.reclaimable.confidence, Confidence::Medium);
        assert_eq!(unknown.notes, vec![EstimateNote::SnapshotsUnknown]);
    }

    #[test]
    fn compression_uses_allocated_not_logical() {
        // PRD §9 row 4: APFS/NTFS/Btrfs transparent compression, allocated < logical.
        let s = summary(1000 * KB, 300 * KB, 0);
        let e = assess(&s, &EstimateContext::new(VolumeFs::Ntfs, Snapshots::Absent));
        assert_eq!(e.sizes.reclaimable.bytes, 300 * KB);
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::High);
        assert_eq!(
            e.notes,
            vec![EstimateNote::CompressedOrSparse { saved: 700 * KB }]
        );
    }

    #[test]
    fn sparse_files_use_allocated() {
        // PRD §9 row 5 (FX-FS-06): a 10 GB sparse file with 1 MB allocated.
        let s = summary(10 << 30, 1 << 20, 0);
        let e = assess(&s, &certain());
        assert_eq!(e.sizes.logical, 10 << 30);
        assert_eq!(e.sizes.reclaimable.bytes, 1 << 20);
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::High);
        assert_eq!(
            e.notes,
            vec![EstimateNote::CompressedOrSparse {
                saved: (10 << 30) - (1 << 20)
            }]
        );
    }

    #[test]
    fn trash_and_quarantine_move_bytes_without_freeing_them() {
        // PRD §9 row 6: report "moved" separately from "freed".
        let s = summary(500 * KB, 400 * KB, 100 * KB);
        let sizes = estimate(&s, &certain());
        for action in [Action::Quarantine, Action::Trash] {
            let r = ActionReclaim::for_action(&sizes, &action);
            assert_eq!(r.freed_now, 0, "{action:?}: moved, not freed");
            assert_eq!(r.moved, 400 * KB, "everything moves, shared bytes too");
            assert_eq!(r.freed_after_purge, 300 * KB);
            assert_eq!(r.confidence, Confidence::High);
        }
        let del = ActionReclaim::for_action(&sizes, &Action::Delete);
        assert_eq!(
            (del.freed_now, del.moved, del.freed_after_purge),
            (300 * KB, 0, 0)
        );
        for git in [Action::GitWorktreeRemove, Action::GitWorktreePrune] {
            assert_eq!(ActionReclaim::for_action(&sizes, &git), del);
        }
        let tool = ActionReclaim::for_action(
            &sizes,
            &Action::ToolCommand {
                tool: "npm".into(),
                args: vec!["cache".into(), "clean".into()],
            },
        );
        assert_eq!(tool.freed_now, 300 * KB);
        assert_eq!(tool.confidence, Confidence::Medium);
        let none = ActionReclaim::for_action(&sizes, &Action::None);
        assert_eq!(
            (none.freed_now, none.moved, none.freed_after_purge),
            (0, 0, 0)
        );
        assert_eq!(none.confidence, Confidence::High);
    }

    #[test]
    fn action_split_keeps_a_weak_confidence() {
        let s = summary(100 * KB, 100 * KB, 0);
        let sizes = estimate(
            &s,
            &EstimateContext::new(VolumeFs::Apfs, Snapshots::Present),
        );
        let q = ActionReclaim::for_action(&sizes, &Action::Quarantine);
        assert_eq!(q.confidence, Confidence::Low);
        let t = ActionReclaim::for_action(
            &sizes,
            &Action::ToolCommand {
                tool: "pnpm".into(),
                args: vec!["store".into(), "prune".into()],
            },
        );
        assert_eq!(t.confidence, Confidence::Low, "Medium cap never raises Low");
    }

    #[test]
    fn vm_disk_images_are_unknown() {
        // PRD §9 row 7: pruning inside the VM may not shrink the host file.
        let s = summary(100 * KB, 100 * KB, 0);
        let mut ctx = certain();
        ctx.inside_vm_image = true;
        let e = assess(&s, &ctx);
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::Unknown);
        assert_eq!(
            e.sizes.reclaimable.bytes,
            100 * KB,
            "in-VM bytes, a best guess"
        );
        assert_eq!(e.notes, vec![EstimateNote::InsideVmImage]);
    }

    #[test]
    fn macos_purgeable_space_is_noted_but_does_not_change_the_estimate() {
        // PRD §9 row 8: affects measuring, not estimating. (APFS is also clone-capable.)
        let s = summary(100 * KB, 100 * KB, 0);
        let e = assess(&s, &EstimateContext::new(VolumeFs::Apfs, Snapshots::Absent));
        assert!(e.notes.contains(&EstimateNote::PurgeableSpace));
        assert_eq!(e.sizes.reclaimable.bytes, 100 * KB);
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::Low); // from clones
        assert!(
            !assess(&s, &certain())
                .notes
                .contains(&EstimateNote::PurgeableSpace)
        );
    }

    #[test]
    fn unknown_allocated_size_is_unknown() {
        let s = summary(100 * KB, 100 * KB, 0);
        let mut ctx = certain();
        ctx.allocated_size_supported = false;
        let e = assess(&s, &ctx);
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::Unknown);
        assert_eq!(e.notes, vec![EstimateNote::AllocatedUnknown]);

        // Per entry: some sizes missing from the scan.
        let mut partial = summary(100 * KB, 60 * KB, 0);
        partial.totals.allocated_unknown = 2;
        partial.reclaim_confidence = Confidence::Low;
        let e = assess(&partial, &certain());
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::Unknown);
        assert_eq!(e.notes, vec![EstimateNote::AllocatedUnknown]);
        // No compression note: logical > allocated only because sizes are missing.
    }

    #[test]
    fn weakest_rule_wins_and_notes_are_ordered() {
        let mut s = summary(1000 * KB, 400 * KB, 100 * KB);
        s.complete = false;
        s.reclaim_confidence = Confidence::Low;
        let ctx = EstimateContext::new(VolumeFs::Btrfs, Snapshots::Present);
        let e = assess(&s, &ctx);
        assert_eq!(e.sizes.reclaimable.confidence, Confidence::Low);
        assert_eq!(
            e.notes,
            vec![
                EstimateNote::SharedWithLinks { bytes: 100 * KB },
                EstimateNote::Incomplete,
                EstimateNote::ClonesPossible(VolumeFs::Btrfs),
                EstimateNote::SnapshotsPresent,
                EstimateNote::CompressedOrSparse { saved: 600 * KB },
            ]
        );
        let mut vm = ctx;
        vm.inside_vm_image = true;
        assert_eq!(conf(&s, &vm), Confidence::Unknown);
    }

    #[test]
    fn estimate_is_deterministic_and_matches_assess() {
        let s = summary(123, 4096, 0);
        let ctx = EstimateContext::new(VolumeFs::Xfs, Snapshots::Possible);
        assert_eq!(estimate(&s, &ctx), estimate(&s, &ctx));
        assert_eq!(estimate(&s, &ctx), assess(&s, &ctx).sizes);
        assert_eq!(estimate(&s, &ctx), {
            let mut r = s.reclaimable();
            r.confidence = Confidence::Low;
            Sizes {
                logical: 123,
                allocated: 4096,
                reclaimable: r,
            }
        });
    }

    #[test]
    fn filesystem_names_parse() {
        for (name, fs) in [
            ("apfs", VolumeFs::Apfs),
            ("APFS", VolumeFs::Apfs),
            ("hfs", VolumeFs::HfsPlus),
            ("btrfs", VolumeFs::Btrfs),
            ("xfs", VolumeFs::Xfs),
            ("zfs", VolumeFs::Zfs),
            ("ext4", VolumeFs::Ext4),
            ("ext3", VolumeFs::Ext4),
            ("NTFS", VolumeFs::Ntfs),
            ("ReFS", VolumeFs::Refs),
            ("exFAT", VolumeFs::Fat),
            ("msdos", VolumeFs::Fat),
            ("tmpfs", VolumeFs::Tmpfs),
            ("fuse.sshfs", VolumeFs::Other),
            (" ", VolumeFs::Unknown),
        ] {
            assert_eq!(VolumeFs::from_name(name), fs, "{name:?}");
        }
        assert_eq!(VolumeFs::default(), VolumeFs::Unknown);
        assert_eq!(Snapshots::default(), Snapshots::Unknown);
        assert_eq!(
            EstimateContext::default().fs.clones(),
            CloneSupport::Unknown
        );
    }
}
