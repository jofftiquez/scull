//! Append-only, hash-chained audit log (SAFE-08, FX-EXEC-13, invariant I-4 data): operation
//! lifecycle, one outcome per item, partial failures item by item, tamper detection with the
//! triggers bypassed, the NDJSON mirror, and a reader running while the executor writes.
//! Everything lives in temp dirs.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use scull_core::{Action, ErrorKind, FsPath, ItemId, OperationId, Plan, ScullError};
use scull_storage::{
    ApprovalMethod, AuditRow, ChainProblem, DiskExpectation, ItemOutcome, ItemRecord, MirrorStatus,
    ObservedChange, OperationAudit, OperationEnd, OperationFinish, OperationStart, OperationStatus,
    Store, StoreError, WalkEnd, WalkSummary, audit_mirror_path, row_hash,
};
use sha2::{Digest, Sha256};
use std::path::Path;

fn op_id(s: &str) -> OperationId {
    OperationId::from_suffix(s).unwrap()
}

fn start_for(p: &Plan, op: &str, at: i64) -> OperationStart {
    OperationStart {
        operation_id: op_id(op),
        plan_id: p.id().clone(),
        content_hash: p.content_hash().clone(),
        approver: "dev".into(),
        method: ApprovalMethod::CliInteractive,
        started_at: at,
    }
}

fn finish(at: i64, end: OperationEnd) -> OperationFinish {
    OperationFinish {
        finished_at: at,
        end,
        measured_delta_bytes: None,
    }
}

fn rejected_kind(e: StoreError) -> ErrorKind {
    match e {
        StoreError::Rejected { kind, .. } => kind,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

fn item(p: &Plan, i: usize) -> ItemId {
    p.items()[i].item_id.clone()
}

/// A store with plan(1) recorded and an operation begun.
fn begun(dir: &tempfile::TempDir) -> (Store, Plan, OperationAudit) {
    let mut s = open(&db_in(dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    let audit = s
        .begin_operation(&start_for(&p, "O1", T0 + 10), &TestPerms::default())
        .unwrap();
    (s, p, audit)
}

/// A normal run over plan(1), in plan order: the delete item removed (800 bytes freed), the
/// quarantine and Trash items moved (their predicted bytes).
fn normal_run(s: &mut Store, p: &Plan, audit: &mut OperationAudit) {
    for (i, planned) in p.items().iter().enumerate() {
        let at = T0 + 11 + i as i64;
        let rec = match planned.action {
            Action::Delete => {
                let mut r = ItemRecord::new(planned.item_id.clone(), at, ItemOutcome::Removed);
                r.freed_bytes = Some(800);
                r.entries_removed = Some(12);
                r.entries_kept = Some(0);
                r
            }
            Action::Quarantine => {
                let mut r = ItemRecord::new(planned.item_id.clone(), at, ItemOutcome::Quarantined);
                r.moved_bytes = Some(planned.reclaimable_bytes);
                r
            }
            _ => {
                let mut r = ItemRecord::new(planned.item_id.clone(), at, ItemOutcome::MovedToTrash);
                r.moved_bytes = Some(planned.reclaimable_bytes);
                r
            }
        };
        s.record_item(audit, &rec).unwrap();
    }
    let mut fin = finish(T0 + 14, OperationEnd::Normal);
    fin.measured_delta_bytes = Some(700);
    s.finish_operation(audit, &fin).unwrap();
}

fn mirror_lines(path: &Path) -> Vec<AuditRow> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Raw connection with the append-only triggers dropped: an attacker (or a bug) editing the
/// database file directly.
fn raw_without_triggers(path: &Path) -> rusqlite::Connection {
    let c = raw(path);
    c.execute_batch(
        "DROP TRIGGER audit_no_update; DROP TRIGGER audit_no_delete; \
         DROP TRIGGER audit_append_in_order; DROP TRIGGER operations_no_update;",
    )
    .unwrap();
    c
}

// ---- FX-EXEC-13: normal run -------------------------------------------------------------------

#[test]
fn fx_exec_13_normal_run_records_one_outcome_per_item_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let op = op_id("O1");

    let report = s.verify_chain(&op).unwrap();
    assert!(report.is_intact(), "{report:?}");
    assert_eq!(report.rows, 5, "started, 3 items, finished");
    assert!(report.finished);

    let rec = s.operation(&op).unwrap().unwrap();
    assert_eq!(rec.plan_id, *p.id());
    assert_eq!(rec.content_hash, *p.content_hash());
    assert_eq!(
        (rec.approver.as_str(), rec.method),
        ("dev", ApprovalMethod::CliInteractive)
    );
    assert_eq!(rec.items_recorded, 3);
    let fin = rec.finished.unwrap();
    assert_eq!(fin.status, OperationStatus::Completed);
    let t = fin.totals;
    assert_eq!(
        (t.items_planned, t.items_done, t.items_missing()),
        (3, 3, 0)
    );
    let predicted: u64 = p.items().iter().map(|i| i.reclaimable_bytes).sum();
    assert_eq!(t.predicted_bytes, predicted);
    assert_eq!(
        (t.moved_bytes, t.freed_bytes, t.measured_delta_bytes),
        (902 + 903, 800, Some(700))
    );

    // Path, action and prediction come from the plan.
    let items = s.audit_items(&op).unwrap();
    assert_eq!(items.len(), 3);
    for (a, planned) in items.iter().zip(p.items()) {
        assert_eq!(a.item_id, planned.item_id);
        assert_eq!(a.path, planned.path);
        assert_eq!(a.predicted_bytes, planned.reclaimable_bytes);
    }
    let mut actions: Vec<&str> = items.iter().map(|a| a.action.as_str()).collect();
    actions.sort_unstable();
    assert_eq!(actions, ["delete", "quarantine", "trash"]);

    // One outcome per item; nothing after the finished row.
    let again = ItemRecord::new(item(&p, 0), T0 + 20, ItemOutcome::Removed);
    let err = s.record_item(&mut audit, &again).unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::Usage);
    let err = s
        .finish_operation(&mut audit, &finish(T0 + 21, OperationEnd::Normal))
        .unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::Usage);
    // Even a resumed handle cannot reopen a finished operation.
    let err = s.resume_operation(&op, &TestPerms::default()).unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::Usage);
}

