//! End-to-end FX-LNX fixtures (docs/FIXTURES.md §3.12) through the real engine and Executor:
//!
//! * **FX-LNX-01**: a headless server with no Trash: an S1 item is quarantined (the default
//!   path never needs a Trash); where quarantine is impossible too, the item is blocked, never
//!   deleted. Audited (I-4).
//! * **FX-LNX-04**: `/proc` open-file detection through the engine's native in-use probe: a
//!   file held open by another process inside a candidate makes it "in use"; processes this
//!   user may not inspect are disclosed (strict policy: the answer is unknown, never "idle").
//!
//! The Trash half of FX-LNX-01 at the platform layer is in `scull-cleaner/tests/trash.rs`.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod exec;

use exec::engine::{
    at, ctx, harness, inputs, locked, plan, print_report, request, start, store, three_tiers,
};
use scull_cleaner::trash::TrashLocations;
use scull_cleaner::{ExecutionReport, ItemOutcome, QuarantineSettings};
use scull_core::config::Config;
use scull_core::{Evidence, OperationId, Plan, Tier};
use scull_engine::paths::canon;
use scull_engine::{CancelToken, Engine, EngineConfig, InUseSource, now_unix};
use scull_fixtures::{Fixture, InvariantId, InvariantReport, Outcome};
use scull_platform::Native;
use scull_safety::GapPolicy;
use scull_storage::{ItemOutcome as AuditOutcome, OperationEnd, Store};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn check(fx: &Fixture, inputs: &scull_fixtures::InvariantInputs) -> InvariantReport {
    let report = fx.check_invariants(inputs);
    println!("{report}");
    report.assert_ok();
    report
}

/// Whether permission bits bind this process (not root): a write into `dir` (made `0555` by
/// the caller) must fail.
fn permissions_enforced(dir: &Path) -> bool {
    let probe = dir.join(".scull-probe");
    match fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            false
        }
        Err(_) => true,
    }
}

/// Set `p`'s mode; returns the previous permissions, to restore with [`restore`].
fn chmod(p: &Path, mode: u32) -> fs::Permissions {
    let before = fs::metadata(p).unwrap().permissions();
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
    before
}

fn restore(p: &Path, perms: fs::Permissions) {
    fs::set_permissions(p, perms).unwrap();
}

/// A headless account: no home Trash, and the volume top (the fixture root) is not writable,
/// so no `$topdir/.Trash-$uid` can be made (as `/` is for a user on a server).
fn headless(fx: &Fixture) -> TrashLocations {
    TrashLocations {
        home: None,
        topdir: Some(fx.root().to_path_buf()),
    }
}

