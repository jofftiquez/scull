//! Read-only history for the Insights screen (P3-UI-10, design 05): free-space samples, per
//! category rollups, regrowth events and scull's own footprint, straight from the history
//! database (`scull-storage`). Nothing here creates, migrates or writes the database: a missing
//! one answers [`HistoryRead::NoDatabase`] with empty series.
//!
//! The screen derives everything else (forecast, cleanups, regrowth counts, mix shares) from
//! these series in the frontend, so the numbers it shows are always traceable to a sample.

use schemars::JsonSchema;
use scull_core::{Category, FsPath};
use scull_storage::{
    FootprintSample, RegrowthEvent, RetentionPolicy, RollupPoint, Store, VolumeSample,
};
use serde::Serialize;

use crate::views::VolumeSampleView;

/// Categories, in the order the Overview wheel and the Insights mix show them.
pub const CATEGORIES: [Category; 8] = [
    Category::ProjectArtifact,
    Category::PackageCache,
    Category::Worktree,
    Category::MlCache,
    Category::Container,
    Category::LargeFile,
    Category::Other,
    Category::ScullData,
];

/// Whether the history database answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRead {
    /// The database answered (the series may still be empty).
    Ok,
    /// No database yet (nothing was ever recorded, or history is off).
    NoDatabase,
    /// The database can't be read right now (`detail` says why).
    Unavailable,
}

/// `free_space_history`: one volume's free-space samples (INS-01), oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FreeSpaceHistory {
    /// Whether the database answered.
    pub state: HistoryRead,
    /// Why it didn't, when it didn't.
    pub detail: Option<String>,
    /// Start of the window (Unix seconds).
    pub since: i64,
    /// The volume the samples belong to (`None` when nothing was ever sampled).
    pub volume_id: Option<String>,
    /// Samples at or after `since`, oldest first.
    pub samples: Vec<VolumeSampleView>,
}

/// One point of a category series: the category's bytes in one scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RollupPointView {
    /// Scan finish time (Unix seconds).
    pub at: i64,
    /// The scan.
    pub scan_id: String,
    /// Candidates.
    pub items: u64,
    /// Logical bytes.
    pub logical_bytes: u64,
    /// Allocated (on-disk) bytes.
    pub allocated_bytes: u64,
    /// Reclaimable bytes.
    pub reclaimable_bytes: u64,
}

impl From<&RollupPoint> for RollupPointView {
    fn from(p: &RollupPoint) -> Self {
        Self {
            at: p.at,
            scan_id: p.scan_id.as_str().to_owned(),
            items: p.items,
            logical_bytes: p.logical_bytes,
            allocated_bytes: p.allocated_bytes,
            reclaimable_bytes: p.reclaimable_bytes,
        }
    }
}

/// One category's bytes per usable scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CategorySeries {
    /// The category.
    pub category: Category,
    /// One point per usable scan in the window (zero when the scan found none), oldest first.
    pub points: Vec<RollupPointView>,
}

/// `rollup_history`: bytes per category per usable scan (INS-02 growth, the mix chart).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RollupHistory {
    /// Whether the database answered.
    pub state: HistoryRead,
    /// Why it didn't, when it didn't.
    pub detail: Option<String>,
    /// Start of the window (Unix seconds).
    pub since: i64,
    /// Categories with at least one non-zero point, in [`CATEGORIES`] order.
    pub series: Vec<CategorySeries>,
}

/// An artifact that came back after it had gone (INS-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegrowthEventView {
    /// Its category.
    pub category: Category,
    /// Path (the same for the item that went and the one that came back).
    pub path: FsPath,
    /// Artifact type, e.g. "npm dependencies".
    pub artifact: String,
    /// Last sighting of the item that went (Unix seconds).
    pub previous_last_seen_at: i64,
    /// When it was marked gone.
    pub disappeared_at: i64,
    /// When it was seen again.
    pub reappeared_at: i64,
    /// The same object came back (e.g. restored), not a rebuild.
    pub same_item: bool,
}

