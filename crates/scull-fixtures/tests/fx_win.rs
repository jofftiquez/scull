//! End-to-end FX-WIN fixtures (docs/FIXTURES.md §3.10) that fill the gaps left by the
//! layer-level Windows tests (`scull-cleaner/tests/executor_windows.rs`, `trash.rs`,
//! `scull-platform/tests/windows.rs`):
//!
//! * **FX-WIN-01** through the whole pipeline: the engine scans a tree whose Cargo target holds
//!   junctions to the canary, the plan is built from its candidates, the real Executor runs it
//!   inside an audited operation; the junctions go, their targets stay (I-2), the audit matches
//!   the disk (I-4).
//! * **FX-WIN-08**: a scan with Defender real-time protection on (the hosted runner default)
//!   completes; its timing is recorded for the benchmarks.
//! * **FX-WIN-12**: the database lives in the non-roaming local app data folder and is created
//!   owner-only (DACL checked).
//!
//! Windows only. Written while the Windows CI was unavailable: type-checked for the Windows
//! target, pending its first CI run (see the registry).

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod exec;

use exec::engine::{harness, inputs, plan, rel, run, store};
use scull_cleaner::ItemOutcome;
use scull_core::{OperationId, Tier};
use scull_engine::PlatformOwnerOnly;
use scull_fixtures::{Fixture, InvariantId, InvariantInputs, InvariantReport, Outcome};
use scull_platform::{AppDirs, DirLayout, DirSource, ensure_owner_only};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn check(fx: &Fixture, inputs: &InvariantInputs) -> InvariantReport {
    let report = fx.check_invariants(inputs);
    println!("{report}");
    report.assert_ok();
    report
}

/// FX-WIN-01 (end to end): junctions inside an idle Cargo target, one to the canary's junction
/// target and one to the canary itself. Planned from the engine's candidate and executed:
/// the item leaves its place (deleted at S0, quarantined above), the junctions with it, and
/// both targets keep every byte (I-2); the audit has one outcome consistent with the disk
/// (I-4).
#[test]
fn fx_win_01_junction_inside_candidate_end_to_end_target_intact() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("lib", "lib")
        .unwrap()
        .cargo_target("lib", "lib", 8 * 1024)
        .unwrap();
    let target = fx.canary().junction_target.clone();
    let canary = fx.canary().dir.clone();
    assert!(
        fx.junction("lib/target/debug/escape", &target)
            .unwrap()
            .is_created()
    );
    assert!(fx.junction("lib/target/up", &canary).unwrap().is_created());
    fx.checkpoint().unwrap();

    let h = harness();
    let scan = h.scan(&fx);
    let p = plan(&fx, &scan, &["lib/target"], "PXW01");
    let tier = p.items()[0].tier;
    println!("lib/target planned at {tier}");
    assert!(tier.is_actionable(), "{tier}");
    let mut s = store(&fx);
    let op = OperationId::from_suffix("OXW01").unwrap();
    let r = run(&fx, &h, &mut s, &p, &op);
    match &r.items[0].outcome {
        ItemOutcome::Deleted(d) => {
            assert_eq!(tier, Tier::S0);
            println!("deleted: {:?} {:?}", d.outcome, d.removed);
            assert!(d.removed.symlinks >= 2, "{d:#?}");
        }
        ItemOutcome::Quarantined(q) => {
            println!("quarantined at {}", q.quarantine_path.display());
        }
        other => panic!("lib/target was neither deleted nor quarantined: {other:?}"),
    }
    assert!(std::fs::symlink_metadata(fx.root().join("lib/target")).is_err());
    assert_eq!(
        std::fs::read(target.join("keep.txt")).unwrap(),
        b"junction target content\n"
    );
    let inv = check(&fx, &inputs(&p, &r, &s, &op, &["lib/target"]));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
    assert!(inv.outcome(InvariantId::I2).ran());
}

/// Defender's real-time protection state, as `Get-MpComputerStatus` reports it, or why it is
/// unknown. Read-only; only for the timing record.
fn defender_state() -> String {
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-MpComputerStatus).RealTimeProtectionEnabled",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_owned();
            match s.as_str() {
                "True" => "on".into(),
                "False" => "off".into(),
                other => format!("unknown ({other:?})"),
            }
        }
        Ok(o) => format!(
            "unknown (Get-MpComputerStatus failed: {})",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => format!("unknown ({e})"),
    }
}