#[test]
fn fx_exec_13_audit_is_append_only_at_the_sql_level() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let c = raw(&db_in(&dir));
    for sql in [
        "UPDATE audit SET freed_bytes = 0",
        "UPDATE audit SET outcome = 'failed' WHERE kind = 'item'",
        "DELETE FROM audit WHERE kind = 'finished'",
        "DELETE FROM audit",
    ] {
        let err = c.execute(sql, []).unwrap_err().to_string();
        assert!(err.contains("append-only"), "{sql}: {err}");
    }
    // Inserting out of order, after the finished row, or into the frozen v1 table.
    let p2 = plan(2);
    s.insert_plan(&p2).unwrap();
    s.begin_operation(&start_for(&p2, "O2", T0 + 30), &TestPerms::default())
        .unwrap();
    for seq in [0, 2] {
        let err = c
            .execute(
                "INSERT INTO audit (operation_id, op_seq, kind, recorded_at, prev_hash, \
                 row_hash) VALUES ('op_O2', ?1, 'legacy', 0, ?2, ?3)",
                rusqlite::params![seq, "0".repeat(64), "1".repeat(64)],
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("in order"), "{err}");
    }
    let err = c
        .execute(
            "INSERT INTO audit (operation_id, op_seq, kind, recorded_at, prev_hash, row_hash) \
             VALUES ('op_O1', 5, 'legacy', 0, ?1, ?2)",
            ["0".repeat(64), "1".repeat(64)],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("closed"), "{err}");
    let err = c
        .execute(
            "INSERT INTO audit_v1 (operation_id, recorded_at, outcome) VALUES ('op_O1', 0, 'x')",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("frozen"), "{err}");
}

#[test]
fn normal_end_requires_an_outcome_for_every_plan_item() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    s.record_item(
        &mut audit,
        &ItemRecord::new(item(&p, 0), T0 + 11, ItemOutcome::Removed),
    )
    .unwrap();
    let err = s
        .finish_operation(&mut audit, &finish(T0 + 12, OperationEnd::Normal))
        .unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::Usage);

    // The data I-4 needs: the two untouched items have no outcome.
    let exp = s.disk_expectations(&op_id("O1")).unwrap();
    assert!(!exp.finished);
    assert_eq!(exp.items.len(), 1);
    assert_eq!(exp.items[0].expect, DiskExpectation::Gone);
    let missing: Vec<&ItemId> = exp.without_outcome.iter().map(|(id, _)| id).collect();
    assert_eq!(missing, [&p.items()[1].item_id, &p.items()[2].item_id]);

    // A cancelled run may leave items without outcome.
    let sum = s
        .finish_operation(&mut audit, &finish(T0 + 13, OperationEnd::Cancelled))
        .unwrap();
    assert_eq!(sum.status, OperationStatus::Cancelled);
    assert_eq!(sum.totals.items_missing(), 2);
    assert!(s.verify_chain(&op_id("O1")).unwrap().is_intact());
}

