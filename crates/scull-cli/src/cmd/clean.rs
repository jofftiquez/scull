//! `scull clean` (P2-CLI-01; PRD §10, §12, CLI-03, SAFE-02, SAFE-07, SAFE-09, SAFE-11,
//! SAFE-14): plan a cleanup (dry run by default), review a plan file, execute it.
//!
//! # Planning: `scull clean [--root PATH]... [--include-s1 | --item ID|PATH...] [--output FILE]`
//!
//! Scans the roots with the read-only engine (recorded in history), selects what the tier
//! table allows by default (PRD §8.2): **S0** items (preselected), **S1** items with
//! `--include-s1` (opt-in), **S2** items only when named one by one with `--item`. Never S3,
//! never an item without an action. Package caches are left to `scull clean caches` (their own
//! tools, QC-03) and Git worktree operations are not executed by this build; both are listed
//! as excluded with the reason. The selection becomes a [`Plan`] bound to this machine and
//! user, expiring in 24 hours (SAFE-11), recorded in the database and, with `--output`,
//! written owner-only to a file. Nothing on disk changes.
//!
//! The preview shows, for every item (§10.2): exact path and owning project, artifact type,
//! rule and detection evidence, logical / allocated / reclaimable bytes with confidence, tier
//! and every reason, regeneration instructions and cost (or "unknown"), the active /
//! hardlinked / cross-mount / symlink / cloud-placeholder flags (`unknown` when not checked),
//! the action and whether and how it can be undone. Tool actions would also show the resolved
//! binary; `scull clean` plans have none (Quick Clean shows them, `scull clean caches`).
//!
//! On a terminal, without `--dry-run`, the preview ends with "Execute this plan now? [y/N]".
//!
//! # Reviewing and executing: `scull clean --plan FILE [--confirm] [--allow-permanent]`
//!
//! 1. **Refuse elevated runs** (SAFE-14, FX-EXEC-12) unless `--allow-root`, before anything.
//! 2. **Verify** the plan file ([`Plan::verify`]): edited, expired, foreign (another machine
//!    or user) or unknown-version plans are rejected with exit code 4 (FX-EXEC-07…09). It
//!    must also match the plan recorded in this machine's database by ID and content hash
//!    (the hash is not a secret, so the database record is the authority), and come from the
//!    CLI (desktop and agent plans are approved where they were made).
//! 3. **Re-derive** candidates with a fresh scan of the plan's roots (`--root`, else the roots
//!    recorded for the plan's scan, else the config) and [`narrow`] the plan: the preview
//!    shows what would run now and what dropped out (gone, replaced, tier raised...).
//! 4. **Confirm**: `--confirm`, or `y` at the prompt on a terminal (never without one,
//!    CLI-03). S1/S2 items whose action can't be undone also need the elevated confirmation:
//!    typing their count on a terminal, `--allow-permanent` without one (SAFE-07,
//!    FX-EXEC-11; see [`super::exec`] for which items that is under D-28).
//! 5. **Execute** under the per-user execution lock (exit 6 if another operation holds it,
//!    SAFE-09), after reconciling any operation a crash interrupted (SAFE-19), as one recorded,
//!    journaled operation ([`scull_cleaner::Executor::begin_operation`] refuses a plan whose
//!    targets another operation touched since it was made, exit 4). Every item is revalidated
//!    right before it is touched ([`scull_cleaner::Executor::execute_plan`]): S0 items are
//!    deleted, S1/S2 items moved into same-volume quarantine; failures are skipped and
//!    reported item by item (exit 3), never "best effort". Ctrl-C stops before the next item
//!    (exit 130).
//! 6. **Report** per item, with the measured free-space delta next to the prediction
//!    (PHY-02) and how to restore quarantined items. The same delta is recorded with the
//!    finished operation (`measured_delta_bytes`, invariant I-5; `scull history` shows it).
//!
//! Machine output: `scull/clean-plan@1` (plan and review previews) and `scull/clean-run@1`
//! (an execution).

use super::exec::{self, ALLOW_PERMANENT_FLAG, Who};
use super::pipeline::{self, CancelBridge, Loaded};
use super::render::{action_label, display_path, signals_section};
use super::{emit_machine, emit_text, json_name};
use crate::cancel;
use crate::cli::CleanArgs;
use crate::output::Ui;
use crate::output::bytes::{confidence_label, format_bytes};
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use crate::output::style::{Style, tier_text};
use crate::output::table::{Align, Cell, Table};
use schemars::JsonSchema;
use scull_cleaner::measure::{
    ItemExecution, MeasureOptions, PlatformFreeSpace, Tally, VolumeMeter,
};
use scull_cleaner::{
    DEFAULT_RETENTION_SECS, DOCKER_UNSUPPORTED, ExecutionReport, ItemOutcome, ItemReport,
    PlanContext, RemoveOutcome, Revalidation, Skip, TOOL_NATIVE_UNSUPPORTED, is_docker_command,
    is_quick_clean_tool,
};
use scull_core::{
    Action, Candidate, Category, ContentHash, DropReason, ErrorKind, Evidence, FsPath, ItemId,
    OperationId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams, RegenerationCost, Restorability,
    ScanId, ScullError, Sizes, Tier, action_allowed_for, narrow,
};
use scull_engine::{Engine, ScanReport, ScanRequest, SignalUnavailable, now_unix};
use scull_scanner::ActionReclaim;
use scull_storage::{ApprovalMethod, OperationEnd, OperationStart, Store};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Schema of the plan and review previews (`--json`).
pub const CLEAN_PLAN_SCHEMA: SchemaId = SchemaId::new("clean-plan", 1);
/// Schema of an execution report (`--json`).
pub const CLEAN_RUN_SCHEMA: SchemaId = SchemaId::new("clean-run", 1);

/// Largest plan file read (a plan of a million items is far smaller).
const MAX_PLAN_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Excluded items listed in the human preview before "... and N more".
const EXCLUDED_LISTED: usize = 20;

// ---------------------------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------------------------

/// Which preview this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewMode {
    /// A new plan from a fresh scan (`scull clean`).
    Plan,
    /// An existing plan file checked against a fresh scan (`scull clean --plan FILE`).
    Review,
}

/// `scull clean` / `scull clean --plan FILE` preview (`scull/clean-plan@1`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanPlanReport {
    /// Plan or review.
    pub mode: PreviewMode,
    /// The fresh scan the preview was built from.
    pub scan_id: ScanId,
    /// Its roots.
    pub roots: Vec<FsPath>,
    /// The plan (exactly what `--output` writes and `--plan` reads). `None` when nothing was
    /// selected.
    pub plan: Option<Plan>,
    /// Where the plan was written, if anywhere.
    pub plan_file: Option<FsPath>,
    /// What would be executed now, in plan order.
    pub items: Vec<PreviewItem>,
    /// Review: plan items that no longer qualify (they are skipped at execution).
    pub dropped: Vec<DroppedItem>,
    /// Plan: candidates left out of the plan, and why.
    pub excluded: Vec<ExcludedItem>,
    /// Totals of `items`.
    pub totals: PreviewTotals,
    /// The elevated confirmation (SAFE-07) the items need.
    pub elevated_confirmation: ElevatedConfirmation,
    /// Signals that could not be observed (affected items are classified as if risky).
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Whether the scan was cancelled (the preview is partial; no plan is made).
    pub cancelled: bool,
    /// What to run next, if anything.
    pub next_step: Option<String>,
}

/// One item of a preview: the plan item as it would run, the fresh candidate it matches, and
/// what undoing it takes.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewItem {
    /// The plan item (tier, action and estimate as they would execute now).
    pub item: PlanItem,
    /// The fresh candidate: path, project, artifact, rule, reasons, evidence, sizes, flags,
    /// regeneration.
    pub candidate: Candidate,
    /// How the action can be undone.
    pub restorability: Restorability,
    /// The same, as a sentence.
    pub undo: String,
    /// Resolved tool binary for tool actions (SAFE-12); `None` for filesystem actions.
    pub tool_binary: Option<FsPath>,
    /// Whether the item needs the elevated confirmation (SAFE-07).
    pub needs_elevated_confirmation: bool,
}

/// A plan item that no longer qualifies.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DroppedItem {
    /// The item.
    pub item_id: ItemId,
    /// Its planned path.
    pub path: FsPath,
    /// Tier at planning time.
    pub planned_tier: Tier,
    /// Why it dropped out.
    pub reason: DropReason,
    /// The same, as a sentence.
    pub message: String,
}

