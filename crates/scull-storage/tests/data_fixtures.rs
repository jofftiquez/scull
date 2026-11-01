//! FX-DATA fixtures (docs/FIXTURES.md §3.13): -01 migration, -03 permissions, -05 busy
//! database, -06 corrupted database. All in temp dirs.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use scull_core::ErrorKind;
use scull_storage::{
    SCHEMA_VERSION, Store, StoreError, StoreOptions, list_backups, restore_backup,
};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// ---- FX-DATA-01: schema migration -------------------------------------------------------
// The backup-before-migration half needs a second migration, so it lives in the crate's
// internal tests (src/tests/mod.rs, `fx_data_01_*`). Here: v0 → latest and newer refused.

#[test]
fn fx_data_01_fresh_database_migrates_from_v0_to_latest() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let store = open(&path);
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert!(
        store.backups_taken().is_empty(),
        "an empty database needs no backup"
    );
    let conn = raw(&path);
    let app: i32 = conn
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .unwrap();
    assert_eq!(app, scull_storage::APPLICATION_ID);
    for table in [
        "meta",
        "profile",
        "scans",
        "scan_rollups",
        "free_space_samples",
        "items",
        "plans",
        "approvals",
        "operations",
        "audit",
        "audit_v1",
        "quarantine_manifest",
        "events",
        "metrics",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "missing table {table}");
    }
    drop(store);
    // Reopening at the latest version runs nothing and takes no backup.
    let again = open(&path);
    assert!(again.backups_taken().is_empty());
    assert!(list_backups(&path).is_empty());
}

#[test]
fn fx_data_01_existing_empty_file_is_treated_as_v0() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    std::fs::write(&path, b"").unwrap();
    assert_eq!(open(&path).schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn fx_data_01_newer_schema_is_refused_and_left_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    {
        let mut s = open(&path);
        start_scan(&mut s, 1);
    }
    // Simulate a newer scull having migrated the file.
    raw(&path)
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    let before = std::fs::read(&path).unwrap();

    let err = Store::open(&path, &TestPerms::default()).unwrap_err();
    match &err {
        StoreError::SchemaTooNew {
            found, supported, ..
        } => {
            assert_eq!(*found, SCHEMA_VERSION + 1);
            assert_eq!(*supported, SCHEMA_VERSION);
        }
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }
    assert!(err.to_string().contains("newer scull"), "{err}");
    assert_eq!(err.kind(), ErrorKind::Config);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "file must not change"
    );
    assert!(
        list_backups(&path).is_empty(),
        "no backup of a refused file"
    );
}

#[test]
fn fx_data_01_foreign_sqlite_database_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    raw(&path)
        .execute_batch("CREATE TABLE other_app (x INTEGER);")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let err = Store::open(&path, &TestPerms::default()).unwrap_err();
    assert!(
        matches!(err, StoreError::NotScullDatabase { .. }),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

// ---- FX-DATA-03: database file permissions ----------------------------------------------

#[test]
fn fx_data_03_owner_only_hooks_cover_dir_db_wal_and_shm() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data").join(scull_storage::DB_FILE_NAME);
    let perms = TestPerms::default();
    {
        let mut s = Store::open(&path, &perms).unwrap();
        start_scan(&mut s, 1);
    }
    let calls = perms.calls();
    assert_eq!(calls[0], ("create_dir".into(), dir.path().join("data")));
    assert_eq!(calls[1], ("create_file".into(), path.clone()));

    // On reopen every existing database file is tightened again.
    let perms2 = TestPerms::default();
    let s = Store::open(&path, &perms2).unwrap();
    let restricted: Vec<_> = perms2
        .calls()
        .into_iter()
        .filter(|(k, _)| k == "restrict")
        .map(|(_, p)| p)
        .collect();
    assert!(restricted.contains(&path));
    drop(s);
}

#[cfg(unix)]
#[test]
fn fx_data_03_database_wal_shm_are_0600_and_new_dir_is_0700() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let path = data.join(scull_storage::DB_FILE_NAME);
    let mut s = Store::open(&path, &TestPerms::default()).unwrap();
    start_scan(&mut s, 1);
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&path), 0o600);
    // SQLite creates -wal/-shm with the database file's mode.
    for side in ["scull.db-wal", "scull.db-shm"] {
        let p = data.join(side);
        assert!(p.exists(), "{side} should exist in WAL mode");
        assert_eq!(mode(&p), 0o600, "{side}");
    }
    drop(s);

    // A database loosened by someone else is tightened on the next open.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let s = Store::open(&path, &TestPerms::default()).unwrap();
    assert_eq!(mode(&path), 0o600);
    drop(s);
}

