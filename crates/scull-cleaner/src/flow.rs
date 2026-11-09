//! The cleanup flow shared by every frontend (#143): what can be planned, how a plan is made
//! from a scan, and how a reviewed plan is executed (lock, operation, quarantine/Trash
//! settings, free-space measurement). UI-free: prompts, plan files and rendering stay in the
//! frontends (the CLI today, the desktop app next), so they all apply the same rules.
//!
//! Messages are plain text: frontends escape them for display (SAFE-17).
//!
//! Nothing here deletes directly: execution goes through [`Executor::execute_plan`], which
//! revalidates every item against a fresh scan (SAFE-11) and journals it (SAFE-10).

use crate::measure::{
    ItemExecution, MeasureOptions, MeasuredDelta, PlatformFreeSpace, Tally, VolumeMeter,
};
use crate::trash::TrashLocations;
use crate::{
    DOCKER_UNSUPPORTED, ExecutionReport, Executor, ItemOutcome, ItemReport, LockOptions,
    PlanContext, QuarantineSettings, RecoveryReport, RemoveOutcome, Revalidation,
    TOOL_NATIVE_UNSUPPORTED, is_docker_command, is_quick_clean_tool,
};
use schemars::JsonSchema;
use scull_core::config::TEST_SANDBOX_ENV;
use scull_core::{
    Action, Candidate, Category, ErrorKind, ItemId, OperationId, Plan, PlanId, PlanItem,
    PlanOrigin, PlanParams, Restorability, ScanId, ScullError, Sizes, Tier, action_allowed_for,
};
use scull_engine::{CancelToken, Engine, now_unix};
use scull_scanner::ActionReclaim;
use scull_storage::{ApprovalMethod, OperationEnd, OperationStart, Store};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The machine and user a plan is bound to (SAFE-11, PRD §10.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// This installation's machine ID (`scull_platform::machine_id`).
    pub machine_id: String,
    /// The account scull runs as (`scull_platform::current_user`).
    pub user: String,
}

impl Binding {
    /// This machine's ID (created in `data_dir` on first use) and the current user.
    pub fn current(data_dir: &Path) -> Result<Self, ScullError> {
        let machine_id = scull_platform::machine_id(data_dir).map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot read or create this machine's ID: {e}"),
            )
        })?;
        let user = scull_platform::current_user().map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot tell which account scull runs as: {e}"),
            )
        })?;
        Ok(Self { machine_id, user })
    }
}

/// SAFE-07: whether an item at `tier` with `action` needs the elevated confirmation (typing
/// the count, or `--allow-permanent` without a terminal): S1/S2 items whose action can't be
/// undone by scull. S0 deletes take the standard confirmation (D-3); quarantine and Trash are
/// restorable; S3 items are never executed.
pub fn needs_elevated_confirmation(tier: Tier, action: &Action) -> bool {
    matches!(tier, Tier::S1 | Tier::S2)
        && matches!(
            action.restorability(),
            Restorability::Regenerate | Restorability::NotRestorable
        )
}

/// A fresh ID suffix: Unix seconds then 64 random bits, in hex (sorts roughly by time).
pub fn new_id_suffix(now_unix: i64) -> String {
    use std::hash::{BuildHasher, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let random = RandomState::new().hash_one((nanos, n, std::process::id()));
    format!("{:x}{random:016x}", u64::try_from(now_unix).unwrap_or(0))
}

/// A new plan ID.
pub fn new_plan_id(now_unix: i64) -> Result<PlanId, ScullError> {
    PlanId::from_suffix(&new_id_suffix(now_unix))
        .map_err(|e| ScullError::new(ErrorKind::Internal, e))
}

/// A new operation ID.
pub fn new_operation_id(now_unix: i64) -> Result<OperationId, ScullError> {
    OperationId::from_suffix(&new_id_suffix(now_unix))
        .map_err(|e| ScullError::new(ErrorKind::Internal, e))
}

/// Where the Trash fallback moves an S1/S2 item whose quarantine is impossible (D-3,
/// P2-EXEC-07): the user's real Trash ([`TrashLocations::from_env`]). Under the test harness
/// ([`TEST_SANDBOX_ENV`], set by the test fixtures) a Trash inside the sandbox instead
/// (`<sandbox>/Trash` and the sandbox as the volume's top directory), never the real one.
pub fn trash_locations() -> TrashLocations {
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

/// The current user's execution lock (SAFE-09; fails at once with `lock_held` when another
/// operation runs), then every operation a crash interrupted reconciled with the disk before
/// anything else runs (SAFE-19: never resumes a deletion or move, only records what the disk
/// shows). The frontend reports the recovered operations.
pub fn lock_and_recover(store: &mut Store) -> Result<(Executor, RecoveryReport), ScullError> {
    let executor = Executor::acquire(&LockOptions::for_current_user()?)?;
    let report = executor.recover(store, now_unix())?;
    Ok((executor, report))
}

// ---------------------------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------------------------

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

/// Why `c` can never be in a cleanup plan, if it can't.
pub fn ineligible(c: &Candidate) -> Option<(ExcludeReason, String)> {
    if !c.tier.is_actionable() {
        return Some((
            ExcludeReason::Protected,
            format!("protected ({} {}): never cleaned", c.tier, c.tier.label()),
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
                "package cache: Quick Clean does not run `{tool}` in this build yet, and \
                 `scull clean` never runs tool commands"
            ),
        )),
        // E.g. `ollama rm` for Ollama models (P4-DET-01): no trusted-tool runner allow-lists it.
        Action::ToolCommand { tool, .. } => Some((
            ExcludeReason::NotSupported,
            format!("{TOOL_NATIVE_UNSUPPORTED} (`{tool}` is never run by scull)"),
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
                match other {
                    Action::Trash => "trash".to_owned(),
                    Action::GitWorktreePrune => "git worktree prune".to_owned(),
                    Action::GitWorktreeRemove => "git worktree remove".to_owned(),
                    other => format!("{other:?}").to_lowercase(),
                }
            ),
        )),
    }
}