/// Why a candidate is not in the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExcludeReason {
    /// S3: never cleaned.
    Protected,
    /// No action at its tier (inspect only).
    NoAction,
    /// A package cache: `scull clean caches` cleans it with its own tool.
    PackageCache,
    /// An action this build's `scull clean` does not execute (Git worktree operations,
    /// tool-native ML removal such as `ollama rm`).
    NotSupported,
    /// S1: opt in with `--include-s1` or `--item`.
    OptInS1,
    /// S2: only planned when named with `--item`.
    PerItemS2,
    /// `--item` was given and this item was not named.
    NotSelected,
}

/// A candidate left out of the plan.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExcludedItem {
    /// The item.
    pub item_id: ItemId,
    /// Its path.
    pub path: FsPath,
    /// Its tier.
    pub tier: Tier,
    /// Its proposed action.
    pub action: Action,
    /// Reclaimable bytes.
    pub reclaimable_bytes: u64,
    /// Why it was left out.
    pub reason: ExcludeReason,
    /// The same, as a sentence.
    pub message: String,
}

/// Totals of a preview's items.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreviewTotals {
    /// Items.
    pub items: u64,
    /// Items to delete (S0).
    pub delete: u64,
    /// Items to quarantine (S1/S2).
    pub quarantine: u64,
    /// Items to move to the Trash.
    pub trash: u64,
    /// Estimated reclaimable bytes of all items.
    pub reclaimable_bytes: u64,
    /// Bytes freed when the run completes (deleted items).
    pub freed_now_bytes: u64,
    /// Bytes moved into quarantine or the Trash: freed only when purged / emptied.
    pub moved_bytes: u64,
}

/// The elevated confirmation a preview needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElevatedConfirmation {
    /// Whether any item needs it.
    pub required: bool,
    /// How many items need it.
    pub items: u64,
    /// How to give it.
    pub how: Option<String>,
}

/// `scull clean --plan FILE --confirm` result (`scull/clean-run@1`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanRunReport {
    /// The executed plan.
    pub plan_id: PlanId,
    /// Its content hash (what the audit log binds to).
    pub content_hash: ContentHash,
    /// The recorded operation (`scull history`, `scull restore`).
    pub operation_id: OperationId,
    /// Who approved it.
    pub approver: String,
    /// How the run was confirmed: `flag` (`--confirm`) or `prompt`.
    pub confirmation: String,
    /// How the elevated confirmation was given, if needed: `typed` or `flag`.
    pub elevated_confirmation: Option<String>,
    /// When execution started (Unix seconds).
    pub started_at: i64,
    /// When it finished.
    pub finished_at: i64,
    /// Whether Ctrl-C stopped it before every item was attempted.
    pub cancelled: bool,
    /// One entry per plan item, in plan order.
    pub items: Vec<RunItem>,
    /// Totals.
    pub totals: RunTotals,
    /// Predicted and measured bytes (PHY-02).
    pub measured: Measured,
    /// How to restore quarantined items, if any.
    pub restore_hint: Option<String>,
}

/// What happened to one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// Deleted completely.
    Deleted,
    /// Partly deleted: some entries were kept (each with a reason) or the walk stopped.
    PartlyDeleted,
    /// Moved into quarantine (restorable).
    Quarantined,
    /// Moved to the Trash because quarantine was impossible here (D-3); restorable from the
    /// Trash, outside scull.
    Trashed,
    /// Failed revalidation; nothing was touched.
    Skipped,
    /// Refused by the Executor; nothing was removed.
    Refused,
    /// Its action is impossible here and there is no fallback; left in place.
    Blocked,
    /// Not attempted (cancelled).
    NotRun,
}

/// One item of a run.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunItem {
    /// The item.
    pub item_id: ItemId,
    /// Its planned path.
    pub path: FsPath,
    /// Tier at planning time.
    pub planned_tier: Tier,
    /// Tier at revalidation, when re-derived.
    pub fresh_tier: Option<Tier>,
    /// The planned action.
    pub action: Action,
    /// What happened.
    pub outcome: RunOutcome,
    /// Stable reason code when it did not fully succeed.
    pub reason_code: Option<String>,
    /// Why, in words.
    pub message: Option<String>,
    /// Allocated bytes removed.
    pub removed_bytes: u64,
    /// Bytes moved into quarantine.
    pub moved_bytes: u64,
    /// Where a quarantined item is now.
    pub quarantine_path: Option<FsPath>,
    /// Its quarantine manifest entry.
    pub quarantine_entry: Option<i64>,
    /// Where a trashed item is now, when known.
    pub trash_path: Option<FsPath>,
    /// How what was done can be undone.
    pub restorability: Restorability,
}

/// Totals of a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunTotals {
    /// Items that fully succeeded.
    pub succeeded: u64,
    /// Items partly deleted.
    pub partial: u64,
    /// Items skipped at revalidation.
    pub skipped: u64,
    /// Items refused.
    pub refused: u64,
    /// Items blocked.
    pub blocked: u64,
    /// Items not attempted.
    pub not_run: u64,
    /// Allocated bytes removed.
    pub removed_bytes: u64,
    /// Bytes moved into quarantine.
    pub moved_bytes: u64,
}

/// Predicted and measured bytes of a run (PHY-02).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Measured {
    /// Predicted bytes freed when the run completes.
    pub predicted_freed_bytes: u64,
    /// Predicted bytes moved into quarantine.
    pub predicted_moved_bytes: u64,
    /// Bytes removed.
    pub removed_bytes: u64,
    /// Bytes moved.
    pub moved_bytes: u64,
    /// Measured free-space delta over the affected volumes (`None` if unmeasurable).
    pub measured_delta_bytes: Option<i64>,
    /// `(measured - predicted) / predicted` in percent.
    pub discrepancy_pct: Option<f64>,
    /// Predicted and measured differ by more than 10%.
    pub exceeds_threshold: bool,
    /// The difference is below what free-space reporting resolves.
    pub within_resolution: bool,
    /// Explanations (always present when `exceeds_threshold`).
    pub explanations: Vec<Explanation>,
}

/// One explanation of a measured result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Explanation {
    /// Stable code.
    pub code: String,
    /// In words.
    pub message: String,
}

// ---------------------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------------------

/// Run `scull clean` (without a subcommand).
pub fn run(args: &CleanArgs, ui: &Ui) -> Result<(), ScullError> {
    match &args.plan {
        None => plan_cmd(args, ui),
        Some(file) => plan_file_cmd(file, args, ui),
    }
}

/// How the run was confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirmed {
    Flag,
    Prompt,
}

/// `scull clean`: plan from a fresh scan.
fn plan_cmd(args: &CleanArgs, ui: &Ui) -> Result<(), ScullError> {
    let mut loaded = pipeline::load_config(&args.roots)?;
    loaded.allow_network_roots = args.allow_network_roots;
    let roots = pipeline::roots(&loaded, args.allow_broad_roots)?;
    let who = exec::who()?;
    let engine = pipeline::engine(&loaded, &roots, true, ui)?;
    let scan = scan(&engine, &roots, &loaded, true, ui)?;
    if scan.cancelled || cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    let cwd = std::env::current_dir().map_err(|e| ScullError::from_io(&e))?;
    let selection = select(&scan.candidates, args.include_s1, &args.items, &cwd)?;
    let now = now_unix();
    let plan = if selection.items.is_empty() {
        None
    } else {
        let items = selection
            .items
            .iter()
            .map(|c| PlanItem::from_candidate(c))
            .collect::<Result<Vec<_>, _>>()?;
        Some(Plan::new(
            PlanParams {
                id: exec::new_plan_id(now)?,
                scan_id: scan.scan_id.clone(),
                created_unix: now,
                ttl_secs: None,
                machine_id: who.machine_id.clone(),
                user: who.user.clone(),
                origin: PlanOrigin::Cli,
            },
            items,
        )?)
    };
    let mut store = None;
    let mut plan_file = None;
    if let Some(p) = &plan {
        let mut s = exec::open_store()?;
        s.insert_plan(p)?;
        store = Some(s);
        if let Some(out) = &args.output {
            write_plan_file(out, p)?;
            plan_file = Some(FsPath::from_path(&absolute(&cwd, out)));
        }
    }
    let by_id: BTreeMap<&ItemId, &Candidate> = scan.candidates.iter().map(|c| (&c.id, c)).collect();
    let items = plan
        .as_ref()
        .map(|p| preview_items(p.items(), &by_id))
        .unwrap_or_default();
    let mut report = preview_report(PreviewMode::Plan, &scan, plan.clone(), plan_file, items);
    report.excluded = selection.excluded;
    let offer = plan.is_some() && !args.dry_run && exec::interactive(ui);
    let elevation = exec::elevation();
    let blocked = elevation.blocks_destructive() && !args.allow_root;
    report.next_step = plan
        .as_ref()
        .map(|p| next_step(p, report.plan_file.as_ref()));
    emit_preview(&report, ui)?;
    let (Some(plan), true) = (plan, offer) else {
        return Ok(());
    };
    if blocked {
        info(
            ui,
            &format!(
                "Not offering to execute: scull is running as root/administrator (SAFE-14). Run \
                 it as your normal user, or pass {} to the execution.",
                exec::ALLOW_ROOT_FLAG
            ),
        );
        return Ok(());
    }
    if !exec::prompt_yes_no(&format!(
        "Execute this plan now ({})?",
        summary(&report.totals)
    ))? {
        info(ui, "Not executed. The plan is kept until it expires.");
        return Ok(());
    }
    let elevated = elevated_gate(&report, args.allow_permanent, true)?;
    let mut store = match store {
        Some(s) => s,
        None => exec::open_store()?,
    };
    let sizes = sizes_by_id(&scan.candidates);
    let run = execute(
        &plan,
        &loaded,
        &who,
        &roots,
        &engine,
        &mut store,
        &sizes,
        Confirmed::Prompt,
        elevated,
        ui,
    )?;
    finish_run(&run, ui)
}