#[test]
fn outcomes_are_checked_against_the_plan_and_privacy_rules() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    let stranger = ItemId::try_from("f".repeat(32)).unwrap();
    let err = s
        .record_item(
            &mut audit,
            &ItemRecord::new(stranger, T0 + 11, ItemOutcome::Removed),
        )
        .unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::PlanInvalidated);

    let id = item(&p, 0);
    let cases = [
        // Skips, refusals, failures and partials need a reason ...
        ItemRecord::new(id.clone(), T0, ItemOutcome::Skipped),
        ItemRecord::new(id.clone(), T0, ItemOutcome::Partial),
        // ... which is a stable code, never free text or an environment value (FX-DATA-07).
        ItemRecord::skipped(id.clone(), T0, "File /home/dev/.ssh/id_rsa changed"),
        ItemRecord::skipped(id.clone(), T0, "HOME=/home/dev"),
        ItemRecord::skipped(id.clone(), T0, ""),
        ItemRecord::skipped(id.clone(), T0, "x".repeat(65)),
        // An outcome that touched nothing cannot report removed bytes.
        {
            let mut r = ItemRecord::new(id.clone(), T0, ItemOutcome::Failed).with_reason("io");
            r.freed_bytes = Some(5);
            r
        },
    ];
    for rec in cases {
        let err = s.record_item(&mut audit, &rec).unwrap_err();
        assert_eq!(rejected_kind(err), ErrorKind::Usage, "{rec:?}");
    }
    assert!(s.audit_items(&op_id("O1")).unwrap().is_empty());
    s.record_item(
        &mut audit,
        &ItemRecord::skipped(id, T0 + 11, "tier_raised_to_s3"),
    )
    .unwrap();
}

#[test]
fn operation_needs_a_recorded_intact_plan_and_a_fresh_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    let perms = TestPerms::default();
    let err = s
        .begin_operation(&start_for(&p, "O1", T0), &perms)
        .unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::NotFound);
    s.insert_plan(&p).unwrap();
    let mut wrong = start_for(&p, "O1", T0);
    wrong.content_hash = plan(2).content_hash().clone();
    let err = s.begin_operation(&wrong, &perms).unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::PlanTampered);
    s.begin_operation(&start_for(&p, "O2", T0), &perms).unwrap();
    let err = s
        .begin_operation(&start_for(&p, "O2", T0 + 1), &perms)
        .unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::Usage);
}

// ---- partial failure, item by item (SAFE-08, FX-EXEC-14 shape) ---------------------------------

#[test]
fn partial_failure_is_recorded_item_by_item() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan_with_items(3, 5);
    s.insert_plan(&p).unwrap();
    let mut audit = s
        .begin_operation(&start_for(&p, "O5", T0 + 10), &TestPerms::default())
        .unwrap();
    let walk = |end, removed, kept, reason: Option<&str>| WalkSummary {
        end,
        entries_removed: removed,
        freed_bytes: removed * 100,
        entries_kept: kept,
        reason: reason.map(str::to_owned),
    };
    // Items 0, 2, 3 complete; 1 is locked (nothing removed); 4 loses entries to permissions.
    let records = [
        ItemRecord::from_walk(item(&p, 0), T0 + 11, &walk(WalkEnd::Complete, 3, 0, None)),
        ItemRecord::from_walk(
            item(&p, 1),
            T0 + 12,
            &walk(WalkEnd::Incomplete, 0, 1, Some("busy")),
        ),
        ItemRecord::from_walk(item(&p, 2), T0 + 13, &walk(WalkEnd::Complete, 4, 0, None)),
        ItemRecord::from_walk(item(&p, 3), T0 + 14, &walk(WalkEnd::Complete, 5, 0, None)),
        ItemRecord::from_walk(
            item(&p, 4),
            T0 + 15,
            &walk(WalkEnd::Incomplete, 6, 2, Some("permission_denied")),
        ),
    ];
    for r in &records {
        s.record_item(&mut audit, r).unwrap();
    }
    let sum = s
        .finish_operation(&mut audit, &finish(T0 + 16, OperationEnd::Normal))
        .unwrap();
    assert_eq!(sum.status, OperationStatus::CompletedWithErrors);
    let t = sum.totals;
    assert_eq!(
        (t.items_done, t.items_failed, t.items_partial, t.freed_bytes),
        (3, 1, 1, 1800)
    );

    let items = s.audit_items(&op_id("O5")).unwrap();
    let got: Vec<(ItemOutcome, Option<&str>, Option<u64>)> = items
        .iter()
        .map(|i| (i.outcome, i.reason.as_deref(), i.entries_kept))
        .collect();
    assert_eq!(
        got,
        [
            (ItemOutcome::Removed, None, Some(0)),
            (ItemOutcome::Failed, Some("busy"), Some(1)),
            (ItemOutcome::Removed, None, Some(0)),
            (ItemOutcome::Removed, None, Some(0)),
            (ItemOutcome::Partial, Some("permission_denied"), Some(2)),
        ]
    );
    assert!(s.verify_chain(&op_id("O5")).unwrap().is_intact());
}

