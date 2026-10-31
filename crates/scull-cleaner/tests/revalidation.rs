//! Revalidation before every item (P2-EXEC-03, SAFE-05): plans are made from a real engine scan
//! of a `scull-fixtures` tree, the tree changes between planning and execution, and
//! [`Executor::execute_plan`] must skip (and report) every item that no longer qualifies while
//! still executing the others. Every test checks the invariants (I-1, I-2, I-3, I-8).
//!
//! Fixture IDs (docs/FIXTURES.md): FX-RACE-01, FX-RACE-03, FX-RACE-04, FX-EXEC-10.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_cleaner::{
    ExecutionReport, Executor, ItemOutcome, PlanContext, RemoveOutcome, Revalidation, Skip,
};
use scull_core::config::Config;
use scull_core::{
    Action, Candidate, DropReason, ErrorKind, ItemId, Plan, PlanId, PlanItem, PlanOrigin,
    PlanParams, Tier,
};
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, InUseAnswer, InUseProbe, InUseSource, ScanReport,
    ScanRequest, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs, InvariantReport};
use scull_git::GitRunner;
use scull_platform::Native;
use scull_safety::Tri;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Activity checks run three days ahead, so freshly written fixture files are not "recent".
const THREE_DAYS: i64 = 3 * 24 * 3600;
const MACHINE: &str = "machine-test";
const USER: &str = "tester";

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// An in-use probe standing in for the OS: a held Cargo lock (`target/debug/.cargo-lock`) is
/// reported in use while `build_running` is set, i.e. after "a build starts".
#[derive(Default)]
struct CargoBuildProbe {
    build_running: AtomicBool,
}

impl InUseProbe for CargoBuildProbe {
    fn check(&self, _dir: &Path, lock_files: &[PathBuf], _processes: &[String]) -> InUseAnswer {
        let held = self.build_running.load(Ordering::SeqCst)
            && lock_files
                .iter()
                .any(|l| l.ends_with("debug/.cargo-lock") && l.exists());
        InUseAnswer {
            in_use: if held { Tri::Yes } else { Tri::No },
            evidence: Vec::new(),
        }
    }
}

struct Harness {
    engine: Engine,
    probe: Arc<CargoBuildProbe>,
}

fn harness(with_git: bool) -> Harness {
    let probe = Arc::new(CargoBuildProbe::default());
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    // Temp dirs are tmpfs on some machines; the D-22 volume policy is tested in scull-platform.
    cfg.allow_network_roots = true;
    if with_git {
        cfg.git = Some(GitRunner::new().expect("trusted git"));
    }
    cfg.in_use = InUseSource::Probe(probe.clone());
    Harness {
        engine: Engine::new(cfg).unwrap(),
        probe,
    }
}

fn request() -> ScanRequest {
    ScanRequest::full()
        .history(false)
        .at(now_unix() + THREE_DAYS)
}

impl Harness {
    /// The planning scan of the whole fixture root.
    fn scan(&self, fx: &Fixture) -> ScanReport {
        self.engine
            .scan(&[canon(fx.root())], &request(), &CancelToken::new(), |_| {})
            .unwrap()
    }

    fn revalidation(&self, fx: &Fixture) -> Revalidation<'_> {
        Revalidation::new(&self.engine, &[fx.root().to_path_buf()]).with_request(request())
    }

    fn execute(&self, fx: &Fixture, plan: &Plan) -> ExecutionReport {
        let report = Executor::new()
            .execute_plan(plan, &ctx(), &self.revalidation(fx), &CancelToken::new())
            .unwrap();
        print_report(fx, &report);
        report
    }
}

fn ctx() -> PlanContext<'static> {
    PlanContext {
        now_unix: now_unix(),
        machine_id: MACHINE,
        user: USER,
    }
}

fn rel(fx: &Fixture, p: &Path) -> String {
    let root = canon(fx.root());
    p.strip_prefix(&root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The candidate at `rel_path`.
fn at<'a>(fx: &Fixture, r: &'a ScanReport, rel_path: &str) -> &'a Candidate {
    r.candidates
        .iter()
        .find(|c| rel(fx, &c.path.to_path_buf()) == rel_path)
        .unwrap_or_else(|| {
            let all: Vec<String> = r
                .candidates
                .iter()
                .map(|c| {
                    format!(
                        "{} {} {:?}",
                        rel(fx, &c.path.to_path_buf()),
                        c.tier,
                        c.action
                    )
                })
                .collect();
            panic!("no candidate at {rel_path}: {all:#?}")
        })
}

