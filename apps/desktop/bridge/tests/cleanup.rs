//! The desktop's cleanups through the core Executor (P3-EXEC-01), in `scull-fixtures` temp
//! sandboxes only: the scan root, quarantine, Trash, execution lock and database are all inside
//! the fixture's temp dir, and the fixture's canary and invariants prove nothing outside the
//! planned items changed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_cleaner::trash::TrashLocations;
use scull_core::config::QUARANTINE_BASE_ENV;
use scull_core::{ErrorKind, Tier};
use scull_desktop_bridge::cleanup::{CleanupDesk, RunItemOutcome, Who};
use scull_desktop_bridge::views::ScanParams;
use scull_desktop_bridge::{OpId, Session, Setup, now_unix};
use scull_fixtures::{Fixture, NodeLock, days_ago};
use std::path::PathBuf;
use std::sync::Arc;

struct Env {
    fx: Fixture,
    session: Session,
    desk: CleanupDesk,
    data: PathBuf,
}

fn who() -> Who {
    Who {
        machine_id: "machine-sandbox".into(),
        user: "tester".into(),
    }
}

/// A Cargo project with build output, an npm project with `node_modules`, and a protected
/// project (S3), all idle; scull's data, quarantine, Trash and lock inside the fixture.
fn env() -> Env {
    env_with(false)
}

/// [`env`], or with every candidate known idle (`idle`: a fixed in-use answer, as the other
/// crates' tests use) so idle build output can be S0.
fn env_with(idle: bool) -> Env {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 64 * 1024)
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad", "is-odd"])
        .unwrap()
        .cargo_project("vault", "vault")
        .unwrap()
        .cargo_target("vault", "vault", 4096)
        .unwrap()
        .file("vault/.scull-protect", "")
        .unwrap();
    for d in ["app", "web", "vault"] {
        fx.age(d, days_ago(5)).unwrap();
    }
    fx.checkpoint().unwrap();
    let base = fx.base().to_path_buf();
    let data = base.join("scull-data");
    let qbase = fx.quarantine_base().to_string_lossy().into_owned();
    let session = Session::new(Setup {
        home: Some(base.join("home")),
        config_file: None,
        data_dir: Some(data.clone()),
        env: Arc::new(move |k| (k == QUARANTINE_BASE_ENV).then(|| qbase.clone())),
        record_history: true,
        git: false,
        detect_synced_roots: false,
        allow_network_roots: true,
        in_use: false,
    });
    let session = if idle {
        session.with_in_use_source(scull_engine::InUseSource::Probe(Arc::new(
            scull_engine::FixedInUse(scull_safety::Tri::No),
        )))
    } else {
        session
    };
    let desk = CleanupDesk::with(
        who(),
        TrashLocations {
            home: Some(base.join("Trash")),
            topdir: Some(base.clone()),
        },
        base.join("execution.lock"),
    );
    Env {
        fx,
        session,
        desk,
        data,
    }
}

impl Env {
    fn scan(&self) {
        let params = ScanParams {
            roots: vec![self.fx.root().to_string_lossy().into_owned()],
            ..ScanParams::default()
        };
        self.session
            .scan(&OpId::parse("op-test").unwrap(), &params, |_| {})
            .unwrap();
    }

    fn id_of(&self, suffix: &str) -> String {
        self.session
            .last_scan()
            .unwrap()
            .candidates
            .iter()
            .find(|c| ends(c.path.as_str(), suffix))
            .unwrap_or_else(|| panic!("no candidate {suffix}"))
            .id
            .as_str()
            .to_owned()
    }

    fn operations(&self) -> usize {
        self.session.history(now_unix()).operations.len()
    }
}

/// Whether `path` ends with `suffix` (written with `/`), whatever the OS separator.
fn ends(path: &str, suffix: &str) -> bool {
    path.replace('\\', "/").ends_with(suffix)
}