/// `scull clean --plan FILE [--confirm]`: review, then execute when confirmed.
fn plan_file_cmd(file: &Path, args: &CleanArgs, ui: &Ui) -> Result<(), ScullError> {
    let wants_execution = args.confirm || (!args.dry_run && exec::interactive(ui));
    if wants_execution {
        exec::check_not_elevated(exec::elevation(), args.allow_root, "scull clean")?;
    }
    let plan = read_plan_file(file)?;
    let who = exec::who()?;
    let now = now_unix();
    plan.verify(now, &who.machine_id, &who.user)?;
    if plan.origin() != PlanOrigin::Cli {
        return Err(ScullError::new(
            ErrorKind::PlanForeign,
            format!(
                "this plan was made by the {} and must be approved there; make a CLI plan with \
                 `scull clean --output FILE`",
                match plan.origin() {
                    PlanOrigin::Mcp => "MCP server (an AI agent)",
                    PlanOrigin::Desktop => "desktop app",
                    PlanOrigin::Cli => "CLI",
                }
            ),
        ));
    }
    let mut store = exec::open_store()?;
    check_recorded(&store, &plan)?;
    let (loaded, roots) = execution_roots(args, &store, &plan)?;
    let engine = pipeline::engine(&loaded, &roots, false, ui)?;
    let scan = scan(&engine, &roots, &loaded, false, ui)?;
    if scan.cancelled || cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    let narrowed = narrow(&plan, &scan.candidates);
    let by_id: BTreeMap<&ItemId, &Candidate> = scan.candidates.iter().map(|c| (&c.id, c)).collect();
    let items = preview_items(&narrowed.executable, &by_id);
    let plan_file = std::env::current_dir()
        .ok()
        .map(|cwd| FsPath::from_path(&absolute(&cwd, file)));
    let mut report = preview_report(
        PreviewMode::Review,
        &scan,
        Some(plan.clone()),
        plan_file,
        items,
    );
    // The elevated confirmation is decided on the plan as reviewed, not only on what still
    // qualifies: narrowing can only make an action safer.
    let elevated_planned = plan
        .items()
        .iter()
        .filter(|i| exec::needs_elevated_confirmation(i.tier, &i.action))
        .count() as u64;
    report.elevated_confirmation = elevated_confirmation(elevated_planned);
    report.dropped = narrowed
        .dropped
        .iter()
        .filter_map(|(id, reason)| {
            let item = plan.items().iter().find(|i| &i.item_id == id)?;
            Some(DroppedItem {
                item_id: id.clone(),
                path: item.path.clone(),
                planned_tier: item.tier,
                reason: reason.clone(),
                message: Skip::Dropped {
                    reason: reason.clone(),
                    fresh_tier: None,
                }
                .to_string(),
            })
        })
        .collect();

    let confirmed = if args.confirm {
        Confirmed::Flag
    } else if wants_execution {
        emit_preview(&report, ui)?;
        if !exec::prompt_yes_no(&format!(
            "Execute this plan now ({})?",
            summary(&report.totals)
        ))? {
            info(ui, "Not executed. The plan is kept until it expires.");
            return Err(cancel::cancelled_error());
        }
        Confirmed::Prompt
    } else {
        report.next_step = Some(format!(
            "Nothing was changed. To execute: {}",
            execute_command(report.plan_file.as_ref())
        ));
        return emit_preview(&report, ui);
    };
    let elevated = elevated_gate(&report, args.allow_permanent, exec::interactive(ui))?;
    if confirmed == Confirmed::Flag && !ui.format.is_machine() {
        emit_preview(&report, ui)?;
    }
    let sizes = sizes_by_id(&scan.candidates);
    let run = execute(
        &plan, &loaded, &who, &roots, &engine, &mut store, &sizes, confirmed, elevated, ui,
    )?;
    finish_run(&run, ui)
}

/// SAFE-07 / CLI-03: the elevated confirmation, when the items need it. `Ok(None)` when not
/// needed; `Ok(Some(how))` when given; `SafetyBlocked` (exit 5) when not given, or
/// `Cancelled` when declined at the prompt.
fn elevated_gate(
    report: &CleanPlanReport,
    allow_permanent: bool,
    interactive: bool,
) -> Result<Option<String>, ScullError> {
    let n = report.elevated_confirmation.items;
    if n == 0 {
        return Ok(None);
    }
    if allow_permanent {
        return Ok(Some("flag".into()));
    }
    if !interactive {
        return Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            format!(
                "{n} item(s) in this plan can't be undone (S1/S2, not restorable): pass \
                 {ALLOW_PERMANENT_FLAG} to confirm them without a terminal (SAFE-07). Nothing \
                 was changed."
            ),
        ));
    }
    let expected = n.to_string();
    if exec::prompt_typed(
        &format!(
            "{n} item(s) can't be undone (S1/S2: not restorable by scull). Type {expected} to \
             confirm them:"
        ),
        &expected,
    )? {
        Ok(Some("typed".into()))
    } else {
        Err(ScullError::new(
            ErrorKind::Cancelled,
            "the elevated confirmation was not given; nothing was changed",
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------------------------

/// The planner's choice: items to plan and candidates left out.
#[derive(Debug)]
pub struct Selection<'a> {
    /// Candidates to plan, in scan order.
    pub items: Vec<&'a Candidate>,
    /// Candidates left out, with the reason.
    pub excluded: Vec<ExcludedItem>,
}

/// Why `c` can never be in a `scull clean` plan, if it can't.
fn ineligible(c: &Candidate) -> Option<(ExcludeReason, String)> {
    if !c.tier.is_actionable() {
        return Some((
            ExcludeReason::Protected,
            format!("protected ({}): never cleaned", tier_text(c.tier)),
        ));
    }
    match &c.action {
        Action::None => Some((
            ExcludeReason::NoAction,
            "no cleanup action at this tier (inspect only)".into(),
        )),
        Action::ToolCommand { tool, .. }
            if c.category == Category::PackageCache && is_quick_clean_tool(tool) =>
        {
            Some((
                ExcludeReason::PackageCache,
                format!("package cache: `scull clean caches` cleans it with {tool}'s own command"),
            ))
        }
        // `docker builder prune` / `docker image prune` (P5-DET-03): Docker is read-only.
        a @ Action::ToolCommand { .. } if is_docker_command(a) => {
            Some((ExcludeReason::NotSupported, DOCKER_UNSUPPORTED.to_owned()))
        }
        // A long-tail tool cache (P5-DET-05, e.g. `ccache --clear`): Quick Clean doesn't run
        // its command in this build, and `scull clean` never does.
        Action::ToolCommand { tool, .. } if c.category == Category::PackageCache => Some((
            ExcludeReason::NotSupported,
            format!(
                "package cache: Quick Clean does not run `{}` in this build yet, and `scull \
                 clean` never runs tool commands",
                escape_for_terminal(tool)
            ),
        )),
        // E.g. `ollama rm` for Ollama models (P4-DET-01): no trusted-tool runner allow-lists it.
        Action::ToolCommand { tool, .. } => Some((
            ExcludeReason::NotSupported,
            format!(
                "{TOOL_NATIVE_UNSUPPORTED} (`{}` is never run by scull)",
                escape_for_terminal(tool)
            ),
        )),
        Action::Delete | Action::Quarantine
            if c.is_consistent() && action_allowed_for(c.tier, &c.action) =>
        {
            None
        }
        Action::Delete | Action::Quarantine => Some((
            ExcludeReason::Protected,
            "its tier, reasons and action disagree; left alone".into(),
        )),
        other => Some((
            ExcludeReason::NotSupported,
            format!(
                "`{}` is not executed by `scull clean` in this build",
                action_label(other)
            ),
        )),
    }
}

/// Select what to plan (PRD §8.2): S0 by default, S1 with `include_s1`, or exactly the
/// `wanted` items (ID or path, the only way to plan S2). An unknown `wanted` entry is a usage
/// error; one that can't be planned is refused (`SafetyBlocked`).
pub fn select<'a>(
    cands: &'a [Candidate],
    include_s1: bool,
    wanted: &[String],
    cwd: &Path,
) -> Result<Selection<'a>, ScullError> {
    let mut picked: BTreeSet<&ItemId> = BTreeSet::new();
    for w in wanted {
        let as_path = scull_engine::paths::canon(&absolute(cwd, Path::new(w)));
        let found = cands.iter().find(|c| {
            c.id.as_str() == w || c.path.as_str() == w || c.path.to_path_buf() == as_path
        });
        let Some(c) = found else {
            return Err(ScullError::new(
                ErrorKind::Usage,
                format!(
                    "--item {}: no candidate with this ID or path in the scan",
                    escape_for_terminal(w)
                ),
            ));
        };
        if let Some((_, why)) = ineligible(c) {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!("--item {}: can't be planned: {why}", escape_for_terminal(w)),
            )
            .with_item(c.id.clone())
            .with_path(c.path.clone()));
        }
        picked.insert(&c.id);
    }
    let mut sel = Selection {
        items: Vec::new(),
        excluded: Vec::new(),
    };
    for c in cands {
        let reason = match ineligible(c) {
            Some(r) => Some(r),
            None if !wanted.is_empty() => (!picked.contains(&c.id))
                .then(|| (ExcludeReason::NotSelected, "not named with --item".into())),
            None => match c.tier {
                Tier::S0 => None,
                Tier::S1 if include_s1 => None,
                Tier::S1 => Some((
                    ExcludeReason::OptInS1,
                    "costly to recreate (S1): opt in with --include-s1 or --item".into(),
                )),
                _ => Some((
                    ExcludeReason::PerItemS2,
                    "uncertain (S2): never preselected; name it with --item to plan it".into(),
                )),
            },
        };
        match reason {
            None => sel.items.push(c),
            Some((reason, message)) => sel.excluded.push(ExcludedItem {
                item_id: c.id.clone(),
                path: c.path.clone(),
                tier: c.tier,
                action: c.action.clone(),
                reclaimable_bytes: c.sizes.reclaimable.bytes,
                reason,
                message,
            }),
        }
    }
    Ok(sel)
}