/// A plan of the candidates at `rels` (as reviewed in the preview).
fn plan(fx: &Fixture, r: &ScanReport, rels: &[&str]) -> Plan {
    let items = rels
        .iter()
        .map(|p| PlanItem::from_candidate(at(fx, r, p)).unwrap())
        .collect();
    Plan::new(
        PlanParams {
            id: PlanId::from_suffix("01JREVALIDATION").unwrap(),
            scan_id: r.scan_id.clone(),
            created_unix: now_unix(),
            ttl_secs: None,
            machine_id: MACHINE.into(),
            user: USER.into(),
            origin: PlanOrigin::Cli,
        },
        items,
    )
    .unwrap()
}

fn print_report(fx: &Fixture, r: &ExecutionReport) {
    println!("--- execution report ({} items) ---", r.items.len());
    for i in &r.items {
        let what = match &i.outcome {
            ItemOutcome::Deleted(d) => format!("deleted ({:?})", d.outcome),
            ItemOutcome::Skipped(s) => format!("SKIPPED [{}] {s}", s.code()),
            ItemOutcome::Refused(e) => format!("refused [{}] {}", e.kind, e.message),
            ItemOutcome::NotRun => "not run".into(),
            other => format!("{other:?}"),
        };
        println!(
            "  {:<24} planned {} fresh {:<4} executed-under {}  {what}",
            rel(fx, &i.path.to_path_buf()),
            i.planned_tier,
            i.fresh_tier.map_or("-".into(), |t| t.to_string()),
            i.executed_tier(),
        );
    }
    println!("  exit kind: {:?}", r.error_kind());
}

/// I-1 (no S3 in the executed plan), I-2 (canary), I-3 (only `allowed` changed since the last
/// checkpoint) and I-8 (no executed tier lower than the planned one).
fn check(fx: &Fixture, plan: &Plan, report: &ExecutionReport, allowed: &[&str]) -> InvariantReport {
    let scan: BTreeMap<ItemId, Tier> = plan
        .items()
        .iter()
        .map(|i| (i.item_id.clone(), i.tier))
        .collect();
    let exec: BTreeMap<ItemId, Tier> = report
        .items
        .iter()
        .map(|i| (i.item_id.clone(), i.executed_tier()))
        .collect();
    let mut inputs = InvariantInputs::new()
        .plan(
            report
                .items
                .iter()
                .filter(|i| matches!(i.outcome, ItemOutcome::Deleted(_)))
                .map(|i| (i.item_id.clone(), i.executed_tier(), Action::Delete)),
        )
        .tiers(scan, exec);
    for a in allowed {
        inputs = inputs.allow(*a);
    }
    let report = fx.check_invariants(&inputs);
    println!("{report}");
    report.assert_ok();
    report
}

fn skip_of<'a>(fx: &Fixture, r: &'a ExecutionReport, rel_path: &str) -> &'a Skip {
    let item = r
        .items
        .iter()
        .find(|i| rel(fx, &i.path.to_path_buf()) == rel_path)
        .unwrap();
    match &item.outcome {
        ItemOutcome::Skipped(s) => s,
        other => panic!("{rel_path} was not skipped: {other:?}"),
    }
}

/// On Unix the walker removes the item; on Windows (walker: #68) it refuses with nothing
/// removed. Either way the item passed revalidation.
fn assert_executed(fx: &Fixture, r: &ExecutionReport, rel_path: &str) {
    let item = r
        .items
        .iter()
        .find(|i| rel(fx, &i.path.to_path_buf()) == rel_path)
        .unwrap();
    if cfg!(unix) {
        match &item.outcome {
            ItemOutcome::Deleted(d) => assert_eq!(d.outcome, RemoveOutcome::Complete, "{d:#?}"),
            other => panic!("{rel_path} was not deleted: {other:?}"),
        }
        assert!(!fx.root().join(rel_path).exists());
    } else {
        match &item.outcome {
            ItemOutcome::Refused(e) => assert_eq!(e.kind, ErrorKind::UnsupportedPlatform),
            other => panic!("{rel_path}: {other:?}"),
        }
    }
}

