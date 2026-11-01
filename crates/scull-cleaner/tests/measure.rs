//! Measured free-space delta on a real volume (P2-EXEC-10, PHY-02, FX-PHY-01, invariant I-5).
//!
//! Each test builds a `scull-fixtures` tree with a canary, predicts the reclaim with the real
//! scanner and estimator, deletes an S0 item through the Executor, measures the volume's free
//! space before and after with [`PlatformFreeSpace`], and checks the invariants.
//!
//! Free space is volume-wide: other processes on the same volume (parallel builds, other test
//! binaries) add noise. Each test retries a few times and, if every attempt is disturbed by
//! noise larger than the tolerance or the filesystem's accounting can't be measured precisely
//! (APFS purgeable space and snapshots, Btrfs/ZFS async frees, tmpfs), skips with the reason
//! printed instead of failing. The tests run one at a time (a shared lock) so they don't
//! disturb each other.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use scull_cleaner::measure::{
    FreeSpaceSource, ItemExecution, MeasureOptions, MeasuredDelta, PlatformFreeSpace, Tally,
    VolumeMeter,
};
use scull_core::{Action, PlanId, PlanItem, PlanOrigin, PlanParams, ScanId};
use scull_fixtures::Fixture;
use scull_platform::Native;
use scull_scanner::{
    ActionReclaim, CancelToken, EstimateContext, EstimateNote, NullSink, ScanOptions, Snapshots,
    VolumeFs, assess, scan,
};
use scull_storage::{
    ApprovalMethod, DefaultPermissions, ItemRecord, OperationEnd, OperationStart, Store, WalkEnd,
    WalkSummary,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MIB: u64 = 1024 * 1024;
const ATTEMPTS: usize = 5;

/// Serializes the tests of this binary: each one measures volume-wide free space.
static SERIAL: Mutex<()> = Mutex::new(());

fn options() -> MeasureOptions {
    MeasureOptions::default()
        .with_samples(5)
        .with_resampling(Duration::from_millis(500), Duration::from_secs(10))
}

/// Filesystems whose free-space accounting can't be held to the prediction in a test.
fn unreliable(fs: VolumeFs) -> Option<&'static str> {
    match fs {
        VolumeFs::Tmpfs => Some("tmpfs accounting"),
        VolumeFs::Apfs => Some("APFS purgeable space and local snapshots"),
        VolumeFs::Btrfs | VolumeFs::Zfs => Some("asynchronous frees and snapshots"),
        VolumeFs::Other | VolumeFs::Unknown => Some("unknown filesystem"),
        _ => None,
    }
}

/// The estimate context for `path`'s volume, as the platform describes it.
fn context(path: &Path) -> EstimateContext {
    let v = PlatformFreeSpace.describe(path).unwrap();
    let snapshots = match v.snapshots {
        Some(false) => Snapshots::Absent,
        Some(true) => Snapshots::Possible,
        None => Snapshots::Unknown,
    };
    EstimateContext::new(v.fs, snapshots)
}

/// Predict the reclaim of deleting the directory `rel` under the fixture root.
fn predict(fx: &Fixture, rel: &str) -> (ActionReclaim, Vec<EstimateNote>) {
    let path = fx.root().join(rel);
    let outcome = scan(
        Arc::new(Native),
        &path,
        &ScanOptions::default(),
        &CancelToken::new(),
        NullSink,
    )
    .unwrap();
    let summary = outcome.root.expect("scan finished");
    let est = assess(&summary, &context(fx.root()));
    (
        ActionReclaim::for_action(&est.sizes, &Action::Delete),
        est.notes,
    )
}

struct Run {
    fx: Fixture,
    item: PlanItem,
    predicted: ActionReclaim,
    delta: MeasuredDelta,
}

