//! Same-volume quarantine with retention and purge (P2-EXEC-05, D-3, PRD §10.6, SAFE-16), and
//! plan invalidation by an overlapping operation (P2-EXEC-04, SAFE-09). Plans come from a real
//! engine scan of a `scull-fixtures` tree; the quarantine lives beside the fixture root (inside
//! the fixture's own temp dir), so I-3 sees only the item leave. Every test checks the
//! invariants.
//!
//! Fixture IDs (docs/FIXTURES.md): FX-EXEC-02 (quarantine half; restore is #72), FX-EXEC-03,
//! FX-EXEC-06, FX-EXEC-16 (owner-only folder; the second-user read check is nightly).
//!
//! macOS, Linux and Windows (same-volume moves on handles there, #71). Making quarantine
//! impossible differs per OS: a read-only parent on Unix, a deny ACL on Windows (FX-EXEC-03).
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/ops.rs"]
mod ops;
#[path = "common/planning.rs"]
mod planning;

use ops::*;
use planning::*;
#[cfg(unix)]
use scull_cleaner::QUARANTINE_DIR_NAME;
use scull_cleaner::{
    DEFAULT_RETENTION_SECS, DISK_MANIFEST, Executor, ItemOutcome, PurgeOutcome, PurgePolicy,
};
use scull_core::{Action, ErrorKind, OperationId, Tier};
use scull_engine::{CancelToken, now_unix};
use scull_fixtures::{Fixture, NodeLock, TreeSnapshot};
#[cfg(unix)]
use scull_platform::ensure_owner_only;
use scull_platform::{Native, Platform};
use scull_storage::{ItemOutcome as AuditOutcome, OperationEnd};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;

/// FX-EXEC-02 (quarantine half): an S1 item is moved into the same-volume quarantine by a
/// rename (same identity, identical tree), recorded in the manifest of record with a 7-day
/// expiry and audited as `quarantined`; the S0 item of the same plan is deleted.
#[test]
fn fx_exec_02_s1_item_quarantined_by_same_volume_rename_tree_identical() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    assert_eq!(at(&fx, &scan, "app/target").tier, Tier::S1);
    assert_eq!(at(&fx, &scan, "lib/target").tier, Tier::S0);
    assert_eq!(at(&fx, &scan, "app/target").action, Action::Quarantine);
    let before = TreeSnapshot::capture(fx.root().join("app/target")).unwrap();
    let identity_before = Native
        .metadata(&fx.root().join("app/target"))
        .unwrap()
        .identity;

    let (plan, r) = run(&fx, &h, &mut s, &["app/target", "lib/target"], "Q1");
    let q = quarantined(&fx, &r, "app/target");
    assert_eq!(q.quarantine_path, op_dir(&fx, "Q1").join("0001"));
    assert!(!fx.root().join("app/target").exists());
    assert_executed(&fx, &r, "lib/target");
    // A rename, not a copy: same identity, identical tree (content hashes, sizes, kinds).
    assert_eq!(
        Native.metadata(&q.quarantine_path).unwrap().identity,
        identity_before
    );
    let after = TreeSnapshot::capture(&q.quarantine_path).unwrap();
    assert!(before.diff(&after).is_empty(), "{}", before.diff(&after));
    assert!(q.bytes >= 96 * 1024, "{q:?}");

    // The database manifest is the record.
    let entries = s.quarantine_held().unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.entry_id, q.entry_id);
    assert_eq!(
        canon(&e.entry.original_path.to_path_buf()),
        canon(&fx.root().join("app").join("target"))
    );
    assert_eq!(e.entry.entry_kind, "dir");
    assert_eq!(
        e.entry.expires_at,
        Some(e.entry.quarantined_at + DEFAULT_RETENTION_SECS)
    );
    assert_eq!(
        Some(e.entry.parent_identity),
        Native.metadata(&fx.root().join("app")).unwrap().identity
    );
    // The audit says so, and its chain is intact.
    let op = OperationId::from_suffix("Q1").unwrap();
    let audit = s.audit_items(&op).unwrap();
    let quarantine_item = &plan
        .items()
        .iter()
        .find(|i| i.action == Action::Quarantine)
        .unwrap()
        .item_id;
    let row = audit
        .iter()
        .find(|a| &a.item_id == quarantine_item)
        .unwrap();
    assert_eq!(row.outcome, AuditOutcome::Quarantined);
    assert_eq!(row.moved_bytes, Some(q.bytes));
    assert!(s.verify_chain(&op).unwrap().is_intact());
    // The on-disk copy exists, owner-only, and lists the entry (JSON escapes `\`).
    let copy = std::fs::read_to_string(op_dir(&fx, "Q1").join(DISK_MANIFEST)).unwrap();
    assert!(copy.replace("\\\\", "/").contains("app/target"), "{copy}");
    check(&fx, &plan, &r, &["app/target", "lib/target"]);
}