fn scan(
    engine: &Engine,
    roots: &[PathBuf],
    loaded: &Loaded,
    history: bool,
    ui: &Ui,
) -> Result<ScanReport, ScullError> {
    let bridge = CancelBridge::start();
    let mut req = ScanRequest::full().history(history);
    req.threads = loaded.config.scan.threads as usize;
    let report = engine.scan(roots, &req, bridge.token(), pipeline::progress(ui))?;
    drop(bridge);
    Ok(report)
}

fn absolute(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Write `plan` to `out`, owner-only, replacing an existing file atomically (a link at `out`
/// is replaced, never followed).
fn write_plan_file(out: &Path, plan: &Plan) -> Result<(), ScullError> {
    let json = serde_json::to_string_pretty(plan).map_err(|e| {
        ScullError::new(
            ErrorKind::Internal,
            format!("cannot serialize the plan: {e}"),
        )
    })?;
    let name = out
        .file_name()
        .ok_or_else(|| ScullError::new(ErrorKind::Usage, "--output must name a file"))?;
    let dir = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        name.to_string_lossy(),
        exec::new_id_suffix(now_unix())
    ));
    let io_err = |e: std::io::Error| ScullError::from_io(&e).with_path(FsPath::from_path(out));
    let mut f = scull_platform::create_file_owner_only(&tmp).map_err(|e| {
        ScullError::new(ErrorKind::Io, format!("cannot write the plan file: {e}"))
            .with_path(FsPath::from_path(out))
    })?;
    let written = f
        .write_all(json.as_bytes())
        .and_then(|()| f.write_all(b"\n"))
        .and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, out)) {
        // Our own temp file, created a moment ago.
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(e));
    }
    Ok(())
}

/// Read and parse a plan file. Anything that is not a well-formed plan is rejected as
/// tampered (exit 4).
fn read_plan_file(file: &Path) -> Result<Plan, ScullError> {
    let path = FsPath::from_path(file);
    let md = std::fs::metadata(file).map_err(|e| {
        ScullError::new(
            ScullError::from_io(&e).kind,
            format!("cannot read the plan file: {e}"),
        )
        .with_path(path.clone())
    })?;
    if md.len() > MAX_PLAN_FILE_BYTES {
        return Err(ScullError::new(
            ErrorKind::PlanTampered,
            "the plan file is far larger than any plan scull writes",
        )
        .with_path(path));
    }
    let bytes = std::fs::read(file).map_err(|e| ScullError::from_io(&e).with_path(path.clone()))?;
    serde_json::from_slice::<Plan>(&bytes).map_err(|e| {
        ScullError::new(
            ErrorKind::PlanTampered,
            format!("not a valid scull plan file (edited or damaged?): {e}"),
        )
        .with_path(path)
    })
}

/// The plan must be the one recorded in this machine's database (SAFE-11: the database record
/// is the authority, the content hash alone is not a secret).
fn check_recorded(store: &Store, plan: &Plan) -> Result<(), ScullError> {
    match store.get_plan(plan.id())? {
        None => Err(ScullError::new(
            ErrorKind::PlanForeign,
            format!(
                "plan {} is not recorded in this machine's scull database (made elsewhere, or \
                 the database was reset); create a new plan",
                plan.id()
            ),
        )),
        Some(recorded) if recorded.content_hash() != plan.content_hash() => Err(ScullError::new(
            ErrorKind::PlanTampered,
            format!(
                "plan {} differs from the plan recorded under that ID; the plan file was edited",
                plan.id()
            ),
        )),
        Some(_) => Ok(()),
    }
}

/// The roots to revalidate under: `--root`, else the roots recorded for the plan's scan, else
/// the config. Checked like any scan root (CFG-04, D-22).
fn execution_roots(
    args: &CleanArgs,
    store: &Store,
    plan: &Plan,
) -> Result<(Loaded, Vec<PathBuf>), ScullError> {
    let cli_roots = if args.roots.is_empty() {
        recorded_roots(store, plan)
            .map(|r| r.iter().map(FsPath::to_path_buf).collect())
            .unwrap_or_default()
    } else {
        args.roots.clone()
    };
    let mut loaded = pipeline::load_config(&cli_roots)?;
    loaded.allow_network_roots = args.allow_network_roots;
    let roots = pipeline::roots(&loaded, args.allow_broad_roots)?;
    Ok((loaded, roots))
}

/// The scan roots recorded in history for the plan's items, found through their ancestors:
/// the roots of the plan's own scan when it is still the latest scan of one of them;
/// otherwise, per item, the shallowest ancestor recorded as the root of any usable scan (a
/// later scan of the same root hides the plan's from `latest_scan`). Nested roots collapse
/// into the outer one. They are checked again like any scan root by the caller.
fn recorded_roots(store: &Store, plan: &Plan) -> Option<Vec<FsPath>> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut known: BTreeMap<String, bool> = BTreeMap::new();
    for item in plan.items() {
        let path = item.path.to_path_buf();
        if found.iter().any(|r| path.starts_with(r)) {
            continue;
        }
        let mut shallowest = None;
        for anc in path.ancestors().skip(1) {
            let key = FsPath::from_path(anc);
            let is_root = match known.get(key.as_str()) {
                Some(r) => *r,
                None => {
                    let latest = store.latest_scan(&key).ok().flatten();
                    if let Some(s) = &latest
                        && &s.scan_id == plan.scan_id()
                    {
                        return Some(s.roots.clone());
                    }
                    known.insert(key.as_str().to_owned(), latest.is_some());
                    latest.is_some()
                }
            };
            if is_root {
                shallowest = Some(anc.to_path_buf());
            }
        }
        if let Some(r) = shallowest {
            found.retain(|f| !f.starts_with(&r));
            found.push(r);
        }
    }
    (!found.is_empty()).then(|| found.iter().map(|p| FsPath::from_path(p)).collect())
}

