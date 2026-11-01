//! FX-OUT-01, NDJSON half (#247, CLI-01): **every** command that prints `--ndjson` lines
//! publishes a line schema, and its real output validates against it.
//!
//! The test walks the clap command tree, so a new command fails here until it is either run
//! below with `--ndjson` (and has `schemas/<name>.ndjson.schema.json`) or listed with the
//! reason it prints no NDJSON. Commands this build refuses (`planned_for`) must print nothing
//! on stdout; once one is implemented, it fails here until it is moved to [`RUNS`].
//!
//! Everything runs with `HOME`, `SCULL_HOME` and the app-data variables in temp dirs; the only
//! things written are scull's own files there (config from `init`, the history database from
//! `check` and `clean`'s plan, a TTL registration, a `data export`). `schedule`, `agent` and
//! `clean caches` run with `--dry-run` only; `clean` only plans and nothing is executed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use clap::CommandFactory;
use scull_fixtures::{Fixture, InvariantInputs};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Commands run with `--ndjson`: (command path, arguments, line schema name). `{root}` is the
/// fixture scan root, `{app}` a Cargo project inside it, `{wt}` a linked Git worktree (the
/// commands using it are skipped when `git` is not installed).
const RUNS: &[(&str, &[&str], &str)] = &[
    ("scan", &["scan", "{root}", "--allow-network-roots"], "scan"),
    (
        "projects",
        &["projects", "--root", "{root}", "--allow-network-roots"],
        "projects",
    ),
    (
        "projects inspect",
        &["projects", "inspect", "{app}", "--allow-network-roots"],
        "project",
    ),
    (
        "worktrees",
        &["worktrees", "--root", "{root}", "--allow-network-roots"],
        "worktrees",
    ),
    (
        "worktrees inspect",
        &["worktrees", "inspect", "{wt}"],
        "worktree",
    ),
    (
        "worktrees register",
        &["worktrees", "register", "{wt}", "--ttl", "7d"],
        "worktree-registration",
    ),
    ("caches", &["caches"], "caches"),
    (
        "large-files",
        &["large-files", "{root}", "--min-size", "1"],
        "large-files",
    ),
    ("rules list", &["rules", "list"], "rules-list"),
    ("rules validate", &["rules", "validate"], "rules-validate"),
    ("status", &["status"], "status"),
    ("check", &["check"], "check"),
    ("insights", &["insights"], "insights"),
    (
        "insights export",
        &["insights", "export", "--format", "json"],
        "insights",
    ),
    (
        "agent install",
        &["agent", "install", "codex", "--dry-run"],
        "agent",
    ),
    (
        "agent uninstall",
        &["agent", "uninstall", "codex", "--dry-run"],
        "agent",
    ),
    (
        "schedule enable",
        &["schedule", "enable", "--dry-run"],
        "schedule",
    ),
    (
        "schedule disable",
        &["schedule", "disable", "--dry-run"],
        "schedule",
    ),
    ("schedule status", &["schedule", "status"], "schedule"),
    ("doctor", &["doctor"], "doctor"),
    // `phase2` commands, without executing anything: `clean` only plans (the plan is recorded
    // in scull's own database), `clean caches --dry-run` previews, `history` lists the (empty)
    // audit log, `data` reads, exports to a temp folder and purges scull's own temp data.
    (
        "clean",
        &["clean", "--root", "{root}", "--allow-network-roots"],
        "clean-plan",
    ),
    (
        "clean caches",
        &["clean", "caches", "--dry-run"],
        "clean-caches",
    ),
    ("history", &["history"], "history"),
    ("data show", &["data", "show"], "data-show"),
    (
        "data export",
        &["data", "export", "--out", "{export}"],
        "data-export",
    ),
    // Emergency headroom (PRO-06) in the temp SCULL_HOME: a 256 MiB reserve, then released.
    ("headroom status", &["headroom", "status"], "headroom"),
    (
        "headroom create",
        &["headroom", "create", "--size", "256M", "--yes"],
        "headroom",
    ),
    (
        "headroom release",
        &["headroom", "release", "--yes"],
        "headroom",
    ),
    (
        "data purge",
        &["data", "purge", "--all", "--confirm"],
        "data-purge",
    ),
    // Last: rewrites the config (kept as config.toml.bak).
    (
        "init",
        &["init", "--yes", "--force", "--roots", "{root}"],
        "init",
    ),
];