/// Record `plan`, begin operation `op` with `quarantine` and the headless Trash, execute it
/// and finish.
fn run_headless(
    fx: &Fixture,
    s: &mut Store,
    p: &Plan,
    op: &OperationId,
    quarantine: QuarantineSettings,
) -> ExecutionReport {
    let h = harness();
    s.insert_plan(p).unwrap();
    let exec = locked(fx);
    let mut operation = exec
        .begin_operation(s, p, &start(p, op))
        .unwrap()
        .with_quarantine(quarantine)
        .with_trash(headless(fx));
    let r = exec
        .execute_plan(
            p,
            &ctx(),
            &h.revalidation(fx),
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    print_report(fx, &r);
    r
}

fn trash_dir(fx: &Fixture) -> std::path::PathBuf {
    use std::os::unix::fs::MetadataExt;
    fx.root()
        .join(format!(".Trash-{}", fs::metadata(fx.root()).unwrap().uid()))
}

/// FX-LNX-01: no Trash anywhere; an S1 item planned for quarantine is quarantined (same
/// volume, beside the root), the Trash is never needed or created, the audit says
/// "quarantined" (I-4).
#[test]
fn fx_lnx_01_headless_server_without_trash_uses_quarantine() {
    let fx = three_tiers();
    let h = harness();
    let scan = h.scan(&fx);
    let p = plan(&fx, &scan, &["app/target"], "PXL01A");
    assert_eq!(p.items()[0].tier, Tier::S1);
    let mut s = store(&fx);
    let op = OperationId::from_suffix("OXL01A").unwrap();
    let root_perms = chmod(fx.root(), 0o555);
    let r = run_headless(
        &fx,
        &mut s,
        &p,
        &op,
        QuarantineSettings {
            base: Some(fx.base().to_path_buf()),
            ..QuarantineSettings::default()
        },
    );
    restore(fx.root(), root_perms);
    let ItemOutcome::Quarantined(q) = &r.items[0].outcome else {
        panic!("not quarantined: {:?}", r.items[0].outcome)
    };
    println!("FX-LNX-01: quarantined at {}", q.quarantine_path.display());
    assert!(q.quarantine_path.starts_with(fx.base()));
    assert!(q.quarantine_path.exists());
    assert!(!fx.root().join("app/target").exists());
    assert!(!trash_dir(&fx).exists(), "no Trash was needed or created");
    let audit = s.audit_items(&op).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].outcome, AuditOutcome::Quarantined);
    let inv = check(&fx, &inputs(&p, &r, &s, &op, &["app/target"]));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
}

/// FX-LNX-01: no Trash, and quarantine is impossible too (the quarantine location can't be
/// created): the item is blocked, left in place, never deleted; the audit records the block
/// (I-4).
#[test]
fn fx_lnx_01_headless_server_quarantine_impossible_item_blocked() {
    let fx = three_tiers();
    let h = harness();
    let scan = h.scan(&fx);
    let p = plan(&fx, &scan, &["app/target"], "PXL01B");
    let mut s = store(&fx);
    let op = OperationId::from_suffix("OXL01B").unwrap();
    let unwritable = fx.base().join("no-quarantine-here");
    fs::create_dir(&unwritable).unwrap();
    let unwritable_perms = chmod(&unwritable, 0o555);
    let root_perms = chmod(fx.root(), 0o555);
    if !permissions_enforced(&unwritable) || !permissions_enforced(fx.root()) {
        restore(fx.root(), root_perms);
        restore(&unwritable, unwritable_perms);
        println!("SKIP: permissions are not enforced for this user (root)");
        check(&fx, &scull_fixtures::InvariantInputs::new());
        return;
    }
    let r = run_headless(
        &fx,
        &mut s,
        &p,
        &op,
        QuarantineSettings {
            base: Some(unwritable.clone()),
            ..QuarantineSettings::default()
        },
    );
    restore(fx.root(), root_perms);
    restore(&unwritable, unwritable_perms);
    let ItemOutcome::Blocked { code, message } = &r.items[0].outcome else {
        panic!("not blocked: {:?}", r.items[0].outcome)
    };
    println!("FX-LNX-01: blocked [{code}] {message}");
    assert_eq!(*code, "quarantine_and_trash_impossible");
    assert!(message.contains("never deleted"), "{message}");
    assert!(
        fx.root().join("app/target/debug/app").is_file(),
        "the item is left in place"
    );
    assert!(!trash_dir(&fx).exists());
    let audit = s.audit_items(&op).unwrap();
    assert_eq!(audit.len(), 1, "{audit:?}");
    println!("  audit: {}", audit[0].outcome.as_str());
    assert_ne!(audit[0].outcome, AuditOutcome::Removed);
    assert_ne!(audit[0].outcome, AuditOutcome::Quarantined);
    let inv = check(&fx, &inputs(&p, &r, &s, &op, &[]));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
}

// ------------------------------------------------------------------------------------------
// FX-LNX-04
// ------------------------------------------------------------------------------------------

const CHILD_ENV: &str = "SCULL_FX_LNX_CHILD";
const HOLD_ENV: &str = "SCULL_FX_LNX_HOLD";
const READY_ENV: &str = "SCULL_FX_LNX_READY";

