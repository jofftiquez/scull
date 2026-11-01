//! Shared helpers for quarantine and restore fixtures: a fixture with S0/S1/S2 items, a store
//! beside the root, operations that quarantine into the fixture's own temp dir.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use super::planning::*;
use scull_cleaner::{
    ExecutionReport, ItemOutcome, Operation, QUARANTINE_DIR_NAME, QuarantineSettings, Quarantined,
};
use scull_core::{OperationId, Plan};
use scull_engine::{CancelToken, PlatformOwnerOnly, now_unix};
use scull_fixtures::{Fixture, NodeLock};
use scull_storage::{ApprovalMethod, OperationEnd, OperationStart, Store};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

/// * `web/node_modules`: npm project without a lockfile → S2, quarantine;
/// * `app/target`: Cargo build output touched within the activity window (FX-CLS-09 bumps it
///   one tier) → S1, quarantine;
/// * `lib/target`: idle Cargo build output → S0, delete.
pub fn fixture() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    // "Recent" relative to the harness clock (three days ahead).
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

pub fn store(fx: &Fixture) -> Store {
    Store::open(&fx.base().join("data").join("scull.db"), &PlatformOwnerOnly).unwrap()
}

pub fn start(plan: &Plan, op: &str) -> OperationStart {
    OperationStart {
        operation_id: OperationId::from_suffix(op).unwrap(),
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        approver: "tester".into(),
        method: ApprovalMethod::CliInteractive,
        started_at: now_unix(),
    }
}

/// The quarantine base used by these tests: the fixture's temp dir, beside the root
/// ([`Fixture::quarantine_base`], P2-EXEC-13). Never the automatic location.
pub fn settings(fx: &Fixture) -> QuarantineSettings {
    QuarantineSettings {
        base: Some(fx.quarantine_base().to_path_buf()),
        ..QuarantineSettings::default()
    }
}

/// Plans, begins an operation, executes and finishes it.
pub fn run(
    fx: &Fixture,
    h: &Harness,
    s: &mut Store,
    rels: &[&str],
    op: &str,
) -> (Plan, ExecutionReport) {
    let scan = h.scan(fx);
    let plan = plan_named(fx, &scan, rels, op);
    s.insert_plan(&plan).unwrap();
    let exec = locked(fx);
    let mut operation: Operation<'_> = exec
        .begin_operation(s, &plan, &start(&plan, op))
        .unwrap()
        .with_quarantine(settings(fx));
    let r = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    print_report(fx, &r);
    (plan, r)
}

pub fn quarantined<'a>(fx: &Fixture, r: &'a ExecutionReport, rel_path: &str) -> &'a Quarantined {
    let item = r
        .items
        .iter()
        .find(|i| rel(fx, &i.path.to_path_buf()) == rel_path)
        .unwrap();
    match &item.outcome {
        ItemOutcome::Quarantined(q) => q,
        other => panic!("{rel_path} was not quarantined: {other:?}"),
    }
}

pub fn op_dir(fx: &Fixture, op: &str) -> PathBuf {
    fx.base()
        .join(QUARANTINE_DIR_NAME)
        .join(OperationId::from_suffix(op).unwrap().as_str())
}

#[cfg(unix)]
pub fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}