/// Build with `build`, predict, measure before, delete `rel` (S0) through the Executor,
/// measure after, and check the invariants (only `rel` may change).
fn run(build: &dyn Fn(&mut Fixture), rel: &str) -> Run {
    let mut fx = Fixture::new().unwrap();
    build(&mut fx);
    fx.checkpoint().unwrap();
    let (predicted, notes) = predict(&fx, rel);
    let mut item = s0_delete(&fx, rel);
    item.reclaimable_bytes = predicted.freed_now;

    let meter = VolumeMeter::new(PlatformFreeSpace, options());
    let baseline = meter.before([fx.root()]);
    assert_eq!(baseline.volume_count(), 1, "{:?}", baseline.unavailable());
    let report = delete(&fx, &item, rel).unwrap();
    assert_complete(&report);
    let mut tally = Tally::new();
    tally.add(
        &predicted,
        &notes,
        ItemExecution::Done {
            removed: report.report.removed.allocated_bytes,
            moved: 0,
        },
    );
    let delta = meter.after(baseline, &tally);
    println!("{delta}");
    check(&fx, &[&item], &[rel]);
    assert_canary_files(&fx);
    Run {
        fx,
        item,
        predicted,
        delta,
    }
}

/// Retry `attempt` until `ok` accepts the measurement; returns the accepted run or `None`
/// (after printing why) when every attempt was disturbed by a busy volume or the filesystem's
/// accounting is unreliable. Fails on a quiet, reliable volume.
fn accepted(name: &str, attempt: &dyn Fn() -> Run, ok: &dyn Fn(&Run) -> bool) -> Option<Run> {
    let mut last = None;
    for i in 1..=ATTEMPTS {
        let r = attempt();
        if ok(&r) {
            println!("{name}: accepted on attempt {i}");
            return Some(r);
        }
        println!(
            "{name}: attempt {i} rejected: {:?}",
            r.delta.explanation_codes()
        );
        last = Some(r);
    }
    let r = last.expect("attempts ran");
    let v = &r.delta.per_volume[0];
    if let Some(why) = unreliable(v.volume.fs) {
        println!(
            "SKIP {name}: {} [{}]: {why}",
            v.volume.mount_point.display(),
            v.volume.fs_type
        );
        return None;
    }
    if v.noise > r.delta.observations.resolution {
        println!(
            "SKIP {name}: volume too busy: sample noise {} B > resolution {} B on every attempt",
            v.noise, r.delta.observations.resolution
        );
        return None;
    }
    panic!(
        "{name}: measured delta disagrees with the prediction on a quiet {} volume:\n{}",
        v.volume.fs_type, r.delta
    );
}

/// A 64 MiB tree (16 × 4 MiB files in nested directories) deleted by the Executor: measured
/// free-space delta ≈ predicted reclaimable (PHY-02), and predicted and measured bytes are
/// both recorded in the audit log (I-5).
#[test]
fn delete_64_mib_measured_matches_predicted() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let build = |fx: &mut Fixture| {
        for i in 0..16 {
            fx.file_of_size(format!("cand/d{}/sub{}/f{i}.bin", i % 4, i % 2), 4 * MIB)
                .unwrap();
        }
        fx.file("keep/sibling.txt", b"must stay").unwrap();
    };
    let attempt = || run(&build, "cand");
    // Strict: within 10% of the prediction, not merely within noise.
    let ok = |r: &Run| r.delta.measured_total.is_some() && !r.delta.exceeds_threshold;
    let Some(r) = accepted("delete_64_mib", &attempt, &ok) else {
        return;
    };
    // The prediction itself: every byte unique, allocated ≥ the logical 64 MiB.
    assert!(r.predicted.freed_now >= 64 * MIB, "{:?}", r.predicted);
    assert!(r.predicted.freed_now < 66 * MIB, "{:?}", r.predicted);
    // The walker counts files only; the prediction also counts the directories' blocks.
    assert!(r.delta.removed <= r.predicted.freed_now);
    assert!(
        r.predicted.freed_now - r.delta.removed < MIB,
        "{:?}",
        r.predicted
    );
    assert!(r.fx.root().join("keep/sibling.txt").exists());
    let m = r.delta.measured_total.unwrap();
    println!(
        "accuracy: predicted {} B, measured {m} B, discrepancy {:+.2}%",
        r.predicted.freed_now,
        r.delta.discrepancy_pct.unwrap()
    );
    assert!(r.delta.discrepancy_pct.unwrap().abs() <= 10.0);

    // I-5: record the operation in a temp store; predicted and measured are both kept.
    let db = tempfile::tempdir().unwrap();
    let mut store = Store::open(&db.path().join("scull.db"), &DefaultPermissions).unwrap();
    let t0 = 1_800_000_000;
    let plan = scull_core::Plan::new(
        PlanParams {
            id: PlanId::from_suffix("PM1").unwrap(),
            scan_id: ScanId::from_suffix("SM1").unwrap(),
            created_unix: t0,
            ttl_secs: None,
            machine_id: "machine".into(),
            user: "dev".into(),
            origin: PlanOrigin::Cli,
            operations_watermark: None,
        },
        vec![r.item.clone()],
    )
    .unwrap();
    store.insert_plan(&plan).unwrap();
    let mut audit = store
        .begin_operation(
            &OperationStart {
                operation_id: scull_core::OperationId::from_suffix("OM1").unwrap(),
                plan_id: plan.id().clone(),
                content_hash: plan.content_hash().clone(),
                approver: "dev".into(),
                method: ApprovalMethod::CliInteractive,
                started_at: t0 + 1,
            },
            &DefaultPermissions,
        )
        .unwrap();
    let walk = WalkSummary {
        end: WalkEnd::Complete,
        entries_removed: 16 + 1 + 4 + 8,
        freed_bytes: r.delta.removed,
        entries_kept: 0,
        reason: None,
    };
    store
        .record_item(
            &mut audit,
            &ItemRecord::from_walk(r.item.item_id.clone(), t0 + 2, &walk),
        )
        .unwrap();
    let summary = store
        .finish_operation(
            &mut audit,
            &r.delta.operation_finish(t0 + 3, OperationEnd::Normal),
        )
        .unwrap();
    println!("I-5 totals: {:?}", summary.totals);
    assert_eq!(summary.totals.predicted_bytes, r.predicted.freed_now);
    assert_eq!(summary.totals.measured_delta_bytes, Some(m));
    assert_eq!(summary.totals.freed_bytes, r.delta.removed);
}

