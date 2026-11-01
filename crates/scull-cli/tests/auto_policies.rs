//! Opt-in unattended auto-policies (P5-PRO-02; PRO-10, D-11, T-05, FX-EXEC-15), in process.
//!
//! The policy runner (`scull_cli::cmd::auto_policy`) is driven directly with a real engine over
//! a `scull-fixtures` tree, a temp database, an execution lock and a quarantine inside the
//! fixture's temp directory. The engine's in-use probe is a stand-in that reports nothing in use,
//! so the idle build outputs are S0 on every machine (the OS probe can't inspect every process
//! in some sandboxes, which makes them S2; `tests/check.rs` covers the binary). The decision
//! ([`prepare`]) and the run ([`execute`]) are separate, so these tests change the disk between
//! them, the way the world changes between a scheduled check's scan and its execution.
//!
//! After every test the fixture invariants run: the canary is untouched (I-2), nothing outside
//! the moved items changed in the root (I-3), nothing S3 was executed (I-1), no executed tier
//! is lower than planned (I-8); after a run, the audit matches the disk (I-4).
//!
//! Cross-mount items are covered by `scull_proactive::auto`'s selection tests (a mount can't be
//! created without privileges) and by the quarantine's same-volume rule in `scull-cleaner`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_cleaner::{Executor, LockOptions, PlanContext, Revalidation};
use scull_cli::cmd::auto_policy::{
    AutoPolicyReport, Gate, Inputs, PolicyOutcome, Prepared, RunState, STATE_FILE, acquire,
    execute, prepare, read_state,
};
use scull_cli::cmd::exec::Who;
use scull_core::config::{
    AutoConfig, AutoKind, AutoPolicy, Config, DurationSpec, QuarantineConfig,
};
use scull_core::{
    Action, ErrorKind, ItemId, OperationId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams, Tier,
};
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, InUseAnswer, InUseProbe, InUseSource, ScanReport,
    ScanRequest, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs, LinkOutcome, days_ago};
use scull_platform::Native;
use scull_safety::Tri;
use scull_storage::{ApprovalMethod, DefaultPermissions, OperationStart, Store};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DAY: i64 = 86_400;

/// Nothing is in use (see the module docs).
struct Idle;

impl InUseProbe for Idle {
    fn check(&self, _dir: &Path, _locks: &[PathBuf], _procs: &[String]) -> InUseAnswer {
        InUseAnswer {
            in_use: Tri::No,
            evidence: Vec::new(),
        }
    }
}

struct H {
    fx: Fixture,
    data: PathBuf,
    engine: Engine,
    store: Store,
    who: Who,
    quarantine: QuarantineConfig,
    /// The config file's scan roots (the fixture root, canonical).
    roots: Vec<PathBuf>,
    /// The execution lock, held from `prepare` to the end of `execute`.
    lock: Option<Executor>,
}

/// Cargo projects named `names`, each with a `target/`, idle for 90 days.
fn harness(names: &[&str]) -> H {
    let mut fx = Fixture::new().unwrap();
    for n in names {
        fx.cargo_project(n, n)
            .unwrap()
            .cargo_target(n, n, 8 * 1024)
            .unwrap()
            .age(n, days_ago(90))
            .unwrap();
    }
    fx.checkpoint().unwrap();
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    // Temp dirs are tmpfs on some machines; the D-22 volume policy is tested in scull-platform.
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(Idle));
    let data = fx.base().join("scull-data");
    std::fs::create_dir_all(&data).unwrap();
    let store = Store::open(&data.join(scull_storage::DB_FILE_NAME), &DefaultPermissions).unwrap();
    let quarantine = QuarantineConfig {
        base: Some(fx.quarantine_base().to_path_buf()),
        ..QuarantineConfig::default()
    };
    H {
        engine: Engine::new(cfg).unwrap(),
        data,
        fx,
        store,
        who: Who {
            machine_id: "machine-test".into(),
            user: "tester".into(),
        },
        quarantine,
        roots: Vec::new(),
        lock: None,
    }
    .with_roots()
}

fn policy(name: &str) -> AutoPolicy {
    AutoPolicy {
        name: name.into(),
        enabled: true,
        kind: AutoKind::BuildOutputs,
        idle: DurationSpec::from_hours(60 * 24),
        every: DurationSpec::from_hours(7 * 24),
        under: Vec::new(),
    }
}

fn auto_with(policies: Vec<AutoPolicy>) -> AutoConfig {
    AutoConfig {
        policy: policies,
        ..AutoConfig::default()
    }
}

