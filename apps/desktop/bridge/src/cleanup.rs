//! The desktop's cleanups through the core Executor (P3-EXEC-01): Cleanup Review plans and runs,
//! Quick Clean, and restoring quarantined items. **The only module of the desktop that reaches
//! `scull-cleaner`**, and it reaches deletion only through its public API
//! (`Executor::execute_plan`, `Executor::restore`, `QuickClean::run`), never a filesystem call
//! of its own (SAFE-10 as the PRD states it; checked by `tests/safe10.rs`).
//!
//! What every run goes through, exactly like `scull clean` / `scull clean caches` /
//! `scull restore`:
//!
//! * **Not elevated** (SAFE-14): the desktop has no `--allow-root`; an elevated or unknown
//!   process is refused before anything is touched.
//! * **A recorded plan** (`PlanOrigin::Desktop`, D-25: 1 h): prepared from this app's last
//!   complete scan, with the SAFE-09 watermark read **before** that scan. Only eligible items
//!   (D-28: delete S0 only, S1/S2 quarantine or Trash only, S3 never; linked worktrees only at S1).
//! * **The human's confirmation in the main window** (the Confirm button, sent as the
//!   reviewed plan ID, hash and the exact items chosen; recorded with `ApprovalMethod::Desktop`
//!   and the OS user as approver). An item whose action scull can't undo at S1/S2
//!   (SAFE-07, `needs_elevated_confirmation`) additionally needs the typed count, including
//!   removal of an eligible linked worktree.
//! * **The Executor under the execution lock** (SAFE-09), after reconciling any interrupted
//!   operation (SAFE-19): the operation is refused if another touched the items after the scan
//!   (watermark); every item is revalidated against the disk right before it is touched
//!   (SAFE-05); everything is journaled and audited (SAFE-08).
//!
//! Commands are invoked by the main window only (`capabilities/default.json`; the menu bar
//! widget can't) and none of this is reachable from MCP (`scull-mcp` doesn't depend on this
//! crate).

use crate::session::{ScanBasis, Session};
use crate::views::OpId;
use schemars::JsonSchema;
use scull_cleaner::quickclean::{
    Ended, OutputStream, QuickClean, QuickCleanEvent, QuickCleanOptions, QuickCleanRequest,
    ToolResult, escape_line,
};
use scull_cleaner::trash::TrashLocations;
use scull_cleaner::{
    Executor, ItemOutcome, ItemReport, LockOptions, PlanContext, QuarantineSettings,
    RestoreOutcome, Revalidation,
};
use scull_core::config::TEST_SANDBOX_ENV;
use scull_core::{
    Action, Candidate, ErrorKind, FsPath, ItemId, OperationId, Plan, PlanId, PlanItem, PlanOrigin,
    PlanParams, Restorability, ScanId, ScullError, Tier, action_allowed_for,
};
use scull_detectors::caches::CacheTool;
use scull_detectors::rules::Platform as RulesOs;
use scull_detectors::{CacheEnv, DetectContext, locate_caches};
use scull_engine::{CancelToken, FsConfigReader, history_db_path, open_history};
use scull_platform::activity::Limits;
use scull_platform::{Elevation, Native};
use scull_safety::NativeActivityProbe;
use scull_storage::{ApprovalMethod, OperationEnd, OperationStart, Store};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Test hook (same as the CLI's): when set, the desktop behaves as if it ran elevated. It can
/// only add a refusal, never remove one.
pub const ASSUME_ELEVATED_ENV: &str = "SCULL_TEST_ASSUME_ELEVATED";

/// Folder under scull's state directory for Quick Clean's private tool sandbox (as the CLI).
const QUICK_CLEAN_SANDBOX: &str = "quickclean";

/// Probe the execution lock without recovering or running an operation.
pub fn execution_busy_at(path: &std::path::Path) -> Result<bool, ScullError> {
    match Executor::acquire(&LockOptions::at(path)) {
        Ok(_) => Ok(false),
        Err(error) if error.kind == ErrorKind::LockHeld => Ok(true),
        Err(error) => Err(error),
    }
}

/// Whether another operation holds the current user's execution lock.
pub fn execution_busy() -> Result<bool, ScullError> {
    execution_busy_at(&LockOptions::for_current_user()?.path)
}

/// A Cleanup Review plan as the human reviews it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopPlan {
    /// Plan ID.
    pub plan_id: String,
    /// Content hash; send it back to execute.
    pub content_hash: String,
    /// When it was made (Unix seconds).
    pub created_at: i64,
    /// When it expires (Unix seconds; D-25: 1 hour).
    pub expires_at: i64,
    /// Every item that may be cleaned, as the last scan classified it (S0, S1, S2).
    pub items: Vec<Candidate>,
    /// What scull left out on purpose: S3 and items with no cleanup action.
    pub left_out: Vec<LeftOut>,
}

/// An item scull doesn't clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeftOut {
    /// Its path.
    pub path: String,
    /// Its tier.
    pub tier: Tier,
    /// Why.
    pub reason: String,
}

/// What happened to one item of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunItemOutcome {
    /// Deleted (S0).
    Deleted,
    /// Moved into quarantine (restorable).
    Quarantined,
    /// Moved to the Trash (quarantine was impossible).
    Trashed,
    /// Not touched: it changed since the scan, is in use, the run was cancelled, or the
    /// Executor refused it.
    Skipped,
}

/// One item of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunItem {
    /// Plan item.
    pub item_id: String,
    /// What happened.
    pub outcome: RunItemOutcome,
    /// Stable reason code when not cleaned (or only partly).
    pub reason: Option<String>,
    /// Bytes freed (the planned estimate for a completed delete).
    pub freed_bytes: u64,
    /// Bytes moved aside (quarantine, Trash).
    pub moved_bytes: u64,
}

/// The result of a Cleanup Review run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopRun {
    /// The plan that ran (the reviewed one, or the chosen subset of it).
    pub plan_id: String,
    /// The recorded operation (`scull history <id>`).
    pub operation_id: String,
    /// Every item, in plan order.
    pub items: Vec<RunItem>,
    /// Bytes the plan predicted.
    pub predicted_bytes: u64,
    /// Free space after minus before, when measured.
    pub measured_delta_bytes: Option<i64>,
    /// Bytes moved aside.
    pub moved_bytes: u64,
    /// Whether the run stopped early.
    pub cancelled: bool,
    /// When it finished (Unix seconds).
    pub finished_at: i64,
}

/// A restored quarantine entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopRestore {
    /// Where it is now.
    pub restored_to: String,
    /// `true` when the original path was taken and a `.scull-restored-<n>` sibling was used.
    pub sibling: bool,
    /// `true` when it had already been restored (nothing moved now).
    pub already: bool,
}

/// One tool Quick Clean ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuickCleanTool {
    /// The cache's candidate (the Quick Clean chip).
    pub item_id: Option<String>,
    /// The tool (`npm`, `pnpm`, ...).
    pub tool: String,
    /// Whether its command succeeded.
    pub succeeded: bool,
    /// Bytes the cache shrank by, when measured.
    pub cache_bytes_freed: Option<i64>,
    /// Core explanation when the tool kept referenced or otherwise needed files.
    pub explanation: Option<String>,
}

