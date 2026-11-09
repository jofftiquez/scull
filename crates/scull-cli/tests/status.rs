//! `scull status [--short]` end to end (P1-CLI-04). Every run points `SCULL_HOME` at a fresh
//! temp dir; the database is seeded through `scull-storage`.
// A command outside the personal MVP: tested only in the full build (#142).
#![cfg(feature = "full")]

use scull_core::{Category, FsPath, ScanId, Tier};
use scull_storage::{
    DefaultPermissions, Rollup, ScanFinish, ScanRecord, ScanStart, ScanStatus, ScanTotals, Store,
    VolumeSample,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GIB: u64 = 1024 * 1024 * 1024;

fn scull_in(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args).env("SCULL_HOME", home).env("TERM", "dumb");
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            cmd.env_remove(k);
        }
    }
    cmd.output().expect("run scull")
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("i64")
}

fn db_path(home: &Path) -> PathBuf {
    home.join("data").join(scull_storage::DB_FILE_NAME)
}

/// Seed `scans` completed scans, the last one finished two hours ago.
fn seed(home: &Path, scans: usize) {
    let mut store = Store::open(&db_path(home), &DefaultPermissions).expect("open");
    let last_finish = now() - 2 * 3_600;
    for i in 0..scans {
        let finished_at = last_finish - i64::try_from(scans - 1 - i).expect("n") * 86_400;
        let scan_id = ScanId::from_suffix(&format!("s{i}")).expect("id");
        store
            .record_scan_start(&ScanStart {
                scan_id: scan_id.clone(),
                started_at: finished_at - 60,
                roots: vec![FsPath::from("/home/u/Projects")],
            })
            .expect("start");
        let rollup = |tier: Tier, bytes: u64| Rollup {
            category: Category::ProjectArtifact,
            tier,
            ecosystem: Some("node".to_owned()),
            items: 1,
            logical_bytes: bytes,
            allocated_bytes: bytes,
            reclaimable_bytes: bytes,
        };
        store
            .record_scan(&ScanRecord {
                finish: ScanFinish {
                    scan_id,
                    finished_at,
                    status: ScanStatus::Completed,
                    totals: ScanTotals {
                        entries_scanned: 1_000,
                        candidates: 2,
                        logical_bytes: 18 * GIB,
                        allocated_bytes: 18 * GIB,
                        reclaimable_bytes: 18 * GIB,
                        errors: 0,
                    },
                    rollups: vec![rollup(Tier::S0, 12 * GIB), rollup(Tier::S1, 6 * GIB)],
                },
                candidates: &[],
                volumes: &[VolumeSample {
                    volume_id: "/".to_owned(),
                    total: 500 * GIB,
                    free: 41 * GIB,
                    at: finished_at,
                }],
            })
            .expect("record");
    }
}

fn validator() -> jsonschema::Validator {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/status.schema.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("schema file"))
        .expect("schema JSON");
    jsonschema::validator_for(&v).expect("compiles")
}

