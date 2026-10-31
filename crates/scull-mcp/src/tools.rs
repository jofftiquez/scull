//! The read-only tools (PRD §13.2): definitions (JSON input and output schemas) and handlers.
//!
//! Handlers are synchronous and run on a blocking thread. Each one takes structured arguments
//! (unknown fields rejected), checks every path against the [`Allowlist`] first, calls the
//! read-only engine, and returns a bounded result: lists are capped at `max_results` (the
//! smaller of the argument and `mcp.max_results`) and say whether they were truncated.
//!
//! There are no plan or execute tools in this version. Nothing here writes to disk: scans run
//! without a history store, and the history database is opened read-only.

use crate::allowlist::{Allowlist, Refusal};
use crate::history;
use crate::redact::Redactor;
use crate::views::{CandidateView, IssueView, ProjectView, SignalView, TotalsView, WorktreeView};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use schemars::JsonSchema;
use scull_core::{Candidate, Category, ItemId, ScullError, Tier};
use scull_detectors::{CacheEnv, LargeFileOptions, inspect_large_files};
use scull_engine::paths::is_within;
use scull_engine::{CancelToken, Engine, ScanReport, ScanRequest, now_unix};
use scull_scanner::ScanOptions;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Every tool this server exposes, in listing order. All are read-only; there are no plan or
/// execute tools in this version (PRD §13.2).
pub const TOOL_NAMES: [&str; 11] = [
    "get_storage_overview",
    "scan_directory",
    "list_projects",
    "inspect_project",
    "list_worktrees",
    "inspect_worktree",
    "list_caches",
    "find_large_files",
    "get_cleanup_candidates",
    "get_cleanup_history",
    "get_status",
];

/// How long a scan of the same roots is reused by later calls.
pub const SCAN_CACHE_TTL: Duration = Duration::from_secs(60);

/// Default `min_size_bytes` of `find_large_files` (500 MiB, like `scull large-files`).
pub const DEFAULT_LARGE_FILE_BYTES: u64 = 500 * 1024 * 1024;

/// Default `since_days` of `get_cleanup_history`.
pub const DEFAULT_HISTORY_DAYS: u32 = 30;

/// A tool failure the client should see (`isError: true`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ToolError {
    /// Stable machine code (`outside_allowlist`, `invalid_arguments`, ...).
    pub code: String,
    /// Human explanation.
    pub message: String,
}

impl ToolError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    fn from_scull(e: &ScullError, r: &Redactor) -> Self {
        Self::new(
            &crate::views::json_name(&e.kind),
            r.text(&match &e.path {
                Some(p) => format!("{} ({})", e.message, p.as_str()),
                None => e.message.clone(),
            }),
        )
    }
}

impl From<Refusal> for ToolError {
    fn from(r: Refusal) -> Self {
        Self::new(r.code(), r.message())
    }
}

/// A successful tool result: structured content plus a one-paragraph human summary.
#[derive(Debug, Clone)]
pub struct Output {
    /// The `structuredContent` object.
    pub structured: Value,
    /// The `text` content.
    pub summary: String,
}

fn output<T: Serialize>(data: &T, summary: String) -> Result<Output, ToolError> {
    let structured = serde_json::to_value(data)
        .map_err(|e| ToolError::new("internal", format!("serialization failed: {e}")))?;
    Ok(Output {
        structured,
        summary,
    })
}

/// Everything the handlers share.
pub struct Ctx {
    /// The read-only pipeline (no history store attached).
    pub engine: Engine,
    /// The configured roots (narrowed per call to the client's roots).
    pub allowlist: Allowlist,
    /// Output redaction.
    pub redactor: Redactor,
    /// `mcp.max_results` (at least 1).
    pub max_results: u32,
    /// Scanner threads (0 = automatic).
    pub threads: usize,
    /// scull's data directory (history database, read-only).
    pub data_dir: Option<PathBuf>,
    /// Where package-manager caches are looked for (`None`: `list_caches` is unavailable).
    pub cache_env: Option<CacheEnv>,
    cache: Mutex<Option<CachedScan>>,
}

struct CachedScan {
    roots: Vec<PathBuf>,
    at: Instant,
    report: Arc<ScanReport>,
}

impl Ctx {
    /// Shared state for the handlers.
    pub fn new(
        engine: Engine,
        allowlist: Allowlist,
        redactor: Redactor,
        max_results: u32,
        threads: usize,
        data_dir: Option<PathBuf>,
        cache_env: Option<CacheEnv>,
    ) -> Self {
        Self {
            engine,
            allowlist,
            redactor,
            max_results: max_results.max(1),
            threads,
            data_dir,
            cache_env,
            cache: Mutex::new(None),
        }
    }

    fn cap(&self, asked: Option<u32>) -> usize {
        asked.unwrap_or(self.max_results).clamp(1, self.max_results) as usize
    }

    /// Scan `roots` (canonical, allowlisted), reusing a recent scan of the same roots.
    fn scan(
        &self,
        roots: Vec<PathBuf>,
        cancel: &CancelToken,
    ) -> Result<Arc<ScanReport>, ToolError> {
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = cache.as_ref()
                && c.roots == roots
                && c.at.elapsed() < SCAN_CACHE_TTL
            {
                return Ok(Arc::clone(&c.report));
            }
        }
        let mut req = ScanRequest::full().history(false);
        req.threads = self.threads;
        let report = self
            .engine
            .scan(&roots, &req, cancel, |_| {})
            .map_err(|e| ToolError::from_scull(&e, &self.redactor))?;
        let report = Arc::new(report);
        if !report.cancelled {
            *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedScan {
                roots,
                at: Instant::now(),
                report: Arc::clone(&report),
            });
        }
        Ok(report)
    }

    /// The roots to scan: the checked `path`, or every allowlisted root.
    fn targets(&self, allow: &Allowlist, path: Option<&str>) -> Result<Vec<PathBuf>, ToolError> {
        match path {
            Some(p) => Ok(vec![allow.check(p)?]),
            None if allow.is_empty() => Err(Refusal::NoRoots.into()),
            None => Ok(allow.canonical_roots()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------------------------

/// No arguments.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

/// A directory inside the allowlisted roots.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathArgs {
    /// Absolute path (or `~/...`) inside an allowlisted root.
    pub path: String,
}

/// A scan of one directory.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanArgs {
    /// Absolute path (or `~/...`) inside an allowlisted root.
    pub path: String,
    /// Most candidates to return (capped by the server's `mcp.max_results`).
    pub max_results: Option<u32>,
}

/// A list over one directory or every allowlisted root.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    /// Absolute path (or `~/...`) inside an allowlisted root; omit for every root.
    pub path: Option<String>,
    /// Most entries to return (capped by the server's `mcp.max_results`).
    pub max_results: Option<u32>,
}