/// The result of a Quick Clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopQuickClean {
    /// The recorded operation, when anything ran.
    pub operation_id: Option<String>,
    /// The tools that ran.
    pub tools: Vec<QuickCleanTool>,
    /// Tools not run, with the reason (`not trusted`, `in use`, ...).
    pub skipped: Vec<QuickCleanSkip>,
    /// Bytes the caches held before (estimate).
    pub predicted_bytes: u64,
    /// Free space after minus before, when measured.
    pub measured_delta_bytes: Option<i64>,
}

/// Window-scoped live output from an approved cache cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopCacheProgress {
    /// Client-chosen operation ID, independent of the audited operation ID.
    pub op_id: OpId,
    /// The reported change.
    pub kind: CacheProgressKind,
    /// Selected cache candidate, when this is a tool event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// Package manager, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Which process stream produced this line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<CacheOutputStream>,
    /// Escaped, capped output supplied by the core runner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// Validated executable and arguments at command start, without environment values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Why a selected cache was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Command success, only present when it finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub succeeded: Option<bool>,
    /// Distinct selected caches that finished or were skipped.
    pub completed: usize,
    /// Number of distinct selected cache IDs.
    pub total: usize,
}

impl DesktopCacheProgress {
    /// Event name emitted only to the requesting webview.
    pub const NAME: &'static str = "scull://cache-clean-progress";
}

/// Cache cleanup event kinds; tool progress is counted only at terminal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheProgressKind {
    /// Refreshing the safe plan before running any command.
    Preparing,
    /// A command is about to run.
    Started,
    /// One escaped process output line.
    Output,
    /// Further process output exceeded the capture limit.
    OutputTruncated,
    /// A command exited.
    Finished,
    /// The cache was not touched.
    Skipped,
}

/// A native process output stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheOutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

type CacheProgressSink<'a> = (
    &'a Mutex<CacheProgress>,
    &'a (dyn Fn(DesktopCacheProgress) + Send + Sync),
);

struct CacheProgress {
    current: DesktopCacheProgress,
    chosen: BTreeSet<String>,
    completed: BTreeSet<String>,
}

impl CacheProgress {
    fn new(op_id: &OpId, chosen: &[String]) -> Self {
        let chosen: BTreeSet<_> = chosen.iter().cloned().collect();
        Self {
            current: DesktopCacheProgress {
                op_id: op_id.clone(),
                kind: CacheProgressKind::Preparing,
                item_id: None,
                tool: None,
                stream: None,
                line: None,
                command: None,
                reason: None,
                succeeded: None,
                completed: 0,
                total: chosen.len(),
            },
            chosen,
            completed: BTreeSet::new(),
        }
    }

    fn event(&mut self, event: QuickCleanEvent) -> Option<DesktopCacheProgress> {
        let mut payload = self.current.clone();
        let (item_id, tool) = match event {
            QuickCleanEvent::Started { item_id, tool } => {
                payload.kind = CacheProgressKind::Started;
                (item_id, tool)
            }
            QuickCleanEvent::Output {
                item_id,
                tool,
                stream,
                line,
            } => {
                payload.kind = CacheProgressKind::Output;
                payload.stream = Some(match stream {
                    OutputStream::Stdout => CacheOutputStream::Stdout,
                    OutputStream::Stderr => CacheOutputStream::Stderr,
                });
                payload.line = Some(line);
                (item_id, tool)
            }
            QuickCleanEvent::OutputTruncated {
                item_id,
                tool,
                stream,
            } => {
                payload.kind = CacheProgressKind::OutputTruncated;
                payload.stream = Some(match stream {
                    OutputStream::Stdout => CacheOutputStream::Stdout,
                    OutputStream::Stderr => CacheOutputStream::Stderr,
                });
                (item_id, tool)
            }
            QuickCleanEvent::Finished {
                item_id,
                tool,
                succeeded,
            } => {
                payload.kind = CacheProgressKind::Finished;
                payload.succeeded = Some(succeeded);
                (item_id, tool)
            }
            QuickCleanEvent::Skipped {
                item_id,
                tool,
                reason,
            } => {
                payload.kind = CacheProgressKind::Skipped;
                payload.reason = Some(reason);
                (item_id, tool)
            }
        };
        let id = item_id.as_str();
        if !self.chosen.contains(id) || self.completed.contains(id) {
            return None;
        }
        if matches!(
            payload.kind,
            CacheProgressKind::Finished | CacheProgressKind::Skipped
        ) {
            self.completed.insert(id.to_owned());
        }
        payload.item_id = Some(id.to_owned());
        payload.tool = Some(tool.rule_id().split('.').next().unwrap_or("").to_owned());
        payload.completed = self.completed.len();
        Some(payload)
    }
}

/// A tool Quick Clean didn't run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuickCleanSkip {
    /// The tool.
    pub tool: String,
    /// Why, for display.
    pub reason: String,
}

/// An exact native command shown for review. Display only; never interpreted as shell text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheCleanCommand {
    /// Canonical trusted executable.
    pub binary: String,
    /// Arguments passed directly to the executable.
    pub args: Vec<String>,
    /// Cache location variables added to the sanitized environment.
    pub env: Vec<CacheCleanEnv>,
}

/// A cache-location variable in the sanitized native environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheCleanEnv {
    /// Variable name.
    pub name: String,
    /// Located cache folder or other scoped value.
    pub value: String,
}

/// A tool and the modes the core planner currently permits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheCleanTool {
    /// Core's human-readable cache name.
    pub name: String,
    /// Classified cache item ID.
    pub item_id: String,
    /// Tool name.
    pub tool: String,
    /// Located cache folder.
    pub cache: String,
    /// Estimated reclaimable bytes.
    pub bytes: u64,
    /// Prune command, when supported.
    pub prune: Option<CacheCleanCommand>,
    /// Wipe command, when supported.
    pub wipe: Option<CacheCleanCommand>,
    /// Core planner's safety notes.
    pub notes: Vec<String>,
}

/// The native Quick Clean preview. Unsupported or protected caches remain skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesktopCachePreview {
    /// Runnable tools.
    pub tools: Vec<CacheCleanTool>,
    /// Tools left alone and the core planner's reasons.
    pub skipped: Vec<CacheCleanSkip>,
}

/// A cache omitted by the native planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheCleanSkip {
    /// Core's human-readable cache name.
    pub name: String,
    /// Tool name.
    pub tool: String,
    /// Located path, if known.
    pub path: Option<String>,
    /// Estimate from classification, when available.
    pub bytes: Option<u64>,
    /// Core planner reason.
    pub reason: String,
}

/// One explicitly selected mode and the command the human reviewed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheCleanChoice {
    /// Cache item ID.
    pub item_id: String,
    /// Either `prune` or `wipe`; checked against the fresh core plan.
    pub mode: String,
    /// The exact command previously shown for this mode.
    pub command: CacheCleanCommand,
}

/// The machine and user plans are bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    /// This installation's machine ID.
    pub machine_id: String,
    /// The OS user.
    pub user: String,
}