impl H {
    fn with_roots(mut self) -> Self {
        self.roots = vec![canon(self.fx.root())];
        self
    }

    fn lock_options(&self) -> LockOptions {
        LockOptions::at(self.fx.base().join("state").join("execution.lock"))
    }

    fn root(&self) -> PathBuf {
        canon(self.fx.root())
    }

    fn data(&self) -> PathBuf {
        self.data.clone()
    }

    fn scan(&self) -> ScanReport {
        self.engine
            .scan(
                &[self.root()],
                &ScanRequest::full().history(false),
                &CancelToken::new(),
                |_| {},
            )
            .unwrap()
    }

    /// Take the execution lock (if not held yet) and decide.
    fn try_prepare(
        &mut self,
        auto: &AutoConfig,
        scan: &ScanReport,
        now: i64,
    ) -> Result<Prepared, AutoPolicyReport> {
        if self.lock.is_none() {
            self.lock = Some(acquire(&self.lock_options(), &mut self.store, now)?);
        }
        let inputs = inputs(
            &self.data,
            &self.quarantine,
            &self.who,
            &self.roots,
            auto,
            now,
        );
        Ok(prepare(
            &inputs,
            self.lock.as_ref().unwrap(),
            &scan.scan_id,
            &scan.candidates,
            &self.store,
        ))
    }

    fn prepare(&mut self, auto: &AutoConfig, scan: &ScanReport, now: i64) -> Prepared {
        self.try_prepare(auto, scan, now).unwrap()
    }

    /// Run under the lock `prepare` took, then release it.
    fn execute(&mut self, auto: &AutoConfig, prepared: Prepared, now: i64) -> AutoPolicyReport {
        let executor = self.lock.take().expect("prepare took the lock");
        let inputs = inputs(
            &self.data,
            &self.quarantine,
            &self.who,
            &self.roots,
            auto,
            now,
        );
        let roots = [canon(self.fx.root())];
        let r = execute(
            &inputs,
            &executor,
            prepared,
            &self.engine,
            &roots,
            None,
            &mut self.store,
            &CancelToken::new(),
        );
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
        r
    }

    /// Decide and run at once (lock taken and released).
    fn run(&mut self, auto: &AutoConfig, now: i64) -> (ScanReport, AutoPolicyReport) {
        let scan = self.scan();
        let p = self.prepare(auto, &scan, now);
        (scan.clone(), self.execute(auto, p, now))
    }

    /// Preview at `t0`, then decide again one interval later: the run is planned.
    fn previewed_then_due(&mut self, auto: &AutoConfig, t0: i64) -> (ScanReport, Prepared) {
        let scan = self.scan();
        let p = self.prepare(auto, &scan, t0);
        let r = self.execute(auto, p, t0);
        assert!(
            r.policies
                .iter()
                .all(|p| matches!(p.outcome, PolicyOutcome::Previewed { .. })),
            "{r:?}"
        );
        let scan = self.scan();
        let p = self.prepare(auto, &scan, t0 + 7 * DAY + 1);
        (scan, p)
    }

    fn rel(&self, p: &str) -> String {
        Path::new(p)
            .strip_prefix(self.root())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Invariants after a run whose report is `r` (I-1, I-2, I-3, I-8; I-4 per operation).
    fn check(&self, r: &AutoPolicyReport, scan: &ScanReport) {
        let tiers: BTreeMap<ItemId, Tier> = scan
            .candidates
            .iter()
            .map(|c| (c.id.clone(), c.tier))
            .collect();
        let mut inputs = InvariantInputs::new();
        let mut entries = Vec::new();
        let mut planned = BTreeMap::new();
        let mut executed = BTreeMap::new();
        for p in &r.policies {
            if let PolicyOutcome::Ran { operation_id, .. } = &p.outcome {
                let op = OperationId::try_from(operation_id.clone()).unwrap();
                inputs = inputs.audit(&self.store, &op).quarantine(&self.store);
            }
            for i in &p.items {
                if matches!(i.outcome.as_str(), "quarantined" | "trashed") {
                    inputs = inputs.allow(self.rel(&i.path));
                    let action = if i.outcome == "quarantined" {
                        Action::Quarantine
                    } else {
                        Action::Trash
                    };
                    entries.push((i.item_id.clone(), Tier::S0, action));
                    planned.insert(i.item_id.clone(), tiers[&i.item_id]);
                    executed.insert(i.item_id.clone(), Tier::S0);
                }
            }
        }
        let report = self
            .fx
            .check_invariants(&inputs.plan(entries).tiers(planned, executed));
        println!("{report}");
        report.assert_ok();
    }

    fn policy_operations(&self) -> Vec<scull_storage::OperationRecord> {
        self.store
            .operations(0)
            .unwrap()
            .into_iter()
            .filter(|o| o.method == ApprovalMethod::AutoPolicy)
            .collect()
    }
}

/// The runner's inputs over the fixture (quarantine inside its temp dir).
fn inputs<'a>(
    data: &'a Path,
    quarantine: &'a QuarantineConfig,
    who: &'a Who,
    roots: &'a [PathBuf],
    auto: &'a AutoConfig,
    now: i64,
) -> Inputs<'a> {
    Inputs {
        auto,
        quarantine,
        who,
        data_dir: data,
        roots,
        rules: &[],
        operations_watermark: None,
        now,
    }
}