// ---- FX-DATA-05: database busy ----------------------------------------------------------

#[test]
fn fx_data_05_wal_mode_and_connection_settings_are_active() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let _s = open(&path);
    let conn = raw(&path);
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal", "WAL is persistent in the file");
}

#[test]
fn fx_data_05_readers_proceed_while_a_writer_holds_a_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    s.insert_plan(&plan(1)).unwrap();

    // Another scull holds the write lock with an uncommitted change.
    let mut writer = raw(&path);
    let tx = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('uncommitted', 'x')",
        [],
    )
    .unwrap();

    // A reader proceeds immediately and sees the last committed state.
    let reader = open(&path);
    let started = Instant::now();
    assert_eq!(reader.get_plan(&plan_id(1)).unwrap().unwrap(), plan(1));
    assert_eq!(reader.schema_version().unwrap(), SCHEMA_VERSION);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "reader must not wait"
    );
    let seen: i64 = raw(&path)
        .query_row(
            "SELECT count(*) FROM meta WHERE key = 'uncommitted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(seen, 0, "readers never see uncommitted writes");

    tx.commit().unwrap();
}

/// A write that finds another scull holding the write lock waits for it, then succeeds, and
/// both writes are kept. Synchronised on events, never on wall-clock time (it held up under
/// load only by luck before): the holder keeps the lock until the waiting writer has reported
/// that it found the lock taken ([`Store::on_lock_wait`]), and releases it only then, so the
/// writer provably met the lock and provably finished after its release.
#[test]
fn fx_data_05_writer_waits_for_the_lock_then_succeeds() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    // Opened (and migrated) before the lock is taken: only the write below meets it.
    let mut s = Store::open_with(
        &path,
        &TestPerms::default(),
        &StoreOptions {
            busy_timeout: Duration::from_secs(60),
        },
    )
    .unwrap();
    let waits = Arc::new(AtomicUsize::new(0));
    let (waiting_tx, waiting_rx) = mpsc::channel();
    {
        let waits = Arc::clone(&waits);
        let waiting_tx = std::sync::Mutex::new(waiting_tx);
        s.on_lock_wait(move || {
            waits.fetch_add(1, Ordering::SeqCst);
            let _ = waiting_tx.lock().unwrap().send(());
        });
    }

    let (locked_tx, locked_rx) = mpsc::channel();
    let releasing = Arc::new(AtomicBool::new(false));
    let holder_path = path.clone();
    let holder_releasing = Arc::clone(&releasing);
    let holder = std::thread::spawn(move || {
        let mut w = raw(&holder_path);
        let tx = w
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("INSERT INTO meta (key, value) VALUES ('holder', '1')", [])
            .unwrap();
        locked_tx.send(()).unwrap();
        // Keep the lock until the writer reports that it is waiting for it. (A generous
        // deadlock guard, not a timing assumption: the report comes as soon as the write runs.)
        waiting_rx
            .recv_timeout(Duration::from_secs(120))
            .expect("the writer reported waiting for the lock");
        // Set before the commit: the writer can only get the lock after this.
        holder_releasing.store(true, Ordering::SeqCst);
        tx.commit().unwrap();
    });
    locked_rx.recv().unwrap();

    s.insert_plan(&plan(1)).unwrap();
    assert!(
        releasing.load(Ordering::SeqCst),
        "the write finished only after the holder released the lock"
    );
    assert_eq!(
        waits.load(Ordering::SeqCst),
        1,
        "the write met the lock once"
    );
    holder.join().unwrap();

    // No corruption and both writes are present.
    s.check_integrity().unwrap();
    assert!(s.get_plan(&plan_id(1)).unwrap().is_some());
    let holder_row: i64 = raw(&path)
        .query_row("SELECT count(*) FROM meta WHERE key = 'holder'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(holder_row, 1);

    // Without contention the observer is not called.
    s.insert_plan(&plan(2)).unwrap();
    assert_eq!(waits.load(Ordering::SeqCst), 1);
}

