//! Restore from quarantine with integrity, never overwriting (P2-EXEC-06, SAFE-16, T-32).
//! Items are quarantined by a real operation into the fixture's own temp dir, then restored.
//! Every test checks the invariants.
//!
//! Fixture IDs (docs/FIXTURES.md): FX-EXEC-02 (restore half), FX-EXEC-04, FX-EXEC-05.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

#[path = "common/ops.rs"]
mod ops;
#[path = "common/planning.rs"]
mod planning;

use ops::*;
use planning::*;
use scull_cleaner::{DISK_MANIFEST, Executor, RestoreOutcome};
use scull_core::ErrorKind;
use scull_engine::now_unix;
use scull_fixtures::{Fixture, TreeSnapshot};
use scull_storage::{QuarantineEvent, Store};

/// Quarantines `rel` in operation `op` and returns the store and the manifest entry ID.
fn quarantine(fx: &Fixture, h: &Harness, rel: &str, op: &str) -> (Store, i64, TreeSnapshot) {
    let mut s = store(fx);
    let before = TreeSnapshot::capture(fx.root().join(rel)).unwrap();
    let (_, r) = run(fx, h, &mut s, &[rel], op);
    let id = quarantined(fx, &r, rel).entry_id;
    assert!(!fx.root().join(rel).exists());
    (s, id, before)
}

fn events(s: &Store, id: i64) -> Vec<QuarantineEvent> {
    s.quarantine_log(id)
        .unwrap()
        .iter()
        .map(|e| e.event)
        .collect()
}

fn print(label: &str, r: &Result<scull_cleaner::RestoreReport, scull_core::ScullError>) {
    match r {
        Ok(rep) => println!("{label}: {:?}", rep.outcome),
        Err(e) => println!("{label}: REFUSED [{}] {}", e.kind, e.message),
    }
}

/// FX-EXEC-02 (restore half): a quarantined S1 item is restored to its recorded path; the
/// restored tree is identical (content hashes, sizes, kinds, identity). Restoring again is an
/// idempotent no-op; every attempt is logged.
#[test]
fn fx_exec_02_quarantined_item_restored_tree_identical_and_idempotent() {
    let mut fx = fixture();
    let h = harness(false);
    let (mut s, id, before) = quarantine(&fx, &h, "app/target", "R2");
    fx.checkpoint().unwrap();

    let exec = locked(&fx);
    let r = exec.restore(&mut s, id, now_unix());
    print("restore", &r);
    let r = r.unwrap();
    assert_eq!(
        r.outcome,
        RestoreOutcome::Restored {
            to: fx.root().join("app/target"),
            sibling: false
        }
    );
    let after = TreeSnapshot::capture(fx.root().join("app/target")).unwrap();
    assert!(before.diff(&after).is_empty(), "{}", before.diff(&after));
    let entry = s.quarantine_entry(id).unwrap().unwrap();
    assert_eq!(
        entry.restored.as_ref().map(|(_, to)| to.to_path_buf()),
        Some(fx.root().join("app/target"))
    );

    let again = exec.restore(&mut s, id, now_unix());
    print("restore again", &again);
    assert!(matches!(
        again.unwrap().outcome,
        RestoreOutcome::AlreadyRestored { .. }
    ));
    assert_eq!(
        events(&s, id),
        [QuarantineEvent::Restored, QuarantineEvent::RestoreNoop]
    );
    check_fs(&fx, &["app/target"]);
}

