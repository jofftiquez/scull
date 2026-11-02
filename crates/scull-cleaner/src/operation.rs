//! One executed operation: its database record, targets, audit trail and quarantine folder.
//!
//! [`Executor::begin_operation`] (under the execution lock) refuses a plan whose targets
//! another operation touched since its scan observed them (SAFE-09, exit code 4: every
//! operation recorded after the plan's [operations watermark](Plan::operations_watermark), or
//! for plans without one, started at or after the plan's creation), then records
//! the operation (`operations` row + `started` audit row, SAFE-08) and the identity of every
//! target. [`Executor::execute_plan`] records one audit outcome per item through it, and
//! quarantines into its folder (D-3). [`Operation::finish`] closes the audit.
//!
//! # Agent plans need a human approval (MCP-04, PRD §13.3)
//!
//! A plan made by the MCP server ([`PlanOrigin::Mcp`]) starts an operation only by
//! **consuming** its approval: a record written by a human through `scull approve` in an
//! interactive terminal or the desktop dialog ([`ApprovalMethod::CliTty`],
//! [`ApprovalMethod::Desktop`]), bound to the plan's exact content hash, unexpired and unused.
//! Consumption is one atomic database update, so an approval starts at most one operation even
//! when several executors race. The approval must have been given by the account the
//! Executor runs as (read from the OS here, `scull_platform::current_user`), which the
//! operation records as its approver, with the approval's channel.
//! [`Executor::execute_plan`] refuses an MCP plan outside such an operation. This is the
//! Executor's own check: whatever calls it (the MCP server, the CLI, a bug), an agent's plan
//! never runs on the agent's say-so. (Approval rows are not authenticated beyond the
//! database's owner-only permissions: a process that can write the user's database can
//! insert one; see `docs/THREAT_MODEL.md` T-41.)
//!
//! A human can revoke the approval before it is used (#298, `Store::revoke_approval`): the
//! revocation spends it with the same single-use marker, so it and this consumption can't
//! both succeed (exactly one update changes the row); a revoked approval is refused here.
//!
//! Every refusal to start (or run) an agent plan is audited as an
//! [`EventKind::ExecuteRefused`] event with source `executor` (or the
//! [`RefusalSource`](crate::RefusalSource) the Executor was given, `cli` for
//! `scull approve --run`) and a stable reason code
//! ([`GATE_REASONS`]; PRD §13.3 step 5). Recording it never touches the approval.

use crate::executor::Executor;
use crate::measure::{Baseline, FreeSpaceSource, MeasuredDelta, Meter, Tally, VolumeMeter};
use crate::quarantine::{QuarantineFolder, QuarantineSettings};
use crate::trash::TrashLocations;
use scull_core::{
    ContentHash, ErrorKind, OperationId, Plan, PlanId, PlanItem, PlanOrigin, ScullError,
};
use scull_engine::{PlatformOwnerOnly, now_unix};
use scull_storage::{
    AgentAuditEvent, ApprovalMethod, EventKind, ItemRecord, OperationAudit, OperationEnd,
    OperationFinish, OperationStart, OperationSummary, Store,
};

/// The reason codes the Executor's gate records when it refuses an agent plan (see the
/// module docs): `lock_not_held`, `operation_mismatch`, `unfinished_operation`,
/// `targets_touched`, `not_approved`, `not_human`, `approval_mismatch`, `account_unknown`,
/// `other_account`, `approver_not_recorded`, `channel_mismatch`, `approval_used`,
/// `approval_revoked`, `approval_expired`, `outside_operation`, `store` and
/// `auto_policy_method_mismatch` (an agent plan presented with the auto-policy method, D-11).
pub const GATE_REASONS: [&str; 17] = [
    "lock_not_held",
    "operation_mismatch",
    "unfinished_operation",
    "targets_touched",
    "not_approved",
    "not_human",
    "approval_mismatch",
    "account_unknown",
    "other_account",
    "approver_not_recorded",
    "channel_mismatch",
    "approval_used",
    "approval_revoked",
    "approval_expired",
    "outside_operation",
    "store",
    "auto_policy_method_mismatch",
];

