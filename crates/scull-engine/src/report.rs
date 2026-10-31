//! Serializable results of the read-only pipeline. Every type here is part of the published
//! JSON output (`scull/scan@1` and friends), so fields only ever get added.

use schemars::JsonSchema;
use scull_core::config::ConfigWarning;
use scull_core::{Candidate, Category, FsPath, ScanId, Tier, TierBreakdown};
use scull_git::{RepoWorktrees, WorktreeDecision, WorktreeKind, WorktreeProblem, WorktreeState};
use serde::Serialize;

/// Everything one `scan` produced.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanReport {
    /// This scan's ID (recorded in history when history is on).
    pub scan_id: ScanId,
    /// Unix time (seconds) the scan started.
    pub started_at: i64,
    /// Unix time (seconds) the scan finished or was cancelled.
    pub finished_at: i64,
    /// The roots scanned (canonical form).
    pub roots: Vec<FsPath>,
    /// `true` when the user cancelled: the report is partial and nothing was marked gone.
    pub cancelled: bool,
    /// Every candidate, deduplicated by item ID, most reclaimable first. Linked worktrees and
    /// stale worktree registrations are included (category `worktree`).
    pub candidates: Vec<Candidate>,
    /// Projects found under the roots.
    pub projects: Vec<ProjectSummary>,
    /// Git worktrees found under the roots (every worktree of every repository seen).
    pub worktrees: Vec<WorktreeReport>,
    /// Worktree registrations whose metadata could not be trusted (treated as protected).
    pub worktree_problems: Vec<WorktreeProblemReport>,
    /// Totals over the candidates.
    pub totals: Totals,
    /// Entries counted by the scanner.
    pub entries_scanned: u64,
    /// Places that could not be read or were not descended.
    pub issues: Vec<Issue>,
    /// Issues beyond the ones listed in `issues`.
    pub issues_truncated: u64,
    /// Safety signals this run could not observe; every affected candidate was classified
    /// as if the answer were the risky one (PRD §8.4).
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Settings in repository files (`.scull.toml`) that were ignored, and why (CFG-03).
    pub config_warnings: Vec<RepoConfigWarning>,
    /// What happened with history recording.
    pub history: HistoryOutcome,
}

/// Byte totals over a set of candidates, split by tier (PHY-01: every size is labeled).
///
/// Linked worktrees are totalled separately (`worktrees`), since their bytes include the
/// artifacts inside them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Totals {
    /// Number of candidates per tier (artifacts and caches).
    pub count: TierBreakdown,
    /// Logical bytes per tier.
    pub logical: TierBreakdown,
    /// Allocated bytes per tier.
    pub allocated: TierBreakdown,
    /// Estimated reclaimable bytes per tier.
    pub reclaimable: TierBreakdown,
    /// Worktree candidates (counted apart from artifacts).
    pub worktrees: WorktreeTotals,
}

/// Totals of worktree candidates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeTotals {
    /// Number of worktree candidates per tier.
    pub count: TierBreakdown,
    /// Allocated bytes per tier.
    pub allocated: TierBreakdown,
    /// Estimated reclaimable bytes per tier.
    pub reclaimable: TierBreakdown,
}

impl Totals {
    /// Totals over `candidates`.
    pub fn of(candidates: &[Candidate]) -> Self {
        let mut t = Totals::default();
        for c in candidates {
            if c.category == Category::Worktree {
                t.worktrees.count.add(c.tier, 1);
                t.worktrees.allocated.add(c.tier, c.sizes.allocated);
                t.worktrees
                    .reclaimable
                    .add(c.tier, c.sizes.reclaimable.bytes);
            } else {
                t.count.add(c.tier, 1);
                t.logical.add(c.tier, c.sizes.logical);
                t.allocated.add(c.tier, c.sizes.allocated);
                t.reclaimable.add(c.tier, c.sizes.reclaimable.bytes);
            }
        }
        t
    }

    /// Reclaimable bytes in actionable tiers (S0–S2): what a cleanup could free at most.
    pub fn actionable_reclaimable(&self) -> u64 {
        Tier::ALL
            .into_iter()
            .filter(|t| t.is_actionable())
            .fold(0u64, |a, t| a.saturating_add(self.reclaimable.get(t)))
    }
}

