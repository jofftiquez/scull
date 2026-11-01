//! Executes one S0 item of a fixture through the real Executor with the measured free-space
//! delta, and records it in an audit store beside the root, so a fixture can check every
//! invariant including I-4 (audit matches disk) and I-5 (predicted and measured recorded).
//!
//! The audit rows are written here from the Executor's `DeleteReport`, with the same mapping the
//! Executor's own `Operation` uses (`scull-cleaner` `execution.rs`), because
//! `Operation::finish` cannot record a measured delta yet (it always writes `None`); this is
//! the only way to keep predicted and measured bytes in one operation record today.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

pub mod engine;

use scull_cleaner::measure::{
    FreeSpaceSource, ItemExecution, MeasureOptions, MeasuredDelta, PlatformFreeSpace, Tally,
    VolumeMeter,
};
use scull_cleaner::{DeleteReport, DeleteTarget, Executor, RemoveOutcome};
use scull_core::{
    Action, Category, FsPath, ItemId, OperationId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams,
    RuleRef, ScanId, Tier,
};
use scull_fixtures::{Fixture, InvariantInputs};
use scull_platform::{Native, Platform, native_path_bytes};
use scull_scanner::{
    ActionReclaim, CancelToken, EstimateContext, EstimateNote, NullSink, ScanOptions, Snapshots,
    assess, scan,
};
use scull_storage::{
    ApprovalMethod, DefaultPermissions, ItemRecord, OperationEnd, OperationStart, Store, WalkEnd,
    WalkSummary,
};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const T0: i64 = 1_800_000_000;

/// The estimate context of `path`'s volume, as the platform describes it.
pub fn context(path: &Path) -> EstimateContext {
    let v = PlatformFreeSpace.describe(path).unwrap();
    let snapshots = match v.snapshots {
        Some(false) => Snapshots::Absent,
        Some(true) => Snapshots::Possible,
        None => Snapshots::Unknown,
    };
    EstimateContext::new(v.fs, snapshots)
}

/// The prediction for deleting `rel`: what the action frees, the notes, and the scanned
/// logical and allocated totals.
pub struct Prediction {
    pub reclaim: ActionReclaim,
    pub notes: Vec<EstimateNote>,
    pub logical: u64,
    pub allocated: u64,
    pub confidence: scull_core::Confidence,
}

/// Scan `rel` with the real scanner and estimate it on its real volume.
pub fn predict(fx: &Fixture, rel: &str) -> Prediction {
    let outcome = scan(
        Arc::new(Native),
        &fx.root().join(rel),
        &ScanOptions::default(),
        &CancelToken::new(),
        NullSink,
    )
    .unwrap();
    let summary = outcome.root.expect("scan finished");
    let est = assess(&summary, &context(fx.root()));
    Prediction {
        reclaim: ActionReclaim::for_action(&est.sizes, &Action::Delete),
        notes: est.notes,
        logical: est.sizes.logical,
        allocated: est.sizes.allocated,
        confidence: est.sizes.reclaimable.confidence,
    }
}

/// One measured, audited deletion.
pub struct Measured {
    pub prediction: Prediction,
    pub item: PlanItem,
    pub report: DeleteReport,
    pub delta: MeasuredDelta,
    pub store: Store,
    pub op: OperationId,
    pub rel: String,
}

impl Measured {
    /// Inputs for every invariant this run can feed: I-1 (the plan), I-3 (only `rel` may
    /// change), I-4 (the audit) and I-5 (predicted and measured bytes of the operation).
    pub fn inputs(&self) -> InvariantInputs {
        InvariantInputs::new()
            .plan([(
                self.item.item_id.clone(),
                self.item.tier,
                self.item.action.clone(),
            )])
            .allow(&self.rel)
            .audit(&self.store, &self.op)
            .operation_bytes(&self.store, &self.op)
    }
}

/// Build the S0 delete plan item for `rel`, record the plan and start an operation in a store
/// beside the root, measure free space, delete through the Executor, measure again and finish
/// the operation with the measured delta.
pub fn delete_measured(fx: &Fixture, rel: &str) -> Measured {
    let prediction = predict(fx, rel);
    let path = fx.root().join(rel);
    let identity = Native.metadata(&path).unwrap().identity.expect("identity");
    let item = PlanItem {
        item_id: ItemId::derive(identity, &native_path_bytes(&path)),
        identity,
        path: FsPath::from_path(&path),
        category: Category::ProjectArtifact,
        rule: RuleRef {
            id: "fixture.measured".into(),
            version: 1,
        },
        tier: Tier::S0,
        action: Action::Delete,
        reclaimable_bytes: prediction.reclaim.freed_now,
    };
    let mut store = Store::open(
        &fx.base().join("data").join("scull.db"),
        &DefaultPermissions,
    )
    .unwrap();
    let plan = Plan::new(
        PlanParams {
            id: PlanId::from_suffix("PFX1").unwrap(),
            scan_id: ScanId::from_suffix("SFX1").unwrap(),
            created_unix: T0,
            ttl_secs: None,
            machine_id: "machine-test".into(),
            user: "tester".into(),
            origin: PlanOrigin::Cli,
            operations_watermark: None,
        },
        vec![item.clone()],
    )
    .unwrap();
    store.insert_plan(&plan).unwrap();
    let op = OperationId::from_suffix("OFX1").unwrap();
    let mut audit = store
        .begin_operation(
            &OperationStart {
                operation_id: op.clone(),
                plan_id: plan.id().clone(),
                content_hash: plan.content_hash().clone(),
                approver: "tester".into(),
                method: ApprovalMethod::CliInteractive,
                started_at: T0 + 1,
            },
            &DefaultPermissions,
        )
        .unwrap();

    let meter = VolumeMeter::new(
        PlatformFreeSpace,
        MeasureOptions::default()
            .with_samples(3)
            .with_resampling(Duration::from_millis(200), Duration::from_secs(3)),
    );
    let baseline = meter.before([fx.root()]);
    let report = Executor::new()
        .delete(
            &item,
            &DeleteTarget {
                anchor: fx.root(),
                anchor_identity: Native.metadata(fx.root()).unwrap().identity,
                path: &path,
            },
        )
        .unwrap();
    let r = &report.report;
    let walk = WalkSummary {
        end: match r.outcome {
            RemoveOutcome::Complete => WalkEnd::Complete,
            RemoveOutcome::Incomplete => WalkEnd::Incomplete,
            _ => WalkEnd::Aborted,
        },
        entries_removed: r.removed.files + r.removed.dirs + r.removed.symlinks + r.removed.special,
        freed_bytes: r.removed.allocated_bytes,
        entries_kept: r.skipped_total,
        reason: None,
    };
    store
        .record_item(
            &mut audit,
            &ItemRecord::from_walk(item.item_id.clone(), T0 + 2, &walk),
        )
        .unwrap();
    let mut tally = Tally::new();
    tally.add(
        &prediction.reclaim,
        &prediction.notes,
        ItemExecution::Done {
            removed: r.removed.allocated_bytes,
            moved: 0,
        },
    );
    let delta = meter.after(baseline, &tally);
    println!("{delta}");
    store
        .finish_operation(
            &mut audit,
            &delta.operation_finish(T0 + 3, OperationEnd::Normal),
        )
        .unwrap();
    Measured {
        prediction,
        item,
        report,
        delta,
        store,
        op,
        rel: rel.to_owned(),
    }
}
