//! Write-ahead journal and crash recovery (P2-EXEC-08, SAFE-19, T-15). Plans come from a real
//! engine scan of a `scull-fixtures` tree; the quarantine, database and lock live beside the
//! fixture root (inside the fixture's own temp dir). Every test checks the invariants: I-1,
//! I-2 (canary), I-3 (only plan items changed) and I-8 through `check_invariants`, and I-4
//! (the audit matches the disk) through `Store::disk_expectations` against before/after
//! snapshots (the fixtures lane's I-4 hook is still a stub).
//!
//! Kill tests: the operation runs in a **child process** (this test binary re-executed with
//! `SCULL_KILL_CHILD` set). The child installs a step hook that, at the crash point named by
//! the environment, writes a marker and blocks; the parent then SIGKILLs it
//! (`Child::kill`), runs `Executor::recover` and asserts the invariants. Crash points are a
//! deterministic list (every point for every item kind) plus a seeded pseudo-random sample
//! (`SCULL_KILL_SEED`, printed, to replay a failure).
//!
//! Fixture IDs (docs/FIXTURES.md): FX-RACE-06 (cancel mid-execution: consistent, audit shows
//! partial, a rerun completes the rest), FX-RACE-07 (killed during a quarantine move: the
//! journal reconciles on next start, no orphaned or double-counted items).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

#[path = "common/ops.rs"]
mod ops;
#[path = "common/planning.rs"]
mod planning;

use ops::*;
use planning::*;
use scull_cleaner::{
    Executor, ItemOutcome, LockOptions, QUARANTINE_DIR_NAME, QuarantineSettings, Recovery,
    Revalidation, StepPoint,
};
use scull_core::{ErrorKind, ItemId, OperationId, Plan, PlanId};
use scull_engine::{CancelToken, now_unix};
use scull_fixtures::{Fixture, TreeSnapshot};
use scull_platform::{Native, Platform};
use scull_storage::{
    ItemOutcome as AuditOutcome, JournalAction, JournalCloseKind, ObservedChange, OperationEnd,
    OperationStatus, Store,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Plan order: an S1 quarantine, an S0 deletion, an S2 quarantine.
const ITEMS: [&str; 3] = ["app/target", "lib/target", "web/node_modules"];

fn lock_path(base: &Path) -> PathBuf {
    base.join("state").join("execution.lock")
}

// ---- I-4 -------------------------------------------------------------------------------------

/// What happened at `path` between two snapshots of the fixture root.
fn observed(
    fx: &Fixture,
    before: &TreeSnapshot,
    after: &TreeSnapshot,
    path: &Path,
) -> ObservedChange {
    let rel = path
        .strip_prefix(scull_engine::paths::canon(fx.root()))
        .unwrap()
        .to_path_buf();
    let under = |s: &TreeSnapshot| -> Vec<(PathBuf, scull_fixtures::Entry)> {
        s.entries()
            .iter()
            .filter(|(p, _)| p.starts_with(&rel))
            .map(|(p, e)| (p.clone(), e.clone()))
            .collect()
    };
    let (b, a) = (under(before), under(after));
    if b.is_empty() {
        ObservedChange::Absent
    } else if after.get(&rel).is_none() {
        ObservedChange::Gone
    } else if a == b {
        ObservedChange::Unchanged
    } else {
        ObservedChange::Changed
    }
}

/// I-4: every plan item has exactly one audit outcome and it matches the disk.
fn assert_audit_matches_disk(
    fx: &Fixture,
    s: &Store,
    op: &OperationId,
    before: &TreeSnapshot,
) -> Vec<String> {
    let after = TreeSnapshot::capture(fx.root()).unwrap();
    let exp = s.disk_expectations(op).unwrap();
    let mismatches = exp.mismatches(|p| observed(fx, before, &after, &p.to_path_buf()));
    assert!(mismatches.is_empty(), "I-4 failed: {mismatches:#?}");
    println!("I-4 audit matches disk: ok ({} items)", exp.items.len());
    mismatches
}

/// No orphaned or double-counted quarantine entries: every entry in the operation folder has
/// exactly one held manifest entry with its identity, and every `quarantined` audit outcome
/// has exactly one manifest entry.
fn assert_quarantine_consistent(fx: &Fixture, s: &Store, op: &OperationId) {
    let entries = s.quarantine_entries(Some(op)).unwrap();
    let dir = fx.base().join(QUARANTINE_DIR_NAME).join(op.as_str());
    let on_disk: BTreeSet<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().is_some_and(|n| n != "manifest.json"))
                .collect()
        })
        .unwrap_or_default();
    let recorded: BTreeSet<PathBuf> = entries
        .iter()
        .map(|e| e.entry.quarantine_path.to_path_buf())
        .collect();
    assert_eq!(on_disk, recorded, "orphaned or missing quarantine entries");
    for e in &entries {
        assert_eq!(
            Native
                .metadata(&e.entry.quarantine_path.to_path_buf())
                .unwrap()
                .identity,
            Some(e.entry.entry_identity)
        );
    }
    let audited: Vec<ItemId> = s
        .audit_items(op)
        .unwrap()
        .into_iter()
        .filter(|a| a.outcome == AuditOutcome::Quarantined)
        .map(|a| a.item_id)
        .collect();
    let mut manifest_items: Vec<ItemId> = entries.iter().map(|e| e.entry.item_id.clone()).collect();
    let mut audited_sorted = audited.clone();
    manifest_items.sort();
    audited_sorted.sort();
    assert_eq!(
        manifest_items, audited_sorted,
        "double-counted or unrecorded moves"
    );
    if !entries.is_empty() {
        assert!(
            dir.join("manifest.json").exists(),
            "the manifest copy is rewritten"
        );
    }
}

