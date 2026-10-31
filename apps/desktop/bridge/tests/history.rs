//! The `history` command reads scull's database without ever creating, migrating or writing it,
//! and every answer validates against the committed `history` schema.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_desktop_bridge::history::{HistoryReadState, read_history};
use scull_desktop_bridge::{Session, Setup};
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
    assert!(!v.audit_api);
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
    // This core has no Executor and no audit API: nothing can have recorded an operation.
    assert!(v.operations.is_empty());
    validate(&serde_json::to_value(&v).unwrap());

    assert_eq!(
        snapshot(data.path()),
        before,
        "the database files are unchanged"
    );
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