fn items_with<'a>(r: &'a AutoPolicyReport, outcome: &str) -> Vec<&'a str> {
    r.policies
        .iter()
        .flat_map(|p| &p.items)
        .filter(|i| i.outcome == outcome)
        .map(|i| i.path.as_str())
        .collect()
}

// ------------------------------------------------------------------------------------------

/// Off by default: with no policy, or a policy without `enabled = true`, nothing is decided,
/// no lock is taken, no state file, plan or operation is written, and the tree is unchanged.
#[test]
fn nothing_runs_when_no_policy_is_enabled() {
    let mut h = harness(&["app", "lib"]);
    let scan = h.scan();
    assert!(scan.candidates.iter().any(|c| c.tier == Tier::S0));
    let mut disabled = policy("p");
    disabled.enabled = false;
    for auto in [AutoConfig::default(), auto_with(vec![disabled])] {
        // `scull check` stops at this gate, before it takes the execution lock.
        assert_eq!(
            Gate::default().check(&auto).map(|r| r.state),
            Some(RunState::Off)
        );
        let p = h.prepare(&auto, &scan, now_unix() + 30 * DAY);
        assert_eq!(p.report().state, RunState::Off);
        let r = h.execute(&auto, p, now_unix() + 30 * DAY);
        assert_eq!(r, AutoPolicyReport::off());
    }
    assert!(!h.data().join(STATE_FILE).exists(), "no state written");
    assert!(h.policy_operations().is_empty());
    assert!(h.store.operations(0).unwrap().is_empty());
    h.check(&AutoPolicyReport::off(), &scan);
}

/// FX-EXEC-15: the first time, a policy only previews (dry run, recorded, nothing touched); it
/// runs one interval later, quarantining (never deleting) the idle S0 build outputs in one
/// audited operation of an auto-policy plan; then it waits for its interval again.
#[test]
fn fx_exec_15_dry_run_first_then_quarantine_through_the_executor() {
    let mut h = harness(&["app", "lib"]);
    let auto = auto_with(vec![policy("old-builds")]);
    let t0 = now_unix();
    let scan = h.scan();
    let p = h.prepare(&auto, &scan, t0);
    let r = h.execute(&auto, p, t0);
    let PolicyOutcome::Previewed {
        items,
        bytes,
        runs_after,
    } = r.policies[0].outcome
    else {
        panic!("{r:?}")
    };
    assert_eq!(items, 2);
    assert!(bytes > 0);
    assert_eq!(runs_after, t0 + 7 * DAY);
    assert_eq!(items_with(&r, "would_quarantine").len(), 2);
    assert!(
        h.policy_operations().is_empty(),
        "a dry run starts no operation"
    );
    h.check(&r, &scan);
    let st = read_state(&h.data()).unwrap();
    assert_eq!(st.policies["old-builds"].preview_items, 2);
    assert!(st.digests[0].message.starts_with("dry run"));

    // A day later: not due yet.
    let p = h.prepare(&auto, &h.scan(), t0 + DAY);
    assert!(matches!(
        p.report().policies[0].outcome,
        PolicyOutcome::NotDue { next_at } if next_at == t0 + 7 * DAY
    ));

    // One interval after the preview: it runs.
    let t1 = t0 + 7 * DAY + 1;
    let scan = h.scan();
    let p = h.prepare(&auto, &scan, t1);
    let r = h.execute(&auto, p, t1);
    let PolicyOutcome::Ran {
        quarantined,
        trashed,
        not_done,
        operation_id,
        restore_command,
        ..
    } = &r.policies[0].outcome
    else {
        panic!("{r:?}")
    };
    assert_eq!((*quarantined, *trashed, *not_done), (2, 0, 0));
    assert_eq!(restore_command, &format!("scull restore {operation_id}"));
    assert!(!h.fx.root().join("app/target").exists());
    assert!(!h.fx.root().join("lib/target").exists());
    // Sources are untouched; the build outputs sit in the quarantine, restorable.
    assert!(h.fx.root().join("app/Cargo.toml").is_file());
    let held = std::fs::read_dir(
        h.fx.quarantine_base()
            .join(scull_cleaner::QUARANTINE_DIR_NAME),
    )
    .unwrap()
    .count();
    assert!(held >= 1, "quarantine folder holds the operation");

    // The audit: one auto-policy operation of an auto-policy plan of S0 quarantines.
    let ops = h.policy_operations();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].operation_id.as_str(), operation_id);
    assert_eq!(ops[0].approver, "tester");
    assert!(ops[0].finished.is_some());
    let plan = h.store.get_plan(&ops[0].plan_id).unwrap().unwrap();
    assert_eq!(plan.origin(), PlanOrigin::Policy);
    assert!(
        plan.items()
            .iter()
            .all(|i| i.tier == Tier::S0 && i.action == Action::Quarantine)
    );
    let audit = h.store.audit_items(&ops[0].operation_id).unwrap();
    assert_eq!(audit.len(), 2);
    assert!(audit.iter().all(|a| a.action == "quarantine"));
    h.check(&r, &scan);

    // The digest is kept; the policy waits for its interval again.
    let st = read_state(&h.data()).unwrap();
    assert_eq!(st.policies["old-builds"].last_run_at, Some(t1));
    let last = st.digests.last().unwrap();
    assert_eq!(last.operation_id.as_deref(), Some(operation_id.as_str()));
    assert!(last.message.contains("scull restore"), "{}", last.message);
    let p = h.prepare(&auto, &h.scan(), t1 + DAY);
    assert!(matches!(
        p.report().policies[0].outcome,
        PolicyOutcome::NotDue { .. }
    ));

    // Editing the policy needs a new dry run.
    let mut edited = auto.clone();
    edited.policy[0].idle = DurationSpec::from_hours(30 * 24);
    let p = h.prepare(&edited, &h.scan(), t1 + 8 * DAY);
    assert!(matches!(
        p.report().policies[0].outcome,
        PolicyOutcome::Previewed { .. }
    ));
}

