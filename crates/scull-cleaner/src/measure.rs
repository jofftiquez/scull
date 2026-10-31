//! Measured free-space delta with explanations (PRD §9, PHY-02, QC-06, invariant I-5).
//!
//! The engine predicts how many bytes an operation frees ([`scull_scanner::assess`],
//! [`ActionReclaim::for_action`]); this module measures what really happened and explains the
//! difference. PHY-02: results show predicted reclaimable, bytes moved, bytes removed and the
//! **measured free-space delta**, with an explanation when predicted and measured differ by
//! more than 10%.
//!
//! # Flow
//!
//! 1. Before the operation, [`VolumeMeter::before`] snapshots the free space of every affected
//!    volume (the item anchors, plus the quarantine folder's volume when there is one), one
//!    snapshot per volume however many paths live on it ([`Baseline`]).
//! 2. While executing, the caller adds every plan item to a [`Tally`]: its predicted
//!    [`ActionReclaim`], its estimate notes and what the Executor really did (bytes removed,
//!    bytes moved, or not executed at all).
//! 3. After the operation, [`VolumeMeter::after`] samples the same volumes again and returns a
//!    [`MeasuredDelta`]: per-volume before / after / delta, the predicted figures, the measured
//!    total, the discrepancy and the [`Explanation`]s.
//! 4. [`MeasuredDelta::operation_finish`] / [`MeasuredDelta::measured_delta_bytes`] feed the
//!    audit log ([`OperationFinish::measured_delta_bytes`]) so predicted and measured bytes are
//!    both recorded (invariant I-5); [`Explanation::for_item`] gives per-item notes.
//!
//! # Noise
//!
//! Free space is a volume-wide figure: any other process writing or deleting during the
//! operation shows up in it, and some filesystems report frees late (Btrfs and ZFS free
//! asynchronously, ext4 at the next journal commit) or fluctuate on their own (macOS purgeable
//! space). The meter therefore
//!
//! * takes [`MeasureOptions::samples`] samples per snapshot and uses their median; their spread
//!   is recorded as the snapshot's noise;
//! * waits [`MeasureOptions::settle`] before the first after-sample;
//! * when the measurement falls short of the prediction by more than 10%, re-samples every
//!   [`MeasureOptions::resample_interval`] for up to [`MeasureOptions::max_settle`], to catch
//!   delayed frees (reported as [`Explanation::DelayedFree`] when the delta grew meanwhile);
//! * treats differences no larger than [`MeasureOptions::resolution`] plus the observed noise
//!   as below what free-space reporting can resolve ([`Explanation::BelowResolution`]).
//!
//! # Testability
//!
//! The free-space source is injected ([`FreeSpaceSource`]); [`PlatformFreeSpace`] is the real
//! one (`scull_platform::volume_of` / `free_space`). The explanation logic
//! ([`MeasuredDelta::from_volumes`]) is pure.

use scull_core::Confidence;
use scull_scanner::{ActionReclaim, CloneSupport, EstimateNote, VolumeFs};
use scull_storage::{OperationEnd, OperationFinish};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// PHY-02: an explanation is required when predicted and measured differ by more than this
/// percentage of the prediction.
pub const DISCREPANCY_THRESHOLD_PCT: u64 = 10;

/// Whether `measured` differs from `predicted` by more than [`DISCREPANCY_THRESHOLD_PCT`] of
/// `predicted` (PHY-02). Exact integer arithmetic; with `predicted == 0` any non-zero
/// measurement exceeds it.
pub fn exceeds_threshold(predicted: u64, measured: i64) -> bool {
    let diff = (i128::from(measured) - i128::from(predicted)).unsigned_abs();
    diff * 100 > u128::from(predicted) * u128::from(DISCREPANCY_THRESHOLD_PCT)
}

// ---- free-space source -----------------------------------------------------------------------

/// A failed free-space or volume query (the message is for display only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError(pub String);

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProbeError {}

/// What the meter needs to know about a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VolumeProbe {
    /// Volume identifier (Unix `st_dev`, Windows volume serial), if known. Paths with the same
    /// ID are measured once.
    pub volume_id: Option<u64>,
    /// Where the volume is mounted; also the fallback probe path when the measured path is
    /// gone after the operation.
    pub mount_point: PathBuf,
    /// Filesystem type as the OS names it (`ext4`, `apfs`, `NTFS`, ...).
    pub fs_type: String,
    /// [`VolumeProbe::fs_type`] parsed for reclaim purposes.
    pub fs: VolumeFs,
    /// Files can share extents (CoW clones, reflinks); `None` = unknown.
    pub clones: Option<bool>,
    /// The filesystem supports snapshots; `None` = unknown.
    pub snapshots: Option<bool>,
}

impl VolumeProbe {
    /// A volume mounted at `mount_point` with filesystem `fs_type`; features unknown.
    pub fn new(mount_point: impl Into<PathBuf>, fs_type: &str) -> Self {
        Self {
            volume_id: None,
            mount_point: mount_point.into(),
            fs_type: fs_type.to_owned(),
            fs: VolumeFs::from_name(fs_type),
            clones: None,
            snapshots: None,
        }
    }

    /// Set the volume ID.
    pub fn with_id(mut self, id: u64) -> Self {
        self.volume_id = Some(id);
        self
    }

    /// Set the clone and snapshot features.
    pub fn with_features(mut self, clones: Option<bool>, snapshots: Option<bool>) -> Self {
        self.clones = clones;
        self.snapshots = snapshots;
        self
    }

    /// Whether deleting here may free less than predicted because of shared extents.
    fn clones_possible(&self) -> bool {
        self.clones == Some(true)
            || (self.clones.is_none() && self.fs.clones() == CloneSupport::Possible)
    }

    /// Whether the filesystem frees blocks asynchronously, so free space can lag a deletion.
    fn frees_asynchronously(&self) -> bool {
        matches!(self.fs, VolumeFs::Btrfs | VolumeFs::Zfs)
    }
}

/// Where free space comes from. Inject a fake to test without I/O.
pub trait FreeSpaceSource {
    /// Describe the volume holding `path` (which must exist).
    fn describe(&self, path: &Path) -> Result<VolumeProbe, ProbeError>;

    /// Bytes available to the current user on the volume holding `path`.
    fn free_bytes(&self, path: &Path) -> Result<u64, ProbeError>;

    /// Wait `d` between samples. Fakes override this with a no-op.
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The real free-space source: `scull_platform::volume_of` and `scull_platform::free_space`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformFreeSpace;

impl FreeSpaceSource for PlatformFreeSpace {
    fn describe(&self, path: &Path) -> Result<VolumeProbe, ProbeError> {
        let v = scull_platform::volume_of(path).map_err(|e| ProbeError(e.to_string()))?;
        let mut probe = VolumeProbe::new(v.mount_point, &v.fs_type)
            .with_features(v.supports.clones, v.supports.snapshots);
        probe.volume_id = v.volume_id;
        Ok(probe)
    }

    fn free_bytes(&self, path: &Path) -> Result<u64, ProbeError> {
        scull_platform::free_space(path).map_err(|e| ProbeError(e.to_string()))
    }
}

// ---- options and sampling --------------------------------------------------------------------

/// How the meter samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MeasureOptions {
    /// Samples per snapshot (at least 1); the median is used, the spread is the noise.
    pub samples: u32,
    /// Pause between the samples of one snapshot.
    pub sample_interval: Duration,
    /// Pause after the operation before the first after-sample.
    pub settle: Duration,
    /// Pause between re-samples while waiting for delayed frees.
    pub resample_interval: Duration,
    /// Longest total wait for delayed frees (0 disables re-sampling).
    pub max_settle: Duration,
    /// Differences up to this many bytes (plus the observed noise) are below what free-space
    /// reporting resolves (block granularity, metadata, unrelated small writes).
    pub resolution: u64,
}

impl Default for MeasureOptions {
    /// 3 samples 20 ms apart, 100 ms settle, re-sample every 500 ms for up to 5 s (ext4's
    /// default journal commit interval), 1 MiB resolution.
    fn default() -> Self {
        Self {
            samples: 3,
            sample_interval: Duration::from_millis(20),
            settle: Duration::from_millis(100),
            resample_interval: Duration::from_millis(500),
            max_settle: Duration::from_secs(5),
            resolution: 1024 * 1024,
        }
    }
}

impl MeasureOptions {
    /// One sample, no waiting, no re-sampling, with `resolution`: for fakes and tight tests.
    pub fn immediate(resolution: u64) -> Self {
        Self {
            samples: 1,
            sample_interval: Duration::ZERO,
            settle: Duration::ZERO,
            resample_interval: Duration::ZERO,
            max_settle: Duration::ZERO,
            resolution,
        }
    }

    /// Set the number of samples per snapshot.
    pub fn with_samples(mut self, samples: u32) -> Self {
        self.samples = samples;
        self
    }

    /// Set the delayed-free re-sampling interval and maximum total wait.
    pub fn with_resampling(mut self, interval: Duration, max: Duration) -> Self {
        self.resample_interval = interval;
        self.max_settle = max;
        self
    }
}