/// A refusal and its audited reason code (one of [`GATE_REASONS`]).
type Refusal = (ScullError, &'static str);

fn store_refusal(e: scull_storage::StoreError) -> Refusal {
    (ScullError::from(e), "store")
}

/// Audit the gate's refusal of the agent plan `plan`, attributed to `by` (best effort: the
/// refusal stands even if it can't be recorded). The user is the account the Executor runs as when the OS can
/// tell, else the plan's.
pub(crate) fn audit_refusal(
    store: &mut Store,
    plan: &Plan,
    reason: &'static str,
    by: crate::RefusalSource,
) {
    let user = scull_platform::current_user().unwrap_or_else(|_| plan.user().to_owned());
    let _ = store.record_agent_event(&AgentAuditEvent {
        event_id: 0,
        at: now_unix(),
        kind: EventKind::ExecuteRefused,
        source: by.source.into(),
        plan_id: Some(plan.id().clone()),
        content_hash: Some(plan.content_hash().clone()),
        reason: reason.into(),
        channel: by.channel.into(),
        machine_id: plan.machine_id().to_owned(),
        user,
        count: 1,
    });
}

/// An operation in progress. Borrowing the store keeps every record of the operation in one
/// place; drop it only after [`Operation::finish`] (an unfinished operation is what crash
/// recovery, [`Executor::recover`], picks up).
#[derive(Debug)]
pub struct Operation<'s> {
    pub(crate) store: &'s mut Store,
    pub(crate) audit: OperationAudit,
    pub(crate) quarantine: QuarantineSettings,
    pub(crate) folders: Vec<QuarantineFolder>,
    /// The journal intent of the step in progress, until it is closed (SAFE-19).
    pub(crate) open_step: Option<i64>,
    /// Where the Trash fallback moves items whose quarantine is impossible (D-3). `None`:
    /// no fallback, such items are blocked.
    pub(crate) trash: Option<TrashLocations>,
    /// Measures the free-space delta around `execute_plan` (PHY-02, I-5).
    pub(crate) meter: Option<Box<dyn Meter>>,
    /// The measured delta of the last `execute_plan` (or the sum of the Quick Clean runs'),
    /// recorded by [`Operation::finish`].
    pub(crate) measured: Option<i64>,
    /// The items of the plan the operation was started for: Quick Clean acts only on an item
    /// that is exactly one of these (what was approved and is audited, SAFE-08).
    pub(crate) plan_items: Vec<PlanItem>,
    /// The ID and content hash of that plan ([`Executor::execute_plan`] runs an MCP plan only
    /// inside the operation that consumed its approval).
    pub(crate) plan: (PlanId, ContentHash),
    /// The plan is an auto-policy plan (D-11): never a Trash fallback ([`Operation::with_trash`]).
    pub(crate) policy_plan: bool,
}