/// The desktop's cleanup runner. One per app.
pub struct CleanupDesk {
    who: Option<Who>,
    trash: Option<TrashLocations>,
    lock: Option<PathBuf>,
    assume_elevated: bool,
}

impl Default for CleanupDesk {
    fn default() -> Self {
        Self::new()
    }
}

fn store_err(e: scull_storage::StoreError) -> ScullError {
    ScullError::from(e)
}

/// Whether this process runs elevated, honoring [`ASSUME_ELEVATED_ENV`].
fn elevation() -> Elevation {
    if std::env::var_os(ASSUME_ELEVATED_ENV).is_some_and(|v| !v.is_empty()) {
        return Elevation::Elevated;
    }
    scull_platform::is_elevated()
}

/// SAFE-14: the desktop never changes anything as root/administrator (or when that can't be
/// told).
fn check_not_elevated() -> Result<(), ScullError> {
    if elevation().blocks_destructive() {
        return Err(elevated_refusal());
    }
    Ok(())
}

fn elevated_refusal() -> ScullError {
    ScullError::new(
        ErrorKind::SafetyBlocked,
        "scull is running as root/administrator (or can't tell) and refuses to change anything \
         (SAFE-14): it never needs elevated rights to clean your own files. Run the app as your \
         normal user. Nothing was changed.",
    )
}

/// SAFE-07: an S1/S2 item whose action scull can't undo needs the typed count.
pub fn needs_elevated_confirmation(tier: Tier, action: &Action) -> bool {
    matches!(tier, Tier::S1 | Tier::S2)
        && matches!(
            action.restorability(),
            Restorability::Regenerate | Restorability::NotRestorable
        )
}

/// Desktop plans allow tier-compatible deletes/quarantines and eligible S1 linked-worktree
/// removal. Tool commands use Quick Clean. `Some(reason)` when ineligible.
fn ineligible(c: &Candidate) -> Option<&'static str> {
    if !c.tier.is_actionable() {
        return Some("protected: never cleaned");
    }
    match &c.action {
        Action::None => Some("no cleanup action at this tier (inspect only)"),
        Action::Delete | Action::Quarantine
            if c.is_consistent() && action_allowed_for(c.tier, &c.action) =>
        {
            None
        }
        Action::Delete | Action::Quarantine => {
            Some("its tier, reasons and action disagree; left alone")
        }
        Action::GitWorktreeRemove
            if c.is_consistent()
                && c.tier == Tier::S1
                && c.rule.id == scull_git::WORKTREE_RULE_ID =>
        {
            None
        }
        Action::ToolCommand { .. } => Some("cleaned by Quick Clean or not run by scull"),
        _ => Some("not executed from the desktop in this build"),
    }
}

