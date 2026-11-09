//! One forecast source (#36): the same free-space history, fed to every surface that shows a
//! disk-full forecast, gives the same days left and confidence everywhere:
//!
//! * `scull status --json` and `scull check --no-scan --json` (CLI, PRO-01, PRO-09),
//! * `scull insights --json` (`free_space[].forecast`).
//!
//! (The desktop app's forecast, digest and tray surfaces left with the desktop restart, #144.)
//!
//! The config sets a non-default reserve (`[proactive] reserve_gib = 20`), so a surface that
//! ignored the configured policy would answer 18 days instead of 13. Everything lives under a
//! temp `SCULL_HOME`; the volume is an imagined path, so `scull check` doesn't sample a real disk.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_storage::{DefaultPermissions, Store, VolumeSample};
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const GIB: u64 = 1024 * 1024 * 1024;
const DAY: i64 = 86_400;
/// Not mounted anywhere: `scull check` lists it as not sampled instead of measuring a disk.
const VOL: &str = "/scull-forecast-surfaces-volume";

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn scull_json(home: &Path, args: &[&str]) -> Value {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args).env("SCULL_HOME", home).env("TERM", "dumb");
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            cmd.env_remove(k);
        }
    }
    let o = cmd.output().unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).unwrap()
}

/// 21 daily samples of a 500 GiB volume losing 2 GiB a day, 46 GiB free an hour ago:
/// (46 − 20) / 2 = 13 days to the configured 20 GiB reserve, high confidence.
fn seed(home: &Path, t: i64) {
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(
        home.join("config").join("config.toml"),
        "[proactive]\nreserve_gib = 20\nalert_days = 14\n",
    )
    .unwrap();
    let mut store = Store::open(
        &home.join("data").join(scull_storage::DB_FILE_NAME),
        &DefaultPermissions,
    )
    .unwrap();
    let samples: Vec<VolumeSample> = (0..=20u64)
        .map(|i| VolumeSample {
            volume_id: VOL.into(),
            total: 500 * GIB,
            free: (46 + 2 * (20 - i)) * GIB,
            at: t - 3_600 - i64::try_from(20 - i).unwrap() * DAY,
        })
        .collect();
    store.record_free_space(&samples, None).unwrap();
}

fn days(v: &Value) -> Option<u64> {
    // CLI reports carry whole days; the full Forecast carries the fraction (shown rounded).
    v.as_u64()
        .or_else(|| v.as_f64().map(|d| d.round().max(1.0) as u64))
}

fn one<'a>(list: &'a Value, what: &str) -> &'a Value {
    let l = list.as_array().unwrap_or_else(|| panic!("{what}: {list}"));
    l.iter()
        .find(|f| f["volume_id"] == VOL)
        .unwrap_or_else(|| panic!("{what}: no forecast for {VOL}: {list}"))
}

#[test]
fn every_surface_reports_the_same_days_left_and_confidence() {
    let home = tempfile::tempdir().unwrap();
    let t = now();
    seed(home.path(), t);
    let expected_days = 13;

    // CLI.
    let status = scull_json(home.path(), &["status", "--json"]);
    let s = one(&status["data"]["forecasts"], "status");
    let insights = scull_json(home.path(), &["insights", "--json"]);
    let vols = insights["data"]["free_space"].clone();
    let i = one(&vols, "insights")["forecast"].clone();
    let check = scull_json(home.path(), &["check", "--no-scan", "--json"]);
    let c = one(&check["data"]["forecasts"], "check");

    let found = [
        (
            "scull status",
            days(&s["days_until_threshold"]),
            s["confidence"].as_str().map(str::to_owned),
        ),
        (
            "scull check",
            days(&c["days_until_threshold"]),
            c["confidence"].as_str().map(str::to_owned),
        ),
        (
            "scull insights",
            days(&i["days_until_threshold"]),
            i["confidence"].as_str().map(str::to_owned),
        ),
    ];
    for (surface, days, confidence) in &found {
        assert_eq!(*days, Some(expected_days), "{surface}: {found:?}");
        assert_eq!(confidence.as_deref(), Some("high"), "{surface}: {found:?}");
    }
    // The same rate everywhere too: 2 GiB a day.
    let rate = s["fill_bytes_per_day"].as_f64().unwrap();
    assert!((rate - 2.0 * GIB as f64).abs() < 1e6, "{rate}");
    assert_eq!(s["fill_bytes_per_day"], c["fill_bytes_per_day"]);
}
