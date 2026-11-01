//! Measured free-space delta on a real volume (P2-EXEC-10, PHY-02, FX-PHY-01, invariant I-5).
//!
//! Each test builds a `scull-fixtures` tree with a canary, predicts the reclaim with the real
//! scanner and estimator, deletes an S0 item through the Executor, measures the volume's free
//! space before and after with [`PlatformFreeSpace`], and checks the invariants.
//!
//! Free space is volume-wide: any other process writing to the same volume during the
//! measurement shows up in the delta. So the strict measurements run on a **private volume**:
//! a tmpfs mounted at `<root>/vol` in the test's own mount namespace (Linux, through the
//! privileged fixture harness: root, an unprivileged user+mount namespace, or `sudo -n` with
//! `SCULL_FIXTURES_ALLOW_SUDO=1`, which CI sets). Nothing else can write there, so the delta is
//! the operation's alone and is held to the prediction with no retries and no skips.
//!
//! Where no private volume can be had (macOS, Windows, a Linux host without those privileges),
//! the test runs on the shared temp volume and asserts the product's own contract instead: the
//! delta is measured and recorded, and a discrepancy beyond the 10% threshold always carries an
//! explanation (`concurrent_activity` last, plus the specific ones), never a silent mismatch.
//! The noise budget seen (sample spread, resolution) is printed with the result.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use scull_cleaner::measure::{
    FreeSpaceSource, ItemExecution, MeasureOptions, MeasuredDelta, PlatformFreeSpace, Tally,
    VolumeMeter,
};
use scull_cleaner::{DeleteTarget, Executor};
use scull_core::{Action, PlanId, PlanItem, PlanOrigin, PlanParams, ScanId};
use scull_fixtures::privileged::{self, Need, Privileges};
use scull_fixtures::{Fixture, Mounts};
use scull_platform::Native;
use scull_scanner::{
    ActionReclaim, CancelToken, EstimateContext, EstimateNote, NullSink, ScanOptions, Snapshots,
    assess, scan,
};
use scull_storage::{
    ApprovalMethod, DefaultPermissions, ItemRecord, OperationEnd, OperationStart, Store, WalkEnd,
    WalkSummary,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MIB: u64 = 1024 * 1024;

/// Serializes the tests of this binary: each one measures volume-wide free space.
static SERIAL: Mutex<()> = Mutex::new(());

fn options() -> MeasureOptions {
    MeasureOptions::default()
        .with_samples(5)
        .with_resampling(Duration::from_millis(500), Duration::from_secs(10))
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
    let est = assess(&summary, &context(&path));
    (
        ActionReclaim::for_action(&est.sizes, &Action::Delete),
        est.notes,
    )
}

/// Where a measurement runs (see the module docs).
#[derive(Clone)]
enum Place {
    /// A private tmpfs at `<root>/vol` that nothing else writes to: strict.
    Private { how: String },
    /// The shared temp volume: the product's contract only.
    Shared { why: String },
}

impl Place {
    /// The place for `test_name`, or `None` when a privileged child already ran the test.
    fn for_test(test_name: &str) -> Option<Place> {
        if !cfg!(target_os = "linux") {
            return Some(Place::Shared {
                why: "no private-volume harness on this OS".into(),
            });
        }
        match privileged::acquire(test_name, Need::Mounts) {
            Privileges::Here(how) => Some(Place::Private { how }),
            Privileges::RanInChild => None,
            Privileges::Unavailable(why) => {
                // CI (Linux) requires the strict measurement: no silent fallback there.
                assert!(
                    std::env::var(privileged::REQUIRE_ENV).as_deref() != Ok("1"),
                    "{} is 1 but `{test_name}` gets no private volume: {why}",
                    privileged::REQUIRE_ENV
                );
                Some(Place::Shared { why })
            }
        }
    }

    fn is_private(&self) -> bool {
        matches!(self, Place::Private { .. })
    }

    fn label(&self) -> String {
        match self {
            Place::Private { how } => format!("private tmpfs ({how})"),
            Place::Shared { why } => format!("shared temp volume (no private volume: {why})"),
        }
    }
}

/// The libtest name of the calling test function.
macro_rules! test_name {
    () => {{
        fn here() {}
        scull_fixtures::net::test_name_from_type_name(std::any::type_name_of_val(&here))
    }};
}

struct Run {
    // Dropped first: unmount before the fixture's temp dir is removed.
    _mounts: Option<Mounts>,
    fx: Fixture,
    /// Where the measured tree lives, relative to the root (`vol/` or empty).
    prefix: String,
    item: PlanItem,
    predicted: ActionReclaim,
    delta: MeasuredDelta,
}

impl Run {
    /// Print the measurement and the noise budget it was taken with.
    fn report(&self, name: &str, place: &Place) {
        let v = &self.delta.per_volume[0];
        println!(
            "{name} on {} [{} {}]: predicted {} B, removed {} B, measured {:?} B; \
             noise budget: sample spread {} B, resolution {} B; explanations {:?}",
            place.label(),
            v.volume.mount_point.display(),
            v.volume.fs_type,
            self.predicted.freed_now,
            self.delta.removed,
            self.delta.measured_total,
            v.noise,
            self.delta.observations.resolution,
            self.delta.explanation_codes()
        );
    }

    /// The product's contract on any volume: measured, and either predicted and measured agree
    /// by the product's own tolerance (within 10%, or within resolution plus the observed
    /// sample noise: `below_resolution`), or the discrepancy carries explanations ending with
    /// `concurrent_activity`. Never a silent mismatch.
    fn assert_recorded_and_explained(&self) {
        assert!(self.delta.measured_total.is_some(), "{}", self.delta);
        let codes = self.delta.explanation_codes();
        if self.delta.exceeds_threshold && self.delta.within_resolution {
            assert!(
                codes.contains(&"below_resolution"),
                "{codes:?}: {}",
                self.delta
            );
        }
        assert!(
            self.delta.agrees() || codes.contains(&"concurrent_activity"),
            "a discrepancy without an explanation: {}",
            self.delta
        );
    }
}

/// Build with `build` (given the prefix to put files under), predict, measure before, delete
/// `rel` (S0) through the Executor, measure after, and check the invariants (only `rel` may
/// change). On a [`Place::Private`] volume the tree is built on the tmpfs and the Executor is
/// anchored there.
fn run(build: &dyn Fn(&mut Fixture, &str), rel: &str, place: &Place) -> Run {
    let mut fx = Fixture::new().unwrap();
    let mut mounts = None;
    let prefix = if place.is_private() {
        fx.dir("vol").unwrap();
        let mut m = Mounts::new(&fx);
        m.tmpfs(&fx.root().join("vol"), 256).unwrap();
        mounts = Some(m);
        "vol/"
    } else {
        ""
    };
    build(&mut fx, prefix);
    fx.checkpoint().unwrap();
    let rel = format!("{prefix}{rel}");
    let (predicted, notes) = predict(&fx, &rel);
    let mut item = s0_delete(&fx, &rel);
    item.reclaimable_bytes = predicted.freed_now;
    let anchor = fx.root().join(prefix);

    let meter = VolumeMeter::new(PlatformFreeSpace, options());
    let baseline = meter.before([anchor.as_path()]);
    assert_eq!(baseline.volume_count(), 1, "{:?}", baseline.unavailable());
    let path = fx.root().join(&rel);
    let report = Executor::new()
        .delete(
            &item,
            &DeleteTarget {
                anchor: &anchor,
                anchor_identity: Some(identity(&anchor)),
                path: &path,
            },
        )
        .unwrap();
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
    check(&fx, &[&item], &[rel.as_str()]);
    assert_canary_files(&fx);
    Run {
        _mounts: mounts,
        fx,
        prefix: prefix.to_owned(),
        item,
        predicted,
        delta,
    }
}

/// A 64 MiB tree (16 × 4 MiB files in nested directories) deleted by the Executor: measured
/// free-space delta ≈ predicted reclaimable (PHY-02), and predicted and measured bytes are
/// both recorded in the audit log (I-5).
#[test]
fn delete_64_mib_measured_matches_predicted() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(place) = Place::for_test(&test_name!()) else {
        return;
    };
    let build = |fx: &mut Fixture, p: &str| {
        for i in 0..16 {
            fx.file_of_size(format!("{p}cand/d{}/sub{}/f{i}.bin", i % 4, i % 2), 4 * MIB)
                .unwrap();
        }
        fx.file(format!("{p}keep/sibling.txt"), b"must stay")
            .unwrap();
    };
    let r = run(&build, "cand", &place);
    r.report("delete_64_mib", &place);
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
    assert!(
        r.fx.root()
            .join(format!("{}keep/sibling.txt", r.prefix))
            .exists()
    );
    r.assert_recorded_and_explained();
    let m = r.delta.measured_total.unwrap();
    if place.is_private() {
        // Nothing else writes to this volume: within 10% of the prediction (PHY-02).
        assert!(!r.delta.exceeds_threshold, "{}", r.delta);
        println!(
            "accuracy: predicted {} B, measured {m} B, discrepancy {:+.2}%",
            r.predicted.freed_now,
            r.delta.discrepancy_pct.unwrap()
        );
        assert!(r.delta.discrepancy_pct.unwrap().abs() <= 10.0);
    }

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
    let Some(place) = Place::for_test(&test_name!()) else {
        return;
    };
    let build = |fx: &mut Fixture, p: &str| {
        for i in 0..4 {
            fx.file_of_size(format!("{p}store/v3/files/{i:02}/pkg.bin"), 4 * MIB)
                .unwrap()
                .hardlink(
                    format!("{p}proj/node_modules/.pnpm/pkg{i}/pkg.bin"),
                    format!("{p}store/v3/files/{i:02}/pkg.bin"),
                )
                .unwrap();
        }
    };
    let r = run(&build, "proj/node_modules", &place);
    r.report("fx_phy_01", &place);
    // ≈ 0: only the directories' own blocks are unique to node_modules.
    assert!(r.predicted.freed_now < MIB, "{:?}", r.predicted);
    assert!(r.delta.removed >= 16 * MIB, "the walker unlinked the links");
    assert!(
        r.delta.explanation_codes().contains(&"shared_hardlinks"),
        "{}",
        r.delta
    );
    r.assert_recorded_and_explained();
    if place.is_private() {
        // Nothing else writes to this volume: |measured| below 10% of the 16 MiB unlinked.
        let m = r.delta.measured_total.unwrap();
        assert!(
            m.unsigned_abs() * 10 < r.delta.removed,
            "measured {m} B for links only: {}",
            r.delta
        );
    }
    // The store keeps its data.
    for i in 0..4 {
        let p =
            r.fx.root()
                .join(format!("{}store/v3/files/{i:02}/pkg.bin", r.prefix));
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