/// FX-WIN-08: a tree of 40 projects with build outputs full of `.exe`, `.dll` and `.pdb` files
/// (what Defender inspects on access) is scanned by the engine with real-time protection in its
/// default state. The scan completes (not cancelled, every file counted, every target found)
/// and its wall-clock time is recorded in `target/tmp/fx-win-08-timing.json` and the test
/// output for the benchmarks (docs/BENCHMARKS.md).
#[test]
fn fx_win_08_scan_completes_with_defender_on_and_timing_is_recorded() {
    let mut fx = Fixture::new().unwrap();
    let projects = 40;
    let per_target = 25;
    for i in 0..projects {
        let d = format!("p{i:02}");
        fx.cargo_project(&d, &d)
            .unwrap()
            .cargo_target(&d, &d, 4096)
            .unwrap();
        for j in 0..per_target {
            let ext = ["exe", "dll", "pdb", "rlib", "d"][j % 5];
            fx.file_of_size(format!("{d}/target/debug/deps/f{j}.{ext}"), 4096)
                .unwrap();
        }
    }
    fx.checkpoint().unwrap();
    let defender = defender_state();
    let h = harness();
    let t = Instant::now();
    let r = h.scan(&fx);
    let elapsed = t.elapsed();
    assert!(!r.cancelled);
    let files = (projects * (per_target + 3 + 6)) as u64;
    assert!(
        r.entries_scanned >= files,
        "{} entries scanned, expected at least {files}",
        r.entries_scanned
    );
    let targets = r
        .candidates
        .iter()
        .filter(|c| rel(&fx, &c.path.to_path_buf()).ends_with("/target"))
        .count();
    assert_eq!(targets, projects, "every target found");
    let record = serde_json::json!({
        "fixture": "FX-WIN-08",
        "defender_real_time_protection": defender,
        "entries_scanned": r.entries_scanned,
        "candidates": r.candidates.len(),
        "elapsed_ms": elapsed.as_millis() as u64,
        "entries_per_sec": (r.entries_scanned as f64 / elapsed.as_secs_f64().max(1e-6)) as u64,
    });
    println!("FX-WIN-08 timing: {record}");
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fx-win-08-timing.json");
    std::fs::write(&out, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    println!("recorded in {}", out.display());
    check(&fx, &InvariantInputs::new());
}

/// Whether `inner` is `outer` or below it, comparing case-insensitively (Windows paths).
fn is_under(inner: &Path, outer: &Path) -> bool {
    let low = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
    let (i, o) = (low(inner), low(outer));
    let o = o.trim_end_matches('\\');
    i == o || i.starts_with(&format!("{o}\\"))
}

/// FX-WIN-12: scull's data directory is in the non-roaming local app data folder (never in
/// `%APPDATA%\Roaming`): for this machine's real known folders (resolved, nothing created) and
/// for an injected profile in a temp dir, where the database is then created and checked
/// owner-only (DACL: the owner only, no inherited or broad grants) with its side files.
#[test]
fn fx_win_12_database_in_local_app_data_with_owner_only_acl() {
    let mut fx = Fixture::new().unwrap();
    fx.checkpoint().unwrap();

    // The real profile: paths only.
    if std::env::var_os("SCULL_HOME").is_some_and(|v| !v.is_empty()) {
        println!("SCULL_HOME is set: the real known-folder resolution is not checked here");
    } else {
        let dirs = scull_platform::app_dirs().unwrap();
        println!("real app dirs: {dirs:?}");
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            assert!(
                is_under(&dirs.data, Path::new(&local)),
                "{} not under LOCALAPPDATA {}",
                dirs.data.display(),
                Path::new(&local).display()
            );
        }
        if let Some(roaming) = std::env::var_os("APPDATA") {
            assert!(!is_under(&dirs.data, Path::new(&roaming)), "{dirs:?}");
            assert!(!is_under(&dirs.state, Path::new(&roaming)), "{dirs:?}");
        }
    }

    // An injected profile: create the database and check its ACL.
    let local = fx.base().join("profile").join("AppData").join("Local");
    let roaming = fx.base().join("profile").join("AppData").join("Roaming");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&roaming).unwrap();
    let (l, ro) = (local.clone(), roaming.clone());
    let dirs = AppDirs::resolve(DirLayout::Windows, move |src| match src {
        DirSource::LocalAppData => Some(OsString::from(l.clone())),
        DirSource::RoamingAppData => Some(OsString::from(ro.clone())),
        _ => None,
    })
    .unwrap();
    assert!(is_under(&dirs.data, &local), "{dirs:?}");
    assert!(!is_under(&dirs.data, &roaming), "{dirs:?}");
    let db = dirs.data.join(scull_storage::DB_FILE_NAME);
    let store = scull_storage::Store::open(&db, &PlatformOwnerOnly).unwrap();
    let mut checked = Vec::new();
    for p in [
        dirs.data.clone(),
        db.clone(),
        db.with_extension("db-wal"),
        db.with_extension("db-shm"),
    ] {
        if p.exists() {
            ensure_owner_only(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            checked.push(p);
        }
    }
    drop(store);
    println!("owner-only: {checked:#?}");
    assert!(checked.len() >= 2, "the data directory and the database");
    check(&fx, &InvariantInputs::new());
}