/// A fresh ID suffix: Unix seconds then 64 random bits, in hex.
fn new_id_suffix(now: i64) -> String {
    use std::hash::{BuildHasher, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let random = RandomState::new().hash_one((nanos, n, std::process::id()));
    format!("{:x}{random:016x}", u64::try_from(now).unwrap_or(0))
}

fn internal(e: String) -> ScullError {
    ScullError::new(ErrorKind::Internal, e)
}

impl CleanupDesk {
    /// The real machine: this installation's machine ID (created on first use, like the CLI),
    /// the OS user, the user's Trash.
    pub fn new() -> Self {
        Self {
            who: None,
            trash: None,
            lock: None,
            assume_elevated: false,
        }
    }

    /// A desk bound to `who`, with the Trash at `trash` and the execution lock at `lock`
    /// (tests: everything inside a temp sandbox).
    pub fn with(who: Who, trash: TrashLocations, lock: PathBuf) -> Self {
        Self {
            who: Some(who),
            trash: Some(trash),
            lock: Some(lock),
            assume_elevated: false,
        }
    }

    /// Behave as if elevated (tests). Can only add the SAFE-14 refusal, never remove it.
    #[must_use]
    pub fn assume_elevated(mut self) -> Self {
        self.assume_elevated = true;
        self
    }

    /// SAFE-14: the desktop never changes anything as root/administrator (or when that can't be
    /// told); it has no `--allow-root`.
    fn check_not_elevated(&self) -> Result<(), ScullError> {
        if self.assume_elevated {
            return Err(elevated_refusal());
        }
        check_not_elevated()
    }

    /// The execution lock and crash reconciliation (SAFE-09, SAFE-19).
    fn lock(&self, store: &mut Store, now: i64) -> Result<Executor, ScullError> {
        let opts = match &self.lock {
            Some(p) => LockOptions::at(p),
            None => LockOptions::for_current_user()?,
        };
        let executor = Executor::acquire(&opts)?;
        executor.recover(store, now)?;
        Ok(executor)
    }

    fn who(&self, session: &Session) -> Result<Who, ScullError> {
        if let Some(w) = &self.who {
            return Ok(w.clone());
        }
        let data = session.setup().data_dir.as_deref().ok_or_else(|| {
            ScullError::new(ErrorKind::Config, "scull's data directory is not known")
        })?;
        let machine_id = scull_platform::machine_id(data).map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot read or create this machine's ID: {e}"),
            )
        })?;
        let user = scull_platform::current_user().map_err(|e| {
            ScullError::new(ErrorKind::Io, format!("cannot tell the current user: {e}"))
        })?;
        Ok(Who { machine_id, user })
    }

    /// The user's Trash, or one inside the test sandbox (never the real one under tests).
    fn trash_locations(&self) -> TrashLocations {
        if let Some(t) = &self.trash {
            return t.clone();
        }
        match std::env::var_os(TEST_SANDBOX_ENV).filter(|v| !v.is_empty()) {
            Some(sandbox) => {
                let sandbox = PathBuf::from(sandbox);
                TrashLocations {
                    home: Some(sandbox.join("Trash")),
                    topdir: Some(sandbox),
                }
            }
            None => TrashLocations::from_env(),
        }
    }

    fn store(session: &Session) -> Result<Store, ScullError> {
        let data = session.setup().data_dir.as_deref().ok_or_else(|| {
            ScullError::new(ErrorKind::Config, "scull's data directory is not known")
        })?;
        open_history(data).map_err(store_err)
    }

    /// Prepare a Cleanup Review plan from the last complete scan: every eligible item, or only
    /// `items` (IDs; each must be eligible). Recorded, not executed.
    pub fn prepare(
        &self,
        session: &Session,
        items: Option<&[String]>,
        now: i64,
    ) -> Result<DesktopPlan, ScullError> {
        let (scan, basis) = session.last_scan_basis().ok_or_else(|| {
            ScullError::new(
                ErrorKind::Usage,
                "there is no complete scan under the current settings: scan again to plan a cleanup",
            )
        })?;
        // A cancelled scan is partial: what it didn't reach isn't known (nor is what it did
        // reach checked against the rest), so nothing is planned from it.
        if scan.cancelled {
            return Err(ScullError::new(
                ErrorKind::Usage,
                "the last scan was cancelled before it finished: scan again to plan a cleanup",
            ));
        }
        let watermark = basis.watermark.ok_or_else(|| {
            ScullError::new(
                ErrorKind::SafetyBlocked,
                "the scan's operations watermark couldn't be read (SAFE-09); scan again",
            )
        })?;
        let who = self.who(session)?;
        let wanted: Option<BTreeSet<&str>> =
            items.map(|ids| ids.iter().map(String::as_str).collect());
        if let Some(w) = &wanted {
            for id in w {
                let Some(c) = scan.candidates.iter().find(|c| c.id.as_str() == *id) else {
                    return Err(ScullError::new(
                        ErrorKind::Usage,
                        format!("item {id} is not in the last scan; scan again"),
                    ));
                };
                if let Some(why) = ineligible(c) {
                    return Err(ScullError::new(
                        ErrorKind::SafetyBlocked,
                        format!("{} can't be planned: {why}", c.path),
                    )
                    .with_item(c.id.clone())
                    .with_path(c.path.clone()));
                }
            }
        }
        let mut chosen = Vec::new();
        let mut left_out = Vec::new();
        for c in &scan.candidates {
            if wanted.as_ref().is_some_and(|w| !w.contains(c.id.as_str())) {
                continue;
            }
            match ineligible(c) {
                None => chosen.push(c.clone()),
                Some(reason) => left_out.push(LeftOut {
                    path: c.path.as_str().to_owned(),
                    tier: c.tier,
                    reason: reason.to_owned(),
                }),
            }
        }
        if chosen.is_empty() {
            return Err(ScullError::new(
                ErrorKind::Usage,
                "nothing in the last scan can be cleaned from here",
            ));
        }
        let plan = desktop_plan(
            &chosen
                .iter()
                .map(PlanItem::from_candidate)
                .collect::<Result<Vec<_>, _>>()?,
            &scan.scan_id,
            watermark,
            &who,
            Lifetime::fresh(now),
        )?;
        Self::store(session)?
            .insert_plan(&plan)
            .map_err(store_err)?;
        Ok(DesktopPlan {
            plan_id: plan.id().as_str().to_owned(),
            content_hash: plan.content_hash().as_str().to_owned(),
            created_at: plan.created_unix(),
            expires_at: plan.expires_unix(),
            items: chosen,
            left_out,
        })
    }

    /// Execute the items `item_ids` of the reviewed plan `plan_id` (hash `content_hash`), after
    /// the human confirmed them in the main window. `typed_count` is the typed confirmation,
    /// needed only when an item's action can't be undone at S1/S2 (never in a D-28 plan).
    pub fn execute(
        &self,
        session: &Session,
        plan_id: &str,
        content_hash: &str,
        item_ids: &[String],
        typed_count: Option<u64>,
        now: i64,
    ) -> Result<DesktopRun, ScullError> {
        self.check_not_elevated()?;
        let id = PlanId::try_from(plan_id.to_owned())
            .map_err(|_| ScullError::new(ErrorKind::Usage, "not a scull plan ID"))?;
        let who = self.who(session)?;
        let mut store = Self::store(session)?;
        let reviewed = store.get_plan(&id).map_err(store_err)?.ok_or_else(|| {
            ScullError::new(ErrorKind::NotFound, format!("no plan {id} on this machine"))
        })?;
        if reviewed.origin() != PlanOrigin::Desktop {
            return Err(ScullError::new(
                ErrorKind::PlanForeign,
                format!(
                    "plan {id} was not made in this app; it must be approved where it was made"
                ),
            ));
        }
        if reviewed.content_hash().as_str() != content_hash {
            return Err(ScullError::new(
                ErrorKind::PlanTampered,
                format!("plan {id} differs from the plan that was reviewed; nothing was changed"),
            ));
        }
        reviewed.verify(now, &who.machine_id, &who.user)?;
        // Revalidation runs under the roots and configuration of the scan the plan was made
        // from, read with that scan's report: if the app has scanned since, the plan is stale.
        let basis = match session.last_scan_basis() {
            Some((scan, basis)) if scan.scan_id == *reviewed.scan_id() && !scan.cancelled => basis,
            _ => {
                return Err(ScullError::new(
                    ErrorKind::PlanInvalidated,
                    format!(
                        "plan {id} no longer matches the current scan or settings; \
                         scan again and review a new plan. Nothing was changed."
                    ),
                ));
            }
        };
        // Exactly the items the human chose, each from the reviewed plan, once.
        let mut picked = BTreeSet::new();
        for i in item_ids {
            if !reviewed.items().iter().any(|p| p.item_id.as_str() == i) {
                return Err(ScullError::new(
                    ErrorKind::PlanTampered,
                    format!("item {i} is not in the reviewed plan; nothing was changed"),
                ));
            }
            picked.insert(i.as_str());
        }
        if picked.is_empty() {
            return Err(ScullError::new(ErrorKind::Usage, "nothing was chosen"));
        }
        let plan = if picked.len() == reviewed.items().len() {
            reviewed
        } else {
            let items: Vec<PlanItem> = reviewed
                .items()
                .iter()
                .filter(|p| picked.contains(p.item_id.as_str()))
                .cloned()
                .collect();
            let watermark = reviewed.operations_watermark().ok_or_else(|| {
                ScullError::new(ErrorKind::PlanInvalidated, "the plan has no watermark")
            })?;
            // The reviewed plan's lifetime, not a fresh one (D-25): narrowing never extends it.
            let narrowed = desktop_plan(
                &items,
                reviewed.scan_id(),
                watermark,
                &who,
                Lifetime::of(&reviewed, now),
            )?;
            store.insert_plan(&narrowed).map_err(store_err)?;
            narrowed
        };
        let elevated = plan
            .items()
            .iter()
            .filter(|i| needs_elevated_confirmation(i.tier, &i.action))
            .count() as u64;
        if elevated > 0 && typed_count != Some(elevated) {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!(
                    "{elevated} item(s) can't be undone by scull: type {elevated} to confirm. \
                     Nothing was changed."
                ),
            ));
        }
        self.run_plan(session, &mut store, &plan, &basis, &who, now)
    }

    fn run_plan(
        &self,
        session: &Session,
        store: &mut Store,
        plan: &Plan,
        basis: &ScanBasis,
        who: &Who,
        now: i64,
    ) -> Result<DesktopRun, ScullError> {
        let executor = self.lock(store, now)?;
        let operation_id = OperationId::from_suffix(&new_id_suffix(now)).map_err(internal)?;
        let mut op = executor
            .begin_operation(
                store,
                plan,
                &OperationStart {
                    operation_id: operation_id.clone(),
                    plan_id: plan.id().clone(),
                    content_hash: plan.content_hash().clone(),
                    approver: who.user.clone(),
                    method: ApprovalMethod::Desktop,
                    started_at: now,
                },
            )
            .map_err(|mut error| {
                if error.kind == ErrorKind::PlanInvalidated {
                    error.message.push_str(". Run Scan again, then review a new cleanup plan. No items from this plan were cleaned.");
                }
                error
            })?
            .with_quarantine(QuarantineSettings::from_config(&basis.config.quarantine))
            .with_trash(self.trash_locations());
        let engine = session.engine_for(basis.config.clone(), &basis.roots)?;
        let revalidation = Revalidation::new(&engine, &basis.roots);
        let ctx = PlanContext {
            now_unix: now,
            machine_id: &who.machine_id,
            user: &who.user,
        };
        let report = match executor.execute_plan(
            plan,
            &ctx,
            &revalidation,
            Some(&mut op),
            &CancelToken::new(),
        ) {
            Ok(r) => r,
            Err(e) => {
                let _ = op.finish(OperationEnd::Cancelled, scull_engine::now_unix());
                return Err(e);
            }
        };
        let finished_at = scull_engine::now_unix();
        op.finish(
            if report.cancelled {
                OperationEnd::Cancelled
            } else {
                OperationEnd::Normal
            },
            finished_at,
        )?;
        let items: Vec<RunItem> = plan
            .items()
            .iter()
            .zip(&report.items)
            .map(|(p, r)| run_item(p, r))
            .collect();
        Ok(DesktopRun {
            plan_id: plan.id().as_str().to_owned(),
            operation_id: operation_id.as_str().to_owned(),
            moved_bytes: items.iter().map(|i| i.moved_bytes).sum(),
            predicted_bytes: plan.items().iter().map(|i| i.reclaimable_bytes).sum(),
            measured_delta_bytes: report.measurement.as_ref().and_then(|m| m.measured_total),
            cancelled: report.cancelled,
            finished_at,
            items,
        })
    }

    /// Restore the quarantined item `item_id` of operation `operation_id` (never overwrites:
    /// the original path when free, else a `.scull-restored-<n>` sibling; the manifest of
    /// record decides what goes back).
    pub fn restore(
        &self,
        session: &Session,
        operation_id: &str,
        item_id: &str,
        now: i64,
    ) -> Result<DesktopRestore, ScullError> {
        self.check_not_elevated()?;
        let op = OperationId::try_from(operation_id.to_owned())
            .map_err(|_| ScullError::new(ErrorKind::Usage, "not a scull operation ID"))?;
        let item = ItemId::try_from(item_id.to_owned())
            .map_err(|_| ScullError::new(ErrorKind::Usage, "not a scull item ID"))?;
        let mut store = Self::store(session)?;
        let entry = store
            .quarantine_entries(Some(&op))
            .map_err(store_err)?
            .into_iter()
            .find(|e| e.entry.item_id == item)
            .ok_or_else(|| {
                ScullError::new(
                    ErrorKind::NotFound,
                    format!("item {item_id} of operation {operation_id} is not in quarantine"),
                )
            })?;
        let executor = self.lock(&mut store, now)?;
        let report = executor.restore(&mut store, entry.entry_id, now)?;
        Ok(match report.outcome {
            RestoreOutcome::Restored { to, sibling } => DesktopRestore {
                restored_to: FsPath::from_path(&to).as_str().to_owned(),
                sibling,
                already: false,
            },
            RestoreOutcome::AlreadyRestored { to } => DesktopRestore {
                restored_to: to.as_str().to_owned(),
                sibling: false,
                already: true,
            },
            _ => {
                return Err(ScullError::new(
                    ErrorKind::Internal,
                    "unexpected restore outcome; see `scull history`",
                ));
            }
        })
    }

    fn cache_clean_context(
        &self,
        session: &Session,
    ) -> Result<
        (
            QuickClean,
            scull_engine::CachesReport,
            scull_detectors::caches::CacheScan,
            u64,
        ),
        ScullError,
    > {
        let setup = session.setup();
        let home = setup
            .home
            .clone()
            .ok_or_else(|| ScullError::new(ErrorKind::Config, "the home folder is not known"))?;
        let os = RulesOs::current().ok_or_else(|| {
            ScullError::new(ErrorKind::UnsupportedPlatform, "no cache rules for this OS")
        })?;
        let env_fn = Arc::clone(&setup.env);
        let env = CacheEnv::new(home, os, move |k| env_fn(k));
        let config = session.config()?;
        let watermark = session.operations_watermark().ok_or_else(|| {
            ScullError::new(
                ErrorKind::SafetyBlocked,
                "the operations watermark couldn't be read",
            )
        })?;
        let engine = session.engine_for(config.clone(), &[])?;
        let caches = engine.caches(
            &env,
            config.scan.threads as usize,
            &CancelToken::new(),
            |_| {},
        );
        let reader = FsConfigReader;
        let ctx = DetectContext::new().with_config_reader(&reader);
        let located = locate_caches(&env, &Native, &ctx);

        let mut opts = QuickCleanOptions::new((setup.env)("PATH").unwrap_or_default());
        opts.excluded_roots = config.scan.roots.clone();
        if let Some(scan) = session.last_scan() {
            opts.excluded_roots
                .extend(scan.roots.iter().map(FsPath::to_path_buf));
        }
        opts.sandbox_base = Some(sandbox_base(session)?);
        let qc = QuickClean::new(opts, Arc::new(Native)).with_elevation(elevation());
        Ok((qc, caches, located, watermark))
    }

    /// Preview both supported modes using the core planner and its actual location scoping.
    pub fn quick_clean_plan(&self, session: &Session) -> Result<DesktopCachePreview, ScullError> {
        let (qc, caches, located, _) = self.cache_clean_context(session)?;
        let probe = NativeActivityProbe::capture(Limits::default());
        let preferred = qc.plan(
            &located,
            &caches.candidates,
            &QuickCleanRequest::default(),
            &probe,
        );
        let wipe = qc.plan(
            &located,
            &caches.candidates,
            &QuickCleanRequest {
                wipe: CacheTool::ALL.into_iter().collect(),
                ..QuickCleanRequest::default()
            },
            &probe,
        );
        let tools = preferred
            .tools
            .iter()
            .map(|t| {
                let prune = (t.mode.to_string() == "prune")
                    .then(|| cache_command(&t.binary, &t.args, &t.env));
                let wipe = if t.mode.to_string() == "wipe" {
                    Some(cache_command(&t.binary, &t.args, &t.env))
                } else {
                    wipe.tools
                        .iter()
                        .find(|w| w.item_id == t.item_id && w.mode.to_string() == "wipe")
                        .map(|t| cache_command(&t.binary, &t.args, &t.env))
                };
                CacheCleanTool {
                    name: t.tool.artifact().to_owned(),
                    item_id: t.item_id.as_str().to_owned(),
                    tool: t.tool.rule_id().split('.').next().unwrap_or("").to_owned(),
                    cache: FsPath::from_path(&t.cache).as_str().to_owned(),
                    bytes: t.reclaimable_bytes,
                    prune,
                    wipe,
                    notes: t.notes.clone(),
                }
            })
            .collect();
        let skipped = preferred
            .skipped
            .iter()
            .map(|s| CacheCleanSkip {
                name: s.tool.artifact().to_owned(),
                tool: s.tool.rule_id().split('.').next().unwrap_or("").to_owned(),
                path: s
                    .path
                    .as_ref()
                    .map(|p| FsPath::from_path(p).as_str().to_owned()),
                bytes: caches
                    .candidates
                    .iter()
                    .find(|c| {
                        s.path.as_ref().is_some_and(|p| c.path.to_path_buf() == *p)
                            && c.rule.id == s.tool.rule_id()
                    })
                    .map(|c| c.sizes.reclaimable.bytes),
                reason: s.reason.to_string(),
            })
            .collect();
        Ok(DesktopCachePreview { tools, skipped })
    }

    /// Quick Clean the caches of the candidates `chosen` (IDs of package-cache candidates):
    /// each tool's own command, found only in trusted locations and run in a private sandbox
    /// (QC-01…04), as one audited operation. Orphaned caches (D-36) are never removed from
    /// here (each needs its own confirmation: `scull clean caches`).
    pub fn quick_clean(
        &self,
        session: &Session,
        chosen: &[String],
        now: i64,
    ) -> Result<DesktopQuickClean, ScullError> {
        self.quick_clean_requested(session, chosen, &[], None, now, None)
    }

    /// Quick Clean with live, operation-scoped output; the cleanup policy is unchanged.
    pub fn quick_clean_with_progress(
        &self,
        session: &Session,
        chosen: &[String],
        op_id: &OpId,
        now: i64,
        progress: &(dyn Fn(DesktopCacheProgress) + Send + Sync),
    ) -> Result<DesktopQuickClean, ScullError> {
        let state = Mutex::new(CacheProgress::new(op_id, chosen));
        progress(
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .current
                .clone(),
        );
        self.quick_clean_requested(session, chosen, &[], None, now, Some((&state, progress)))
    }

    /// Run only the chosen cache IDs and modes, after checking the reviewed commands still
    /// match a fresh safe plan. Execution uses the same audited runner as Overview.
    pub fn quick_clean_run(
        &self,
        session: &Session,
        choices: &[CacheCleanChoice],
        now: i64,
    ) -> Result<DesktopQuickClean, ScullError> {
        self.quick_clean_run_requested(session, choices, now, None)
    }

    /// Run the exact reviewed commands with live, operation-scoped output.
    pub fn quick_clean_run_with_progress(
        &self,
        session: &Session,
        choices: &[CacheCleanChoice],
        op_id: &OpId,
        now: i64,
        progress: &(dyn Fn(DesktopCacheProgress) + Send + Sync),
    ) -> Result<DesktopQuickClean, ScullError> {
        let chosen: Vec<_> = choices.iter().map(|c| c.item_id.clone()).collect();
        let state = Mutex::new(CacheProgress::new(op_id, &chosen));
        progress(
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .current
                .clone(),
        );
        self.quick_clean_run_requested(session, choices, now, Some((&state, progress)))
    }

    fn quick_clean_run_requested(
        &self,
        session: &Session,
        choices: &[CacheCleanChoice],
        now: i64,
        progress: Option<CacheProgressSink<'_>>,
    ) -> Result<DesktopQuickClean, ScullError> {
        if choices.is_empty() {
            return Err(ScullError::new(ErrorKind::Usage, "nothing was chosen"));
        }
        let mut seen = BTreeSet::new();
        for c in choices {
            if !matches!(c.mode.as_str(), "prune" | "wipe") || !seen.insert(&c.item_id) {
                return Err(ScullError::new(
                    ErrorKind::Usage,
                    "invalid or repeated cache choice",
                ));
            }
        }
        let chosen: Vec<String> = choices.iter().map(|c| c.item_id.clone()).collect();
        let wipe: Vec<String> = choices
            .iter()
            .filter(|c| c.mode == "wipe")
            .map(|c| c.item_id.clone())
            .collect();
        self.quick_clean_requested(session, &chosen, &wipe, Some(choices), now, progress)
    }

    fn quick_clean_requested(
        &self,
        session: &Session,
        chosen: &[String],
        wipe: &[String],
        reviewed: Option<&[CacheCleanChoice]>,
        now: i64,
        progress: Option<CacheProgressSink<'_>>,
    ) -> Result<DesktopQuickClean, ScullError> {
        self.check_not_elevated()?;
        let (qc, caches, located, watermark) = self.cache_clean_context(session)?;
        // The chips the human left ticked; every other tool is unticked (never run).
        let chosen: BTreeSet<&str> = chosen.iter().map(String::as_str).collect();
        let ticked_rules: BTreeSet<String> = caches
            .candidates
            .iter()
            .filter(|c| chosen.contains(c.id.as_str()))
            .map(|c| c.rule.id.clone())
            .collect();
        let mut request = QuickCleanRequest::default();
        for tool in CacheTool::ALL {
            if !ticked_rules.contains(tool.rule_id()) {
                request.unticked.insert(tool);
            }
        }
        for tool in CacheTool::ALL {
            if caches.candidates.iter().any(|c| {
                c.rule.id == tool.rule_id() && wipe.iter().any(|id| id.as_str() == c.id.as_str())
            }) {
                request.wipe.insert(tool);
            }
        }
        let probe = NativeActivityProbe::capture(Limits::default());
        let mut plan = qc.plan(&located, &caches.candidates, &request, &probe);
        // A tool can have several cache locations: selection is by exact item, never by rule.
        plan.tools.retain(|t| chosen.contains(t.item_id.as_str()));
        plan.orphaned.clear();
        if let Some(reviewed) = reviewed {
            for choice in reviewed {
                let unchanged = plan.tools.iter().any(|t| {
                    t.item_id.as_str() == choice.item_id
                        && t.mode.to_string() == choice.mode
                        && cache_command(&t.binary, &t.args, &t.env) == choice.command
                });
                if !unchanged {
                    let reason = plan
                        .skipped
                        .iter()
                        .map(|s| s.reason.to_string())
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(ScullError::new(
                        ErrorKind::PlanInvalidated,
                        format!(
                            "the selected cache or command changed; refresh the preview before cleaning. Nothing was changed. {reason}"
                        ),
                    ));
                }
            }
        }
        let emit = |event| {
            if let Some((state, callback)) = progress {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(mut event) = state.event(event) {
                    if event.kind == CacheProgressKind::Started {
                        event.command = plan
                            .tools
                            .iter()
                            .find(|t| Some(t.item_id.as_str()) == event.item_id.as_deref())
                            .map(|t| {
                                std::iter::once(t.binary.as_os_str())
                                    .chain(t.args.iter().map(|a| a.as_os_str()))
                                    .map(|part| escape_line(&part.to_string_lossy()))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            });
                    }
                    callback(event);
                }
            }
        };
        for candidate in &caches.candidates {
            if chosen.contains(candidate.id.as_str())
                && !plan.tools.iter().any(|t| t.item_id == candidate.id)
                && let Some(tool) = CacheTool::ALL
                    .into_iter()
                    .find(|t| t.rule_id() == candidate.rule.id)
            {
                let reason = plan
                    .skipped
                    .iter()
                    .find(|s| {
                        s.tool == tool
                            && s.path
                                .as_ref()
                                .is_none_or(|p| *p == candidate.path.to_path_buf())
                    })
                    .map(|s| s.reason.to_string())
                    .unwrap_or_else(|| "cache is no longer eligible".to_owned());
                emit(QuickCleanEvent::Skipped {
                    item_id: candidate.id.clone(),
                    tool,
                    reason,
                });
            }
        }
        let skipped: Vec<QuickCleanSkip> = plan
            .skipped
            .iter()
            .filter(|s| !request.unticked.contains(&s.tool))
            .map(|s| QuickCleanSkip {
                tool: s.tool.rule_id().split('.').next().unwrap_or("").to_owned(),
                reason: s.reason.to_string(),
            })
            .collect();
        if plan.tools.is_empty() {
            return Ok(DesktopQuickClean {
                operation_id: None,
                tools: vec![],
                skipped,
                predicted_bytes: 0,
                measured_delta_bytes: None,
            });
        }
        let who = self.who(session)?;
        let mut store = Self::store(session)?;
        let executor = self.lock(&mut store, now)?;
        let audit_plan = plan.audit_plan(
            &BTreeSet::new(),
            PlanParams {
                id: PlanId::from_suffix(&new_id_suffix(now)).map_err(internal)?,
                scan_id: ScanId::from_suffix(&new_id_suffix(now)).map_err(internal)?,
                created_unix: now,
                ttl_secs: None,
                machine_id: who.machine_id.clone(),
                user: who.user.clone(),
                origin: PlanOrigin::Desktop,
                operations_watermark: Some(watermark),
            },
        )?;
        store.insert_plan(&audit_plan).map_err(store_err)?;
        let operation_id = OperationId::from_suffix(&new_id_suffix(now)).map_err(internal)?;
        let mut op = executor.begin_operation(
            &mut store,
            &audit_plan,
            &OperationStart {
                operation_id: operation_id.clone(),
                plan_id: audit_plan.id().clone(),
                content_hash: audit_plan.content_hash().clone(),
                approver: who.user.clone(),
                method: ApprovalMethod::Desktop,
                started_at: now,
            },
        )?;
        let fresh = NativeActivityProbe::capture(Limits::default());
        let ran = match qc.run_with_progress(&mut op, &plan, &fresh, &CancelToken::new(), &emit) {
            Ok(r) => r,
            Err(e) => {
                let _ = op.finish(OperationEnd::Cancelled, scull_engine::now_unix());
                return Err(e);
            }
        };
        op.finish_measured(
            if ran.cancelled {
                OperationEnd::Cancelled
            } else {
                OperationEnd::Normal
            },
            scull_engine::now_unix(),
            ran.measured_delta_bytes,
        )?;
        let id_of = |r: &ToolResult| {
            caches
                .candidates
                .iter()
                .find(|c| c.rule.id == r.tool.rule_id() && c.path.to_path_buf() == r.cache)
                .map(|c| c.id.as_str().to_owned())
        };
        Ok(DesktopQuickClean {
            operation_id: Some(operation_id.as_str().to_owned()),
            tools: ran
                .results
                .iter()
                .map(|r| QuickCleanTool {
                    item_id: id_of(r),
                    tool: r.tool.rule_id().split('.').next().unwrap_or("").to_owned(),
                    succeeded: r.succeeded() && matches!(r.ended, Ended::Exited { .. }),
                    cache_bytes_freed: r.cache_bytes_freed().and_then(|b| i64::try_from(b).ok()),
                    explanation: quick_clean_explanation(r),
                })
                .collect(),
            skipped: ran
                .skipped
                .iter()
                .filter(|s| !request.unticked.contains(&s.tool))
                .map(|s| QuickCleanSkip {
                    tool: s.tool.rule_id().split('.').next().unwrap_or("").to_owned(),
                    reason: s.reason.to_string(),
                })
                .collect(),
            predicted_bytes: plan.total_reclaimable(),
            measured_delta_bytes: ran.measured_delta_bytes,
        })
    }
}