/// FX-EXEC-04: the original path exists again (rebuilt) → restored to the sibling
/// `<name>.scull-restored-1`; the new original is untouched.
#[test]
fn fx_exec_04_original_path_exists_restored_to_sibling_nothing_overwritten() {
    let mut fx = fixture();
    let h = harness(false);
    let (mut s, id, before) = quarantine(&fx, &h, "app/target", "R4");
    // The user rebuilt the project meanwhile.
    std::fs::create_dir_all(fx.root().join("app/target/debug")).unwrap();
    std::fs::write(fx.root().join("app/target/debug/app"), b"new build").unwrap();
    // A first sibling is already taken too.
    std::fs::write(fx.root().join("app/target.scull-restored-1"), b"taken").unwrap();
    fx.checkpoint().unwrap();

    let r = locked(&fx).restore(&mut s, id, now_unix());
    print("restore", &r);
    let to = fx.root().join("app/target.scull-restored-2");
    assert_eq!(
        r.unwrap().outcome,
        RestoreOutcome::Restored {
            to: to.clone(),
            sibling: true
        }
    );
    assert_eq!(
        std::fs::read(fx.root().join("app/target/debug/app")).unwrap(),
        b"new build"
    );
    assert_eq!(
        std::fs::read(fx.root().join("app/target.scull-restored-1")).unwrap(),
        b"taken"
    );
    let after = TreeSnapshot::capture(&to).unwrap();
    assert!(before.diff(&after).is_empty(), "{}", before.diff(&after));
    check_fs(&fx, &["app/target.scull-restored-2"]);
}

/// FX-EXEC-05: the on-disk manifest copy is tampered to point restore at `~/.ssh` → ignored:
/// the database manifest is authoritative (and can't be edited), so the entry goes back to its
/// recorded path and the fake `.ssh` is untouched. A parent folder swapped for a link to
/// `.ssh` is refused outright.
#[test]
fn fx_exec_05_tampered_manifest_pointing_at_ssh_is_refused_db_is_authoritative() {
    let mut fx = fixture();
    let h = harness(false);
    let (mut s, id, _) = quarantine(&fx, &h, "app/target", "R5");
    // A stand-in for ~/.ssh, outside the fixture root (inside the fixture's temp dir).
    let ssh = fx.base().join("home").join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::write(ssh.join("id_ed25519"), b"PRIVATE KEY").unwrap();
    let ssh_before = TreeSnapshot::capture(&ssh).unwrap();

    // 1. Tamper the on-disk copy.
    let entry = s.quarantine_entry(id).unwrap().unwrap();
    let qdir = entry.entry.quarantine_path.to_path_buf();
    let copy = qdir.parent().unwrap().join(DISK_MANIFEST);
    let text = std::fs::read_to_string(&copy).unwrap();
    let tampered = text.replace(
        entry.entry.original_path.as_str(),
        ssh.join("target").to_str().unwrap(),
    );
    assert_ne!(text, tampered);
    std::fs::write(&copy, tampered).unwrap();
    // 2. The manifest of record can't be redirected: its recorded paths are immutable
    //    (triggers; `manifest_paths_and_facts_are_immutable_and_rows_never_deleted` in
    //    scull-storage) and no API changes them.

    // 3. The original folder is swapped for a link to `.ssh`.
    let app = fx.root().join("app");
    std::fs::rename(&app, fx.root().join("app-moved")).unwrap();
    std::os::unix::fs::symlink(&ssh, &app).unwrap();
    fx.checkpoint().unwrap();
    let exec = locked(&fx);
    let refused = exec.restore(&mut s, id, now_unix());
    print("restore into a swapped folder", &refused);
    let e = refused.unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked);
    assert!(
        TreeSnapshot::capture(&ssh)
            .unwrap()
            .diff(&ssh_before)
            .is_empty()
    );
    assert!(s.quarantine_entry(id).unwrap().unwrap().is_held());
    assert_eq!(events(&s, id), [QuarantineEvent::RestoreRefused]);
    let log = s.quarantine_log(id).unwrap();
    assert_eq!(log[0].reason.as_deref(), Some("parent_changed"));

    // 4. With the real folder back, restore goes to the recorded path only.
    std::fs::remove_file(&app).unwrap();
    std::fs::rename(fx.root().join("app-moved"), &app).unwrap();
    fx.checkpoint().unwrap();
    let ok = exec.restore(&mut s, id, now_unix());
    print("restore (database manifest)", &ok);
    assert_eq!(
        ok.unwrap().outcome,
        RestoreOutcome::Restored {
            to: fx.root().join("app/target"),
            sibling: false
        }
    );
    assert!(
        TreeSnapshot::capture(&ssh)
            .unwrap()
            .diff(&ssh_before)
            .is_empty()
    );
    assert!(!ssh.join("target").exists());
    check_fs(&fx, &["app/target"]);
}

