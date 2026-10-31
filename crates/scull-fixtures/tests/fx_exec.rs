//! End-to-end FX-EXEC fixtures (docs/FIXTURES.md §3.5) that fill the gaps left by the
//! layer-level tests: the read-only engine plans from a real fixture tree, the real Executor
//! runs the plan under the execution lock inside an audited operation, and every invariant
//! with an input runs, including **I-4** on the Executor's own audit.
//!
//! I-5 (an operation record holding predicted *and* measured bytes) cannot hold for an
//! Executor `Operation` yet: `Operation::finish` always records `measured_delta_bytes: None`
//! (#76). FX-EXEC-01 therefore measures the delta and asserts it is reported, but feeds no
//! operation bytes, and the registry keeps it tracked by #76.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod exec;

use exec::engine::{
    MACHINE, USER, ctx, harness, inputs, locked, plan, print_report, run, start, store, three_tiers,
};
use scull_cleaner::measure::{
    ItemExecution, MeasureOptions, PlatformFreeSpace, Tally, VolumeMeter,
};
use scull_cleaner::{ItemOutcome, PlanContext, RemoveOutcome};
use scull_core::{CLI_PLAN_TTL_SECS, ErrorKind, OperationId, Plan};
use scull_engine::{CancelToken, now_unix};
use scull_fixtures::{Fixture, InvariantId, InvariantInputs, InvariantReport, Outcome};
use scull_storage::{ApprovalMethod, ItemOutcome as AuditOutcome, MirrorStatus, NewApproval};
use std::collections::BTreeSet;
use std::time::Duration;

fn check(fx: &Fixture, inputs: &InvariantInputs) -> InvariantReport {
    let report = fx.check_invariants(inputs);
    println!("{report}");
    report.assert_ok();
    report
}

fn assert_ran(report: &InvariantReport, ids: &[InvariantId]) {
    for id in ids {
        assert!(report.outcome(*id).ran(), "{id} did not run: {report}");
    }
}

