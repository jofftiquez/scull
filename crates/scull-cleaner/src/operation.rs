//! One executed operation: its database record, targets, audit trail and quarantine folder.
//!
//! [`Executor::begin_operation`] (under the execution lock) refuses a plan whose targets
//! another operation touched since the plan was created (SAFE-09, exit code 4), then records
//! the operation (`operations` row + `started` audit row, SAFE-08) and the identity of every
//! target. [`Executor::execute_plan`] records one audit outcome per item through it, and
//! quarantines into its folder (D-3). [`Operation::finish`] closes the audit.

use crate::executor::Executor;
use crate::quarantine::{QuarantineFolder, QuarantineSettings};
use scull_core::{ErrorKind, OperationId, Plan, ScullError};
use scull_engine::PlatformOwnerOnly;
use scull_storage::{
    ItemRecord, OperationAudit, OperationEnd, OperationFinish, OperationStart, OperationSummary,
    Store,
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

    /// Record one item outcome in the audit log.
    pub(crate) fn record(&mut self, rec: &ItemRecord) -> Result<(), ScullError> {
        self.store
            .record_item(&mut self.audit, rec)
            .map(drop)
            .map_err(ScullError::from)
    }

    /// Close the audit (`end` says whether every item was handled or the run was cancelled),
    /// then write the on-disk manifest copy of each quarantine folder it used.
    pub fn finish(
        mut self,
        end: OperationEnd,
        finished_at: i64,
    ) -> Result<OperationSummary, ScullError> {
        let summary = self
            .store
            .finish_operation(
                &mut self.audit,
                &OperationFinish {
                    finished_at,
                    end,
                    measured_delta_bytes: None,
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
        })
    }
}