impl Operation<'_> {
    /// The operation ID.
    pub fn id(&self) -> &OperationId {
        self.audit.operation_id()
    }

    /// The store.
    pub fn store(&mut self) -> &mut Store {
        self.store
    }

    /// Use these quarantine settings (location override, retention) for this operation.
    #[must_use]
    pub fn with_quarantine(mut self, settings: QuarantineSettings) -> Self {
        self.quarantine = settings;
        self
    }

    /// Enable the Trash fallback (P2-EXEC-07, D-3): an S1/S2 item whose same-volume quarantine
    /// is impossible (another volume, no writable or private quarantine location, read-only,
    /// no no-replace rename) is moved to the Trash at `locations` instead; if that is
    /// impossible too, it is blocked. Never deleted as a fallback. Without this call there is
    /// no fallback (such items are blocked). Interfaces pass [`TrashLocations::from_env`];
    /// tests point it inside their fixture.
    ///
    /// Ignored for an auto-policy plan ([`PlanOrigin::Policy`], D-11): the OS may empty the
    /// Trash on its own, which would be a permanent deletion nobody clicked, so an item whose
    /// quarantine is impossible is blocked (left in place, audited) instead.
    #[must_use]
    pub fn with_trash(mut self, locations: TrashLocations) -> Self {
        if !self.policy_plan {
            self.trash = Some(locations);
        }
        self
    }

    /// Measure the free-space delta of the next [`Executor::execute_plan`] with `meter`
    /// (default: [`VolumeMeter::platform_quick`]). The result is in
    /// [`ExecutionReport::measurement`](crate::ExecutionReport::measurement) and recorded by
    /// [`Operation::finish`] (`measured_delta_bytes`, invariant I-5).
    #[must_use]
    pub fn with_meter<S: FreeSpaceSource + std::fmt::Debug + Send + 'static>(
        mut self,
        meter: VolumeMeter<S>,
    ) -> Self {
        self.meter = Some(Box::new(meter));
        self
    }

    /// Don't measure the free-space delta (the operation then records none).
    #[must_use]
    pub fn without_measurement(mut self) -> Self {
        self.meter = None;
        self
    }

    /// Whether this operation was started for exactly `plan` (same ID and content hash).
    pub(crate) fn is_for(&self, plan: &Plan) -> bool {
        self.plan.0 == *plan.id() && self.plan.1 == *plan.content_hash()
    }

    /// Whether `item` is, field for field, one of the items of the operation's plan.
    pub(crate) fn approved(&self, item: &PlanItem) -> bool {
        self.plan_items.iter().any(|i| i == item)
    }

    /// Free space of the volumes holding `paths`, before a step (`None` without a meter).
    pub(crate) fn measure_before(&self, paths: &[std::path::PathBuf]) -> Option<Baseline> {
        self.meter.as_ref().map(|m| m.before(paths))
    }

    /// Free space after a step measured from `baseline`; adds the delta to what
    /// [`Operation::finish`] records (several Quick Clean steps add up).
    pub(crate) fn measure_after(
        &mut self,
        baseline: Option<Baseline>,
        tally: &Tally,
    ) -> Option<MeasuredDelta> {
        let delta = self.meter.as_ref()?.after(baseline?, tally);
        if let Some(d) = delta.measured_delta_bytes() {
            self.measured = Some(self.measured.unwrap_or(0).saturating_add(d));
        }
        Some(delta)
    }

    /// Record one item outcome in the audit log.
    pub(crate) fn record(&mut self, rec: &ItemRecord) -> Result<(), ScullError> {
        self.store
            .record_item(&mut self.audit, rec)
            .map(drop)
            .map_err(ScullError::from)
    }

    /// Close the audit (`end` says whether every item was handled or the run was cancelled)
    /// with the free-space delta measured by the last [`Executor::execute_plan`] (I-5), then
    /// write the on-disk manifest copy of each quarantine folder it used.
    pub fn finish(
        self,
        end: OperationEnd,
        finished_at: i64,
    ) -> Result<OperationSummary, ScullError> {
        let measured = self.measured;
        self.finish_measured(end, finished_at, measured)
    }

    /// [`Operation::finish`] with an explicit measured free-space delta in bytes (positive =
    /// space gained), e.g. from the caller's own [`VolumeMeter`] spanning several runs.
    pub fn finish_measured(
        mut self,
        end: OperationEnd,
        finished_at: i64,
        measured_delta_bytes: Option<i64>,
    ) -> Result<OperationSummary, ScullError> {
        let summary = self
            .store
            .finish_operation(
                &mut self.audit,
                &OperationFinish {
                    finished_at,
                    end,
                    measured_delta_bytes,
                },
            )
            .map_err(ScullError::from)?;
        for folder in &self.folders {
            folder.write_disk_manifest(self.store, self.audit.operation_id())?;
        }
        Ok(summary)
    }
}

