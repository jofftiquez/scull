//! `scull doctor` end to end (P1-CLI-05). Every run points `SCULL_HOME` at a fresh temp dir, so
//! nothing reads or writes the real home directory; doctor itself must create nothing.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scull_in(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args).env("SCULL_HOME", home).env("TERM", "dumb");
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            cmd.env_remove(k);
        }
    }
    for k in ["NO_COLOR", "SUDO_USER", "DOAS_USER"] {
        cmd.env_remove(k);
    }
    cmd.envs(env.iter().copied());
    cmd.output().expect("run scull")
}

fn doctor_json(home: &Path, env: &[(&str, &str)]) -> Value {
    let o = scull_in(home, &["doctor", "--json"], env);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON on stdout");
    let errors: Vec<String> = validator()
        .iter_errors(&v)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
    v
}

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/doctor.schema.json")
}

/// The committed schema. Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test doctor`.
fn validator() -> jsonschema::Validator {
    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(schema_path()).expect("schemas/doctor.schema.json exists"),
    )
    .expect("schema is JSON");
    jsonschema::validator_for(&committed).expect("schema compiles")
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = walk(dir);
    v.sort();
    v
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).expect("read_dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

#[test]
fn committed_schema_matches_generated() {
    let generated = scull_cli::output::json::document_schema::<scull_cli::cmd::doctor::DoctorReport>(
        scull_cli::cmd::doctor::DOCTOR_SCHEMA,
    );
    let text = serde_json::to_string_pretty(&generated).expect("serialize") + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(schema_path(), &text).expect("write schema");
    }
    let committed = std::fs::read_to_string(schema_path()).expect("schema file");
    assert_eq!(
        committed.replace("\r\n", "\n"),
        text,
        "schemas/doctor.schema.json is stale; rerun with SCULL_BLESS=1"
    );
    let v: Value = serde_json::from_str(&committed).expect("json");
    assert_eq!(v["$schema"], "https://json-schema.org/draft/2020-12/schema");
    assert_eq!(v["properties"]["schema"]["const"], "scull/doctor@1");
}

