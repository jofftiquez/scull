//! One executed operation: its database record, targets, audit trail and quarantine folder.
//!
//! [`Executor::begin_operation`] (under the execution lock) refuses a plan whose targets
//! another operation touched since the plan was created (SAFE-09, exit code 4), then records
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

use crate::executor::Executor;
use crate::measure::{Baseline, FreeSpaceSource, MeasuredDelta, Meter, Tally, VolumeMeter};
use crate::quarantine::{QuarantineFolder, QuarantineSettings};
use crate::trash::TrashLocations;
use scull_core::{
    ContentHash, ErrorKind, OperationId, Plan, PlanId, PlanItem, PlanOrigin, ScullError,
};
use scull_engine::{PlatformOwnerOnly, now_unix};
use scull_storage::{
    ApprovalMethod, ItemRecord, OperationAudit, OperationEnd, OperationFinish, OperationStart,
    OperationSummary, Store,
};

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
    #[must_use]
    pub fn with_trash(mut self, locations: TrashLocations) -> Self {
        self.trash = Some(locations);
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
    /// unfinished (a crash: run [`Executor::recover`] first, SAFE-19). Refuses with [`ErrorKind::PlanInvalidated`] (exit 4) when another
    /// operation that started at or after the plan's creation targeted any of its items
    /// (SAFE-09); otherwise records the operation, its `started` audit row and the identity of
    /// every plan item.
    ///
    /// A plan made by the MCP server ([`PlanOrigin::Mcp`]) additionally needs a human approval
    /// (MCP-04): this consumes it atomically and refuses with [`ErrorKind::SafetyBlocked`]
    /// (exit 5) when there is none, it was not given through a human channel or by the account
    /// this process runs as, it expired or it was already used, and with
    /// [`ErrorKind::PlanTampered`] when it is bound to other content or `start` does not record
    /// this account as the approver and the approval's channel. See the module docs.
    pub fn begin_operation<'s>(
        &self,
        store: &'s mut Store,
        plan: &Plan,
        start: &OperationStart,
    ) -> Result<Operation<'s>, ScullError> {
        if self.lock().is_none() {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "operations start only while holding the execution lock (SAFE-09)",
            ));
        }
        if start.plan_id != *plan.id() || start.content_hash != *plan.content_hash() {
            return Err(ScullError::new(
                ErrorKind::PlanTampered,
                "the operation does not match the plan",
            ));
        }
        let pending = store.unfinished_operations().map_err(ScullError::from)?;
        if !pending.is_empty() {
            let ids: Vec<&str> = pending.iter().map(OperationId::as_str).collect();
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!(
                    "an earlier operation did not finish ({}): run crash recovery \
                     (Executor::recover) before starting a new one (SAFE-19)",
                    ids.join(", ")
                ),
            ));
        }
        let identities: Vec<_> = plan.items().iter().map(|i| i.identity).collect();
        let touched = store
            .operations_touching(&identities, plan.created_unix(), None)
            .map_err(ScullError::from)?;
        if !touched.is_empty() {
            let ids: Vec<&str> = touched.iter().map(OperationId::as_str).collect();
            return Err(ScullError::new(
                ErrorKind::PlanInvalidated,
                format!(
                    "another operation touched this plan's targets since it was created ({}); \
                     create a new plan",
                    ids.join(", ")
                ),
            ));
        }
        // Last check before anything is recorded, so a refusal above never spends the approval.
        if plan.origin() == PlanOrigin::Mcp {
            consume_human_approval(store, plan, start)?;
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
        })
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
) -> Result<(), ScullError> {
    let blocked = |m: String| ScullError::new(ErrorKind::SafetyBlocked, m);
    let Some(approval) = store.get_approval(plan.id()).map_err(ScullError::from)? else {
        return Err(blocked(format!(
            "plan {id} was prepared by an AI agent and has no human approval: the user approves \
             it with `scull approve {id}` in their own terminal (MCP-04); nothing was changed",
            id = plan.id()
        )));
    };
    if !matches!(
        approval.method,
        ApprovalMethod::CliTty | ApprovalMethod::Desktop
    ) {
        return Err(blocked(format!(
            "plan {} was prepared by an AI agent; its approval was not given in the `scull \
             approve` terminal or the desktop dialog (MCP-04); nothing was changed",
            plan.id()
        )));
    }
    if approval.content_hash != *plan.content_hash() {
        return Err(ScullError::new(
            ErrorKind::PlanTampered,
            format!(
                "the approval of plan {} is bound to different plan content; nothing was changed",
                plan.id()
            ),
        ));
    }
    // The approver must be the account this Executor runs as, read from the OS here (not
    // taken from the caller or the approval): an approval recorded for another account never
    // runs, and the audit records who actually executed.
    let owner = scull_platform::current_user().map_err(|e| {
        blocked(format!(
            "cannot tell which account scull runs as ({e}), so the approval of plan {} can't be \
             checked; nothing was changed",
            plan.id()
        ))
    })?;
    if approval.approver != owner {
        return Err(blocked(format!(
            "plan {} was approved by another account; approvals run only for the account that \
             gave them; nothing was changed",
            plan.id()
        )));
    }
    if start.approver != owner {
        return Err(ScullError::new(
            ErrorKind::PlanTampered,
            "the operation must record the account that runs it as the approver (SAFE-08)",
        ));
    }
    // Audit consistency only: the channel is the approval's (the caller copies it).
    if start.method != approval.method {
        return Err(ScullError::new(
            ErrorKind::PlanTampered,
            "the operation must record the approval's channel (SAFE-08)",
        ));
    }
    let consumed = store
        .consume_approval(plan.id(), plan.content_hash(), now_unix())
        .map_err(ScullError::from)?;
    if !consumed {
        return Err(blocked(format!(
            "the approval of plan {} has expired or was already used (approvals are single-use \
             and lapse with the plan): the agent must prepare a new plan; nothing was changed",
            plan.id()
        )));
    }
    Ok(())
}