fn quick_clean_explanation(result: &ToolResult) -> Option<String> {
    if result.succeeded() {
        return result.explanation.clone();
    }
    let status = match &result.ended {
        Ended::Exited(Some(code)) => format!("Exited with status {code}"),
        Ended::Exited(None) => "Terminated without an exit status".into(),
        Ended::TimedOut(_) => "Timed out".into(),
        Ended::Cancelled => "Cancelled".into(),
        Ended::SpawnFailed(reason) => format!("Could not start: {}", escape_line(reason)),
    };
    let output = if result.stderr.is_empty() {
        &result.stdout
    } else {
        &result.stderr
    };
    let mut explanation = if output.is_empty() {
        status
    } else {
        format!("{status}: {}", output.lines.join("\n"))
    };
    let truncated = explanation.len() > 2048;
    if truncated {
        let mut end = 2048;
        while !explanation.is_char_boundary(end) {
            end -= 1;
        }
        explanation.truncate(end);
    }
    if truncated || output.truncated {
        explanation.push_str(" [output truncated]");
    }
    Some(explanation)
}

fn cache_command(
    binary: &std::path::Path,
    args: &[std::ffi::OsString],
    env: &[(String, std::ffi::OsString)],
) -> CacheCleanCommand {
    CacheCleanCommand {
        binary: binary.to_string_lossy().into_owned(),
        args: args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        env: env
            .iter()
            .map(|(k, v)| CacheCleanEnv {
                name: k.clone(),
                value: v.to_string_lossy().into_owned(),
            })
            .collect(),
    }
}