/// One free-space snapshot of a volume: the median of its samples and their spread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Median free bytes.
    pub free: u64,
    /// Largest minus smallest sample.
    pub spread: u64,
}

impl Sample {
    /// A snapshot from raw samples; `None` if there are none.
    pub fn from_samples(samples: &[u64]) -> Option<Self> {
        let mut s = samples.to_vec();
        s.sort_unstable();
        let (&min, &max) = (s.first()?, s.last()?);
        Some(Self {
            free: s[s.len() / 2],
            spread: max - min,
        })
    }
}

/// A path that could not be measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmeasured {
    /// The path given to [`VolumeMeter::before`] (or the volume's probe path).
    pub path: PathBuf,
    /// Why.
    pub reason: String,
}

/// One volume of a [`Baseline`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct BaselineVolume {
    probe: VolumeProbe,
    path: PathBuf,
    before: Sample,
}

/// Free space of every affected volume before an operation ([`VolumeMeter::before`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Baseline {
    volumes: Vec<BaselineVolume>,
    unavailable: Vec<Unmeasured>,
}

impl Baseline {
    /// Number of volumes measured.
    pub fn volume_count(&self) -> usize {
        self.volumes.len()
    }

    /// Paths that could not be measured.
    pub fn unavailable(&self) -> &[Unmeasured] {
        &self.unavailable
    }
}

/// Snapshots free space per affected volume before and after an operation.
#[derive(Debug, Clone)]
pub struct VolumeMeter<S> {
    source: S,
    options: MeasureOptions,
}

impl VolumeMeter<PlatformFreeSpace> {
    /// A meter on the real platform with default options.
    pub fn platform() -> Self {
        Self::new(PlatformFreeSpace, MeasureOptions::default())
    }

    /// A meter on the real platform with default sampling but **no** waiting for delayed frees
    /// (a measurement costs about 0.2 s): what every [`Operation`](crate::Operation) uses
    /// unless told otherwise, so predicted and measured bytes are always both recorded (I-5).
    /// Use [`VolumeMeter::platform`] to also catch delayed frees (Btrfs, ZFS, ext4 commits).
    pub fn platform_quick() -> Self {
        Self::new(
            PlatformFreeSpace,
            MeasureOptions::default().with_resampling(Duration::ZERO, Duration::ZERO),
        )
    }
}

impl<S: FreeSpaceSource> VolumeMeter<S> {
    /// A meter reading `source`.
    pub fn new(source: S, options: MeasureOptions) -> Self {
        Self { source, options }
    }

    /// The options.
    pub fn options(&self) -> &MeasureOptions {
        &self.options
    }

    /// Snapshot the free space of the volumes holding `paths`, once per volume. Pass paths
    /// that survive the operation (item anchors / scan roots, the quarantine folder); a path
    /// gone afterwards is replaced by its volume's mount point. Failures are recorded, never
    /// fatal: measuring must not block an operation.
    pub fn before<P: AsRef<Path>>(&self, paths: impl IntoIterator<Item = P>) -> Baseline {
        let mut b = Baseline::default();
        for p in paths {
            let path = p.as_ref();
            let probe = match self.source.describe(path) {
                Ok(probe) => probe,
                Err(e) => {
                    b.unavailable.push(Unmeasured {
                        path: path.to_path_buf(),
                        reason: e.0,
                    });
                    continue;
                }
            };
            let seen = b.volumes.iter().any(|v| same_volume(&v.probe, &probe));
            if seen {
                continue;
            }
            match self.sample(path) {
                Ok(before) => b.volumes.push(BaselineVolume {
                    probe,
                    path: path.to_path_buf(),
                    before,
                }),
                Err(e) => b.unavailable.push(Unmeasured {
                    path: path.to_path_buf(),
                    reason: e.0,
                }),
            }
        }
        b
    }

    /// Sample the baseline's volumes again and compare with `tally`'s prediction. Re-samples
    /// for up to [`MeasureOptions::max_settle`] while the measurement falls short of the
    /// prediction by more than 10%, to catch delayed frees.
    pub fn after(&self, baseline: Baseline, tally: &Tally) -> MeasuredDelta {
        let o = self.options;
        if !o.settle.is_zero() {
            self.source.sleep(o.settle);
        }
        let mut unavailable = baseline.unavailable;
        let mut current: Vec<(BaselineVolume, Sample)> = Vec::new();
        for v in baseline.volumes {
            match self.sample_after(&v) {
                Ok(s) => current.push((v, s)),
                Err(e) => unavailable.push(Unmeasured {
                    path: v.path.clone(),
                    reason: e.0,
                }),
            }
        }
        let total = |c: &[(BaselineVolume, Sample)]| -> i64 {
            c.iter()
                .map(|(v, s)| signed_delta(v.before.free, s.free))
                .sum()
        };
        let first_total = total(&current);
        let predicted = tally.predicted.freed_now;
        let mut waited = Duration::ZERO;
        let mut grew = false;
        while !current.is_empty()
            && !o.resample_interval.is_zero()
            && waited + o.resample_interval <= o.max_settle
        {
            let t = total(&current);
            let short = t < i64::try_from(predicted).unwrap_or(i64::MAX);
            if !(short && exceeds_threshold(predicted, t)) {
                break;
            }
            self.source.sleep(o.resample_interval);
            waited += o.resample_interval;
            for (v, s) in &mut current {
                // A failed re-sample keeps the previous one.
                if let Ok(new) = self.sample_after(v) {
                    *s = new;
                }
            }
            let noise: u64 = current
                .iter()
                .map(|(v, s)| v.before.spread.max(s.spread))
                .sum();
            let floor = i64::try_from(o.resolution.saturating_add(noise)).unwrap_or(i64::MAX);
            grew |= total(&current).saturating_sub(first_total) > floor;
        }
        let volumes = current
            .into_iter()
            .map(|(v, s)| VolumeDelta::new(v.probe, v.before, s))
            .collect();
        MeasuredDelta::from_volumes(
            volumes,
            unavailable,
            tally,
            Observations {
                waited,
                delayed_free_observed: grew,
                resolution: o.resolution,
            },
        )
    }

    fn sample_after(&self, v: &BaselineVolume) -> Result<Sample, ProbeError> {
        self.sample(&v.path)
            .or_else(|_| self.sample(&v.probe.mount_point))
    }

    fn sample(&self, path: &Path) -> Result<Sample, ProbeError> {
        let n = self.options.samples.max(1);
        let mut values = Vec::with_capacity(n as usize);
        let mut last_err = None;
        for i in 0..n {
            if i > 0 && !self.options.sample_interval.is_zero() {
                self.source.sleep(self.options.sample_interval);
            }
            match self.source.free_bytes(path) {
                Ok(v) => values.push(v),
                Err(e) => last_err = Some(e),
            }
        }
        Sample::from_samples(&values)
            .ok_or_else(|| last_err.unwrap_or_else(|| ProbeError("no samples".into())))
    }
}

/// What an [`Operation`](crate::Operation) needs from a meter, object-safe so any
/// [`VolumeMeter`] can be installed with [`Operation::with_meter`](crate::Operation::with_meter).
pub(crate) trait Meter: fmt::Debug + Send {
    fn before(&self, paths: &[PathBuf]) -> Baseline;
    fn after(&self, baseline: Baseline, tally: &Tally) -> MeasuredDelta;
}

impl<S: FreeSpaceSource + fmt::Debug + Send> Meter for VolumeMeter<S> {
    fn before(&self, paths: &[PathBuf]) -> Baseline {
        VolumeMeter::before(self, paths)
    }

    fn after(&self, baseline: Baseline, tally: &Tally) -> MeasuredDelta {
        VolumeMeter::after(self, baseline, tally)
    }
}

/// The sizes a plan item's own estimate implies (`reclaimable_bytes`, medium confidence), for
/// items without a fresh estimate (skipped before re-derivation).
pub(crate) fn planned_sizes(item: &scull_core::PlanItem) -> scull_core::Sizes {
    let b = item.reclaimable_bytes;
    scull_core::Sizes {
        logical: b,
        allocated: b,
        reclaimable: scull_core::Reclaimable {
            bytes: b,
            confidence: Confidence::Medium,
            shared_bytes: 0,
        },
    }
}

fn same_volume(a: &VolumeProbe, b: &VolumeProbe) -> bool {
    match (a.volume_id, b.volume_id) {
        (Some(x), Some(y)) => x == y,
        _ => a.mount_point == b.mount_point,
    }
}

fn signed_delta(before: u64, after: u64) -> i64 {
    let d = i128::from(after) - i128::from(before);
    i64::try_from(d).unwrap_or(if d < 0 { i64::MIN } else { i64::MAX })
}

// ---- tally of predicted and executed ---------------------------------------------------------

/// What the Executor did with one plan item, for [`Tally::add`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ItemExecution {
    /// Done completely: `removed` allocated bytes unlinked (the walker's
    /// `RemovedCounts::allocated_bytes`), `moved` bytes moved aside (quarantine, Trash).
    Done {
        /// Allocated bytes removed.
        removed: u64,
        /// Bytes moved aside.
        moved: u64,
    },
    /// Partly done (some entries kept or the walk aborted).
    Partial {
        /// Allocated bytes removed.
        removed: u64,
        /// Bytes moved aside.
        moved: u64,
    },
    /// Not executed: skipped at revalidation, refused or failed before removing anything.
    NotExecuted,
}