/// Set by CI's standard-user run on Windows (`scripts/ci/run-as-standard-user.ps1`): an
/// elevated process then fails instead of skipping.
const REQUIRE_NOT_ELEVATED_ENV: &str = "SCULL_TEST_REQUIRE_NOT_ELEVATED";

fn elevated_here() -> bool {
    scull_platform::is_elevated().blocks_destructive()
}

/// Elevated, the desktop refuses to clean (SAFE-14): `test` stops here. GitHub's Windows
/// runners are elevated; CI runs this binary again as a standard user with
/// [`REQUIRE_NOT_ELEVATED_ENV`] set, where a skip is a failure and a `RUN` marker is printed.
fn skip_if_elevated(test: &str) -> bool {
    let required = std::env::var_os(REQUIRE_NOT_ELEVATED_ENV).is_some_and(|v| !v.is_empty());
    if elevated_here() {
        assert!(
            !required,
            "{test}: {REQUIRE_NOT_ELEVATED_ENV} is set but this process runs elevated"
        );
        eprintln!("SKIP {test}: running elevated, so the desktop refuses to clean (SAFE-14)");
        return true;
    }
    if required {
        eprintln!("RUN {test}: not elevated (standard user)");
    }
    false
}

#[test]
fn cleanup_review_runs_exactly_the_chosen_items_through_the_executor() {
    if skip_if_elevated("cleanup_review_runs_exactly_the_chosen_items_through_the_executor") {
        return;
    }
    let e = env();
    let now = now_unix();
    // No scan yet: nothing to plan from.
    assert_eq!(
        e.desk.prepare(&e.session, None, now).unwrap_err().kind,
        ErrorKind::Usage
    );
    e.scan();
    let plan = e.desk.prepare(&e.session, None, now).unwrap();
    let paths: Vec<&str> = plan.items.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(plan.items.len(), 2, "{paths:?}");
    assert!(plan.items.iter().all(|c| c.tier != Tier::S3));
    assert!(
        plan.left_out
            .iter()
            .any(|l| ends(l.path.as_str(), "vault/target") && l.tier == Tier::S3)
    );
    assert_eq!(
        plan.expires_at - plan.created_at,
        3600,
        "D-25: desktop plans live 1 hour"
    );
    let target = e.id_of("app/target");
    let modules = e.id_of("web/node_modules");

    // Refusals change nothing and record no operation.
    let wrong_hash = e.desk.execute(
        &e.session,
        &plan.plan_id,
        &"0".repeat(64),
        std::slice::from_ref(&target),
        None,
        now,
    );
    assert_eq!(wrong_hash.unwrap_err().kind, ErrorKind::PlanTampered);
    let foreign = e.desk.execute(
        &e.session,
        &plan.plan_id,
        &plan.content_hash,
        &["f".repeat(32)],
        None,
        now,
    );
    assert_eq!(foreign.unwrap_err().kind, ErrorKind::PlanTampered);
    let none = e.desk.execute(
        &e.session,
        &plan.plan_id,
        &plan.content_hash,
        &[],
        None,
        now,
    );
    assert_eq!(none.unwrap_err().kind, ErrorKind::Usage);
    assert_eq!(e.operations(), 0);

    // The chosen item only, through the Executor, recorded as the human's desktop choice.
    let run = e
        .desk
        .execute(
            &e.session,
            &plan.plan_id,
            &plan.content_hash,
            std::slice::from_ref(&target),
            None,
            now,
        )
        .unwrap();
    assert_eq!(run.items.len(), 1);
    assert_eq!(run.items[0].item_id, target);
    assert_eq!(
        run.items[0].outcome,
        RunItemOutcome::Quarantined,
        "D-28: S2 is quarantined"
    );
    assert!(run.items[0].moved_bytes > 0);
    assert!(!e.fx.root().join("app/target").exists());
    assert!(
        e.fx.root().join("web/node_modules").exists(),
        "not chosen: untouched"
    );
    assert!(
        e.fx.root().join("vault/target").exists(),
        "S3: never planned"
    );
    let history = e.session.history(now_unix());
    let op = history
        .operations
        .iter()
        .find(|o| o.operation_id == run.operation_id)
        .unwrap();
    assert_eq!(op.method, "desktop");
    assert_eq!(op.approver, "tester");
    let _ = modules;

    // Restore: back where it was (never overwriting); a second restore moves nothing.
    let r = e
        .desk
        .restore(&e.session, &run.operation_id, &target, now_unix())
        .unwrap();
    assert!(ends(&r.restored_to, "app/target"), "{}", r.restored_to);
    assert!(!r.sibling && !r.already);
    assert!(e.fx.root().join("app/target").is_dir());
    let again = e
        .desk
        .restore(&e.session, &run.operation_id, &target, now_unix())
        .unwrap();
    assert!(again.already);

    e.fx.check_invariants(
        &scull_fixtures::InvariantInputs::new()
            .plan(plan.items.iter())
            .allow("app/target"),
    )
    .assert_ok();
}