// ---------------------------------------------------------------------------------------------
// Previews
// ---------------------------------------------------------------------------------------------

fn preview_items(items: &[PlanItem], by_id: &BTreeMap<&ItemId, &Candidate>) -> Vec<PreviewItem> {
    items
        .iter()
        .filter_map(|i| {
            let c = by_id.get(&i.item_id)?;
            Some(PreviewItem {
                item: i.clone(),
                candidate: (*c).clone(),
                restorability: i.action.restorability(),
                undo: undo_text(&i.action, c),
                tool_binary: None,
                needs_elevated_confirmation: exec::needs_elevated_confirmation(i.tier, &i.action),
            })
        })
        .collect()
}

fn preview_report(
    mode: PreviewMode,
    scan: &ScanReport,
    plan: Option<Plan>,
    plan_file: Option<FsPath>,
    items: Vec<PreviewItem>,
) -> CleanPlanReport {
    let mut totals = PreviewTotals::default();
    for p in &items {
        totals.items += 1;
        let r = ActionReclaim::for_action(&p.candidate.sizes, &p.item.action);
        totals.reclaimable_bytes = totals
            .reclaimable_bytes
            .saturating_add(p.item.reclaimable_bytes);
        totals.freed_now_bytes = totals.freed_now_bytes.saturating_add(r.freed_now);
        totals.moved_bytes = totals.moved_bytes.saturating_add(r.moved);
        match p.item.action {
            Action::Delete => totals.delete += 1,
            Action::Quarantine => totals.quarantine += 1,
            Action::Trash => totals.trash += 1,
            _ => {}
        }
    }
    let elevated = items
        .iter()
        .filter(|i| i.needs_elevated_confirmation)
        .count() as u64;
    CleanPlanReport {
        mode,
        scan_id: scan.scan_id.clone(),
        roots: scan.roots.clone(),
        plan,
        plan_file,
        items,
        dropped: Vec::new(),
        excluded: Vec::new(),
        totals,
        elevated_confirmation: elevated_confirmation(elevated),
        signals_unavailable: scan.signals_unavailable.clone(),
        cancelled: scan.cancelled,
        next_step: None,
    }
}

fn elevated_confirmation(n: u64) -> ElevatedConfirmation {
    ElevatedConfirmation {
        required: n > 0,
        items: n,
        how: (n > 0).then(|| {
            format!("type {n} at the prompt, or pass {ALLOW_PERMANENT_FLAG} without a terminal")
        }),
    }
}

fn undo_text(action: &Action, c: &Candidate) -> String {
    match action.restorability() {
        Restorability::Restorable => format!(
            "restorable: `scull restore <operation>` brings it back; kept {} days, then purged",
            DEFAULT_RETENTION_SECS / 86_400
        ),
        Restorability::ViaTrash => "restorable from the Trash / Recycle Bin".into(),
        Restorability::Regenerate => format!(
            "not restorable: deleted permanently; regenerate it with {}",
            c.regeneration
                .instructions
                .as_deref()
                .map_or_else(|| "a rebuild or reinstall".to_owned(), |i| format!("`{i}`"))
        ),
        Restorability::NotRestorable => "NOT restorable".into(),
        Restorability::NotApplicable => "nothing is done".into(),
    }
}

fn undo_label(r: Restorability) -> String {
    match r {
        Restorability::Restorable => format!("restore ({} d)", DEFAULT_RETENTION_SECS / 86_400),
        Restorability::ViaTrash => "Trash".into(),
        Restorability::Regenerate => "regenerate".into(),
        Restorability::NotRestorable => "NOT restorable".into(),
        Restorability::NotApplicable => "-".into(),
    }
}

fn summary(t: &PreviewTotals) -> String {
    let mut parts = Vec::new();
    if t.delete > 0 {
        parts.push(format!("{} to delete", t.delete));
    }
    if t.quarantine > 0 {
        parts.push(format!("{} to quarantine", t.quarantine));
    }
    if t.trash > 0 {
        parts.push(format!("{} to the Trash", t.trash));
    }
    if parts.is_empty() {
        return "nothing to do".into();
    }
    format!(
        "{}, {} reclaimable",
        parts.join(", "),
        format_bytes(t.reclaimable_bytes)
    )
}

fn execute_command(plan_file: Option<&FsPath>) -> String {
    let file = plan_file.map_or_else(|| "<plan file>".to_owned(), |f| shell_word(f.as_str()));
    format!("scull clean --plan {file} --confirm")
}

fn next_step(plan: &Plan, plan_file: Option<&FsPath>) -> String {
    match plan_file {
        Some(f) => format!("To execute: {}", execute_command(Some(f))),
        None => format!(
            "Plan {} is recorded; write it to a file with --output to execute it with \
             `scull clean --plan FILE --confirm`",
            plan.id()
        ),
    }
}

/// Quote a word for a POSIX shell when it needs it (display only).
fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-+:=@%,".contains(&b))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn emit_preview(r: &CleanPlanReport, ui: &Ui) -> Result<(), ScullError> {
    if ui.format.is_machine() {
        emit_machine(ui.format, CLEAN_PLAN_SCHEMA, "plan", r)
    } else {
        emit_text(&render_preview(r, ui.color_out))
    }
}

fn info(ui: &Ui, msg: &str) {
    if !ui.quiet {
        let _ = writeln!(std::io::stderr(), "{}", escape_for_terminal(msg));
    }
}

/// `YYYY-MM-DD HH:MM UTC` for Unix seconds.
pub fn utc_datetime(secs: i64) -> String {
    // Civil-from-days (Howard Hinnant's algorithm), proleptic Gregorian calendar.
    let z = secs.div_euclid(86_400) + 719_468;
    let tod = secs.rem_euclid(86_400);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        tod / 3600,
        (tod % 3600) / 60
    )
}

fn flag(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "YES",
        Some(false) => "no",
        None => "unknown",
    }
}

fn evidence_text(e: &Evidence) -> String {
    match e {
        Evidence::Marker { path } => format!("project marker {}", path.as_str()),
        Evidence::Lockfile { path } => format!("lockfile {}", path.as_str()),
        Evidence::KnownArtifact { artifact } => format!("known artifact type: {artifact}"),
        Evidence::GitTracked { files } => format!("{files} Git-tracked file(s)"),
        Evidence::ProtectedRoot { root } => format!("inside protected root {}", root.as_str()),
        Evidence::UserExclusion { rule } => format!("user exclusion {rule}"),
        Evidence::ProtectMarker { path } => format!(".scull-protect at {}", path.as_str()),
        Evidence::InUse { signal, by } => match by {
            Some(b) => format!("in use ({signal}) by {b}"),
            None => format!("in use ({signal})"),
        },
        Evidence::LastModified {
            newest_unix,
            sampled,
        } => format!(
            "newest change {}{}",
            utc_datetime(*newest_unix),
            if *sampled { " (sampled)" } else { "" }
        ),
        Evidence::CrossesMount => "crosses a mount boundary".into(),
        Evidence::LinkEscape { target } => format!("link escapes to {}", target.as_str()),
        Evidence::CloudSync { provider } => format!("cloud-synced ({provider})"),
        Evidence::Hardlinked { shared_bytes } => {
            format!("{} shared with hard links", format_bytes(*shared_bytes))
        }
        Evidence::SignalUnavailable { signal, reason } => {
            format!("signal unavailable: {signal} ({reason})")
        }
        Evidence::Note { text } => text.clone(),
        other => format!("{other:?}"),
    }
}

fn regeneration_text(c: &Candidate) -> String {
    let how = c
        .regeneration
        .instructions
        .clone()
        .unwrap_or_else(|| "unknown".into());
    let cost = match &c.regeneration.cost {
        RegenerationCost::Unknown => "cost unknown".to_owned(),
        RegenerationCost::Estimate { seconds, network } => format!(
            "about {seconds} s{}",
            if *network { ", needs the network" } else { "" }
        ),
        RegenerationCost::NotRegenerable => "can't be regenerated".to_owned(),
    };
    format!("{how} ({cost})")
}

