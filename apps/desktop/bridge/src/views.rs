//! Payloads the desktop commands return and the events they emit. Engine results
//! ([`ScanReport`], [`scull_engine::CachesReport`]) are passed through unchanged; the views
//! here use the same field names as the CLI's JSON output wherever the CLI has an equivalent
//! (`scull projects`, `scull worktrees`, `scull status`), and tests validate them against the
//! CLI's schemas.

use schemars::JsonSchema;
use scull_core::FsPath;
use scull_engine::{
    EngineProgress, Issue, ProjectSummary, ScanReport, SignalUnavailable, WorktreeProblemReport,
    WorktreeReport,
};
use serde::{Deserialize, Serialize};

/// Client-chosen ID of one long-running operation (scan, caches). Progress events carry it,
/// and `cancel_operation` takes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct OpId(
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[A-Za-z0-9_-]+$"))] String,
);

impl OpId {
    /// Validate an ID from the UI: 1–64 characters of `[A-Za-z0-9_-]`.
    pub fn parse(s: &str) -> Result<Self, scull_core::ScullError> {
        let ok = !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(scull_core::ScullError::new(
                scull_core::ErrorKind::Usage,
                "operation IDs are 1-64 characters of A-Z, a-z, 0-9, '-' and '_'",
            ))
        }
    }

    /// The ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `scan` parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ScanParams {
    /// Absolute folders to scan; empty uses `scan.roots` from the configuration.
    pub roots: Vec<String>,
    /// The user confirmed a broad root (a filesystem root, the home folder or a folder
    /// containing it; CFG-04).
    pub allow_broad_roots: bool,
    /// Record the scan in history (default: on).
    pub record_history: Option<bool>,
}

/// Which stage an operation is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Walking a root.
    Scanning,
    /// Post-walk analysis (Git, classification).
    Analyzing,
}

/// The `scull://progress` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProgressEvent {
    /// The operation this event belongs to.
    pub op_id: OpId,
    /// Stage.
    pub phase: Phase,
    /// `scanning`: the root being walked.
    pub root: Option<String>,
    /// `scanning`: entries counted so far in this root.
    pub entries: Option<u64>,
    /// `scanning`: allocated bytes counted so far in this root.
    pub allocated_bytes: Option<u64>,
    /// `scanning`: directory being listed.
    pub current: Option<String>,
    /// `analyzing`: what is being analysed.
    pub what: Option<String>,
}

impl ProgressEvent {
    /// Event name on the Tauri side.
    pub const NAME: &'static str = "scull://progress";

    /// Convert an engine progress report (`None` for kinds the UI doesn't show).
    pub fn of(op: &OpId, p: &EngineProgress) -> Option<Self> {
        let blank = Self {
            op_id: op.clone(),
            phase: Phase::Scanning,
            root: None,
            entries: None,
            allocated_bytes: None,
            current: None,
            what: None,
        };
        match p {
            EngineProgress::Scanning {
                root,
                entries,
                allocated,
                current,
            } => Some(Self {
                root: Some(root.display().to_string()),
                entries: Some(*entries),
                allocated_bytes: Some(*allocated),
                current: current.as_ref().map(|c| c.display().to_string()),
                ..blank
            }),
            EngineProgress::Analyzing { what } => Some(Self {
                phase: Phase::Analyzing,
                what: Some(what.clone()),
                ..blank
            }),
            _ => None,
        }
    }
}

/// `projects`: the projects of the last scan. Same shape as `scull projects --json` data.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectsView {
    /// The roots scanned.
    pub roots: Vec<FsPath>,
    /// Whether the user cancelled (partial).
    pub cancelled: bool,
    /// Projects, by path.
    pub projects: Vec<ProjectSummary>,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Places that could not be read.
    pub issues: Vec<Issue>,
}

impl ProjectsView {
    /// From a scan report (empty before the first scan).
    pub fn of(r: Option<&ScanReport>) -> Self {
        match r {
            Some(r) => Self {
                roots: r.roots.clone(),
                cancelled: r.cancelled,
                projects: r.projects.clone(),
                signals_unavailable: r.signals_unavailable.clone(),
                issues: r.issues.clone(),
            },
            None => Self {
                roots: Vec::new(),
                cancelled: false,
                projects: Vec::new(),
                signals_unavailable: Vec::new(),
                issues: Vec::new(),
            },
        }
    }
}

