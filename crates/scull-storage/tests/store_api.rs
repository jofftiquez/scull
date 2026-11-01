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
            operations_watermark: None,
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

/// What a human can approve (§13.3): unexpired plans of one origin without an approval
/// record, newest first; approved, expired and other-origin plans are left out.
#[test]
fn plans_awaiting_approval_lists_unapproved_unexpired_plans_of_one_origin() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    for n in 1..=3 {
        s.insert_plan(&plan(n)).unwrap();
    }
    let ids = |s: &scull_storage::Store, now| {
        s.plans_awaiting_approval(scull_core::PlanOrigin::Mcp, now, 10)
            .unwrap()
    };
    assert_eq!(ids(&s, T0 + 1), vec![plan_id(3), plan_id(2), plan_id(1)]);
    assert_eq!(
        s.plans_awaiting_approval(scull_core::PlanOrigin::Mcp, T0 + 1, 1)
            .unwrap(),
        vec![plan_id(3)]
    );
    assert!(
        s.plans_awaiting_approval(scull_core::PlanOrigin::Cli, T0 + 1, 10)
            .unwrap()
            .is_empty()
    );
    s.insert_approval(&approval_for(&plan(2), T0 + 10, T0 + 600))
        .unwrap();
    assert_eq!(ids(&s, T0 + 20), vec![plan_id(3), plan_id(1)]);
    // MCP plans expire after 15 minutes.
    assert!(ids(&s, T0 + 15 * 60).is_empty());
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

fn refusal_event(
    at: i64,
    reason: &str,
    plan: Option<scull_core::PlanId>,
) -> scull_storage::AgentAuditEvent {
    scull_storage::AgentAuditEvent {
        event_id: 0,
        at,
        kind: scull_storage::EventKind::ExecuteRefused,
        source: "mcp".into(),
        plan_id: plan,
        content_hash: None,
        reason: reason.into(),
        channel: "mcp".into(),
        machine_id: "machine-a".into(),
        user: "dev".into(),
        count: 1,
    }
}

/// Audit flooding: 1000 identical refusals within the merge window are one row counting
/// 1000; refusals naming 1000 made-up plan IDs fold together too; a recorded plan, another
/// reason, or the next window gets its own row. Listing is capped and newest-first by limit.
#[test]
fn repeated_agent_refusals_fold_into_bounded_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    for i in 0..1000 {
        s.record_agent_event(&refusal_event(
            T0 + i % 30,
            "not_approved",
            Some(p.id().clone()),
        ))
        .unwrap();
    }
    for i in 0..1000u32 {
        let made_up = scull_core::PlanId::from_suffix(&format!("fake{i}")).unwrap();
        s.record_agent_event(&refusal_event(T0 + 5, "unknown_plan", Some(made_up)))
            .unwrap();
    }
    s.record_agent_event(&refusal_event(T0 + 6, "invalid_arguments", None))
        .unwrap();
    s.record_agent_event(&refusal_event(T0 + 7, "invalid_arguments", None))
        .unwrap();
    assert_eq!(s.agent_event_rows(0).unwrap(), 3);
    let rows = s.agent_events(0, 100).unwrap();
    let got: Vec<(&str, u64)> = rows.iter().map(|e| (e.reason.as_str(), e.count)).collect();
    assert_eq!(
        got,
        vec![
            ("not_approved", 1000),
            ("unknown_plan", 1000),
            ("invalid_arguments", 2)
        ]
    );
    // The next window starts a new row.
    s.record_agent_event(&refusal_event(
        T0 + scull_storage::AGENT_EVENT_MERGE_SECS + 1,
        "not_approved",
        Some(p.id().clone()),
    ))
    .unwrap();
    assert_eq!(s.agent_event_rows(0).unwrap(), 4);
    // Capped listing: the most recent ones, oldest first.
    let last2 = s.agent_events(0, 2).unwrap();
    assert_eq!(last2.len(), 2);
    assert_eq!(last2[1].count, 1);
    assert_eq!(last2[1].reason, "not_approved");
}

fn revocation(p: &scull_core::Plan, at: i64) -> scull_storage::AgentAuditEvent {
    scull_storage::AgentAuditEvent {
        event_id: 0,
        at,
        kind: scull_storage::EventKind::ApprovalRevoked,
        source: "cli".into(),
        plan_id: Some(p.id().clone()),
        content_hash: Some(p.content_hash().clone()),
        reason: "revoked".into(),
        channel: "terminal".into(),
        machine_id: "machine-a".into(),
        user: "dev".into(),
        count: 1,
    }
}