/// When a desktop plan is made and how long it lives.
struct Lifetime {
    /// Now, for the plan's ID.
    now: i64,
    created_unix: i64,
    /// `None`: the desktop default (D-25: 1 hour).
    ttl_secs: Option<i64>,
}

impl Lifetime {
    /// A new plan: created now, the default lifetime.
    fn fresh(now: i64) -> Self {
        Self {
            now,
            created_unix: now,
            ttl_secs: None,
        }
    }

    /// A plan narrowed from `reviewed`: the same creation time and expiry.
    fn of(reviewed: &Plan, now: i64) -> Self {
        Self {
            now,
            created_unix: reviewed.created_unix(),
            ttl_secs: Some(reviewed.expires_unix() - reviewed.created_unix()),
        }
    }
}

/// A desktop plan of `items` from scan `scan_id`.
fn desktop_plan(
    items: &[PlanItem],
    scan_id: &ScanId,
    watermark: u64,
    who: &Who,
    life: Lifetime,
) -> Result<Plan, ScullError> {
    Plan::new(
        PlanParams {
            id: PlanId::from_suffix(&new_id_suffix(life.now)).map_err(internal)?,
            scan_id: scan_id.clone(),
            created_unix: life.created_unix,
            ttl_secs: life.ttl_secs,
            machine_id: who.machine_id.clone(),
            user: who.user.clone(),
            origin: PlanOrigin::Desktop,
            operations_watermark: Some(watermark),
        },
        items.to_vec(),
    )
}