#[test]
fn a_plan_from_a_scan_older_than_an_operation_on_its_items_is_refused() {
    if skip_if_elevated("a_plan_from_a_scan_older_than_an_operation_on_its_items_is_refused") {
        return;
    }
    let e = env();
    let now = now_unix();
    e.scan();
    let target = e.id_of("app/target");
    let first = e.desk.prepare(&e.session, None, now).unwrap();
    e.desk
        .execute(
            &e.session,
            &first.plan_id,
            &first.content_hash,
            std::slice::from_ref(&target),
            None,
            now,
        )
        .unwrap();
    // Another plan from the same (now stale) scan names the item the operation touched.
    let second = e.desk.prepare(&e.session, None, now).unwrap();
    let err = e
        .desk
        .execute(
            &e.session,
            &second.plan_id,
            &second.content_hash,
            &[target.clone(), e.id_of("web/node_modules")],
            None,
            now,
        )
        .unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::PlanInvalidated,
        "SAFE-09: {}",
        err.message
    );
    assert!(
        e.fx.root().join("web/node_modules").exists(),
        "nothing of the stale plan ran"
    );
    e.fx.check_invariants(
        &scull_fixtures::InvariantInputs::new()
            .plan(first.items.iter())
            .allow("app/target"),
    )
    .assert_ok();
}

#[test]
fn elevated_runs_and_plans_made_elsewhere_are_refused() {
    let e = env();
    let now = now_unix();
    e.scan();
    let plan = e.desk.prepare(&e.session, None, now).unwrap();
    let all: Vec<String> = plan
        .items
        .iter()
        .map(|c| c.id.as_str().to_owned())
        .collect();
    // SAFE-14: the desktop has no --allow-root.
    let elevated = CleanupDesk::with(
        who(),
        TrashLocations {
            home: None,
            topdir: None,
        },
        e.fx.base().join("execution.lock"),
    )
    .assume_elevated();
    let err = elevated
        .execute(
            &e.session,
            &plan.plan_id,
            &plan.content_hash,
            &all,
            None,
            now,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::SafetyBlocked);
    assert!(elevated.restore(&e.session, "op_x", &all[0], now).is_err());
    assert!(elevated.quick_clean(&e.session, &all, now).is_err());
    if elevated_here() {
        // The real desk refuses everything elevated (SAFE-14); the rest needs a standard user.
        e.fx.check_invariants(&scull_fixtures::InvariantInputs::new())
            .assert_ok();
        return;
    }
    // A CLI plan can't be executed from the desktop.
    let scan = e.session.last_scan().unwrap();
    let c = scan
        .candidates
        .iter()
        .find(|c| ends(c.path.as_str(), "app/target"))
        .unwrap();
    let cli = scull_core::Plan::new(
        scull_core::PlanParams {
            id: scull_core::PlanId::from_suffix("01JCLIPLAN").unwrap(),
            scan_id: scan.scan_id.clone(),
            created_unix: now,
            ttl_secs: None,
            machine_id: who().machine_id,
            user: who().user,
            origin: scull_core::PlanOrigin::Cli,
            operations_watermark: Some(0),
        },
        vec![scull_core::PlanItem::from_candidate(c).unwrap()],
    )
    .unwrap();
    scull_engine::open_history(&e.data)
        .unwrap()
        .insert_plan(&cli)
        .unwrap();
    let err = e
        .desk
        .execute(
            &e.session,
            cli.id().as_str(),
            cli.content_hash().as_str(),
            &[c.id.as_str().to_owned()],
            None,
            now,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::PlanForeign);
    assert_eq!(e.operations(), 0, "nothing ran");
    e.fx.check_invariants(&scull_fixtures::InvariantInputs::new())
        .assert_ok();
}

