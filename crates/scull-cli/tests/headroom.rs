//! `scull headroom status|create|release` end to end (PRO-06). Every run points `SCULL_HOME`
//! and `SCULL_TEST_SANDBOX` at a fresh temp dir: the reserve is only ever created and released
//! there. The config keeps 1 GiB free (`[proactive] reserve_gib = 1`) so the check fits CI
//! runners; the reserve is the 256 MiB minimum.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scull(home: &Path, sandbox: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args)
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::null());
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("SCULL_HOME", home)
        .env("SCULL_TEST_SANDBOX", sandbox);
    cmd.output().expect("run scull")
}

fn json(o: &Output) -> Value {
    assert!(
        o.status.success(),
        "exit {:?}: {}",
        o.status.code(),
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).unwrap()
}

fn home() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("config")).unwrap();
    std::fs::write(
        t.path().join("config").join("config.toml"),
        "[proactive]\nreserve_gib = 1\n",
    )
    .unwrap();
    t
}

fn reserve(home: &Path) -> PathBuf {
    home.join("data").join("headroom").join("reserve.bin")
}

fn validate(v: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/headroom.schema.json");
    let s: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&s).expect("compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test headroom`.
#[test]
fn committed_schema_matches_generated() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/headroom.schema.json");
    let generated = scull_cli::output::json::document_schema::<
        scull_cli::cmd::headroom::HeadroomReport,
    >(scull_cli::cmd::headroom::HEADROOM_SCHEMA);
    let text = serde_json::to_string_pretty(&generated).unwrap() + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    let committed = std::fs::read_to_string(&path).expect("schema file");
    assert_eq!(
        committed.replace("\r\n", "\n"),
        text,
        "schemas/headroom.schema.json is stale; rerun with SCULL_BLESS=1"
    );
}

#[test]
fn create_status_release_round_trip_is_audited() {
    let h = home();
    let s = h.path();

    let v = json(&scull(s, s, &["headroom", "status", "--json"]));
    validate(&v);
    assert_eq!(v["schema"], "scull/headroom@1");
    assert_eq!(v["data"]["status"]["state"], "none");

    // A change without a terminal needs --yes; nothing is created.
    let o = scull(s, s, &["headroom", "create", "--size", "256M"]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!reserve(s).exists());

    let v = json(&scull(
        s,
        s,
        &["headroom", "create", "--size", "256M", "--yes", "--json"],
    ));
    validate(&v);
    let created = &v["data"]["created"];
    assert_eq!(created["size_bytes"], 256u64 << 20);
    assert!(created["allocated_bytes"].as_u64().unwrap() >= (256u64 << 20) * 98 / 100);
    assert_eq!(v["data"]["status"]["state"], "ready");
    assert!(v["data"]["audit_error"].is_null());
    assert_eq!(std::fs::metadata(reserve(s)).unwrap().len(), 256 << 20);

    // `scull status` reports it.
    let st = json(&scull(s, s, &["status", "--json"]));
    assert_eq!(st["data"]["headroom"]["state"], "ready");

    // A second one is refused (exit 5, safety).
    let o = scull(s, s, &["headroom", "create", "--size", "256M", "--yes"]);
    assert_eq!(
        o.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );

    let v = json(&scull(s, s, &["headroom", "release", "--yes", "--json"]));
    validate(&v);
    assert_eq!(v["data"]["released"]["outcome"], "released");
    assert!(v["data"]["released"]["freed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(v["data"]["status"]["state"], "none");
    assert!(!reserve(s).exists());

    // Nothing left: release is "not found" (exit 1).
    let o = scull(s, s, &["headroom", "release", "--yes"]);
    assert_eq!(o.status.code(), Some(1));

    // The audit trail: refused (second create), created, released.
    let store = scull_storage::Store::open(
        &s.join("data").join(scull_storage::DB_FILE_NAME),
        &scull_storage::DefaultPermissions,
    )
    .unwrap();
    let actions: Vec<String> = store
        .events(scull_storage::EventKind::Headroom, 0)
        .unwrap()
        .iter()
        .map(|e| e.payload["action"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(actions, ["created", "refused", "released"]);
}

#[test]
fn outside_the_test_sandbox_nothing_is_created() {
    let h = home();
    let elsewhere = tempfile::tempdir().unwrap();
    let o = scull(
        h.path(),
        elsewhere.path(),
        &["headroom", "create", "--size", "256M", "--yes"],
    );
    assert_eq!(
        o.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("outside the test sandbox"));
    assert!(!reserve(h.path()).exists());
}

#[test]
fn below_the_configured_reserve_creation_is_refused() {
    let h = home();
    // Keep more free than any test machine has.
    std::fs::write(
        h.path().join("config").join("config.toml"),
        "[proactive]\nreserve_gib = 1000000\n",
    )
    .unwrap();
    let o = scull(
        h.path(),
        h.path(),
        &["headroom", "create", "--size", "256M", "--yes"],
    );
    assert_eq!(
        o.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("below"));
    assert!(!reserve(h.path()).exists());
}