/// FX-EXEC-16: the quarantine and operation folders (and the manifest copy) are owner-only;
/// another account can't list or read them. (Reading as a second test user is the nightly
/// half of this fixture.)
#[test]
#[cfg(unix)]
fn fx_exec_16_quarantine_folder_is_owner_only() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let (plan, r) = run(&fx, &h, &mut s, &["web/node_modules"], "Q16");
    quarantined(&fx, &r, "web/node_modules");
    let root = fx.base().join(QUARANTINE_DIR_NAME);
    let dir = op_dir(&fx, "Q16");
    for p in [&root, &dir] {
        ensure_owner_only(p).unwrap();
        assert_eq!(mode(p), 0o700, "{}", p.display());
    }
    ensure_owner_only(&dir.join(DISK_MANIFEST)).unwrap();
    assert_eq!(mode(&dir.join(DISK_MANIFEST)) & 0o077, 0);
    check(&fx, &plan, &r, &["web/node_modules"]);
}

/// FX-EXEC-03: quarantine is impossible (read-only parent) and the Trash fallback is not
/// available in this build → the item is blocked and left in place; it is never deleted
/// permanently instead.
#[test]
fn fx_exec_03_quarantine_impossible_blocked_never_deleted() {
    let mut fx = fixture();
    #[cfg(unix)]
    if owned_by_root(fx.root()) {
        eprintln!("running as root: directory permissions don't apply; skipped");
        return;
    }
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let plan = plan_named(&fx, &scan, &["web/node_modules"], "Q3");
    s.insert_plan(&plan).unwrap();
    let web = fx.root().join("web");
    let guard = no_moves_out_of(&web);
    fx.checkpoint().unwrap();

    let exec = locked(&fx);
    let mut operation = exec
        .begin_operation(&mut s, &plan, &start(&plan, "Q3"))
        .unwrap()
        .with_quarantine(settings(&fx));
    let r = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(&fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    drop(guard);
    print_report(&fx, &r);

    match &r.items[0].outcome {
        ItemOutcome::Blocked { code, message } => {
            assert_eq!(*code, "quarantine_impossible");
            assert!(message.contains("never deleted"), "{message}");
        }
        other => panic!("expected blocked, got {other:?}"),
    }
    assert!(fx.root().join("web/node_modules/react/big.js").is_file());
    assert!(s.quarantine_held().unwrap().is_empty());
    let audit = s
        .audit_items(&OperationId::from_suffix("Q3").unwrap())
        .unwrap();
    assert_eq!(audit[0].outcome, AuditOutcome::Refused);
    assert_eq!(audit[0].reason.as_deref(), Some("quarantine_impossible"));
    assert_eq!(r.error_kind(), Some(ErrorKind::PartialFailure));
    // Restore the mode before the I-3 snapshot compares it.
    fx.checkpoint().unwrap();
    check(&fx, &plan, &r, &[]);
}

/// Makes moving entries out of `dir` impossible until dropped: a read-only directory on Unix;
/// on Windows (where the read-only attribute doesn't apply to folders) ACEs denying this
/// account DELETE on its entries and DELETE_CHILD on it, which a rename needs.
struct NoMovesOut {
    dir: std::path::PathBuf,
    #[cfg(windows)]
    entries: Vec<std::path::PathBuf>,
}

fn no_moves_out_of(dir: &std::path::Path) -> NoMovesOut {
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    #[cfg(windows)]
    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    #[cfg(windows)]
    {
        icacls(dir, &["/deny", &format!("{}:(DC)", win_user())]);
        for e in &entries {
            icacls(e, &["/deny", &format!("{}:(DE)", win_user())]);
        }
    }
    NoMovesOut {
        dir: dir.to_path_buf(),
        #[cfg(windows)]
        entries,
    }
}

impl Drop for NoMovesOut {
    fn drop(&mut self) {
        #[cfg(unix)]
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        #[cfg(windows)]
        {
            for e in &self.entries {
                icacls(e, &["/remove:d", &win_user()]);
            }
            icacls(&self.dir, &["/remove:d", &win_user()]);
        }
    }
}

#[cfg(windows)]
fn win_user() -> String {
    std::env::var("USERNAME").expect("USERNAME is set on Windows")
}

#[cfg(windows)]
fn icacls(path: &std::path::Path, args: &[&str]) {
    let o = std::process::Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .expect("icacls runs");
    assert!(
        o.status.success(),
        "icacls {} {args:?}: {}",
        path.display(),
        String::from_utf8_lossy(&o.stdout)
    );
}

/// Whether this process runs as root (it owns what it creates).
#[cfg(unix)]
fn owned_by_root(p: &Path) -> bool {
    std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(p).unwrap()) == 0
}

