//! End-to-end FX-RACE fixture (docs/FIXTURES.md §3.4) through an audited operation, so the
//! race's outcome is also checked against the Executor's own audit (I-4) besides the canary
//! (I-2), the root (I-3), the executed plan (I-1) and the tiers (I-8). The walker-level and
//! revalidation-level races (FX-RACE-01…07) are in `crates/scull-cleaner/tests/`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod exec;

use exec::engine::{harness, inputs, plan, run, store, three_tiers};
use scull_cleaner::{ItemOutcome, Skip};
use scull_core::OperationId;
use scull_fixtures::{InvariantId, Outcome};
use scull_storage::ItemOutcome as AuditOutcome;

/// FX-RACE-01: between plan and execute, the planned `lib/target` is moved away and a link to
/// the canary directory takes its place (a symlink, or a junction on Windows hosts without
/// symlink rights). The item is skipped (identity mismatch), the audit records the skip, the
/// canary is intact, and the other plan item still runs.
#[test]
fn fx_race_01_candidate_replaced_by_link_to_canary_skipped_audited() {
    let mut fx = three_tiers();
    let h = harness();
    let scan = h.scan(&fx);
    let rels = ["lib/target", "app/target"];
    let p = plan(&fx, &scan, &rels, "PR01");

    std::fs::rename(fx.root().join("lib/target"), fx.root().join("lib/moved")).unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    let linked = fx.symlink("lib/target", &canary).unwrap().is_created()
        || fx.junction("lib/target", &canary).unwrap().is_created();
    assert!(linked, "neither a symlink nor a junction could be created");
    // The disk as the Executor finds it: the baseline for I-3 and I-4.
    fx.checkpoint().unwrap();

    let mut s = store(&fx);
    let op = OperationId::from_suffix("OR01").unwrap();
    let r = run(&fx, &h, &mut s, &p, &op);
    let lib = r
        .items
        .iter()
        .find(|i| exec::engine::rel(&fx, &i.path.to_path_buf()) == "lib/target")
        .unwrap();
    let ItemOutcome::Skipped(skip) = &lib.outcome else {
        panic!("lib/target was not skipped: {:?}", lib.outcome)
    };
    assert!(matches!(skip, Skip::Dropped { .. }), "{skip:?}");
    println!("lib/target skipped [{}]: {skip}", skip.code());
    assert!(std::fs::symlink_metadata(fx.root().join("lib/target")).is_ok());
    assert!(fx.root().join("lib/moved/debug/lib").is_file());
    assert!(canary.join("keep.txt").is_file());

    let audit = s.audit_items(&op).unwrap();
    let rec = audit
        .iter()
        .find(|i| i.item_id == lib.item_id)
        .expect("the skip is audited");
    assert_eq!(rec.outcome, AuditOutcome::Skipped);
    assert_eq!(rec.reason.as_deref(), Some(skip.code()));

    let report = fx.check_invariants(&inputs(&p, &r, &s, &op, &["app/target"]));
    println!("{report}");
    report.assert_ok();
    for id in [InvariantId::I2, InvariantId::I4] {
        assert_eq!(report.outcome(id), &Outcome::Passed, "{id}: {report}");
    }
}
