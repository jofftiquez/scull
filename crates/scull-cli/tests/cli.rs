//! End-to-end tests of the `scull` binary: help, version, usage errors, stub commands, JSON
//! error envelopes and shell completions (P1-CLI-01). No command here touches the filesystem
//! beyond reading the committed schema file.

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

fn scull(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scull"))
        .args(args)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CLICOLOR")
        .env_remove("FORCE_COLOR")
        .env("TERM", "dumb")
        .output()
        .expect("run scull")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("utf-8 stdout")
}

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("utf-8 stderr")
}

fn error_schema_validator() -> jsonschema::Validator {
    let generated = serde_json::to_value(scull_cli::output::json::error_envelope_schema())
        .expect("schema serializes");
    jsonschema::validator_for(&generated).expect("schema compiles")
}

const ALL_COMMANDS: [&str; 20] = [
    "scan",
    "projects",
    "worktrees",
    "caches",
    "clean",
    "large-files",
    "history",
    "restore",
    "approve",
    "rules",
    "init",
    "status",
    "check",
    "insights",
    "agent",
    "schedule",
    "data",
    "doctor",
    "mcp",
    "completions",
];

#[test]
fn help_lists_every_command() {
    for flag in ["--help", "-h"] {
        let o = scull(&[flag]);
        assert_eq!(o.status.code(), Some(0), "{flag}");
        let out = stdout(&o);
        for cmd in ALL_COMMANDS {
            assert!(
                out.lines()
                    .any(|l| l.trim_start().starts_with(&format!("{cmd} "))),
                "{flag}: `{cmd}` missing from help:\n{out}"
            );
        }
        for global in [
            "--json",
            "--ndjson",
            "--quiet",
            "--color",
            "--no-progress",
            "--verbose",
        ] {
            assert!(out.contains(global), "{flag}: {global} missing");
        }
        assert!(out.contains("130"), "exit codes documented");
    }
}

#[test]
fn version_includes_target_triple() {
    let o = scull(&["--version"]);
    assert_eq!(o.status.code(), Some(0));
    let out = stdout(&o);
    let line = out.trim_end();
    assert!(
        line.starts_with(&format!("scull {} (", env!("CARGO_PKG_VERSION"))),
        "{line}"
    );
    assert!(line.ends_with(')'), "{line}");
    let triple = &line[line.find('(').expect("paren") + 1..line.len() - 1];
    assert!(triple.split('-').count() >= 3, "target triple: {triple}");
}

#[test]
fn no_arguments_is_a_usage_error() {
    let o = scull(&[]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("Usage: scull"));
}

#[test]
fn unknown_command_exits_2() {
    let o = scull(&["frobnicate"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(o.stdout.is_empty());
    assert!(stderr(&o).contains("unrecognized subcommand"));
}

#[test]
fn json_and_ndjson_conflict_exits_2_with_json_error() {
    for args in [
        &["--json", "--ndjson", "caches"][..],
        &["caches", "--ndjson", "--json"][..],
        // Global flags given at different command levels.
        &["--json", "caches", "--ndjson"][..],
    ] {
        let o = scull(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(o.stdout.is_empty(), "stdout must stay clean: {args:?}");
        let err = stderr(&o);
        assert_eq!(err.lines().count(), 1, "one JSON line: {err}");
        let v: Value = serde_json::from_str(&err).expect("JSON error on stderr");
        assert_eq!(v["error"]["kind"], "usage");
        assert!(
            v["error"]["message"]
                .as_str()
                .expect("message")
                .contains("cannot be used with"),
            "{v}"
        );
        assert!(error_schema_validator().is_valid(&v), "{v}");
    }
}

/// Every not-yet-implemented command fails with exit 1 and names its tracking issue.
#[test]
fn stub_commands_exit_1_with_tracking_issue() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["worktrees", "register", ".", "--ttl", "7d"],
            "(tracked in #111)",
        ),
        (&["check"], "(tracked in #114)"),
        (&["insights"], "(tracked in #116)"),
        (&["agent", "install", "codex"], "(tracked in #110)"),
        (&["schedule", "enable"], "(tracked in #114)"),
    ];
    for (args, want) in cases {
        let o = scull(args);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        assert!(o.stdout.is_empty(), "{args:?}: stdout must stay empty");
        let err = stderr(&o);
        assert!(err.starts_with("scull: error: "), "{args:?}: {err}");
        assert!(err.contains(want), "{args:?}: {err}");
        assert!(err.contains("not implemented yet"), "{args:?}: {err}");
    }
}

