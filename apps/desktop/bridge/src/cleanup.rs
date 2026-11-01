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
//!   (D-28: delete S0 only, S1/S2 quarantine or Trash only, S3 never, no tool or Git action).
//! * **The human's confirmation in the main window** (the Confirm button, sent as the
//!   reviewed plan ID, hash and the exact items chosen; recorded with `ApprovalMethod::Desktop`
//!   and the OS user as approver). An item whose action scull can't undo at S1/S2
//!   (SAFE-07, `needs_elevated_confirmation`) additionally needs the typed count; D-28 plans
//!   never contain one, but the rule is enforced on every run.
//! * **The Executor under the execution lock** (SAFE-09), after reconciling any interrupted
//!   operation (SAFE-19): the operation is refused if another touched the items after the scan
//!   (watermark); every item is revalidated against the disk right before it is touched
//!   (SAFE-05); everything is journaled and audited (SAFE-08).
//!
//! Commands are invoked by the main window only (`capabilities/default.json`; the menu bar
//! widget can't) and none of this is reachable from MCP (`scull-mcp` doesn't depend on this
//! crate).

use crate::session::{ScanBasis, Session};
use schemars::JsonSchema;
use scull_cleaner::quickclean::{
    Ended, QuickClean, QuickCleanOptions, QuickCleanRequest, ToolResult,
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
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// Test hook (same as the CLI's): when set, the desktop behaves as if it ran elevated. It can
/// only add a refusal, never remove one.
pub const ASSUME_ELEVATED_ENV: &str = "SCULL_TEST_ASSUME_ELEVATED";

/// Folder under scull's state directory for Quick Clean's private tool sandbox (as the CLI).
const QUICK_CLEAN_SANDBOX: &str = "quickclean";

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

/// A tool Quick Clean didn't run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuickCleanSkip {
    /// The tool.
    pub tool: String,
    /// Why, for display.
    pub reason: String,
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

/// D-28 for desktop plans: an actionable tier and a delete or quarantine its tier allows (no
/// tool or Git action, which `scull clean` doesn't run either). `Some(reason)` when not.
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
                "there is no complete scan to plan from yet: scan first",
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
                        "plan {id} was made from an earlier scan than the app's last one; \
                         review a new plan. Nothing was changed."
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
            )?
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
        self.check_not_elevated()?;
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
        opts.sandbox_base = Some(sandbox_base(session)?);
        let qc = QuickClean::new(opts, Arc::new(Native)).with_elevation(elevation());
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
        let probe = NativeActivityProbe::capture(Limits::default());
        let mut plan = qc.plan(&located, &caches.candidates, &request, &probe);
        plan.orphaned.clear();
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
        let ran = match qc.run(&mut op, &plan, &fresh, &CancelToken::new()) {
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
                })
                .collect(),
            skipped,
            predicted_bytes: plan.total_reclaimable(),
            measured_delta_bytes: ran.measured_delta_bytes,
        })
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
    let reason = r.reason_code().map(str::to_owned);
    let (outcome, freed, moved) = match &r.outcome {
        ItemOutcome::Deleted(_) if r.succeeded() => {
            (RunItemOutcome::Deleted, p.reclaimable_bytes, 0)
        }
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