/// Predicted reclaim of an operation: the sum of its items' [`ActionReclaim`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredictedReclaim {
    /// Bytes freed when the operation completes.
    pub freed_now: u64,
    /// Bytes moved into quarantine / Trash (not freed).
    pub moved: u64,
    /// Bytes freed later, when the quarantine / Trash is purged.
    pub freed_after_purge: u64,
    /// The weakest confidence of the items (High for an empty operation).
    pub confidence: Confidence,
}

impl Default for PredictedReclaim {
    fn default() -> Self {
        Self {
            freed_now: 0,
            moved: 0,
            freed_after_purge: 0,
            confidence: Confidence::High,
        }
    }
}

impl PredictedReclaim {
    fn add(&mut self, r: &ActionReclaim) {
        self.freed_now = self.freed_now.saturating_add(r.freed_now);
        self.moved = self.moved.saturating_add(r.moved);
        self.freed_after_purge = self.freed_after_purge.saturating_add(r.freed_after_purge);
        self.confidence = self.confidence.max(r.confidence);
    }
}

impl From<&ActionReclaim> for PredictedReclaim {
    fn from(r: &ActionReclaim) -> Self {
        let mut p = Self::default();
        p.add(r);
        p
    }
}

/// Predicted and executed bytes of an operation, built item by item while it runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tally {
    /// What the plan predicted, over all its items.
    pub predicted: PredictedReclaim,
    /// Allocated bytes the Executor removed (PHY-02 "bytes removed").
    pub removed: u64,
    /// Bytes the Executor moved aside (PHY-02 "bytes moved").
    pub moved: u64,
    /// Items not executed.
    pub not_executed: u64,
    /// Predicted `freed_now` bytes of the items not executed.
    pub not_executed_bytes: u64,
    /// Items only partly executed.
    pub partial: u64,
    /// Every distinct estimate note of the items.
    pub notes: Vec<EstimateNote>,
}

impl Tally {
    /// An empty tally.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one plan item: its predicted reclaim, its estimate notes ([`scull_scanner::assess`])
    /// and what was done.
    pub fn add(&mut self, predicted: &ActionReclaim, notes: &[EstimateNote], done: ItemExecution) {
        self.predicted.add(predicted);
        for n in notes {
            self.note(*n);
        }
        match done {
            ItemExecution::Done { removed, moved } => self.executed(removed, moved),
            ItemExecution::Partial { removed, moved } => {
                self.partial += 1;
                self.executed(removed, moved);
            }
            ItemExecution::NotExecuted => {
                self.not_executed += 1;
                self.not_executed_bytes =
                    self.not_executed_bytes.saturating_add(predicted.freed_now);
            }
        }
    }

    fn executed(&mut self, removed: u64, moved: u64) {
        self.removed = self.removed.saturating_add(removed);
        self.moved = self.moved.saturating_add(moved);
    }

    fn note(&mut self, n: EstimateNote) {
        // Byte-carrying notes are summed into one entry of their kind.
        if let EstimateNote::SharedWithLinks { bytes } = n {
            for existing in &mut self.notes {
                if let EstimateNote::SharedWithLinks { bytes: b } = existing {
                    *b = b.saturating_add(bytes);
                    return;
                }
            }
        }
        if let EstimateNote::CompressedOrSparse { saved } = n {
            for existing in &mut self.notes {
                if let EstimateNote::CompressedOrSparse { saved: s } = existing {
                    *s = s.saturating_add(saved);
                    return;
                }
            }
        }
        if !self.notes.contains(&n) {
            self.notes.push(n);
        }
    }
}

// ---- result ----------------------------------------------------------------------------------

/// Free space of one volume before and after.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VolumeDelta {
    /// The volume.
    pub volume: VolumeProbe,
    /// Free bytes before (median of the samples).
    pub before: u64,
    /// Free bytes after (median of the last samples).
    pub after: u64,
    /// `after - before`: positive = space gained.
    pub delta: i64,
    /// Largest sample spread of the two snapshots.
    pub noise: u64,
}

impl VolumeDelta {
    /// A delta from two snapshots.
    pub fn new(volume: VolumeProbe, before: Sample, after: Sample) -> Self {
        Self {
            volume,
            before: before.free,
            after: after.free,
            delta: signed_delta(before.free, after.free),
            noise: before.spread.max(after.spread),
        }
    }
}

/// What the meter observed besides the free-space figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Observations {
    /// Total time spent re-sampling for delayed frees.
    pub waited: Duration,
    /// The delta grew (beyond resolution) while re-sampling: frees were reported late.
    pub delayed_free_observed: bool,
    /// [`MeasureOptions::resolution`].
    pub resolution: u64,
}

impl Observations {
    /// Nothing observed, with `resolution`.
    pub fn new(resolution: u64) -> Self {
        Self {
            waited: Duration::ZERO,
            delayed_free_observed: false,
            resolution,
        }
    }

    /// Mark a delayed free as observed after `waited`.
    pub fn with_delayed_free(mut self, waited: Duration) -> Self {
        self.waited = waited;
        self.delayed_free_observed = true;
        self
    }
}

/// Why the measured delta differs from the prediction, or what to keep in mind reading it
/// (PRD §9). Stable codes: [`Explanation::code`]; text: [`Explanation::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Explanation {
    /// Bytes were moved into quarantine or the Trash: moved, not freed (FX-PHY-07).
    MovedNotFreed {
        /// Bytes moved.
        bytes: u64,
    },
    /// macOS purgeable space makes reported free space fluctuate.
    PurgeableSpace,
    /// Some affected volumes could not be measured; the total covers the others only.
    MeasurementUnavailable {
        /// Paths not measured.
        paths: usize,
    },
    /// The difference is within what free-space reporting resolves (block granularity,
    /// metadata, small unrelated writes): predicted and measured agree.
    BelowResolution {
        /// Bytes considered noise: resolution plus observed sample spread.
        tolerance: u64,
    },
    /// Some plan items were not executed (skipped, refused, failed); their predicted bytes
    /// were not freed.
    ItemsNotExecuted {
        /// Items not executed.
        items: u64,
        /// Their predicted bytes.
        bytes: u64,
    },
    /// Some items were only partly executed.
    PartiallyExecuted {
        /// Items.
        items: u64,
    },
    /// Files have hard links elsewhere (e.g. pnpm `node_modules` into the store): removing
    /// them frees ~0 until the other links go (`pnpm store prune`).
    SharedHardlinks {
        /// Bytes pinned by the other links.
        bytes: u64,
    },
    /// Some multi-linked files could not be attributed; their bytes may still be linked
    /// elsewhere.
    UnattributedLinks,
    /// The filesystem supports CoW clones / reflinks: shared extents are not freed.
    ClonesPossible {
        /// The filesystem.
        fs: VolumeFs,
    },
    /// Snapshots exist on the volume: deleted data stays pinned until they expire.
    SnapshotsPresent,
    /// Snapshots may exist: space may not be freed until they expire.
    SnapshotsPossible,
    /// Whether snapshots pin the deleted data is unknown.
    SnapshotsUnknown,
    /// The filesystem frees blocks asynchronously or reported the free late.
    DelayedFree {
        /// The filesystem.
        fs: VolumeFs,
        /// The delta grew while re-sampling (seen, not just possible).
        observed: bool,
    },
    /// The bytes were inside a VM disk image: freed inside the VM; the host file may not
    /// shrink until compacted.
    InsideVmImage,
    /// The estimate missed bytes (unreadable parts, unknown allocated sizes): more may be
    /// freed than predicted.
    EstimateIncomplete,
    /// Other processes may have written to or freed space on the volume during the operation;
    /// scull can't tell them apart.
    ConcurrentActivity {
        /// Observed sample spread, bytes.
        noise: u64,
    },
}