/// Package-manager caches.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CachesArgs {
    /// Most caches to return (capped by the server's `mcp.max_results`).
    pub max_results: Option<u32>,
}

/// Large files under a directory.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LargeFilesArgs {
    /// Absolute path (or `~/...`) inside an allowlisted root.
    pub path: String,
    /// Smallest allocated size listed, in bytes (default 500 MiB).
    pub min_size_bytes: Option<u64>,
    /// Most files to return (capped by the server's `mcp.max_results`).
    pub max_results: Option<u32>,
}

/// Cleanup candidates, filtered.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandidatesArgs {
    /// Absolute path (or `~/...`) inside an allowlisted root; omit for every root.
    pub path: Option<String>,
    /// Only these tiers (default: S0, S1 and S2; S3 items are protected and never cleaned).
    pub tiers: Option<Vec<Tier>>,
    /// Only this category.
    pub category: Option<Category>,
    /// Most candidates to return (capped by the server's `mcp.max_results`).
    pub max_results: Option<u32>,
}

/// History query.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryArgs {
    /// Only this category (default: every category with data).
    pub category: Option<Category>,
    /// How far back, in days (default 30, at most 3650).
    pub since_days: Option<u32>,
    /// Also return the lifecycle of this item (an `id` from another tool).
    pub item_id: Option<String>,
    /// Most points or events per list (capped by the server's `mcp.max_results`).
    pub max_results: Option<u32>,
}

fn parse<T: DeserializeOwned>(args: JsonObject) -> Result<T, ToolError> {
    serde_json::from_value(Value::Object(args))
        .map_err(|e| ToolError::new("invalid_arguments", e.to_string()))
}

// ---------------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------------

/// Bytes and items of one category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CategoryTotal {
    /// The category.
    pub category: Category,
    /// Candidates.
    pub count: u64,
    /// Allocated bytes.
    pub allocated_bytes: u64,
    /// Reclaimable bytes.
    pub reclaimable_bytes: u64,
}

/// A volume's capacity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct VolumeView {
    /// Mount point.
    pub mount_point: String,
    /// Size in bytes, when known.
    pub total_bytes: Option<u64>,
    /// Bytes available to the user, when known.
    pub free_bytes: Option<u64>,
}

/// One allowlisted root in the overview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RootOverview {
    /// The root.
    pub path: String,
    /// The volume it is on.
    pub volume: Option<VolumeView>,
    /// Candidate totals inside it.
    pub totals: TotalsView,
    /// Projects inside it.
    pub projects: u64,
}

/// `get_storage_overview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OverviewOutput {
    /// Per allowlisted root.
    pub roots: Vec<RootOverview>,
    /// Candidate totals over every root.
    pub totals: TotalsView,
    /// Totals by category, largest reclaimable first.
    pub by_category: Vec<CategoryTotal>,
    /// The largest projects by reclaimable bytes (at most 5).
    pub largest_projects: Vec<ProjectView>,
    /// Projects found.
    pub projects: u64,
    /// Git worktrees found.
    pub worktrees: u64,
    /// Filesystem entries visited.
    pub entries_scanned: u64,
    /// Places that could not be read.
    pub issues: u64,
    /// Whether the scan was cancelled (partial numbers).
    pub cancelled: bool,
}

/// `scan_directory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ScanOutput {
    /// The scanned directory.
    pub path: String,
    /// Whether the scan was cancelled (partial results).
    pub cancelled: bool,
    /// Filesystem entries visited.
    pub entries_scanned: u64,
    /// Candidate totals.
    pub totals: TotalsView,
    /// Projects found.
    pub projects: u64,
    /// Git worktrees found.
    pub worktrees: u64,
    /// Candidates, largest reclaimable first.
    pub candidates: Vec<CandidateView>,
    /// Candidates found (may exceed the list).
    pub candidates_total: u64,
    /// Whether `candidates` was cut at the cap.
    pub truncated: bool,
    /// Places that could not be read (capped like `candidates`).
    pub issues: Vec<IssueView>,
    /// Issues found (may exceed the list).
    pub issues_total: u64,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalView>,
}

/// `list_projects`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProjectsOutput {
    /// Projects, largest reclaimable first.
    pub projects: Vec<ProjectView>,
    /// Projects found (may exceed the list).
    pub total: u64,
    /// Whether the list was cut at the cap.
    pub truncated: bool,
    /// Whether the scan was cancelled.
    pub cancelled: bool,
}

/// `inspect_project`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProjectOutput {
    /// The inspected directory.
    pub path: String,
    /// The project rooted there, if it is one.
    pub project: Option<ProjectView>,
    /// The Git worktree it is in, if any.
    pub worktree: Option<WorktreeView>,
    /// Candidates inside, largest reclaimable first.
    pub candidates: Vec<CandidateView>,
    /// Candidates found (may exceed the list).
    pub candidates_total: u64,
    /// Whether `candidates` was cut at the cap.
    pub truncated: bool,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalView>,
    /// Whether the scan was cancelled.
    pub cancelled: bool,
}

