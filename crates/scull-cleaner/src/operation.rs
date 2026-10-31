//! One executed operation: its database record, targets, audit trail and quarantine folder.
//!
//! [`Executor::begin_operation`] (under the execution lock) refuses a plan whose targets
//! another operation touched since the plan was created (SAFE-09, exit code 4), then records
//! the operation (`operations` row + `started` audit row, SAFE-08) and the identity of every
//! target. [`Executor::execute_plan`] records one audit outcome per item through it, and
//! quarantines into its folder (D-3). [`Operation::finish`] closes the audit.

use crate::executor::Executor;
use crate::measure::{FreeSpaceSource, Meter, VolumeMeter};
use crate::quarantine::{QuarantineFolder, QuarantineSettings};
use crate::trash::TrashLocations;
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
    /// Where the Trash fallback moves items whose quarantine is impossible (D-3). `None`:
    /// no fallback, such items are blocked.
    pub(crate) trash: Option<TrashLocations>,
    /// Measures the free-space delta around `execute_plan` (PHY-02, I-5).
    pub(crate) meter: Option<Box<dyn Meter>>,
    /// The measured delta of the last `execute_plan`, recorded by [`Operation::finish`].
    pub(crate) measured: Option<i64>,
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
            trash: None,
            meter: Some(Box::new(VolumeMeter::platform_quick())),
            measured: None,
        })
    }
}