/// `worktrees`: the Git worktrees of the last scan. Same shape as `scull worktrees --json`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreesView {
    /// The roots scanned.
    pub roots: Vec<FsPath>,
    /// Whether the user cancelled (partial; worktrees are analysed after the walk).
    pub cancelled: bool,
    /// Every worktree of every repository found.
    pub worktrees: Vec<WorktreeReport>,
    /// Registrations whose metadata could not be trusted (protected).
    pub problems: Vec<WorktreeProblemReport>,
    /// Signals that could not be observed (e.g. `git` missing).
    pub signals_unavailable: Vec<SignalUnavailable>,
}

impl WorktreesView {
    /// From a scan report (empty before the first scan).
    pub fn of(r: Option<&ScanReport>) -> Self {
        match r {
            Some(r) => Self {
                roots: r.roots.clone(),
                cancelled: r.cancelled,
                worktrees: r.worktrees.clone(),
                problems: r.worktree_problems.clone(),
                signals_unavailable: r.signals_unavailable.clone(),
            },
            None => Self {
                roots: Vec::new(),
                cancelled: false,
                worktrees: Vec::new(),
                problems: Vec::new(),
                signals_unavailable: Vec::new(),
            },
        }
    }
}

/// Whether there is a scan to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    /// A usable scan is recorded.
    Ok,
    /// The database exists but holds no completed or partial scan yet.
    NoScan,
    /// No database yet (nothing was ever recorded).
    NoDatabase,
    /// The database can't be read right now.
    Unavailable,
}

/// The latest usable scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LastScan {
    /// Scan identifier.
    pub scan_id: String,
    /// Start time (Unix seconds).
    pub started_at: i64,
    /// Finish time (Unix seconds).
    pub finished_at: i64,
    /// `completed`, `partial` or `cancelled`.
    pub status: String,
    /// Seconds between the finish and `now` (0 if the clock went backwards).
    pub age_secs: u64,
    /// Number of roots scanned, when known (scans of this session).
    pub roots: Option<u64>,
    /// Candidates found.
    pub candidates: u64,
    /// Reclaimable bytes of all candidates.
    pub reclaimable_bytes: u64,
    /// Reclaimable bytes of S0 (regenerable) candidates.
    pub safe_reclaimable_bytes: u64,
}

fn age(now: i64, finished: i64) -> u64 {
    u64::try_from(now.saturating_sub(finished)).unwrap_or(0)
}

impl LastScan {
    /// From a history record.
    pub fn of_history(l: &scull_storage::LastScan, now: i64) -> Self {
        use scull_storage::ScanStatus;
        Self {
            scan_id: l.scan_id.as_str().to_owned(),
            started_at: l.started_at,
            finished_at: l.finished_at,
            status: match l.status {
                ScanStatus::Completed => "completed",
                ScanStatus::Partial => "partial",
                ScanStatus::Failed => "failed",
                ScanStatus::Cancelled => "cancelled",
                _ => "unknown",
            }
            .to_owned(),
            age_secs: age(now, l.finished_at),
            roots: None,
            candidates: l.candidates,
            reclaimable_bytes: l.reclaimable_bytes,
            safe_reclaimable_bytes: l.safe_reclaimable_bytes,
        }
    }

    /// From a scan of this session.
    pub fn of_report(r: &ScanReport, now: i64) -> Self {
        let partial = !r.issues.is_empty() || r.issues_truncated > 0;
        Self {
            scan_id: r.scan_id.as_str().to_owned(),
            started_at: r.started_at,
            finished_at: r.finished_at,
            status: if r.cancelled {
                "cancelled"
            } else if partial {
                "partial"
            } else {
                "completed"
            }
            .to_owned(),
            age_secs: age(now, r.finished_at),
            roots: Some(r.roots.len() as u64),
            candidates: r.candidates.len() as u64,
            reclaimable_bytes: r.totals.reclaimable.total(),
            safe_reclaimable_bytes: r.totals.reclaimable.get(scull_core::Tier::S0),
        }
    }
}

/// One volume's latest free-space sample from history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeSampleView {
    /// Volume identifier (mount point or volume GUID path).
    pub volume_id: String,
    /// Volume size in bytes.
    pub total_bytes: u64,
    /// Bytes available to the user.
    pub free_bytes: u64,
    /// When it was sampled (Unix seconds).
    pub sampled_at: i64,
}