/// A candidate that changes between the plan and the run is left in place: one becomes
/// protected (S3, `.scull-protect`), one is built into right now (recent activity: no longer
/// S0), one was built 30 days ago (still S0, but no longer idle for the policy's 60 days: the
/// per-item policy re-check). Only the unchanged one is quarantined.
#[test]
fn tier_change_between_plan_and_run_is_skipped() {
    let mut h = harness(&["app", "lib", "old", "web"]);
    let auto = auto_with(vec![policy("p")]);
    let (scan, p) = h.previewed_then_due(&auto, now_unix());
    assert_eq!(p.report().policies[0].items.len(), 4);

    std::fs::write(h.fx.root().join("app/.scull-protect"), "").unwrap();
    std::fs::write(h.fx.root().join("lib/target/debug/fresh.o"), "obj").unwrap();
    std::fs::write(h.fx.root().join("old/target/debug/fresh.o"), "obj").unwrap();
    h.fx.age("old", days_ago(30)).unwrap();
    h.fx.checkpoint().unwrap();

    let r = h.execute(&auto, p, now_unix() + 7 * DAY + 1);
    let PolicyOutcome::Ran {
        quarantined,
        not_done,
        ..
    } = r.policies[0].outcome
    else {
        panic!("{r:?}")
    };
    assert_eq!((quarantined, not_done), (1, 3));
    let reason = |rel: &str| {
        r.policies[0]
            .items
            .iter()
            .find(|i| h.rel(&i.path) == rel)
            .unwrap()
            .outcome
            .clone()
    };
    assert_eq!(reason("app/target"), "now_protected");
    assert!(
        matches!(
            reason("lib/target").as_str(),
            "tier_raised" | "no_longer_idle"
        ),
        "{}",
        reason("lib/target")
    );
    assert_eq!(reason("old/target"), "no_longer_idle");
    assert_eq!(reason("web/target"), "quarantined");
    assert!(h.fx.root().join("old/target/debug").is_dir());
    assert!(h.fx.root().join("app/target/debug").is_dir());
    assert!(h.fx.root().join("lib/target/debug").is_dir());
    assert!(!h.fx.root().join("web/target").exists());
    h.check(&r, &scan);
}