#[test]
fn executor_helpers_map_errors_and_walks() {
    let id = ItemId::try_from("a".repeat(32)).unwrap();
    let refused = ItemRecord::from_error(
        id.clone(),
        T0,
        &ScullError::new(ErrorKind::PlanInvalidated, "path swapped: /secret"),
    );
    assert_eq!(refused.outcome, ItemOutcome::Refused);
    assert_eq!(refused.reason.as_deref(), Some("plan_invalidated"));
    let failed = ItemRecord::from_error(
        id.clone(),
        T0,
        &ScullError::new(ErrorKind::PermissionDenied, "x"),
    );
    assert_eq!(
        (failed.outcome, failed.reason.as_deref()),
        (ItemOutcome::Failed, Some("permission_denied"))
    );
    let aborted = ItemRecord::from_walk(
        id,
        T0,
        &WalkSummary {
            end: WalkEnd::Aborted,
            entries_removed: 2,
            freed_bytes: 10,
            entries_kept: 0,
            reason: None,
        },
    );
    assert_eq!(
        (aborted.outcome, aborted.reason.as_deref()),
        (ItemOutcome::Partial, Some("aborted"))
    );
}

// ---- tamper evidence (triggers bypassed) ------------------------------------------------------

#[test]
fn editing_a_row_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let c = raw_without_triggers(&db_in(&dir));
    c.execute(
        "UPDATE audit SET outcome = 'skipped', reason = 'x' WHERE op_seq = 2",
        [],
    )
    .unwrap();
    let r = s.verify_chain(&op_id("O1")).unwrap();
    assert!(
        r.problems
            .contains(&ChainProblem::HashMismatch { op_seq: 2 }),
        "{r:?}"
    );
    assert!(r.problems.contains(&ChainProblem::TotalsMismatch), "{r:?}");
    assert_eq!(s.verify_all_chains().unwrap().len(), 1);
    // The mirror still has the original row.
    assert_eq!(
        s.verify_mirror(&op_id("O1")).unwrap().status,
        MirrorStatus::Diverged { op_seq: 2 }
    );
}

#[test]
fn editing_a_row_and_recomputing_its_hash_breaks_the_next_link() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let op = op_id("O1");
    let mut row = s.audit_rows(&op).unwrap()[1].clone();
    row.body.freed_bytes = Some(999_999);
    let forged = row_hash(&row.prev_hash, &row.body);
    let c = raw_without_triggers(&db_in(&dir));
    c.execute(
        "UPDATE audit SET freed_bytes = 999999, row_hash = ?1 WHERE op_seq = 1",
        [&forged],
    )
    .unwrap();
    let r = s.verify_chain(&op).unwrap();
    assert!(
        r.problems.contains(&ChainProblem::BrokenLink { op_seq: 2 }),
        "{r:?}"
    );
    assert!(r.problems.contains(&ChainProblem::TotalsMismatch), "{r:?}");
    assert!(
        !r.problems
            .iter()
            .any(|p| matches!(p, ChainProblem::HashMismatch { .. }))
    );
}

#[test]
fn deleting_a_row_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let c = raw_without_triggers(&db_in(&dir));
    // Hide the quarantined item.
    c.execute("DELETE FROM audit WHERE op_seq = 2", []).unwrap();
    let r = s.verify_chain(&op_id("O1")).unwrap();
    assert!(
        r.problems.contains(&ChainProblem::SeqGap {
            expected: 2,
            found: 3
        }),
        "{r:?}"
    );
    assert!(r.problems.contains(&ChainProblem::BrokenLink { op_seq: 3 }));
    assert!(r.problems.contains(&ChainProblem::TotalsMismatch));
    // I-4 data now shows the item as having no outcome.
    assert_eq!(
        s.disk_expectations(&op_id("O1"))
            .unwrap()
            .without_outcome
            .len(),
        1
    );
}