impl Explanation {
    /// Stable snake_case code, for JSON, logs and the audit record.
    pub fn code(&self) -> &'static str {
        match self {
            Explanation::MovedNotFreed { .. } => "moved_not_freed",
            Explanation::PurgeableSpace => "purgeable_space",
            Explanation::MeasurementUnavailable { .. } => "measurement_unavailable",
            Explanation::BelowResolution { .. } => "below_resolution",
            Explanation::ItemsNotExecuted { .. } => "items_not_executed",
            Explanation::PartiallyExecuted { .. } => "partially_executed",
            Explanation::SharedHardlinks { .. } => "shared_hardlinks",
            Explanation::UnattributedLinks => "unattributed_links",
            Explanation::ClonesPossible { .. } => "clones_possible",
            Explanation::SnapshotsPresent => "snapshots_present",
            Explanation::SnapshotsPossible => "snapshots_possible",
            Explanation::SnapshotsUnknown => "snapshots_unknown",
            Explanation::DelayedFree { .. } => "delayed_free",
            Explanation::InsideVmImage => "inside_vm_image",
            Explanation::EstimateIncomplete => "estimate_incomplete",
            Explanation::ConcurrentActivity { .. } => "concurrent_activity",
        }
    }

    /// One-line human explanation.
    pub fn message(&self) -> String {
        match *self {
            Explanation::MovedNotFreed { bytes } => format!(
                "{} moved to quarantine/Trash: moved, not freed until purged",
                fmt_bytes(bytes)
            ),
            Explanation::PurgeableSpace => {
                "macOS purgeable space makes reported free space fluctuate".into()
            }
            Explanation::MeasurementUnavailable { paths } => format!(
                "{paths} affected location(s) could not be measured; the total covers the rest"
            ),
            Explanation::BelowResolution { tolerance } => format!(
                "difference within free-space reporting resolution (±{})",
                fmt_bytes(tolerance)
            ),
            Explanation::ItemsNotExecuted { items, bytes } => format!(
                "{items} item(s) not executed; their predicted {} were not freed",
                fmt_bytes(bytes)
            ),
            Explanation::PartiallyExecuted { items } => {
                format!("{items} item(s) only partly removed")
            }
            Explanation::SharedHardlinks { bytes } => format!(
                "{} shared with hard links elsewhere (e.g. the pnpm store): frees ~0 until \
                 `pnpm store prune`",
                fmt_bytes(bytes)
            ),
            Explanation::UnattributedLinks => {
                "some hard-linked files could not be attributed and may be linked elsewhere".into()
            }
            Explanation::ClonesPossible { fs } => format!(
                "{} supports clones/reflinks: shared extents are not freed",
                fs_name(fs)
            ),
            Explanation::SnapshotsPresent => {
                "snapshots exist: space may not be freed until snapshots expire".into()
            }
            Explanation::SnapshotsPossible => {
                "snapshots may exist: space may not be freed until snapshots expire".into()
            }
            Explanation::SnapshotsUnknown => {
                "unknown whether snapshots pin the deleted data".into()
            }
            Explanation::DelayedFree { fs, observed: true } => format!(
                "{} reported the freed space late (re-sampled after a delay)",
                fs_name(fs)
            ),
            Explanation::DelayedFree {
                fs,
                observed: false,
            } => format!(
                "{} frees space asynchronously; free space may still be catching up",
                fs_name(fs)
            ),
            Explanation::InsideVmImage => {
                "bytes were inside a VM disk image: the host file may not shrink until compacted"
                    .into()
            }
            Explanation::EstimateIncomplete => {
                "the estimate missed some bytes (unreadable or unknown sizes)".into()
            }
            Explanation::ConcurrentActivity { noise } => format!(
                "other processes may have written to or freed space on the volume meanwhile \
                 (sample noise {})",
                fmt_bytes(noise)
            ),
        }
    }

    /// Notes for one plan item, independent of any measurement: why the item may free less
    /// (or more) than its logical size suggests, from its estimate notes and what its action
    /// does with the bytes. For per-item reporting next to the item's audit record.
    pub fn for_item(predicted: &ActionReclaim, notes: &[EstimateNote]) -> Vec<Explanation> {
        let mut out = Vec::new();
        if predicted.moved > 0 {
            out.push(Explanation::MovedNotFreed {
                bytes: predicted.moved,
            });
        }
        for n in notes {
            let e = match *n {
                EstimateNote::SharedWithLinks { bytes } => Explanation::SharedHardlinks { bytes },
                EstimateNote::UnattributedLinks => Explanation::UnattributedLinks,
                EstimateNote::ClonesPossible(fs) => Explanation::ClonesPossible { fs },
                EstimateNote::SnapshotsPresent => Explanation::SnapshotsPresent,
                EstimateNote::SnapshotsPossible => Explanation::SnapshotsPossible,
                EstimateNote::SnapshotsUnknown => Explanation::SnapshotsUnknown,
                EstimateNote::InsideVmImage => Explanation::InsideVmImage,
                EstimateNote::PurgeableSpace => Explanation::PurgeableSpace,
                EstimateNote::Incomplete | EstimateNote::AllocatedUnknown => {
                    Explanation::EstimateIncomplete
                }
                // Compression / sparse files are already in the allocated-based prediction;
                // an unknown filesystem only weakens confidence.
                _ => continue,
            };
            if !out.contains(&e) {
                out.push(e);
            }
        }
        out
    }
}

/// The measured result of an operation (PHY-02): predicted reclaim, bytes removed and moved,
/// the measured free-space delta per volume and in total, and explanations.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct MeasuredDelta {
    /// Before / after per measured volume.
    pub per_volume: Vec<VolumeDelta>,
    /// Paths that could not be measured.
    pub unavailable: Vec<Unmeasured>,
    /// The plan's prediction.
    pub predicted: PredictedReclaim,
    /// Allocated bytes removed by the Executor.
    pub removed: u64,
    /// Bytes moved aside by the Executor.
    pub moved: u64,
    /// Sum of the per-volume deltas; `None` when no volume could be measured.
    pub measured_total: Option<i64>,
    /// `(measured - predicted.freed_now) / predicted.freed_now` in percent; `None` without a
    /// measurement or with a zero prediction.
    pub discrepancy_pct: Option<f64>,
    /// Predicted and measured differ by more than [`DISCREPANCY_THRESHOLD_PCT`] (PHY-02).
    pub exceeds_threshold: bool,
    /// The difference is within resolution plus noise: they agree even if the percentage is
    /// large (tiny predictions).
    pub within_resolution: bool,
    /// What the meter observed.
    pub observations: Observations,
    /// Explanations, most specific first. Always non-empty when
    /// [`MeasuredDelta::exceeds_threshold`] is set; always-on notes (moved not freed, purgeable
    /// space, bytes shared with hard links, unmeasured volumes) appear regardless.
    pub explanations: Vec<Explanation>,
}

impl MeasuredDelta {
    /// Build the result from measured volumes. Pure: all explanation logic lives here.
    pub fn from_volumes(
        per_volume: Vec<VolumeDelta>,
        unavailable: Vec<Unmeasured>,
        tally: &Tally,
        observations: Observations,
    ) -> Self {
        let measured_total = if per_volume.is_empty() {
            None
        } else {
            Some(
                per_volume
                    .iter()
                    .fold(0i64, |a, v| a.saturating_add(v.delta)),
            )
        };
        let predicted = tally.predicted.freed_now;
        let noise: u64 = per_volume.iter().map(|v| v.noise).sum();
        let tolerance = observations.resolution.saturating_add(noise);
        let (exceeds, within, pct) = match measured_total {
            Some(m) => {
                let diff = (i128::from(m) - i128::from(predicted)).unsigned_abs();
                let pct = (predicted > 0)
                    .then(|| (m as f64 - predicted as f64) / predicted as f64 * 100.0);
                (
                    exceeds_threshold(predicted, m),
                    diff <= u128::from(tolerance),
                    pct,
                )
            }
            None => (false, false, None),
        };
        let mut d = Self {
            per_volume,
            unavailable,
            predicted: tally.predicted,
            removed: tally.removed,
            moved: tally.moved,
            measured_total,
            discrepancy_pct: pct,
            exceeds_threshold: exceeds,
            within_resolution: within,
            observations,
            explanations: Vec::new(),
        };
        d.explanations = d.explain(tally, noise, tolerance);
        d
    }

