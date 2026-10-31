//! Shared helpers for plan-level Executor fixtures: a real engine over a `scull-fixtures`
//! tree, plans built from its candidates, execution reports and invariant checks.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use scull_cleaner::{
    ExecutionReport, Executor, ItemOutcome, LockOptions, PlanContext, RemoveOutcome, Revalidation,
    Skip,
};
use scull_core::config::Config;
use scull_core::{
    Action, Candidate, ErrorKind, ItemId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams, Tier,
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
pub const THREE_DAYS: i64 = 3 * 24 * 3600;
pub const MACHINE: &str = "machine-test";
pub const USER: &str = "tester";

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// An in-use probe standing in for the OS: a held Cargo lock (`target/debug/.cargo-lock`) is
/// reported in use while `build_running` is set, i.e. after "a build starts".
#[derive(Default)]
pub struct CargoBuildProbe {
    pub build_running: AtomicBool,
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

pub struct Harness {
    pub engine: Engine,
    pub probe: Arc<CargoBuildProbe>,
}

pub fn harness(with_git: bool) -> Harness {
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

pub fn request() -> ScanRequest {
    ScanRequest::full()
        .history(false)
        .at(now_unix() + THREE_DAYS)
}

impl Harness {
    /// The planning scan of the whole fixture root.
    pub fn scan(&self, fx: &Fixture) -> ScanReport {
        self.engine
            .scan(&[canon(fx.root())], &request(), &CancelToken::new(), |_| {})
            .unwrap()
    }

    pub fn revalidation(&self, fx: &Fixture) -> Revalidation<'_> {
        Revalidation::new(&self.engine, &[fx.root().to_path_buf()]).with_request(request())
    }

    pub fn execute(&self, fx: &Fixture, plan: &Plan) -> ExecutionReport {
        let report = locked(fx)
            .execute_plan(plan, &ctx(), &self.revalidation(fx), &CancelToken::new())
            .unwrap();
        print_report(fx, &report);
        report
    }
}

/// An Executor holding the execution lock, kept beside (never inside) the fixture root.
pub fn locked(fx: &Fixture) -> Executor {
    Executor::acquire(&LockOptions::at(
        fx.base().join("state").join("execution.lock"),
    ))
    .unwrap()
}

pub fn ctx() -> PlanContext<'static> {
    PlanContext {
        now_unix: now_unix(),
        machine_id: MACHINE,
        user: USER,
    }
}

pub fn rel(fx: &Fixture, p: &Path) -> String {
    let root = canon(fx.root());
    p.strip_prefix(&root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The candidate at `rel_path`.
pub fn at<'a>(fx: &Fixture, r: &'a ScanReport, rel_path: &str) -> &'a Candidate {
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
pub fn plan(fx: &Fixture, r: &ScanReport, rels: &[&str]) -> Plan {
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

pub fn print_report(fx: &Fixture, r: &ExecutionReport) {
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
pub fn check(
    fx: &Fixture,
    plan: &Plan,
    report: &ExecutionReport,
    allowed: &[&str],
) -> InvariantReport {
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

pub fn skip_of<'a>(fx: &Fixture, r: &'a ExecutionReport, rel_path: &str) -> &'a Skip {
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
pub fn assert_executed(fx: &Fixture, r: &ExecutionReport, rel_path: &str) {
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
pub fn two_cargo_projects() -> Fixture {
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
