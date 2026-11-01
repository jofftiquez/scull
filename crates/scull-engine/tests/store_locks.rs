//! Two scull processes on one database: opening a `Store` must never drop the SQLite locks
//! this process already holds on it.
//!
//! On Unix, closing *any* descriptor of a file releases every POSIX record lock the process
//! holds on that file. SQLite keeps its database and `-shm` (WAL shared memory) locks that
//! way. `Store::open` restricts the database, `-wal` and `-shm` files to owner-only
//! (`scull_platform::owner_only::restrict`); if that opened and closed them while a
//! connection of this process was live, the connection lost its `-shm` lock, and the next
//! process to open the database believed it was alone and reset the shared memory under it:
//! the first process then died of `SIGBUS` (or could read and write a database another
//! process was checkpointing).
//!
//! The test re-runs this test binary as a child process (A) that holds a store open and
//! opens a second one in the same process (the pre-open restriction while a connection is
//! live), while this process (B) opens, writes and closes the same database; then A keeps
//! using its first connection. A must finish normally with every write recorded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_engine::open_history;
use scull_storage::{Event, EventKind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "SCULL_TEST_STORE_LOCKS_CHILD";
const TEST_NAME: &str = "a_second_process_never_resets_a_live_connection";

fn event(source: &str, n: i64) -> Event {
    Event {
        event_id: 0,
        at: 1_700_000_000 + n,
        kind: EventKind::Alert,
        source: source.into(),
        payload: serde_json::json!({ "n": n }),
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Process A: hold a connection, open a second store beside it, let B work, keep writing.
fn child(dir: &Path) {
    let data = dir.join("data");
    let mut a = open_history(&data).unwrap();
    a.record_events(&[event("cli", 1)]).unwrap();
    // A second store in this process while `a` is live (both restriction passes run).
    let mut second = open_history(&data).unwrap();
    second.record_events(&[event("cli", 2)]).unwrap();
    drop(second);
    std::fs::write(dir.join("ready"), "").unwrap();
    wait_for(&dir.join("go"));
    // Without its locks, this connection's shared memory was reset by B: SIGBUS here.
    for n in 3..50 {
        a.record_events(&[event("cli", n)]).unwrap();
    }
    let alerts = a.events(EventKind::Alert, 0).unwrap().len();
    assert_eq!(alerts, 49 + 20, "A's 49 writes and B's 20 are all there");
}

#[test]
fn a_second_process_never_resets_a_live_connection() {
    if let Some(dir) = std::env::var_os(CHILD_ENV) {
        child(&PathBuf::from(dir));
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let data = dir.join("data");
    drop(open_history(&data).unwrap());

    let mut a = Command::new(std::env::current_exe().unwrap())
        .args([TEST_NAME, "--exact", "--nocapture", "--test-threads", "1"])
        .env(CHILD_ENV, &dir)
        .spawn()
        .unwrap();
    wait_for(&dir.join("ready"));
    // Process B opens (its first connection takes over the shared memory if nobody holds
    // it), writes and closes while A's connection is live.
    let wal = data.join(format!("{}-wal", scull_storage::DB_FILE_NAME));
    let shm = data.join(format!("{}-shm", scull_storage::DB_FILE_NAME));
    let mut lost = Vec::new();
    for n in 0..20 {
        let mut b = open_history(&data).unwrap();
        b.record_events(&[event("mcp", 100 + n)]).unwrap();
        drop(b);
        // A's connection still holds its shared locks, so closing B's is not the last close:
        // SQLite keeps the WAL and the shared memory. Without A's locks, B believes it is
        // alone and checkpoints, then deletes both under A.
        if !wal.exists() || !shm.exists() {
            lost.push(n);
        }
    }
    std::fs::write(dir.join("go"), "").unwrap();
    let status = a.wait().unwrap();
    assert!(
        lost.is_empty(),
        "process A's SQLite locks were gone: B deleted the WAL under it after writes {lost:?}"
    );
    assert!(
        status.success(),
        "process A failed (killed by SIGBUS when its SQLite locks were dropped): {status:?}"
    );
}
