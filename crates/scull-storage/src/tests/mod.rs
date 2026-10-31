//! Crate-internal tests: the migration runner with an extra (test-only) migration, which the
//! public API cannot inject. Temp dirs only.
#![allow(clippy::unwrap_used)]

use crate::backup::utc_stamp;
use crate::migrate::{MIGRATIONS, Migration};
use crate::{DefaultPermissions, SCHEMA_VERSION, Store, StoreError, StoreOptions, list_backups};
use rusqlite::Connection;
use std::path::Path;

const V2_TEST: &str = "CREATE TABLE test_v2 (x INTEGER NOT NULL) STRICT; \
                       INSERT INTO meta (key, value) VALUES ('migrated', 'v2');";

fn migrations_with_v2() -> Vec<Migration> {
    let mut m = MIGRATIONS.to_vec();
    m.push(Migration::sql(V2_TEST));
    m
}

fn open_with(path: &Path, migrations: &[Migration]) -> crate::Result<Store> {
    Store::open_inner(
        path,
        &DefaultPermissions,
        &StoreOptions::default(),
        migrations,
    )
}

fn meta(path: &Path, key: &str) -> Option<String> {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .ok()
}

#[test]
fn fx_data_01_backup_is_taken_before_migration_and_migration_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::DB_FILE_NAME);
    {
        let s = open_with(&path, MIGRATIONS).unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
        s.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('before', 'kept')",
                [],
            )
            .unwrap();
    }

    let v2 = migrations_with_v2();
    let s = open_with(&path, &v2).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION + 1);
    assert_eq!(s.backups_taken().len(), 1);
    let backup = s.backups_taken()[0].clone();
    assert_eq!(list_backups(&path), vec![backup.clone()]);
    let name = backup.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("scull.db.backup-"), "{name}");
    assert!(name.ends_with(&format!("-v{SCHEMA_VERSION}")), "{name}");

    // The backup is the pre-migration state: old version, old data, no v2 changes.
    let b = Connection::open(&backup).unwrap();
    let v: u32 = b
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    assert_eq!(meta(&backup, "before").as_deref(), Some("kept"));
    assert_eq!(meta(&backup, "migrated"), None);
    // The migrated database has both.
    assert_eq!(meta(&path, "before").as_deref(), Some("kept"));
    assert_eq!(meta(&path, "migrated").as_deref(), Some("v2"));
    drop(s);

    // Already at v2: nothing to do, no new backup.
    let s = open_with(&path, &v2).unwrap();
    assert!(s.backups_taken().is_empty());
    assert_eq!(list_backups(&path).len(), 1);
}

#[test]
fn fx_data_01_older_binary_refuses_the_migrated_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::DB_FILE_NAME);
    drop(open_with(&path, &migrations_with_v2()).unwrap());
    let before = std::fs::read(&path).unwrap();
    let err = open_with(&path, MIGRATIONS).unwrap_err();
    assert!(
        matches!(err, StoreError::SchemaTooNew { found, supported, .. }
            if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn failed_migration_rolls_back_and_keeps_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::DB_FILE_NAME);
    drop(open_with(&path, MIGRATIONS).unwrap());
    let mut broken = MIGRATIONS.to_vec();
    broken.push(Migration::sql(
        "CREATE TABLE half (x INTEGER); THIS IS NOT SQL;",
    ));
    assert!(open_with(&path, &broken).is_err());
    // Still at the old version with no partial changes, and the backup exists.
    let s = open_with(&path, MIGRATIONS).unwrap();
    assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    let half: i64 = s
        .conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'half'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(half, 0);
    assert_eq!(list_backups(&path).len(), 1);
}

#[test]
fn connection_pragmas_are_applied() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("x.db"), &DefaultPermissions).unwrap();
    let q = |name: &str| -> i64 { s.conn.pragma_query_value(None, name, |r| r.get(0)).unwrap() };
    assert_eq!(q("foreign_keys"), 1);
    assert_eq!(q("synchronous"), 2, "FULL");
    assert_eq!(q("busy_timeout"), 5000);
    let mode: String = s
        .conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn utc_stamp_formats_known_instants() {
    assert_eq!(utc_stamp(0), "19700101T000000Z");
    assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
    assert_eq!(utc_stamp(1_800_000_000), "20270115T080000Z");
    assert_eq!(utc_stamp(-1), "19691231T235959Z");
}

// ---- v1 → v2 (history) ---------------------------------------------------------------------

const ITEM_A: &str = "0123456789abcdef0123456789abcdef";