/// A project: a directory with a project marker (`Cargo.toml`, `package.json`, ...) or the
/// owner of a detected artifact.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectSummary {
    /// Project directory.
    pub root: FsPath,
    /// Display name (the directory name).
    pub name: String,
    /// Ecosystems seen (`rust`, `node`, ...), sorted.
    pub ecosystems: Vec<String>,
    /// Marker files found in the directory.
    pub markers: Vec<String>,
    /// The Git worktree the project lives in, if any.
    pub git_worktree: Option<FsPath>,
    /// Candidates attributed to this project (by their first project).
    pub candidates: u64,
    /// Allocated bytes of those candidates per tier.
    pub allocated: TierBreakdown,
    /// Reclaimable bytes of those candidates per tier.
    pub reclaimable: TierBreakdown,
    /// Most protective tier among its candidates (parent badge, PRD §8.3); `null` without
    /// candidates.
    pub badge: Option<Tier>,
    /// Allocated size of the whole project directory, when measured.
    pub total_allocated: Option<u64>,
    /// Newest modification time seen in the project (Unix seconds), when measured.
    pub newest_mtime: Option<i64>,
}

/// One worktree with its GIT-02 state and GIT-03 decision.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeReport {
    /// The worktree folder (for a stale registration, the folder it was registered at).
    pub path: FsPath,
    /// Main, bare or linked.
    pub kind: WorktreeKind,
    /// The repository's common Git dir.
    pub common_dir: FsPath,
    /// Whether the folder lies inside a scan root. Worktrees elsewhere are reported but never
    /// offered as candidates.
    pub inside_roots: bool,
    /// Everything Git reported about it.
    pub state: WorktreeState,
    /// The removal decision.
    pub decision: WorktreeDecision,
    /// Allocated size of the folder, when measured.
    pub allocated: Option<u64>,
    /// Item ID of its candidate, when it is one.
    pub candidate: Option<scull_core::ItemId>,
}

/// A worktree registration that could not be trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeProblemReport {
    /// The repository's common Git dir.
    pub common_dir: FsPath,
    /// The problem.
    pub problem: WorktreeProblem,
}

impl WorktreeProblemReport {
    /// Every problem of `repo`.
    pub fn of(repo: &RepoWorktrees) -> Vec<Self> {
        repo.problems
            .iter()
            .map(|p| Self {
                common_dir: repo.common_dir.clone(),
                problem: p.clone(),
            })
            .collect()
    }
}

/// Something the scan could not read or did not descend into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    /// Where.
    pub path: FsPath,
    /// What kind of issue.
    pub kind: IssueKind,
    /// Human explanation.
    pub message: String,
}

/// Kinds of [`Issue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    /// Permission denied.
    PermissionDenied,
    /// Vanished while scanning.
    NotFound,
    /// Listing took too long.
    TimedOut,
    /// Another read error.
    ReadError,
    /// On another volume: counted, not descended.
    OtherVolume,
    /// A directory loop: not descended.
    Loop,
    /// Depth limit: not descended.
    MaxDepth,
    /// A cloud-sync placeholder (iCloud/File Provider dataless, OneDrive recall-on-access):
    /// never listed so nothing gets downloaded (PLT-04); counted from metadata only.
    CloudPlaceholder,
    /// Not descended for another scanner reason.
    NotDescended,
    /// A Git operation failed.
    Git,
    /// Something outside the scan roots that was not measured.
    OutsideRoots,
    /// History could not be recorded.
    History,
}

/// A safety signal the run could not observe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignalUnavailable {
    /// Stable signal name (`in_use`, `git`, ...).
    pub signal: String,
    /// Why, and what scull did instead.
    pub reason: String,
    /// How many candidates it affected.
    pub affected: u64,
}

/// A repository-file setting that was ignored (CFG-03).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoConfigWarning {
    /// The `.scull.toml` file.
    pub file: FsPath,
    /// The warning.
    pub warning: ConfigWarning,
}

/// Whether the scan was recorded in history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryOutcome {
    /// History is off for this run (`--no-history`, or no database).
    Disabled,
    /// Recorded.
    Recorded {
        /// Items seen for the first time.
        new_items: u64,
        /// Items marked gone by this scan (only after a completed scan).
        disappeared: u64,
    },
    /// Recording failed; the scan result is still valid.
    Failed {
        /// Why.
        message: String,
    },
}

/// Result of locating package-manager caches (`scull caches`).
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CachesReport {
    /// Unix time (seconds) of the measurement.
    pub measured_at: i64,
    /// `true` when the user cancelled while sizing.
    pub cancelled: bool,
    /// One candidate per cache found (category `package_cache`).
    pub candidates: Vec<Candidate>,
    /// Tools whose cache location was refused (QC-05); nothing of them is offered.
    pub refused: Vec<RefusedCacheReport>,
    /// Totals over the candidates.
    pub totals: Totals,
    /// Signals this run could not observe.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Places that could not be read.
    pub issues: Vec<Issue>,
}

/// A tool whose cache location was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefusedCacheReport {
    /// Rule ID of the tool (e.g. `cache.npm`).
    pub rule: String,
    /// The refused location, if known.
    pub path: Option<FsPath>,
    /// Where it came from.
    pub source: String,
    /// Why it was refused.
    pub reason: String,
}
