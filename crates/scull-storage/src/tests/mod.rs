//! Crate-internal tests: the migration runner with an extra (test-only) migration, which the
//! public API cannot inject. Temp dirs only.
#![allow(clippy::unwrap_used)]

use crate::backup::utc_stamp;
use crate::migrate::MIGRATIONS;
use crate::{DefaultPermissions, SCHEMA_VERSION, Store, StoreError, StoreOptions, list_backups};
use rusqlite::Connection;
use std::path::Path;

const V2_TEST: &str = "CREATE TABLE test_v2 (x INTEGER NOT NULL) STRICT; \
                       INSERT INTO meta (key, value) VALUES ('migrated', 'v2');";

fn migrations_with_v2() -> Vec<&'static str> {
    let mut m = MIGRATIONS.to_vec();
    m.push(V2_TEST);
    m
}

fn open_with(path: &Path, migrations: &[&str]) -> crate::Result<Store> {
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
    broken.push("CREATE TABLE half (x INTEGER); THIS IS NOT SQL;");
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

    let s = open_with(&path, MIGRATIONS).unwrap();
    assert_eq!(s.schema_version().unwrap(), 2);
    assert_eq!(SCHEMA_VERSION, 2);
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
    let again = open_with(&path, MIGRATIONS).unwrap();
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