/// Not a fixture: the body of the process FX-LNX-04 spawns (this test binary re-executed). It
/// holds `SCULL_FX_LNX_HOLD` open and sleeps; idle unless `SCULL_FX_LNX_CHILD` is set.
#[test]
#[ignore = "helper process for FX-LNX-04"]
fn lnx_sleeper_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let _held = std::env::var_os(HOLD_ENV).map(|p| fs::File::open(p).expect("open held file"));
    if let Some(ready) = std::env::var_os(READY_ENV) {
        fs::write(ready, "ready").expect("write ready marker");
    }
    std::thread::sleep(Duration::from_secs(120));
}

/// Kills the child on drop.
struct Sleeper(Child);

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the sleeper with its working directory in `cwd`, holding `hold` open.
fn spawn_holder(cwd: &Path, hold: &Path) -> Sleeper {
    let ready_dir = tempfile::tempdir().unwrap();
    let ready = ready_dir.path().join("ready");
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lnx_sleeper_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, "1")
        .env(READY_ENV, &ready)
        .env(HOLD_ENV, hold)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let child = Sleeper(child);
    let t = Instant::now();
    while !ready.exists() {
        assert!(t.elapsed() < Duration::from_secs(60), "sleeper not ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

fn native_engine(policy: GapPolicy) -> Engine {
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Native(policy);
    Engine::new(cfg).unwrap()
}

/// FX-LNX-04: another process holds a file open inside `lib/target` (its working directory is
/// elsewhere, so only `/proc/<pid>/fd` shows it). The engine's native probe finds it: the
/// candidate is "in use" (never S0) with the evidence. Processes this user may not inspect
/// (other users', when not root) are reported: under the strict policy an otherwise idle
/// candidate is "unknown" (S2, `signal_unavailable`) and the report lists the gap; never
/// "idle".
#[test]
fn fx_lnx_04_proc_open_file_detection_through_the_engine() {
    let fx = three_tiers();
    let held = fx.root().join("lib/target/debug/lib");
    assert!(held.is_file());
    let elsewhere = tempfile::tempdir().unwrap();
    let child = spawn_holder(elsewhere.path(), &held);
    let roots = [canon(fx.root())];

    for policy in [GapPolicy::OwnUser, GapPolicy::Strict] {
        let r = native_engine(policy)
            .scan(&roots, &request(), &CancelToken::new(), |_| {})
            .unwrap();
        let lib = at(&fx, &r, "lib/target");
        println!(
            "{policy:?}: lib/target {} evidence {:?}",
            lib.tier, lib.evidence
        );
        assert!(
            lib.evidence
                .iter()
                .any(|e| matches!(e, Evidence::InUse { .. })),
            "{policy:?}: held file not detected: {:?}",
            lib.evidence
        );
        assert!(lib.tier > Tier::S0, "{policy:?}: in use is never S0");
        let web = at(&fx, &r, "web/node_modules");
        println!(
            "{policy:?}: signals unavailable {:?}",
            r.signals_unavailable
        );
        if policy == GapPolicy::Strict {
            let snap = scull_platform::activity::Snapshot::capture(Default::default());
            let gaps = snap.cwds_under(fx.root());
            if gaps.denied_other > 0 || gaps.denied_own > 0 {
                assert!(
                    web.evidence
                        .iter()
                        .any(|e| matches!(e, Evidence::SignalUnavailable { .. })),
                    "uninspected processes never read as idle: {:?}",
                    web.evidence
                );
                assert!(web.tier >= Tier::S2, "{}", web.tier);
                assert!(!r.signals_unavailable.is_empty());
            } else {
                println!("full process visibility (root): no permission gap to report");
            }
        }
    }
    drop(child);
    // The held file was only read by the child; nothing changed.
    check(&fx, &scull_fixtures::InvariantInputs::new());
}