fn run_item(p: &PlanItem, r: &ItemReport) -> RunItem {
    let reason = match &r.outcome {
        ItemOutcome::Refused(e) if p.action == Action::GitWorktreeRemove => Some(e.message.clone()),
        ItemOutcome::WorktreeRemovalFailed { reason } => Some(reason.clone()),
        _ => r.reason_code().map(str::to_owned),
    };
    let (outcome, freed, moved) = match &r.outcome {
        ItemOutcome::Deleted(_) if r.succeeded() => {
            (RunItemOutcome::Deleted, p.reclaimable_bytes, 0)
        }
        ItemOutcome::WorktreeRemoved { freed_bytes } => (RunItemOutcome::Deleted, *freed_bytes, 0),
        ItemOutcome::Quarantined(q) => (RunItemOutcome::Quarantined, 0, q.bytes),
        ItemOutcome::Trashed(_) => (RunItemOutcome::Trashed, 0, p.reclaimable_bytes),
        _ => (RunItemOutcome::Skipped, 0, 0),
    };
    RunItem {
        item_id: p.item_id.as_str().to_owned(),
        outcome,
        reason,
        freed_bytes: freed,
        moved_bytes: moved,
    }
}

/// The private folder Quick Clean's tool sandbox is made in (owner-only), under scull's state
/// directory, or the system temp folder when scull's directories aren't known.
fn sandbox_base(session: &Session) -> Result<PathBuf, ScullError> {
    let Some(data) = session.setup().data_dir.as_deref() else {
        return Ok(std::env::temp_dir());
    };
    let dir = data.join(QUICK_CLEAN_SANDBOX);
    scull_platform::create_dir_owner_only(&dir).map_err(|e| {
        ScullError::new(
            ErrorKind::Io,
            format!("cannot create the Quick Clean sandbox folder: {e}"),
        )
    })?;
    scull_platform::ensure_owner_only(&dir).map_err(|e| {
        ScullError::new(
            ErrorKind::SafetyBlocked,
            format!("the Quick Clean sandbox folder is not private: {e}"),
        )
    })?;
    Ok(dir)
}