impl From<&scull_storage::VolumeSample> for VolumeSampleView {
    fn from(v: &scull_storage::VolumeSample) -> Self {
        Self {
            volume_id: v.volume_id.clone(),
            total_bytes: v.total,
            free_bytes: v.free,
            sampled_at: v.at,
        }
    }
}

/// `status`: the header's freshness line and the nav figures come from here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopStatus {
    /// Whether there is something to show.
    pub state: StatusState,
    /// Why the status is unavailable, when it is.
    pub detail: Option<String>,
    /// When this report was produced (Unix seconds).
    pub now: i64,
    /// The latest usable scan (history, or this session's scan when history is off).
    pub last_scan: Option<LastScan>,
    /// Latest free-space sample of every volume ever sampled.
    pub volumes: Vec<VolumeSampleView>,
}

impl DesktopStatus {
    /// A status without volume samples.
    pub fn new(
        now: i64,
        state: StatusState,
        detail: Option<String>,
        last_scan: Option<LastScan>,
    ) -> Self {
        Self {
            state,
            detail,
            now,
            last_scan,
            volumes: Vec::new(),
        }
    }
}

/// Whether `git` can be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitInfo {
    /// A trusted `git` was found.
    pub available: bool,
    /// Its version.
    pub version: Option<String>,
    /// Why it isn't available.
    pub detail: Option<String>,
}

/// State of the history database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryState {
    /// The database exists.
    Present,
    /// Not created yet (created by the first recorded scan).
    NotCreated,
    /// History is off for this session.
    Off,
}

/// Whether the process runs with root/administrator rights (SAFE-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElevationView {
    /// Root or an elevated Windows token.
    Elevated,
    /// An ordinary user.
    NotElevated,
    /// Unknown (treated as elevated).
    Unknown,
}

impl From<scull_platform::Elevation> for ElevationView {
    fn from(e: scull_platform::Elevation) -> Self {
        match e {
            scull_platform::Elevation::NotElevated => ElevationView::NotElevated,
            scull_platform::Elevation::Elevated => ElevationView::Elevated,
            _ => ElevationView::Unknown,
        }
    }
}

/// One safety signal and whether this build can observe it (PRD §8.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalView {
    /// Stable name.
    pub name: String,
    /// Observable here.
    pub available: bool,
}

/// `capabilities`: what this machine and build can observe (doctor-like).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesReport {
    /// scull version.
    pub version: String,
    /// Operating system.
    pub os: crate::Os,
    /// Root/administrator.
    pub elevation: ElevationView,
    /// Filesystem facts the platform layer provides.
    pub signals: Vec<SignalView>,
    /// `git`.
    pub git: GitInfo,
    /// Native in-use probes are on (otherwise every candidate is "in use: unknown").
    pub in_use_probes: bool,
    /// History database.
    pub history: HistoryState,
    /// The config file read, if any.
    pub config_file: Option<String>,
    /// scull's data directory.
    pub data_dir: Option<String>,
    /// Always `true`: the desktop has no deletion path (SAFE-10). Cleanups go through the
    /// Executor with a reviewed plan.
    pub read_only: bool,
}

impl CapabilitiesReport {
    /// Assemble the report.
    pub fn new(
        caps: &scull_platform::Capabilities,
        elevation: scull_platform::Elevation,
        git: GitInfo,
        in_use_probes: bool,
        history: HistoryState,
        config_file: Option<String>,
        data_dir: Option<String>,
    ) -> Self {
        let s = |name: &str, available: bool| SignalView {
            name: name.to_owned(),
            available,
        };
        Self {
            version: crate::VERSION.to_owned(),
            os: crate::Os::current(),
            elevation: elevation.into(),
            signals: vec![
                s("file_identity", caps.file_identity),
                s("allocated_size", caps.allocated_size),
                s("link_count", caps.link_count),
                s("mtime", caps.mtime),
                s("no_follow_metadata", caps.no_follow_metadata),
                s("special_file_kinds", caps.special_file_kinds),
                s("reparse_points", caps.reparse_points),
                s("race_free_read_dir", caps.race_free_read_dir),
                s("elevation_detection", caps.elevation_detection),
            ],
            git,
            in_use_probes,
            history,
            config_file,
            data_dir,
            read_only: true,
        }
    }
}