/// A protected project (`.scull-protect`, S3) is never chosen, not even previewed.
#[test]
fn protected_paths_are_never_chosen() {
    let mut h = harness(&["app", "vault"]);
    std::fs::write(h.fx.root().join("vault/.scull-protect"), "").unwrap();
    h.fx.checkpoint().unwrap();
    let auto = auto_with(vec![policy("p")]);
    let scan = h.scan();
    let p = h.prepare(&auto, &scan, now_unix());
    let entry = &p.report().policies[0];
    assert_eq!(entry.items.len(), 1);
    assert_eq!(h.rel(&entry.items[0].path), "app/target");
    assert!(entry.excluded.not_s0 >= 1, "{:?}", entry.excluded);
    let (scan, p) = h.previewed_then_due(&auto, now_unix());
    let r = h.execute(&auto, p, now_unix() + 7 * DAY + 1);
    assert_eq!(items_with(&r, "quarantined").len(), 1);
    assert!(h.fx.root().join("vault/target/debug").is_dir());
    h.check(&r, &scan);
}

/// A build output swapped for a symlink (to the canary) between the plan and the run is left
/// alone: revalidation sees another object; nothing behind the link is touched.
#[test]
fn symlink_swap_between_plan_and_run_is_skipped() {
    let mut h = harness(&["app", "lib"]);
    let auto = auto_with(vec![policy("p")]);
    let (scan, p) = h.previewed_then_due(&auto, now_unix());
    let root = h.fx.root().to_path_buf();
    std::fs::rename(root.join("app/target"), root.join("app/target.real")).unwrap();
    let target = h.fx.canary().symlink_target_dir.clone();
    if h.fx.symlink("app/target", &target).unwrap() != LinkOutcome::Created {
        eprintln!("symlinks not permitted here; swap test skipped");
        return;
    }
    h.fx.checkpoint().unwrap();
    let r = h.execute(&auto, p, now_unix() + 7 * DAY + 1);
    let app = r.policies[0]
        .items
        .iter()
        .find(|i| h.rel(&i.path) == "app/target")
        .unwrap();
    assert!(
        !matches!(app.outcome.as_str(), "quarantined" | "trashed"),
        "{app:?}"
    );
    assert!(
        std::fs::symlink_metadata(root.join("app/target"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(root.join("app/target.real/debug").is_dir());
    assert_eq!(items_with(&r, "quarantined").len(), 1, "lib still runs");
    h.check(&r, &scan);
}

/// Caps: a run takes at most `max_items_per_run` (the rest is deferred and reported); an
/// exhausted daily cap takes nothing; the day's usage is read back from the audit log.
#[test]
fn caps_bound_a_run_and_the_day() {
    let mut h = harness(&["a", "b", "c"]);
    let mut auto = auto_with(vec![policy("p")]);
    auto.max_items_per_run = 2;
    let t0 = now_unix();
    let (scan, p) = h.previewed_then_due(&auto, t0);
    assert_eq!(p.report().policies[0].deferred, 1);
    assert_eq!(
        p.report().policies[0].outcome,
        PolicyOutcome::Planned { items: 2 }
    );
    let r = h.execute(&auto, p, t0 + 7 * DAY + 1);
    assert_eq!(items_with(&r, "quarantined").len(), 2);
    assert_eq!(r.policies[0].deferred, 1);
    h.check(&r, &scan);
    let used = scull_proactive::auto::day_usage(&h.store, t0 + 7 * DAY + 2).unwrap();
    assert_eq!(used.items, 2);
    assert!(used.bytes > 0);
    // 24 hours later the run no longer counts.
    let later = scull_proactive::auto::day_usage(&h.store, t0 + 9 * DAY).unwrap();
    assert_eq!(later.items, 0);

    // A zero daily cap: nothing is taken, everything is deferred, nothing is touched.
    let mut h = harness(&["a", "b"]);
    let mut auto = auto_with(vec![policy("p")]);
    auto.max_gib_per_day = 0;
    let (scan, p) = h.previewed_then_due(&auto, t0);
    let r = h.execute(&auto, p, t0 + 7 * DAY + 1);
    assert_eq!(r.policies[0].outcome, PolicyOutcome::NothingToDo);
    assert_eq!(r.policies[0].deferred, 2);
    assert!(h.policy_operations().is_empty());
    h.check(&r, &scan);
    // Deferred by the caps: retried at the next check (the interval did not restart).
    let st = read_state(&h.data()).unwrap();
    assert_eq!(st.policies["p"].last_run_at, None);

    // A cap above its ceiling refuses every policy.
    let mut bad = auto_with(vec![policy("p")]);
    bad.max_gib_per_run = 100_000;
    let p = h.prepare(&bad, &h.scan(), t0);
    assert_eq!(p.report().state, RunState::Refused);
}

/// Two policies in one run share the caps and never take the same item twice.
#[test]
fn policies_share_the_caps() {
    let mut h = harness(&["a", "b", "c"]);
    let mut auto = auto_with(vec![policy("one"), policy("two")]);
    auto.max_items_per_run = 2;
    let (scan, p) = h.previewed_then_due(&auto, now_unix());
    let r = h.execute(&auto, p, now_unix() + 7 * DAY + 1);
    assert_eq!(items_with(&r, "quarantined").len(), 2);
    assert_eq!(
        h.policy_operations().len(),
        1,
        "policy two had nothing left"
    );
    h.check(&r, &scan);
}

/// Another scull operation holds the execution lock: nothing is decided (the state, the daily
/// usage and what is due are read only under the lock, review L6), nothing is touched, and the
/// policy is retried at the next check.
#[test]
fn a_held_lock_refuses_before_anything_is_decided() {
    let mut h = harness(&["app"]);
    let auto = auto_with(vec![policy("p")]);
    let t0 = now_unix();
    let (scan, _) = h.run(&auto, t0);
    let before = read_state(&h.data()).unwrap();
    let held = Executor::acquire(&h.lock_options()).unwrap();
    let r = h
        .try_prepare(&auto, &h.scan(), t0 + 7 * DAY + 1)
        .unwrap_err();
    drop(held);
    assert_eq!(r.state, RunState::Refused, "{r:?}");
    assert!(r.reason.as_deref().unwrap().contains("retried"), "{r:?}");
    assert!(h.fx.root().join("app/target/debug").is_dir());
    assert!(h.policy_operations().is_empty());
    assert_eq!(
        read_state(&h.data()).unwrap(),
        before,
        "nothing decided or saved"
    );
    h.check(&AutoPolicyReport::off(), &scan);
}

/// Review M1: the dry run covers the config file's scan roots. A root added after the preview
/// means a new dry run, so items under it are never cleaned unseen.
#[test]
fn a_new_scan_root_needs_a_new_dry_run() {
    let mut h = harness(&["app"]);
    let auto = auto_with(vec![policy("p")]);
    let t0 = now_unix();
    let (scan, r) = h.run(&auto, t0);
    assert!(matches!(
        r.policies[0].outcome,
        PolicyOutcome::Previewed { .. }
    ));
    let other = h.fx.base().join("other-root");
    std::fs::create_dir_all(&other).unwrap();
    h.roots.push(canon(&other));
    let p = h.prepare(&auto, &h.scan(), t0 + 7 * DAY + 1);
    assert!(
        matches!(
            p.report().policies[0].outcome,
            PolicyOutcome::Previewed { .. }
        ),
        "{:?}",
        p.report()
    );
    let r = h.execute(&auto, p, t0 + 7 * DAY + 1);
    assert!(h.fx.root().join("app/target/debug").is_dir());
    h.check(&r, &scan);
}

/// Review M2: where quarantine is impossible (here the configured base is a file), the item is
/// left in place and reported blocked; it never goes to the Trash, which the OS may empty on
/// its own. The Executor ignores a Trash fallback for auto-policy plans even when one is set.
#[test]
fn quarantine_impossible_is_blocked_never_trashed() {
    let mut h = harness(&["app"]);
    let not_a_dir = h.fx.base().join("q-is-a-file");
    std::fs::write(&not_a_dir, "x").unwrap();
    h.quarantine.base = Some(not_a_dir);
    let auto = auto_with(vec![policy("p")]);
    let (scan, p) = h.previewed_then_due(&auto, now_unix());
    let r = h.execute(&auto, p, now_unix() + 7 * DAY + 1);
    let PolicyOutcome::Ran {
        quarantined,
        trashed,
        not_done,
        ..
    } = r.policies[0].outcome
    else {
        panic!("{r:?}")
    };
    assert_eq!((quarantined, trashed, not_done), (0, 0, 1));
    assert_eq!(r.policies[0].items[0].outcome, "quarantine_impossible");
    assert!(h.fx.root().join("app/target/debug").is_dir());
    assert!(!h.fx.base().join("Trash").exists());
    let op = &h.policy_operations()[0];
    let audit = h.store.audit_items(&op.operation_id).unwrap();
    assert_eq!(audit[0].reason.as_deref(), Some("quarantine_impossible"));
    h.check(&r, &scan);

    // The Executor's own guard: a Trash set on a policy operation is ignored.
    let later = now_unix() + 8 * DAY;
    let c = scan.candidates.iter().find(|c| c.tier == Tier::S0).unwrap();
    let plan = Plan::new(
        PlanParams {
            id: PlanId::from_suffix("01POLICYTRASH").unwrap(),
            scan_id: scan.scan_id.clone(),
            created_unix: later,
            ttl_secs: None,
            machine_id: "machine-test".into(),
            user: "tester".into(),
            origin: PlanOrigin::Policy,
            operations_watermark: None,
        },
        vec![PlanItem {
            action: Action::Quarantine,
            ..PlanItem::from_candidate(c).unwrap()
        }],
    )
    .unwrap();
    h.store.insert_plan(&plan).unwrap();
    let ex = Executor::acquire(&h.lock_options()).unwrap();
    let roots = [h.root()];
    let settings = scull_cleaner::QuarantineSettings::from_config(&h.quarantine);
    let trash = scull_cleaner::trash::TrashLocations {
        home: Some(h.fx.base().join("Trash")),
        topdir: Some(h.fx.base().to_path_buf()),
    };
    let mut op = ex
        .begin_operation(
            &mut h.store,
            &plan,
            &OperationStart {
                operation_id: OperationId::from_suffix("01POLICYTRASH").unwrap(),
                plan_id: plan.id().clone(),
                content_hash: plan.content_hash().clone(),
                approver: "tester".into(),
                method: ApprovalMethod::AutoPolicy,
                started_at: later,
            },
        )
        .unwrap()
        .with_quarantine(settings)
        .with_trash(trash);
    let report = ex
        .execute_plan(
            &plan,
            &PlanContext {
                now_unix: later,
                machine_id: "machine-test",
                user: "tester",
            },
            &Revalidation::new(&h.engine, &roots),
            Some(&mut op),
            &CancelToken::new(),
        )
        .unwrap();
    op.finish(scull_storage::OperationEnd::Normal, now_unix())
        .unwrap();
    assert!(
        matches!(
            report.items[0].outcome,
            scull_cleaner::ItemOutcome::Blocked {
                code: "quarantine_impossible",
                ..
            }
        ),
        "{:?}",
        report.items[0].outcome
    );
    assert!(h.fx.root().join("app/target/debug").is_dir());
    assert!(!h.fx.base().join("Trash").exists());
    h.check(&AutoPolicyReport::off(), &scan);
}

/// Review L4: an item the user restored from quarantine is not taken again within the
/// policy's idle window.
#[test]
fn restored_items_are_not_taken_again() {
    let mut h = harness(&["app", "lib"]);
    let auto = auto_with(vec![policy("p")]);
    let t0 = now_unix();
    let (scan, p) = h.previewed_then_due(&auto, t0);
    let t1 = t0 + 7 * DAY + 1;
    let r = h.execute(&auto, p, t1);
    assert_eq!(items_with(&r, "quarantined").len(), 2);
    h.check(&r, &scan);
    // The user restores both.
    let ex = Executor::acquire(&h.lock_options()).unwrap();
    for e in h.store.quarantine_held().unwrap() {
        ex.restore(&mut h.store, e.entry_id, now_unix()).unwrap();
    }
    drop(ex);
    assert!(h.fx.root().join("app/target/debug").is_dir());
    h.fx.checkpoint().unwrap();
    // One interval later: due again, but nothing is taken.
    let (scan, r) = h.run(&auto, t1 + 7 * DAY + 1);
    assert_eq!(r.policies[0].outcome, PolicyOutcome::NothingToDo, "{r:?}");
    assert_eq!(r.policies[0].excluded.recently_restored, 2);
    assert!(h.fx.root().join("lib/target/debug").is_dir());
    h.check(&r, &scan);
}

/// Review L5: the project's own files count as activity. A source edit before the decision
/// keeps the item out; one between the decision and the move skips it at the re-check.
#[test]
fn an_active_project_keeps_its_build_output() {
    let mut h = harness(&["app", "lib", "web"]);
    let auto = auto_with(vec![policy("p")]);
    let t0 = now_unix();
    let (_, r) = h.run(&auto, t0);
    assert!(matches!(
        r.policies[0].outcome,
        PolicyOutcome::Previewed { items: 3, .. }
    ));
    std::fs::write(h.fx.root().join("app/src/new.rs"), "pub fn f() {}\n").unwrap();
    h.fx.checkpoint().unwrap();
    let t1 = t0 + 7 * DAY + 1;
    let scan = h.scan();
    let p = h.prepare(&auto, &scan, t1);
    assert_eq!(p.report().policies[0].excluded.project_active, 1);
    assert_eq!(p.report().policies[0].items.len(), 2);
    std::fs::write(
        h.fx.root().join("lib/Cargo.toml"),
        "[package]\nname = \"lib\"\n",
    )
    .unwrap();
    h.fx.checkpoint().unwrap();
    let r = h.execute(&auto, p, t1);
    let reason = |rel: &str| {
        r.policies[0]
            .items
            .iter()
            .find(|i| h.rel(&i.path) == rel)
            .map(|i| i.outcome.clone())
    };
    assert_eq!(reason("lib/target").as_deref(), Some("project_active"));
    assert_eq!(reason("web/target").as_deref(), Some("quarantined"));
    assert!(h.fx.root().join("app/target/debug").is_dir());
    assert!(h.fx.root().join("lib/target/debug").is_dir());
    h.check(&r, &scan);
}

/// The Executor's own checks: an auto-policy plan starts only as an auto-policy operation and
/// runs only inside it; no other plan may use the click-less method.
#[test]
fn the_executor_keeps_policy_plans_on_their_path() {
    let mut h = harness(&["app"]);
    let scan = h.scan();
    let c = scan.candidates.iter().find(|c| c.tier == Tier::S0).unwrap();
    let item = PlanItem {
        action: Action::Quarantine,
        ..PlanItem::from_candidate(c).unwrap()
    };
    let now = now_unix();
    let make = |origin, id: &str, item: PlanItem| {
        Plan::new(
            PlanParams {
                id: PlanId::from_suffix(id).unwrap(),
                scan_id: scan.scan_id.clone(),
                created_unix: now,
                ttl_secs: None,
                machine_id: "machine-test".into(),
                user: "tester".into(),
                origin,
                operations_watermark: None,
            },
            vec![item],
        )
        .unwrap()
    };
    let policy_plan = make(PlanOrigin::Policy, "01POLICY", item.clone());
    let cli_plan = make(PlanOrigin::Cli, "01CLI", item);
    h.store.insert_plan(&policy_plan).unwrap();
    h.store.insert_plan(&cli_plan).unwrap();
    let ex = Executor::acquire(&LockOptions::at(
        h.fx.base().join("state").join("execution.lock"),
    ))
    .unwrap();
    let start = |plan: &Plan, method: ApprovalMethod| OperationStart {
        operation_id: OperationId::from_suffix(&format!("01{}", method.as_str().replace('_', "")))
            .unwrap(),
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        approver: "tester".into(),
        method,
        started_at: now,
    };
    for (plan, method) in [
        (&policy_plan, ApprovalMethod::CliFlag),
        (&policy_plan, ApprovalMethod::CliInteractive),
        (&cli_plan, ApprovalMethod::AutoPolicy),
    ] {
        let err = ex
            .begin_operation(&mut h.store, plan, &start(plan, method))
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::SafetyBlocked, "{}", err.message);
    }
    let roots = [h.root()];
    let err = ex
        .execute_plan(
            &policy_plan,
            &PlanContext {
                now_unix: now,
                machine_id: "machine-test",
                user: "tester",
            },
            &Revalidation::new(&h.engine, &roots),
            None,
            &CancelToken::new(),
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::SafetyBlocked, "{}", err.message);
    assert!(h.store.operations(0).unwrap().is_empty());
    assert!(h.fx.root().join("app/target/debug").is_dir());
    h.check(&AutoPolicyReport::off(), &scan);
}

/// The state file is scull's own: one that others can read or write, or that is invalid,
/// stops every policy (fail closed); a missing one only means "preview again".
#[test]
fn an_untrusted_or_broken_state_file_refuses() {
    let mut h = harness(&["app"]);
    let auto = auto_with(vec![policy("p")]);
    let (scan, _) = h.previewed_then_due(&auto, now_unix());
    let path = h.data().join(STATE_FILE);
    std::fs::write(&path, b"{ not json").unwrap();
    let p = h.prepare(&auto, &scan, now_unix() + 7 * DAY + 1);
    assert_eq!(p.report().state, RunState::Refused);
    assert!(p.report().reason.as_deref().unwrap().contains("invalid"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&path, b"{\"version\":1,\"policies\":{},\"digests\":[]}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let p = h.prepare(&auto, &scan, now_unix());
        assert_eq!(p.report().state, RunState::Refused);
        assert!(
            p.report()
                .reason
                .as_deref()
                .unwrap()
                .contains("not private"),
            "{:?}",
            p.report().reason
        );
    }
    std::fs::remove_file(&path).unwrap();
    let p = h.prepare(&auto, &scan, now_unix());
    assert!(matches!(
        p.report().policies[0].outcome,
        PolicyOutcome::Previewed { .. }
    ));
    h.check(&AutoPolicyReport::off(), &scan);
}