/// The human preview.
pub fn render_preview(r: &CleanPlanReport, color: bool) -> String {
    let mut out = String::new();
    let base = (r.roots.len() == 1).then(|| r.roots[0].to_path_buf());
    let base = base.as_deref();
    match r.mode {
        PreviewMode::Plan => out.push_str(&format!(
            "{}\n",
            Style::Bold.paint("Cleanup plan (dry run): nothing has been changed.", color)
        )),
        PreviewMode::Review => {
            if let Some(p) = &r.plan {
                out.push_str(&format!(
                    "{}\n",
                    Style::Bold.paint(
                        &format!(
                            "Reviewing plan {} (made {}): checked against a fresh scan.",
                            p.id(),
                            utc_datetime(p.created_unix())
                        ),
                        color
                    )
                ));
            }
        }
    }
    let roots: Vec<String> = r
        .roots
        .iter()
        .map(|p| escape_for_terminal(p.as_str()).into_owned())
        .collect();
    out.push_str(&format!("Scan roots: {}\n\n", roots.join(", ")));

    if r.items.is_empty() {
        out.push_str(match r.mode {
            PreviewMode::Plan => "Nothing to clean with the current selection.\n",
            PreviewMode::Review => "No item of this plan qualifies any more.\n",
        });
    } else {
        let mut t = Table::new([
            ("#", Align::Right),
            ("Tier", Align::Left),
            ("Action", Align::Left),
            ("Undo", Align::Left),
            ("Reclaimable", Align::Right),
            ("Path", Align::Left),
        ]);
        for (n, p) in r.items.iter().enumerate() {
            t.push(vec![
                Cell::text(&(n + 1).to_string()),
                Cell::tier(p.item.tier),
                Cell::text(&action_label(&p.item.action)),
                Cell::text(&undo_label(p.restorability)),
                Cell::bytes(p.item.reclaimable_bytes),
                Cell::path(&display_path(&p.candidate.path, base)),
            ]);
        }
        out.push_str(&t.render(color));
        out.push_str(&format!("\n{}\n", Style::Bold.paint("Details:", color)));
        for (n, p) in r.items.iter().enumerate() {
            out.push_str(&render_item(n + 1, p));
        }
    }

    if !r.dropped.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            Style::Warn.paint(
                &format!(
                    "No longer qualifying ({}; skipped at execution, never cleaned \"best effort\"):",
                    r.dropped.len()
                ),
                color
            )
        ));
        for d in &r.dropped {
            out.push_str(&format!(
                "  {} {}: {}\n",
                tier_text(d.planned_tier),
                escape_for_terminal(&display_path(&d.path, base)),
                escape_for_terminal(&d.message)
            ));
        }
    }

    if !r.excluded.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            Style::Bold.paint(&format!("Not in this plan ({}):", r.excluded.len()), color)
        ));
        for e in r.excluded.iter().take(EXCLUDED_LISTED) {
            out.push_str(&format!(
                "  {:<14} {:>10}  {}: {}\n",
                tier_text(e.tier),
                format_bytes(e.reclaimable_bytes),
                escape_for_terminal(&display_path(&e.path, base)),
                escape_for_terminal(&e.message)
            ));
        }
        if r.excluded.len() > EXCLUDED_LISTED {
            out.push_str(&format!(
                "  ... and {} more (see --json)\n",
                r.excluded.len() - EXCLUDED_LISTED
            ));
        }
    }

    out.push_str(&signals_section(&r.signals_unavailable, color));

    let t = &r.totals;
    if t.items > 0 {
        out.push_str(&format!("\nTotal: {}.\n", summary(t)));
        if t.delete > 0 {
            out.push_str(&format!(
                "  Deleted items free {} when the run completes; they are not restorable \
                 (regenerate them).\n",
                format_bytes(t.freed_now_bytes)
            ));
        }
        if t.quarantine + t.trash > 0 {
            out.push_str(&format!(
                "  Quarantined items move {} aside on the same volume (restorable); the space \
                 is freed when the quarantine is purged.\n",
                format_bytes(t.moved_bytes)
            ));
        }
    }
    if r.elevated_confirmation.required {
        out.push_str(&format!(
            "{}\n",
            Style::Warn.paint(
                &format!(
                    "{} item(s) can't be undone (S1/S2): {}.",
                    r.elevated_confirmation.items,
                    r.elevated_confirmation.how.as_deref().unwrap_or_default()
                ),
                color
            )
        ));
    }
    if let Some(p) = &r.plan {
        out.push_str(&format!(
            "\nPlan {} · {} item(s) · expires {} · bound to this machine and user ({})\n",
            p.id(),
            p.items().len(),
            utc_datetime(p.expires_unix()),
            escape_for_terminal(p.user())
        ));
        if let Some(f) = &r.plan_file {
            out.push_str(&format!("Plan file: {}\n", escape_for_terminal(f.as_str())));
        }
    }
    if let Some(n) = &r.next_step {
        out.push_str(&format!("{}\n", escape_for_terminal(n)));
    }
    out
}

fn render_item(n: usize, p: &PreviewItem) -> String {
    let c = &p.candidate;
    let mut out = format!("  {n}. {}\n", escape_for_terminal(c.path.as_str()));
    let mut line = |label: &str, value: &str| {
        out.push_str(&format!(
            "       {label:<9} {}\n",
            escape_for_terminal(value)
        ));
    };
    let project = c.project.as_ref().map_or_else(String::new, |pr| {
        format!(
            ", project {} ({}) at {}",
            pr.name,
            pr.ecosystem,
            pr.root.as_str()
        )
    });
    line(
        "what",
        &format!(
            "{} (rule {} v{}){project}",
            c.artifact, c.rule.id, c.rule.version
        ),
    );
    line(
        "sizes",
        &format!(
            "logical {} · allocated {} · reclaimable {} ({} confidence)",
            format_bytes(c.sizes.logical),
            format_bytes(c.sizes.allocated),
            format_bytes(c.sizes.reclaimable.bytes),
            confidence_label(c.sizes.reclaimable.confidence)
        ),
    );
    for (i, r) in c.reasons.iter().enumerate() {
        line(
            if i == 0 { "tier" } else { "" },
            &format!(
                "{}: {} [{}]",
                tier_text(r.tier),
                r.message,
                json_name(&r.code)
            ),
        );
    }
    if c.reasons.is_empty() {
        line("tier", &tier_text(c.tier));
    }
    for (i, e) in c.evidence.iter().enumerate() {
        line(if i == 0 { "evidence" } else { "" }, &evidence_text(e));
    }
    line("regen", &regeneration_text(c));
    line(
        "flags",
        &format!(
            "active {} · hardlinked {} · cross-mount {} · symlink {} · cloud placeholder {}",
            flag(c.flags.active),
            flag(c.flags.hardlinked),
            flag(c.flags.cross_mount),
            flag(c.flags.symlink),
            flag(c.flags.cloud_placeholder)
        ),
    );
    let action = match &p.tool_binary {
        Some(b) => format!("{} (runs {})", action_label(&p.item.action), b.as_str()),
        None => action_label(&p.item.action),
    };
    line("action", &format!("{action}: {}", p.undo));
    out
}

// ---------------------------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------------------------

fn sizes_by_id(cands: &[Candidate]) -> BTreeMap<ItemId, Sizes> {
    cands.iter().map(|c| (c.id.clone(), c.sizes)).collect()
}

