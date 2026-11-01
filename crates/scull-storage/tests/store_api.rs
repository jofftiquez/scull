//! Typed API: scans, items, plans, approvals (single use, expiry), operations, audit, and the
//! append-only triggers (SAFE-08, DATA-02).
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use scull_core::{Action, Category, ContentHash, ErrorKind, FsPath, OperationId, Tier};
use scull_storage::{
    ApprovalMethod, ItemOutcome, ItemRecord, NewApproval, OperationEnd, OperationFinish,
    OperationStart, Rollup, ScanFinish, ScanStatus, ScanTotals, Store, StoreError,
};

fn approval_for(plan: &scull_core::Plan, approved_at: i64, expires_at: i64) -> NewApproval {
    NewApproval {
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        method: ApprovalMethod::CliTty,
        approver: "dev".into(),
        approved_at,
        expires_at,
    }
}

fn other_hash() -> ContentHash {
    ContentHash::try_from("a".repeat(64)).unwrap()
}

fn rejected_kind(e: StoreError) -> ErrorKind {
    match e {
        StoreError::Rejected { kind, .. } => kind,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

/// Assert a raw statement is aborted by an append-only trigger.
fn assert_blocked(conn: &rusqlite::Connection, sql: &str) {
    let err = conn.execute(sql, []).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("append-only") || msg.contains("immutable") || msg.contains("never deleted"),
        "{sql}: unexpected error {msg}"
    );
}

/// Store with a plan, an approval, an operation and one audit row.
fn populated() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    s.insert_approval(&approval_for(&p, T0 + 10, T0 + 600))
        .unwrap();
    let op = OperationId::from_suffix("O1").unwrap();
    let mut audit = s
        .begin_operation(
            &OperationStart {
                operation_id: op,
                plan_id: p.id().clone(),
                content_hash: p.content_hash().clone(),
                approver: "dev".into(),
                method: ApprovalMethod::CliTty,
                started_at: T0 + 20,
            },
            &TestPerms::default(),
        )
        .unwrap();
    let item = &p.items()[0];
    let mut rec = ItemRecord::new(item.item_id.clone(), T0 + 21, ItemOutcome::Removed);
    rec.freed_bytes = Some(item.reclaimable_bytes - 1);
    s.record_item(&mut audit, &rec).unwrap();
    s.finish_operation(
        &mut audit,
        &OperationFinish {
            finished_at: T0 + 22,
            end: OperationEnd::Cancelled,
            measured_delta_bytes: None,
        },
    )
    .unwrap();
    (dir, path, s)
}

// ---- append-only ------------------------------------------------------------------------