impl RegrowthEventView {
    fn of(category: Category, e: &RegrowthEvent) -> Self {
        Self {
            category,
            path: e.path.clone(),
            artifact: e.artifact.clone(),
            previous_last_seen_at: e.previous_last_seen_at,
            disappeared_at: e.disappeared_at,
            reappeared_at: e.reappeared_at,
            same_item: e.same_item(),
        }
    }
}

/// `regrowth`: every regrowth event in the window, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegrowthHistory {
    /// Whether the database answered.
    pub state: HistoryRead,
    /// Why it didn't, when it didn't.
    pub detail: Option<String>,
    /// Start of the window (Unix seconds).
    pub since: i64,
    /// Events, oldest first.
    pub events: Vec<RegrowthEventView>,
}

/// scull's own disk use at one moment (DATA-05).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FootprintSampleView {
    /// When it was measured (Unix seconds).
    pub at: i64,
    /// The database file.
    pub database_bytes: u64,
    /// The `-wal` and `-shm` files.
    pub wal_bytes: u64,
    /// Migration backups.
    pub backup_bytes: u64,
    /// Damaged databases set aside.
    pub damaged_bytes: u64,
    /// Sum of the parts.
    pub total_bytes: u64,
}

impl From<&FootprintSample> for FootprintSampleView {
    fn from(s: &FootprintSample) -> Self {
        let f = &s.footprint;
        Self {
            at: s.at,
            database_bytes: f.database_bytes,
            wal_bytes: f.wal_bytes,
            backup_bytes: f.backup_bytes,
            damaged_bytes: f.damaged_bytes,
            total_bytes: f.total(),
        }
    }
}

/// `footprint_history`: scull's own footprint over time ("What scull knows").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FootprintHistory {
    /// Whether the database answered.
    pub state: HistoryRead,
    /// Why it didn't, when it didn't.
    pub detail: Option<String>,
    /// Start of the window (Unix seconds).
    pub since: i64,
    /// Samples at or after `since`, oldest first.
    pub samples: Vec<FootprintSampleView>,
    /// Raw free-space samples are kept this many days, then downsampled (INS-02).
    pub raw_retention_days: u64,
}

/// What a history read needs: the open store, or why there is none.
pub(crate) enum Source {
    Store(Store),
    Missing,
    Failed(String),
}

impl Source {
    pub(crate) fn state(&self) -> (HistoryRead, Option<String>) {
        match self {
            Source::Store(_) => (HistoryRead::Ok, None),
            Source::Missing => (HistoryRead::NoDatabase, None),
            Source::Failed(e) => (HistoryRead::Unavailable, Some(e.clone())),
        }
    }
}

fn failed(e: impl std::fmt::Display) -> (HistoryRead, Option<String>) {
    (HistoryRead::Unavailable, Some(e.to_string()))
}

/// The volume to show: `requested` if given; else the home volume (`home_volume`) if it was
/// ever sampled; else `/`; else the most recently sampled volume.
fn pick_volume(
    requested: Option<&str>,
    home_volume: Option<&str>,
    latest: &[VolumeSample],
) -> Option<String> {
    if let Some(v) = requested.filter(|v| !v.is_empty()) {
        return Some(v.to_owned());
    }
    let sampled = |id: &str| latest.iter().any(|s| s.volume_id == id);
    if let Some(h) = home_volume.filter(|h| sampled(h)) {
        return Some(h.to_owned());
    }
    if sampled("/") {
        return Some("/".to_owned());
    }
    latest
        .iter()
        .max_by_key(|s| s.at)
        .map(|s| s.volume_id.clone())
}

pub(crate) fn free_space_history(
    src: &Source,
    since: i64,
    requested: Option<&str>,
    home_volume: Option<&str>,
) -> FreeSpaceHistory {
    let (mut state, mut detail) = src.state();
    let mut volume_id = None;
    let mut samples = Vec::new();
    if let Source::Store(s) = src {
        match s.status_summary().and_then(|sum| {
            let id = pick_volume(requested, home_volume, &sum.volumes);
            let rows = match &id {
                Some(id) => s.free_space_history(id, since)?,
                None => Vec::new(),
            };
            Ok((id, rows))
        }) {
            Ok((id, rows)) => {
                volume_id = id;
                samples = rows.iter().map(VolumeSampleView::from).collect();
            }
            Err(e) => (state, detail) = failed(e),
        }
    }
    FreeSpaceHistory {
        state,
        detail,
        since,
        volume_id,
        samples,
    }
}

