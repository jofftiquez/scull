//! `execute_plan` wiring (P2-EXEC-07 #73, P2-EXEC-10 #76): the quarantine → Trash → blocked
//! chain (D-3; never a deletion as fallback) and the measured free-space delta (PHY-02, I-5)
//! with its accuracy report. Plans come from a real engine scan of a `scull-fixtures` tree;
//! the quarantine, the Trash, the database and the lock all live beside the fixture root
//! (inside the fixture's own temp dir), so the user's real Trash is never touched. Every test
//! checks the invariants.
//!
//! Fixture IDs (docs/FIXTURES.md): FX-EXEC-03 (quarantine impossible → Trash; Trash impossible
//! too → blocked, never deleted), FX-PHY-07 (Trash reported as moved, not freed), FX-EXEC-01
//! (measured free-space delta reported), I-5 (predicted and measured bytes both recorded).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

#[path = "common/ops.rs"]
mod ops;
#[path = "common/planning.rs"]
mod planning;

use ops::*;
use planning::*;
use scull_cleaner::measure::{
    FreeSpaceSource, MeasureOptions, ProbeError, VolumeMeter, VolumeProbe,
};
use scull_cleaner::trash::TrashLocations;
use scull_cleaner::{ExecutionReport, Executor, ItemOutcome, Operation, Recovery, StepPoint};
use scull_core::{ErrorKind, OperationId, Plan};
use scull_engine::{CancelToken, now_unix};
use scull_fixtures::Fixture;
use scull_storage::{
    ItemOutcome as AuditOutcome, JournalAction, OperationEnd, OperationSummary, Store,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// Whether permission bits bind this process (not root).
fn permissions_enforced(dir: &Path) -> bool {
    let probe = dir.join(".scull-probe");
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            false
        }
        Err(_) => true,
    }
}

/// The invariants of a test skipped before acting (nothing may have changed).
fn skipped(fx: &Fixture) {
    let inv = fx.check_invariants(&scull_fixtures::InvariantInputs::new());
    println!("{inv}");
    inv.assert_ok();
}

fn chmod(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// A home Trash inside the fixture's temp dir (beside the root, same volume).
fn trash_in(fx: &Fixture) -> TrashLocations {
    let xdg = fx.base().join("xdg");
    std::fs::create_dir_all(&xdg).unwrap();
    TrashLocations {
        home: Some(xdg.join("Trash")),
        topdir: Some(fx.base().to_path_buf()),
    }
}

/// A quarantine base nobody can write into: quarantine is impossible (no writable location).
fn unwritable_quarantine(fx: &Fixture) -> Option<scull_cleaner::QuarantineSettings> {
    let base = fx.base().join("qro");
    std::fs::create_dir_all(&base).unwrap();
    chmod(&base, 0o555);
    if !permissions_enforced(&base) {
        chmod(&base, 0o755);
        return None;
    }
    Some(scull_cleaner::QuarantineSettings {
        base: Some(base),
        ..scull_cleaner::QuarantineSettings::default()
    })
}

/// Plans `rels`, begins an operation set up by `setup`, executes and finishes it.
fn run_with(
    fx: &Fixture,
    s: &mut Store,
    rels: &[&str],
    op: &str,
    exec: &Executor,
    setup: impl FnOnce(Operation<'_>) -> Operation<'_>,
) -> (Plan, ExecutionReport, OperationSummary) {
    let h = harness(false);
    let plan = plan_named(fx, &h.scan(fx), rels, op);
    s.insert_plan(&plan).unwrap();
    // The fixture's quarantine base unless `setup` sets another (P2-EXEC-13).
    let mut operation = setup(
        exec.begin_operation(s, &plan, &start(&plan, op))
            .unwrap()
            .with_quarantine(settings(fx)),
    );
    let r = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    let summary = operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    print_report(fx, &r);
    (plan, r, summary)
}

fn outcome_of<'a>(fx: &Fixture, r: &'a ExecutionReport, rel_path: &str) -> &'a ItemOutcome {
    &r.items
        .iter()
        .find(|i| rel(fx, &i.path.to_path_buf()) == rel_path)
        .unwrap()
        .outcome
}