/// Lock, record the operation, execute every item with revalidation, measure, finish.
#[allow(clippy::too_many_arguments)]
fn execute(
    plan: &Plan,
    loaded: &Loaded,
    who: &Who,
    roots: &[PathBuf],
    engine: &Engine,
    store: &mut Store,
    sizes: &BTreeMap<ItemId, Sizes>,
    confirmed: Confirmed,
    elevated: Option<String>,
    ui: &Ui,
) -> Result<CleanRunReport, ScullError> {
    let executor = exec::lock_and_recover(store, ui)?;
    let started_at = now_unix();
    let operation_id = exec::new_operation_id(started_at)?;
    let mut op = executor
        .begin_operation(
            store,
            plan,
            &OperationStart {
                operation_id: operation_id.clone(),
                plan_id: plan.id().clone(),
                content_hash: plan.content_hash().clone(),
                approver: who.user.clone(),
                // `--confirm` without a prompt, or `y` at the prompt on a terminal.
                method: match confirmed {
                    Confirmed::Flag => ApprovalMethod::CliFlag,
                    Confirmed::Prompt => ApprovalMethod::CliInteractive,
                },
                started_at,
            },
        )?
        // One measurement: the CLI's own meter below (with resampling for delayed frees) is what
        // the report shows and what `finish_measured` records (I-5).
        .without_measurement()
        // The configured quarantine location (P2-EXEC-13), and the Trash where quarantine is
        // impossible (D-3: quarantine → Trash → blocked, never deleted).
        .with_quarantine(exec::quarantine_settings(&loaded.config.quarantine))
        .with_trash(exec::trash_locations());
    let predicted = |i: &PlanItem| {
        ActionReclaim::for_action(
            &sizes.get(&i.item_id).copied().unwrap_or_default(),
            &i.action,
        )
    };
    let predicted_freed: u64 = plan.items().iter().map(|i| predicted(i).freed_now).sum();
    let mut options = MeasureOptions::default();
    if predicted_freed <= options.resolution {
        // Waiting for delayed frees can't resolve a difference this small.
        options = options.with_resampling(Duration::ZERO, Duration::ZERO);
    }
    let meter = VolumeMeter::new(PlatformFreeSpace, options);
    let baseline = meter.before(roots);
    let revalidation = Revalidation::new(engine, roots);
    if ui.progress {
        ui.progress(&format!("executing {} item(s)...", plan.items().len()));
    }
    let bridge = CancelBridge::start();
    let ctx = PlanContext {
        now_unix: now_unix(),
        machine_id: &who.machine_id,
        user: &who.user,
    };
    let result = executor.execute_plan(plan, &ctx, &revalidation, Some(&mut op), bridge.token());
    drop(bridge);
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = op.finish(OperationEnd::Cancelled, now_unix());
            return Err(e);
        }
    };
    let mut tally = Tally::new();
    for (item, r) in plan.items().iter().zip(&report.items) {
        tally.add(&predicted(item), &[], item_execution(r));
    }
    let delta = meter.after(baseline, &tally);
    let finished_at = now_unix();
    op.finish_measured(
        if report.cancelled {
            OperationEnd::Cancelled
        } else {
            OperationEnd::Normal
        },
        finished_at,
        delta.measured_delta_bytes(),
    )?;
    Ok(run_report(
        plan,
        &report,
        operation_id,
        who,
        confirmed,
        elevated,
        started_at,
        finished_at,
        &delta,
    ))
}