/// FX-EXEC-01: an S0 item is deleted after the user confirmed the plan (one-time approval
/// consumed), and the measured free-space delta is reported beside the prediction.
#[test]
fn fx_exec_01_s0_deleted_after_confirmation_measured_delta_reported() {
    let fx = three_tiers();
    let h = harness();
    let scan = h.scan(&fx);
    let p = plan(&fx, &scan, &["lib/target"], "PX01");
    let mut s = store(&fx);
    s.insert_plan(&p).unwrap();
    // Confirmation: the approval is recorded, then consumed exactly once.
    s.insert_approval(&NewApproval {
        plan_id: p.id().clone(),
        content_hash: p.content_hash().clone(),
        method: ApprovalMethod::CliInteractive,
        approver: USER.into(),
        approved_at: now_unix(),
        expires_at: now_unix() + 600,
    })
    .unwrap();
    assert!(
        s.consume_approval(p.id(), p.content_hash(), now_unix())
            .unwrap()
    );
    assert!(
        !s.consume_approval(p.id(), p.content_hash(), now_unix())
            .unwrap(),
        "an approval is single-use"
    );

    let op = OperationId::from_suffix("OX01").unwrap();
    let predicted = exec::predict(&fx, "lib/target");
    let meter = VolumeMeter::new(
        PlatformFreeSpace,
        MeasureOptions::default()
            .with_samples(3)
            .with_resampling(Duration::from_millis(200), Duration::from_secs(3)),
    );
    let baseline = meter.before([fx.root()]);
    let exec = locked(&fx);
    let mut operation = exec.begin_operation(&mut s, &p, &start(&p, &op)).unwrap();
    let r = exec
        .execute_plan(
            &p,
            &ctx(),
            &h.revalidation(&fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    operation
        .finish(scull_storage::OperationEnd::Normal, now_unix())
        .unwrap();
    print_report(&fx, &r);
    let ItemOutcome::Deleted(d) = &r.items[0].outcome else {
        panic!("not deleted: {:?}", r.items[0].outcome)
    };
    assert_eq!(d.outcome, RemoveOutcome::Complete);
    assert!(!fx.root().join("lib/target").exists());
    let item = &p.items()[0];
    let mut tally = Tally::new();
    tally.add(
        &predicted.reclaim,
        &predicted.notes,
        ItemExecution::Done {
            removed: d.removed.allocated_bytes,
            moved: 0,
        },
    );
    let delta = meter.after(baseline, &tally);
    println!("{delta}");
    let measured = delta
        .measured_total
        .expect("measured free-space delta reported");
    println!(
        "FX-EXEC-01: predicted {} B, removed {} B, measured {measured:+} B",
        item.reclaimable_bytes, d.removed.allocated_bytes
    );
    assert_eq!(r.error_kind(), None);
    let inv = check(&fx, &inputs(&p, &r, &s, &op, &["lib/target"]));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
    assert_ran(&inv, &[InvariantId::I1, InvariantId::I8]);
}

/// Execute `p` without an operation under `ctx`; the plan must be rejected with `kind`
/// (exit 4) before any item, and nothing may change.
fn assert_rejected(fx: &Fixture, p: &Plan, ctx: &PlanContext<'_>, kind: ErrorKind) {
    let h = harness();
    let e = locked(fx)
        .execute_plan(p, ctx, &h.revalidation(fx), None, &CancelToken::new())
        .unwrap_err();
    println!(
        "rejected: [{}] {} (exit {})",
        e.kind,
        e.message,
        e.exit_code().as_i32()
    );
    assert_eq!(e.kind, kind);
    assert_eq!(e.exit_code().as_i32(), 4);
    let inv = check(fx, &InvariantInputs::new());
    assert_ran(&inv, &[InvariantId::I2, InvariantId::I3]);
}

/// FX-EXEC-07: the plan file is edited to add a path (`web/node_modules`, a real candidate that
/// was not in the reviewed plan). Rejected with exit code 4; nothing touched.
#[test]
fn fx_exec_07_plan_file_edited_to_add_a_path_rejected_exit_4() {
    let fx = three_tiers();
    let h = harness();
    let scan = h.scan(&fx);
    let p = plan(&fx, &scan, &["lib/target"], "PX07");
    let file = fx.base().join("plan.json");
    std::fs::write(&file, serde_json::to_vec_pretty(&p).unwrap()).unwrap();
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    // A well-formed item for a path that was never reviewed in this plan.
    let extra =
        scull_core::PlanItem::from_candidate(exec::engine::at(&fx, &scan, "web/node_modules"))
            .unwrap();
    v["items"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(extra).unwrap());
    std::fs::write(&file, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    let edited: Plan = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(edited.items().len(), 2);
    assert_rejected(&fx, &edited, &ctx(), ErrorKind::PlanTampered);
    assert!(fx.root().join("lib/target").exists() && fx.root().join("web/node_modules").exists());
}

/// FX-EXEC-08: an expired plan. Rejected with exit code 4; nothing touched.
#[test]
fn fx_exec_08_expired_plan_rejected_exit_4() {
    let fx = three_tiers();
    let scan = harness().scan(&fx);
    let p = plan(&fx, &scan, &["lib/target"], "PX08");
    let later = PlanContext {
        now_unix: p.created_unix() + CLI_PLAN_TTL_SECS + 1,
        machine_id: MACHINE,
        user: USER,
    };
    assert_rejected(&fx, &p, &later, ErrorKind::PlanExpired);
}

/// FX-EXEC-09: a plan created on another machine, or for another user. Rejected with exit code
/// 4; nothing touched.
#[test]
fn fx_exec_09_plan_from_another_machine_or_user_rejected_exit_4() {
    let fx = three_tiers();
    let scan = harness().scan(&fx);
    let p = plan(&fx, &scan, &["lib/target"], "PX09");
    for (machine_id, user) in [("machine-other", USER), (MACHINE, "someone-else")] {
        let foreign = PlanContext {
            now_unix: now_unix(),
            machine_id,
            user,
        };
        assert_rejected(&fx, &p, &foreign, ErrorKind::PlanForeign);
    }
}

/// FX-EXEC-13: a normal run of S0 (delete), S1 and S2 (quarantine) items. The audit has exactly
/// one outcome per plan item, consistent with the disk (I-4); its hash chain is intact and the
/// NDJSON mirror matches the database; nothing can be appended after the finish.
#[test]
fn fx_exec_13_normal_run_audit_append_only_one_outcome_per_item() {
    let fx = three_tiers();
    let h = harness();
    let scan = h.scan(&fx);
    let rels = ["lib/target", "app/target", "web/node_modules"];
    let p = plan(&fx, &scan, &rels, "PX13");
    let mut s = store(&fx);
    let op = OperationId::from_suffix("OX13").unwrap();
    let r = run(&fx, &h, &mut s, &p, &op);
    if cfg!(unix) {
        assert_eq!(r.error_kind(), None);
    }

    let items = s.audit_items(&op).unwrap();
    for i in &items {
        println!(
            "  audit #{} {} {} -> {}",
            i.op_seq,
            i.action,
            exec::engine::rel(&fx, &i.path.to_path_buf()),
            i.outcome.as_str()
        );
    }
    assert_eq!(items.len(), p.items().len());
    let ids: BTreeSet<_> = items.iter().map(|i| i.item_id.clone()).collect();
    let planned: BTreeSet<_> = p.items().iter().map(|i| i.item_id.clone()).collect();
    assert_eq!(ids, planned, "exactly one outcome per plan item");
    let outcomes: Vec<AuditOutcome> = items.iter().map(|i| i.outcome).collect();
    assert!(outcomes.contains(&AuditOutcome::Removed), "{outcomes:?}");
    // Windows has no quarantine moves yet: there the S1/S2 items are blocked (refused, audited)
    // instead, still exactly one outcome each.
    if cfg!(unix) {
        assert!(
            outcomes.contains(&AuditOutcome::Quarantined),
            "{outcomes:?}"
        );
    }
    let chain = s.verify_chain(&op).unwrap();
    assert!(chain.is_intact(), "{chain:?}");
    assert_eq!(s.verify_mirror(&op).unwrap().status, MirrorStatus::Matches);
    let rec = s.operation(&op).unwrap().unwrap();
    assert!(rec.finished.is_some());
    assert_eq!(rec.items_recorded, 3);

    let inv = check(&fx, &inputs(&p, &r, &s, &op, &rels));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
    assert_ran(&inv, &[InvariantId::I1, InvariantId::I8]);
}

/// FX-EXEC-14 (Unix): five S0 items, one of which can't be fully removed (a folder inside it
/// is read-only, so the walker may not unlink its entries: permission denied). Exit code 3; the
/// other four complete; the failure is explained (the kept entries with their reason, the
/// audit's `partial` outcome with a reason code). The Windows variant needs a deny-delete ACL
/// injection (#68).
#[test]
#[cfg(unix)]
fn fx_exec_14_one_of_five_fails_exit_3_others_complete() {
    use std::os::unix::fs::PermissionsExt;
    let mut fx = Fixture::new().unwrap();
    for n in 1..=5 {
        let d = format!("p{n}");
        fx.cargo_project(&d, &d)
            .unwrap()
            .cargo_target(&d, &d, 4096)
            .unwrap();
    }
    fx.file("p3/target/debug/locked/held.bin", b"cannot unlink me")
        .unwrap();
    let locked_dir = fx.root().join("p3/target/debug/locked");
    std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    fx.checkpoint().unwrap();
    if std::fs::remove_file(locked_dir.join("held.bin")).is_ok() {
        // Running as root: permissions don't stop unlink, so the failure can't be staged.
        println!("SKIP: permissions are not enforced for this user");
        fx.check_invariants(&InvariantInputs::new().allow("p3"))
            .assert_ok();
        return;
    }

    let h = harness();
    let scan = h.scan(&fx);
    let rels = [
        "p1/target",
        "p2/target",
        "p3/target",
        "p4/target",
        "p5/target",
    ];
    let p = plan(&fx, &scan, &rels, "PX14");
    let mut s = store(&fx);
    let op = OperationId::from_suffix("OX14").unwrap();
    let r = run(&fx, &h, &mut s, &p, &op);

    assert_eq!(r.error_kind(), Some(ErrorKind::PartialFailure));
    assert_eq!(
        scull_core::ScullError::new(ErrorKind::PartialFailure, "")
            .exit_code()
            .as_i32(),
        3
    );
    let mut complete = 0;
    for i in &r.items {
        let rel = exec::engine::rel(&fx, &i.path.to_path_buf());
        let ItemOutcome::Deleted(d) = &i.outcome else {
            panic!("{rel}: {:?}", i.outcome)
        };
        if rel == "p3/target" {
            assert_eq!(d.outcome, RemoveOutcome::Incomplete, "{d:#?}");
            assert!(!d.skipped.is_empty(), "kept entries explained: {d:#?}");
            for k in &d.skipped {
                println!("  kept {} ({})", k.relative.display(), k.reason.as_str());
            }
        } else {
            assert_eq!(d.outcome, RemoveOutcome::Complete, "{rel}: {d:#?}");
            assert!(!fx.root().join(&rel).exists());
            complete += 1;
        }
    }
    assert_eq!(complete, 4);
    assert!(locked_dir.join("held.bin").is_file());
    let audit = s.audit_items(&op).unwrap();
    let p3 = audit
        .iter()
        .find(|i| exec::engine::rel(&fx, &i.path.to_path_buf()) == "p3/target")
        .unwrap();
    assert_eq!(p3.outcome, AuditOutcome::Partial);
    assert!(p3.reason.is_some(), "{p3:?}");
    println!(
        "  audit p3/target -> partial ({})",
        p3.reason.as_deref().unwrap()
    );
    assert_eq!(
        s.operation(&op).unwrap().unwrap().finished.unwrap().status,
        scull_storage::OperationStatus::CompletedWithErrors
    );

    let inv = check(&fx, &inputs(&p, &r, &s, &op, &rels));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
    // Let the temp dir be removed.
    std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
}