#[test]
fn fx_data_05_writer_times_out_with_lock_held_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = Store::open_with(
        &path,
        &TestPerms::default(),
        &StoreOptions {
            busy_timeout: Duration::from_millis(100),
        },
    )
    .unwrap();
    let mut w = raw(&path);
    let tx = w
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let err = s.insert_plan(&plan(1)).unwrap_err();
    assert!(matches!(err, StoreError::Busy { .. }), "{err:?}");
    assert_eq!(err.kind(), ErrorKind::LockHeld);
    tx.rollback().unwrap();
    // The failed write left nothing behind and the store is still usable.
    assert!(s.get_plan(&plan_id(1)).unwrap().is_none());
    s.insert_plan(&plan(1)).unwrap();
}

// ---- FX-DATA-06: corrupted database -----------------------------------------------------

/// Make a valid database with one plan, plus a backup copy named like a migration backup.
fn db_with_backup(dir: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let path = db_in(dir);
    {
        let mut s = open(&path);
        s.insert_plan(&plan(1)).unwrap();
    } // closing checkpoints the WAL into the main file
    let backup = dir.path().join(format!(
        "{}.backup-20260101T000000Z-v1",
        scull_storage::DB_FILE_NAME
    ));
    std::fs::copy(&path, &backup).unwrap();
    (path, backup)
}

#[test]
fn fx_data_06_garbage_file_is_detected_untouched_and_backup_offered() {
    let dir = tempfile::tempdir().unwrap();
    let (path, backup) = db_with_backup(&dir);
    let garbage: Vec<u8> = (0..8192u32).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(&path, &garbage).unwrap();

    let err = Store::open(&path, &TestPerms::default()).unwrap_err();
    match &err {
        StoreError::Corrupt { backups, .. } => assert_eq!(backups, &vec![backup.clone()]),
        other => panic!("expected Corrupt, got {other:?}"),
    }
    assert_eq!(err.kind(), ErrorKind::Io);
    // Callers keep working without history (read-only scan): the error converts to the shared
    // model with the path, and the damaged file is not modified.
    let shared: scull_core::ScullError = err.into();
    assert_eq!(shared.path.unwrap().as_str(), path.to_string_lossy());
    assert_eq!(std::fs::read(&path).unwrap(), garbage);

    // Restore: damaged file is set aside (not deleted), backup content comes back.
    let moved = restore_backup(&path, &backup, &TestPerms::default()).unwrap();
    assert_eq!(std::fs::read(&moved).unwrap(), garbage);
    let s = open(&path);
    assert_eq!(s.get_plan(&plan_id(1)).unwrap().unwrap(), plan(1));
    s.check_integrity().unwrap();
    assert!(backup.exists(), "the backup itself is kept");
    let footprint = scull_storage::footprint_files(&path);
    assert!(footprint.contains(&moved) && footprint.contains(&backup));
}

#[test]
fn fx_data_06_damaged_pages_are_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _backup) = db_with_backup(&dir);
    // Keep the header page, overwrite everything after it.
    let mut bytes = std::fs::read(&path).unwrap();
    let page = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
    assert!(bytes.len() > page, "database should span several pages");
    for (i, b) in bytes.iter_mut().enumerate().skip(page) {
        *b = (i % 253) as u8;
    }
    std::fs::write(&path, &bytes).unwrap();

    let detected = match Store::open(&path, &TestPerms::default()) {
        Err(StoreError::Corrupt { .. }) => true,
        Err(other) => panic!("expected Corrupt, got {other:?}"),
        Ok(s) => {
            // Header and schema page intact: the full check must catch it.
            let r = s
                .check_integrity()
                .err()
                .or_else(|| s.get_plan(&plan_id(1)).err());
            matches!(r, Some(StoreError::Corrupt { .. }))
        }
    };
    assert!(detected, "damage must be reported as Corrupt");
}

#[test]
fn fx_data_06_restore_refuses_a_bad_backup() {
    let dir = tempfile::tempdir().unwrap();
    let (path, backup) = db_with_backup(&dir);
    std::fs::write(&backup, b"not a database at all, definitely not").unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(restore_backup(&path, &backup, &TestPerms::default()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before, "database untouched");
}

#[cfg(unix)]
#[test]
fn fx_data_03_symlinked_database_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = dir.path().join("elsewhere.db");
    std::fs::write(&elsewhere, b"").unwrap();
    let path = db_in(&dir);
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    let err = Store::open(&path, &TestPerms::default()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Config, "{err}");
    assert!(std::fs::read(&elsewhere).unwrap().is_empty());
}