/// #298: a revocation spends an unused, unexpired approval and records its audit event in
/// the same transaction; it never creates an approval, and a used, expired or mismatched
/// approval is not revoked (nothing written).
#[test]
fn revoking_spends_the_approval_and_audits_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    // No approval: nothing to revoke, none created, nothing audited.
    assert!(
        !s.revoke_approval(p.id(), p.content_hash(), T0 + 5, &revocation(&p, T0 + 5))
            .unwrap()
    );
    assert!(s.get_approval(p.id()).unwrap().is_none());
    s.insert_approval(&approval_for(&p, T0 + 10, T0 + 600))
        .unwrap();
    // Other content, before it was given, after it lapsed: refused.
    assert!(
        !s.revoke_approval(p.id(), &other_hash(), T0 + 20, &revocation(&p, T0 + 20))
            .unwrap()
    );
    assert!(
        !s.revoke_approval(p.id(), p.content_hash(), T0 + 5, &revocation(&p, T0 + 5))
            .unwrap()
    );
    assert!(
        !s.revoke_approval(
            p.id(),
            p.content_hash(),
            T0 + 600,
            &revocation(&p, T0 + 600)
        )
        .unwrap()
    );
    assert!(s.agent_events(0, 10_000).unwrap().is_empty());
    assert!(!s.approval_revoked(p.id()).unwrap());
    // Revoked once, then the Executor's consumption fails, and so does a second revocation.
    assert!(
        s.revoke_approval(p.id(), p.content_hash(), T0 + 30, &revocation(&p, T0 + 30))
            .unwrap()
    );
    assert_eq!(
        s.get_approval(p.id()).unwrap().unwrap().used_at,
        Some(T0 + 30)
    );
    assert!(s.approval_revoked(p.id()).unwrap());
    assert!(
        !s.consume_approval(p.id(), p.content_hash(), T0 + 31)
            .unwrap()
    );
    assert!(
        !s.revoke_approval(p.id(), p.content_hash(), T0 + 32, &revocation(&p, T0 + 32))
            .unwrap()
    );
    let events = s.agent_events(0, 10_000).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, scull_storage::EventKind::ApprovalRevoked);
    // A used approval is not "revoked" after the fact.
    let q = plan(2);
    s.insert_plan(&q).unwrap();
    s.insert_approval(&approval_for(&q, T0 + 10, T0 + 600))
        .unwrap();
    assert!(
        s.consume_approval(q.id(), q.content_hash(), T0 + 40)
            .unwrap()
    );
    assert!(
        !s.revoke_approval(q.id(), q.content_hash(), T0 + 41, &revocation(&q, T0 + 41))
            .unwrap()
    );
    assert!(!s.approval_revoked(q.id()).unwrap());
    // Only revoke_approval writes a revocation: a forged one through record_agent_event is
    // refused and changes nothing.
    let r = plan(3);
    s.insert_plan(&r).unwrap();
    s.insert_approval(&approval_for(&r, T0 + 10, T0 + 600))
        .unwrap();
    assert_eq!(
        rejected_kind(s.record_agent_event(&revocation(&r, T0 + 41)).unwrap_err()),
        ErrorKind::Usage
    );
    assert!(!s.approval_revoked(r.id()).unwrap());
    assert_eq!(s.get_approval(r.id()).unwrap().unwrap().used_at, None);
    // The audit must be a revocation of the same plan.
    let mut wrong = revocation(&q, T0 + 42);
    wrong.plan_id = Some(p.id().clone());
    assert_eq!(
        rejected_kind(
            s.revoke_approval(q.id(), q.content_hash(), T0 + 42, &wrong)
                .unwrap_err()
        ),
        ErrorKind::Usage
    );
}

/// #298: a revocation racing the Executor's consumption of the same approval, each through
/// its own connection: exactly one wins, every time, and the audit matches the winner.
#[test]
fn revocation_and_consumption_race_with_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let mut revoked_wins = 0;
    for n in 1..=25u32 {
        let p = plan(n);
        s.insert_plan(&p).unwrap();
        s.insert_approval(&approval_for(&p, T0 + 10, T0 + 600))
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let (pa, pb) = (p.clone(), p.clone());
        let (ba, bb) = (barrier.clone(), barrier);
        let (path_a, path_b) = (path.clone(), path.clone());
        let consume = std::thread::spawn(move || {
            let mut c = open(&path_a);
            ba.wait();
            c.consume_approval(pa.id(), pa.content_hash(), T0 + 30)
                .unwrap()
        });
        let revoke = std::thread::spawn(move || {
            let mut c = open(&path_b);
            bb.wait();
            c.revoke_approval(
                pb.id(),
                pb.content_hash(),
                T0 + 30,
                &revocation(&pb, T0 + 30),
            )
            .unwrap()
        });
        let (consumed, revoked) = (consume.join().unwrap(), revoke.join().unwrap());
        assert!(
            consumed ^ revoked,
            "plan {n}: consumed={consumed} revoked={revoked}"
        );
        assert_eq!(s.approval_revoked(p.id()).unwrap(), revoked);
        assert!(s.get_approval(p.id()).unwrap().unwrap().used_at.is_some());
        revoked_wins += usize::from(revoked);
    }
    assert_eq!(s.agent_events(0, 10_000).unwrap().len(), revoked_wins);
}

