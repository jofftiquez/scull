//! The `history` command reads scull's database without ever creating, migrating or writing it,
//! and every answer validates against the committed `history` schema.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{
    Action, Candidate, Category, FileIdentity, Flags, FsPath, ItemId, OperationId, Plan, PlanId,
    PlanItem, PlanOrigin, PlanParams, ReasonCode, Regeneration, RuleRef, ScanId, Sizes, Tier,
    TierReason,
};
use scull_desktop_bridge::history::{
    AuditLineKind, HistoryReadState, ItemOutcome, OperationKind, OperationStatus, read_history,
};
use scull_desktop_bridge::{Session, Setup};
use scull_storage::{
    ApprovalMethod, DefaultPermissions, ItemOutcome as StoredOutcome, ItemRecord, OperationEnd,
    OperationFinish, OperationStart, Store,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn validate(v: &Value) {
    let path = desktop().join("schemas/history.schema.json");
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "history: {errors:?}\n{v:#}");
}

fn session(data: &Path) -> Session {
    Session::new(Setup {
        home: None,
        config_file: None,
        data_dir: Some(data.to_path_buf()),
        env: Arc::new(|_| None),
        record_history: true,
        git: false,
        detect_synced_roots: false,
        allow_network_roots: true,
        in_use: false,
    })
}

/// `after` holds every file of `before` with the same bytes, except SQLite's WAL index
/// (`-shm`), which any reader may create or update; the only other new file allowed is an
/// empty `-wal` (a reader writes no frames).
fn assert_unchanged(before: &[(PathBuf, Vec<u8>)], after: &[(PathBuf, Vec<u8>)]) {
    let shm = |p: &Path| p.to_string_lossy().ends_with("-shm");
    for (p, bytes) in before.iter().filter(|(p, _)| !shm(p)) {
        let now = after.iter().find(|(q, _)| q == p).map(|(_, b)| b);
        assert_eq!(now, Some(bytes), "{} changed or vanished", p.display());
    }
    for (p, bytes) in after {
        if before.iter().any(|(q, _)| q == p) || shm(p) {
            continue;
        }
        assert!(
            p.to_string_lossy().ends_with("-wal") && bytes.is_empty(),
            "{} was created ({} bytes)",
            p.display(),
            bytes.len()
        );
    }
}

/// Every file under `dir` with its bytes.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| (e.path(), std::fs::read(e.path()).unwrap_or_default()))
        .collect();
    out.sort();
    out
}

#[test]
fn no_database_reads_empty_and_creates_nothing() {
    let data = tempfile::tempdir().unwrap();
    let v = session(data.path()).history(1_900_000_000);
    assert_eq!(v.state, HistoryReadState::NoDatabase);
    assert!(v.operations.is_empty());
    assert!(
        v.audit_api,
        "this core reads operations (Phase 2 audit API)"
    );
    validate(&serde_json::to_value(&v).unwrap());
    assert!(
        std::fs::read_dir(data.path()).unwrap().next().is_none(),
        "history must not create the database"
    );
}

#[test]
fn current_database_is_read_without_changing_a_byte() {
    let data = tempfile::tempdir().unwrap();
    let db = data.path().join(scull_storage::DB_FILE_NAME);
    drop(scull_storage::Store::open(&db, &scull_storage::DefaultPermissions).unwrap());
    let before = snapshot(data.path());

    let v = session(data.path()).history(1_900_000_000);
    assert_eq!(v.state, HistoryReadState::Ok, "{v:?}");
    assert_eq!(v.now, 1_900_000_000);
    // Nothing recorded an operation yet.
    assert!(v.audit_api);
    assert!(v.operations.is_empty());
    validate(&serde_json::to_value(&v).unwrap());

    assert_unchanged(&before, &snapshot(data.path()));
}

#[test]
fn foreign_or_damaged_files_are_unavailable_and_untouched() {
    for (bytes, needle) in [
        (b"not a database at all, just text".to_vec(), "SQLite"),
        (
            {
                let mut h = vec![0u8; 128];
                h[..16].copy_from_slice(b"SQLite format 3\0");
                h[60..64].copy_from_slice(&1u32.to_be_bytes());
                h[68..72].copy_from_slice(&0x0BAD_F00Du32.to_be_bytes());
                h
            },
            "another application",
        ),
        (
            {
                let mut h = vec![0u8; 128];
                h[..16].copy_from_slice(b"SQLite format 3\0");
                h[60..64].copy_from_slice(&999u32.to_be_bytes());
                h[68..72].copy_from_slice(&scull_storage::APPLICATION_ID.to_be_bytes());
                h
            },
            "schema version 999",
        ),
    ] {
        let data = tempfile::tempdir().unwrap();
        let db = data.path().join(scull_storage::DB_FILE_NAME);
        std::fs::write(&db, &bytes).unwrap();
        let v = read_history(Some(data.path()), 5);
        assert_eq!(v.state, HistoryReadState::Unavailable, "{v:?}");
        let detail = v.detail.clone().unwrap();
        assert!(detail.contains(needle), "{detail}");
        validate(&serde_json::to_value(&v).unwrap());
        assert_eq!(std::fs::read(&db).unwrap(), bytes, "file untouched");
        assert_eq!(snapshot(data.path()).len(), 1, "no -wal/-shm created");
    }
}