/// A v1 database with one finished scan, one item, free-space samples and a plan.
fn v1_database(path: &Path) {
    let s = open_with(path, &MIGRATIONS[..1]).unwrap();
    assert_eq!(s.schema_version().unwrap(), 1);
    s.conn
        .execute_batch(&format!(
            "INSERT INTO scans (scan_id, started_at, finished_at, roots_json, status, \
               entries_scanned, candidates, logical_bytes, allocated_bytes, reclaimable_bytes, \
               errors) \
             VALUES ('scan_S1', 100, 110, '[\"/work\",\"/other\"]', 'completed', 5, 1, 10, 10, \
                     10, 0); \
             INSERT INTO scan_rollups (scan_id, category, tier, ecosystem, items, logical_bytes, \
               allocated_bytes, reclaimable_bytes) \
             VALUES ('scan_S1', 'project_artifact', 'S0', '', 1, 10, 10, 10); \
             INSERT INTO items (item_id, path, category, artifact, tier, first_seen_at, \
               last_seen_at, first_seen_scan, last_seen_scan, logical_bytes, allocated_bytes, \
               reclaimable_bytes) \
             VALUES ('{ITEM_A}', '/work/a/target', 'project_artifact', 'Cargo build output', \
                     'S0', 105, 105, 'scan_S1', 'scan_S1', 10, 10, 10); \
             INSERT INTO free_space_samples (sampled_at, scan_id, volume, total_bytes, \
               free_bytes) VALUES (90, NULL, '/', 1000, 600), (110, 'scan_S1', '/', 1000, 500); \
             INSERT INTO plans (plan_id, content_hash, origin, scan_id, created_at, expires_at, \
               item_count, reclaimable_bytes, canonical_json) \
             VALUES ('plan_P1', '{h}', 'cli', 'scan_S1', 1, 2, 0, 0, '{{}}');",
            h = "a".repeat(64)
        ))
        .unwrap();
}

#[test]
fn fx_data_01_v1_database_upgrades_to_v2_with_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::DB_FILE_NAME);
    v1_database(&path);

    let s = open_with(&path, &MIGRATIONS[..2]).unwrap();
    assert_eq!(s.schema_version().unwrap(), 2);
    assert_eq!(s.backups_taken().len(), 1);
    let backup = s.backups_taken()[0].clone();
    let name = backup.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with("-v1"), "{name}");

    // The backup is the untouched v1 database.
    let b = Connection::open(&backup).unwrap();
    let v: u32 = b
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, 1);
    let has_v2: i64 = b
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name IN ('scan_roots', 'item_events')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_v2, 0);
    let items: i64 = b
        .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(items, 1);

    // Existing data is carried into the v2 structures.
    let latest = s
        .latest_scan(&scull_core::FsPath::from("/work"))
        .unwrap()
        .unwrap();
    assert_eq!(latest.scan_id.as_str(), "scan_S1");
    assert!(
        s.latest_scan(&scull_core::FsPath::from("/other"))
            .unwrap()
            .is_some()
    );
    let item = scull_core::ItemId::try_from(ITEM_A.to_owned()).unwrap();
    let h = s.item_history(&item).unwrap().unwrap();
    assert_eq!(h.disappeared_at, None);
    assert_eq!(h.events.len(), 1, "baseline event");
    assert_eq!(h.events[0].kind, crate::ItemEventKind::FirstSeen);
    assert_eq!(h.events[0].at, 105);
    let st = s.status_summary().unwrap();
    assert_eq!(st.last_scan.unwrap().safe_reclaimable_bytes, 10);
    assert_eq!(st.volumes.len(), 1);
    assert_eq!((st.volumes[0].at, st.volumes[0].free), (110, 500));
    assert_eq!(
        s.rollup_history(scull_core::Category::ProjectArtifact, 0)
            .unwrap()
            .len(),
        1
    );

    // Append-only invariants survive the migration.
    for sql in ["UPDATE plans SET origin = 'mcp'", "DELETE FROM plans"] {
        let err = s.conn.execute(sql, []).unwrap_err();
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
    }
    s.check_integrity().unwrap();
    drop(s);

    // Reopening takes no further backup.
    let again = open_with(&path, &MIGRATIONS[..2]).unwrap();
    assert!(again.backups_taken().is_empty());
    assert_eq!(list_backups(&path).len(), 1);
}

#[test]
fn history_queries_use_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("x.db"), &DefaultPermissions).unwrap();
    let plan = |sql: &str| -> String {
        let mut stmt = s
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows.join(" | ")
    };
    let status = plan(
        "SELECT scan_id FROM scans WHERE status IN ('completed', 'partial') \
         ORDER BY started_at DESC, scan_id DESC LIMIT 1",
    );
    assert!(status.contains("scans_started"), "{status}");
    assert!(!status.contains("TEMP B-TREE"), "{status}");
    let latest = plan(
        "SELECT s.scan_id FROM scan_roots r JOIN scans s ON s.scan_id = r.scan_id \
         WHERE r.root = '/w' AND s.status IN ('completed', 'partial') \
         ORDER BY r.started_at DESC, r.scan_id DESC LIMIT 1",
    );
    assert!(!latest.contains("TEMP B-TREE"), "{latest}");
    assert!(!latest.contains("SCAN r"), "{latest}");
    let free = plan(
        "SELECT * FROM free_space_samples WHERE volume = '/' AND sampled_at >= 0 \
         ORDER BY sampled_at, sample_id",
    );
    assert!(free.contains("free_space_samples_volume"), "{free}");
}