/// FX-EXEC-06: retention expires → the entry is purged through the walker and its freed
/// bytes are recorded and reported; nothing is purged before expiry.
#[test]
fn fx_exec_06_retention_expires_purged_freed_bytes_recorded() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let (plan, r) = run(&fx, &h, &mut s, &["web/node_modules"], "Q6");
    let q = quarantined(&fx, &r, "web/node_modules").clone();
    let entry = s.quarantine_entry(q.entry_id).unwrap().unwrap();
    let expires = entry.entry.expires_at.unwrap();

    let exec = locked(&fx);
    let early = exec
        .purge_quarantine(&mut s, &PurgePolicy::expired_at(expires - 1))
        .unwrap();
    assert!(early.entries.is_empty());
    assert!(q.quarantine_path.exists());

    let purged = exec
        .purge_quarantine(&mut s, &PurgePolicy::expired_at(expires))
        .unwrap();
    println!(
        "purge at expiry: {} entr(y/ies), {} bytes freed",
        purged.entries.len(),
        purged.freed_bytes
    );
    assert_eq!(purged.entries.len(), 1);
    let PurgeOutcome::Purged { freed_bytes } = purged.entries[0].1 else {
        panic!("{:?}", purged.entries[0].1)
    };
    assert!(freed_bytes >= 64 * 1024);
    assert_eq!(purged.freed_bytes, freed_bytes);
    assert!(!q.quarantine_path.exists());
    let after = s.quarantine_entry(q.entry_id).unwrap().unwrap();
    assert_eq!(after.purged, Some((expires, freed_bytes)));
    assert!(s.quarantine_held().unwrap().is_empty());
    // Purging again finds nothing.
    assert!(
        exec.purge_quarantine(&mut s, &PurgePolicy::expired_at(expires + 1))
            .unwrap()
            .entries
            .is_empty()
    );
    check(&fx, &plan, &r, &["web/node_modules"]);
}