/// Whether the history database exists (the desk creates it on the first run, like the CLI).
pub fn has_database(session: &Session) -> bool {
    session
        .setup()
        .data_dir
        .as_deref()
        .is_some_and(|d| history_db_path(d).is_file())
}

#[cfg(test)]
mod failure_output_tests {
    use super::*;
    use scull_cleaner::quickclean::{CapturedOutput, CleanMode};
    use std::time::Duration;

    #[test]
    fn cache_progress_counts_selected_terminal_events_once() {
        let first = ItemId::derive(scull_core::FileIdentity::new(1, 1), b"first");
        let second = ItemId::derive(scull_core::FileIdentity::new(1, 2), b"second");
        let other = ItemId::derive(scull_core::FileIdentity::new(1, 3), b"unticked");
        let chosen = vec![
            first.as_str().to_owned(),
            second.as_str().to_owned(),
            first.as_str().to_owned(),
        ];
        let mut state = CacheProgress::new(&OpId::parse("cleanup-1").unwrap(), &chosen);
        assert_eq!(state.current.total, 2);
        assert_eq!(state.current.completed, 0);
        let start = state
            .event(QuickCleanEvent::Started {
                item_id: first.clone(),
                tool: CacheTool::Bun,
            })
            .unwrap();
        assert_eq!(start.completed, 0);
        assert_eq!(start.item_id.as_deref(), Some(first.as_str()));
        let output = state
            .event(QuickCleanEvent::Output {
                item_id: first.clone(),
                tool: CacheTool::Bun,
                stream: OutputStream::Stderr,
                line: "escaped\\x1btext".into(),
            })
            .unwrap();
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(json["op_id"], "cleanup-1");
        assert_eq!(json["kind"], "output");
        assert_eq!(json["stream"], "stderr");
        assert!(json.get("reason").is_none());
        assert_eq!(output.completed, 0);
        let truncated = state
            .event(QuickCleanEvent::OutputTruncated {
                item_id: first.clone(),
                tool: CacheTool::Bun,
                stream: OutputStream::Stdout,
            })
            .unwrap();
        assert_eq!(truncated.kind, CacheProgressKind::OutputTruncated);
        assert_eq!(truncated.stream, Some(CacheOutputStream::Stdout));
        assert!(truncated.line.is_none());
        assert_eq!(truncated.completed, 0);
        let finished = state
            .event(QuickCleanEvent::Finished {
                item_id: first.clone(),
                tool: CacheTool::Bun,
                succeeded: false,
            })
            .unwrap();
        assert_eq!(finished.completed, 1);
        assert_eq!(finished.succeeded, Some(false));
        assert!(
            state
                .event(QuickCleanEvent::Finished {
                    item_id: first.clone(),
                    tool: CacheTool::Bun,
                    succeeded: true
                })
                .is_none()
        );
        assert!(
            state
                .event(QuickCleanEvent::Started {
                    item_id: other,
                    tool: CacheTool::Bun
                })
                .is_none()
        );
        let skipped = state
            .event(QuickCleanEvent::Skipped {
                item_id: second.clone(),
                tool: CacheTool::Bun,
                reason: "in use".into(),
            })
            .unwrap();
        assert_eq!(skipped.completed, 2);
        assert_eq!(skipped.total, 2);
        assert_eq!(skipped.reason.as_deref(), Some("in use"));
        assert!(skipped.succeeded.is_none());
        assert!(
            state
                .event(QuickCleanEvent::Skipped {
                    item_id: second,
                    tool: CacheTool::Bun,
                    reason: "duplicate".into()
                })
                .is_none()
        );
    }

    #[test]
    fn failed_tools_explain_exit_and_capped_escaped_output() {
        let mut result = ToolResult {
            tool: CacheTool::Bun,
            cache: PathBuf::from("/cache"),
            binary: PathBuf::from("/bin/bun"),
            args: vec![],
            mode: CleanMode::Wipe,
            ended: Ended::Exited(Some(1)),
            stdout: CapturedOutput::default(),
            stderr: CapturedOutput {
                lines: vec!["\\x1b[31mfailed".repeat(1000)],
                truncated: true,
            },
            elapsed: Duration::ZERO,
            free_before: None,
            free_after: None,
            size_before: None,
            size_after: None,
            explanation: None,
        };
        let failure = quick_clean_explanation(&result).expect("failure details");
        assert!(failure.starts_with("Exited with status 1:"), "{failure}");
        assert!(failure.contains("\\x1b[31mfailed"));
        assert!(failure.ends_with("[output truncated]"));
        assert!(failure.len() < 2200);
        result.stderr.lines = vec!["文".repeat(1000)];
        let failure = quick_clean_explanation(&result).unwrap();
        assert!(failure.ends_with("[output truncated]") && failure.len() < 2200);
        result.stderr = CapturedOutput::default();
        result.stdout.lines = vec!["stdout failure".into()];
        assert!(
            quick_clean_explanation(&result)
                .unwrap()
                .contains("stdout failure")
        );
        result.ended = Ended::Exited(Some(0));
        result.explanation = Some("Referenced files kept".into());
        assert_eq!(quick_clean_explanation(&result), result.explanation);
        result.ended = Ended::SpawnFailed("bad\u{001b}binary".into());
        let failure = quick_clean_explanation(&result).unwrap();
        assert!(failure.contains("bad\\x1bbinary"), "{failure}");
        assert!(!failure.contains('\u{001b}'));
    }
}