/// Two Cargo projects with a `target/` each and a held-able Cargo lock file.
fn two_cargo_projects() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 4096)
        .unwrap()
        .file("app/target/debug/.cargo-lock", "")
        .unwrap()
        .cargo_project("lib", "lib")
        .unwrap()
        .cargo_target("lib", "lib", 2048)
        .unwrap()
        .file("lib/target/debug/.cargo-lock", "")
        .unwrap();
    fx.checkpoint().unwrap();
    fx
}

// ------------------------------------------------------------------------------------------

/// FX-EXEC-10: a plan item became S3 since planning (a `.scull-protect` marker appeared in its
/// project) → skipped with reason `now_protected`; the other item still runs.
#[test]
fn fx_exec_10_item_became_s3_since_planning_is_skipped_with_reason() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    assert_eq!(at(&fx, &scan, "app/target").tier, Tier::S0);
    assert_eq!(at(&fx, &scan, "lib/target").tier, Tier::S0);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);

    // After the review, the user protects `app`.
    std::fs::write(fx.root().join("app/.scull-protect"), "").unwrap();
    let mut fx = fx;
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    let skip = skip_of(&fx, &r, "app/target");
    assert_eq!(
        skip,
        &Skip::Dropped {
            reason: DropReason::NowProtected,
            fresh_tier: Some(Tier::S3)
        }
    );
    assert_eq!(skip.code(), "now_protected");
    assert!(fx.root().join("app/target/debug/app").is_file());
    assert_executed(&fx, &r, "lib/target");
    assert_eq!(r.error_kind(), Some(ErrorKind::PartialFailure));
    check(&fx, &plan, &r, &["lib/target"]);
}

/// FX-RACE-04: a build starts after the scan (Cargo's lock is held) → the fresh in-use probe
/// sees it at revalidation; the item is skipped ("in use"), the idle project still runs.
#[test]
fn fx_race_04_build_started_after_scan_item_skipped_in_use() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);

    // `cargo build` starts in `app` only: its lock is held from now on.
    h.probe.build_running.store(true, Ordering::SeqCst);
    std::fs::remove_file(fx.root().join("lib/target/debug/.cargo-lock")).unwrap();
    let mut fx = fx;
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    let skip = skip_of(&fx, &r, "app/target");
    match skip {
        Skip::Dropped {
            reason: DropReason::TierRaised { planned, fresh },
            fresh_tier,
        } => {
            assert_eq!(*planned, Tier::S0);
            assert!(*fresh > Tier::S0);
            assert_eq!(*fresh_tier, Some(*fresh));
        }
        Skip::Dropped {
            reason: DropReason::NowProtected,
            ..
        } => {}
        other => panic!("expected an in-use skip, got {other:?}"),
    }
    assert!(fx.root().join("app/target/debug/app").is_file());
    assert_executed(&fx, &r, "lib/target");
    check(&fx, &plan, &r, &["lib/target"]);
}

/// FX-RACE-01: the candidate is replaced by a symlink to the canary directory between plan and
/// execute → skipped (it is no longer the planned object); the canary is intact.
#[test]
fn fx_race_01_candidate_replaced_by_symlink_skipped_canary_intact() {
    let mut fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);

    // The attacker moves `app/target` away and links the canary in its place.
    std::fs::rename(fx.root().join("app/target"), fx.root().join("app/moved")).unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    if !fx.symlink("app/target", &canary).unwrap().is_created() {
        eprintln!("symlinks unavailable on this host: FX-RACE-01 covered by the walker tests");
        return;
    }
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    let skip = skip_of(&fx, &r, "app/target");
    assert!(
        matches!(
            skip,
            Skip::Dropped {
                reason: DropReason::Missing | DropReason::IdentityChanged,
                ..
            }
        ),
        "{skip:?}"
    );
    assert!(std::fs::symlink_metadata(fx.root().join("app/target")).is_ok());
    assert!(fx.root().join("app/moved/debug/app").is_file());
    assert!(canary.join("keep.txt").is_file());
    assert_executed(&fx, &r, "lib/target");
    check(&fx, &plan, &r, &["lib/target"]);
}

/// A directory of the same name but another identity (rebuilt or swapped) is not the planned
/// object: skipped.
#[test]
fn replaced_directory_with_new_identity_is_skipped() {
    let mut fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target"]);

    std::fs::rename(
        fx.root().join("app/target"),
        fx.root().join("app/old-target"),
    )
    .unwrap();
    fx.cargo_target("app", "app", 4096).unwrap();
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    assert!(matches!(
        skip_of(&fx, &r, "app/target"),
        Skip::Dropped {
            reason: DropReason::Missing | DropReason::IdentityChanged,
            ..
        }
    ));
    assert!(fx.root().join("app/target/debug/app").is_file());
    check(&fx, &plan, &r, &[]);
}