#[test]
fn deleting_the_whole_tail_or_chain_is_caught_by_the_mirror_or_the_start_check() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let op = op_id("O1");
    let c = raw_without_triggers(&db_in(&dir));
    // Truncating the tail keeps a consistent prefix: only the mirror can tell.
    c.execute("DELETE FROM audit WHERE op_seq >= 3", [])
        .unwrap();
    assert!(s.verify_chain(&op).unwrap().is_intact());
    assert_eq!(s.verify_mirror(&op).unwrap().status, MirrorStatus::Ahead);
    // Removing every row leaves the operation without its start.
    c.execute("DELETE FROM audit", []).unwrap();
    let r = s.verify_chain(&op).unwrap();
    assert_eq!(r.problems, [ChainProblem::MissingStart]);
}

#[test]
fn editing_the_operation_row_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let c = raw_without_triggers(&db_in(&dir));
    c.execute("UPDATE operations SET approver = 'mallory'", [])
        .unwrap();
    let r = s.verify_chain(&op_id("O1")).unwrap();
    assert_eq!(r.problems, [ChainProblem::StartMismatch]);
}

#[test]
fn unknown_operation_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let s = open(&db_in(&dir));
    let err = s.verify_chain(&op_id("NOPE")).unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::NotFound);
    assert!(s.operation(&op_id("NOPE")).unwrap().is_none());
}

// ---- NDJSON mirror ----------------------------------------------------------------------------

#[test]
fn ndjson_mirror_equals_database_rows_and_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let op = op_id("O1");
    let path = audit_mirror_path(dir.path(), &op);
    assert_eq!(audit.mirror_path(), path);
    assert_eq!(path.parent().unwrap().file_name().unwrap(), "audit");

    let db = s.audit_rows(&op).unwrap();
    assert_eq!(mirror_lines(&path), db);
    let report = s.verify_mirror(&op).unwrap();
    assert_eq!(report.status, MirrorStatus::Matches);
    assert_eq!((report.db_rows, report.mirror_rows), (5, 5));

    // Each line is checkable with standard tools: the line starts with the canonical body and
    // row_hash = sha256(prev_hash || canonical body).
    let text = std::fs::read_to_string(&path).unwrap();
    for (line, row) in text.lines().zip(&db) {
        let canonical = row.body.canonical_json();
        let open_body = &canonical[..canonical.len() - 1];
        assert!(line.as_bytes().starts_with(open_body), "{line}");
        let mut h = Sha256::new();
        h.update(row.prev_hash.as_bytes());
        h.update(&canonical);
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, row.row_hash);
    }

    // Part of scull's footprint (DATA-05), so `scull data purge` finds it.
    assert!(scull_storage::footprint_files(&db_in(&dir)).contains(&path));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }
}

#[test]
fn mirror_that_fell_behind_is_caught_up_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    s.record_item(
        &mut audit,
        &ItemRecord::new(item(&p, 0), T0 + 11, ItemOutcome::Removed),
    )
    .unwrap();
    let op = op_id("O1");
    let path = audit_mirror_path(dir.path(), &op);
    drop(audit);
    // Simulate a crash after the database commit but before the mirror write (test-only
    // truncation; scull itself never truncates), plus a torn half-line.
    let text = std::fs::read_to_string(&path).unwrap();
    let first = text.lines().next().unwrap();
    std::fs::write(&path, format!("{first}\n{{\"v\":1,\"oper")).unwrap();
    let r = s.verify_mirror(&op).unwrap();
    assert_eq!(
        (r.status, r.bad_lines.as_slice()),
        (MirrorStatus::Behind, &[2][..])
    );

    let mut audit = s.resume_operation(&op, &TestPerms::default()).unwrap();
    let r = s.verify_mirror(&op).unwrap();
    assert_eq!(r.status, MirrorStatus::Matches);
    assert_eq!(r.bad_lines, [2], "the torn line stays visible");
    for i in 1..3 {
        s.record_item(
            &mut audit,
            &ItemRecord::skipped(item(&p, i), T0 + 12, "identity_changed"),
        )
        .unwrap();
    }
    s.finish_operation(&mut audit, &finish(T0 + 13, OperationEnd::Interrupted))
        .unwrap();
    assert_eq!(s.verify_mirror(&op).unwrap().status, MirrorStatus::Matches);
    assert!(s.verify_chain(&op).unwrap().is_intact());

    // A missing mirror is reported, and recreated from the database on resume.
    let mut s2 = open(&db_in(&dir));
    let p2 = plan(2);
    s2.insert_plan(&p2).unwrap();
    let a2 = s2
        .begin_operation(&start_for(&p2, "O2", T0 + 20), &TestPerms::default())
        .unwrap();
    std::fs::remove_file(a2.mirror_path()).unwrap();
    assert_eq!(
        s2.verify_mirror(&op_id("O2")).unwrap().status,
        MirrorStatus::Missing
    );
    s2.resume_operation(&op_id("O2"), &TestPerms::default())
        .unwrap();
    assert_eq!(
        s2.verify_mirror(&op_id("O2")).unwrap().status,
        MirrorStatus::Matches
    );
}

