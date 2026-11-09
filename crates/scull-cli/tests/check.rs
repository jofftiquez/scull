//! `scull check` and the forecast in `scull status` end to end (P4-PRO-01). Every run points
//! `SCULL_HOME` at a fresh temp dir; history is seeded through `scull-storage`.
//!
//! The seeded volume id is not a real mount point on any OS, so `scull check` reports it as
//! not connected instead of sampling it, and the forecast uses only the seeded history.
// A command outside the personal MVP: tested only in the full build (#142).
#![cfg(feature = "full")]

use scull_core::{FsPath, ScanId};
use scull_storage::{
    DefaultPermissions, ScanFinish, ScanRecord, ScanStart, ScanStatus, ScanTotals, Store,
    VolumeSample,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const GIB: u64 = 1024 * 1024 * 1024;
const VOL: &str = "scull-test-volume";

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

/// 21 daily samples of `VOL` ending two hours ago, losing 4 GiB a day down to 46 GiB free:
/// (46 − 10) / 4 = 9 days to the 10 GiB reserve. With `scan`, the last sample comes with a
/// completed scan (so `scull status` has something to show).
fn seed(home: &Path, scan: bool) {
    let mut store = Store::open(&db_path(home), &DefaultPermissions).expect("open");
    let last = now() - 2 * 3_600;
    for i in 0..=20u64 {
        let at = last - i64::try_from(20 - i).expect("n") * 86_400;
        let sample = VolumeSample {
            volume_id: VOL.to_owned(),
            total: 500 * GIB,
            free: (46 + 4 * (20 - i)) * GIB,
            at,
        };
        if scan && i == 20 {
            let scan_id = ScanId::from_suffix("c1").expect("id");
            store
                .record_scan_start(&ScanStart {
                    scan_id: scan_id.clone(),
                    started_at: at - 60,
                    roots: vec![FsPath::from("/home/u/Projects")],
                })
                .expect("start");
            store
                .record_scan(&ScanRecord {
                    finish: ScanFinish {
                        scan_id,
                        finished_at: at,
                        status: ScanStatus::Completed,
                        totals: ScanTotals {
                            entries_scanned: 10,
                            candidates: 1,
                            logical_bytes: 18 * GIB,
                            allocated_bytes: 18 * GIB,
                            reclaimable_bytes: 18 * GIB,
                            errors: 0,
                        },
                        rollups: Vec::new(),
                    },
                    candidates: &[],
                    volumes: &[sample],
                })
                .expect("scan");
        } else {
            store.record_free_space(&[sample], None).expect("sample");
        }
    }
}

fn validate(schema: &str, v: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("schemas")
        .join(schema);
    let s: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("schema file"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&s).expect("compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test check`.
#[test]
fn committed_schema_matches_generated() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/check.schema.json");
    let generated = scull_cli::output::json::document_schema::<scull_cli::cmd::check::CheckReport>(
        scull_cli::cmd::check::CHECK_SCHEMA,
    );
    let text = serde_json::to_string_pretty(&generated).expect("serialize") + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(&path, &text).expect("write");
    }
    let committed = std::fs::read_to_string(&path).expect("schema file");
    assert_eq!(
        committed.replace("\r\n", "\n"),
        text,
        "schemas/check.schema.json is stale; rerun with SCULL_BLESS=1"
    );
}

#[test]
fn check_forecasts_and_alerts() {
    let home = tempfile::tempdir().expect("tempdir");
    seed(home.path(), false);

    let o = scull_in(home.path(), &["check", "--json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    assert_eq!(v["schema"], "scull/check@1");
    let d = &v["data"];
    let f = d["forecasts"]
        .as_array()
        .expect("forecasts")
        .iter()
        .find(|f| f["volume_id"] == VOL)
        .expect("forecast of the seeded volume");
    assert_eq!(f["state"], "filling");
    assert_eq!(f["confidence"], "high");
    assert_eq!(f["days_until_threshold"], 9);
    assert_eq!(f["fill_bytes_per_day"], 4 * GIB);
    assert_eq!(f["samples"], 21);
    let alert = d["alerts"]
        .as_array()
        .expect("alerts")
        .iter()
        .find(|a| a["subject"] == VOL)
        .expect("alert for the seeded volume");
    assert_eq!(alert["kind"], "disk_full_forecast");
    assert_eq!(alert["severity"], "warning");
    assert_eq!(alert["key"], format!("disk_full_forecast:{VOL}"));
    assert!(
        d["not_sampled"]
            .as_array()
            .expect("not_sampled")
            .iter()
            .any(|m| m["volume_id"] == VOL && m["reason"] == "not connected"),
        "{d:#}"
    );

    let o = scull_in(home.path(), &["check"]);
    assert_eq!(o.status.code(), Some(0));
    let text = String::from_utf8(o.stdout).expect("utf-8");
    assert!(
        text.lines().count() >= 4,
        "one line per volume and alert: {text:?}"
    );
    assert!(!text.contains("\\n"), "newlines are not escaped: {text:?}");
    assert!(
        text.contains(&format!(
            "warning: {VOL} will drop below 10.0 GiB free in about 9 days (high confidence)"
        )),
        "{text}"
    );
    assert!(
        text.contains("below 10.0 GiB free in ~9 days (high confidence)"),
        "{text}"
    );
}

#[test]
fn check_on_an_empty_home_records_nothing_and_succeeds() {
    let home = tempfile::tempdir().expect("tempdir");
    let o = scull_in(home.path(), &["check", "--json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    assert_eq!(v["data"]["alerts"], Value::Array(Vec::new()));
}

#[test]
fn status_shows_a_confident_forecast() {
    let home = tempfile::tempdir().expect("tempdir");
    seed(home.path(), true);

    let o = scull_in(home.path(), &["status"]);
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(o.stdout).expect("utf-8"),
        format!(
            "scull: 18.0 GiB reclaimable (0 B safe) \u{b7} 46.0 GiB free on {VOL} \u{b7} full in ~9 days \u{b7} last scan 2h ago\n"
        )
    );
    let o = scull_in(home.path(), &["status", "--short"]);
    assert_eq!(
        String::from_utf8(o.stdout).expect("utf-8"),
        "\u{2620} 46G free \u{b7} full ~9d \u{b7} 2h \u{b7} 18G reclaimable\n"
    );
    let o = scull_in(home.path(), &["status", "--json"]);
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("status.schema.json", &v);
    assert_eq!(v["data"]["forecasts"][0]["days_until_threshold"], 9);
    assert_eq!(v["data"]["forecasts"][0]["confidence"], "high");
}

/// `[proactive]` thresholds from the user config drive the budget and free-space drop alerts
/// (P4-PRO-02).
#[test]
fn check_raises_budget_and_drop_alerts_from_config() {
    let home = tempfile::tempdir().expect("tempdir");
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).expect("config dir");
    std::fs::write(
        config.join("config.toml"),
        "[proactive]\nspike_gib = 30\n\n[[proactive.budgets]]\ncategory = \"container\"\nmax_gib = 50\n",
    )
    .expect("config");
    let mut store = Store::open(&db_path(home.path()), &DefaultPermissions).expect("open");
    let at = now() - 3_600;
    let scan_id = ScanId::from_suffix("b1").expect("id");
    store
        .record_scan_start(&ScanStart {
            scan_id: scan_id.clone(),
            started_at: at - 60,
            roots: vec![FsPath::from("/home/u")],
        })
        .expect("start");
    store
        .record_scan(&ScanRecord {
            finish: ScanFinish {
                scan_id,
                finished_at: at,
                status: ScanStatus::Completed,
                totals: ScanTotals {
                    entries_scanned: 1,
                    candidates: 1,
                    logical_bytes: 70 * GIB,
                    allocated_bytes: 70 * GIB,
                    reclaimable_bytes: 70 * GIB,
                    errors: 0,
                },
                rollups: vec![scull_storage::Rollup {
                    category: scull_core::Category::Container,
                    tier: scull_core::Tier::S1,
                    ecosystem: None,
                    items: 1,
                    logical_bytes: 70 * GIB,
                    allocated_bytes: 70 * GIB,
                    reclaimable_bytes: 70 * GIB,
                }],
            },
            candidates: &[],
            volumes: &[VolumeSample {
                volume_id: VOL.to_owned(),
                total: 500 * GIB,
                free: 160 * GIB,
                at,
            }],
        })
        .expect("scan");
    store
        .record_free_space(
            &[VolumeSample {
                volume_id: VOL.to_owned(),
                total: 500 * GIB,
                free: 200 * GIB,
                at: at - 19 * 3_600,
            }],
            None,
        )
        .expect("sample");
    drop(store);

    let o = scull_in(home.path(), &["check", "--json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    let alerts = v["data"]["alerts"].as_array().expect("alerts");
    let keys: Vec<&str> = alerts.iter().filter_map(|a| a["key"].as_str()).collect();
    assert!(keys.contains(&"budget_exceeded:container"), "{keys:?}");
    let drop_key = format!("free_space_drop:{VOL}");
    assert!(keys.contains(&drop_key.as_str()), "{keys:?}");
    let drop = alerts
        .iter()
        .find(|a| a["kind"] == "free_space_drop")
        .expect("drop");
    assert_eq!(drop["bytes"], 40 * GIB);

    // Raise the spike threshold above the drop: only the budget remains.
    std::fs::write(
        config.join("config.toml"),
        "[proactive]\nspike_gib = 50\nspike_percent = 0\n\n[[proactive.budgets]]\ncategory = \"container\"\nmax_gib = 50\n",
    )
    .expect("config");
    let o = scull_in(home.path(), &["check", "--json"]);
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    let kinds: Vec<&str> = v["data"]["alerts"]
        .as_array()
        .expect("alerts")
        .iter()
        .filter_map(|a| a["kind"].as_str())
        .collect();
    assert_eq!(kinds, vec!["budget_exceeded"]);
    let text = String::from_utf8(scull_in(home.path(), &["check"]).stdout).expect("utf-8");
    assert!(
        text.contains("warning: container uses 70.0 GiB, over its 50.0 GiB budget"),
        "{text}"
    );
}

/// Write the user config of `home`.
fn write_config(home: &Path, toml: &str) {
    let config = home.join("config");
    std::fs::create_dir_all(&config).expect("config dir");
    std::fs::write(config.join("config.toml"), toml).expect("config");
}

fn alert_keys(v: &Value) -> Vec<String> {
    v["data"]["alerts"]
        .as_array()
        .expect("alerts")
        .iter()
        .filter_map(|a| a["key"].as_str().map(str::to_owned))
        .collect()
}

/// `scull check` scans `scan.roots` and raises staleness, ecosystem/path budget and category
/// spike alerts from it, and records every alert in history (P4-PRO-02 follow-up). Read-only:
/// the fixture tree is unchanged (I-2/I-3).
#[test]
fn check_scans_roots_and_raises_stale_budget_and_spike_alerts() {
    use scull_fixtures::{Fixture, InvariantInputs, days_ago};
    let home = tempfile::tempdir().expect("tempdir");
    let mut fx = Fixture::new().expect("fixture");
    fx.cargo_project("app", "app")
        .expect("project")
        .cargo_target("app", "app", 64 * 1024)
        .expect("target")
        .age("app", days_ago(45))
        .expect("age");
    fx.checkpoint().expect("checkpoint");
    let root = scull_engine::paths::canon(fx.root());
    let root_s = root.to_string_lossy().into_owned();
    let app = root.join("app").to_string_lossy().into_owned();
    write_config(
        home.path(),
        &format!(
            "[scan]\nroots = ['{root_s}']\n\n[proactive]\nspike_gib = 0\nspike_percent = 10\n\
             spike_min_gib = 0\n\n[[proactive.budgets]]\necosystem = \"rust\"\nmax_gib = 0\n\n\
             [[proactive.budgets]]\npath = '{app}'\nmax_gib = 0\n"
        ),
    );

    let o = scull_in(home.path(), &["check", "--json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    let scan = &v["data"]["scan"];
    assert_eq!(scan["state"], "scanned", "{v:#}");
    assert_eq!(scan["roots"][0], root_s.as_str());
    let keys = alert_keys(&v);
    assert!(keys.contains(&"stale:build_output".to_owned()), "{keys:?}");
    assert!(
        keys.contains(&"budget_exceeded:rust".to_owned()),
        "{keys:?}"
    );
    assert!(keys.contains(&format!("budget_exceeded:{app}")), "{keys:?}");
    // One scan of these roots: no baseline for a spike yet.
    assert!(
        !keys.iter().any(|k| k.starts_with("growth_spike:")),
        "{keys:?}"
    );
    let stale = v["data"]["alerts"]
        .as_array()
        .expect("alerts")
        .iter()
        .find(|a| a["key"] == "stale:build_output")
        .expect("stale");
    assert_eq!(stale["severity"], "info");
    assert!(
        stale["paths"][0]
            .as_str()
            .is_some_and(|p| p.ends_with("target")),
        "{stale:#}"
    );

    // A second project's build output appears: project artifacts grew within the spike window.
    fx.cargo_project("lib", "lib")
        .expect("project")
        .cargo_target("lib", "lib", 256 * 1024)
        .expect("target");
    fx.checkpoint().expect("checkpoint");
    // Scans are compared by finish time in seconds.
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let o = scull_in(home.path(), &["check", "--json"]);
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    let spike = v["data"]["alerts"]
        .as_array()
        .expect("alerts")
        .iter()
        .find(|a| a["key"] == "growth_spike:project_artifact")
        .unwrap_or_else(|| panic!("spike alert: {:?}", alert_keys(&v)));
    assert_eq!(spike["kind"], "growth_spike");
    assert!(
        spike["paths"][0]
            .as_str()
            .is_some_and(|p| p.contains("lib")),
        "largest item named first: {spike:#}"
    );

    // Every alert raised was recorded in the alert stream.
    let store = Store::open(&db_path(home.path()), &DefaultPermissions).expect("open");
    let recorded: Vec<String> = store
        .alerts(0)
        .expect("alerts")
        .into_iter()
        .map(|a| a.key)
        .collect();
    assert!(
        recorded.contains(&"stale:build_output".to_owned()),
        "{recorded:?}"
    );
    assert!(
        recorded.contains(&"growth_spike:project_artifact".to_owned()),
        "{recorded:?}"
    );
    drop(store);

    // `--no-scan`: history only (no staleness: history keeps no modification times).
    let o = scull_in(home.path(), &["check", "--no-scan", "--json"]);
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    assert_eq!(v["data"]["scan"]["state"], "disabled");
    assert!(!alert_keys(&v).contains(&"stale:build_output".to_owned()));
    let text = String::from_utf8(scull_in(home.path(), &["check"]).stdout).expect("utf-8");
    assert!(text.starts_with("scanned "), "{text}");
    assert!(
        text.contains("info: 1 build output of projects idle for more than 30 days"),
        "{text}"
    );

    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Roots `scull scan` would refuse are not scanned by an unattended check: it says why and
/// uses recorded history.
#[test]
fn check_skips_roots_it_cannot_scan() {
    let home = tempfile::tempdir().expect("tempdir");
    write_config(home.path(), "[scan]\nroots = ['not/absolute']\n");
    let o = scull_in(home.path(), &["check", "--json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    assert_eq!(v["data"]["scan"]["state"], "skipped");
    assert!(
        v["data"]["scan"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("not an absolute path")),
        "{v:#}"
    );
    let text = String::from_utf8(scull_in(home.path(), &["check"]).stdout).expect("utf-8");
    assert!(text.starts_with("scan skipped: "), "{text}");
}

/// Whether `scull scan --no-history` of `root` finds an S0 candidate on this machine.
fn has_s0(home: &Path, root: &str) -> bool {
    let o = scull_in(home, &["scan", "--no-history", "--json", root]);
    let v: Value = serde_json::from_slice(&o.stdout).expect("scan JSON");
    v["data"]["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .any(|c| c["tier"] == "S0")
}

/// `scull check` keeps a ready-to-review plan of the scan's S0 items (P4-PRO-04, PRO-05): a
/// CLI plan bound to this machine and user, recorded in the database, written to the data
/// directory, reused while the work is unchanged and refreshed when it changes, surfaced by
/// `scull status`. Nothing is removed (I-2/I-3 on the fixture).
#[test]
fn check_prepares_a_review_plan_and_refreshes_it() {
    use scull_core::{Plan, PlanOrigin, Tier};
    use scull_fixtures::{Fixture, InvariantInputs, days_ago};
    let home = tempfile::tempdir().expect("tempdir");
    let mut fx = Fixture::new().expect("fixture");
    // Idle projects: their build output is S0 (recent activity would make it S1).
    fx.cargo_project("app", "app")
        .expect("project")
        .cargo_target("app", "app", 64 * 1024)
        .expect("target")
        .age("app", days_ago(45))
        .expect("age");
    fx.checkpoint().expect("checkpoint");
    let root = scull_engine::paths::canon(fx.root());
    let root_s = root.to_string_lossy().into_owned();
    write_config(home.path(), &format!("[scan]\nroots = ['{root_s}']\n"));

    let o = scull_in(home.path(), &["check", "--json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    let p = &v["data"]["review_plan"];
    if p.is_null() && !has_s0(home.path(), &root_s) {
        // This machine can't prove the build output idle (e.g. a sandbox hiding other
        // processes from the in-use probes, D-30): it is S2, so nothing qualifies. The unit
        // tests in `cmd::review_plan` cover preparation with synthetic S0 candidates.
        eprintln!("no S0 candidate on this machine; end-to-end plan checks skipped");
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    }
    assert_eq!(
        p["reused"],
        false,
        "{v:#}\n{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(p["includes_s1"], false);
    assert!(p["items"].as_u64().is_some_and(|n| n >= 1), "{p:#}");
    let file = PathBuf::from(p["file"].as_str().expect("file"));
    assert_eq!(file, home.path().join("data").join("review-plan.json"));
    assert_eq!(
        p["review_command"],
        format!("scull clean --plan {}", file.display())
    );

    // The file is exactly the recorded plan: a CLI plan bound to this machine and user, of
    // S0 items only, that verifies like any `scull clean` plan.
    let plan: Plan =
        serde_json::from_slice(&std::fs::read(&file).expect("plan file")).expect("plan JSON");
    assert_eq!(plan.id().as_str(), p["plan_id"].as_str().expect("id"));
    assert_eq!(plan.origin(), PlanOrigin::Cli);
    assert!(plan.items().iter().all(|i| i.tier == Tier::S0));
    assert!(
        plan.items()
            .iter()
            .any(|i| i.path.as_str().ends_with("target")),
        "{plan:#?}"
    );
    let machine = scull_platform::machine_id(&home.path().join("data")).expect("machine id");
    let user = scull_platform::current_user().expect("user");
    plan.verify(now(), &machine, &user).expect("verifies");
    let store = Store::open(&db_path(home.path()), &DefaultPermissions).expect("open");
    assert_eq!(
        store.get_plan(plan.id()).expect("get").as_ref(),
        Some(&plan),
        "recorded in the plans table"
    );
    drop(store);

    // Same work on the next check: the plan is kept, not recorded again.
    let v: Value =
        serde_json::from_slice(&scull_in(home.path(), &["check", "--json"]).stdout).expect("JSON");
    assert_eq!(v["data"]["review_plan"]["reused"], true, "{v:#}");
    assert_eq!(v["data"]["review_plan"]["plan_id"], p["plan_id"]);

    // `scull status` and `scull check` point to it.
    let line = String::from_utf8(scull_in(home.path(), &["status"]).stdout).expect("utf-8");
    assert!(
        line.contains(&format!(
            "cleanup plan ready to review: scull clean --plan {}",
            file.display()
        )),
        "{line}"
    );
    let s: Value =
        serde_json::from_slice(&scull_in(home.path(), &["status", "--json"]).stdout).expect("JSON");
    validate("status.schema.json", &s);
    assert_eq!(s["data"]["review_plan"]["plan_id"], p["plan_id"]);
    let short =
        String::from_utf8(scull_in(home.path(), &["status", "--short"]).stdout).expect("utf-8");
    assert!(
        !short.contains("plan"),
        "the prompt line is unchanged: {short}"
    );
    let text =
        String::from_utf8(scull_in(home.path(), &["check", "--no-scan"]).stdout).expect("utf-8");
    assert!(
        text.contains("cleanup plan is ready to review: scull clean --plan"),
        "{text}"
    );

    // New work (another project's build output): a new plan replaces it.
    fx.cargo_project("lib", "lib")
        .expect("project")
        .cargo_target("lib", "lib", 32 * 1024)
        .expect("target")
        .age("lib", days_ago(40))
        .expect("age");
    fx.checkpoint().expect("checkpoint");
    let v: Value =
        serde_json::from_slice(&scull_in(home.path(), &["check", "--json"]).stdout).expect("JSON");
    let q = &v["data"]["review_plan"];
    assert_eq!(q["reused"], false, "{v:#}");
    assert_ne!(q["plan_id"], p["plan_id"]);
    assert!(q["items"].as_u64() > p["items"].as_u64(), "{q:#} vs {p:#}");
    let newer: Plan =
        serde_json::from_slice(&std::fs::read(&file).expect("plan file")).expect("plan JSON");
    assert_eq!(newer.id().as_str(), q["plan_id"].as_str().expect("id"));

    // Opting in to S1 is a different proposal.
    let v: Value =
        serde_json::from_slice(&scull_in(home.path(), &["check", "--include-s1", "--json"]).stdout)
            .expect("JSON");
    assert_eq!(v["data"]["review_plan"]["includes_s1"], true, "{v:#}");
    assert_eq!(v["data"]["review_plan"]["reused"], false);

    // Nothing on disk changed: the plan is a proposal only.
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// No eligible item: no plan, and a previous plan is withdrawn.
#[test]
fn check_withdraws_the_plan_when_nothing_qualifies() {
    use scull_fixtures::{Fixture, InvariantInputs, days_ago};
    let home = tempfile::tempdir().expect("tempdir");
    let mut fx = Fixture::new().expect("fixture");
    fx.cargo_project("app", "app")
        .expect("project")
        .cargo_target("app", "app", 4096)
        .expect("target")
        .age("app", days_ago(45))
        .expect("age");
    fx.checkpoint().expect("checkpoint");
    let root = scull_engine::paths::canon(fx.root());
    write_config(
        home.path(),
        &format!("[scan]\nroots = ['{}']\n", root.to_string_lossy()),
    );
    let v: Value =
        serde_json::from_slice(&scull_in(home.path(), &["check", "--json"]).stdout).expect("JSON");
    if !has_s0(home.path(), &root.to_string_lossy()) {
        eprintln!("no S0 candidate on this machine; end-to-end withdrawal check skipped");
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    }
    assert!(v["data"]["review_plan"].is_object(), "{v:#}");

    // Point the check at an empty folder: nothing to propose.
    let empty = tempfile::tempdir().expect("tempdir");
    let empty_root = scull_engine::paths::canon(empty.path());
    write_config(
        home.path(),
        &format!("[scan]\nroots = ['{}']\n", empty_root.to_string_lossy()),
    );
    let v: Value =
        serde_json::from_slice(&scull_in(home.path(), &["check", "--json"]).stdout).expect("JSON");
    validate("check.schema.json", &v);
    assert_eq!(v["data"]["scan"]["state"], "scanned");
    assert!(v["data"]["review_plan"].is_null(), "{v:#}");
    let s: Value =
        serde_json::from_slice(&scull_in(home.path(), &["status", "--json"]).stdout).expect("JSON");
    assert!(s["data"]["review_plan"].is_null(), "{s:#}");
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Expired worktree TTLs registered by agents (`register_worktree`, `scull worktrees
/// register`) raise a review alert in `scull check`, never a removal (PRO-08). Registrations of
/// folders that no longer exist are ignored.
#[test]
fn check_reports_expired_worktree_ttls() {
    let home = tempfile::tempdir().expect("tempdir");
    let wt = tempfile::tempdir().expect("worktree stand-in");
    let wt_path = scull_engine::paths::canon(wt.path())
        .to_string_lossy()
        .into_owned();
    let gone = home
        .path()
        .join("gone-worktree")
        .to_string_lossy()
        .into_owned();
    let data = home.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let t = now();
    let line = |path: &str, expires_at: i64| {
        serde_json::json!({
            "v": 1, "path": path, "ttl_secs": 60, "registered_at": expires_at - 60,
            "expires_at": expires_at, "source": "mcp"
        })
        .to_string()
    };
    std::fs::write(
        data.join("worktree-ttl.jsonl"),
        format!(
            "{}\n{}\n",
            line(&wt_path, t - 3_600),
            line(&gone, t - 3_600)
        ),
    )
    .expect("registry");

    let o = scull_in(home.path(), &["check", "--json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate("check.schema.json", &v);
    let keys = alert_keys(&v);
    assert_eq!(
        keys,
        vec![format!("worktree_ttl_expired:{wt_path}")],
        "{v:#}"
    );
    let a = &v["data"]["alerts"][0];
    assert_eq!(a["severity"], "warning");
    assert!(
        a["message"]
            .as_str()
            .is_some_and(|m| m.contains("nothing was removed")),
        "{a:#}"
    );
    assert!(wt.path().is_dir(), "never removed");
}