#[test]
fn stub_errors_are_json_on_stderr_in_machine_modes() {
    let validator = error_schema_validator();
    for mode in ["--json", "--ndjson"] {
        let o = scull(&["check", mode]);
        assert_eq!(o.status.code(), Some(1), "{mode}");
        assert!(o.stdout.is_empty(), "{mode}: no partial document on stdout");
        let err = stderr(&o);
        assert_eq!(err.lines().count(), 1, "{mode}: {err}");
        let v: Value = serde_json::from_str(&err).expect("JSON");
        assert_eq!(v["schema"], "scull/error@1");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["error"]["kind"], "unsupported_platform");
        assert_eq!(
            v["error"]["message"],
            "`scull check` is not implemented yet (tracked in #114)"
        );
        let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{mode}: {errors:?}");
    }
}

#[test]
fn quiet_still_reports_errors() {
    let o = scull(&["check", "-q"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("tracked in #114"));
    // -q and -v are mutually exclusive.
    assert_eq!(scull(&["check", "-q", "-v"]).status.code(), Some(2));
    assert_eq!(scull(&["-q", "check", "-v"]).status.code(), Some(2));
}

#[test]
fn color_never_and_no_color_produce_no_escapes() {
    let o = scull(&["check", "--color", "never"]);
    assert!(!stderr(&o).contains('\x1b'));
    let o = Command::new(env!("CARGO_BIN_EXE_scull"))
        .args(["check"])
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("run");
    assert!(!String::from_utf8_lossy(&o.stderr).contains('\x1b'));
    let o = scull(&["check", "--color", "always"]);
    assert!(stderr(&o).contains("\x1b["), "--color always forces color");
    assert_eq!(
        scull(&["check", "--color", "rainbow"]).status.code(),
        Some(2)
    );
}

#[test]
fn completions_for_all_shells() {
    let markers = [
        ("bash", "complete"),
        ("zsh", "#compdef scull"),
        ("fish", "complete -c scull"),
        ("powershell", "Register-ArgumentCompleter"),
    ];
    for (shell, marker) in markers {
        let o = scull(&["completions", shell]);
        assert_eq!(o.status.code(), Some(0), "{shell}");
        let script = stdout(&o);
        assert!(script.len() > 1000, "{shell}: script too short");
        assert!(script.contains(marker), "{shell}: missing {marker:?}");
        for cmd in ["scan", "worktrees", "large-files", "completions"] {
            assert!(script.contains(cmd), "{shell}: `{cmd}` not completed");
        }
        assert!(o.stderr.is_empty(), "{shell}: {}", stderr(&o));
    }
    assert_eq!(scull(&["completions", "elvish"]).status.code(), Some(2));
    assert_eq!(scull(&["completions"]).status.code(), Some(2));
}

/// `schemas/error.schema.json` is the published copy of the error-envelope schema (to move to
/// the workspace `schemas/` in P0-ARCH-02). Real output must validate against it. Regenerate
/// with `SCULL_BLESS=1 cargo test -p scull-cli --test cli`.
#[test]
fn committed_error_schema_validates_real_output() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/error.schema.json");
    if std::env::var_os("SCULL_BLESS").is_some() {
        let generated =
            serde_json::to_string_pretty(&scull_cli::output::json::error_envelope_schema())
                .expect("serialize");
        std::fs::write(&path, generated + "\n").expect("write schema");
    }
    let committed: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("schema file exists"))
            .expect("schema is JSON");
    assert_eq!(
        committed["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    let validator = jsonschema::validator_for(&committed).expect("schema compiles");
    for args in [&["check", "--json"][..], &["frob", "--ndjson"][..]] {
        let o = scull(args);
        let v: Value = serde_json::from_str(&stderr(&o)).expect("JSON error");
        let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{args:?}: {errors:?}");
    }
}