/// A quarantined entry that was modified (or replaced) is not restored.
#[test]
fn modified_quarantined_entry_fails_integrity_and_is_not_restored() {
    let mut fx = fixture();
    let h = harness(false);
    let (mut s, id, _) = quarantine(&fx, &h, "app/target", "RM");
    let q = s
        .quarantine_entry(id)
        .unwrap()
        .unwrap()
        .entry
        .quarantine_path
        .to_path_buf();
    std::fs::write(q.join("planted.sh"), b"#!/bin/sh\necho owned\n").unwrap();
    fx.checkpoint().unwrap();
    let r = locked(&fx).restore(&mut s, id, now_unix());
    print("restore a modified entry", &r);
    assert_eq!(r.unwrap_err().kind, ErrorKind::SafetyBlocked);
    assert!(!fx.root().join("app/target").exists());
    assert_eq!(
        s.quarantine_log(id).unwrap()[0].reason.as_deref(),
        Some("integrity_failed")
    );
    check_fs(&fx, &[]);
}

/// A crash after the move but before the database update: restoring again finds the entry
/// at its original path (by identity) and records it as restored.
#[test]
fn restore_after_an_unrecorded_move_is_recorded_idempotently() {
    let mut fx = fixture();
    let h = harness(false);
    let (mut s, id, _) = quarantine(&fx, &h, "app/target", "RC");
    let q = s
        .quarantine_entry(id)
        .unwrap()
        .unwrap()
        .entry
        .quarantine_path
        .to_path_buf();
    std::fs::rename(&q, fx.root().join("app/target")).unwrap();
    fx.checkpoint().unwrap();
    let r = locked(&fx).restore(&mut s, id, now_unix()).unwrap();
    assert_eq!(
        r.outcome,
        RestoreOutcome::Restored {
            to: fx.root().join("app/target"),
            sibling: false
        }
    );
    assert!(!s.quarantine_entry(id).unwrap().unwrap().is_held());
    check_fs(&fx, &[]);
}

/// A purged entry can't be restored; restore needs the lock.
#[test]
fn purged_entry_or_missing_lock_is_refused() {
    let fx = fixture();
    let h = harness(false);
    let (mut s, id, _) = quarantine(&fx, &h, "app/target", "RP");
    assert_eq!(
        Executor::new()
            .restore(&mut s, id, now_unix())
            .unwrap_err()
            .kind,
        ErrorKind::SafetyBlocked
    );
    let exec = locked(&fx);
    exec.purge_quarantine(
        &mut s,
        &scull_cleaner::PurgePolicy {
            now: now_unix(),
            expired: false,
            older_than: Some(now_unix() + 1),
            size_cap: None,
        },
    )
    .unwrap();
    assert_eq!(
        exec.restore(&mut s, id, now_unix()).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert!(!fx.root().join("app/target").exists());
    assert_eq!(
        events(&s, id),
        [QuarantineEvent::Purged, QuarantineEvent::RestoreRefused]
    );
    // The quarantine (outside the root) was purged; inside the root only the item left.
    check_fs(&fx, &["app/target"]);
}

/// I-2 and I-3 for a restore (no plan executed here: I-1 and I-8 are not applicable).
fn check_fs(fx: &Fixture, allowed: &[&str]) {
    let mut inputs = scull_fixtures::InvariantInputs::new();
    for a in allowed {
        inputs = inputs.allow(*a);
    }
    let report = fx.check_invariants(&inputs);
    println!("{report}");
    report.assert_ok();
}