fn status_json(home: &Path) -> Value {
    let o = scull_in(home, &["status", "--json"]);
    assert_eq!(o.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    let errors: Vec<String> = validator().iter_errors(&v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
    v
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test status`.
#[test]
fn committed_schema_matches_generated() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/status.schema.json");
    let generated = scull_cli::output::json::document_schema::<scull_cli::cmd::status::StatusReport>(
        scull_cli::cmd::status::STATUS_SCHEMA,
    );
    let text = serde_json::to_string_pretty(&generated).expect("serialize") + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(&path, &text).expect("write");
    }
    let committed = std::fs::read_to_string(&path).expect("schema file");
    assert_eq!(
        committed.replace("\r\n", "\n"),
        text,
        "schemas/status.schema.json is stale; rerun with SCULL_BLESS=1"
    );
}

#[test]
fn seeded_database_prints_one_line() {
    let home = tempfile::tempdir().expect("tempdir");
    seed(home.path(), 3);

    let o = scull_in(home.path(), &["status"]);
    assert_eq!(o.status.code(), Some(0));
    let line = String::from_utf8(o.stdout).expect("utf-8");
    assert_eq!(
        line,
        "scull: 18.0 GiB reclaimable (12.0 GiB safe) \u{b7} 41.0 GiB free on / \u{b7} last scan 2h ago\n"
    );
    assert!(o.stderr.is_empty());

    let o = scull_in(home.path(), &["status", "--short"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(o.stdout).expect("utf-8"),
        "\u{2620} 41G free \u{b7} 2h \u{b7} 18G reclaimable\n"
    );

    let v = status_json(home.path());
    assert_eq!(v["schema"], "scull/status@1");
    let d = &v["data"];
    assert_eq!(d["state"], "ok");
    assert_eq!(d["last_scan"]["scan_id"], "scan_s2");
    assert_eq!(d["last_scan"]["reclaimable_bytes"], 18 * GIB);
    assert_eq!(d["last_scan"]["safe_reclaimable_bytes"], 12 * GIB);
    assert_eq!(d["volumes"][0]["free_bytes"], 41 * GIB);
}

#[test]
fn nothing_recorded_is_silent_for_prompts_and_creates_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let o = scull_in(home.path(), &["status", "--short"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(o.stdout.is_empty(), "prompt output must be empty");
    assert!(o.stderr.is_empty(), "no error noise in a prompt");

    let o = scull_in(home.path(), &["status"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        String::from_utf8(o.stdout)
            .expect("utf-8")
            .contains("no scan recorded yet")
    );
    let v = status_json(home.path());
    assert_eq!(v["data"]["state"], "no_database");
    assert!(v["data"]["last_scan"].is_null());
    assert_eq!(v["data"]["short"], "");
    assert_eq!(
        std::fs::read_dir(home.path()).expect("dir").count(),
        0,
        "status must not create the database or its directory"
    );

    // An empty database (no scan yet) is just as quiet.
    drop(Store::open(&db_path(home.path()), &DefaultPermissions).expect("open"));
    let o = scull_in(home.path(), &["status", "--short"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(o.stdout.is_empty() && o.stderr.is_empty());
    assert_eq!(status_json(home.path())["data"]["state"], "no_scan");
}

#[test]
fn unusable_database_is_silent_for_prompts() {
    let home = tempfile::tempdir().expect("tempdir");
    let db = db_path(home.path());
    std::fs::create_dir_all(db.parent().expect("parent")).expect("dir");
    std::fs::write(&db, b"not sqlite").expect("junk");
    let o = scull_in(home.path(), &["status", "--short"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(o.stdout.is_empty() && o.stderr.is_empty());
    let o = scull_in(home.path(), &["status"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        String::from_utf8(o.stdout)
            .expect("utf-8")
            .contains("status unavailable")
    );
    assert_eq!(status_json(home.path())["data"]["state"], "unavailable");
    assert_eq!(std::fs::read(&db).expect("db"), b"not sqlite");
}

/// PRO-09 budget: under 50 ms from the cache. Measured in process (open + one query, what
/// `scull status` does after argument parsing), cold: the first call in a fresh process
/// against a database with a year of daily scans. Debug builds of the bundled SQLite are
/// unoptimized, so the release timing is shown in the evidence; this guards against
/// regressions such as an accidental scan or migration.
#[test]
fn status_is_fast_from_cache() {
    let home = tempfile::tempdir().expect("tempdir");
    seed(home.path(), 365);
    let db = db_path(home.path());

    // Open the database and build the report from scratch, best of three: a single sample is
    // at the mercy of the scheduler when the machine is loaded (seen at load average ~90).
    let cold = (0..3)
        .map(|_| {
            let start = Instant::now();
            let store = scull_cli::db::open_current(&db).expect("current database");
            let summary = store.status_summary().expect("summary");
            let report = scull_cli::cmd::status::from_summary(&summary, now());
            let took = start.elapsed();
            assert_eq!(report.last_scan.expect("scan").scan_id, "scan_s364");
            took
        })
        .min()
        .expect("runs");
    assert!(
        cold < Duration::from_millis(50),
        "status from a fresh database handle took {cold:?} (best of 3)"
    );

    // The whole process, best of three (spawn cost varies on CI runners).
    let best = (0..3)
        .map(|_| {
            let t = Instant::now();
            let o = scull_in(home.path(), &["status", "--short"]);
            assert_eq!(o.status.code(), Some(0));
            t.elapsed()
        })
        .min()
        .expect("runs");
    eprintln!(
        "status timing: in-process cold {cold:?}; `scull status --short` process best of 3 {best:?}"
    );
    assert!(
        best < Duration::from_millis(500),
        "scull status --short took {best:?}"
    );
}

/// Evidence helper, not part of the suite: seeds `$SCULL_DEMO_HOME` like a real install
/// (owner-only directories, a week of scans, a user config) for screenshots.
/// `SCULL_DEMO_HOME=target/demo cargo test -p scull-cli --test status -- --ignored`
#[test]
#[ignore = "evidence helper"]
fn seed_demo_home() {
    let Some(home) = std::env::var_os("SCULL_DEMO_HOME").map(PathBuf::from) else {
        return;
    };
    for d in ["config", "data", "cache", "state"] {
        scull_platform::create_dir_owner_only(&home.join(d)).expect("dir");
    }
    let mut f =
        scull_platform::create_file_owner_only(&home.join("config/config.toml")).expect("config");
    std::io::Write::write_all(
        &mut f,
        b"[scan]\nroots = [\"~/Projects\", \"~/work\"]\nprotect = [\"~/Projects/thesis\"]\n",
    )
    .expect("write");
    drop(Store::open(&db_path(&home), &PlatformPerms).expect("db"));
    seed(&home, 7);
}

/// scull-platform's owner-only primitives behind scull-storage's trait.
struct PlatformPerms;

impl scull_storage::OwnerOnly for PlatformPerms {
    fn create_dir(&self, path: &Path) -> std::io::Result<()> {
        scull_platform::owner_only::create_dir(path)
    }
    fn create_file(&self, path: &Path) -> std::io::Result<()> {
        scull_platform::owner_only::create_file(path).map(drop)
    }
    fn restrict(&self, path: &Path) -> std::io::Result<()> {
        scull_platform::owner_only::restrict(path)
    }
}
