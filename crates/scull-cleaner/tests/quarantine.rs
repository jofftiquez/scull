//! Same-volume quarantine with retention and purge (P2-EXEC-05, D-3, PRD §10.6, SAFE-16), and
//! plan invalidation by an overlapping operation (P2-EXEC-04, SAFE-09). Plans come from a real
//! engine scan of a `scull-fixtures` tree; the quarantine lives beside the fixture root (inside
//! the fixture's own temp dir), so I-3 sees only the item leave. Every test checks the
//! invariants.
//!
//! Fixture IDs (docs/FIXTURES.md): FX-EXEC-02 (quarantine half; restore is #72), FX-EXEC-03,
//! FX-EXEC-06, FX-EXEC-16 (owner-only folder; the second-user read check is nightly).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

#[path = "common/planning.rs"]
mod planning;

use planning::*;
use scull_cleaner::{
    DEFAULT_RETENTION_SECS, DISK_MANIFEST, ExecutionReport, Executor, ItemOutcome, Operation,
    PurgeOutcome, PurgePolicy, QUARANTINE_DIR_NAME, QuarantineSettings, Quarantined,
};
use scull_core::{Action, ErrorKind, OperationId, Plan, Tier};
use scull_engine::{CancelToken, PlatformOwnerOnly, now_unix};
use scull_fixtures::{Fixture, NodeLock, TreeSnapshot};
use scull_platform::{Native, Platform, ensure_owner_only};
use scull_storage::{
    ApprovalMethod, ItemOutcome as AuditOutcome, OperationEnd, OperationStart, Store,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// * `web/node_modules`: npm project without a lockfile → S2, quarantine;
/// * `app/target`: Cargo build output touched within the activity window (FX-CLS-09 bumps it
///   one tier) → S1, quarantine;
/// * `lib/target`: idle Cargo build output → S0, delete.
fn fixture() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    // "Recent" relative to the harness clock (three days ahead).
    let recent = std::time::SystemTime::now()
        + std::time::Duration::from_secs(u64::try_from(THREE_DAYS).unwrap() - 600);
    fx.npm_project("web", "web", NodeLock::None)
        .unwrap()
        .node_modules("web", &["left-pad", "react"])
        .unwrap()
        .file_of_size("web/node_modules/react/big.js", 64 * 1024)
        .unwrap()
        .cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 96 * 1024)
        .unwrap()
        .set_mtime("app/target/debug/app", recent)
        .unwrap()
        .cargo_project("lib", "lib")
        .unwrap()
        .cargo_target("lib", "lib", 4096)
        .unwrap();
    fx.checkpoint().unwrap();
    fx
}

fn store(fx: &Fixture) -> Store {
    Store::open(&fx.base().join("data").join("scull.db"), &PlatformOwnerOnly).unwrap()
}

fn start(plan: &Plan, op: &str) -> OperationStart {
    OperationStart {
        operation_id: OperationId::from_suffix(op).unwrap(),
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        approver: "tester".into(),
        method: ApprovalMethod::CliInteractive,
        started_at: now_unix(),
    }
}

/// The quarantine base used by these tests: the fixture's temp dir, beside the root.
fn settings(fx: &Fixture) -> QuarantineSettings {
    QuarantineSettings {
        base: Some(fx.base().to_path_buf()),
        ..QuarantineSettings::default()
    }
}

/// Plans, begins an operation, executes and finishes it.
fn run(
    fx: &Fixture,
    h: &Harness,
    s: &mut Store,
    rels: &[&str],
    op: &str,
) -> (Plan, ExecutionReport) {
    let scan = h.scan(fx);
    let plan = plan_named(fx, &scan, rels, op);
    s.insert_plan(&plan).unwrap();
    let exec = locked(fx);
    let mut operation: Operation<'_> = exec
        .begin_operation(s, &plan, &start(&plan, op))
        .unwrap()
        .with_quarantine(settings(fx));
    let r = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    print_report(fx, &r);
    (plan, r)
}

fn quarantined<'a>(fx: &Fixture, r: &'a ExecutionReport, rel_path: &str) -> &'a Quarantined {
    let item = r
        .items
        .iter()
        .find(|i| rel(fx, &i.path.to_path_buf()) == rel_path)
        .unwrap();
    match &item.outcome {
        ItemOutcome::Quarantined(q) => q,
        other => panic!("{rel_path} was not quarantined: {other:?}"),
    }
}

fn op_dir(fx: &Fixture, op: &str) -> PathBuf {
    fx.base()
        .join(QUARANTINE_DIR_NAME)
        .join(OperationId::from_suffix(op).unwrap().as_str())
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

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
        e.entry.original_path.to_path_buf(),
        fx.root().join("app/target")
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
    // The on-disk copy exists, owner-only, and lists the entry.
    let copy = std::fs::read_to_string(op_dir(&fx, "Q1").join(DISK_MANIFEST)).unwrap();
    assert!(copy.contains("app/target"), "{copy}");
    check(&fx, &plan, &r, &["app/target", "lib/target"]);
}

/// FX-EXEC-16: the quarantine and operation folders (and the manifest copy) are owner-only;
/// another account can't list or read them. (Reading as a second test user is the nightly
/// half of this fixture.)
#[test]
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
    std::fs::set_permissions(&web, std::fs::Permissions::from_mode(0o555)).unwrap();
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
    std::fs::set_permissions(&web, std::fs::Permissions::from_mode(0o755)).unwrap();
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

/// Whether this process runs as root (it owns what it creates).
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