/// A worktree registration that could not be trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct WorktreeProblemView {
    /// The repository's common Git directory.
    pub common_dir: String,
    /// Problem category.
    pub kind: scull_git::ProblemKind,
    /// Details.
    pub detail: String,
}

/// `list_worktrees`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct WorktreesOutput {
    /// Worktrees.
    pub worktrees: Vec<WorktreeView>,
    /// Worktrees found (may exceed the list).
    pub total: u64,
    /// Whether the list was cut at the cap.
    pub truncated: bool,
    /// Registrations that could not be trusted (capped).
    pub problems: Vec<WorktreeProblemView>,
    /// Whether the scan was cancelled.
    pub cancelled: bool,
}

/// `inspect_worktree`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct WorktreeOutput {
    /// The worktree.
    pub worktree: WorktreeView,
}

/// A cache location that was refused (e.g. configured inside a project).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RefusedCacheView {
    /// Cache rule.
    pub rule: String,
    /// The refused location, if known.
    pub path: Option<String>,
    /// Where the location came from.
    pub source: String,
    /// Why it was refused.
    pub reason: String,
}

/// `list_caches`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CachesOutput {
    /// Package-manager caches found (well-known locations, not caller-chosen paths), largest
    /// reclaimable first.
    pub caches: Vec<CandidateView>,
    /// Caches found (may exceed the list).
    pub total: u64,
    /// Whether the list was cut at the cap.
    pub truncated: bool,
    /// Locations refused.
    pub refused: Vec<RefusedCacheView>,
    /// Totals by tier.
    pub totals: TotalsView,
    /// Whether the measurement was cancelled.
    pub cancelled: bool,
}

/// `find_large_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct LargeFilesOutput {
    /// The inspected directory.
    pub path: String,
    /// Smallest allocated size listed, bytes.
    pub min_size_bytes: u64,
    /// Files, largest first (inspect-only: S2, no action). Names only, never contents.
    pub files: Vec<CandidateView>,
    /// Files at or above the threshold (may exceed the list).
    pub matched: u64,
    /// Allocated bytes of every matched file.
    pub matched_bytes: u64,
    /// Whether the list was cut at the cap.
    pub truncated: bool,
    /// Whether every folder could be read.
    pub complete: bool,
}

/// `get_cleanup_candidates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CandidatesOutput {
    /// Candidates matching the filter, largest reclaimable first.
    pub candidates: Vec<CandidateView>,
    /// Matching candidates (may exceed the list).
    pub total: u64,
    /// Whether the list was cut at the cap.
    pub truncated: bool,
    /// Reclaimable bytes of every matching candidate.
    pub reclaimable_bytes: u64,
    /// Always true: this server never prepares or executes a cleanup. To clean, the user runs
    /// `scull clean` in their own terminal and reviews the plan there.
    pub read_only: bool,
    /// Whether the scan was cancelled.
    pub cancelled: bool,
}

/// One point of a category's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RollupPointView {
    /// Scan finish time (Unix seconds).
    pub at: i64,
    /// Scan ID.
    pub scan_id: String,
    /// Candidates.
    pub items: u64,
    /// Allocated bytes.
    pub allocated_bytes: u64,
    /// Reclaimable bytes.
    pub reclaimable_bytes: u64,
}

/// A category's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CategoryHistory {
    /// The category.
    pub category: Category,
    /// Points, oldest first (the most recent `max_results` when truncated).
    pub points: Vec<RollupPointView>,
    /// Whether older points were left out.
    pub truncated: bool,
}

/// An artifact that came back after it had gone (cleaned, then rebuilt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RegrowthView {
    /// Where.
    pub path: String,
    /// What.
    pub artifact: String,
    /// The item seen now.
    pub item_id: String,
    /// When the previous item was marked gone.
    pub disappeared_at: i64,
    /// When it was seen again.
    pub reappeared_at: i64,
}

/// One lifecycle event of an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ItemEventView {
    /// When (scan time).
    pub at: i64,
    /// `first_seen`, `changed`, `disappeared` or `reappeared`.
    pub kind: String,
    /// Tier at that time.
    pub tier: Tier,
    /// Allocated bytes at that time.
    pub allocated_bytes: u64,
    /// Reclaimable bytes at that time.
    pub reclaimable_bytes: u64,
}

/// An item's lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ItemHistoryView {
    /// The item.
    pub item_id: String,
    /// Where.
    pub path: String,
    /// Category.
    pub category: Category,
    /// Last seen tier.
    pub tier: Tier,
    /// First sighting.
    pub first_seen_at: i64,
    /// Last sighting.
    pub last_seen_at: i64,
    /// When it was marked gone, if gone.
    pub disappeared_at: Option<i64>,
    /// Events, oldest first (the most recent `max_results`).
    pub events: Vec<ItemEventView>,
}

/// `get_cleanup_history`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct HistoryOutput {
    /// `ok`, `no_database` or `unavailable`.
    pub state: String,
    /// Why history is unavailable, when it is.
    pub detail: Option<String>,
    /// Start of the window (Unix seconds).
    pub since: i64,
    /// Per category with data.
    pub categories: Vec<CategoryHistory>,
    /// Artifacts that came back inside the allowlisted roots (most recent first).
    pub regrowth: Vec<RegrowthView>,
    /// Whether `regrowth` was cut at the cap.
    pub regrowth_truncated: bool,
    /// The requested item, if found inside the allowlisted roots.
    pub item: Option<ItemHistoryView>,
}

/// Server facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ServerFacts {
    /// `scull`.
    pub name: String,
    /// scull version.
    pub version: String,
    /// Always true in this version: no tool can change anything on disk.
    pub read_only: bool,
    /// The tools offered.
    pub tools: Vec<String>,
    /// Allowlisted roots: every path argument must be inside one of these.
    pub allowlisted_roots: Vec<String>,
    /// Whether the home folder is shown as `~`.
    pub redact_home: bool,
    /// Whether project and folder names are hashed (`h-<hex>`).
    pub hash_names: bool,
    /// Most results per list.
    pub max_results: u32,
}