/// FX-PHY-01: a pnpm-style `node_modules` hard-linked to a store predicts ≈ 0 with an
/// explanation ("shared with store"), and the measured delta agrees (≈ 0).
#[test]
fn fx_phy_01_hardlinked_node_modules_predicts_and_measures_zero() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let build = |fx: &mut Fixture| {
        for i in 0..4 {
            fx.file_of_size(format!("store/v3/files/{i:02}/pkg.bin"), 4 * MIB)
                .unwrap()
                .hardlink(
                    format!("proj/node_modules/.pnpm/pkg{i}/pkg.bin"),
                    format!("store/v3/files/{i:02}/pkg.bin"),
                )
                .unwrap();
        }
    };
    let attempt = || run(&build, "proj/node_modules");
    // Strict: |measured| below 10% of the 16 MiB the walker unlinked.
    let ok = |r: &Run| {
        r.delta
            .measured_total
            .is_some_and(|m| m.unsigned_abs() * 10 < r.delta.removed)
    };
    let Some(r) = accepted("fx_phy_01", &attempt, &ok) else {
        return;
    };
    // ≈ 0: only the directories' own blocks are unique to node_modules.
    assert!(r.predicted.freed_now < MIB, "{:?}", r.predicted);
    assert!(r.delta.removed >= 16 * MIB, "the walker unlinked the links");
    assert!(
        r.delta.explanation_codes().contains(&"shared_hardlinks"),
        "{}",
        r.delta
    );
    // The store keeps its data.
    for i in 0..4 {
        let p = r.fx.root().join(format!("store/v3/files/{i:02}/pkg.bin"));
        assert_eq!(std::fs::metadata(p).unwrap().len(), 4 * MIB);
    }
}

/// The hard-link prediction carries the explanation for the preview and per-item notes.
#[test]
fn fx_phy_01_prediction_explains_shared_bytes() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("store/pkg.bin", 2 * MIB)
        .unwrap()
        .hardlink("proj/node_modules/pkg.bin", "store/pkg.bin")
        .unwrap()
        .checkpoint()
        .unwrap();
    let (predicted, notes) = predict(&fx, "proj/node_modules");
    assert!(
        predicted.freed_now < MIB,
        "≈ 0 (directory blocks only): {predicted:?}"
    );
    let item = scull_cleaner::measure::Explanation::for_item(&predicted, &notes);
    let codes: Vec<_> = item.iter().map(|e| e.code()).collect();
    assert!(codes.contains(&"shared_hardlinks"), "{codes:?}");
    for e in &item {
        println!("  - {}: {}", e.code(), e.message());
    }
    check(&fx, &[], &[]);
}
