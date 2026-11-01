//! Schema v4: the quarantine manifest of record (SAFE-16) and per-operation targets (SAFE-09).
//! Temp dirs only.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use scull_core::{ErrorKind, FileIdentity, FsPath, OperationId, Plan};
use scull_storage::{
    ApprovalMethod, NewQuarantineEntry, OperationAudit, OperationEnd, OperationFinish,
    OperationStart, Store, StoreError, identity_key, parse_identity_key,
};

fn op_id(s: &str) -> OperationId {
    OperationId::from_suffix(s).unwrap()
}

fn begin(s: &mut Store, p: &Plan, op: &str, at: i64) -> OperationAudit {
    s.begin_operation(
        &OperationStart {
            operation_id: op_id(op),
            plan_id: p.id().clone(),
            content_hash: p.content_hash().clone(),
            approver: "dev".into(),
            method: ApprovalMethod::CliInteractive,
            started_at: at,
        },
        &TestPerms::default(),
    )
    .unwrap()
}

/// Ends an operation (cancelled: its items need no outcome).
fn finish(s: &mut Store, audit: &mut OperationAudit, at: i64) {
    s.finish_operation(
        audit,
        &OperationFinish {
            finished_at: at,
            end: OperationEnd::Cancelled,
            measured_delta_bytes: None,
        },
    )
    .unwrap();
}

fn entry(p: &Plan, op: &str, n: u128) -> NewQuarantineEntry {
    NewQuarantineEntry {
        operation_id: op_id(op),
        item_id: p.items()[0].item_id.clone(),
        original_path: FsPath::from("/work/p/node_modules"),
        quarantine_path: FsPath::from(format!("/work/.scull-quarantine/op_{op}/{n:04}").as_str()),
        entry_identity: FileIdentity::new(7, 1000 + n),
        entry_kind: "dir".into(),
        parent_identity: FileIdentity::new(7, 99),
        bytes: 4096,
        logical_bytes: 1234,
        quarantined_at: T0 + 20,
        expires_at: Some(T0 + 20 + 7 * 86_400),
    }
}

fn rejected(e: StoreError) -> ErrorKind {
    match e {
        StoreError::Rejected { kind, .. } => kind,
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn identity_keys_round_trip() {
    let id = FileIdentity::new(u64::MAX, u128::MAX - 5);
    assert_eq!(parse_identity_key(&identity_key(id)), Some(id));
    assert_eq!(parse_identity_key("nonsense"), None);
}

#[test]
fn manifest_entries_are_recorded_and_marked_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    begin(&mut s, &p, "O1", T0 + 10);

    let a = s.insert_quarantine_entry(&entry(&p, "O1", 1)).unwrap();
    let b = s.insert_quarantine_entry(&entry(&p, "O1", 2)).unwrap();
    let got = s.quarantine_entry(a).unwrap().unwrap();
    assert_eq!(got.entry, entry(&p, "O1", 1));
    assert!(got.is_held());
    assert_eq!(s.quarantine_held().unwrap().len(), 2);

    s.mark_quarantine_purged(a, T0 + 100, 4096).unwrap();
    s.mark_quarantine_restored(b, T0 + 101, &FsPath::from("/work/p/node_modules"))
        .unwrap();
    assert_eq!(
        s.quarantine_entry(a).unwrap().unwrap().purged,
        Some((T0 + 100, 4096))
    );
    assert!(s.quarantine_held().unwrap().is_empty());
    // Set once: a purged entry is never restored, a restored one never purged or re-restored.
    for e in [
        s.mark_quarantine_restored(a, T0 + 102, &FsPath::from("/x"))
            .unwrap_err(),
        s.mark_quarantine_purged(b, T0 + 102, 1).unwrap_err(),
        s.mark_quarantine_purged(a, T0 + 102, 1).unwrap_err(),
    ] {
        assert_eq!(rejected(e), ErrorKind::Usage);
    }
}

#[test]
fn manifest_paths_and_facts_are_immutable_and_rows_never_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    begin(&mut s, &p, "O1", T0 + 10);
    let id = s.insert_quarantine_entry(&entry(&p, "O1", 1)).unwrap();
    drop(s);

    let c = raw(&path);
    for sql in [
        "UPDATE quarantine_manifest SET original_path = '/home/me/.ssh'",
        "UPDATE quarantine_manifest SET quarantine_path = '/tmp/x'",
        "UPDATE quarantine_manifest SET entry_identity = '7:1'",
        "UPDATE quarantine_manifest SET parent_identity = '7:1'",
        "UPDATE quarantine_manifest SET entry_kind = 'file'",
        "UPDATE quarantine_manifest SET bytes = 1",
        "UPDATE quarantine_manifest SET expires_at = NULL",
        // A purge records its bytes together with its time.
        "UPDATE quarantine_manifest SET purged_at = 1",
        "DELETE FROM quarantine_manifest",
    ] {
        assert!(c.execute(sql, []).is_err(), "{sql} must be refused");
    }
    // Entries start held and carry the integrity facts.
    assert!(
        c.execute(
            "INSERT INTO quarantine_manifest (operation_id, item_id, original_path, \
             quarantine_path, bytes, quarantined_at) VALUES ('op_O1', 'x', '/a', '/b', 1, 1)",
            [],
        )
        .is_err()
    );
    drop(c);
    let s = open(&path);
    assert_eq!(
        s.quarantine_entry(id).unwrap().unwrap().entry,
        entry(&p, "O1", 1)
    );
}