/// The size cap purges the oldest entries first until the quarantine fits.
#[test]
fn size_cap_purges_oldest_first() {
    let mut fx = Fixture::new().unwrap();
    for p in ["a", "b"] {
        fx.npm_project(p, p, NodeLock::None)
            .unwrap()
            .node_modules(p, &["x"])
            .unwrap()
            .file_of_size(format!("{p}/node_modules/x/blob"), 32 * 1024)
            .unwrap();
    }
    fx.checkpoint().unwrap();
    let h = harness(false);
    let mut s = store(&fx);
    let (_, r1) = run(&fx, &h, &mut s, &["a/node_modules"], "QA");
    let (plan, r2) = run(&fx, &h, &mut s, &["b/node_modules"], "QB");
    let qa = quarantined(&fx, &r1, "a/node_modules").clone();
    let qb = quarantined(&fx, &r2, "b/node_modules").clone();
    let exec = locked(&fx);
    let report = exec
        .purge_quarantine(
            &mut s,
            &PurgePolicy {
                now: now_unix(),
                expired: true,
                older_than: None,
                size_cap: Some(qb.bytes),
            },
        )
        .unwrap();
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].0.entry_id, qa.entry_id);
    assert!(!qa.quarantine_path.exists());
    assert!(qb.quarantine_path.exists());
    check(&fx, &plan, &r2, &["a/node_modules", "b/node_modules"]);
}

/// A tampered quarantine entry (replaced on disk) is never purged.
#[test]
fn replaced_quarantine_entry_is_kept_not_purged() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let (plan, r) = run(&fx, &h, &mut s, &["web/node_modules"], "QT");
    let q = quarantined(&fx, &r, "web/node_modules").clone();
    let moved = q.quarantine_path.with_extension("moved");
    std::fs::rename(&q.quarantine_path, &moved).unwrap();
    std::fs::create_dir(&q.quarantine_path).unwrap();
    std::fs::write(q.quarantine_path.join("victim.txt"), b"not scull's").unwrap();
    let report = locked(&fx)
        .purge_quarantine(
            &mut s,
            &PurgePolicy {
                now: now_unix(),
                expired: false,
                older_than: Some(now_unix() + 1),
                size_cap: None,
            },
        )
        .unwrap();
    assert!(
        matches!(report.entries[0].1, PurgeOutcome::Kept { .. }),
        "{report:?}"
    );
    assert_eq!(
        std::fs::read(q.quarantine_path.join("victim.txt")).unwrap(),
        b"not scull's"
    );
    assert!(s.quarantine_entry(q.entry_id).unwrap().unwrap().is_held());
    check(&fx, &plan, &r, &["web/node_modules"]);
}

/// Without an operation record there is no manifest of record: quarantine is refused.
#[test]
fn quarantine_without_an_operation_is_refused() {
    let fx = fixture();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["web/node_modules"]);
    let r = h.execute(&fx, &plan);
    assert!(
        matches!(&r.items[0].outcome, ItemOutcome::Refused(e) if e.kind == ErrorKind::SafetyBlocked)
    );
    assert!(fx.root().join("web/node_modules").is_dir());
    check(&fx, &plan, &r, &[]);
}

/// FX-RACE-05 / SAFE-09: a plan is invalidated (exit 4) when another operation touched its
/// targets after it was created; nothing is touched.
#[test]
fn fx_race_05_plan_invalidated_when_another_operation_touched_its_targets() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let stale = plan_named(&fx, &scan, &["web/node_modules"], "STALE");
    s.insert_plan(&stale).unwrap();
    // Meanwhile another operation (another plan over the same item) runs.
    let (_, r) = run(&fx, &h, &mut s, &["web/node_modules"], "FIRST");
    quarantined(&fx, &r, "web/node_modules");

    let exec = locked(&fx);
    let e = exec
        .begin_operation(&mut s, &stale, &start(&stale, "SECOND"))
        .unwrap_err();
    println!("stale plan: [{}] {}", e.kind, e.message);
    assert_eq!(e.kind, ErrorKind::PlanInvalidated);
    assert_eq!(e.exit_code().as_i32(), 4);
    assert!(
        s.operation(&OperationId::from_suffix("SECOND").unwrap())
            .unwrap()
            .is_none()
    );
    check(&fx, &stale, &r, &["web/node_modules"]);
}