/// The latest recorded scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct LastScanView {
    /// Scan ID.
    pub scan_id: String,
    /// Finish time (Unix seconds).
    pub finished_at: i64,
    /// `completed` or `partial`.
    pub status: String,
    /// Seconds since it finished.
    pub age_secs: u64,
    /// Candidates found.
    pub candidates: u64,
    /// Reclaimable bytes of all candidates.
    pub reclaimable_bytes: u64,
    /// Reclaimable bytes of S0 candidates.
    pub safe_reclaimable_bytes: u64,
}

/// A volume's latest free-space sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct VolumeSampleView {
    /// Volume (mount point).
    pub volume: String,
    /// Size, bytes.
    pub total_bytes: u64,
    /// Free, bytes.
    pub free_bytes: u64,
    /// When sampled (Unix seconds).
    pub sampled_at: i64,
}

/// `get_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct StatusOutput {
    /// This server.
    pub server: ServerFacts,
    /// `ok`, `no_scan`, `no_database` or `unavailable` (history database).
    pub state: String,
    /// Why history is unavailable, when it is.
    pub detail: Option<String>,
    /// The latest recorded scan (from `scull scan` runs; MCP calls record nothing).
    pub last_scan: Option<LastScanView>,
    /// Latest free-space sample of every volume.
    pub volumes: Vec<VolumeSampleView>,
}

// ---------------------------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------------------------

fn tool<A: JsonSchema + 'static, O: JsonSchema + 'static>(
    name: &'static str,
    title: &'static str,
    description: &'static str,
) -> Tool {
    Tool::new(name, description, JsonObject::new())
        .with_input_schema::<A>()
        .with_output_schema::<O>()
        .with_title(title)
        .with_annotations(
            ToolAnnotations::with_title(title)
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        )
}