fn trash_files(fx: &Fixture) -> Vec<PathBuf> {
    std::fs::read_dir(fx.base().join("xdg/Trash/files"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// FX-EXEC-03 (Trash branch), FX-PHY-07: quarantine is impossible (no writable quarantine
/// location), so the S1 and S2 items fall back to the Trash: moved there with their identity,
/// audited `moved_to_trash` with the moved bytes and nothing freed, journaled as a Trash
/// step. The S0 item of the same plan is deleted as usual.
#[test]
fn fx_exec_03_quarantine_impossible_falls_back_to_trash() {
    let fx = fixture();
    let Some(q) = unwritable_quarantine(&fx) else {
        println!("skipped: running with privileges that bypass permissions");
        skipped(&fx);
        return;
    };
    let mut s = store(&fx);
    let exec = locked(&fx);
    let trash = trash_in(&fx);
    let (plan, r, summary) = run_with(
        &fx,
        &mut s,
        &["app/target", "lib/target", "web/node_modules"],
        "T1",
        &exec,
        |op| op.with_quarantine(q).with_trash(trash),
    );
    chmod(&fx.base().join("qro"), 0o755);
    for rel_path in ["app/target", "web/node_modules"] {
        match outcome_of(&fx, &r, rel_path) {
            ItemOutcome::Trashed(t) => {
                let loc = t.outcome.location.clone().unwrap();
                assert!(
                    loc.starts_with(fx.base().join("xdg/Trash/files")),
                    "{loc:?}"
                );
                assert_eq!(t.freed_bytes(), 0, "Trash moves bytes, it frees none");
                assert!(t.moved_bytes() > 0);
            }
            other => panic!("{rel_path}: expected the Trash fallback, got {other:?}"),
        }
        assert!(!fx.root().join(rel_path).exists());
    }
    assert_executed(&fx, &r, "lib/target");
    assert_eq!(trash_files(&fx).len(), 2);
    assert!(s.quarantine_held().unwrap().is_empty());
    let op = OperationId::from_suffix("T1").unwrap();
    let audit = s.audit_items(&op).unwrap();
    let trashed: Vec<_> = audit
        .iter()
        .filter(|a| a.outcome == AuditOutcome::MovedToTrash)
        .collect();
    assert_eq!(trashed.len(), 2, "{audit:#?}");
    for a in &trashed {
        assert_eq!(a.reason.as_deref(), Some("quarantine_impossible"));
        assert!(a.moved_bytes.is_some_and(|b| b > 0));
        assert_eq!(a.freed_bytes, None);
    }
    assert_eq!(summary.totals.items_done, 3);
    assert!(summary.totals.moved_bytes > 0);
    // Journaled as Trash steps, each closed.
    let steps = s.journal_steps(&op).unwrap();
    let trash_steps = steps
        .iter()
        .filter(|st| st.intent.action == JournalAction::Trash)
        .count();
    assert_eq!(trash_steps, 2, "{steps:#?}");
    assert!(steps.iter().all(|st| st.close.is_some()));
    assert_eq!(r.error_kind(), None);
    check(
        &fx,
        &plan,
        &r,
        &["app/target", "lib/target", "web/node_modules"],
    );
}

/// FX-EXEC-03 (cross-volume): the configured quarantine location is on another filesystem, so
/// it is refused for this item before anything is journaled or created there (P2-EXEC-13: a
/// configured base must be on the item's volume), and the Trash (same volume) takes the item.
/// A move that still hits another volume (e.g. a bind mount on the same device) fails with
/// EXDEV and falls back the same way. (Linux, when `/dev/shm` is a separate filesystem.)
#[cfg(target_os = "linux")]
#[test]
fn fx_exec_03_cross_volume_quarantine_falls_back_to_trash() {
    use std::os::unix::fs::MetadataExt;
    let fx = fixture();
    let shm = Path::new("/dev/shm");
    let dev = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
    if dev(shm).is_none() || dev(shm) == dev(fx.root()) {
        println!("skipped: no second filesystem at /dev/shm");
        let inv = fx.check_invariants(&scull_fixtures::InvariantInputs::new());
        inv.assert_ok();
        return;
    }
    // Created by this test, removed at the end; nothing is ever moved into it.
    let other = shm.join(format!(
        "scull-wiring-test-{}-{}",
        std::process::id(),
        now_unix()
    ));
    std::fs::create_dir(&other).unwrap();
    chmod(&other, 0o700);
    let mut s = store(&fx);
    let exec = locked(&fx);
    let trash = trash_in(&fx);
    let q = scull_cleaner::QuarantineSettings {
        base: Some(other.clone()),
        ..scull_cleaner::QuarantineSettings::default()
    };
    let (plan, r, _) = run_with(&fx, &mut s, &["app/target"], "T2", &exec, |op| {
        op.with_quarantine(q).with_trash(trash)
    });
    let leftover: Vec<_> = walk(&other);
    std::fs::remove_dir_all(&other).unwrap();
    assert!(
        matches!(outcome_of(&fx, &r, "app/target"), ItemOutcome::Trashed(_)),
        "{r:#?}"
    );
    assert!(
        leftover.is_empty(),
        "nothing was moved or even created on the other volume: {leftover:?}"
    );
    let op = OperationId::from_suffix("T2").unwrap();
    let steps = s.journal_steps(&op).unwrap();
    let actions: Vec<_> = steps.iter().map(|st| st.intent.action).collect();
    assert_eq!(actions, vec![JournalAction::Trash], "{steps:#?}");
    assert_eq!(
        steps[0]
            .close
            .as_ref()
            .unwrap()
            .outcome
            .as_ref()
            .unwrap()
            .outcome,
        AuditOutcome::MovedToTrash
    );
    check(&fx, &plan, &r, &["app/target"]);
}

#[cfg(target_os = "linux")]
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                out.push(e.path());
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                }
            }
        }
    }
    out
}