    fn explain(&self, tally: &Tally, noise: u64, tolerance: u64) -> Vec<Explanation> {
        let mut out = Vec::new();
        let push = |out: &mut Vec<Explanation>, e: Explanation| {
            if !out.contains(&e) {
                out.push(e);
            }
        };
        let has = |n: EstimateNote| tally.notes.contains(&n);

        // Always-on notes.
        let moved = self.moved.max(tally.predicted.moved);
        if moved > 0 {
            push(&mut out, Explanation::MovedNotFreed { bytes: moved });
        }
        if has(EstimateNote::PurgeableSpace)
            || self
                .per_volume
                .iter()
                .any(|v| v.volume.fs == VolumeFs::Apfs)
        {
            push(&mut out, Explanation::PurgeableSpace);
        }
        // Hard links explain why bytes removed did not free space, agreeing or not.
        for n in &tally.notes {
            if let EstimateNote::SharedWithLinks { bytes } = *n
                && bytes > 0
            {
                push(&mut out, Explanation::SharedHardlinks { bytes });
            }
        }
        if !self.unavailable.is_empty() {
            push(
                &mut out,
                Explanation::MeasurementUnavailable {
                    paths: self.unavailable.len(),
                },
            );
        }
        let Some(measured) = self.measured_total else {
            return out;
        };
        if !self.exceeds_threshold {
            return out;
        }
        if self.within_resolution {
            push(&mut out, Explanation::BelowResolution { tolerance });
            return out;
        }

        let shortfall = i128::from(measured) < i128::from(self.predicted.freed_now);
        if shortfall {
            if tally.not_executed > 0 {
                push(
                    &mut out,
                    Explanation::ItemsNotExecuted {
                        items: tally.not_executed,
                        bytes: tally.not_executed_bytes,
                    },
                );
            }
            if tally.partial > 0 {
                push(
                    &mut out,
                    Explanation::PartiallyExecuted {
                        items: tally.partial,
                    },
                );
            }
            if self.observations.delayed_free_observed {
                let fs = self
                    .per_volume
                    .iter()
                    .find(|v| v.volume.frees_asynchronously())
                    .or(self.per_volume.first())
                    .map_or(VolumeFs::Unknown, |v| v.volume.fs);
                push(&mut out, Explanation::DelayedFree { fs, observed: true });
            }
            if has(EstimateNote::UnattributedLinks) {
                push(&mut out, Explanation::UnattributedLinks);
            }
            // Clones: from the estimate, or from the measured volume's filesystem.
            let mut clone_fs: Vec<VolumeFs> = tally
                .notes
                .iter()
                .filter_map(|n| match n {
                    EstimateNote::ClonesPossible(fs) => Some(*fs),
                    _ => None,
                })
                .collect();
            clone_fs.extend(
                self.per_volume
                    .iter()
                    .filter(|v| v.volume.clones_possible())
                    .map(|v| v.volume.fs),
            );
            for fs in clone_fs {
                push(&mut out, Explanation::ClonesPossible { fs });
            }
            // Snapshots: the strongest statement known.
            let vol_snapshots = self
                .per_volume
                .iter()
                .any(|v| v.volume.snapshots == Some(true));
            if has(EstimateNote::SnapshotsPresent) {
                push(&mut out, Explanation::SnapshotsPresent);
            } else if has(EstimateNote::SnapshotsPossible) || vol_snapshots {
                push(&mut out, Explanation::SnapshotsPossible);
            } else if has(EstimateNote::SnapshotsUnknown) {
                push(&mut out, Explanation::SnapshotsUnknown);
            }
            if !self.observations.delayed_free_observed {
                for v in self
                    .per_volume
                    .iter()
                    .filter(|v| v.volume.frees_asynchronously())
                {
                    push(
                        &mut out,
                        Explanation::DelayedFree {
                            fs: v.volume.fs,
                            observed: false,
                        },
                    );
                }
            }
            if has(EstimateNote::InsideVmImage) {
                push(&mut out, Explanation::InsideVmImage);
            }
        } else if has(EstimateNote::Incomplete) || has(EstimateNote::AllocatedUnknown) {
            push(&mut out, Explanation::EstimateIncomplete);
        }
        // Always possible, never provable: last.
        push(&mut out, Explanation::ConcurrentActivity { noise });
        out
    }

    /// The measured total for [`OperationFinish::measured_delta_bytes`] (I-5).
    pub fn measured_delta_bytes(&self) -> Option<i64> {
        self.measured_total
    }

    /// The audit log's end-of-operation record carrying the measured delta (I-5).
    pub fn operation_finish(&self, finished_at: i64, end: OperationEnd) -> OperationFinish {
        OperationFinish {
            finished_at,
            end,
            measured_delta_bytes: self.measured_delta_bytes(),
        }
    }

    /// Predicted and measured agree: within 10%, or within resolution.
    pub fn agrees(&self) -> bool {
        self.measured_total.is_some() && (!self.exceeds_threshold || self.within_resolution)
    }

    /// The explanations' stable codes.
    pub fn explanation_codes(&self) -> Vec<&'static str> {
        self.explanations.iter().map(Explanation::code).collect()
    }
}

/// The accuracy report: predicted vs measured, per volume, with explanations.
impl fmt::Display for MeasuredDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = &self.predicted;
        writeln!(
            f,
            "predicted reclaimable: {} freed now, {} moved, {} after purge (confidence {:?})",
            fmt_bytes(p.freed_now),
            fmt_bytes(p.moved),
            fmt_bytes(p.freed_after_purge),
            p.confidence
        )?;
        writeln!(
            f,
            "bytes removed: {} · bytes moved: {}",
            fmt_bytes(self.removed),
            fmt_bytes(self.moved)
        )?;
        match self.measured_total {
            Some(m) => {
                let pct = self
                    .discrepancy_pct
                    .map_or_else(|| "n/a".to_owned(), |x| format!("{x:+.1}%"));
                let verdict = if !self.exceeds_threshold {
                    "within 10%"
                } else if self.within_resolution {
                    "within resolution"
                } else {
                    "differs by more than 10%"
                };
                writeln!(
                    f,
                    "measured free-space delta: {} (discrepancy {pct}; {verdict})",
                    fmt_signed(m)
                )?;
            }
            None => writeln!(f, "measured free-space delta: not measured")?,
        }
        for v in &self.per_volume {
            writeln!(
                f,
                "  {} [{}]: before {} after {} delta {} (noise {})",
                v.volume.mount_point.display(),
                v.volume.fs_type,
                fmt_bytes(v.before),
                fmt_bytes(v.after),
                fmt_signed(v.delta),
                fmt_bytes(v.noise)
            )?;
        }
        for u in &self.unavailable {
            writeln!(f, "  {}: not measured ({})", u.path.display(), u.reason)?;
        }
        if !self.observations.waited.is_zero() {
            writeln!(
                f,
                "  re-sampled for {} ms waiting for delayed frees",
                self.observations.waited.as_millis()
            )?;
        }
        for e in &self.explanations {
            writeln!(f, "  - {}: {}", e.code(), e.message())?;
        }
        Ok(())
    }
}

fn fs_name(fs: VolumeFs) -> &'static str {
    match fs {
        VolumeFs::Apfs => "APFS",
        VolumeFs::HfsPlus => "HFS+",
        VolumeFs::Btrfs => "Btrfs",
        VolumeFs::Xfs => "XFS",
        VolumeFs::Zfs => "ZFS",
        VolumeFs::Ext4 => "ext4",
        VolumeFs::Ntfs => "NTFS",
        VolumeFs::Refs => "ReFS",
        VolumeFs::Fat => "FAT",
        VolumeFs::Tmpfs => "tmpfs",
        _ => "the filesystem",
    }
}

fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

fn fmt_signed(d: i64) -> String {
    let sign = if d < 0 { "-" } else { "+" };
    format!("{sign}{}", fmt_bytes(d.unsigned_abs()))
}

#[cfg(test)]
mod tests {
    //! Pure tests: every PRD §9 row's explanation, the >10% rule (PHY-02), sampling and the
    //! audit helpers, against an injected free-space source (no I/O).
    #![allow(clippy::unwrap_used)]

    use super::*;
    use scull_core::{Action, Reclaimable, Sizes};
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    /// Scripted free space: each path pops its next value; the last one repeats.
    #[derive(Default)]
    struct Fake {
        volumes: HashMap<PathBuf, VolumeProbe>,
        free: RefCell<HashMap<PathBuf, VecDeque<u64>>>,
        failing: Vec<PathBuf>,
        slept: RefCell<Vec<Duration>>,
    }

    impl Fake {
        fn volume(mut self, path: &str, probe: VolumeProbe, free: &[u64]) -> Self {
            self.volumes.insert(PathBuf::from(path), probe);
            self.free
                .borrow_mut()
                .insert(PathBuf::from(path), free.iter().copied().collect());
            self
        }

        fn fail(mut self, path: &str) -> Self {
            self.failing.push(PathBuf::from(path));
            self
        }
    }

    impl FreeSpaceSource for Fake {
        fn describe(&self, path: &Path) -> Result<VolumeProbe, ProbeError> {
            self.volumes
                .get(path)
                .cloned()
                .ok_or_else(|| ProbeError(format!("no volume for {}", path.display())))
        }

        fn free_bytes(&self, path: &Path) -> Result<u64, ProbeError> {
            if self.failing.iter().any(|p| p == path) {
                return Err(ProbeError("gone".into()));
            }
            let mut free = self.free.borrow_mut();
            let q = free
                .get_mut(path)
                .ok_or_else(|| ProbeError(format!("not found: {}", path.display())))?;
            if q.len() > 1 {
                Ok(q.pop_front().unwrap())
            } else {
                q.front().copied().ok_or_else(|| ProbeError("empty".into()))
            }
        }

        fn sleep(&self, d: Duration) {
            self.slept.borrow_mut().push(d);
        }
    }

    fn ext4(id: u64) -> VolumeProbe {
        VolumeProbe::new("/", "ext4")
            .with_id(id)
            .with_features(Some(false), Some(false))
    }

    fn sizes(logical: u64, allocated: u64, reclaim: u64, shared: u64, c: Confidence) -> Sizes {
        Sizes {
            logical,
            allocated,
            reclaimable: Reclaimable {
                bytes: reclaim,
                confidence: c,
                shared_bytes: shared,
            },
        }
    }

    fn delete(bytes: u64, c: Confidence) -> ActionReclaim {
        ActionReclaim::for_action(&sizes(bytes, bytes, bytes, 0, c), &Action::Delete)
    }

    fn done(removed: u64) -> ItemExecution {
        ItemExecution::Done { removed, moved: 0 }
    }