impl Executor {
    /// Start an operation for `plan` (which must already be recorded in `store`), under the
    /// execution lock. Refuses with [`ErrorKind::SafetyBlocked`] while an earlier operation is
    /// unfinished (a crash: run [`Executor::recover`] first, SAFE-19). Refuses with
    /// [`ErrorKind::PlanInvalidated`] (exit 4) when another operation targeted any of its items
    /// after the plan's scan observed them (SAFE-09): one recorded after the plan's
    /// [`Plan::operations_watermark`], taken when the scan started; for a plan without one, one
    /// started at or after the plan's creation. Otherwise records the operation, its `started`
    /// audit row and the identity of every plan item.
    ///
    /// A plan made by the MCP server ([`PlanOrigin::Mcp`]) additionally needs a human approval
    /// (MCP-04): this consumes it atomically and refuses with [`ErrorKind::SafetyBlocked`]
    /// (exit 5) when there is none, it was not given through a human channel or by the account
    /// this process runs as, it expired or it was already used, and with
    /// [`ErrorKind::PlanTampered`] when it is bound to other content or `start` does not record
    /// this account as the approver and the approval's channel. See the module docs.
    ///
    /// An auto-policy plan ([`PlanOrigin::Policy`], D-11) starts only with
    /// [`ApprovalMethod::AutoPolicy`], and that method only for such a plan; otherwise
    /// [`ErrorKind::SafetyBlocked`].
    pub fn begin_operation<'s>(
        &self,
        store: &'s mut Store,
        plan: &Plan,
        start: &OperationStart,
    ) -> Result<Operation<'s>, ScullError> {
        if let Err((e, reason)) = self.admit(store, plan, start) {
            if plan.origin() == PlanOrigin::Mcp {
                audit_refusal(store, plan, reason, self.refusal_source());
            }
            return Err(e);
        }
        let audit = store
            .begin_operation(start, &PlatformOwnerOnly)
            .map_err(ScullError::from)?;
        let targets: Vec<_> = plan
            .items()
            .iter()
            .map(|i| (i.item_id.clone(), i.identity))
            .collect();
        store
            .record_operation_targets(&start.operation_id, &targets, start.started_at)
            .map_err(ScullError::from)?;
        Ok(Operation {
            store,
            audit,
            quarantine: QuarantineSettings::default(),
            folders: Vec::new(),
            open_step: None,
            trash: None,
            meter: Some(Box::new(VolumeMeter::platform_quick())),
            measured: None,
            plan_items: plan.items().to_vec(),
            plan: (plan.id().clone(), plan.content_hash().clone()),
            policy_plan: plan.origin() == PlanOrigin::Policy,
        })
    }

    /// Every check [`Executor::begin_operation`] makes before recording anything; for an agent
    /// plan the last one consumes its approval.
    fn admit(&self, store: &mut Store, plan: &Plan, start: &OperationStart) -> Result<(), Refusal> {
        if self.lock().is_none() {
            return Err((
                ScullError::new(
                    ErrorKind::SafetyBlocked,
                    "operations start only while holding the execution lock (SAFE-09)",
                ),
                "lock_not_held",
            ));
        }
        if start.plan_id != *plan.id() || start.content_hash != *plan.content_hash() {
            return Err((
                ScullError::new(
                    ErrorKind::PlanTampered,
                    "the operation does not match the plan",
                ),
                "operation_mismatch",
            ));
        }
        // D-11: an auto-policy plan starts only as an auto-policy operation, and no other plan
        // may claim that (click-less) method.
        let policy_plan = plan.origin() == PlanOrigin::Policy;
        if policy_plan != (start.method == ApprovalMethod::AutoPolicy) {
            return Err((
                ScullError::new(
                    ErrorKind::SafetyBlocked,
                    if policy_plan {
                        "an auto-policy plan runs only as an auto-policy operation (D-11); \
                         nothing was changed"
                    } else {
                        "only auto-policy plans run without a human confirmation (D-11); \
                         nothing was changed"
                    },
                ),
                "auto_policy_method_mismatch",
            ));
        }
        let pending = store.unfinished_operations().map_err(store_refusal)?;
        if !pending.is_empty() {
            let ids: Vec<&str> = pending.iter().map(OperationId::as_str).collect();
            return Err((
                ScullError::new(
                    ErrorKind::SafetyBlocked,
                    format!(
                        "an earlier operation did not finish ({}): run crash recovery \
                         (Executor::recover) before starting a new one (SAFE-19)",
                        ids.join(", ")
                    ),
                ),
                "unfinished_operation",
            ));
        }
        let identities: Vec<_> = plan.items().iter().map(|i| i.identity).collect();
        let touched = store
            .operations_touching(
                &identities,
                plan.created_unix(),
                plan.operations_watermark(),
                None,
            )
            .map_err(store_refusal)?;
        if !touched.is_empty() {
            let ids: Vec<&str> = touched.iter().map(OperationId::as_str).collect();
            return Err((
                ScullError::new(
                    ErrorKind::PlanInvalidated,
                    format!(
                        "another operation touched this plan's targets after its scan \
                         observed them ({}); create a new plan",
                        ids.join(", ")
                    ),
                ),
                "targets_touched",
            ));
        }
        // Last check before anything is recorded, so a refusal above never spends the approval.
        if plan.origin() == PlanOrigin::Mcp {
            consume_human_approval(store, plan, start)?;
        }
        Ok(())
    }
}