fn item_execution(r: &ItemReport) -> ItemExecution {
    match &r.outcome {
        ItemOutcome::Deleted(w) if w.outcome == RemoveOutcome::Complete => ItemExecution::Done {
            removed: w.removed.allocated_bytes,
            moved: 0,
        },
        ItemOutcome::Deleted(w) => ItemExecution::Partial {
            removed: w.removed.allocated_bytes,
            moved: 0,
        },
        ItemOutcome::Quarantined(q) => ItemExecution::Done {
            removed: 0,
            moved: q.bytes,
        },
        ItemOutcome::Trashed(t) => ItemExecution::Done {
            removed: 0,
            moved: t.moved_bytes(),
        },
        _ => ItemExecution::NotExecuted,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_report(
    plan: &Plan,
    report: &ExecutionReport,
    operation_id: OperationId,
    who: &Who,
    confirmed: Confirmed,
    elevated: Option<String>,
    started_at: i64,
    finished_at: i64,
    delta: &scull_cleaner::measure::MeasuredDelta,
) -> CleanRunReport {
    let mut totals = RunTotals::default();
    let items: Vec<RunItem> = plan
        .items()
        .iter()
        .zip(&report.items)
        .map(|(planned, r)| {
            let mut item = RunItem {
                item_id: r.item_id.clone(),
                path: r.path.clone(),
                planned_tier: r.planned_tier,
                fresh_tier: r.fresh_tier,
                action: planned.action.clone(),
                outcome: RunOutcome::NotRun,
                reason_code: r.reason_code().map(str::to_owned),
                message: None,
                removed_bytes: 0,
                moved_bytes: 0,
                quarantine_path: None,
                quarantine_entry: None,
                trash_path: None,
                restorability: Restorability::NotApplicable,
            };
            match &r.outcome {
                ItemOutcome::Deleted(w) => {
                    item.removed_bytes = w.removed.allocated_bytes;
                    item.restorability = Restorability::Regenerate;
                    if w.outcome == RemoveOutcome::Complete {
                        item.outcome = RunOutcome::Deleted;
                        totals.succeeded += 1;
                    } else {
                        item.outcome = RunOutcome::PartlyDeleted;
                        item.message = Some(format!(
                            "{} entr(ies) kept{}",
                            w.skipped_total,
                            w.aborted
                                .as_ref()
                                .map(|a| format!("; stopped: {a:?}"))
                                .unwrap_or_default()
                        ));
                        totals.partial += 1;
                    }
                }
                ItemOutcome::Quarantined(q) => {
                    item.outcome = RunOutcome::Quarantined;
                    item.moved_bytes = q.bytes;
                    item.quarantine_path = Some(FsPath::from_path(&q.quarantine_path));
                    item.quarantine_entry = Some(q.entry_id);
                    item.restorability = Restorability::Restorable;
                    totals.succeeded += 1;
                }
                ItemOutcome::Trashed(tr) => {
                    item.outcome = RunOutcome::Trashed;
                    item.moved_bytes = tr.moved_bytes();
                    item.trash_path = Some(FsPath::from_path(
                        tr.outcome
                            .location
                            .as_deref()
                            .unwrap_or(&tr.outcome.trash_dir),
                    ));
                    item.message = Some(
                        "quarantine was impossible here: moved to the Trash (restore it from there)"
                            .into(),
                    );
                    item.restorability = tr.restorability();
                    totals.succeeded += 1;
                }
                ItemOutcome::Skipped(s) => {
                    item.outcome = RunOutcome::Skipped;
                    item.message = Some(s.to_string());
                    totals.skipped += 1;
                }
                ItemOutcome::Refused(e) => {
                    item.outcome = RunOutcome::Refused;
                    item.message = Some(e.message.clone());
                    totals.refused += 1;
                }
                ItemOutcome::Blocked { message, .. } => {
                    item.outcome = RunOutcome::Blocked;
                    item.message = Some(message.clone());
                    totals.blocked += 1;
                }
                ItemOutcome::NotRun => {
                    item.message = Some("not attempted: cancelled".into());
                    totals.not_run += 1;
                }
                other => {
                    item.outcome = RunOutcome::Refused;
                    item.message = Some(format!("{other:?}"));
                    totals.refused += 1;
                }
            }
            totals.removed_bytes = totals.removed_bytes.saturating_add(item.removed_bytes);
            totals.moved_bytes = totals.moved_bytes.saturating_add(item.moved_bytes);
            item
        })
        .collect();
    let quarantined = items.iter().any(|i| i.outcome == RunOutcome::Quarantined);
    CleanRunReport {
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        restore_hint: quarantined.then(|| {
            format!(
                "Quarantined items can be restored for {} days: scull restore {}",
                DEFAULT_RETENTION_SECS / 86_400,
                operation_id
            )
        }),
        operation_id,
        approver: who.user.clone(),
        confirmation: match confirmed {
            Confirmed::Flag => "flag",
            Confirmed::Prompt => "prompt",
        }
        .into(),
        elevated_confirmation: elevated,
        started_at,
        finished_at,
        cancelled: report.cancelled,
        items,
        totals,
        measured: Measured {
            predicted_freed_bytes: delta.predicted.freed_now,
            predicted_moved_bytes: delta.predicted.moved,
            removed_bytes: delta.removed,
            moved_bytes: delta.moved,
            measured_delta_bytes: delta.measured_total,
            discrepancy_pct: delta.discrepancy_pct,
            exceeds_threshold: delta.exceeds_threshold,
            within_resolution: delta.within_resolution,
            explanations: delta
                .explanations
                .iter()
                .map(|e| Explanation {
                    code: e.code().to_owned(),
                    message: e.message(),
                })
                .collect(),
        },
    }
}

/// Print a run report and turn its outcome into the exit code.
fn finish_run(run: &CleanRunReport, ui: &Ui) -> Result<(), ScullError> {
    if ui.format.is_machine() {
        emit_machine(ui.format, CLEAN_RUN_SCHEMA, "run", run)?;
    } else {
        emit_text(&render_run(run, ui.color_out))?;
    }
    if run.cancelled || cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    let t = &run.totals;
    let failed = t.partial + t.skipped + t.refused + t.blocked + t.not_run;
    if failed > 0 {
        return Err(ScullError::new(
            ErrorKind::PartialFailure,
            format!(
                "{failed} of {} item(s) did not complete; each is listed with the reason",
                run.items.len()
            ),
        ));
    }
    Ok(())
}

fn outcome_label(o: RunOutcome) -> &'static str {
    match o {
        RunOutcome::Deleted => "deleted",
        RunOutcome::PartlyDeleted => "PARTLY deleted",
        RunOutcome::Quarantined => "quarantined",
        RunOutcome::Trashed => "moved to Trash",
        RunOutcome::Skipped => "skipped",
        RunOutcome::Refused => "refused",
        RunOutcome::Blocked => "blocked",
        RunOutcome::NotRun => "not run",
    }
}

/// The human run report.
pub fn render_run(r: &CleanRunReport, color: bool) -> String {
    let mut out = format!(
        "\n{}\n",
        Style::Bold.paint(
            &format!(
                "Operation {} (plan {}){}",
                r.operation_id,
                r.plan_id,
                if r.cancelled { ": CANCELLED" } else { "" }
            ),
            color
        )
    );
    let mut t = Table::new([
        ("Outcome", Align::Left),
        ("Tier", Align::Left),
        ("Removed", Align::Right),
        ("Moved", Align::Right),
        ("Path", Align::Left),
        ("Why / where", Align::Left),
    ]);
    for i in &r.items {
        let style = match i.outcome {
            RunOutcome::Deleted | RunOutcome::Quarantined | RunOutcome::Trashed => Style::Plain,
            _ => Style::Warn,
        };
        let why = match (
            i.quarantine_path.as_ref().or(i.trash_path.as_ref()),
            &i.message,
        ) {
            (Some(q), _) => format!("-> {}", q.as_str()),
            (None, Some(m)) => match &i.reason_code {
                Some(code) => format!("[{code}] {m}"),
                None => m.clone(),
            },
            (None, None) => String::new(),
        };
        t.push(vec![
            Cell::text(outcome_label(i.outcome)).styled(style),
            Cell::tier(
                i.fresh_tier
                    .map_or(i.planned_tier, |f| i.planned_tier.max_protective(f)),
            ),
            Cell::bytes(i.removed_bytes),
            Cell::bytes(i.moved_bytes),
            Cell::path(i.path.as_str()),
            Cell::text(&why),
        ]);
    }
    out.push_str(&t.render(color));
    let tt = &r.totals;
    out.push_str(&format!(
        "\n{} of {} item(s) done: {} removed, {} moved into quarantine",
        tt.succeeded,
        r.items.len(),
        format_bytes(tt.removed_bytes),
        format_bytes(tt.moved_bytes)
    ));
    let failed = [
        (tt.partial, "partly deleted"),
        (tt.skipped, "skipped"),
        (tt.refused, "refused"),
        (tt.blocked, "blocked"),
        (tt.not_run, "not run"),
    ];
    let failed: Vec<String> = failed
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, l)| format!("{n} {l}"))
        .collect();
    if failed.is_empty() {
        out.push_str(".\n");
    } else {
        out.push_str(&format!("; {}.\n", failed.join(", ")));
    }
    let m = &r.measured;
    let measured = m.measured_delta_bytes.map_or_else(
        || "not measurable".to_owned(),
        |d| {
            if d < 0 {
                format!("-{}", format_bytes(d.unsigned_abs()))
            } else {
                format_bytes(d.unsigned_abs())
            }
        },
    );
    out.push_str(&format!(
        "Free space: predicted +{} now, measured {measured}{}.\n",
        format_bytes(m.predicted_freed_bytes),
        m.discrepancy_pct
            .filter(|_| m.exceeds_threshold && !m.within_resolution)
            .map(|p| format!(" ({p:+.0}%)"))
            .unwrap_or_default()
    ));
    for e in &m.explanations {
        out.push_str(&format!("  - {}\n", escape_for_terminal(&e.message)));
    }
    if let Some(h) = &r.restore_hint {
        out.push_str(&format!("{}\n", escape_for_terminal(h)));
    }
    out.push_str(&format!(
        "Recorded in the audit log (scull history {}).\n",
        r.operation_id
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_datetime_is_civil() {
        assert_eq!(utc_datetime(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_datetime(1_800_000_000), "2027-01-15 08:00 UTC");
    }

    fn tool_candidate(category: Category, rule: &str, action: Action) -> Candidate {
        let identity = scull_core::FileIdentity::new(7, 1);
        let path = "/home/u/.ollama/models";
        Candidate {
            id: ItemId::derive(identity, path.as_bytes()),
            path: FsPath::from(path),
            identity,
            category,
            artifact: "test".into(),
            rule: scull_core::RuleRef {
                id: rule.into(),
                version: 1,
            },
            project: None,
            tier: Tier::S1,
            reasons: vec![scull_core::TierReason::new(
                Tier::S1,
                scull_core::ReasonCode::PackageCache,
                "test",
            )],
            evidence: vec![],
            sizes: Sizes::default(),
            flags: scull_core::Flags::default(),
            regeneration: scull_core::Regeneration::default(),
            action,
        }
    }

    /// A package cache points at `scull clean caches`; any other tool command (`ollama rm`
    /// for Ollama models) is left out as not supported, with the reason, never planned.
    #[test]
    fn tool_commands_are_never_planned_and_ollama_says_why() {
        let npm = tool_candidate(
            Category::PackageCache,
            "npm.cache",
            Action::ToolCommand {
                tool: "npm".into(),
                args: vec!["cache".into(), "verify".into()],
            },
        );
        let (reason, why) = ineligible(&npm).unwrap();
        assert_eq!(reason, ExcludeReason::PackageCache);
        assert!(why.contains("scull clean caches"), "{why}");

        let ollama = tool_candidate(
            Category::MlCache,
            "ollama.models",
            Action::ToolCommand {
                tool: "ollama".into(),
                args: vec!["rm".into(), "llama3:latest".into()],
            },
        );
        let (reason, why) = ineligible(&ollama).unwrap();
        assert_eq!(reason, ExcludeReason::NotSupported);
        assert!(why.contains(TOOL_NATIVE_UNSUPPORTED), "{why}");
        assert!(why.contains("`ollama`"), "{why}");
        let cands = [ollama];
        let sel = select(&cands, true, &[], Path::new("/")).unwrap();
        assert!(sel.items.is_empty());
        assert_eq!(sel.excluded.len(), 1);
        let err = select(&cands, true, &[cands[0].id.to_string()], Path::new("/")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::SafetyBlocked);
        assert!(err.message.contains(TOOL_NATIVE_UNSUPPORTED), "{err}");
    }

    /// A long-tail tool cache (P5-DET-05) is a package cache Quick Clean doesn't run yet: it
    /// is excluded as not supported, never pointed at `scull clean caches`, never planned.
    #[test]
    fn long_tail_tool_caches_are_never_planned() {
        let ccache = tool_candidate(
            Category::PackageCache,
            "ccache.cache",
            Action::ToolCommand {
                tool: "ccache".into(),
                args: vec!["--clear".into()],
            },
        );
        assert!(!is_quick_clean_tool("ccache"));
        let (reason, why) = ineligible(&ccache).unwrap();
        assert_eq!(reason, ExcludeReason::NotSupported);
        assert!(why.contains("does not run `ccache`"), "{why}");
        let cands = [ccache];
        assert!(
            select(&cands, true, &[], Path::new("/"))
                .unwrap()
                .items
                .is_empty()
        );
        let err = select(&cands, true, &[cands[0].id.to_string()], Path::new("/")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::SafetyBlocked);
    }

    /// Docker's proposed prunes (P5-DET-03) are never planned: excluded with the Docker
    /// reason, and naming one is refused.
    #[test]
    fn docker_commands_are_never_planned() {
        for kind in [
            scull_detectors::DockerKind::BuildCache,
            scull_detectors::DockerKind::DanglingImages,
        ] {
            let c = tool_candidate(Category::Other, "docker", kind.action());
            let (reason, why) = ineligible(&c).unwrap();
            assert_eq!(reason, ExcludeReason::NotSupported);
            assert_eq!(why, DOCKER_UNSUPPORTED);
            let cands = [c];
            let sel = select(&cands, true, &[], Path::new("/")).unwrap();
            assert!(sel.items.is_empty());
            let err = select(&cands, true, &[cands[0].id.to_string()], Path::new("/")).unwrap_err();
            assert_eq!(err.kind, ErrorKind::SafetyBlocked);
            assert!(err.message.contains(DOCKER_UNSUPPORTED), "{err}");
        }
    }

    #[test]
    fn shell_words_are_quoted_when_needed() {
        assert_eq!(shell_word("plan.json"), "plan.json");
        assert_eq!(shell_word("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }
}