/// SAFE-09 with a watermark: what counts is whether an operation was recorded after the plan's
/// scan started, not what its clock says. The timeline of one plan, all in the same second as
/// its creation unless noted:
///
/// * `BEFORE` ran before the scan started: ignored, even in the plan's creation second (the
///   old one-second rule wrongly invalidated the plan here);
/// * `DURING` ran while the scan walked, `BETWEEN` after the scan but before the plan was
///   created, `AFTER` after the plan: all count;
/// * `SKEWED` was recorded after the scan started but its clock reads an hour earlier: counts.
#[test]
fn operations_touching_with_a_watermark_counts_every_operation_recorded_after_the_scan_started() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1); // created at T0
    s.insert_plan(&p).unwrap();
    let item = p.items()[0].item_id.clone();
    // Each operation runs to its end (a still-running one is the next test).
    let target = |s: &mut Store, op: &str, id: FileIdentity, at: i64| {
        let mut audit = begin(s, &p, op, at);
        s.record_operation_targets(&op_id(op), &[(item.clone(), id)], at)
            .unwrap();
        finish(s, &mut audit, at);
    };
    let ids: Vec<FileIdentity> = (1..=5).map(|n| FileIdentity::new(9, n)).collect();

    assert_eq!(s.operations_watermark().unwrap(), 0, "no operation yet");
    target(&mut s, "BEFORE", ids[0], T0);
    let watermark = s.operations_watermark().unwrap(); // the scan starts
    assert!(watermark > 0);
    target(&mut s, "DURING", ids[1], T0);
    target(&mut s, "BETWEEN", ids[2], T0);
    target(&mut s, "AFTER", ids[3], T0);
    target(&mut s, "SKEWED", ids[4], T0 - 3_600);

    let touching = |only: &[FileIdentity]| {
        s.operations_touching(only, T0, Some(watermark), None)
            .unwrap()
    };
    assert!(touching(&ids[..1]).is_empty(), "before the scan: ignored");
    for (n, op) in ["DURING", "BETWEEN", "AFTER", "SKEWED"].iter().enumerate() {
        assert_eq!(touching(&ids[n + 1..n + 2]), [op_id(op)], "{op}");
    }
    assert_eq!(touching(&ids).len(), 4);
    // Without a watermark (older plans) the clock decides: the same-second `BEFORE` counts
    // (protective) and the skewed one does not.
    let old = s.operations_touching(&ids, T0, None, None).unwrap();
    assert!(old.contains(&op_id("BEFORE")) && !old.contains(&op_id("SKEWED")));
    // An operation never invalidates itself.
    assert!(
        s.operations_touching(&ids[3..4], T0, Some(watermark), Some(&op_id("AFTER")))
            .unwrap()
            .is_empty()
    );
}

/// SAFE-09: an operation that began before the scan but is still running when the
/// watermark is read may change its targets after the scan observed them. The watermark stops
/// below it, so it invalidates the plan (here: it later touches item Y).
#[test]
fn an_operation_still_running_when_the_scan_starts_counts_as_touching() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    let item = p.items()[0].item_id.clone();
    let (x, y) = (FileIdentity::new(9, 1), FileIdentity::new(9, 2));

    // DONE finished before the scan; RUNNING began before it and is still running.
    let mut done = begin(&mut s, &p, "DONE", T0);
    s.record_operation_targets(&op_id("DONE"), &[(item.clone(), x)], T0)
        .unwrap();
    finish(&mut s, &mut done, T0);
    let mut running = begin(&mut s, &p, "RUNNING", T0);
    s.record_operation_targets(&op_id("RUNNING"), &[(item.clone(), y)], T0)
        .unwrap();

    let watermark = s.operations_watermark().unwrap(); // the scan starts
    // ... the scan observes Y, RUNNING then changes it and finishes.
    finish(&mut s, &mut running, T0 + 5);
    assert_eq!(
        s.operations_touching(&[y], T0 + 10, Some(watermark), None)
            .unwrap(),
        [op_id("RUNNING")],
        "a running operation is never below the watermark"
    );
    assert!(
        s.operations_touching(&[x], T0 + 10, Some(watermark), None)
            .unwrap()
            .is_empty(),
        "a finished one before the scan is"
    );
    // With nothing running, the watermark is the last operation again.
    assert!(s.operations_watermark().unwrap() > watermark);
}

#[test]
fn operations_touching_finds_later_operations_on_the_same_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let p = plan(1);
    s.insert_plan(&p).unwrap();
    let a = FileIdentity::new(7, 1);
    let b = FileIdentity::new(7, 2);
    begin(&mut s, &p, "O1", T0 + 10);
    s.record_operation_targets(&op_id("O1"), &[(p.items()[0].item_id.clone(), a)], T0 + 10)
        .unwrap();
    begin(&mut s, &p, "O2", T0 + 50);
    s.record_operation_targets(&op_id("O2"), &[(p.items()[0].item_id.clone(), b)], T0 + 50)
        .unwrap();

    assert_eq!(
        s.operations_touching(&[a], T0, None, None).unwrap(),
        [op_id("O1")]
    );
    assert_eq!(
        s.operations_touching(&[a, b], T0, None, None).unwrap(),
        [op_id("O1"), op_id("O2")]
    );
    // Only operations started at or after the plan's creation count.
    assert_eq!(
        s.operations_touching(&[a, b], T0 + 20, None, None).unwrap(),
        [op_id("O2")]
    );
    // An operation never invalidates itself.
    assert!(
        s.operations_touching(&[b], T0, None, Some(&op_id("O2")))
            .unwrap()
            .is_empty()
    );
    drop(s);
    let c = raw(&path);
    for sql in [
        "UPDATE operation_targets SET identity = '1:1'",
        "DELETE FROM operation_targets",
    ] {
        assert!(c.execute(sql, []).is_err(), "{sql} must be refused");
    }
}
