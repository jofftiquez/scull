//! `scull insights`, `insights export` and `insights purge` end to end (P4-INS-01). Every run
//! points `SCULL_HOME` at a fresh temp dir; the history is seeded through `scull-storage`
//! (`scull_insights::fixtures`), describing paths that do not exist.

use scull_storage::{DefaultPermissions, Store};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn scull_in(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args)
        .env("SCULL_HOME", home)
        .env("TERM", "dumb")
        .stdin(Stdio::null());
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

fn seeded() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(&db_path(home.path()), &DefaultPermissions).expect("open");
    scull_insights::fixtures::seed_demo(&mut store, now()).expect("seed");
    home
}

fn schema_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("schemas/{name}.schema.json"))
}

fn validate(name: &str, v: &Value) {
    let schema: Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path(name)).expect("schema file"))
            .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

fn json_ok(home: &Path, args: &[&str], schema: &str) -> Value {
    let o = scull_in(home, args);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate(schema, &v);
    v
}

fn bless(name: &str, generated: Value) {
    let path = schema_path(name);
    let text = serde_json::to_string_pretty(&generated).expect("serialize") + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(&path, &text).expect("write");
    }
    let committed = std::fs::read_to_string(&path).expect("schema file");
    assert_eq!(
        committed.replace("\r\n", "\n"),
        text,
        "schemas/{name}.schema.json is stale; rerun with SCULL_BLESS=1"
    );
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test insights`.
#[test]
fn committed_schema_matches_generated() {
    use scull_cli::cmd::insights::{INSIGHTS_PURGE_SCHEMA, INSIGHTS_SCHEMA, InsightsPurgeReport};
    use scull_cli::output::json::document_schema;
    bless(
        "insights",
        document_schema::<scull_insights::InsightsReport>(INSIGHTS_SCHEMA),
    );
    bless(
        "insights-purge",
        document_schema::<InsightsPurgeReport>(INSIGHTS_PURGE_SCHEMA),
    );
}

#[test]
fn fx_ins_15_insights_json_from_a_seeded_history_matches_the_schema() {
    let home = seeded();
    let v = json_ok(home.path(), &["insights", "--json"], "insights");
    assert_eq!(v["schema"], "scull/insights@1");
    let d = &v["data"];
    assert_eq!(d["window_days"], 30);
    assert_eq!(d["growth"][1]["projects"][0]["label"], "ml-lab");
    assert_eq!(d["regrowth"][0]["rebuilds"], 4);
    assert_eq!(d["duplication"][1]["projects"], 5);
    assert_eq!(d["agent_footprint"]["by_agent"][1]["agent"], "codex");
    assert_eq!(
        d["accuracy"]["operations"].as_array().map(Vec::len),
        Some(4)
    );
    assert_eq!(d["ecosystem_mix"].as_array().map(Vec::len), Some(6));
    assert_eq!(d["free_space"][0]["volume_id"], "/");

    // --since narrows the window; --ndjson carries the same report as one record.
    let v = json_ok(
        home.path(),
        &["insights", "--since", "7d", "--json"],
        "insights",
    );
    assert_eq!(v["data"]["window_days"], 7);
    assert_eq!(v["data"]["regrowth"][0]["rebuilds"], 1);
    let o = scull_in(home.path(), &["insights", "--ndjson"]);
    assert_eq!(o.status.code(), Some(0));
    let line: Value = serde_json::from_slice(&o.stdout).expect("one JSON line");
    assert_eq!(line["type"], "insights");
    assert_eq!(line["data"]["window_days"], 30);

    // The human report names the figures.
    let o = scull_in(home.path(), &["insights"]);
    assert_eq!(o.status.code(), Some(0));
    let text = String::from_utf8(o.stdout).expect("utf-8");
    for needle in [
        "Top growers \u{b7} 30 days",
        "shop-api/node_modules came back 4 times",
        "Installed npm dependencies in 5 projects",
        "claude-code",
        "median error",
        "What scull keeps",
    ] {
        assert!(text.contains(needle), "{needle:?} in\n{text}");
    }
}

#[test]
fn fx_ins_16_no_history_gives_an_empty_report_and_creates_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let v = json_ok(home.path(), &["insights", "--json"], "insights");
    assert_eq!(v["data"]["coverage"]["items"], 0);
    let o = scull_in(home.path(), &["insights"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stdout).contains("No history recorded yet"));
    // Purging with no history creates nothing either, and says so.
    let v = json_ok(
        home.path(),
        &["insights", "purge", "--confirm", "--json"],
        "insights-purge",
    );
    assert_eq!(v["data"]["database_found"], false);
    assert_eq!(v["data"]["deleted"]["total"], 0);
    let o = scull_in(home.path(), &["insights", "purge"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stdout).contains("Nothing to purge"));
    assert_eq!(
        std::fs::read_dir(home.path()).expect("dir").count(),
        0,
        "insights never creates the database"
    );
}

#[test]
fn fx_ins_17_export_json_and_csv() {
    let home = seeded();
    let v = json_ok(home.path(), &["insights", "export"], "insights");
    assert_eq!(v["data"]["regrowth"][0]["rebuilds"], 4);
    let v = json_ok(
        home.path(),
        &["insights", "export", "--format", "json", "--since", "7d"],
        "insights",
    );
    assert_eq!(v["data"]["window_days"], 7);

    let o = scull_in(home.path(), &["insights", "export", "--format", "csv"]);
    assert_eq!(o.status.code(), Some(0));
    let csv = String::from_utf8(o.stdout).expect("utf-8");
    assert!(csv.starts_with("section,period,subject,metric,value\n"));
    assert!(csv.contains("regrowth,30d,/home/dev/work/shop-api/node_modules,rebuilds,4\n"));
    assert!(csv.lines().all(|l| l.split(',').count() >= 5));

    // CSV is not JSON: asking for both is a usage error.
    let o = scull_in(
        home.path(),
        &["insights", "export", "--format", "csv", "--json"],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(o.stdout.is_empty());
    // Bad --since values are usage errors too.
    for bad in ["soon", "2026-02-30", "0d"] {
        let o = scull_in(home.path(), &["insights", "--since", bad]);
        assert_eq!(o.status.code(), Some(2), "{bad}");
    }
}

fn count(home: &Path, what: &str) -> i64 {
    // Counted through the report over ten years (the typed API has no row counts).
    let v = json_ok(
        home,
        &["insights", "--json", "--since", "3650d"],
        "insights",
    );
    match what {
        "free_space" => v["data"]["free_space"].as_array().map_or(0, |a| {
            a.iter()
                .map(|x| x["points"].as_array().map_or(0, Vec::len))
                .sum::<usize>()
        }) as i64,
        "regrowth" => v["data"]["regrowth"].as_array().map_or(0, Vec::len) as i64,
        "operations" => v["data"]["accuracy"]["operations"]
            .as_array()
            .map_or(0, Vec::len) as i64,
        "items" => v["data"]["coverage"]["items"].as_i64().unwrap_or(-1),
        other => panic!("{other}"),
    }
}

/// The `insights_purged` events recorded in the database: `(older_than, total)` each.
fn purge_events(home: &Path) -> Vec<(Value, Value)> {
    let store = Store::open(&db_path(home), &DefaultPermissions).expect("open");
    store
        .events(scull_storage::EventKind::InsightsPurged, 0)
        .expect("events")
        .into_iter()
        .map(|e| {
            assert_eq!(e.source, "cli");
            (e.payload["older_than"].clone(), e.payload["total"].clone())
        })
        .collect()
}

/// INS-02: purging needs confirmation (`--confirm` without a terminal or in machine modes),
/// forgets only history rows (all, or those older than `--older-than`), keeps live items,
/// scans, the volumes cache and the append-only audit, and records each purge as an
/// `insights_purged` event. Everything lives in a temp `SCULL_HOME`.
#[test]
fn fx_ins_18_purge_requires_confirmation_forgets_only_history_and_is_recorded() {
    let home = seeded();
    let h = home.path();
    assert_eq!(count(h, "free_space"), 60);
    assert_eq!(count(h, "regrowth"), 1);
    let operations = count(h, "operations");
    assert!(operations > 0);

    // Without a terminal, --confirm is required (also in machine modes); nothing is deleted.
    for args in [
        &["insights", "purge"][..],
        &["insights", "purge", "--json"],
        &["insights", "purge", "--older-than", "20d", "--ndjson"],
    ] {
        let o = scull_in(h, args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(o.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("--confirm"),
            "{args:?}"
        );
    }
    assert_eq!(count(h, "free_space"), 60);
    assert!(purge_events(h).is_empty());

    // Older than 20 days: only old history goes.
    let v = json_ok(
        h,
        &[
            "insights",
            "purge",
            "--older-than",
            "20d",
            "--confirm",
            "--json",
        ],
        "insights-purge",
    );
    assert_eq!(v["schema"], "scull/insights-purge@1");
    let d = &v["data"]["deleted"];
    assert_eq!(d["free_space_samples"], 40);
    assert!(d["gone_items"].as_u64().unwrap_or(0) > 0);
    assert_eq!(count(h, "free_space"), 20);
    let cutoff = v["data"]["older_than"].clone();
    assert!(cutoff.is_i64(), "{v:#}");

    // Everything.
    let o = scull_in(h, &["insights", "purge", "--confirm"]);
    assert_eq!(o.status.code(), Some(0));
    let text = String::from_utf8(o.stdout).expect("utf-8");
    assert!(text.starts_with("Deleted "), "{text}");
    assert!(text.contains("append-only"), "{text}");
    assert_eq!(count(h, "free_space"), 0);
    assert_eq!(count(h, "regrowth"), 0);
    // Audit rows are append-only and kept: cleanup accuracy still reads them.
    assert_eq!(count(h, "operations"), operations);
    // Live items are kept.
    assert!(count(h, "items") > 0);
    // `scull status` still works (the volumes cache and scans are kept).
    let o = scull_in(h, &["status"]);
    assert_eq!(o.status.code(), Some(0));

    // Both purges are recorded, with their cutoff and row count; neither deleted the other.
    let events = purge_events(h);
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], (cutoff, d["total"].clone()));
    assert!(events[1].0.is_null());
}
