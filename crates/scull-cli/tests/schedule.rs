//! `scull schedule` end to end (P4-PRO-03), **without ever registering a schedule**: every run
//! points `HOME`, `USERPROFILE`, `XDG_CONFIG_HOME` and `SCULL_HOME` at a fresh temp dir, and
//! only the paths that stop before any tool runs are exercised (`--dry-run`, refusal without
//! consent, `disable`/`status` with nothing registered). Applying, with a fake command runner
//! and an injected unit folder, is covered by the unit tests in `cmd::schedule`.
// A command outside the personal MVP: tested only in the full build (#142).
#![cfg(feature = "full")]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn scull_in(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args)
        .env("SCULL_HOME", home.join("scull-home"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("TERM", "dumb")
        .stdin(Stdio::null());
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            cmd.env_remove(k);
        }
    }
    cmd.output().expect("run scull")
}

/// Every entry under `dir`, recursively (to prove nothing was written).
fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p.clone());
                }
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn validate(v: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/schedule.schema.json");
    let s: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("schema file"))
        .expect("schema JSON");
    let validator = jsonschema::validator_for(&s).expect("compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test schedule`.
#[test]
fn committed_schema_matches_generated() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/schedule.schema.json");
    let generated = scull_cli::output::json::document_schema::<
        scull_cli::cmd::schedule::ScheduleReport,
    >(scull_cli::cmd::schedule::SCHEDULE_SCHEMA);
    let text = serde_json::to_string_pretty(&generated).expect("serialize") + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(&path, &text).expect("write");
    }
    let committed = std::fs::read_to_string(&path).expect("schema file");
    assert_eq!(
        committed.replace("\r\n", "\n"),
        text,
        "schemas/schedule.schema.json is stale; rerun with SCULL_BLESS=1"
    );
}

#[test]
fn dry_run_shows_the_exact_files_and_commands_and_changes_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let before = tree(home.path());

    let o = scull_in(home.path(), &["schedule", "enable", "--dry-run", "--json"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate(&v);
    assert_eq!(v["schema"], "scull/schedule@1");
    let d = &v["data"];
    assert_eq!(d["operation"], "enable");
    assert_eq!(d["dry_run"], true);
    assert_eq!(d["applied"], false);
    assert_eq!(d["interval_secs"], 21_600);
    assert!(
        d["job"]
            .as_str()
            .is_some_and(|j| j.ends_with("check --quiet --no-progress")),
        "{d:#}"
    );
    let files = d["files"].as_array().expect("files");
    assert!(!files.is_empty());
    for f in files {
        let p = PathBuf::from(f["path"].as_str().expect("path"));
        assert!(
            p.starts_with(home.path()),
            "unit files go under HOME: {p:?}"
        );
        assert_eq!(f["change"], "write");
        assert!(
            f["contents"].as_str().is_some_and(|c| c.contains("check")),
            "{f:#}"
        );
    }
    let cmds = d["commands"].as_array().expect("commands");
    assert!(!cmds.is_empty());
    assert!(cmds.iter().all(|c| c["ran"] == false), "{cmds:?}");
    assert_eq!(tree(home.path()), before, "--dry-run writes nothing");

    let o = scull_in(
        home.path(),
        &["schedule", "enable", "--dry-run", "--every", "12h"],
    );
    assert_eq!(o.status.code(), Some(0));
    let text = String::from_utf8(o.stdout).expect("utf-8");
    assert!(
        text.contains("every 12 hours at low priority, as you"),
        "{text}"
    );
    assert!(text.contains("Files to write:"), "{text}");
    assert!(text.contains("Commands to run:"), "{text}");
    assert!(text.ends_with("Dry run: nothing was changed.\n"), "{text}");
    assert_eq!(tree(home.path()), before);
}

#[test]
fn enable_without_consent_refuses_and_changes_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let before = tree(home.path());
    for args in [
        &["schedule", "enable"][..],
        &["schedule", "enable", "--json"],
        &["schedule", "enable", "--every", "1d"],
    ] {
        let o = scull_in(home.path(), args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("--yes") || err.contains("--dry-run"),
            "{args:?}: {err}"
        );
        assert_eq!(tree(home.path()), before, "{args:?} wrote something");
    }
    // Bad intervals are usage errors before anything is shown.
    let o = scull_in(
        home.path(),
        &["schedule", "enable", "--every", "5m", "--dry-run"],
    );
    assert_eq!(o.status.code(), Some(2));
    // --yes and --dry-run together are contradictory.
    let o = scull_in(home.path(), &["schedule", "enable", "--yes", "--dry-run"]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(tree(home.path()), before);
}

#[test]
fn disable_and_status_with_nothing_registered_run_no_tool() {
    let home = tempfile::tempdir().expect("tempdir");
    let before = tree(home.path());

    let o = scull_in(home.path(), &["schedule", "disable"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let text = String::from_utf8(o.stdout).expect("utf-8");
    assert!(text.contains("no periodic check is registered"), "{text}");

    let o = scull_in(home.path(), &["schedule", "status", "--json"]);
    assert_eq!(o.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate(&v);
    assert_eq!(v["data"]["operation"], "status");
    assert_eq!(v["data"]["registered"], false);
    assert_eq!(
        v["data"]["commands"],
        Value::Array(Vec::new()),
        "no query ran"
    );
    assert!(
        v["data"]["files"]
            .as_array()
            .expect("files")
            .iter()
            .all(|f| f["change"] == "missing")
    );
    let o = scull_in(home.path(), &["schedule", "status"]);
    let text = String::from_utf8(o.stdout).expect("utf-8");
    assert!(text.starts_with("scull schedule: not registered"), "{text}");
    assert_eq!(tree(home.path()), before);
}