/// #299: the pending cap is enforced in the insert's own transaction: concurrent sessions
/// recording agent plans for one user get exactly `max_pending` in; approved, expired and
/// other users' plans don't count; non-agent plans are refused by this call.
#[test]
fn agent_plan_cap_holds_under_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (1..=8u32)
        .map(|n| {
            let (path, barrier) = (path.clone(), barrier.clone());
            std::thread::spawn(move || {
                let mut c = open(&path);
                barrier.wait();
                c.insert_agent_plan(&plan(n), T0 + 1, 3).unwrap()
            })
        })
        .collect();
    let inserted = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|i| *i)
        .count();
    assert_eq!(inserted, 3);
    assert_eq!(
        s.pending_agent_plans("machine-a", "dev", T0 + 1).unwrap(),
        3
    );
    assert_eq!(
        s.pending_agent_plans("machine-a", "other", T0 + 1).unwrap(),
        0
    );
    assert_eq!(
        s.pending_agent_plans("machine-b", "dev", T0 + 1).unwrap(),
        0
    );
    // An approved plan no longer counts; an expired one neither.
    let ids = s
        .plans_awaiting_approval(scull_core::PlanOrigin::Mcp, T0 + 1, 10)
        .unwrap();
    let first = s.get_plan(&ids[0]).unwrap().unwrap();
    s.insert_approval(&approval_for(&first, T0 + 2, T0 + 600))
        .unwrap();
    assert_eq!(
        s.pending_agent_plans("machine-a", "dev", T0 + 3).unwrap(),
        2
    );
    assert!(s.insert_agent_plan(&plan(20), T0 + 3, 3).unwrap());
    assert!(!s.insert_agent_plan(&plan(21), T0 + 3, 3).unwrap());
    assert!(
        s.get_plan(&plan_id(21)).unwrap().is_none(),
        "nothing recorded"
    );
    assert_eq!(
        s.pending_agent_plans("machine-a", "dev", T0 + 15 * 60)
            .unwrap(),
        0
    );
}

fn denial(
    at: i64,
    plan: &scull_core::Plan,
    reason: &str,
    source: &str,
) -> scull_storage::AgentAuditEvent {
    scull_storage::AgentAuditEvent {
        event_id: 0,
        at,
        kind: scull_storage::EventKind::ApprovalDenied,
        source: source.into(),
        plan_id: Some(plan.id().clone()),
        content_hash: Some(plan.content_hash().clone()),
        reason: reason.into(),
        channel: "terminal".into(),
        machine_id: "machine-a".into(),
        user: "dev".into(),
        count: 1,
    }
}

/// #299 review: a plan the human denied (`denied`, or too many wrong codes) no longer counts
/// toward the pending cap and leaves the approval list; a refused attempt (`no_terminal`,
/// what an agent's shell tool gets) does not free a slot.
#[test]
fn denied_plans_stop_counting_as_pending() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    for n in 1..=4 {
        s.insert_plan(&plan(n)).unwrap();
    }
    let pending = |s: &Store| s.pending_agent_plans("machine-a", "dev", T0 + 5).unwrap();
    let listed = |s: &Store| {
        s.plans_awaiting_approval(scull_core::PlanOrigin::Mcp, T0 + 5, 10)
            .unwrap()
    };
    assert_eq!(pending(&s), 4);
    s.record_agent_event(&denial(T0 + 1, &plan(1), "denied", "cli"))
        .unwrap();
    s.record_agent_event(&denial(T0 + 1, &plan(2), "wrong_code", "desktop"))
        .unwrap();
    s.record_agent_event(&denial(T0 + 1, &plan(3), "no_terminal", "cli"))
        .unwrap();
    s.record_agent_event(&denial(T0 + 1, &plan(4), "plan_expired", "cli"))
        .unwrap();
    assert_eq!(pending(&s), 2);
    assert_eq!(listed(&s), vec![plan_id(4), plan_id(3)]);
    // The cap counts the same way.
    assert!(s.insert_agent_plan(&plan(5), T0 + 5, 3).unwrap());
    assert!(!s.insert_agent_plan(&plan(6), T0 + 5, 3).unwrap());
}