    /// A one-volume result: `before` → `after` on `probe`, noise `noise`, for `tally`.
    fn measure(probe: VolumeProbe, before: u64, after: u64, tally: &Tally) -> MeasuredDelta {
        measure_obs(probe, before, after, tally, Observations::new(MIB))
    }

    fn measure_obs(
        probe: VolumeProbe,
        before: u64,
        after: u64,
        tally: &Tally,
        obs: Observations,
    ) -> MeasuredDelta {
        let v = VolumeDelta::new(
            probe,
            Sample {
                free: before,
                spread: 0,
            },
            Sample {
                free: after,
                spread: 0,
            },
        );
        MeasuredDelta::from_volumes(vec![v], vec![], tally, obs)
    }

    fn codes(d: &MeasuredDelta) -> Vec<&'static str> {
        d.explanation_codes()
    }

    // ---- PHY-02: the >10% rule ----------------------------------------------------------------

    #[test]
    fn phy_02_threshold_is_strictly_more_than_ten_percent() {
        assert!(!exceeds_threshold(100, 100));
        assert!(!exceeds_threshold(100, 110));
        assert!(!exceeds_threshold(100, 90));
        assert!(exceeds_threshold(100, 111));
        assert!(exceeds_threshold(100, 89));
        assert!(exceeds_threshold(100, -5));
        assert!(!exceeds_threshold(0, 0));
        assert!(exceeds_threshold(0, 1));
        assert!(exceeds_threshold(0, -1));
        // No overflow at the extremes.
        assert!(exceeds_threshold(u64::MAX, i64::MIN));
    }

    #[test]
    fn phy_02_within_ten_percent_needs_no_explanation() {
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::High), &[], done(100 * MIB));
        for after in [
            10 * GIB + 91 * MIB,
            10 * GIB + 100 * MIB,
            10 * GIB + 110 * MIB,
        ] {
            let d = measure(ext4(1), 10 * GIB, after, &t);
            assert!(!d.exceeds_threshold, "{d}");
            assert!(d.agrees());
            assert!(d.explanations.is_empty(), "{d}");
        }
        let d = measure(ext4(1), 10 * GIB, 10 * GIB + 100 * MIB, &t);
        assert_eq!(d.discrepancy_pct, Some(0.0));
        assert_eq!(d.measured_total, Some(i64::try_from(100 * MIB).unwrap()));
        assert_eq!((d.removed, d.moved), (100 * MIB, 0));
    }

    #[test]
    fn phy_02_more_than_ten_percent_always_explained() {
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::High), &[], done(100 * MIB));
        for after in [10 * GIB, 10 * GIB + 50 * MIB, 10 * GIB + 200 * MIB, 9 * GIB] {
            let d = measure(ext4(1), 10 * GIB, after, &t);
            assert!(d.exceeds_threshold, "{d}");
            assert!(!d.agrees());
            assert!(!d.explanations.is_empty(), "{d}");
            // With nothing else known, other processes are the remaining explanation.
            assert_eq!(codes(&d), ["concurrent_activity"], "{d}");
        }
        let d = measure(ext4(1), 10 * GIB, 10 * GIB + 50 * MIB, &t);
        assert_eq!(d.discrepancy_pct, Some(-50.0));
    }

    #[test]
    fn tiny_predictions_within_resolution_agree_with_an_explanation() {
        let mut t = Tally::new();
        t.add(&delete(8 * 1024, Confidence::High), &[], done(8 * 1024));
        // 8 KiB predicted, 200 KiB measured (unrelated writes): >10% but below 1 MiB.
        let d = measure(ext4(1), GIB, GIB + 200 * 1024, &t);
        assert!(
            d.exceeds_threshold && d.within_resolution && d.agrees(),
            "{d}"
        );
        assert_eq!(codes(&d), ["below_resolution"]);
        assert_eq!(
            d.explanations[0],
            Explanation::BelowResolution { tolerance: MIB }
        );
    }

    // ---- PRD §9 rows --------------------------------------------------------------------------

    /// Hard links (FX-PHY-01): a pnpm `node_modules` hard-linked to the store predicts ≈ 0 with
    /// an explanation, and a measured ≈ 0 agrees.
    #[test]
    fn fx_phy_01_hardlinks_predict_zero_and_measured_zero_agrees() {
        let notes = [EstimateNote::SharedWithLinks { bytes: 300 * MIB }];
        let s = sizes(300 * MIB, 300 * MIB, 0, 300 * MIB, Confidence::High);
        let r = ActionReclaim::for_action(&s, &Action::Delete);
        assert_eq!(r.freed_now, 0);
        let item = Explanation::for_item(&r, &notes);
        assert_eq!(item, [Explanation::SharedHardlinks { bytes: 300 * MIB }]);
        assert!(
            item[0].message().contains("pnpm store prune"),
            "{}",
            item[0].message()
        );

        let mut t = Tally::new();
        t.add(&r, &notes, done(300 * MIB));
        let d = measure(ext4(1), GIB, GIB + 64 * 1024, &t);
        assert!(d.agrees(), "{d}");
        assert_eq!(codes(&d), ["shared_hardlinks", "below_resolution"]);
        // Explained even when measured equals predicted.
        let d = measure(ext4(1), GIB, GIB, &t);
        assert_eq!(codes(&d), ["shared_hardlinks"]);
        assert_eq!(
            d.removed,
            300 * MIB,
            "walker unlinked the links (upper bound)"
        );
    }

    /// Hard links when the prediction did not know: deleting frees far less → explained.
    #[test]
    fn hardlinks_explain_a_shortfall() {
        let notes = [
            EstimateNote::SharedWithLinks { bytes: 40 * MIB },
            EstimateNote::UnattributedLinks,
        ];
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::Low), &notes, done(100 * MIB));
        let d = measure(ext4(1), GIB, GIB + 20 * MIB, &t);
        assert_eq!(
            codes(&d),
            [
                "shared_hardlinks",
                "unattributed_links",
                "concurrent_activity"
            ]
        );
    }

    /// CoW clones (FX-PHY-02): APFS clone, confidence low, measured delta reported and
    /// explained (plus the purgeable-space note on APFS).
    #[test]
    fn fx_phy_02_apfs_clone_low_confidence_explained() {
        let notes = [
            EstimateNote::ClonesPossible(VolumeFs::Apfs),
            EstimateNote::PurgeableSpace,
        ];
        let mut t = Tally::new();
        let r = delete(500 * MIB, Confidence::Low);
        assert_eq!(r.confidence, Confidence::Low);
        t.add(&r, &notes, done(500 * MIB));
        let apfs = VolumeProbe::new("/System/Volumes/Data", "apfs").with_id(7);
        let d = measure(apfs, 50 * GIB, 50 * GIB + MIB * 3, &t);
        assert!(d.exceeds_threshold);
        assert_eq!(d.predicted.confidence, Confidence::Low);
        assert_eq!(d.measured_total, Some(i64::try_from(3 * MIB).unwrap()));
        let c = codes(&d);
        assert!(c.contains(&"clones_possible"), "{c:?}");
        assert!(c.contains(&"purgeable_space"), "{c:?}");
        assert!(
            d.explanations
                .contains(&Explanation::ClonesPossible { fs: VolumeFs::Apfs })
        );
    }

    /// Clones are also explained from the measured volume's filesystem alone (Btrfs, XFS,
    /// ReFS), and never on ext4 / NTFS.
    #[test]
    fn clone_filesystems_explain_shortfall_from_the_volume() {
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::High), &[], done(100 * MIB));
        for (name, fs) in [
            ("btrfs", VolumeFs::Btrfs),
            ("xfs", VolumeFs::Xfs),
            ("ReFS", VolumeFs::Refs),
        ] {
            let d = measure(VolumeProbe::new("/v", name), GIB, GIB, &t);
            assert!(
                d.explanations.contains(&Explanation::ClonesPossible { fs }),
                "{name}: {d}"
            );
        }
        // Features say no clones (e.g. XFS without reflink): not claimed.
        let no = VolumeProbe::new("/v", "xfs").with_features(Some(false), Some(false));
        assert!(!codes(&measure(no, GIB, GIB, &t)).contains(&"clones_possible"));
        for name in ["ext4", "NTFS"] {
            let d = measure(VolumeProbe::new("/v", name), GIB, GIB, &t);
            assert!(!codes(&d).contains(&"clones_possible"), "{name}: {d}");
        }
    }

    /// Snapshots (FX-PHY-03 APFS local snapshots, FX-PHY-04 Btrfs): measured lower than
    /// predicted and explained.
    #[test]
    fn fx_phy_03_04_snapshots_pin_deleted_data() {
        let mut t = Tally::new();
        t.add(
            &delete(200 * MIB, Confidence::Low),
            &[EstimateNote::SnapshotsPresent],
            done(200 * MIB),
        );
        let d = measure(VolumeProbe::new("/", "apfs"), GIB, GIB, &t);
        assert!(codes(&d).contains(&"snapshots_present"), "{d}");
        assert!(
            Explanation::SnapshotsPresent
                .message()
                .contains("until snapshots expire")
        );

        // Btrfs: snapshots supported by the volume, no estimate note → possible; Btrfs frees
        // asynchronously too.
        let mut t = Tally::new();
        t.add(&delete(200 * MIB, Confidence::Low), &[], done(200 * MIB));
        let btrfs = VolumeProbe::new("/home", "btrfs").with_features(Some(true), Some(true));
        let d = measure(btrfs, GIB, GIB + 5 * MIB, &t);
        let c = codes(&d);
        assert!(c.contains(&"snapshots_possible"), "{c:?}");
        assert!(
            d.explanations.contains(&Explanation::DelayedFree {
                fs: VolumeFs::Btrfs,
                observed: false
            }),
            "{d}"
        );

        // Unknown snapshot state is said as such.
        let mut t = Tally::new();
        t.add(
            &delete(200 * MIB, Confidence::Medium),
            &[EstimateNote::SnapshotsUnknown],
            done(200 * MIB),
        );
        assert!(codes(&measure(ext4(1), GIB, GIB, &t)).contains(&"snapshots_unknown"));
    }

    /// Transparent compression / sparse files (FX-PHY-05): the prediction uses allocated, not
    /// logical, bytes, so a measured delta equal to the allocated size agrees.
    #[test]
    fn fx_phy_05_compression_uses_allocated_bytes() {
        let s = sizes(100 * MIB, 40 * MIB, 40 * MIB, 0, Confidence::High);
        let r = ActionReclaim::for_action(&s, &Action::Delete);
        assert_eq!(r.freed_now, 40 * MIB, "allocated, not logical");
        let notes = [EstimateNote::CompressedOrSparse { saved: 60 * MIB }];
        assert!(Explanation::for_item(&r, &notes).is_empty());
        let mut t = Tally::new();
        t.add(&r, &notes, done(40 * MIB));
        let d = measure(VolumeProbe::new("C:\\", "NTFS"), GIB, GIB + 40 * MIB, &t);
        assert!(d.agrees() && d.explanations.is_empty(), "{d}");
        assert_eq!(t.notes, notes);
    }

    /// Trash / quarantine (FX-PHY-07): reported as moved, not freed, even when the
    /// measurement agrees.
    #[test]
    fn fx_phy_07_trash_and_quarantine_are_moved_not_freed() {
        for action in [Action::Trash, Action::Quarantine] {
            let s = sizes(80 * MIB, 64 * MIB, 64 * MIB, 0, Confidence::High);
            let r = ActionReclaim::for_action(&s, &action);
            assert_eq!(
                (r.freed_now, r.moved, r.freed_after_purge),
                (0, 64 * MIB, 64 * MIB)
            );
            let mut t = Tally::new();
            t.add(
                &r,
                &[],
                ItemExecution::Done {
                    removed: 0,
                    moved: 64 * MIB,
                },
            );
            let d = measure(ext4(1), GIB, GIB, &t);
            assert!(d.agrees(), "{action:?}: {d}");
            assert_eq!(codes(&d), ["moved_not_freed"], "{action:?}");
            assert_eq!(d.moved, 64 * MIB);
            assert_eq!(d.predicted.freed_after_purge, 64 * MIB);
            assert!(d.explanations[0].message().contains("moved, not freed"));
            assert_eq!(
                Explanation::for_item(&r, &[]),
                [Explanation::MovedNotFreed { bytes: 64 * MIB }]
            );
        }
    }

    /// VM disk images: pruning inside the VM may not shrink the host file; never claim host
    /// bytes without measuring.
    #[test]
    fn vm_disk_images_explain_a_shortfall() {
        let s = sizes(2 * GIB, 2 * GIB, 2 * GIB, 0, Confidence::Unknown);
        let r = ActionReclaim::for_action(
            &s,
            &Action::ToolCommand {
                tool: "docker".into(),
                args: vec!["system".into(), "prune".into()],
            },
        );
        let notes = [EstimateNote::InsideVmImage];
        let mut t = Tally::new();
        t.add(&r, &notes, done(0));
        let d = measure(VolumeProbe::new("/", "apfs"), 20 * GIB, 20 * GIB, &t);
        assert!(codes(&d).contains(&"inside_vm_image"), "{d}");
        assert_eq!(d.predicted.confidence, Confidence::Unknown);
        assert_eq!(
            Explanation::for_item(&r, &notes),
            [Explanation::InsideVmImage]
        );
    }

    /// macOS purgeable space: noted whenever measuring on APFS; a gain larger than predicted
    /// is explained by it (and by other activity).
    #[test]
    fn purgeable_space_is_noted_on_apfs() {
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::Low), &[], done(100 * MIB));
        let apfs = VolumeProbe::new("/", "apfs");
        let d = measure(apfs.clone(), GIB, GIB + 100 * MIB, &t);
        assert!(d.agrees());
        assert!(codes(&d).contains(&"purgeable_space"), "{d}");
        let d = measure(apfs, GIB, GIB + 900 * MIB, &t);
        assert_eq!(codes(&d), ["purgeable_space", "concurrent_activity"], "{d}");
    }

    /// Delayed frees (Btrfs/ZFS async free, ext4 journal commit): the meter re-samples while
    /// short and reports the free as observed late.
    #[test]
    fn delayed_free_is_resampled_and_reported() {
        let before = 10 * GIB;
        // Before: 3 samples. After: settle, 3 samples (short), then frees arrive.
        let fake = Fake::default().volume(
            "/data",
            VolumeProbe::new("/data", "btrfs").with_id(3),
            &[
                before,
                before,
                before,
                before,
                before,
                before,
                before + 60 * MIB,
                before + 100 * MIB,
            ],
        );
        let meter = VolumeMeter::new(
            fake,
            MeasureOptions::default()
                .with_resampling(Duration::from_millis(500), Duration::from_secs(5)),
        );
        let base = meter.before(["/data"]);
        assert_eq!(base.volume_count(), 1);
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::Low), &[], done(100 * MIB));
        let d = meter.after(base, &t);
        assert!(d.observations.delayed_free_observed, "{d}");
        assert!(d.observations.waited > Duration::ZERO);
        assert!(d.agrees(), "{d}");
        let slept = meter.source.slept.borrow();
        assert!(slept.contains(&Duration::from_millis(500)), "{slept:?}");
        assert!(slept.iter().sum::<Duration>() < Duration::from_secs(6));
    }

    #[test]
    fn delayed_free_observed_but_still_short_is_explained() {
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::High), &[], done(100 * MIB));
        let obs = Observations::new(MIB).with_delayed_free(Duration::from_secs(5));
        let d = measure_obs(ext4(1), GIB, GIB + 50 * MIB, &t, obs);
        assert_eq!(
            d.explanations[0],
            Explanation::DelayedFree {
                fs: VolumeFs::Ext4,
                observed: true
            }
        );
    }

    #[test]
    fn resampling_stops_at_max_settle() {
        let fake = Fake::default().volume("/d", ext4(1), &[GIB]);
        let meter = VolumeMeter::new(
            fake,
            MeasureOptions::immediate(MIB)
                .with_resampling(Duration::from_millis(250), Duration::from_secs(1)),
        );
        let base = meter.before(["/d"]);
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::High), &[], done(100 * MIB));
        let d = meter.after(base, &t);
        assert_eq!(d.observations.waited, Duration::from_secs(1));
        assert!(!d.observations.delayed_free_observed);
        assert_eq!(codes(&d), ["concurrent_activity"]);
    }

    /// Other processes writing: noisy samples are recorded as noise and widen the tolerance.
    #[test]
    fn concurrent_activity_noise_is_recorded() {
        let fake = Fake::default().volume(
            "/d",
            ext4(1),
            &[
                GIB,
                GIB + 3 * MIB,
                GIB - MIB,
                GIB + 100 * MIB,
                GIB + 96 * MIB,
                GIB + 99 * MIB,
            ],
        );
        let meter = VolumeMeter::new(fake, MeasureOptions::immediate(MIB).with_samples(3));
        let base = meter.before(["/d"]);
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::High), &[], done(100 * MIB));
        let d = meter.after(base, &t);
        let v = &d.per_volume[0];
        assert_eq!((v.before, v.after), (GIB, GIB + 99 * MIB), "medians");
        assert_eq!(v.noise, 4 * MIB);
        assert!(d.agrees(), "{d}");
    }

    #[test]
    fn items_not_executed_and_partial_explain_a_shortfall() {
        let mut t = Tally::new();
        t.add(&delete(100 * MIB, Confidence::High), &[], done(100 * MIB));
        t.add(
            &delete(100 * MIB, Confidence::High),
            &[],
            ItemExecution::NotExecuted,
        );
        t.add(
            &delete(100 * MIB, Confidence::High),
            &[],
            ItemExecution::Partial {
                removed: 30 * MIB,
                moved: 0,
            },
        );
        assert_eq!(t.predicted.freed_now, 300 * MIB);
        assert_eq!((t.removed, t.not_executed, t.partial), (130 * MIB, 1, 1));
        let d = measure(ext4(1), GIB, GIB + 130 * MIB, &t);
        assert_eq!(
            codes(&d),
            [
                "items_not_executed",
                "partially_executed",
                "concurrent_activity"
            ]
        );
        assert_eq!(
            d.explanations[0],
            Explanation::ItemsNotExecuted {
                items: 1,
                bytes: 100 * MIB
            }
        );
    }

    #[test]
    fn incomplete_estimate_explains_an_excess() {
        let mut t = Tally::new();
        t.add(
            &delete(100 * MIB, Confidence::Unknown),
            &[EstimateNote::Incomplete, EstimateNote::AllocatedUnknown],
            done(100 * MIB),
        );
        let d = measure(ext4(1), GIB, GIB + 300 * MIB, &t);
        assert_eq!(codes(&d), ["estimate_incomplete", "concurrent_activity"]);
        // Shortfall causes are not offered for an excess.
        let mut t = Tally::new();
        t.add(
            &delete(100 * MIB, Confidence::Low),
            &[EstimateNote::SnapshotsPresent],
            done(100 * MIB),
        );
        let d = measure(ext4(1), GIB, GIB + 300 * MIB, &t);
        assert_eq!(codes(&d), ["concurrent_activity"]);
    }

    // ---- meter --------------------------------------------------------------------------------

    #[test]
    fn before_measures_each_volume_once_and_records_failures() {
        let fake = Fake::default()
            .volume("/a", ext4(1), &[GIB])
            .volume("/a/b", ext4(1), &[GIB])
            .volume("/q", ext4(2).with_features(None, None), &[5 * GIB])
            .volume("/bad", ext4(9), &[1])
            .fail("/bad");
        let meter = VolumeMeter::new(fake, MeasureOptions::immediate(MIB));
        let base = meter.before(["/a", "/a/b", "/q", "/missing", "/bad"]);
        assert_eq!(base.volume_count(), 2);
        assert_eq!(base.unavailable().len(), 2);
        let d = meter.after(base, &Tally::new());
        assert_eq!(d.per_volume.len(), 2);
        assert_eq!(d.measured_total, Some(0));
        assert_eq!(
            d.explanations,
            [Explanation::MeasurementUnavailable { paths: 2 }]
        );
    }

    #[test]
    fn nothing_measurable_gives_no_total() {
        let meter = VolumeMeter::new(Fake::default(), MeasureOptions::immediate(MIB));
        let base = meter.before(["/nowhere"]);
        let mut t = Tally::new();
        t.add(&delete(MIB, Confidence::High), &[], done(MIB));
        let d = meter.after(base, &t);
        assert_eq!(d.measured_total, None);
        assert_eq!(d.measured_delta_bytes(), None);
        assert!(!d.agrees() && !d.exceeds_threshold);
        assert_eq!(codes(&d), ["measurement_unavailable"]);
        assert!(d.to_string().contains("not measured"));
    }

    #[test]
    fn a_path_gone_after_the_operation_falls_back_to_the_mount_point() {
        let fake = Fake::default()
            .volume("/m/item", VolumeProbe::new("/m", "ext4").with_id(4), &[GIB])
            .volume(
                "/m",
                VolumeProbe::new("/m", "ext4").with_id(4),
                &[GIB + 10 * MIB],
            );
        let meter = VolumeMeter::new(fake, MeasureOptions::immediate(MIB));
        let base = meter.before(["/m/item"]);
        // The item path vanishes.
        meter.source.free.borrow_mut().remove(Path::new("/m/item"));
        let mut t = Tally::new();
        t.add(&delete(10 * MIB, Confidence::High), &[], done(10 * MIB));
        let d = meter.after(base, &t);
        assert_eq!(d.measured_total, Some(i64::try_from(10 * MIB).unwrap()));
        assert!(d.agrees());
    }

    #[test]
    fn multiple_volumes_sum_and_quarantine_elsewhere_nets_out() {
        // Item on volume 1 moved to a quarantine on volume 2: +64 MiB there, -64 MiB here.
        let a = VolumeDelta::new(
            ext4(1),
            Sample {
                free: GIB,
                spread: 0,
            },
            Sample {
                free: GIB + 64 * MIB,
                spread: 0,
            },
        );
        let b = VolumeDelta::new(
            ext4(2),
            Sample {
                free: GIB,
                spread: 0,
            },
            Sample {
                free: GIB - 64 * MIB,
                spread: 0,
            },
        );
        let s = sizes(64 * MIB, 64 * MIB, 64 * MIB, 0, Confidence::High);
        let mut t = Tally::new();
        t.add(
            &ActionReclaim::for_action(&s, &Action::Quarantine),
            &[],
            ItemExecution::Done {
                removed: 0,
                moved: 64 * MIB,
            },
        );
        let d = MeasuredDelta::from_volumes(vec![a, b], vec![], &t, Observations::new(MIB));
        assert_eq!(d.measured_total, Some(0));
        assert!(d.agrees());
        assert_eq!(d.per_volume[1].delta, -i64::try_from(64 * MIB).unwrap());
    }

    // ---- audit and report ---------------------------------------------------------------------

    /// I-5: the result feeds the audit log's measured delta next to the store's predicted total.
    #[test]
    fn operation_finish_carries_the_measured_delta() {
        let mut t = Tally::new();
        t.add(&delete(64 * MIB, Confidence::High), &[], done(64 * MIB));
        let d = measure(ext4(1), GIB, GIB + 64 * MIB, &t);
        let fin = d.operation_finish(1_700_000_000, OperationEnd::Normal);
        assert_eq!(
            fin.measured_delta_bytes,
            Some(i64::try_from(64 * MIB).unwrap())
        );
        assert_eq!(fin.finished_at, 1_700_000_000);
        assert_eq!(fin.end, OperationEnd::Normal);
    }

    #[test]
    fn report_lists_predicted_removed_moved_measured_and_explanations() {
        let mut t = Tally::new();
        t.add(
            &delete(100 * MIB, Confidence::Low),
            &[EstimateNote::SnapshotsPresent],
            done(100 * MIB),
        );
        let d = measure(ext4(1), GIB, GIB + 10 * MIB, &t);
        let r = d.to_string();
        println!("{r}");
        for needle in [
            "predicted reclaimable: 100.0 MiB freed now",
            "bytes removed: 100.0 MiB",
            "bytes moved: 0 B",
            "measured free-space delta: +10.0 MiB (discrepancy -90.0%; differs by more than 10%)",
            "/ [ext4]: before 1.0 GiB after 1.0 GiB delta +10.0 MiB",
            "- snapshots_present: snapshots exist",
            "- concurrent_activity:",
        ] {
            assert!(r.contains(needle), "missing {needle:?} in\n{r}");
        }
    }

    #[test]
    fn every_explanation_has_a_distinct_code_and_a_message() {
        let all = [
            Explanation::MovedNotFreed { bytes: 1 },
            Explanation::PurgeableSpace,
            Explanation::MeasurementUnavailable { paths: 1 },
            Explanation::BelowResolution { tolerance: 1 },
            Explanation::ItemsNotExecuted { items: 1, bytes: 1 },
            Explanation::PartiallyExecuted { items: 1 },
            Explanation::SharedHardlinks { bytes: 1 },
            Explanation::UnattributedLinks,
            Explanation::ClonesPossible {
                fs: VolumeFs::Btrfs,
            },
            Explanation::SnapshotsPresent,
            Explanation::SnapshotsPossible,
            Explanation::SnapshotsUnknown,
            Explanation::DelayedFree {
                fs: VolumeFs::Zfs,
                observed: true,
            },
            Explanation::DelayedFree {
                fs: VolumeFs::Zfs,
                observed: false,
            },
            Explanation::InsideVmImage,
            Explanation::EstimateIncomplete,
            Explanation::ConcurrentActivity { noise: 1 },
        ];
        let mut seen = std::collections::HashSet::new();
        for e in &all {
            assert!(!e.message().is_empty());
            let code = e.code();
            assert!(
                code.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{code}"
            );
            if !matches!(
                e,
                Explanation::DelayedFree {
                    observed: false,
                    ..
                }
            ) {
                assert!(seen.insert(code), "duplicate {code}");
            }
        }
    }

    #[test]
    fn sample_median_and_spread() {
        assert_eq!(Sample::from_samples(&[]), None);
        assert_eq!(
            Sample::from_samples(&[5, 1, 9]),
            Some(Sample { free: 5, spread: 8 })
        );
        assert_eq!(
            Sample::from_samples(&[7]),
            Some(Sample { free: 7, spread: 0 })
        );
    }

    #[test]
    fn tally_merges_notes() {
        let mut t = Tally::new();
        let r = delete(MIB, Confidence::High);
        t.add(
            &r,
            &[
                EstimateNote::SharedWithLinks { bytes: 5 },
                EstimateNote::SnapshotsUnknown,
            ],
            done(MIB),
        );
        t.add(
            &r,
            &[
                EstimateNote::SharedWithLinks { bytes: 7 },
                EstimateNote::SnapshotsUnknown,
            ],
            done(MIB),
        );
        assert_eq!(
            t.notes,
            [
                EstimateNote::SharedWithLinks { bytes: 12 },
                EstimateNote::SnapshotsUnknown
            ]
        );
        assert_eq!(t.predicted.freed_now, 2 * MIB);
    }
}