#[cfg(unix)]
#[test]
fn a_linked_database_is_not_followed() {
    let data = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let real = elsewhere.path().join("real.db");
    drop(scull_storage::Store::open(&real, &scull_storage::DefaultPermissions).unwrap());
    std::os::unix::fs::symlink(&real, data.path().join(scull_storage::DB_FILE_NAME)).unwrap();
    let v = read_history(Some(data.path()), 5);
    assert_eq!(v.state, HistoryReadState::Unavailable);
    assert!(v.detail.unwrap().contains("not a regular file"));
}

fn candidate(n: u128, tier: Tier, action: Action, path: &str) -> Candidate {
    let identity = FileIdentity::new(7, n);
    Candidate {
        id: ItemId::derive(identity, path.as_bytes()),
        path: FsPath::from(path),
        identity,
        category: if matches!(action, Action::ToolCommand { .. }) {
            Category::PackageCache
        } else {
            Category::ProjectArtifact
        },
        artifact: "test".into(),
        rule: RuleRef {
            id: "rust.target".into(),
            version: 1,
        },
        project: None,
        tier,
        reasons: vec![TierReason::new(tier, ReasonCode::Regenerable, "test")],
        evidence: vec![],
        sizes: Sizes {
            logical: 1000,
            allocated: 1024,
            reclaimable: scull_core::Reclaimable {
                bytes: 900 + n as u64,
                confidence: scull_core::Confidence::High,
                shared_bytes: 0,
            },
        },
        flags: Flags::default(),
        regeneration: Regeneration::default(),
        action,
    }
}

fn plan(suffix: &str, origin: PlanOrigin, at: i64, cands: &[Candidate]) -> Plan {
    Plan::new(
        PlanParams {
            id: PlanId::from_suffix(suffix).unwrap(),
            scan_id: ScanId::from_suffix("S1").unwrap(),
            created_unix: at,
            ttl_secs: None,
            machine_id: "machine-a".into(),
            user: "dev".into(),
            origin,
        },
        cands
            .iter()
            .map(|c| PlanItem::from_candidate(c).unwrap())
            .collect(),
    )
    .unwrap()
}

/// Records `plan` as an operation whose items get `outcomes` (in plan order; `None`: not
/// attempted, only with a cancelled end).
fn record(
    store: &mut Store,
    plan: &Plan,
    op: &str,
    at: i64,
    outcomes: &[Option<StoredOutcome>],
    end: OperationEnd,
    measured: Option<i64>,
) {
    store.insert_plan(plan).unwrap();
    let mut audit = store
        .begin_operation(
            &OperationStart {
                operation_id: OperationId::from_suffix(op).unwrap(),
                plan_id: plan.id().clone(),
                content_hash: plan.content_hash().clone(),
                approver: "dev".into(),
                method: ApprovalMethod::CliInteractive,
                started_at: at,
            },
            &DefaultPermissions,
        )
        .unwrap();
    for (i, (pi, o)) in plan.items().iter().zip(outcomes).enumerate() {
        let Some(o) = o else { continue };
        let mut r = ItemRecord::new(pi.item_id.clone(), at + 1 + i as i64, *o);
        match o {
            StoredOutcome::Removed => {
                r.freed_bytes = Some(800);
                r.entries_removed = Some(3);
                r.entries_kept = Some(0);
            }
            StoredOutcome::ToolRun => r.freed_bytes = Some(500),
            StoredOutcome::Quarantined => r.moved_bytes = Some(pi.reclaimable_bytes),
            StoredOutcome::Skipped => r = r.with_reason("changed"),
            _ => {}
        }
        store.record_item(&mut audit, &r).unwrap();
    }
    store
        .finish_operation(
            &mut audit,
            &OperationFinish {
                finished_at: at + 20,
                end,
                measured_delta_bytes: measured,
            },
        )
        .unwrap();
}

/// Every file below `dir` (recursively) with its bytes.
fn deep_snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            out.extend(deep_snapshot(&p));
        } else {
            out.push((p.clone(), std::fs::read(&p).unwrap()));
        }
    }
    out.sort();
    out
}