#[cfg(unix)]
#[test]
fn planted_mirror_symlink_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    let path = audit_mirror_path(dir.path(), &op_id("O1"));
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    std::os::unix::fs::symlink(&victim, &path).unwrap();
    assert!(
        s.begin_operation(&start_for(&p, "O1", T0), &TestPerms::default())
            .is_err()
    );
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    assert!(s.operation(&op_id("O1")).unwrap().is_none());
}

// ---- queries and I-4 data ---------------------------------------------------------------------

#[test]
fn operations_since_and_disk_expectations() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, p, mut audit) = begun(&dir);
    normal_run(&mut s, &p, &mut audit);
    let p2 = plan(2);
    s.insert_plan(&p2).unwrap();
    s.begin_operation(&start_for(&p2, "O2", T0 + 100), &TestPerms::default())
        .unwrap();
    let all: Vec<String> = s
        .operations(0)
        .unwrap()
        .into_iter()
        .map(|o| o.operation_id.to_string())
        .collect();
    assert_eq!(all, ["op_O1", "op_O2"]);
    let recent = s.operations(T0 + 50).unwrap();
    assert_eq!(recent.len(), 1);
    assert!(recent[0].finished.is_none());

    let exp = s.disk_expectations(&op_id("O1")).unwrap();
    assert!(exp.finished && exp.without_outcome.is_empty());
    let expect: Vec<DiskExpectation> = exp.items.iter().map(|i| i.expect).collect();
    assert_eq!(expect, [DiskExpectation::Gone; 3]);
    assert!(exp.mismatches(|_| ObservedChange::Gone).is_empty());
    let first = exp.items[0].path.clone();
    let bad = exp.mismatches(|path: &FsPath| {
        if *path == first {
            ObservedChange::Unchanged
        } else {
            ObservedChange::Gone
        }
    });
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].contains("removed"), "{bad:?}");
}

// ---- concurrency: a reader while the executor writes (FX-DATA-05) ------------------------------

#[test]
fn reader_sees_a_consistent_chain_while_items_are_appended() {
    const N: u128 = 60;
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let p = plan_with_items(9, N);
    s.insert_plan(&p).unwrap();
    let mut audit = s
        .begin_operation(&start_for(&p, "OC", T0), &TestPerms::default())
        .unwrap();
    let op = op_id("OC");

    let reader_path = path.clone();
    let reader_op = op.clone();
    let reader = std::thread::spawn(move || {
        let r = open(&reader_path);
        let mut last = 0usize;
        let mut checks = 0u32;
        loop {
            let rows = r.audit_rows(&reader_op).unwrap();
            assert!(rows.len() >= last, "rows never disappear");
            last = rows.len();
            let report = r.verify_chain(&reader_op).unwrap();
            assert!(report.is_intact(), "{report:?}");
            checks += 1;
            if report.finished {
                return checks;
            }
        }
    });
    for i in 0..N as usize {
        s.record_item(
            &mut audit,
            &ItemRecord::new(item(&p, i), T0 + 1, ItemOutcome::Removed),
        )
        .unwrap();
    }
    s.finish_operation(&mut audit, &finish(T0 + 2, OperationEnd::Normal))
        .unwrap();
    let checks = reader.join().unwrap();
    assert!(checks >= 1);
    assert_eq!(s.audit_items(&op).unwrap().len(), N as usize);
    assert_eq!(s.verify_mirror(&op).unwrap().status, MirrorStatus::Matches);
}