/// Commands that never print NDJSON.
const NO_NDJSON: &[(&str, &str)] = &[
    ("completions", "prints a shell script"),
    ("mcp serve", "stdout carries JSON-RPC only"),
    (
        "approve",
        "interactive only: refuses --json/--ndjson (FX-MCP-08)",
    ),
    (
        "ask",
        "prints a prose answer from the user's own model; --json/--ndjson are refused",
    ),
];

/// Commands whose `--ndjson` output needs an executed cleanup operation: their line schema must
/// be published here, and FX-OUT-01 (`crates/scull-fixtures/tests/fx_out.rs`) validates their
/// real output against it inside a sandboxed fixture.
const NEEDS_OPERATION: &[(&str, &str)] = &[("restore", "restore")];

/// Flags that switch a command to another line schema, run like [`RUNS`].
const VARIANTS: &[(&str, &[&str], &str)] = &[
    ("doctor --report", &["doctor", "--report"], "doctor-report"),
    (
        "clean --purge-quarantine",
        &["clean", "--purge-quarantine"],
        "quarantine-purge",
    ),
    (
        "doctor --report --hash-names",
        &["doctor", "--report", "--hash-names"],
        "doctor-report",
    ),
];

/// Commands this build refuses (not implemented / inspect-only gate): stdout stays empty.
const REFUSED: &[(&str, &[&str])] = &[("insights purge", &["insights", "purge"])];

/// Every runnable command path in the clap tree (`rules list`, `scan`, ...): commands that
/// require a subcommand and `help` are left out.
fn command_paths() -> BTreeSet<String> {
    fn walk(cmd: &clap::Command, prefix: &str, out: &mut BTreeSet<String>) {
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            if name == "help" {
                continue;
            }
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix} {name}")
            };
            if !sub.is_subcommand_required_set() {
                out.insert(path.clone());
            }
            walk(sub, &path, out);
        }
    }
    let mut out = BTreeSet::new();
    walk(&scull_cli::cli::Cli::command(), "", &mut out);
    out
}

struct Env {
    home: tempfile::TempDir,
}

impl Env {
    fn scull_home(&self) -> PathBuf {
        self.home.path().join("scull-home")
    }

    fn run(&self, args: &[String]) -> Output {
        let home = self.home.path();
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(args)
            .arg("--ndjson")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("SCULL_HOME", self.scull_home())
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local").join("share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("LOCALAPPDATA", home.join("AppData").join("Local"))
            .env("APPDATA", home.join("AppData").join("Roaming"))
            .stdin(Stdio::null());
        for (k, _) in std::env::vars() {
            if (k.starts_with("SCULL_") && k != "SCULL_HOME")
                || k.to_ascii_lowercase().starts_with("npm_config")
                || matches!(
                    k.as_str(),
                    "CARGO_TARGET_DIR" | "PNPM_HOME" | "BUN_INSTALL" | "YARN_CACHE_FOLDER"
                )
            {
                c.env_remove(k);
            }
        }
        c.output().unwrap()
    }
}

fn schema_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("schemas/{name}.ndjson.schema.json"))
}

fn validator(path: &Path) -> jsonschema::Validator {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    jsonschema::validator_for(&v).unwrap()
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn toml_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "\\\\")
}