/// MCP-04 (PRD §13.3 step 4): consume the human approval of the agent-prepared `plan`, or
/// refuse. The approval must exist, come from a human channel (the `scull approve` terminal
/// or the desktop dialog), be bound to the plan's exact content hash, have been given by the
/// account this process runs as (the OS's answer, not the caller's), which `start` must record
/// as the approver along with the approval's channel; consumption ([`Store::consume_approval`]) is one atomic update that also requires
/// it to be unused and unexpired at the real current time (not the caller's `started_at`).
fn consume_human_approval(
    store: &mut Store,
    plan: &Plan,
    start: &OperationStart,
) -> Result<(), Refusal> {
    let blocked = |m: String| ScullError::new(ErrorKind::SafetyBlocked, m);
    let Some(approval) = store.get_approval(plan.id()).map_err(store_refusal)? else {
        return Err((
            blocked(format!(
                "plan {id} was prepared by an AI agent and has no human approval: the user approves \
             it with `scull approve {id}` in their own terminal (MCP-04); nothing was changed",
                id = plan.id()
            )),
            "not_approved",
        ));
    };
    if !matches!(
        approval.method,
        ApprovalMethod::CliTty | ApprovalMethod::Desktop
    ) {
        return Err((
            blocked(format!(
                "plan {} was prepared by an AI agent; its approval was not given in the `scull \
             approve` terminal or the desktop dialog (MCP-04); nothing was changed",
                plan.id()
            )),
            "not_human",
        ));
    }
    if approval.content_hash != *plan.content_hash() {
        return Err((
            ScullError::new(
                ErrorKind::PlanTampered,
                format!(
                    "the approval of plan {} is bound to different plan content; nothing was changed",
                    plan.id()
                ),
            ),
            "approval_mismatch",
        ));
    }
    // The approver must be the account this Executor runs as, read from the OS here (not
    // taken from the caller or the approval): an approval recorded for another account never
    // runs, and the audit records who actually executed.
    let owner = scull_platform::current_user().map_err(|e| {
        (
            blocked(format!(
                "cannot tell which account scull runs as ({e}), so the approval of plan {} can't \
                 be checked; nothing was changed",
                plan.id()
            )),
            "account_unknown",
        )
    })?;
    if approval.approver != owner {
        return Err((
            blocked(format!(
                "plan {} was approved by another account; approvals run only for the account \
                 that gave them; nothing was changed",
                plan.id()
            )),
            "other_account",
        ));
    }
    if start.approver != owner {
        return Err((
            ScullError::new(
                ErrorKind::PlanTampered,
                "the operation must record the account that runs it as the approver (SAFE-08)",
            ),
            "approver_not_recorded",
        ));
    }
    // Audit consistency only: the channel is the approval's (the caller copies it).
    if start.method != approval.method {
        return Err((
            ScullError::new(
                ErrorKind::PlanTampered,
                "the operation must record the approval's channel (SAFE-08)",
            ),
            "channel_mismatch",
        ));
    }
    let consumed = store
        .consume_approval(plan.id(), plan.content_hash(), now_unix())
        .map_err(store_refusal)?;
    if !consumed {
        // Which, for the audit: revoked by the human (#298), spent already, or lapsed unused.
        let used = store
            .get_approval(plan.id())
            .ok()
            .flatten()
            .is_some_and(|a| a.used_at.is_some());
        if used && store.approval_revoked(plan.id()).unwrap_or(false) {
            return Err((
                blocked(format!(
                    "the user revoked the approval of plan {}: it will not run; nothing was \
                     changed",
                    plan.id()
                )),
                "approval_revoked",
            ));
        }
        return Err((
            blocked(format!(
                "the approval of plan {} has expired or was already used (approvals are \
                 single-use and lapse with the plan): the agent must prepare a new plan; nothing \
                 was changed",
                plan.id()
            )),
            if used {
                "approval_used"
            } else {
                "approval_expired"
            },
        ));
    }
    Ok(())
}