#[test]
fn append_only_triggers_reject_update_and_delete() {
    let (_dir, path, _s) = populated();
    let c = raw(&path);
    assert_blocked(&c, "UPDATE plans SET expires_at = expires_at + 1");
    assert_blocked(&c, "UPDATE plans SET canonical_json = '{}'");
    assert_blocked(&c, "DELETE FROM plans");
    assert_blocked(&c, "UPDATE audit SET freed_bytes = 0");
    assert_blocked(&c, "UPDATE audit SET outcome = 'failed'");
    assert_blocked(&c, "DELETE FROM audit");
    assert_blocked(&c, "UPDATE operations SET approver = 'mallory'");
    assert_blocked(&c, "DELETE FROM operations");
    assert_blocked(&c, "DELETE FROM approvals");
    assert_blocked(&c, "UPDATE approvals SET expires_at = expires_at + 1000");
    assert_blocked(
        &c,
        "UPDATE approvals SET method = 'desktop', used_at = 1800000100",
    );
    // Setting used_at once from NULL is the one allowed change ...
    c.execute("UPDATE approvals SET used_at = 1800000100", [])
        .unwrap();
    // ... and only once.
    assert_blocked(&c, "UPDATE approvals SET used_at = 1800000200");
    assert_blocked(&c, "UPDATE approvals SET used_at = NULL");
    let rows: i64 = c
        .query_row("SELECT count(*) FROM audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 3, "started, one item, finished");
}

#[test]
fn approvals_cannot_be_inserted_already_used_or_for_another_hash() {
    let (_dir, path, _s) = populated();
    let c = raw(&path);
    c.execute_batch(
        "INSERT INTO plans (plan_id, content_hash, origin, scan_id, created_at, expires_at, \
         item_count, reclaimable_bytes, canonical_json) VALUES ('plan_X', \
         'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'cli', 'scan_S1', \
         1, 2, 0, 0, '{}')",
    )
    .unwrap();
    let wrong_hash = c.execute(
        "INSERT INTO approvals (plan_id, content_hash, method, approver, approved_at, expires_at) \
         VALUES ('plan_X', 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', \
         'cli_tty', 'dev', 1, 2)",
        [],
    );
    assert!(
        wrong_hash
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
    let pre_used = c.execute(
        "INSERT INTO approvals (plan_id, content_hash, method, approver, approved_at, expires_at, \
         used_at) VALUES ('plan_X', \
         'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'cli_tty', 'dev', \
         1, 2, 1)",
        [],
    );
    assert!(
        pre_used
            .unwrap_err()
            .to_string()
            .contains("already be used")
    );
}

#[test]
fn quarantine_manifest_paths_are_immutable_and_rows_never_deleted() {
    let (_dir, path, _s) = populated();
    let c = raw(&path);
    c.execute(
        "INSERT INTO quarantine_manifest (operation_id, item_id, original_path, quarantine_path, \
         bytes, quarantined_at, entry_identity, entry_kind, parent_identity, logical_bytes) \
         VALUES ('op_O1', 'i', '/work/p1/target', '/q/1', 10, 5, '7:1', 'dir', '7:2', 8)",
        [],
    )
    .unwrap();
    assert_blocked(&c, "UPDATE quarantine_manifest SET original_path = '/etc'");
    assert_blocked(
        &c,
        "UPDATE quarantine_manifest SET quarantine_path = '/q/2'",
    );
    assert_blocked(&c, "DELETE FROM quarantine_manifest");
    c.execute(
        "UPDATE quarantine_manifest SET restored_at = 6, restored_to = '/work/p1/target'",
        [],
    )
    .unwrap();
    assert_blocked(
        &c,
        "UPDATE quarantine_manifest SET restored_at = 7, restored_to = '/elsewhere'",
    );
}

// ---- plans --------------------------------------------------------------------------------

#[test]
fn plan_round_trips_and_stored_json_hashes_to_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    s.insert_plan(&p).unwrap(); // identical re-insert is a no-op
    assert_eq!(s.get_plan(p.id()).unwrap().unwrap(), p);
    assert!(s.get_plan(&plan_id(99)).unwrap().is_none());
    let stored: String = raw(&path)
        .query_row("SELECT canonical_json FROM plans", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, p.canonical_json().unwrap());
}

#[test]
fn different_plan_under_same_id_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    s.insert_plan(&plan(1)).unwrap();
    let items = vec![
        scull_core::PlanItem::from_candidate(&candidate(9, Tier::S0, Action::Delete)).unwrap(),
    ];
    let substitute = scull_core::Plan::new(
        scull_core::PlanParams {
            id: plan_id(1),
            scan_id: scan_id(1),
            created_unix: T0,
            ttl_secs: None,
            machine_id: "machine-a".into(),
            user: "dev".into(),
            origin: scull_core::PlanOrigin::Mcp,
        },
        items,
    )
    .unwrap();
    let err = s.insert_plan(&substitute).unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::PlanTampered);
    assert_eq!(s.get_plan(&plan_id(1)).unwrap().unwrap(), plan(1));
}

#[test]
fn tampered_plan_record_is_detected_on_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    s.insert_plan(&plan(1)).unwrap();
    // Someone edits the file with the triggers dropped.
    let c = raw(&path);
    c.execute_batch(
        "DROP TRIGGER plans_no_update; \
         UPDATE plans SET canonical_json = replace(canonical_json, '/work/p1', '/home/me');",
    )
    .unwrap();
    let err = s.get_plan(&plan_id(1)).unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::PlanTampered);
}

// ---- approvals ----------------------------------------------------------------------------

#[test]
fn approval_is_single_use() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    s.insert_approval(&approval_for(&p, T0 + 10, T0 + 600))
        .unwrap();
    assert!(
        s.consume_approval(p.id(), p.content_hash(), T0 + 30)
            .unwrap()
    );
    assert!(
        !s.consume_approval(p.id(), p.content_hash(), T0 + 31)
            .unwrap()
    );
    let a = s.get_approval(p.id()).unwrap().unwrap();
    assert_eq!(a.used_at, Some(T0 + 30));
    assert_eq!(a.method, ApprovalMethod::CliTty);
    // A second approval for the same plan is refused: a plan executes at most once.
    let err = s
        .insert_approval(&approval_for(&p, T0 + 40, T0 + 600))
        .unwrap_err();
    assert_eq!(rejected_kind(err), ErrorKind::Usage);
}