#[test]
fn fresh_home_reports_missing_and_creates_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let v = doctor_json(home.path(), &[]);
    assert!(
        entries(home.path()).is_empty(),
        "doctor created {:?}",
        entries(home.path())
    );
    assert_eq!(v["schema"], "scull/doctor@1");
    let d = &v["data"];
    assert_eq!(d["platform"]["os"], std::env::consts::OS);
    assert_eq!(d["platform"]["arch"], std::env::consts::ARCH);
    assert_eq!(d["scull"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(d["database"]["state"], "missing");
    assert_eq!(d["database"]["integrity"], "not_checked");
    assert_eq!(d["config"]["file"], "missing");
    assert_eq!(d["dirs"]["entries"][0]["exists"], false);
    let signals = d["signals"].as_array().expect("signals");
    for name in [
        "file_identity",
        "allocated_size",
        "link_count",
        "race_free_read_dir",
        "elevation_detection",
    ] {
        let s = signals
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(
            s["available"].as_bool().expect("bool"),
            s["reason"].is_null(),
            "{s}"
        );
    }
    // The test process is not elevated in CI.
    assert!(d["privileges"]["elevation"].is_string());
    // Every config field is listed with an origin.
    assert_eq!(
        d["config"]["effective"]
            .as_array()
            .expect("effective")
            .len(),
        scull_core::config::FIELDS.len()
    );
}

#[test]
fn seeded_database_and_config_with_origins() {
    let home = tempfile::tempdir().expect("tempdir");
    let data = home.path().join("data");
    let db = data.join(scull_storage::DB_FILE_NAME);
    drop(scull_storage::Store::open(&db, &scull_storage::DefaultPermissions).expect("seed db"));
    let config = home.path().join("config");
    std::fs::create_dir(&config).expect("config dir");
    std::fs::write(
        config.join("config.toml"),
        "[scan]\nroots = [\"/srv/projects\"]\nthreads = 2\n",
    )
    .expect("config");
    let before = entries(home.path());

    let v = doctor_json(home.path(), &[("SCULL_THREADS", "6")]);
    assert_eq!(entries(home.path()), before, "doctor must not add files");
    let d = &v["data"];
    assert_eq!(d["database"]["state"], "ok", "{}", d["database"]);
    assert_eq!(d["database"]["integrity"], "ok");
    assert_eq!(
        d["database"]["schema_version"],
        scull_storage::SCHEMA_VERSION
    );
    assert!(d["database"]["footprint_bytes"].as_u64().expect("bytes") > 0);
    assert_eq!(d["config"]["file"], "ok");
    let field = |f: &str| {
        d["config"]["effective"]
            .as_array()
            .expect("effective")
            .iter()
            .find(|v| v["field"] == f)
            .cloned()
            .unwrap_or_else(|| panic!("{f}"))
    };
    assert_eq!(
        field("scan.roots")["value"],
        serde_json::json!(["/srv/projects"])
    );
    assert_eq!(
        field("scan.roots")["origins"],
        serde_json::json!([{"source": "user_config"}])
    );
    assert_eq!(field("scan.threads")["value"], 6);
    assert_eq!(
        field("scan.threads")["origins"],
        serde_json::json!([{"source": "env", "var": "SCULL_THREADS"}])
    );
}

#[test]
fn broken_database_and_config_are_reported_untouched() {
    let home = tempfile::tempdir().expect("tempdir");
    let data = home.path().join("data");
    std::fs::create_dir(&data).expect("data dir");
    let db = data.join(scull_storage::DB_FILE_NAME);
    std::fs::write(&db, b"definitely not a database").expect("junk db");
    let config = home.path().join("config");
    std::fs::create_dir(&config).expect("config dir");
    std::fs::write(config.join("config.toml"), "[scan]\nrootz = 1\n").expect("config");

    let v = doctor_json(home.path(), &[("SCULL_LOW_RESOURCE", "maybe")]);
    let d = &v["data"];
    assert_eq!(d["database"]["state"], "not_sqlite");
    assert_eq!(
        std::fs::read(&db).expect("db"),
        b"definitely not a database",
        "doctor must not modify the database"
    );
    assert_eq!(d["config"]["file"], "invalid");
    assert!(
        d["config"]["error"]
            .as_str()
            .expect("error")
            .contains("rootz")
    );
    assert!(
        d["config"]["env_error"]
            .as_str()
            .expect("env")
            .contains("SCULL_LOW_RESOURCE")
    );
    let warnings = d["warnings"].as_array().expect("warnings");
    assert!(warnings.len() >= 3, "{warnings:?}");
}

#[test]
fn old_schema_is_not_migrated() {
    let home = tempfile::tempdir().expect("tempdir");
    let data = home.path().join("data");
    std::fs::create_dir(&data).expect("data dir");
    let db = data.join(scull_storage::DB_FILE_NAME);
    // A valid SQLite header claiming scull's application id and schema version 1.
    let mut header = vec![0u8; 4096];
    header[..16].copy_from_slice(b"SQLite format 3\0");
    header[60..64].copy_from_slice(&1u32.to_be_bytes());
    header[68..72].copy_from_slice(&scull_storage::APPLICATION_ID.to_be_bytes());
    std::fs::write(&db, &header).expect("db");
    let v = doctor_json(home.path(), &[]);
    let d = &v["data"]["database"];
    assert_eq!(d["state"], "needs_migration", "{d}");
    assert_eq!(d["schema_version"], 1);
    assert_eq!(std::fs::read(&db).expect("db"), header);
    assert_eq!(entries(&data), vec![db]);
}

#[cfg(unix)]
#[test]
fn group_readable_directory_is_flagged() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join("state");
    std::fs::create_dir(&state).expect("dir");
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let v = doctor_json(home.path(), &[]);
    let d = &v["data"];
    let e = d["dirs"]["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["roles"] == serde_json::json!(["state"]))
        .expect("state entry")
        .clone();
    assert_eq!(e["exists"], true);
    assert_eq!(e["owner_only"], false);
    assert!(!e["exposures"].as_array().expect("exposures").is_empty());
    assert_eq!(
        std::fs::metadata(&state).expect("md").permissions().mode() & 0o777,
        0o755,
        "doctor reports permissions, never changes them"
    );
    assert!(d["warnings"].as_array().expect("warnings").iter().any(|w| {
        w.as_str()
            .is_some_and(|w| w.contains("accessible by other users"))
    }));
}

#[test]
fn human_and_ndjson_output() {
    let home = tempfile::tempdir().expect("tempdir");
    let o = scull_in(home.path(), &["doctor"], &[]);
    assert_eq!(o.status.code(), Some(0));
    let out = String::from_utf8(o.stdout).expect("utf-8");
    for section in [
        "Signals",
        "Privileges",
        "Directories",
        "Database",
        "Git",
        "Config",
        "Warnings",
    ] {
        assert!(out.contains(&format!("\n{section}\n")), "{section}:\n{out}");
    }
    assert!(!out.contains('\x1b'), "no color when piped");

    let o = scull_in(home.path(), &["doctor", "--ndjson"], &[]);
    assert_eq!(o.status.code(), Some(0));
    let text = String::from_utf8(o.stdout).expect("utf-8");
    assert_eq!(text.lines().count(), 1);
    let rec: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(rec["schema"], "scull/doctor@1");
    assert_eq!(rec["type"], "doctor");
    assert!(entries(home.path()).is_empty());
}