// ---- the child process -----------------------------------------------------------------------

/// What the child is told through the environment.
struct ChildSpec {
    root: PathBuf,
    base: PathBuf,
    plan_id: String,
    op: String,
    /// Item path relative to the root, point name, and (for `walk`) the event number.
    item: String,
    point: String,
    n: u64,
}

impl ChildSpec {
    fn from_env() -> Option<Self> {
        let v = |k: &str| std::env::var(k).ok();
        Some(Self {
            root: v("SCULL_KILL_ROOT")?.into(),
            base: v("SCULL_KILL_BASE")?.into(),
            plan_id: v("SCULL_KILL_PLAN")?,
            op: v("SCULL_KILL_OP")?,
            item: v("SCULL_KILL_ITEM")?,
            point: v("SCULL_KILL_POINT")?,
            n: v("SCULL_KILL_N")?.parse().ok()?,
        })
    }
}

/// The child: runs the operation and blocks at the crash point. Not a test on its own (it
/// returns at once unless the parent set `SCULL_KILL_CHILD`).
#[test]
fn kill_child() {
    if std::env::var_os("SCULL_KILL_CHILD").is_none() {
        return;
    }
    let spec = ChildSpec::from_env().expect("child spec");
    let h = harness(false);
    let mut s = Store::open(
        &spec.base.join("data").join("scull.db"),
        &scull_engine::PlatformOwnerOnly,
    )
    .unwrap();
    let plan = s
        .get_plan(&PlanId::try_from(spec.plan_id.clone()).unwrap())
        .unwrap()
        .unwrap();
    let target = plan
        .items()
        .iter()
        .find(|i| i.path.to_path_buf() == spec.root.join(&spec.item))
        .expect("crash item in plan")
        .item_id
        .clone();
    let marker = spec.base.join("kill-reached");
    let (point, n) = (spec.point.clone(), spec.n);
    let hook = Arc::new(move |p: &StepPoint<'_>| {
        let hit = *p.item() == target
            && p.name() == point
            && match p {
                StepPoint::Walk(_, k) => *k == n,
                _ => true,
            };
        if hit {
            std::fs::write(&marker, p.name()).unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
    let exec = Executor::acquire(&LockOptions::at(lock_path(&spec.base)))
        .unwrap()
        .with_step_hook(hook);
    let revalidation =
        Revalidation::new(&h.engine, std::slice::from_ref(&spec.root)).with_request(request());
    let mut operation = exec
        .begin_operation(&mut s, &plan, &start(&plan, &spec.op))
        .unwrap()
        .with_quarantine(QuarantineSettings {
            base: Some(spec.base.clone()),
            ..QuarantineSettings::default()
        });
    exec.execute_plan(
        &plan,
        &ctx(),
        &revalidation,
        Some(&mut operation),
        &CancelToken::new(),
    )
    .unwrap();
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    std::fs::write(spec.base.join("kill-finished"), "finished").unwrap();
}

/// Runs the operation of `plan` in a child process and SIGKILLs it at `point`. Returns
/// whether the point was reached (otherwise the child finished normally).
fn run_and_kill(fx: &Fixture, plan: &Plan, op: &str, item: &str, point: &str, n: u64) -> bool {
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args(["kill_child", "--exact", "--nocapture", "--test-threads=1"])
        .env("SCULL_KILL_CHILD", "1")
        .env("SCULL_KILL_ROOT", scull_engine::paths::canon(fx.root()))
        .env("SCULL_KILL_BASE", fx.base())
        .env("SCULL_KILL_PLAN", plan.id().as_str())
        .env("SCULL_KILL_OP", op)
        .env("SCULL_KILL_ITEM", item)
        .env("SCULL_KILL_POINT", point)
        .env("SCULL_KILL_N", n.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    let reached = fx.base().join("kill-reached");
    let finished = fx.base().join("kill-finished");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if reached.exists() {
            child.kill().unwrap(); // SIGKILL: no destructors, no flushing.
            child.wait().unwrap();
            std::fs::remove_file(&reached).unwrap();
            return true;
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "the child failed before the crash point");
            assert!(finished.exists());
            std::fs::remove_file(&finished).unwrap();
            return false;
        }
        assert!(Instant::now() < deadline, "the child hung");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One kill test: fresh fixture, plan, kill at the point, recover, check everything. Returns
/// the audit outcome the crash item ended with.
fn kill_and_recover(item: &str, point: &str, n: u64) -> AuditOutcome {
    println!(
        "=== crash at {point}{} of {item} ===",
        if point == "walk" {
            format!(" #{n}")
        } else {
            String::new()
        }
    );
    let fx = fixture();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan_named(&fx, &scan, &ITEMS, "KILL");
    let op_name = "K1";
    let op = OperationId::from_suffix(op_name).unwrap();
    {
        let mut s = store(&fx);
        s.insert_plan(&plan).unwrap();
    }
    let before = TreeSnapshot::capture(fx.root()).unwrap();
    let reached = run_and_kill(&fx, &plan, op_name, item, point, n);
    println!("crash point reached: {reached}");

    let mut s = store(&fx);
    let exec = locked(&fx);
    // An unfinished operation blocks new ones until it is recovered.
    if reached {
        let err = exec
            .begin_operation(&mut s, &plan, &start(&plan, "K2"))
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::SafetyBlocked, "{err}");
    }
    let report = exec.recover(&mut s, now_unix()).unwrap();
    if reached {
        assert_eq!(report.operations.len(), 1, "{report:#?}");
        let rec = &report.operations[0];
        assert_eq!(rec.summary.status, OperationStatus::Interrupted);
        assert_eq!(rec.summary.totals.items_missing(), 0, "{rec:#?}");
        for i in &rec.items {
            println!(
                "  recovered {} -> {} {:?} ({:?})",
                i.item_id,
                i.outcome.as_str(),
                i.reason,
                i.how
            );
        }
        // Recovery never resumes: an item whose step was cut short is never `removed` unless
        // the disk shows it gone.
        if point == "before_intent" || point == "after_intent" {
            let it = rec
                .items
                .iter()
                .find(|i| {
                    plan.items().iter().any(|p| {
                        p.item_id == i.item_id
                            && p.path.to_path_buf()
                                == scull_engine::paths::canon(fx.root()).join(item)
                    })
                })
                .unwrap();
            assert_eq!(it.outcome, AuditOutcome::Skipped, "{it:?}");
        }
    } else {
        assert!(report.is_empty());
    }
    // The journal has no open step; the chain is intact.
    let steps = s.journal_steps(&op).unwrap();
    assert!(steps.iter().all(|st| st.close.is_some()), "{steps:#?}");
    assert!(s.verify_chain(&op).unwrap().is_intact());
    assert_audit_matches_disk(&fx, &s, &op, &before);
    assert_quarantine_consistent(&fx, &s, &op);
    // Recovery is idempotent, and new operations may start again.
    assert!(exec.recover(&mut s, now_unix()).unwrap().is_empty());
    let changed: Vec<&str> = ITEMS.to_vec();
    let inv = fx.check_invariants(
        &scull_fixtures::InvariantInputs::new()
            .plan(
                plan.items()
                    .iter()
                    .map(|i| (i.item_id.clone(), i.tier, i.action.clone())),
            )
            .allow(changed[0])
            .allow(changed[1])
            .allow(changed[2]),
    );
    println!("{inv}");
    inv.assert_ok();
    let crash_path = scull_engine::paths::canon(fx.root()).join(item);
    s.audit_items(&op)
        .unwrap()
        .into_iter()
        .find(|a| a.path.to_path_buf() == crash_path)
        .expect("the crash item has an outcome")
        .outcome
}

/// How many walker events deleting `lib/target` of a fresh fixture produces (in process).
fn walk_events() -> u64 {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let plan = plan_named(&fx, &h.scan(&fx), &["lib/target"], "W1");
    s.insert_plan(&plan).unwrap();
    let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let c = count.clone();
    let exec = locked(&fx).with_step_hook(Arc::new(move |p: &StepPoint<'_>| {
        if let StepPoint::Walk(_, n) = p {
            c.store(*n, std::sync::atomic::Ordering::SeqCst);
        }
    }));
    let mut operation = exec
        .begin_operation(&mut s, &plan, &start(&plan, "W1"))
        .unwrap();
    let r = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(&fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    check(&fx, &plan, &r, &["lib/target"]);
    count.load(std::sync::atomic::Ordering::SeqCst)
}

/// A small deterministic PRNG (xorshift64*), seeded from `SCULL_KILL_SEED`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

const QUARANTINE_POINTS: [&str; 6] = [
    "before_intent",
    "after_intent",
    "after_move",
    "after_manifest",
    "after_close",
    "after_audit",
];
const DELETE_POINTS: [&str; 4] = [
    "before_intent",
    "after_intent",
    "after_close",
    "after_audit",
];

/// FX-RACE-07: the process is SIGKILLed during a quarantine move (and at every other step of
/// the S1/S2 quarantine items): on the next start the journal reconciles; no orphaned or
/// double-counted items; the audit matches the disk.
#[test]
fn fx_race_07_killed_during_quarantine_move_journal_reconciles() {
    for point in QUARANTINE_POINTS {
        let o = kill_and_recover("app/target", point, 0);
        let expect = match point {
            "before_intent" | "after_intent" => AuditOutcome::Skipped,
            _ => AuditOutcome::Quarantined,
        };
        assert_eq!(o, expect, "{point}");
    }
    assert_eq!(
        kill_and_recover("web/node_modules", "after_move", 0),
        AuditOutcome::Quarantined
    );
}

/// SAFE-19: the process is SIGKILLed at every step of an S0 deletion, including in the middle
/// of the removal walk: recovery records removed / partial / skipped from the disk, never
/// resumes the deletion, and the audit matches the disk.
#[test]
fn killed_during_deletion_recovery_never_resumes_and_audit_matches_disk() {
    for point in DELETE_POINTS {
        let o = kill_and_recover("lib/target", point, 0);
        let expect = match point {
            "before_intent" | "after_intent" => AuditOutcome::Skipped,
            _ => AuditOutcome::Removed,
        };
        assert_eq!(o, expect, "{point}");
    }
    // Before anything is removed, mid-walk, and at the last event (the target itself checked).
    let total = walk_events();
    println!("the walk of lib/target has {total} events");
    assert!(total >= 6, "{total}");
    let outcomes: Vec<AuditOutcome> = [1, total / 2, total - 1, total]
        .into_iter()
        .map(|n| kill_and_recover("lib/target", "walk", n))
        .collect();
    println!("mid-walk outcomes: {outcomes:?}");
    assert_eq!(
        outcomes[0],
        AuditOutcome::Skipped,
        "nothing removed at the first event"
    );
    assert!(
        outcomes.contains(&AuditOutcome::Partial),
        "a walk cut short after removing entries is recorded as partial: {outcomes:?}"
    );
}

/// SAFE-19 random crash points: a seeded sample over every item and point (replay with
/// `SCULL_KILL_SEED=<seed>`).
#[test]
fn killed_at_random_points_recovery_keeps_invariants() {
    let seed = std::env::var("SCULL_KILL_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5c01_1f00_d5eeu64);
    println!("SCULL_KILL_SEED={seed}");
    let mut rng = Rng(seed | 1);
    for _ in 0..4 {
        let item = ITEMS[(rng.next() % 3) as usize];
        let (point, n) = if item == "lib/target" {
            let all = DELETE_POINTS.len() as u64 + 1;
            match rng.next() % all {
                k if k < DELETE_POINTS.len() as u64 => (DELETE_POINTS[k as usize], 0),
                _ => ("walk", 1 + rng.next() % 8),
            }
        } else {
            (QUARANTINE_POINTS[(rng.next() % 6) as usize], 0)
        };
        let _ = kill_and_recover(item, point, n);
    }
}

// ---- in-process ------------------------------------------------------------------------------

/// SAFE-19: a normal run journals each executed step (intent with the source identity, the
/// exact quarantine destination, the tree fingerprint for a deletion) and closes it with the
/// audited outcome; nothing is left open and recovery finds nothing.
#[test]
fn normal_run_journals_and_closes_every_step() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let (plan, r) = run(&fx, &h, &mut s, &ITEMS, "J1");
    let op = OperationId::from_suffix("J1").unwrap();
    let steps = s.journal_steps(&op).unwrap();
    assert_eq!(steps.len(), 3, "{steps:#?}");
    for st in &steps {
        let close = st.close.as_ref().expect("closed");
        assert_eq!(close.kind, JournalCloseKind::Done);
        let outcome = close.outcome.as_ref().unwrap();
        let item = plan
            .items()
            .iter()
            .find(|i| i.item_id == st.intent.item_id)
            .unwrap();
        assert_eq!(st.intent.source_identity, item.identity);
        match st.intent.action {
            JournalAction::Quarantine => {
                assert_eq!(outcome.outcome, AuditOutcome::Quarantined);
                assert!(st.intent.destination.is_some());
                assert!(st.intent.source_parent.is_some());
            }
            JournalAction::Delete => {
                assert_eq!(outcome.outcome, AuditOutcome::Removed);
                assert!(st.intent.tree.is_some_and(|t| t.entries > 1));
            }
            other => panic!("unexpected step {other:?}"),
        }
        println!(
            "  journal #{} {} {} -> {}",
            st.seq,
            st.intent.action.as_str(),
            rel(&fx, &st.intent.source_path.to_path_buf()),
            outcome.outcome.as_str()
        );
    }
    let exec = locked(&fx);
    assert!(exec.recover(&mut s, now_unix()).unwrap().is_empty());
    check(&fx, &plan, &r, &ITEMS);
}

/// FX-RACE-06: the user cancels mid-execution (after the first item). The state is
/// consistent (the first item done, the others untouched and not run), the audit shows the
/// partial run (cancelled, items without an outcome), nothing is left open in the journal, and
/// a rerun (a new plan) completes the remaining items.
#[test]
fn fx_race_06_cancel_mid_execution_consistent_and_rerun_completes() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let plan = plan_named(&fx, &scan, &ITEMS, "C1");
    s.insert_plan(&plan).unwrap();
    let cancel = CancelToken::new();
    let first = plan.items()[0].item_id.clone();
    let token = cancel.clone();
    let exec = locked(&fx).with_step_hook(Arc::new(move |p: &StepPoint<'_>| {
        if matches!(p, StepPoint::AfterAudit(i) if **i == first) {
            token.cancel(); // Ctrl-C
        }
    }));
    let mut operation = exec
        .begin_operation(&mut s, &plan, &start(&plan, "C1"))
        .unwrap()
        .with_quarantine(settings(&fx));
    let r = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(&fx),
            Some(&mut operation),
            &cancel,
        )
        .unwrap();
    let summary = operation
        .finish(OperationEnd::Cancelled, now_unix())
        .unwrap();
    print_report(&fx, &r);
    assert!(r.cancelled);
    assert_eq!(r.error_kind(), Some(ErrorKind::Cancelled));
    assert!(r.items[0].succeeded());
    assert!(
        r.items[1..]
            .iter()
            .all(|i| i.outcome == ItemOutcome::NotRun)
    );
    assert_eq!(summary.status, OperationStatus::Cancelled);
    assert_eq!(summary.totals.items_done, 1);
    assert_eq!(summary.totals.items_missing(), 2);
    let op = OperationId::from_suffix("C1").unwrap();
    assert!(
        s.journal_steps(&op)
            .unwrap()
            .iter()
            .all(|st| st.close.is_some())
    );
    // The items not run are untouched.
    let first_rel = rel(&fx, &plan.items()[0].path.to_path_buf());
    let rest: Vec<String> = plan.items()[1..]
        .iter()
        .map(|i| rel(&fx, &i.path.to_path_buf()))
        .collect();
    for r in &rest {
        assert!(fx.root().join(r).exists(), "{r} must be untouched");
    }
    drop(exec);
    assert!(locked(&fx).recover(&mut s, now_unix()).unwrap().is_empty());

    // Rerun: a new plan of what is left completes it. (Plans record whole seconds and an
    // operation that started in the plan's second counts as touching it, SAFE-09: the user
    // re-plans after the cancel.)
    std::thread::sleep(Duration::from_millis(1100));
    let rest_refs: Vec<&str> = rest.iter().map(String::as_str).collect();
    let (plan2, r2) = run(&fx, &h, &mut s, &rest_refs, "C2");
    assert!(r2.items.iter().all(|i| i.succeeded()), "{r2:#?}");
    for r in &rest {
        assert!(!fx.root().join(r).exists());
    }
    assert!(!fx.root().join(&first_rel).exists());
    let _ = plan2;
    let inv = fx.check_invariants(
        &scull_fixtures::InvariantInputs::new()
            .plan(
                plan.items()
                    .iter()
                    .map(|i| (i.item_id.clone(), i.tier, i.action.clone())),
            )
            .allow(ITEMS[0])
            .allow(ITEMS[1])
            .allow(ITEMS[2]),
    );
    println!("{inv}");
    inv.assert_ok();
}