#[test]
fn every_ndjson_command_has_a_line_schema_and_validates() {
    // Coverage: every command is accounted for, and nothing is listed twice.
    let mut listed: Vec<&str> = RUNS.iter().map(|r| r.0).collect();
    listed.extend(NO_NDJSON.iter().map(|r| r.0));
    listed.extend(REFUSED.iter().map(|r| r.0));
    listed.extend(NEEDS_OPERATION.iter().map(|r| r.0));
    let unique: BTreeSet<String> = listed.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(unique.len(), listed.len(), "a command is listed twice");
    let paths = command_paths();
    let missing: Vec<&String> = paths.difference(&unique).collect();
    assert!(
        missing.is_empty(),
        "commands not checked for NDJSON line schemas (add them to RUNS with a published \
         schemas/<name>.ndjson.schema.json, or to NO_NDJSON with a reason): {missing:?}"
    );
    let stale: Vec<&String> = unique.difference(&paths).collect();
    assert!(stale.is_empty(), "no such command: {stale:?}");

    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 4096)
        .unwrap();
    let git = git_available();
    if git {
        let repo = fx.git_repo("repo").unwrap();
        fx.file("repo/README.md", "hello\n").unwrap();
        repo.commit_all("init").unwrap();
        repo.run(["worktree", "add", "--quiet", "-b", "task", "../repo-wt"])
            .unwrap();
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let app = root.join("app");
    let wt = root.join("repo-wt");
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    let config = env.scull_home().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        format!("[scan]\nroots = [\"{}\"]\n", toml_path(&root)),
    )
    .unwrap();
    let export = env.home.path().join("export");
    let subst = |a: &str| {
        a.replace("{root}", &root.to_string_lossy())
            .replace("{export}", &export.to_string_lossy())
            .replace("{app}", &app.to_string_lossy())
            .replace("{wt}", &wt.to_string_lossy())
    };

    for (path, args, name) in RUNS.iter().chain(VARIANTS) {
        if !git && args.contains(&"{wt}") {
            eprintln!("skipping `scull {path}`: git is not installed");
            continue;
        }
        let args: Vec<String> = args.iter().map(|a| subst(a)).collect();
        let o = env.run(&args);
        let out = String::from_utf8(o.stdout).unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(0), "{path}: {err}");
        let file = schema_path(name);
        assert!(
            file.is_file(),
            "`scull {path} --ndjson` prints lines but {} is not published",
            file.display()
        );
        let v = validator(&file);
        let lines: Vec<&str> = out.lines().collect();
        assert!(!lines.is_empty(), "{path}: no NDJSON lines");
        for line in lines {
            let doc: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{path}: not one JSON object per line ({e}): {line}"));
            assert_eq!(doc["schema"], format!("scull/{name}@1"), "{path}");
            let errors: Vec<String> = v.iter_errors(&doc).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "{path}: {errors:#?}\n{doc:#}");
        }
    }

    for (path, name) in NEEDS_OPERATION {
        assert!(
            schema_path(name).is_file(),
            "`scull {path} --ndjson` prints lines but its line schema is not published"
        );
    }

    for (path, args) in REFUSED {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let o = env.run(&args);
        assert_ne!(o.status.code(), Some(0), "{path} is refused in this build");
        assert!(
            o.stdout.is_empty(),
            "`scull {path} --ndjson` now prints output: move it to RUNS and publish its line \
             schema"
        );
    }

    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// A single-document command's line has exactly `schema`, `version`, `type` (fixed per
/// command) and `data`: the line schemas reject anything else.
#[test]
fn single_record_line_schemas_are_strict() {
    let v = validator(&schema_path("status"));
    let good = serde_json::json!({
        "schema": "scull/status@1", "version": "0", "type": "status",
        "data": { "state": "no_database", "detail": null, "now": 0, "last_scan": null,
                  "volumes": [], "forecasts": [] }
    });
    let mut wrong_type = good.clone();
    wrong_type["type"] = "doctor".into();
    let mut extra = good.clone();
    extra["extra"] = true.into();
    let mut wrong_schema = good.clone();
    wrong_schema["schema"] = "scull/doctor@1".into();
    for bad in [&wrong_type, &extra, &wrong_schema] {
        assert!(!v.is_valid(bad), "{bad}");
    }
}
