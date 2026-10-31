//! `scull check` and the forecast in `scull status` end to end (P4-PRO-01). Every run points
//! `SCULL_HOME` at a fresh temp dir; history is seeded through `scull-storage`.
//!
//! The seeded volume id is not a real mount point on any OS, so `scull check` reports it as
//! not connected instead of sampling it, and the forecast uses only the seeded history.

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
