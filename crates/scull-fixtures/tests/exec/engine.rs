//! Plan-level execution through the real pipeline: the read-only engine scans the fixture,
//! plans are built from its candidates exactly as the preview would, and the real Executor
//! runs them under the execution lock inside an audited operation (quarantine beside the root,
//! store and lock beside the root, never inside it). [`inputs`] feeds every invariant the run
//! can: I-1 and I-8 from the execution report, I-3 the allowed paths, I-4 the operation's audit.

use scull_cleaner::{
    ExecutionReport, Executor, ItemOutcome, LockOptions, Operation, PlanContext, QuarantineSettings,
};
use scull_core::config::Config;
use scull_core::{
    Action, Candidate, ItemId, OperationId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams, Tier,
};
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, InUseAnswer, InUseProbe, InUseSource, PlatformOwnerOnly,
    ScanReport, ScanRequest, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs, NodeLock};
use scull_platform::Native;
use scull_safety::Tri;
use scull_storage::{ApprovalMethod, OperationEnd, OperationStart, Store};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Activity checks run three days ahead, so freshly written fixture files are not "recent".
pub const THREE_DAYS: i64 = 3 * 24 * 3600;
pub const MACHINE: &str = "machine-test";
pub const USER: &str = "tester";

/// Nothing is in use (the fixtures that need "in use" live in scull-cleaner).
struct NotInUse;

impl InUseProbe for NotInUse {
    fn check(&self, _dir: &Path, _lock_files: &[PathBuf], _processes: &[String]) -> InUseAnswer {
        InUseAnswer {
            in_use: Tri::No,
            evidence: Vec::new(),
        }
    }
}

pub struct Harness {
    pub engine: Engine,
}

pub fn harness() -> Harness {
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    // Temp dirs are tmpfs on some machines; the D-22 volume policy is tested in scull-platform.
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(NotInUse));
    Harness {
        engine: Engine::new(cfg).unwrap(),
    }
}

pub fn request() -> ScanRequest {
    ScanRequest::full()
        .history(false)
        .at(now_unix() + THREE_DAYS)
}

impl Harness {
    pub fn scan(&self, fx: &Fixture) -> ScanReport {
        self.engine
            .scan(&[canon(fx.root())], &request(), &CancelToken::new(), |_| {})
            .unwrap()
    }

    pub fn revalidation(&self, fx: &Fixture) -> scull_cleaner::Revalidation<'_> {
        scull_cleaner::Revalidation::new(&self.engine, &[fx.root().to_path_buf()])
            .with_request(request())
    }
}

/// * `lib/target`: idle Cargo build output → S0, delete;
/// * `app/target`: Cargo build output touched within the activity window → S1, quarantine;
/// * `web/node_modules`: npm project without a lockfile → S2, quarantine.
pub fn three_tiers() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    let recent = std::time::SystemTime::now()
        + std::time::Duration::from_secs(u64::try_from(THREE_DAYS).unwrap() - 600);
    fx.npm_project("web", "web", NodeLock::None)
        .unwrap()
        .node_modules("web", &["left-pad", "react"])
        .unwrap()
        .file_of_size("web/node_modules/react/big.js", 64 * 1024)
        .unwrap()
        .cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 96 * 1024)
        .unwrap()
        .set_mtime("app/target/debug/app", recent)
        .unwrap()
        .cargo_project("lib", "lib")
        .unwrap()
        .cargo_target("lib", "lib", 4096)
        .unwrap();
    fx.checkpoint().unwrap();
    fx
}

/// An Executor holding the execution lock, kept beside the root.
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