#[test]
fn quick_clean_with_no_chip_ticked_runs_nothing() {
    if skip_if_elevated("quick_clean_with_no_chip_ticked_runs_nothing") {
        return;
    }
    let e = env();
    let r = e.desk.quick_clean(&e.session, &[], now_unix()).unwrap();
    assert!(r.operation_id.is_none());
    assert!(r.tools.is_empty());
    assert_eq!(e.operations(), 0);
    e.fx.check_invariants(&scull_fixtures::InvariantInputs::new())
        .assert_ok();
}

#[test]
fn an_s0_item_is_deleted_and_the_rest_left_alone() {
    if skip_if_elevated("an_s0_item_is_deleted_and_the_rest_left_alone") {
        return;
    }
    let e = env_with(true);
    e.scan();
    let scan = e.session.last_scan().unwrap();
    let Some(s0) = scan.candidates.iter().find(|c| c.tier == Tier::S0) else {
        let tiers: Vec<_> = scan
            .candidates
            .iter()
            .map(|c| (c.path.as_str(), c.tier))
            .collect();
        let why: Vec<_> = scan
            .candidates
            .iter()
            .map(|c| (c.path.as_str(), &c.reasons))
            .collect();
        panic!("no S0 candidate: {tiers:?} {why:?}");
    };
    let id = s0.id.as_str().to_owned();
    let rel = s0
        .path
        .as_str()
        .replace('\\', "/")
        .split("/root/")
        .nth(1)
        .unwrap()
        .to_owned();
    let plan = e
        .desk
        .prepare(&e.session, Some(std::slice::from_ref(&id)), now_unix())
        .unwrap();
    assert_eq!(plan.items.len(), 1);
    let run = e
        .desk
        .execute(
            &e.session,
            &plan.plan_id,
            &plan.content_hash,
            &[id],
            None,
            now_unix(),
        )
        .unwrap();
    assert_eq!(
        run.items[0].outcome,
        RunItemOutcome::Deleted,
        "D-28: S0 is deleted"
    );
    assert!(run.items[0].freed_bytes > 0);
    assert!(!e.fx.root().join(&rel).exists());
    e.fx.check_invariants(
        &scull_fixtures::InvariantInputs::new()
            .plan(plan.items.iter())
            .allow(&rel),
    )
    .assert_ok();
}

#[test]
fn a_cancelled_scan_is_never_planned_from() {
    let e = env();
    // The cancel overtakes the scan's start, so the scan stops at once: a partial report.
    let op = OpId::parse("op-cancelled").unwrap();
    e.session.cancel(&op);
    let params = ScanParams {
        roots: vec![e.fx.root().to_string_lossy().into_owned()],
        ..ScanParams::default()
    };
    let report = e.session.scan(&op, &params, |_| {}).unwrap();
    assert!(report.cancelled);
    let refused = e.desk.prepare(&e.session, None, now_unix()).unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Usage, "{}", refused.message);
    assert!(refused.message.contains("cancelled"), "{}", refused.message);
    // A complete scan replaces it and can be planned from.
    e.scan();
    assert!(!e.session.last_scan().unwrap().cancelled);
    e.desk.prepare(&e.session, None, now_unix()).unwrap();
    e.fx.check_invariants(&scull_fixtures::InvariantInputs::new())
        .assert_ok();
}

