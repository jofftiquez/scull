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
