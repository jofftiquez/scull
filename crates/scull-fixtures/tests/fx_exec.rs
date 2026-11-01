//! End-to-end FX-EXEC fixtures (docs/FIXTURES.md §3.5) that fill the gaps left by the
//! layer-level tests: the read-only engine plans from a real fixture tree, the real Executor
//! runs the plan under the execution lock inside an audited operation, and every invariant
//! with an input runs, including **I-4** on the Executor's own audit.
//!
//! FX-EXEC-01 also checks **I-5**: the operation measures the free-space delta itself
//! (`Operation::with_meter`) and `Operation::finish` records it next to the predicted bytes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod exec;

use exec::engine::{
    MACHINE, USER, ctx, harness, inputs, locked, plan, print_report, run, start, store, three_tiers,
};
use scull_cleaner::measure::{MeasureOptions, PlatformFreeSpace, VolumeMeter};
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
    let meter = VolumeMeter::new(
        PlatformFreeSpace,
        MeasureOptions::default()
            .with_samples(3)
            .with_resampling(Duration::from_millis(200), Duration::from_secs(3)),
    );
    let exec = locked(&fx);
    let mut operation = exec
        .begin_operation(&mut s, &p, &start(&p, &op))
        .unwrap()
        .with_quarantine(scull_cleaner::QuarantineSettings {
            base: Some(fx.quarantine_base().to_path_buf()),
            ..scull_cleaner::QuarantineSettings::default()
        })
        .with_meter(meter);
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
    // The measured free-space delta is reported beside the prediction ...
    let delta = r
        .measurement
        .as_ref()
        .expect("the operation measured the run");
    println!("{delta}");
    let measured = delta
        .measured_total
        .expect("measured free-space delta reported");
    println!(
        "FX-EXEC-01: predicted {} B, removed {} B, measured {measured:+} B",
        item.reclaimable_bytes, d.removed.allocated_bytes
    );
    assert_eq!(delta.removed, d.removed.allocated_bytes);
    // ... and recorded with the operation (I-5).
    let rec = s.operation(&op).unwrap().expect("operation recorded");
    let fin = rec.finished.as_ref().expect("operation finished");
    assert_eq!(fin.totals.measured_delta_bytes, Some(measured), "{rec:#?}");
    assert_eq!(r.error_kind(), None);
    let inv = check(
        &fx,
        &inputs(&p, &r, &s, &op, &["lib/target"]).operation_bytes(&s, &op),
    );
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
    assert_eq!(inv.outcome(InvariantId::I5), &Outcome::Passed, "{inv}");
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

/// FX-EXEC-14: five S0 items, one of which can't be fully removed (permission denied on a
/// folder inside it: read-only on Unix, deny-delete ACEs on Windows). Exit code 3; the
/// other four complete; the failure is explained (the kept entries with their reason, the
/// audit's `partial` outcome with a reason code).
#[test]
fn fx_exec_14_one_of_five_fails_exit_3_others_complete() {
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
    let staged = deny_removal(&locked_dir);
    fx.checkpoint().unwrap();
    if !staged {
        println!("SKIP: permission denied can't be staged for this user");
        allow_removal(&locked_dir);
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
    // Restore access first, so the snapshot (I-3) can read the kept folder and the temp dir
    // can be removed.
    allow_removal(&locked_dir);
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
}

/// Make the entries of `dir` impossible to remove for this user: Unix: a read-only directory
/// (false when running as root, where permissions don't stop unlink); Windows: ACEs denying
/// everyone "delete child" on `dir` and "delete" on its files (false if icacls can't set them).
#[cfg(unix)]
fn deny_removal(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // A file this process creates is owned by its effective user; root ignores permissions.
    let probe = tempfile::NamedTempFile::new().unwrap();
    probe.as_file().metadata().unwrap().uid() != 0
}

#[cfg(unix)]
fn allow_removal(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(windows)]
fn deny_removal(dir: &std::path::Path) -> bool {
    // Reading stays allowed (the scan must see the folder whole, or it would not be S0); only
    // removal is denied: "delete child" on the folder and "delete" on each file in it.
    let mut ok = icacls(&[dir.as_os_str(), "/deny".as_ref(), "*S-1-1-0:(DC)".as_ref()]);
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        ok &= icacls(&[p.as_os_str(), "/deny".as_ref(), "*S-1-1-0:(DE)".as_ref()]);
    }
    ok
}