/// FX-EXEC-03 (end of the chain): the item's parent is read-only, so quarantine is
/// impossible and so is the Trash (both renames need write access to the parent): the item is
/// blocked and left in place, never deleted.
#[test]
fn fx_exec_03_quarantine_and_trash_impossible_blocked_never_deleted() {
    let mut fx = fixture();
    let web = fx.root().join("web");
    chmod(&web, 0o555);
    if !permissions_enforced(&web) {
        chmod(&web, 0o755);
        println!("skipped: running with privileges that bypass permissions");
        skipped(&fx);
        return;
    }
    fx.checkpoint().unwrap();
    let mut s = store(&fx);
    let exec = locked(&fx);
    let trash = trash_in(&fx);
    let settings = settings(&fx);
    let (plan, r, _) = run_with(&fx, &mut s, &["web/node_modules"], "T3", &exec, |op| {
        op.with_quarantine(settings).with_trash(trash)
    });
    chmod(&web, 0o755);
    match outcome_of(&fx, &r, "web/node_modules") {
        ItemOutcome::Blocked { code, message } => {
            assert_eq!(*code, "quarantine_and_trash_impossible");
            assert!(message.contains("never deleted"), "{message}");
        }
        other => panic!("expected blocked, got {other:?}"),
    }
    assert!(fx.root().join("web/node_modules/react/big.js").is_file());
    assert!(trash_files(&fx).is_empty());
    let audit = s
        .audit_items(&OperationId::from_suffix("T3").unwrap())
        .unwrap();
    assert_eq!(audit[0].outcome, AuditOutcome::Refused);
    assert_eq!(
        audit[0].reason.as_deref(),
        Some("quarantine_and_trash_impossible")
    );
    assert_eq!(r.error_kind(), Some(ErrorKind::PartialFailure));
    fx.checkpoint().unwrap();
    check(&fx, &plan, &r, &[]);
}