/// The operations the Phase 2 core recorded are read back through the audit API, newest
/// first, with kind, items in plan order, outcomes, the audit log and an intact chain; the
/// answer validates against the schema and no file in the data folder changes.
#[test]
fn recorded_operations_are_listed_newest_first_read_only() {
    let data = tempfile::tempdir().unwrap();
    let db = data.path().join(scull_storage::DB_FILE_NAME);
    let mut store = Store::open(&db, &DefaultPermissions).unwrap();
    let t0 = 1_800_000_000;
    let cleanup = plan(
        "P1",
        PlanOrigin::Cli,
        t0,
        &[
            candidate(1, Tier::S0, Action::Delete, "/work/a/target"),
            candidate(2, Tier::S1, Action::Quarantine, "/work/b/target"),
            candidate(3, Tier::S1, Action::Quarantine, "/work/c/target"),
        ],
    );
    record(
        &mut store,
        &cleanup,
        "O1",
        t0 + 10,
        &[
            Some(StoredOutcome::Removed),
            Some(StoredOutcome::Quarantined),
            None,
        ],
        OperationEnd::Cancelled,
        Some(700),
    );
    let quick = plan(
        "P2",
        PlanOrigin::Cli,
        t0 + 100,
        &[candidate(
            4,
            Tier::S1,
            Action::ToolCommand {
                tool: "/usr/bin/npm".into(),
                args: vec![
                    "cache".into(),
                    "verify".into(),
                    "--cache".into(),
                    "/home/u/.npm".into(),
                ],
            },
            "/home/u/.npm/_cacache",
        )],
    );
    record(
        &mut store,
        &quick,
        "O2",
        t0 + 110,
        &[Some(StoredOutcome::ToolRun)],
        OperationEnd::Normal,
        None,
    );
    drop(store);
    let before = deep_snapshot(data.path());

    let v = session(data.path()).history(t0 + 1000);
    assert_eq!(v.state, HistoryReadState::Ok, "{v:?}");
    assert!(v.audit_api);
    validate(&serde_json::to_value(&v).unwrap());
    let ids: Vec<&str> = v
        .operations
        .iter()
        .map(|o| o.operation_id.as_str())
        .collect();
    assert_eq!(ids, ["op_O2", "op_O1"], "newest first");

    let q = &v.operations[0];
    assert_eq!(q.kind, OperationKind::QuickClean);
    assert_eq!(q.status, OperationStatus::Completed);
    assert_eq!(q.method, "cli_interactive");
    assert_eq!(q.origin.as_deref(), Some("CLI"));
    assert_eq!(q.items.len(), 1);
    assert_eq!(q.items[0].action, "tool");
    assert_eq!(q.items[0].label.as_deref(), Some("npm cache verify"));
    assert_eq!(q.items[0].outcome, ItemOutcome::ToolRun);
    assert_eq!(q.items[0].freed_bytes, Some(500));
    assert!(q.chain.intact, "{:?}", q.chain);

    let c = &v.operations[1];
    assert_eq!(c.kind, OperationKind::Cleanup);
    assert_eq!(c.status, OperationStatus::Cancelled);
    assert_eq!(c.started_at, t0 + 10);
    assert_eq!(c.finished_at, Some(t0 + 30));
    assert_eq!(c.measured_delta_bytes, Some(700));
    let outcomes: Vec<ItemOutcome> = c.items.iter().map(|i| i.outcome).collect();
    assert_eq!(
        outcomes,
        [
            ItemOutcome::Removed,
            ItemOutcome::Quarantined,
            ItemOutcome::Skipped
        ],
        "plan order"
    );
    let planned: Vec<&str> = cleanup.items().iter().map(|i| i.path.as_str()).collect();
    let shown: Vec<&str> = c.items.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(shown, planned);
    assert_eq!(c.items[0].tier, Some(cleanup.items()[0].tier));
    assert_eq!(c.items[0].freed_bytes, Some(800));
    assert_eq!(c.items[2].reason.as_deref(), Some("not_attempted"));
    assert!(c.chain.intact, "{:?}", c.chain);
    assert_eq!(c.chain.rows, 4, "started, two items, finished");
    let kinds: Vec<AuditLineKind> = c.log.iter().map(|l| l.kind).collect();
    assert_eq!(
        kinds,
        [
            AuditLineKind::Approval,
            AuditLineKind::Action,
            AuditLineKind::Action,
            AuditLineKind::Measured
        ]
    );
    assert!(c.log[0].text.contains("plan_P1"), "{:?}", c.log[0]);

    // The database and the audit mirrors are byte-identical.
    assert_unchanged(&before, &deep_snapshot(data.path()));
}