#[cfg(windows)]
fn allow_removal(dir: &std::path::Path) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        icacls(&[p.as_os_str(), "/remove:d".as_ref(), "*S-1-1-0".as_ref()]);
    }
    assert!(icacls(&[
        dir.as_os_str(),
        "/remove:d".as_ref(),
        "*S-1-1-0".as_ref()
    ]));
}

#[cfg(windows)]
fn icacls(args: &[&std::ffi::OsStr]) -> bool {
    std::process::Command::new("icacls")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// FX-CLS-06 (deletion half, through the real Executor): `node_modules` holding a workspace
/// symlink to a sibling package, an `npm link` symlink to a folder outside the project and a
/// link into the canary's directory is planned S0 and deleted under the execution lock in an
/// audited operation: the links go, every linked folder and the canary stay intact (I-2),
/// nothing else changed (I-3), the audit matches the disk (I-4). The classification half is
/// `fx_cls.rs::fx_cls_06_linked_workspace_sources_survive_node_modules_removal`.
#[test]
fn fx_cls_06_executor_deletes_node_modules_links_only_sources_intact() {
    let mut fx = Fixture::new().unwrap();
    fx.file(
        "web/package.json",
        "{\"name\":\"web\",\"private\":true,\"workspaces\":[\"packages/*\"]}\n",
    )
    .unwrap()
    .file("web/package-lock.json", "{}")
    .unwrap()
    .npm_project(
        "web/packages/ui",
        "@acme/ui",
        scull_fixtures::NodeLock::None,
    )
    .unwrap()
    .file("web/packages/ui/src/index.js", "export const ui = 1;\n")
    .unwrap()
    .npm_project("linked-lib", "linked-lib", scull_fixtures::NodeLock::None)
    .unwrap()
    .file("linked-lib/src/index.js", "module.exports = 1;\n")
    .unwrap()
    .node_modules("web", &["left-pad"])
    .unwrap();
    let ws = fx
        .symlink("web/node_modules/@acme/ui", "../../packages/ui")
        .unwrap();
    let linked = fx
        .symlink("web/node_modules/linked-lib", "../../linked-lib")
        .unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    let escape = fx.symlink("web/node_modules/escape", &canary).unwrap();
    let links = [&ws, &linked, &escape]
        .iter()
        .filter(|l| l.is_created())
        .count();
    println!("links created: {links} of 3 (Windows without symlink rights creates none)");
    fx.checkpoint().unwrap();

    let h = harness();
    let scan = h.scan(&fx);
    let c = exec::engine::at(&fx, &scan, "web/node_modules");
    assert_eq!(c.tier, scull_core::Tier::S0, "{:?}", c.reasons);
    assert_eq!(c.action, scull_core::Action::Delete);
    let p = plan(&fx, &scan, &["web/node_modules"], "PXCLS06");
    let mut s = store(&fx);
    let op = OperationId::from_suffix("OXCLS06").unwrap();
    let r = run(&fx, &h, &mut s, &p, &op);

    let ItemOutcome::Deleted(d) = &r.items[0].outcome else {
        panic!("not deleted: {:?}", r.items[0].outcome)
    };
    assert_eq!(d.outcome, RemoveOutcome::Complete);
    assert!(!fx.root().join("web/node_modules").exists());
    // The links went; what they pointed to did not.
    for kept in [
        "web/packages/ui/src/index.js",
        "web/packages/ui/package.json",
        "linked-lib/src/index.js",
        "linked-lib/package.json",
        "web/package.json",
        "web/package-lock.json",
    ] {
        assert!(fx.root().join(kept).is_file(), "{kept} must survive");
    }
    assert!(
        canary.join("keep.txt").is_file(),
        "the canary's folder survives"
    );
    assert_eq!(r.error_kind(), None);
    let inv = check(&fx, &inputs(&p, &r, &s, &op, &["web/node_modules"]));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
    assert_ran(
        &inv,
        &[
            InvariantId::I1,
            InvariantId::I2,
            InvariantId::I3,
            InvariantId::I8,
        ],
    );
}