/// `p` relative to the fixture root, with `/`: the root as scull canonicalizes it (scans) or
/// as it was given on the command line (on Windows an 8.3 short temp path such as
/// `C:\Users\RUNNER~1\...`).
pub fn rel(fx: &Fixture, p: &Path) -> String {
    p.strip_prefix(canon(fx.root()))
        .or_else(|_| p.strip_prefix(fx.root()))
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn at<'a>(fx: &Fixture, r: &'a ScanReport, rel_path: &str) -> &'a Candidate {
    r.candidates
        .iter()
        .find(|c| rel(fx, &c.path.to_path_buf()) == rel_path)
        .unwrap_or_else(|| {
            let all: Vec<String> = r
                .candidates
                .iter()
                .map(|c| format!("{} {}", rel(fx, &c.path.to_path_buf()), c.tier))
                .collect();
            panic!("no candidate at {rel_path}: {all:#?}")
        })
}

/// A plan of the candidates at `rels`, as reviewed in the preview.
pub fn plan(fx: &Fixture, r: &ScanReport, rels: &[&str], id: &str) -> Plan {
    let items = rels
        .iter()
        .map(|p| PlanItem::from_candidate(at(fx, r, p)).unwrap())
        .collect();
    Plan::new(
        PlanParams {
            id: PlanId::from_suffix(id).unwrap(),
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

pub fn store(fx: &Fixture) -> Store {
    Store::open(&fx.base().join("data").join("scull.db"), &PlatformOwnerOnly).unwrap()
}

pub fn start(plan: &Plan, op: &OperationId) -> OperationStart {
    OperationStart {
        operation_id: op.clone(),
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        approver: USER.into(),
        method: ApprovalMethod::CliInteractive,
        started_at: now_unix(),
    }
}

/// Record `plan`, begin an operation `op`, execute it (quarantine beside the root) and finish.
pub fn run(
    fx: &Fixture,
    h: &Harness,
    s: &mut Store,
    plan: &Plan,
    op: &OperationId,
) -> ExecutionReport {
    s.insert_plan(plan).unwrap();
    let exec = locked(fx);
    let mut operation: Operation<'_> = exec
        .begin_operation(s, plan, &start(plan, op))
        .unwrap()
        .with_quarantine(QuarantineSettings {
            base: Some(fx.base().to_path_buf()),
            ..QuarantineSettings::default()
        });
    let r = exec
        .execute_plan(
            plan,
            &ctx(),
            &h.revalidation(fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    print_report(fx, &r);
    r
}

pub fn print_report(fx: &Fixture, r: &ExecutionReport) {
    println!("--- execution report ({} items) ---", r.items.len());
    for i in &r.items {
        let what = match &i.outcome {
            ItemOutcome::Deleted(d) => format!("deleted ({:?})", d.outcome),
            ItemOutcome::Skipped(s) => format!("SKIPPED [{}] {s}", s.code()),
            ItemOutcome::Refused(e) => format!("refused [{}] {}", e.kind, e.message),
            ItemOutcome::Quarantined(q) => format!("quarantined ({} B held)", q.bytes),
            ItemOutcome::Blocked { code, .. } => format!("BLOCKED [{code}]"),
            other => format!("{other:?}"),
        };
        println!(
            "  {:<20} planned {} executed-under {}  {what}",
            rel(fx, &i.path.to_path_buf()),
            i.planned_tier,
            i.executed_tier(),
        );
    }
    println!(
        "  exit: {:?} ({})",
        r.error_kind(),
        r.error_kind()
            .map_or(0, |k| scull_core::ScullError::new(k, "")
                .exit_code()
                .as_i32())
    );
}

/// I-1 on what was executed, I-8 planned vs executed tiers, I-3 `allowed`, I-4 the audit of
/// `op` in `s`.
pub fn inputs(
    plan: &Plan,
    report: &ExecutionReport,
    s: &Store,
    op: &OperationId,
    allowed: &[&str],
) -> InvariantInputs {
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
        .plan(report.items.iter().filter_map(|i| {
            let action = match i.outcome {
                ItemOutcome::Deleted(_) => Action::Delete,
                ItemOutcome::Quarantined(_) => Action::Quarantine,
                _ => return None,
            };
            Some((i.item_id.clone(), i.executed_tier(), action))
        }))
        .tiers(scan, exec)
        .audit(s, op);
    for a in allowed {
        inputs = inputs.allow(*a);
    }
    inputs
}