/// The tool definitions, in [`TOOL_NAMES`] order.
pub fn definitions() -> Vec<Tool> {
    vec![
        tool::<NoArgs, OverviewOutput>(
            "get_storage_overview",
            "Storage overview",
            "Scan every allowlisted root and summarize developer storage: totals by safety tier \
             (S0 regenerable, S1 costly, S2 uncertain, S3 protected), by category, volume \
             capacity and the largest projects. Read-only.",
        ),
        tool::<ScanArgs, ScanOutput>(
            "scan_directory",
            "Scan a directory",
            "Scan one directory inside an allowlisted root and list what was found (build \
             outputs, dependency folders, worktrees), each with a safety tier and the reasons. \
             Read-only.",
        ),
        tool::<ListArgs, ProjectsOutput>(
            "list_projects",
            "List projects",
            "List the projects found in a directory (or every allowlisted root) with their \
             reclaimable bytes by tier. Read-only.",
        ),
        tool::<PathArgs, ProjectOutput>(
            "inspect_project",
            "Inspect a project",
            "Show one project: its artifacts with tiers and reasons, and its Git worktree \
             state. Read-only.",
        ),
        tool::<ListArgs, WorktreesOutput>(
            "list_worktrees",
            "List Git worktrees",
            "List Git worktrees in a directory (or every allowlisted root): branch, upstream, \
             uncommitted and unpushed counts, and the safety tier of removing each. Commit \
             messages and file lists are never returned. Read-only.",
        ),
        tool::<PathArgs, WorktreeOutput>(
            "inspect_worktree",
            "Inspect a Git worktree",
            "Analyse one Git worktree folder inside an allowlisted root. Read-only.",
        ),
        tool::<CachesArgs, CachesOutput>(
            "list_caches",
            "List package-manager caches",
            "Locate and measure package-manager caches (npm, pnpm, Yarn, Cargo, pip, ...) in \
             their well-known locations. Read-only.",
        ),
        tool::<LargeFilesArgs, LargeFilesOutput>(
            "find_large_files",
            "Find large files",
            "List the largest regular files under a directory inside an allowlisted root \
             (names and sizes only, never contents). Read-only.",
        ),
        tool::<CandidatesArgs, CandidatesOutput>(
            "get_cleanup_candidates",
            "Get cleanup candidates",
            "List items that could be cleaned, filtered by tier and category, largest first. \
             This never prepares or runs a cleanup: the user runs `scull clean` in their own \
             terminal to review and approve one. Read-only.",
        ),
        tool::<HistoryArgs, HistoryOutput>(
            "get_cleanup_history",
            "Get storage history",
            "Storage history recorded by scull scans: bytes per category over time, artifacts \
             that came back after being cleaned, and one item's lifecycle. Read-only.",
        ),
        tool::<NoArgs, StatusOutput>(
            "get_status",
            "Get status",
            "This server's allowlisted roots and settings, plus the latest recorded scan and \
             free space. Call this first to learn which paths are allowed. Read-only.",
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Dispatch and handlers
// ---------------------------------------------------------------------------------------------

/// Run tool `name`. `None` if there is no such tool.
pub fn call(
    ctx: &Ctx,
    allow: &Allowlist,
    name: &str,
    args: JsonObject,
    cancel: &CancelToken,
) -> Option<Result<Output, ToolError>> {
    let r = match name {
        "get_storage_overview" => parse::<NoArgs>(args).and_then(|_| overview(ctx, allow, cancel)),
        "scan_directory" => parse(args).and_then(|a| scan_directory(ctx, allow, a, cancel)),
        "list_projects" => parse(args).and_then(|a| list_projects(ctx, allow, a, cancel)),
        "inspect_project" => parse(args).and_then(|a| inspect_project(ctx, allow, a, cancel)),
        "list_worktrees" => parse(args).and_then(|a| list_worktrees(ctx, allow, a, cancel)),
        "inspect_worktree" => parse(args).and_then(|a| inspect_worktree(ctx, allow, a)),
        "list_caches" => parse(args).and_then(|a| list_caches(ctx, a, cancel)),
        "find_large_files" => parse(args).and_then(|a| find_large_files(ctx, allow, a, cancel)),
        "get_cleanup_candidates" => parse(args).and_then(|a| candidates(ctx, allow, a, cancel)),
        "get_cleanup_history" => parse(args).and_then(|a| cleanup_history(ctx, allow, a)),
        "get_status" => parse::<NoArgs>(args).and_then(|_| status(ctx, allow)),
        _ => return None,
    };
    Some(r)
}

fn by_reclaimable(a: &&Candidate, b: &&Candidate) -> std::cmp::Ordering {
    b.sizes
        .reclaimable
        .bytes
        .cmp(&a.sizes.reclaimable.bytes)
        .then_with(|| a.path.cmp(&b.path))
}

/// The first `cap` items of `items` as views, plus (total, truncated).
fn capped<'a, I, T, F>(items: I, cap: usize, f: F) -> (Vec<T>, u64, bool)
where
    I: IntoIterator<Item = &'a Candidate>,
    F: Fn(&Candidate) -> T,
{
    let mut v: Vec<&Candidate> = items.into_iter().collect();
    v.sort_by(by_reclaimable);
    let total = v.len() as u64;
    let out: Vec<T> = v.iter().take(cap).map(|c| f(c)).collect();
    let truncated = total > out.len() as u64;
    (out, total, truncated)
}

fn reclaimable_sum(t: &scull_core::TierBreakdown) -> u64 {
    t.s0.saturating_add(t.s1)
        .saturating_add(t.s2)
        .saturating_add(t.s3)
}

fn within(p: &str, root: &Path) -> bool {
    is_within(Path::new(p), root)
}

fn overview(ctx: &Ctx, allow: &Allowlist, cancel: &CancelToken) -> Result<Output, ToolError> {
    let roots = ctx.targets(allow, None)?;
    let rep = ctx.scan(roots.clone(), cancel)?;
    let r = &ctx.redactor;
    let mut per_root = Vec::new();
    for root in &roots {
        let cands: Vec<Candidate> = rep
            .candidates
            .iter()
            .filter(|c| within(c.path.as_str(), root))
            .cloned()
            .collect();
        let volume = scull_platform::volume_of(root).ok().map(|v| VolumeView {
            mount_point: r.path_of(&v.mount_point),
            total_bytes: v.total_bytes,
            free_bytes: v.free_bytes,
        });
        per_root.push(RootOverview {
            path: r.path_of(root),
            volume,
            totals: TotalsView::of(&scull_engine::Totals::of(&cands)),
            projects: rep
                .projects
                .iter()
                .filter(|p| within(p.root.as_str(), root))
                .count() as u64,
        });
    }
    let mut cats: Vec<CategoryTotal> = Vec::new();
    for c in &rep.candidates {
        let i = match cats.iter().position(|x| x.category == c.category) {
            Some(i) => i,
            None => {
                cats.push(CategoryTotal {
                    category: c.category,
                    count: 0,
                    allocated_bytes: 0,
                    reclaimable_bytes: 0,
                });
                cats.len() - 1
            }
        };
        let t = &mut cats[i];
        t.count += 1;
        t.allocated_bytes = t.allocated_bytes.saturating_add(c.sizes.allocated);
        t.reclaimable_bytes = t
            .reclaimable_bytes
            .saturating_add(c.sizes.reclaimable.bytes);
    }
    cats.sort_by_key(|c| std::cmp::Reverse(c.reclaimable_bytes));
    let mut projects: Vec<_> = rep.projects.iter().collect();
    projects.sort_by(|a, b| {
        reclaimable_sum(&b.reclaimable)
            .cmp(&reclaimable_sum(&a.reclaimable))
            .then_with(|| a.root.cmp(&b.root))
    });
    let out = OverviewOutput {
        roots: per_root,
        totals: TotalsView::of(&rep.totals),
        by_category: cats,
        largest_projects: projects
            .iter()
            .take(5.min(ctx.cap(None)))
            .map(|p| ProjectView::of(p, r))
            .collect(),
        projects: rep.projects.len() as u64,
        worktrees: rep.worktrees.len() as u64,
        entries_scanned: rep.entries_scanned,
        issues: rep.issues.len() as u64 + rep.issues_truncated,
        cancelled: rep.cancelled,
    };
    let summary = format!(
        "{} root(s), {} project(s), {} worktree(s): {} candidates, {} reclaimable ({} S0 \
         regenerable). Read-only; nothing was changed.",
        out.roots.len(),
        out.projects,
        out.worktrees,
        reclaimable_count(&out.totals),
        fmt_bytes(reclaimable_sum(&out.totals.reclaimable)),
        fmt_bytes(out.totals.reclaimable.s0),
    );
    output(&out, summary)
}

fn reclaimable_count(t: &TotalsView) -> u64 {
    reclaimable_sum(&t.count)
}

fn scan_directory(
    ctx: &Ctx,
    allow: &Allowlist,
    a: ScanArgs,
    cancel: &CancelToken,
) -> Result<Output, ToolError> {
    let roots = ctx.targets(allow, Some(&a.path))?;
    let rep = ctx.scan(roots.clone(), cancel)?;
    let r = &ctx.redactor;
    let cap = ctx.cap(a.max_results);
    let (candidates, candidates_total, truncated) =
        capped(&rep.candidates, cap, |c| CandidateView::of(c, r));
    let out = ScanOutput {
        path: r.path_of(&roots[0]),
        cancelled: rep.cancelled,
        entries_scanned: rep.entries_scanned,
        totals: TotalsView::of(&rep.totals),
        projects: rep.projects.len() as u64,
        worktrees: rep.worktrees.len() as u64,
        candidates,
        candidates_total,
        truncated,
        issues: rep
            .issues
            .iter()
            .take(cap)
            .map(|i| IssueView::of(i, r))
            .collect(),
        issues_total: rep.issues.len() as u64 + rep.issues_truncated,
        signals_unavailable: rep
            .signals_unavailable
            .iter()
            .map(|s| SignalView::of(s, r))
            .collect(),
    };
    let summary = format!(
        "Scanned {}: {} candidate(s){}, {} reclaimable ({} S0 regenerable), {} project(s), \
         {} worktree(s). Read-only; nothing was changed.",
        out.path,
        out.candidates_total,
        shown(out.candidates.len(), out.truncated),
        fmt_bytes(reclaimable_sum(&out.totals.reclaimable)),
        fmt_bytes(out.totals.reclaimable.s0),
        out.projects,
        out.worktrees,
    );
    output(&out, summary)
}

fn shown(n: usize, truncated: bool) -> String {
    if truncated {
        format!(" ({n} shown)")
    } else {
        String::new()
    }
}

fn list_projects(
    ctx: &Ctx,
    allow: &Allowlist,
    a: ListArgs,
    cancel: &CancelToken,
) -> Result<Output, ToolError> {
    let roots = ctx.targets(allow, a.path.as_deref())?;
    let rep = ctx.scan(roots, cancel)?;
    let r = &ctx.redactor;
    let cap = ctx.cap(a.max_results);
    let mut projects: Vec<_> = rep.projects.iter().collect();
    projects.sort_by(|a, b| {
        reclaimable_sum(&b.reclaimable)
            .cmp(&reclaimable_sum(&a.reclaimable))
            .then_with(|| a.root.cmp(&b.root))
    });
    let total = projects.len() as u64;
    let list: Vec<ProjectView> = projects
        .iter()
        .take(cap)
        .map(|p| ProjectView::of(p, r))
        .collect();
    let out = ProjectsOutput {
        truncated: total > list.len() as u64,
        projects: list,
        total,
        cancelled: rep.cancelled,
    };
    let summary = format!(
        "{} project(s){}.",
        out.total,
        shown(out.projects.len(), out.truncated)
    );
    output(&out, summary)
}

fn inspect_project(
    ctx: &Ctx,
    allow: &Allowlist,
    a: PathArgs,
    cancel: &CancelToken,
) -> Result<Output, ToolError> {
    let roots = ctx.targets(allow, Some(&a.path))?;
    let want = roots[0].clone();
    let rep = ctx.scan(roots, cancel)?;
    let r = &ctx.redactor;
    let project = rep.projects.iter().find(|p| p.root.to_path_buf() == want);
    let worktree = project
        .and_then(|p| p.git_worktree.as_ref())
        .and_then(|g| rep.worktrees.iter().find(|w| &w.path == g))
        .map(|w| WorktreeView::of(w, r));
    let (candidates, candidates_total, truncated) =
        capped(&rep.candidates, ctx.cap(None), |c| CandidateView::of(c, r));
    let out = ProjectOutput {
        path: r.path_of(&want),
        project: project.map(|p| ProjectView::of(p, r)),
        worktree,
        candidates,
        candidates_total,
        truncated,
        signals_unavailable: rep
            .signals_unavailable
            .iter()
            .map(|s| SignalView::of(s, r))
            .collect(),
        cancelled: rep.cancelled,
    };
    let summary = format!(
        "{}: {}, {} candidate(s){}.",
        out.path,
        if out.project.is_some() {
            "a project"
        } else {
            "not a project root"
        },
        out.candidates_total,
        shown(out.candidates.len(), out.truncated)
    );
    output(&out, summary)
}

fn list_worktrees(
    ctx: &Ctx,
    allow: &Allowlist,
    a: ListArgs,
    cancel: &CancelToken,
) -> Result<Output, ToolError> {
    let roots = ctx.targets(allow, a.path.as_deref())?;
    let rep = ctx.scan(roots, cancel)?;
    let r = &ctx.redactor;
    let cap = ctx.cap(a.max_results);
    let total = rep.worktrees.len() as u64;
    let worktrees: Vec<WorktreeView> = rep
        .worktrees
        .iter()
        .take(cap)
        .map(|w| WorktreeView::of(w, r))
        .collect();
    let out = WorktreesOutput {
        truncated: total > worktrees.len() as u64,
        worktrees,
        total,
        problems: rep
            .worktree_problems
            .iter()
            .take(cap)
            .map(|p| WorktreeProblemView {
                common_dir: r.path(p.common_dir.as_str()),
                kind: p.problem.kind,
                detail: r.text(&p.problem.detail),
            })
            .collect(),
        cancelled: rep.cancelled,
    };
    let summary = format!(
        "{} worktree(s){}, {} problem registration(s).",
        out.total,
        shown(out.worktrees.len(), out.truncated),
        rep.worktree_problems.len()
    );
    output(&out, summary)
}

fn inspect_worktree(ctx: &Ctx, allow: &Allowlist, a: PathArgs) -> Result<Output, ToolError> {
    let path = allow.check(&a.path)?;
    let r = &ctx.redactor;
    let w = ctx
        .engine
        .inspect_worktree(&path)
        .map_err(|e| ToolError::from_scull(&e, r))?;
    let out = WorktreeOutput {
        worktree: WorktreeView::of(&w, r),
    };
    let summary = format!(
        "{}: {} worktree, tier {:?}, proposed action {}.",
        out.worktree.path,
        crate::views::json_name(&out.worktree.kind),
        out.worktree.tier,
        out.worktree.action
    );
    output(&out, summary)
}

fn list_caches(ctx: &Ctx, a: CachesArgs, cancel: &CancelToken) -> Result<Output, ToolError> {
    let Some(env) = &ctx.cache_env else {
        return Err(ToolError::new(
            "unavailable",
            "package caches can't be located here (unknown home folder or OS)",
        ));
    };
    let r = &ctx.redactor;
    let rep = ctx.engine.caches(env, ctx.threads, cancel, |_| {});
    let (caches, total, truncated) = capped(&rep.candidates, ctx.cap(a.max_results), |c| {
        CandidateView::of(c, r)
    });
    let out = CachesOutput {
        caches,
        total,
        truncated,
        refused: rep
            .refused
            .iter()
            .take(ctx.cap(a.max_results))
            .map(|x| RefusedCacheView {
                rule: r.label(&x.rule),
                path: x.path.as_ref().map(|p| r.path(p.as_str())),
                source: r.text(&x.source),
                reason: r.text(&x.reason),
            })
            .collect(),
        totals: TotalsView::of(&rep.totals),
        cancelled: rep.cancelled,
    };
    let summary = format!(
        "{} cache(s){}, {} reclaimable.",
        out.total,
        shown(out.caches.len(), out.truncated),
        fmt_bytes(reclaimable_sum(&out.totals.reclaimable))
    );
    output(&out, summary)
}

fn find_large_files(
    ctx: &Ctx,
    allow: &Allowlist,
    a: LargeFilesArgs,
    cancel: &CancelToken,
) -> Result<Output, ToolError> {
    let path = allow.check(&a.path)?;
    let r = &ctx.redactor;
    let threshold = a.min_size_bytes.unwrap_or(DEFAULT_LARGE_FILE_BYTES);
    let opts = LargeFileOptions {
        threshold,
        top: ctx.cap(a.max_results),
    };
    let (report, outcome) = inspect_large_files(
        Arc::new(scull_platform::Native),
        &path,
        opts,
        &ScanOptions::default(),
        cancel,
    )
    .map_err(|e| ToolError::from_scull(&ScullError::from(&e), r))?;
    let truncated = report.truncated();
    let (matched, matched_bytes) = (report.matched, report.matched_bytes);
    let files: Vec<CandidateView> = report
        .into_candidates()
        .iter()
        .map(|c| CandidateView::of(c, r))
        .collect();
    let out = LargeFilesOutput {
        path: r.path_of(&path),
        min_size_bytes: threshold,
        files,
        matched,
        matched_bytes,
        truncated,
        complete: outcome.root.as_ref().is_some_and(|x| x.complete) && !outcome.is_cancelled(),
    };
    let summary = format!(
        "{} file(s) of {} or more under {}{} ({}).",
        out.matched,
        fmt_bytes(threshold),
        out.path,
        shown(out.files.len(), out.truncated),
        fmt_bytes(out.matched_bytes)
    );
    output(&out, summary)
}

fn candidates(
    ctx: &Ctx,
    allow: &Allowlist,
    a: CandidatesArgs,
    cancel: &CancelToken,
) -> Result<Output, ToolError> {
    let roots = ctx.targets(allow, a.path.as_deref())?;
    let rep = ctx.scan(roots, cancel)?;
    let r = &ctx.redactor;
    let tiers = a
        .tiers
        .unwrap_or_else(|| vec![Tier::S0, Tier::S1, Tier::S2]);
    let matching: Vec<&Candidate> = rep
        .candidates
        .iter()
        .filter(|c| tiers.contains(&c.tier))
        .filter(|c| a.category.is_none_or(|k| c.category == k))
        .collect();
    let reclaimable_bytes = matching
        .iter()
        .fold(0u64, |s, c| s.saturating_add(c.sizes.reclaimable.bytes));
    let (list, total, truncated) = capped(matching, ctx.cap(a.max_results), |c| {
        CandidateView::of(c, r)
    });
    let out = CandidatesOutput {
        candidates: list,
        total,
        truncated,
        reclaimable_bytes,
        read_only: true,
        cancelled: rep.cancelled,
    };
    let summary = format!(
        "{} candidate(s){}, {} reclaimable. Nothing was prepared or changed: to clean, run \
         `scull clean` in a terminal and review the plan.",
        out.total,
        shown(out.candidates.len(), out.truncated),
        fmt_bytes(out.reclaimable_bytes)
    );
    output(&out, summary)
}

const ALL_CATEGORIES: [Category; 8] = [
    Category::ProjectArtifact,
    Category::PackageCache,
    Category::Worktree,
    Category::MlCache,
    Category::Container,
    Category::LargeFile,
    Category::ScullData,
    Category::Other,
];

fn cleanup_history(ctx: &Ctx, allow: &Allowlist, a: HistoryArgs) -> Result<Output, ToolError> {
    let r = &ctx.redactor;
    let days = i64::from(a.since_days.unwrap_or(DEFAULT_HISTORY_DAYS).clamp(1, 3650));
    let since = now_unix().saturating_sub(days * 86_400);
    let cap = ctx.cap(a.max_results);
    let item_id = match &a.item_id {
        Some(s) => Some(
            serde_json::from_value::<ItemId>(Value::String(s.clone())).map_err(|_| {
                ToolError::new("invalid_arguments", "item_id is not a valid item ID")
            })?,
        ),
        None => None,
    };
    let mut out = HistoryOutput {
        state: "ok".into(),
        detail: None,
        since,
        categories: Vec::new(),
        regrowth: Vec::new(),
        regrowth_truncated: false,
        item: None,
    };
    let store = match history::open_read_only(ctx.data_dir.as_deref()) {
        Ok(s) => s,
        Err(u) => {
            out.state = u.state().into();
            out.detail = Some(r.text(&u.detail()));
            let summary = format!("No history: {}", u.detail());
            return output(&out, r.text(&summary));
        }
    };
    let db_err = |e: scull_storage::StoreError| ToolError::new("history", r.text(&e.to_string()));
    let cats: Vec<Category> = match a.category {
        Some(c) => vec![c],
        None => ALL_CATEGORIES.to_vec(),
    };
    let mut regrowth = Vec::new();
    for cat in cats {
        let points = store.rollup_history(cat, since).map_err(db_err)?;
        if !points.is_empty() && points.iter().any(|p| p.items > 0) {
            let skip = points.len().saturating_sub(cap);
            out.categories.push(CategoryHistory {
                category: cat,
                truncated: skip > 0,
                points: points
                    .iter()
                    .skip(skip)
                    .map(|p| RollupPointView {
                        at: p.at,
                        scan_id: p.scan_id.as_str().to_owned(),
                        items: p.items,
                        allocated_bytes: p.allocated_bytes,
                        reclaimable_bytes: p.reclaimable_bytes,
                    })
                    .collect(),
            });
        }
        regrowth.extend(
            store
                .regrowth(cat, since)
                .map_err(db_err)?
                .into_iter()
                .filter(|g| allow.contains_canonical(&g.path.to_path_buf())),
        );
    }
    regrowth.sort_by_key(|g| std::cmp::Reverse(g.reappeared_at));
    out.regrowth_truncated = regrowth.len() > cap;
    out.regrowth = regrowth
        .iter()
        .take(cap)
        .map(|g| RegrowthView {
            path: r.path(g.path.as_str()),
            artifact: r.label(&g.artifact),
            item_id: g.item_id.as_str().to_owned(),
            disappeared_at: g.disappeared_at,
            reappeared_at: g.reappeared_at,
        })
        .collect();
    if let Some(id) = item_id
        && let Some(h) = store.item_history(&id).map_err(db_err)?
        && allow.contains_canonical(&h.path.to_path_buf())
    {
        let skip = h.events.len().saturating_sub(cap);
        out.item = Some(ItemHistoryView {
            item_id: h.item_id.as_str().to_owned(),
            path: r.path(h.path.as_str()),
            category: h.category,
            tier: h.tier,
            first_seen_at: h.first_seen_at,
            last_seen_at: h.last_seen_at,
            disappeared_at: h.disappeared_at,
            events: h
                .events
                .iter()
                .skip(skip)
                .map(|e| ItemEventView {
                    at: e.at,
                    kind: event_kind(e.kind),
                    tier: e.tier,
                    allocated_bytes: e.allocated_bytes,
                    reclaimable_bytes: e.reclaimable_bytes,
                })
                .collect(),
        });
    }
    let summary = format!(
        "History since {} day(s) ago: {} categor(ies) with data, {} regrowth event(s){}.",
        days,
        out.categories.len(),
        out.regrowth.len(),
        if out.item.is_some() {
            ", item found"
        } else {
            ""
        }
    );
    output(&out, summary)
}

fn event_kind(k: scull_storage::ItemEventKind) -> String {
    use scull_storage::ItemEventKind as K;
    match k {
        K::FirstSeen => "first_seen",
        K::Changed => "changed",
        K::Disappeared => "disappeared",
        K::Reappeared => "reappeared",
        _ => "other",
    }
    .to_owned()
}

fn scan_status(s: scull_storage::ScanStatus) -> String {
    format!("{s:?}").to_lowercase()
}

fn status(ctx: &Ctx, allow: &Allowlist) -> Result<Output, ToolError> {
    let r = &ctx.redactor;
    let server = ServerFacts {
        name: "scull".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        read_only: true,
        tools: TOOL_NAMES.iter().map(|s| (*s).to_owned()).collect(),
        allowlisted_roots: allow.roots().iter().map(|x| r.path_of(&x.given)).collect(),
        redact_home: r.redacts_home(),
        hash_names: r.hashes_names(),
        max_results: ctx.max_results,
    };
    let mut out = StatusOutput {
        server,
        state: "ok".into(),
        detail: None,
        last_scan: None,
        volumes: Vec::new(),
    };
    match history::open_read_only(ctx.data_dir.as_deref()) {
        Err(u) => {
            out.state = u.state().into();
            out.detail = Some(r.text(&u.detail()));
        }
        Ok(store) => match store.status_summary() {
            Err(e) => {
                out.state = "unavailable".into();
                out.detail = Some(r.text(&e.to_string()));
            }
            Ok(s) => {
                let now = now_unix();
                out.last_scan = s.last_scan.map(|l| LastScanView {
                    scan_id: l.scan_id.as_str().to_owned(),
                    finished_at: l.finished_at,
                    status: scan_status(l.status),
                    age_secs: u64::try_from(now.saturating_sub(l.finished_at)).unwrap_or(0),
                    candidates: l.candidates,
                    reclaimable_bytes: l.reclaimable_bytes,
                    safe_reclaimable_bytes: l.safe_reclaimable_bytes,
                });
                if out.last_scan.is_none() {
                    out.state = "no_scan".into();
                }
                out.volumes = s
                    .volumes
                    .iter()
                    .take(ctx.cap(None))
                    .map(|v| VolumeSampleView {
                        volume: r.path(&v.volume_id),
                        total_bytes: v.total,
                        free_bytes: v.free,
                        sampled_at: v.at,
                    })
                    .collect();
            }
        },
    }
    let summary = format!(
        "scull {} read-only MCP server. Allowlisted roots: {}. History: {}.",
        out.server.version,
        if out.server.allowlisted_roots.is_empty() {
            "none (set scan.roots in the config)".to_owned()
        } else {
            out.server.allowlisted_roots.join(", ")
        },
        out.state
    );
    output(&out, summary)
}

/// Human byte count (binary units).
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::action_name;

    #[test]
    fn definitions_match_names_and_are_read_only() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names, TOOL_NAMES);
        for t in &defs {
            let ann = t.annotations.as_ref().expect("annotations");
            assert_eq!(ann.read_only_hint, Some(true), "{}", t.name);
            assert_eq!(ann.destructive_hint, Some(false), "{}", t.name);
            assert!(t.output_schema.is_some(), "{}", t.name);
            assert_eq!(
                t.input_schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{}",
                t.name
            );
        }
    }

    #[test]
    fn bytes_are_human() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(500 * 1024 * 1024), "500.0 MiB");
    }

    #[test]
    fn action_names() {
        assert_eq!(action_name(&scull_core::Action::Delete), "delete");
        assert_eq!(action_name(&scull_core::Action::None), "none");
    }
}