// ---- v2 → v3 (hash-chained audit) ----------------------------------------------------------

/// A v2 database with a plan, two operations, and v1-style audit rows for the first.
fn v2_database_with_audit(path: &Path) {
    let s = open_with(path, &MIGRATIONS[..2]).unwrap();
    assert_eq!(s.schema_version().unwrap(), 2);
    s.conn
        .execute_batch(&format!(
            "INSERT INTO plans (plan_id, content_hash, origin, scan_id, created_at, expires_at, \
               item_count, reclaimable_bytes, canonical_json) \
             VALUES ('plan_P1', '{h}', 'cli', 'scan_S1', 1, 2, 1, 10, '{{}}'); \
             INSERT INTO operations (operation_id, plan_id, content_hash, approver, method, \
               started_at) VALUES ('op_O1', 'plan_P1', '{h}', 'dev', 'cli_tty', 100), \
               ('op_O2', 'plan_P1', '{h}', 'dev', 'desktop', 200); \
             INSERT INTO audit (operation_id, recorded_at, item_id, path, action, outcome, \
               reason, predicted_bytes, measured_bytes) \
             VALUES ('op_O1', 101, '{ITEM_A}', '/work/a/target', 'delete', 'succeeded', NULL, \
                     10, 9), \
                    ('op_O1', 102, NULL, NULL, NULL, 'operation_finished', NULL, NULL, NULL);",
            h = "a".repeat(64)
        ))
        .unwrap();
}

#[test]
fn fx_data_01_v2_database_upgrades_to_v3_with_backup_and_chains_legacy_audit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(crate::DB_FILE_NAME);
    v2_database_with_audit(&path);

    let s = open_with(&path, MIGRATIONS).unwrap();
    assert_eq!(SCHEMA_VERSION, 3);
    assert_eq!(s.schema_version().unwrap(), 3);
    assert_eq!(s.backups_taken().len(), 1);
    let backup = s.backups_taken()[0].clone();
    let name = backup.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with("-v2"), "{name}");

    // The backup is the untouched v2 database, v1 audit table included.
    let b = Connection::open(&backup).unwrap();
    let v: u32 = b
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, 2);
    let old: i64 = b
        .query_row("SELECT sum(measured_bytes) FROM audit", [], |r| r.get(0))
        .unwrap();
    assert_eq!(old, 9);

    // The v1 rows are kept, frozen, and chained after a started row per operation.
    let n = |sql: &str| -> i64 { s.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(n("SELECT count(*) FROM audit_v1"), 2);
    let err = s
        .conn
        .execute(
            "INSERT INTO audit_v1 (operation_id, recorded_at, outcome) VALUES ('op_O2', 1, 'x')",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("frozen"), "{err}");
    for sql in ["UPDATE audit_v1 SET outcome = 'x'", "DELETE FROM audit_v1"] {
        let err = s.conn.execute(sql, []).unwrap_err();
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
    }
    let o1 = scull_core::OperationId::from_suffix("O1").unwrap();
    let o2 = scull_core::OperationId::from_suffix("O2").unwrap();
    let rows = s.audit_rows(&o1).unwrap();
    let kinds: Vec<&str> = rows.iter().map(|r| r.body.kind.as_str()).collect();
    assert_eq!(kinds, ["started", "legacy", "legacy"]);
    assert_eq!(rows[1].body.outcome.as_deref(), Some("succeeded"));
    assert_eq!(
        (rows[1].body.predicted_bytes, rows[1].body.freed_bytes),
        (Some(10), Some(9))
    );
    assert_eq!(rows[2].body.outcome.as_deref(), Some("operation_finished"));
    assert_eq!(s.audit_rows(&o2).unwrap().len(), 1);
    assert!(s.verify_all_chains().unwrap().is_empty());
    let rec = s.operation(&o1).unwrap().unwrap();
    assert_eq!((rec.started_at, rec.approver.as_str()), (100, "dev"));
    s.check_integrity().unwrap();
    drop(s);

    // Reopening takes no further backup.
    let again = open_with(&path, MIGRATIONS).unwrap();
    assert!(again.backups_taken().is_empty());
    assert_eq!(list_backups(&path).len(), 1);
}

#[test]
fn audit_queries_use_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("x.db"), &DefaultPermissions).unwrap();
    let plan = |sql: &str| -> String {
        let mut stmt = s
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows.join(" | ")
    };
    for sql in [
        "SELECT op_seq, row_hash FROM audit WHERE operation_id = 'op_x' \
         ORDER BY op_seq DESC LIMIT 1",
        "SELECT * FROM audit WHERE operation_id = 'op_x' AND op_seq >= 0 ORDER BY op_seq, seq",
        "SELECT operation_id FROM operations WHERE started_at >= 0 \
         ORDER BY started_at, operation_id",
    ] {
        let p = plan(sql);
        assert!(
            !p.contains("SCAN audit") && !p.contains("SCAN operations"),
            "{sql}: {p}"
        );
    }
}