/// D-3: a quarantine refused because the item **changed** (here: replaced right before the
/// move) is not "impossible", so it is not retried in the Trash: refused, the new object left
/// alone, the Trash untouched.
#[test]
fn quarantine_refused_for_a_changed_item_is_not_retried_in_the_trash() {
    let fx = fixture();
    let mut s = store(&fx);
    let target = fx.root().join("app/target");
    let moved = fx.root().join("app/target.moved");
    let (t, m) = (target.clone(), moved.clone());
    let exec = locked(&fx).with_step_hook(Arc::new(move |p: &StepPoint<'_>| {
        if matches!(p, StepPoint::BeforeIntent(_)) && !m.exists() {
            std::fs::rename(&t, &m).unwrap();
            std::fs::create_dir(&t).unwrap();
        }
    }));
    let trash = trash_in(&fx);
    let settings = settings(&fx);
    let (plan, r, _) = run_with(&fx, &mut s, &["app/target"], "T4", &exec, |op| {
        op.with_quarantine(settings).with_trash(trash)
    });
    match outcome_of(&fx, &r, "app/target") {
        ItemOutcome::Refused(e) => assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e}"),
        other => panic!("expected refused, got {other:?}"),
    }
    assert!(trash_files(&fx).is_empty(), "never retried in the Trash");
    assert!(moved.join("debug").exists(), "the original tree is intact");
    check(&fx, &plan, &r, &["app/target", "app/target.moved"]);
}

/// Without a configured Trash (the default), an impossible quarantine is blocked as before.
#[test]
fn without_a_trash_an_impossible_quarantine_is_blocked() {
    let fx = fixture();
    let Some(q) = unwritable_quarantine(&fx) else {
        println!("skipped: running with privileges that bypass permissions");
        skipped(&fx);
        return;
    };
    let mut s = store(&fx);
    let exec = locked(&fx);
    let (plan, r, _) = run_with(&fx, &mut s, &["app/target"], "T5", &exec, |op| {
        op.with_quarantine(q)
    });
    chmod(&fx.base().join("qro"), 0o755);
    match outcome_of(&fx, &r, "app/target") {
        ItemOutcome::Blocked { code, message } => {
            assert_eq!(*code, "quarantine_impossible");
            assert!(message.contains("no Trash fallback"), "{message}");
        }
        other => panic!("expected blocked, got {other:?}"),
    }
    assert!(fx.root().join("app/target/debug").exists());
    check(&fx, &plan, &r, &[]);
}

// ---- measured delta (#76) --------------------------------------------------------------------

/// A deterministic volume: `before` free bytes on the first sample, `after` on every later one.
#[derive(Debug)]
struct FakeVolume {
    before: u64,
    after: u64,
    calls: AtomicU64,
}

impl FreeSpaceSource for FakeVolume {
    fn describe(&self, _path: &Path) -> Result<VolumeProbe, ProbeError> {
        Ok(VolumeProbe::new("/fake", "ext4").with_id(7))
    }

    fn free_bytes(&self, _path: &Path) -> Result<u64, ProbeError> {
        Ok(if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.before
        } else {
            self.after
        })
    }

    fn sleep(&self, _d: std::time::Duration) {}
}

/// FX-EXEC-01, I-5: the free-space delta around `execute_plan` is measured; the accuracy
/// report (predicted, removed, moved, measured, discrepancy, explanations) is in the
/// execution report, and `Operation::finish` records the measured delta next to the predicted
/// bytes.
#[test]
fn fx_exec_01_measured_delta_and_accuracy_report_recorded() {
    let fx = fixture();
    let mut s = store(&fx);
    let exec = locked(&fx);
    let meter = VolumeMeter::new(
        FakeVolume {
            before: 10 * GIB,
            after: 10 * GIB + MIB,
            calls: AtomicU64::new(0),
        },
        MeasureOptions::immediate(4096),
    );
    let (plan, r, summary) = run_with(
        &fx,
        &mut s,
        &["app/target", "lib/target"],
        "M1",
        &exec,
        |op| op.with_quarantine(settings(&fx)).with_meter(meter),
    );
    let m = r.measurement.as_ref().expect("accuracy report");
    println!("{m}");
    assert_eq!(m.measured_total, Some(MIB as i64));
    assert_eq!(m.measured_delta_bytes(), Some(MIB as i64));
    let removed: u64 = r
        .items
        .iter()
        .map(|i| match &i.outcome {
            ItemOutcome::Deleted(d) => d.removed.allocated_bytes,
            _ => 0,
        })
        .sum();
    assert_eq!(m.removed, removed);
    let moved = match outcome_of(&fx, &r, "app/target") {
        ItemOutcome::Quarantined(q) => q.bytes,
        other => panic!("{other:?}"),
    };
    assert_eq!(m.moved, moved);
    assert!(m.predicted.freed_now > 0 && m.predicted.moved > 0, "{m:?}");
    // I-5: both predicted and measured bytes are recorded in the operation.
    assert_eq!(summary.totals.measured_delta_bytes, Some(MIB as i64));
    assert!(summary.totals.predicted_bytes > 0);
    let rec = s
        .operation(&OperationId::from_suffix("M1").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        rec.finished.unwrap().totals.measured_delta_bytes,
        Some(MIB as i64)
    );
    check(&fx, &plan, &r, &["app/target", "lib/target"]);
}