#[test]
fn a_plan_runs_only_under_the_basis_of_its_own_scan() {
    if skip_if_elevated("a_plan_runs_only_under_the_basis_of_its_own_scan") {
        return;
    }
    let e = env();
    let now = now_unix();
    e.scan();
    let target = e.id_of("app/target");
    let plan = e.desk.prepare(&e.session, None, now).unwrap();
    // The app scans again: the reviewed plan's scan is no longer the one it would revalidate
    // under, so it is refused, whole or narrowed, and nothing changes.
    e.scan();
    for items in [
        plan.items
            .iter()
            .map(|c| c.id.as_str().to_owned())
            .collect::<Vec<_>>(),
        vec![target.clone()],
    ] {
        let refused = e
            .desk
            .execute(
                &e.session,
                &plan.plan_id,
                &plan.content_hash,
                &items,
                None,
                now,
            )
            .unwrap_err();
        assert_eq!(
            refused.kind,
            ErrorKind::PlanInvalidated,
            "{}",
            refused.message
        );
    }
    assert_eq!(e.operations(), 0);
    assert!(e.fx.root().join("app/target").exists());
    // A plan from the new scan runs.
    let fresh = e.desk.prepare(&e.session, None, now).unwrap();
    e.desk
        .execute(
            &e.session,
            &fresh.plan_id,
            &fresh.content_hash,
            std::slice::from_ref(&target),
            None,
            now,
        )
        .unwrap();
    e.fx.check_invariants(
        &scull_fixtures::InvariantInputs::new()
            .plan(fresh.items.iter())
            .allow("app/target"),
    )
    .assert_ok();
}

#[test]
fn a_narrowed_plan_keeps_the_reviewed_plans_lifetime() {
    if skip_if_elevated("a_narrowed_plan_keeps_the_reviewed_plans_lifetime") {
        return;
    }
    let e = env();
    e.scan();
    let target = e.id_of("app/target");
    // Reviewed 50 minutes ago: 10 minutes of its hour are left.
    let now = now_unix();
    let reviewed_at = now - 3000;
    let plan = e.desk.prepare(&e.session, None, reviewed_at).unwrap();
    assert_eq!(plan.expires_at, reviewed_at + 3600);
    let run = e
        .desk
        .execute(
            &e.session,
            &plan.plan_id,
            &plan.content_hash,
            std::slice::from_ref(&target),
            None,
            now,
        )
        .unwrap();
    assert_ne!(run.plan_id, plan.plan_id, "a subset is its own plan");
    let store = scull_storage::Store::open_read_only(
        &scull_engine::history_db_path(&e.data),
        &scull_storage::StoreOptions::default(),
    )
    .unwrap();
    let narrowed = store
        .get_plan(&scull_core::PlanId::try_from(run.plan_id.clone()).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(narrowed.created_unix(), plan.created_at);
    assert_eq!(
        narrowed.expires_unix(),
        plan.expires_at,
        "no fresh hour for a narrowed plan"
    );
    // Past the reviewed plan's hour, nothing of it runs.
    let modules = e.id_of("web/node_modules");
    let late = e.desk.execute(
        &e.session,
        &plan.plan_id,
        &plan.content_hash,
        std::slice::from_ref(&modules),
        None,
        plan.expires_at + 1,
    );
    assert!(late.is_err());
    assert!(e.fx.root().join("web/node_modules").exists());
    e.fx.check_invariants(
        &scull_fixtures::InvariantInputs::new()
            .plan(plan.items.iter())
            .allow("app/target"),
    )
    .assert_ok();
}