/// Recovery needs the execution lock; a recovered item's journal step is closed as
/// `recovered`, never as `done`.
#[test]
fn recovery_needs_the_lock_and_closes_steps_as_recovered() {
    let fx = fixture();
    let mut s = store(&fx);
    let err = Executor::new().recover(&mut s, now_unix()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::SafetyBlocked);
    // Kill right after the S1 move: the step is open, the move happened.
    let h = harness(false);
    let plan = plan_named(&fx, &h.scan(&fx), &["app/target"], "R1");
    s.insert_plan(&plan).unwrap();
    drop(s);
    let before = TreeSnapshot::capture(fx.root()).unwrap();
    assert!(run_and_kill(
        &fx,
        &plan,
        "R1",
        "app/target",
        "after_move",
        0
    ));
    let mut s = store(&fx);
    let report = locked(&fx).recover(&mut s, now_unix()).unwrap();
    let item = &report.operations[0].items[0];
    assert_eq!(item.how, Recovery::Reconciled(JournalAction::Quarantine));
    assert_eq!(item.outcome, AuditOutcome::Quarantined);
    assert!(
        item.manifest_entry.is_some(),
        "the unrecorded move is recorded once"
    );
    let op = OperationId::from_suffix("R1").unwrap();
    let steps = s.journal_steps(&op).unwrap();
    assert_eq!(
        steps[0].close.as_ref().unwrap().kind,
        JournalCloseKind::Recovered
    );
    assert_audit_matches_disk(&fx, &s, &op, &before);
    assert_quarantine_consistent(&fx, &s, &op);
    // The recovered entry is restorable like any other.
    let entry = s.quarantine_held().unwrap().remove(0);
    let restored = locked(&fx)
        .restore(&mut s, entry.entry_id, now_unix())
        .unwrap();
    println!("restored: {restored:?}");
    assert!(fx.root().join("app/target").exists());
    let inv = fx.check_invariants(&scull_fixtures::InvariantInputs::new().allow("app/target"));
    println!("{inv}");
    inv.assert_ok();
}