/// I-5 by default: an operation measures its volumes with the platform meter unless told not
/// to; `finish_measured` records an explicit figure; `without_measurement` records none.
#[test]
fn every_operation_measures_by_default_and_finish_measured_overrides() {
    let fx = fixture();
    let mut s = store(&fx);
    let exec = locked(&fx);
    let (_, r, summary) = run_with(&fx, &mut s, &["lib/target"], "M2", &exec, |op| op);
    assert!(r.measurement.is_some());
    assert!(summary.totals.measured_delta_bytes.is_some(), "{summary:?}");

    // Explicit figure.
    let h = harness(false);
    let plan = plan_named(&fx, &h.scan(&fx), &["app/target"], "M3");
    s.insert_plan(&plan).unwrap();
    let mut operation = exec
        .begin_operation(&mut s, &plan, &start(&plan, "M3"))
        .unwrap()
        .with_quarantine(settings(&fx))
        .without_measurement();
    let r3 = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(&fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    assert!(r3.measurement.is_none());
    let summary = operation
        .finish_measured(OperationEnd::Normal, now_unix(), Some(42))
        .unwrap();
    assert_eq!(summary.totals.measured_delta_bytes, Some(42));
    let inv = fx.check_invariants(
        &scull_fixtures::InvariantInputs::new()
            .allow("lib/target")
            .allow("app/target"),
    );
    println!("{inv}");
    inv.assert_ok();
}

// ---- journal + Trash (#74) -------------------------------------------------------------------

/// SAFE-19 for the Trash step: a crash right after the move to the Trash (before the journal
/// close) is reconciled as `moved_to_trash`; a crash right after its intent as `skipped`.
/// (The crash is a panic from the step hook; the kill tests in journal.rs SIGKILL a child.)
#[test]
fn crash_during_trash_fallback_is_reconciled() {
    for (point, expect) in [
        ("after_move", AuditOutcome::MovedToTrash),
        ("after_intent", AuditOutcome::Skipped),
    ] {
        let fx = fixture();
        let Some(q) = unwritable_quarantine(&fx) else {
            println!("skipped: running with privileges that bypass permissions");
            skipped(&fx);
            return;
        };
        let mut s = store(&fx);
        let h = harness(false);
        let plan = plan_named(&fx, &h.scan(&fx), &["app/target"], "X1");
        s.insert_plan(&plan).unwrap();
        let trash = trash_in(&fx);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let exec = locked(&fx).with_step_hook(Arc::new(move |p: &StepPoint<'_>| {
                if p.name() == point {
                    panic!("simulated crash at {point}");
                }
            }));
            let mut operation = exec
                .begin_operation(&mut s, &plan, &start(&plan, "X1"))
                .unwrap()
                .with_quarantine(q)
                .with_trash(trash);
            let _ = exec.execute_plan(
                &plan,
                &ctx(),
                &h.revalidation(&fx),
                Some(&mut operation),
                &CancelToken::new(),
            );
        }));
        assert!(crashed.is_err(), "the hook crashed the run");
        chmod(&fx.base().join("qro"), 0o755);
        let report = locked(&fx).recover(&mut s, now_unix()).unwrap();
        let item = &report.operations[0].items[0];
        println!(
            "crash at {point}: recovered as {:?} ({:?})",
            item.outcome, item.how
        );
        assert_eq!(item.how, Recovery::Reconciled(JournalAction::Trash));
        assert_eq!(item.outcome, expect);
        assert_eq!(
            fx.root().join("app/target").exists(),
            expect == AuditOutcome::Skipped
        );
        let inv = fx.check_invariants(&scull_fixtures::InvariantInputs::new().allow("app/target"));
        println!("{inv}");
        inv.assert_ok();
    }
}