/// SAFE-09 with the watermark taken when the plan's scan started: an operation recorded after
/// it invalidates the plan even though it started *before* the plan was created (between the
/// scan and the plan, where the creation-time rule only caught it within the same second). A
/// watermark taken after that operation (a rescan) is not invalidated by it.
#[test]
fn fx_race_05_operation_recorded_after_the_scan_started_invalidates_the_plan() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let watermark = s.operations_watermark().unwrap();
    let scan = h.scan(&fx);
    // After the scan, before the plan is created, another operation runs on the same item.
    let (_, r) = run(&fx, &h, &mut s, &["web/node_modules"], "FIRST");
    quarantined(&fx, &r, "web/node_modules");
    let stale = plan_watermarked(
        &fx,
        &scan,
        &["web/node_modules", "app/target"],
        "STALE",
        watermark,
    );
    s.insert_plan(&stale).unwrap();

    let exec = locked(&fx);
    let e = exec
        .begin_operation(&mut s, &stale, &start(&stale, "SECOND"))
        .unwrap_err();
    println!("stale plan: [{}] {}", e.kind, e.message);
    assert_eq!(e.kind, ErrorKind::PlanInvalidated);
    assert!(e.message.contains("FIRST"), "{}", e.message);
    drop(exec);

    // A rescan takes a new watermark: FIRST is before it and no longer counts.
    let rescan_watermark = s.operations_watermark().unwrap();
    assert!(rescan_watermark > watermark);
    let rescan = h.scan(&fx);
    let fresh = plan_watermarked(&fx, &rescan, &["app/target"], "FRESH", rescan_watermark);
    s.insert_plan(&fresh).unwrap();
    let exec = locked(&fx);
    exec.begin_operation(&mut s, &fresh, &start(&fresh, "THIRD"))
        .expect("an operation before the rescan does not invalidate the new plan");
    check(&fx, &stale, &r, &["web/node_modules"]);
}

/// SAFE-09: an operation that began before the scan and is still running when the watermark
/// is read may change the items after the scan observed them, so it invalidates the plan once
/// it is over (begin_operation refuses while it runs: SAFE-19).
#[test]
fn fx_race_05_operation_running_when_the_scan_started_invalidates_the_plan() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let before = h.scan(&fx);
    let other = plan_named(&fx, &before, &["app/target"], "OTHER");
    s.insert_plan(&other).unwrap();
    let exec = locked(&fx);
    let running = exec
        .begin_operation(&mut s, &other, &start(&other, "RUNNING"))
        .unwrap();
    // The scan starts while RUNNING is still running (read through a second connection).
    let watermark = store(&fx).operations_watermark().unwrap();
    let scan = h.scan(&fx);
    running.finish(OperationEnd::Cancelled, now_unix()).unwrap();
    drop(exec);

    let plan = plan_watermarked(&fx, &scan, &["app/target"], "LATER", watermark);
    s.insert_plan(&plan).unwrap();
    let exec = locked(&fx);
    let e = exec
        .begin_operation(&mut s, &plan, &start(&plan, "LATEROP"))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{}", e.message);
    assert!(e.message.contains("RUNNING"), "{}", e.message);
    fx.check_invariants(&scull_fixtures::InvariantInputs::new())
        .assert_ok();
}

/// Operations need the execution lock.
#[test]
fn operations_and_purge_need_the_lock() {
    let fx = fixture();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let p = plan(&fx, &scan, &["web/node_modules"]);
    s.insert_plan(&p).unwrap();
    let unlocked = Executor::new();
    assert_eq!(
        unlocked
            .begin_operation(&mut s, &p, &start(&p, "NOLOCK"))
            .unwrap_err()
            .kind,
        ErrorKind::SafetyBlocked
    );
    assert_eq!(
        unlocked
            .purge_quarantine(&mut s, &PurgePolicy::expired_at(now_unix()))
            .unwrap_err()
            .kind,
        ErrorKind::SafetyBlocked
    );
    fx.check_invariants(&scull_fixtures::InvariantInputs::new())
        .assert_ok();
}