pub(crate) fn rollup_history(src: &Source, since: i64) -> RollupHistory {
    let (mut state, mut detail) = src.state();
    let mut series = Vec::new();
    if let Source::Store(s) = src {
        let all: Result<Vec<_>, _> = CATEGORIES
            .iter()
            .map(|&c| s.rollup_history(c, since).map(|p| (c, p)))
            .collect();
        match all {
            Ok(all) => {
                series = all
                    .into_iter()
                    .filter(|(_, p)| p.iter().any(|x| x.allocated_bytes > 0 || x.items > 0))
                    .map(|(category, p)| CategorySeries {
                        category,
                        points: p.iter().map(RollupPointView::from).collect(),
                    })
                    .collect();
            }
            Err(e) => (state, detail) = failed(e),
        }
    }
    RollupHistory {
        state,
        detail,
        since,
        series,
    }
}

pub(crate) fn regrowth(src: &Source, since: i64) -> RegrowthHistory {
    let (mut state, mut detail) = src.state();
    let mut events = Vec::new();
    if let Source::Store(s) = src {
        let all: Result<Vec<_>, _> = CATEGORIES
            .iter()
            .map(|&c| {
                s.regrowth(c, since).map(|es| {
                    es.iter()
                        .map(|e| RegrowthEventView::of(c, e))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        match all {
            Ok(all) => {
                events = all.into_iter().flatten().collect();
                events.sort_by(|a, b| {
                    a.reappeared_at
                        .cmp(&b.reappeared_at)
                        .then_with(|| a.path.as_str().cmp(b.path.as_str()))
                });
            }
            Err(e) => (state, detail) = failed(e),
        }
    }
    RegrowthHistory {
        state,
        detail,
        since,
        events,
    }
}

pub(crate) fn footprint_history(src: &Source, since: i64) -> FootprintHistory {
    let (mut state, mut detail) = src.state();
    let mut samples = Vec::new();
    if let Source::Store(s) = src {
        match s.footprint_history(since) {
            Ok(rows) => samples = rows.iter().map(FootprintSampleView::from).collect(),
            Err(e) => (state, detail) = failed(e),
        }
    }
    let secs = RetentionPolicy::default().raw_secs;
    FootprintHistory {
        state,
        detail,
        since,
        samples,
        raw_retention_days: u64::try_from(secs / 86_400).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, at: i64) -> VolumeSample {
        VolumeSample {
            volume_id: id.into(),
            total: 100,
            free: 10,
            at,
        }
    }

    #[test]
    fn picks_requested_then_home_then_root_then_latest() {
        let vols = [
            sample("/", 5),
            sample("/Volumes/Dev", 9),
            sample("/home", 1),
        ];
        assert_eq!(
            pick_volume(Some("/x"), Some("/home"), &vols).as_deref(),
            Some("/x")
        );
        assert_eq!(
            pick_volume(None, Some("/home"), &vols).as_deref(),
            Some("/home")
        );
        assert_eq!(
            pick_volume(None, Some("/nope"), &vols).as_deref(),
            Some("/")
        );
        assert_eq!(
            pick_volume(Some(""), None, &vols[1..]).as_deref(),
            Some("/Volumes/Dev")
        );
        assert_eq!(pick_volume(None, None, &[]), None);
    }

    #[test]
    fn missing_database_reads_empty() {
        let f = free_space_history(&Source::Missing, 7, None, None);
        assert_eq!(f.state, HistoryRead::NoDatabase);
        assert!(f.samples.is_empty() && f.volume_id.is_none());
        assert_eq!(rollup_history(&Source::Missing, 7).series, vec![]);
        assert_eq!(regrowth(&Source::Missing, 7).events, vec![]);
        let fp = footprint_history(&Source::Failed("locked".into()), 7);
        assert_eq!(fp.state, HistoryRead::Unavailable);
        assert_eq!(fp.detail.as_deref(), Some("locked"));
        assert_eq!(fp.raw_retention_days, 365);
    }
}