/// The planner's choice: items to plan and candidates left out.
#[derive(Debug)]
pub struct Selection<'a> {
    /// Candidates to plan, in scan order.
    pub items: Vec<&'a Candidate>,
    /// Candidates left out, with the reason and a sentence.
    pub excluded: Vec<(&'a Candidate, ExcludeReason, String)>,
}

/// Select what to plan (PRD §8.2): S0 by default, S1 with `include_s1`, or exactly the
/// `picked` items (the only way to plan S2). The frontend resolves what the user named into
/// `picked` and refuses named items that are [`ineligible`] first.
pub fn select<'a>(
    cands: &'a [Candidate],
    include_s1: bool,
    picked: Option<&BTreeSet<&ItemId>>,
) -> Selection<'a> {
    let mut sel = Selection {
        items: Vec::new(),
        excluded: Vec::new(),
    };
    for c in cands {
        let reason = match ineligible(c) {
            Some(r) => Some(r),
            None => match picked {
                Some(picked) => (!picked.contains(&c.id))
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
            },
        };
        match reason {
            None => sel.items.push(c),
            Some((reason, message)) => sel.excluded.push((c, reason, message)),
        }
    }
    sel
}

/// A plan of `items` from scan `scan_id`, bound to `who`, or `None` when nothing was selected.
/// `operations_watermark` is the last operation recorded before the scan started (SAFE-09):
/// any later operation invalidates the plan.
pub fn new_plan(
    scan_id: &ScanId,
    items: &[&Candidate],
    who: &Binding,
    origin: PlanOrigin,
    operations_watermark: u64,
    now_unix: i64,
) -> Result<Option<Plan>, ScullError> {
    if items.is_empty() {
        return Ok(None);
    }
    let items = items
        .iter()
        .map(|c| PlanItem::from_candidate(c))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(Plan::new(
        PlanParams {
            id: new_plan_id(now_unix)?,
            scan_id: scan_id.clone(),
            created_unix: now_unix,
            ttl_secs: None,
            machine_id: who.machine_id.clone(),
            user: who.user.clone(),
            origin,
            operations_watermark: Some(operations_watermark),
        },
        items,
    )?))
}

// ---------------------------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------------------------

/// What a frontend supplies to execute a reviewed, confirmed plan.
pub struct ExecuteParams<'a> {
    /// The plan, verified and confirmed by the user.
    pub plan: &'a Plan,
    /// Who confirmed it (must match the plan's binding).
    pub who: &'a Binding,
    /// How it was confirmed (recorded in the audit log).
    pub method: ApprovalMethod,
    /// The scan roots revalidation re-derives candidates under.
    pub roots: &'a [PathBuf],
    /// The read-only engine for revalidation.
    pub engine: &'a Engine,
    /// The configured quarantine location (P2-EXEC-13).
    pub quarantine: QuarantineSettings,
    /// The Trash fallback (D-3: quarantine → Trash → blocked, never deleted).
    pub trash: TrashLocations,
    /// The scan's sizes per item, for the predicted reclaim.
    pub sizes: &'a BTreeMap<ItemId, Sizes>,
}

/// What an execution did, for the frontend's report.
#[derive(Debug)]
pub struct Run {
    /// The recorded operation.
    pub operation_id: OperationId,
    /// Per-item outcomes.
    pub report: ExecutionReport,
    /// When it started and finished (Unix seconds).
    pub started_at: i64,
    /// When it finished (Unix seconds).
    pub finished_at: i64,
    /// Predicted vs measured free space.
    pub delta: MeasuredDelta,
}

/// Each item's sizes, by ID.
pub fn sizes_by_id(cands: &[Candidate]) -> BTreeMap<ItemId, Sizes> {
    cands.iter().map(|c| (c.id.clone(), c.sizes)).collect()
}