#[test]
fn approval_expiry_and_hash_binding_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    s.insert_approval(&approval_for(&p, T0 + 10, T0 + 300))
        .unwrap();
    // Wrong hash, before approval time, at/after expiry: all false, approval stays unused.
    assert!(!s.consume_approval(p.id(), &other_hash(), T0 + 20).unwrap());
    assert!(
        !s.consume_approval(p.id(), p.content_hash(), T0 + 5)
            .unwrap()
    );
    assert!(
        !s.consume_approval(p.id(), p.content_hash(), T0 + 300)
            .unwrap()
    );
    assert!(
        !s.consume_approval(&plan_id(2), p.content_hash(), T0 + 20)
            .unwrap()
    );
    assert_eq!(s.get_approval(p.id()).unwrap().unwrap().used_at, None);
    // Just before expiry works.
    assert!(
        s.consume_approval(p.id(), p.content_hash(), T0 + 299)
            .unwrap()
    );
}

#[test]
fn approval_insert_validation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1); // MCP plan: expires at T0 + 900
    let err = s
        .insert_approval(&approval_for(&p, T0, T0 + 60))
        .unwrap_err();
    assert_eq!(
        rejected_kind(err),
        ErrorKind::NotFound,
        "plan must be recorded first"
    );
    s.insert_plan(&p).unwrap();

    let mut wrong = approval_for(&p, T0 + 1, T0 + 60);
    wrong.content_hash = other_hash();
    assert_eq!(
        rejected_kind(s.insert_approval(&wrong).unwrap_err()),
        ErrorKind::PlanTampered
    );
    assert_eq!(
        rejected_kind(
            s.insert_approval(&approval_for(&p, T0 + 1, p.expires_unix() + 1))
                .unwrap_err()
        ),
        ErrorKind::Usage,
        "approval cannot outlive the plan"
    );
    assert_eq!(
        rejected_kind(
            s.insert_approval(&approval_for(&p, p.expires_unix(), p.expires_unix() + 5))
                .unwrap_err()
        ),
        ErrorKind::PlanExpired
    );
    assert_eq!(
        rejected_kind(
            s.insert_approval(&approval_for(&p, T0 + 9, T0 + 9))
                .unwrap_err()
        ),
        ErrorKind::Usage
    );
    assert!(s.get_approval(p.id()).unwrap().is_none());
}

// ---- operations and audit -----------------------------------------------------------------

#[test]
fn audit_rows_record_hash_approver_method_and_bytes() {
    let (_dir, path, _s) = populated();
    let row: (String, String, String, String, i64, i64) = raw(&path)
        .query_row(
            "SELECT o.content_hash, o.approver, o.method, a.outcome, a.predicted_bytes, \
             a.freed_bytes FROM audit a JOIN operations o USING (operation_id) \
             WHERE a.kind = 'item'",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row.0, plan(1).content_hash().as_str());
    assert_eq!(
        (row.1.as_str(), row.2.as_str(), row.3.as_str()),
        ("dev", "cli_tty", "removed")
    );
    assert_eq!(row.4 - row.5, 1);
}