/// FX-RACE-03: a new untracked file appears in a linked worktree after the scan → the fresh
/// Git state raises its tier at revalidation; the item is skipped.
#[test]
fn fx_race_03_new_untracked_file_in_worktree_raises_tier_item_skipped() {
    if !git_available() {
        eprintln!("git not available: FX-RACE-03 needs a repository");
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let repo = fx.git_repo("repo").unwrap();
    fx.file("repo/README.md", "hello\n").unwrap();
    repo.commit_all("initial").unwrap();
    repo.run(["worktree", "add", "--quiet", "-b", "feature", "../repo-wt"])
        .unwrap();
    fx.checkpoint().unwrap();

    let h = harness(true);
    let scan = h.scan(&fx);
    let wt = at(&fx, &scan, "repo-wt");
    println!("planned worktree: {} {:?}", wt.tier, wt.action);
    assert!(wt.tier.is_actionable(), "{wt:#?}");
    let plan = plan(&fx, &scan, &["repo-wt"]);

    // Someone starts working in the worktree.
    std::fs::write(fx.root().join("repo-wt/notes.txt"), "uncommitted work\n").unwrap();
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    let skip = skip_of(&fx, &r, "repo-wt");
    let fresh = skip.fresh_tier().expect("re-derived");
    assert!(fresh > wt.tier, "{skip:?}");
    assert!(matches!(
        skip,
        Skip::Dropped {
            reason: DropReason::TierRaised { .. } | DropReason::NowProtected,
            ..
        }
    ));
    assert_eq!(
        std::fs::read(fx.root().join("repo-wt/notes.txt")).unwrap(),
        b"uncommitted work\n"
    );
    check(&fx, &plan, &r, &[]);
}

/// Nothing changed: every item passes revalidation and is executed under its planned tier.
#[test]
fn unchanged_items_pass_revalidation() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);
    let r = h.execute(&fx, &plan);
    for i in &r.items {
        assert_eq!(i.fresh_tier, Some(Tier::S0));
        assert_eq!(i.executed_tier(), Tier::S0);
    }
    assert_executed(&fx, &r, "app/target");
    assert_executed(&fx, &r, "lib/target");
    if cfg!(unix) {
        assert_eq!(r.error_kind(), None);
    }
    check(&fx, &plan, &r, &["app/target", "lib/target"]);
}

/// An expired plan is rejected (exit 4) before anything is revalidated or touched.
#[test]
fn expired_or_foreign_plan_is_rejected_before_any_item() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target"]);
    let reval = h.revalidation(&fx);
    let late = PlanContext {
        now_unix: plan.expires_unix(),
        ..ctx()
    };
    let e = Executor::new()
        .execute_plan(&plan, &late, &reval, &CancelToken::new())
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanExpired);
    let foreign = PlanContext {
        user: "someone-else",
        ..ctx()
    };
    let e = Executor::new()
        .execute_plan(&plan, &foreign, &reval, &CancelToken::new())
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanForeign);
    assert_eq!(e.exit_code().as_i32(), 4);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Items outside the trusted roots are skipped; nothing is touched.
#[test]
fn item_outside_the_trusted_roots_is_skipped() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target"]);
    let reval = Revalidation::new(&h.engine, &[fx.root().join("lib")]).with_request(request());
    let r = Executor::new()
        .execute_plan(&plan, &ctx(), &reval, &CancelToken::new())
        .unwrap();
    print_report(&fx, &r);
    assert_eq!(skip_of(&fx, &r, "app/target"), &Skip::OutsideRoots);
    check(&fx, &plan, &r, &[]);
}

/// A cancelled operation attempts nothing further and says so.
#[test]
fn cancelled_operation_runs_no_item() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let r = Executor::new()
        .execute_plan(&plan, &ctx(), &h.revalidation(&fx), &cancel)
        .unwrap();
    assert!(r.cancelled);
    assert!(r.items.iter().all(|i| i.outcome == ItemOutcome::NotRun));
    assert_eq!(r.error_kind(), Some(ErrorKind::Cancelled));
    check(&fx, &plan, &r, &[]);
}