/// Execute a reviewed plan with the `executor` from [`lock_and_recover`]: record the
/// operation, execute every item with revalidation, measure, finish. A cancelled or failed
/// run is recorded as cancelled; nothing is retried.
pub fn execute(
    executor: &Executor,
    store: &mut Store,
    p: ExecuteParams<'_>,
    cancel: &CancelToken,
) -> Result<Run, ScullError> {
    let started_at = now_unix();
    let operation_id = new_operation_id(started_at)?;
    let mut op = executor
        .begin_operation(
            store,
            p.plan,
            &OperationStart {
                operation_id: operation_id.clone(),
                plan_id: p.plan.id().clone(),
                content_hash: p.plan.content_hash().clone(),
                approver: p.who.user.clone(),
                method: p.method,
                started_at,
            },
        )?
        // One measurement: the meter below (with resampling for delayed frees) is what the
        // report shows and what `finish_measured` records (I-5).
        .without_measurement()
        .with_quarantine(p.quarantine)
        .with_trash(p.trash);
    let predicted = |i: &PlanItem| {
        ActionReclaim::for_action(
            &p.sizes.get(&i.item_id).copied().unwrap_or_default(),
            &i.action,
        )
    };
    let predicted_freed: u64 = p.plan.items().iter().map(|i| predicted(i).freed_now).sum();
    let mut options = MeasureOptions::default();
    if predicted_freed <= options.resolution {
        // Waiting for delayed frees can't resolve a difference this small.
        options = options.with_resampling(Duration::ZERO, Duration::ZERO);
    }
    let meter = VolumeMeter::new(PlatformFreeSpace, options);
    let baseline = meter.before(p.roots);
    let revalidation = Revalidation::new(p.engine, p.roots);
    let ctx = PlanContext {
        now_unix: now_unix(),
        machine_id: &p.who.machine_id,
        user: &p.who.user,
    };
    let report = match executor.execute_plan(p.plan, &ctx, &revalidation, Some(&mut op), cancel) {
        Ok(r) => r,
        Err(e) => {
            let _ = op.finish(OperationEnd::Cancelled, now_unix());
            return Err(e);
        }
    };
    let mut tally = Tally::new();
    for (item, r) in p.plan.items().iter().zip(&report.items) {
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
    Ok(Run {
        operation_id,
        report,
        started_at,
        finished_at,
        delta,
    })
}

/// What an item's outcome means for the free-space tally.
pub fn item_execution(r: &ItemReport) -> ItemExecution {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The example candidate (S0 delete) at `tier` with its matching action and reason, so it
    /// stays consistent (`is_consistent`).
    fn cand(n: u128, tier: Tier) -> Candidate {
        let mut c: Candidate =
            serde_json::from_str(include_str!("../../../schemas/v1/examples/candidate.json"))
                .expect("example candidate");
        let path = format!("/p/{n}");
        c.path = scull_core::FsPath::from(path.as_str());
        c.identity = scull_core::FileIdentity::new(7, n);
        c.id = ItemId::derive(c.identity, path.as_bytes());
        c.tier = tier;
        c.action = if tier == Tier::S0 {
            Action::Delete
        } else {
            Action::Quarantine
        };
        for r in &mut c.reasons {
            r.tier = tier;
        }
        c
    }

    #[test]
    fn selection_preselects_s0_opts_in_s1_and_plans_s2_only_when_picked() {
        let cs = [
            cand(1, Tier::S0),
            cand(2, Tier::S1),
            cand(3, Tier::S2),
            cand(4, Tier::S3),
        ];
        let ids = |s: &Selection<'_>| s.items.iter().map(|c| c.tier).collect::<Vec<_>>();
        let reason = |s: &Selection<'_>, t: Tier| {
            s.excluded
                .iter()
                .find(|(c, ..)| c.tier == t)
                .map(|(_, r, _)| *r)
        };

        let s = select(&cs, false, None);
        assert_eq!(ids(&s), [Tier::S0]);
        assert_eq!(reason(&s, Tier::S1), Some(ExcludeReason::OptInS1));
        assert_eq!(reason(&s, Tier::S2), Some(ExcludeReason::PerItemS2));
        assert_eq!(reason(&s, Tier::S3), Some(ExcludeReason::Protected));

        assert_eq!(ids(&select(&cs, true, None)), [Tier::S0, Tier::S1]);

        // Picking: exactly what was named, S2 included; S3 is never planned.
        let picked: BTreeSet<&ItemId> = [&cs[2].id, &cs[3].id].into_iter().collect();
        let s = select(&cs, false, Some(&picked));
        assert_eq!(ids(&s), [Tier::S2]);
        assert_eq!(reason(&s, Tier::S0), Some(ExcludeReason::NotSelected));
        assert_eq!(reason(&s, Tier::S3), Some(ExcludeReason::Protected));
    }

    #[test]
    fn elevated_confirmation_covers_unrestorable_s1_s2_only() {
        assert!(!needs_elevated_confirmation(Tier::S0, &Action::Delete));
        assert!(!needs_elevated_confirmation(Tier::S1, &Action::Quarantine));
        assert!(needs_elevated_confirmation(Tier::S2, &Action::Delete));
    }
}