#[test]
fn operation_must_match_recorded_plan_hash() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    let err = s
        .begin_operation(
            &OperationStart {
                operation_id: OperationId::from_suffix("O1").unwrap(),
                plan_id: p.id().clone(),
                content_hash: other_hash(),
                approver: "dev".into(),
                method: ApprovalMethod::Desktop,
                started_at: T0,
            },
            &TestPerms::default(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");
}

// ---- scans and items ----------------------------------------------------------------------

#[test]
fn scan_lifecycle_with_rollups_and_items() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    start_scan(&mut s, 1);
    s.upsert_items(&scan_id(1), T0 + 5, &candidates()).unwrap();
    s.record_scan_finish(&ScanFinish {
        scan_id: scan_id(1),
        finished_at: T0 + 10,
        status: ScanStatus::Completed,
        totals: ScanTotals {
            entries_scanned: 1000,
            candidates: 3,
            logical_bytes: 3000,
            allocated_bytes: 3072,
            reclaimable_bytes: 2706,
            errors: 0,
        },
        rollups: vec![Rollup {
            category: Category::ProjectArtifact,
            tier: Tier::S0,
            ecosystem: Some("rust".into()),
            items: 1,
            logical_bytes: 1000,
            allocated_bytes: 1024,
            reclaimable_bytes: 901,
        }],
    })
    .unwrap();
    // Finishing twice is refused.
    let again = s.record_scan_finish(&ScanFinish {
        scan_id: scan_id(1),
        finished_at: T0 + 11,
        status: ScanStatus::Failed,
        totals: ScanTotals::default(),
        rollups: vec![],
    });
    assert_eq!(rejected_kind(again.unwrap_err()), ErrorKind::NotFound);

    // A later scan updates last-seen and sizes but keeps first-seen.
    s.record_scan_start(&scull_storage::ScanStart {
        scan_id: scan_id(2),
        started_at: T0 + 100,
        roots: vec![FsPath::from("/work")],
    })
    .unwrap();
    let mut grown = candidates();
    grown[0].sizes.logical = 5000;
    grown[0].tier = Tier::S0;
    s.upsert_items(&scan_id(2), T0 + 105, &grown[..1]).unwrap();

    let c = raw(&path);
    let (first, last, first_scan, last_scan, logical, category): (
        i64,
        i64,
        String,
        String,
        i64,
        String,
    ) = c
        .query_row(
            "SELECT first_seen_at, last_seen_at, first_seen_scan, last_seen_scan, \
             logical_bytes, category FROM items WHERE item_id = ?1",
            [grown[0].id.as_str()],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!((first, last), (T0 + 5, T0 + 105));
    assert_eq!(
        (first_scan.as_str(), last_scan.as_str()),
        ("scan_S1", "scan_S2")
    );
    assert_eq!(logical, 5000);
    assert_eq!(category, "project_artifact");
    let (status, rollup_tier): (String, String) = c
        .query_row(
            "SELECT s.status, r.tier FROM scans s JOIN scan_rollups r USING (scan_id)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), rollup_tier.as_str()), ("completed", "S0"));
}

#[test]
fn items_require_a_recorded_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    assert!(s.upsert_items(&scan_id(7), T0, &candidates()).is_err());
}

/// `open_read_only` (desktop History, SAFE-10) reads a current database, refuses every write,
/// and never creates, migrates or changes a file.
#[test]
fn open_read_only_reads_but_never_writes_or_creates() {
    use scull_storage::StoreOptions;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scull.db");
    let opts = StoreOptions::default();

    // Missing: refused, nothing created.
    let e = Store::open_read_only(&path, &opts).unwrap_err();
    assert_eq!(rejected_kind(e), ErrorKind::NotFound);
    assert!(!path.exists());
    // A folder at the path: refused.
    std::fs::create_dir(dir.path().join("dir.db")).unwrap();
    let e = Store::open_read_only(&dir.path().join("dir.db"), &opts).unwrap_err();
    assert_eq!(rejected_kind(e), ErrorKind::Config);

    // A current database with one plan.
    let mut store = Store::open(&path, &TestPerms::default()).unwrap();
    let plan = plan(1);
    store.insert_plan(&plan).unwrap();
    drop(store);
    let before = std::fs::read(&path).unwrap();

    let ro = Store::open_read_only(&path, &opts).unwrap();
    assert_eq!(ro.get_plan(plan.id()).unwrap().unwrap().id(), plan.id());
    assert!(ro.operations(0).unwrap().is_empty());
    assert!(ro.backups_taken().is_empty());
    // Every write is refused by SQLite.
    let mut ro = ro;
    assert!(ro.insert_plan(&plan_with_items(2, 1)).is_err());
    drop(ro);
    assert_eq!(std::fs::read(&path).unwrap(), before, "byte-identical");

    // An older schema is not migrated.
    let old = dir.path().join("old.db");
    std::fs::copy(&path, &old).unwrap();
    {
        let c = rusqlite::Connection::open(&old).unwrap();
        c.pragma_update(None, "user_version", 1).unwrap();
    }
    let old_bytes = std::fs::read(&old).unwrap();
    let e = Store::open_read_only(&old, &opts).unwrap_err();
    assert_eq!(rejected_kind(e), ErrorKind::Config);
    assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
}
