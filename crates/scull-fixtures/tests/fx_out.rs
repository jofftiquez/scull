//! End-to-end FX-OUT fixtures (docs/FIXTURES.md §3.14): output contracts of the real `scull`
//! binary over generated fixture trees.
//!
//! * **FX-OUT-01**: every command's `--json` document and `--ndjson` lines validate against the
//!   published schemas (`crates/scull-cli/schemas/`, `schemas/v1/`), every published command
//!   schema is exercised, error envelopes validate, hostile file names keep JSON and NDJSON
//!   valid (one record per line) and never reach the terminal raw, and a plan built from a real
//!   scan validates against `schemas/v1/plan.schema.json`.
//! * **FX-OUT-02**: the documented exit-code table (`docs/user/CLI.md`, CLI-05) is exactly `scull_core::ExitCode`,
//!   and the binary returns the documented code for each condition: 0, 1, 2, 3, 4, 5, 6 (the
//!   last three through `scull clean`, with I-4 on the CLI's own audit) and 130 (SIGINT on Unix, a console Ctrl-C on Windows).
//!
//! `scull clean`, `history` and `restore` run on fixture trees only, against items the fixture
//! created; the audited operation is checked against the disk (I-4).
//! * **FX-OUT-03**: the same tree through `scull scan --json` and through `scull mcp serve`
//!   (`scan_directory`, `get_cleanup_candidates`) gives identical IDs, tiers and categories.
//!
//! Nothing here writes outside its temp dirs: scull's directories are a temp `SCULL_HOME`, the
//! user's home another temp dir, and every fixture checks the invariants (I-2 canary untouched,
//! I-3 the tree unchanged, I-6 no network).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cli;
mod exec;

use cli::{Cli, Schemas, assert_exit, stdout, toml_path, workspace};
use scull_core::ExitCode;
use scull_fixtures::{Fixture, InvariantId, InvariantInputs, InvariantReport, NodeLock, days_ago};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn check(fx: &Fixture) -> InvariantReport {
    let report = fx.check_invariants(&InvariantInputs::new());
    println!("{report}");
    report.assert_ok();
    for id in [InvariantId::I2, InvariantId::I3] {
        assert!(report.outcome(id).ran(), "{id} did not run: {report}");
    }
    report
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Cargo (idle, with lockfile), npm with and without a lockfile, a pnpm workspace, a
/// `.scull-protect`ed project, a large file, and (with `git`) a repository with a committed
/// `target/` and a linked worktree.
fn tree(with_git: bool) -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 64 * 1024)
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .npm_project("nolock", "nolock", NodeLock::None)
        .unwrap()
        .node_modules("nolock", &["x"])
        .unwrap()
        .pnpm_workspace("mono", "mono", &["a"])
        .unwrap()
        .node_modules("mono/packages/a", &["lodash"])
        .unwrap()
        .cargo_project("vault", "vault")
        .unwrap()
        .cargo_target("vault", "vault", 1024)
        .unwrap()
        .file("vault/.scull-protect", "")
        .unwrap()
        .file_of_size("media/recording.mov", 300 * 1024)
        .unwrap();
    if with_git {
        let repo = fx.git_repo("repo").unwrap();
        fx.file("repo/.gitignore", "node_modules/\n")
            .unwrap()
            .cargo_project("repo/tracked", "tracked")
            .unwrap()
            .cargo_target("repo/tracked", "tracked", 512)
            .unwrap();
        repo.commit_all("initial").unwrap();
        repo.run(["worktree", "add", "--quiet", "-b", "feature", "../repo-wt"])
            .unwrap();
    }
    fx.checkpoint().unwrap();
    fx
}

/// FX-OUT-01: every command with machine output, in both modes, over one fixture tree, against
/// the published schemas; every published command schema is used; nothing on disk changes
/// except scull's own temp directories.
#[test]
fn fx_out_01_every_json_and_ndjson_output_validates_against_published_schemas() {
    let with_git = git_available();
    let fx = tree(with_git);
    let cli = Cli::new();
    let schemas = Schemas::default();
    let r = s(fx.root());

    // Read-only listing commands: document and line schemas both published.
    let scan = schemas.json(&cli, &["scan", &r], "scan");
    assert!(
        scan["data"]["candidates"].as_array().unwrap().len() >= 5,
        "{scan:#}"
    );
    let lines = schemas.ndjson(&cli, &["scan", &r, "--no-history"], "scan");
    assert_eq!(lines.last().unwrap()["type"], "summary");
    schemas.json(&cli, &["projects", "--root", &r], "projects");
    schemas.ndjson(&cli, &["projects", "--root", &r], "projects");
    let app = s(&fx.root().join("app"));
    schemas.json(
        &cli,
        &["projects", "inspect", &app, "--root", &r],
        "project",
    );
    schemas.ndjson(
        &cli,
        &["projects", "inspect", &app, "--root", &r],
        "project",
    );
    schemas.json(&cli, &["worktrees", "--root", &r], "worktrees");
    schemas.ndjson(&cli, &["worktrees", "--root", &r], "worktrees");
    if with_git {
        let wt = s(&fx.root().join("repo-wt"));
        schemas.json(&cli, &["worktrees", "inspect", &wt], "worktree");
        schemas.ndjson(&cli, &["worktrees", "inspect", &wt], "worktree");
        // `worktrees register` (main, #271): recorded in scull's own data folder only.
        let reg = ["worktrees", "register", wt.as_str(), "--ttl", "7d"];
        schemas.json(&cli, &reg, "worktree-registration");
        schemas.ndjson(&cli, &reg, "worktree-registration");
    }
    schemas.json(&cli, &["caches"], "caches");
    schemas.ndjson(&cli, &["caches"], "caches");

    // Single-document commands.
    let large = schemas.json(
        &cli,
        &["large-files", &r, "--min-size", "256K"],
        "large-files",
    );
    for f in large["data"]["files"].as_array().unwrap() {
        schemas.validate("v1/candidate.schema.json", f, "large file");
    }
    schemas.ndjson(
        &cli,
        &["large-files", &r, "--min-size", "256K"],
        "large-files",
    );
    schemas.json(&cli, &["status"], "status");
    schemas.ndjson(&cli, &["status"], "status");
    schemas.json(&cli, &["doctor"], "doctor");
    schemas.ndjson(&cli, &["doctor"], "doctor");
    schemas.json(&cli, &["rules", "list"], "rules-list");
    schemas.ndjson(&cli, &["rules", "list"], "rules-list");
    schemas.json(&cli, &["rules", "validate"], "rules-validate");
    schemas.ndjson(&cli, &["rules", "validate"], "rules-validate");
    // `scull rules test` (#60, from main) on the shipped example rule and its cases.
    let example_rule = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples/rules/godot.toml");
    let example_rule = example_rule.to_str().unwrap();
    schemas.json(&cli, &["rules", "test", example_rule], "rules-test");
    schemas.ndjson(&cli, &["rules", "test", example_rule], "rules-test");
    schemas.json(&cli, &["init", "--yes", "--roots", &r], "init");
    schemas.ndjson(&cli, &["init", "--yes", "--force", "--roots", &r], "init");
    // The config file `init` wrote validates against the published config schema.
    let config_file = cli.scull_home().join("config").join("config.toml");
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(&config_file).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", config_file.display()));
    schemas.validate(
        "v1/config.schema.json",
        &serde_json::to_value(&config).unwrap(),
        "config.toml written by init",
    );
    schemas.json(&cli, &["data", "show"], "data-show");
    schemas.ndjson(&cli, &["data", "show"], "data-show");
    let out1 = cli.home().join("export-json");
    let export = schemas.json(&cli, &["data", "export", "--out", &s(&out1)], "data-export");
    println!("export: {}", export["data"]);
    let dump_file = out1.join("scull-data.json");
    let dump: Value = serde_json::from_slice(&std::fs::read(&dump_file).unwrap()).unwrap();
    assert_eq!(dump["schema"], "scull/data-dump@1");
    schemas.validate("data-dump.schema.json", &dump, "scull-data.json");
    let out2 = cli.home().join("export-ndjson");
    schemas.ndjson(&cli, &["data", "export", "--out", &s(&out2)], "data-export");
    schemas.json(&cli, &["data", "purge", "--all", "--confirm"], "data-purge");
    schemas.ndjson(&cli, &["data", "purge", "--all", "--confirm"], "data-purge");

    // Phase 4 read-only commands from main: forecast check and insights (exit 0 whether or not
    // an alert fired; both only read history and the volumes' free space).
    schemas.json(&cli, &["check"], "check");
    schemas.ndjson(&cli, &["check"], "check");
    schemas.json(&cli, &["insights"], "insights");
    schemas.ndjson(&cli, &["insights"], "insights");
    // `insights purge` (#31): forgets the history recorded in the temp `SCULL_HOME` only.
    let purged = schemas.json(&cli, &["insights", "purge", "--confirm"], "insights-purge");
    assert_eq!(purged["data"]["database_found"], true, "{purged:#}");
    schemas.ndjson(&cli, &["insights", "purge", "--confirm"], "insights-purge");
    // `agent install` (main, #267) as a dry run: shows the diff for the fixture home's Cursor
    // config and writes nothing.
    schemas.json(&cli, &["agent", "install", "cursor", "--dry-run"], "agent");
    schemas.ndjson(&cli, &["agent", "install", "cursor", "--dry-run"], "agent");
    // `schedule` (main, #281) without registering anything: `enable --dry-run` shows the unit
    // files and commands for the fixture home and stops; `status` with nothing registered runs
    // no tool.
    let dry = schemas.json(&cli, &["schedule", "enable", "--dry-run"], "schedule");
    assert_eq!(dry["data"]["dry_run"], true, "{dry:#}");
    assert_eq!(dry["data"]["applied"], false, "{dry:#}");
    schemas.ndjson(&cli, &["schedule", "enable", "--dry-run"], "schedule");
    let status = schemas.json(&cli, &["schedule", "status"], "schedule");
    schemas.ndjson(&cli, &["schedule", "status"], "schedule");
    // `doctor --report` (main, #248): the shareable report, printed and never written without
    // `--out`; `--hash-names` hides project names too.
    for extra in [&[][..], &["--hash-names"][..]] {
        let mut a = vec!["doctor", "--report"];
        a.extend_from_slice(extra);
        schemas.json(&cli, &a, "doctor-report");
        schemas.ndjson(&cli, &a, "doctor-report");
    }
    assert_eq!(status["data"]["registered"], false, "{status:#}");
    // `headroom status` (PRO-06): read-only, nothing reserved in the fixture home.
    let headroom = schemas.json(&cli, &["headroom", "status"], "headroom");
    assert_eq!(headroom["data"]["status"]["state"], "none", "{headroom:#}");
    schemas.ndjson(&cli, &["headroom", "status"], "headroom");

    // Phase 2 commands: clean, history, restore, quarantine purge, Quick Clean preview.
    clean_history_restore_outputs(&schemas, &cli);

    // Error envelopes, in both machine modes.
    let missing = s(&fx.root().join("no-such-dir"));
    for mode in ["--json", "--ndjson"] {
        let o = cli.run(&["scan", &missing, mode]);
        assert_exit(&o, 1);
        let e = schemas.error(&o, &format!("scan missing root {mode}"));
        assert_eq!(e["error"]["kind"], "not_found");
        let o = cli.run(&["no-such-command", mode]);
        assert_exit(&o, 2);
        schemas.error(&o, &format!("unknown command {mode}"));
    }

    // Every published command schema was exercised (a new command can't skip this fixture).
    let used = schemas.used();
    let mut unused = Vec::new();
    for e in std::fs::read_dir(workspace().join("crates/scull-cli/schemas")).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        let needs_git = name.starts_with("worktree.") || name.starts_with("worktree-registration.");
        if !used.contains(&name) && !(needs_git && !with_git) {
            unused.push(name);
        }
    }
    assert!(
        unused.is_empty(),
        "published schemas never validated against real output: {unused:?}"
    );
    for core in [
        "v1/candidate.schema.json",
        "v1/error.schema.json",
        "v1/config.schema.json",
    ] {
        assert!(used.contains(core), "{core} unused");
    }
    println!("schemas validated against real output: {used:#?}");
    check(&fx);
}

/// Phase 2 commands over their own fixture, all in machine modes: plan (`clean --output`),
/// review (`clean --plan --dry-run`), execute (`clean --plan --confirm`), `history` (list and
/// one operation), `restore` (preview and confirmed), `clean --purge-quarantine` (preview) and
/// `clean caches --dry-run`. The executed operation's audit is checked against the disk (I-4)
/// before the restore; every invariant runs at the end.
fn clean_history_restore_outputs(schemas: &Schemas, _shared: &Cli) {
    let mut fx = Fixture::new().unwrap();
    // `lib/target`: idle Cargo output (S0, deleted; S2 where "in use" is unknown).
    // `web/node_modules`: npm without a lockfile (S2 everywhere, so quarantined).
    fx.cargo_project("lib", "lib")
        .unwrap()
        .cargo_target("lib", "lib", 16 * 1024)
        .unwrap()
        .age("lib", days_ago(5))
        .unwrap()
        .npm_project("web", "web", NodeLock::None)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .age("web", days_ago(5))
        .unwrap();
    fx.checkpoint().unwrap();
    // Its own environment, sandboxed in this fixture (P2-EXEC-13).
    let cli = &Cli::new().sandboxed(&fx);
    let r = s(fx.root());
    // Named one by one (`--item`): the only way to plan S2 items.
    let lib = s(&fx.root().join("lib/target"));
    let app = s(&fx.root().join("web/node_modules"));
    let plan_file = s(&fx.base().join("plan.json"));
    let planned = schemas.json(
        cli,
        &[
            "clean",
            "--root",
            &r,
            "--allow-network-roots",
            "--item",
            &lib,
            "--item",
            &app,
            "--output",
            &plan_file,
        ],
        "clean-plan",
    );
    println!("clean plan items: {:#}", planned["data"]["items"]);
    assert_eq!(
        planned["data"]["items"].as_array().unwrap().len(),
        2,
        "{planned:#}"
    );
    schemas.validate(
        "v1/plan.schema.json",
        &serde_json::from_slice(&std::fs::read(&plan_file).unwrap()).unwrap(),
        "plan file written by scull clean --output",
    );
    schemas.ndjson(
        cli,
        &[
            "clean",
            "--root",
            &r,
            "--allow-network-roots",
            "--item",
            &lib,
            "--item",
            &app,
        ],
        "clean-plan",
    );
    schemas.json(
        cli,
        &[
            "clean",
            "--plan",
            &plan_file,
            "--dry-run",
            "--allow-network-roots",
        ],
        "clean-plan",
    );
    let run = schemas.json(
        cli,
        &[
            "clean",
            "--plan",
            &plan_file,
            "--confirm",
            "--allow-network-roots",
        ],
        "clean-run",
    );
    let op = run["data"]["operation_id"].as_str().unwrap().to_owned();
    println!("clean run {op}: {:#}", run["data"]["items"]);
    assert!(!fx.root().join("lib/target").exists());
    assert!(!fx.root().join("web/node_modules").exists());
    // I-4 on the operation the CLI recorded in its own database.
    {
        let store =
            scull_storage::Store::open(&cli.db(), &scull_storage::DefaultPermissions).unwrap();
        let id: scull_core::OperationId = serde_json::from_value(json!(op)).unwrap();
        let report = fx.check_invariants(
            &InvariantInputs::new()
                .allow("lib/target")
                .allow("web/node_modules")
                .audit(&store, &id),
        );
        println!("{report}");
        report.assert_ok();
        assert_eq!(
            report.outcome(InvariantId::I4),
            &scull_fixtures::Outcome::Passed
        );
    }

    schemas.json(cli, &["history"], "history");
    schemas.ndjson(cli, &["history"], "history");
    schemas.json(cli, &["history", &op], "history-operation");
    schemas.ndjson(cli, &["history", &op], "history-operation");
    schemas.json(cli, &["restore", &op, "--dry-run"], "restore");
    schemas.ndjson(cli, &["restore", &op, "--dry-run"], "restore");
    let restored = schemas.json(cli, &["restore", &op, "--confirm"], "restore");
    println!("restore: {:#}", restored["data"]);
    assert!(
        fx.root()
            .join("web/node_modules/left-pad/package.json")
            .is_file(),
        "the quarantined item is back"
    );
    schemas.json(cli, &["clean", "--purge-quarantine"], "quarantine-purge");
    schemas.ndjson(cli, &["clean", "--purge-quarantine"], "quarantine-purge");
    schemas.json(cli, &["clean", "caches", "--dry-run"], "clean-caches");
    schemas.ndjson(cli, &["clean", "caches", "--dry-run"], "clean-caches");
    // An execution's NDJSON: plan the restored item again and run it.
    let plan2 = s(&fx.base().join("plan2.json"));
    schemas.json(
        cli,
        &[
            "clean",
            "--root",
            &r,
            "--allow-network-roots",
            "--item",
            &app,
            "--output",
            &plan2,
        ],
        "clean-plan",
    );
    schemas.ndjson(
        cli,
        &[
            "clean",
            "--plan",
            &plan2,
            "--confirm",
            "--allow-network-roots",
        ],
        "clean-run",
    );
    let report = fx.check_invariants(
        &InvariantInputs::new()
            .allow("lib/target")
            .allow("web/node_modules"),
    );
    println!("{report}");
    report.assert_ok();
}

/// Hostile names: control characters (Unix), ANSI escapes, a right-to-left override and
/// zero-width characters in project folder names.
fn hostile_tree() -> (Fixture, Vec<&'static str>) {
    let mut names = vec!["rtl\u{202e}gpj.exe", "zero\u{200b}width", "emoji-\u{1f4e6}"];
    if cfg!(unix) {
        names.extend(["new\nline", "ansi\u{1b}[2J\u{1b}[31mred", "bell\u{7}cr\r"]);
    }
    let mut fx = Fixture::new().unwrap();
    for n in &names {
        fx.cargo_project(n, "evil")
            .unwrap()
            .cargo_target(n, "evil", 4096)
            .unwrap();
    }
    fx.checkpoint().unwrap();
    (fx, names)
}

/// FX-OUT-01 (output safety): hostile names keep `--json` valid and exact, `--ndjson` one record
/// per line (a newline in a name never splits a record), and human output free of raw control,
/// bidi or zero-width characters (SAFE-17, T-51).
#[test]
fn fx_out_01_hostile_names_keep_json_and_ndjson_valid_and_terminal_output_escaped() {
    let (fx, names) = hostile_tree();
    let cli = Cli::new();
    let schemas = Schemas::default();
    let r = s(fx.root());

    let doc = schemas.json(&cli, &["scan", &r, "--no-history"], "scan");
    let paths: Vec<String> = doc["data"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap().to_owned())
        .collect();
    for n in &names {
        assert!(
            paths.iter().any(|p| p.contains(n)),
            "JSON keeps the exact name {n:?}: {paths:#?}"
        );
    }
    let lines = schemas.ndjson(&cli, &["scan", &r, "--no-history"], "scan");
    let cand_lines = lines.iter().filter(|l| l["type"] == "candidate").count();
    assert_eq!(cand_lines, paths.len(), "one NDJSON record per candidate");
    assert_eq!(
        lines.last().unwrap()["data"]["candidate_count"],
        paths.len() as u64
    );

    for args in [
        vec!["scan", &r, "--no-history"],
        vec!["projects", "--root", &r],
        vec!["large-files", &r, "--min-size", "1K"],
    ] {
        let o = cli.run(&args);
        assert_exit(&o, 0);
        let out = stdout(&o);
        let raw: Vec<char> = out
            .chars()
            .filter(|c| *c != '\n' && scull_cli::output::escape::needs_escape(*c))
            .collect();
        assert!(
            raw.is_empty(),
            "{args:?}: raw control/bidi characters reach the terminal: {raw:?}\n{out}"
        );
        if cfg!(unix) {
            // The escaped forms are what the user sees.
            assert!(out.contains("\\n") || out.contains("\\x1b"), "{out}");
        }
        assert!(
            out.contains("\\u{202e}"),
            "{args:?}: RTL override shown escaped:\n{out}"
        );
    }
    check(&fx);
}

/// FX-OUT-01 (plan files, SAFE-11): a plan built from a real scan's candidates, serialized as
/// `scull clean --output` writes it, validates against `schemas/v1/plan.schema.json`, and so do
/// its tier breakdowns against `tier-breakdown.schema.json`.
#[test]
fn fx_out_01_plan_from_a_real_scan_validates_against_the_plan_schema() {
    let fx = exec::engine::three_tiers();
    let h = exec::engine::harness();
    let scan = h.scan(&fx);
    let p = exec::engine::plan(
        &fx,
        &scan,
        &["lib/target", "app/target", "web/node_modules"],
        "PXOUT1",
    );
    let schemas = Schemas::default();
    let file = fx.base().join("plan.json");
    std::fs::write(&file, serde_json::to_vec_pretty(&p).unwrap()).unwrap();
    let v: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    schemas.validate("v1/plan.schema.json", &v, "plan file");
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    for c in &scan.candidates {
        schemas.validate(
            "v1/candidate.schema.json",
            &serde_json::to_value(c).unwrap(),
            "engine candidate",
        );
    }
    let report = serde_json::to_value(&scan).unwrap();
    let mut breakdowns = 0;
    for (k, b) in report["totals"].as_object().unwrap() {
        if b.get("s0").is_some() {
            schemas.validate("v1/tier-breakdown.schema.json", b, k);
            breakdowns += 1;
        }
    }
    assert!(
        breakdowns > 0,
        "no tier breakdown in {:#}",
        report["totals"]
    );
    check(&fx);
}

// ------------------------------------------------------------------------------------------
// FX-OUT-02: exit codes
// ------------------------------------------------------------------------------------------

/// The `| Code | Meaning |` exit-code table of `docs/user/CLI.md` (CLI-05).
fn prd_exit_codes() -> BTreeMap<i32, String> {
    let prd = std::fs::read_to_string(workspace().join("docs/user/CLI.md")).unwrap();
    let mut out = BTreeMap::new();
    let mut in_table = false;
    for line in prd.lines() {
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if cells.first() == Some(&"Code") && cells.get(1) == Some(&"Meaning") {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if !line.trim_start().starts_with('|') {
            break;
        }
        if let Ok(n) = cells[0].parse::<i32>() {
            out.insert(n, cells[1].to_owned());
        }
    }
    out
}

/// FX-OUT-02 (contract): the documented exit codes are exactly the ones the code defines.
#[test]
fn fx_out_02_prd_exit_code_table_is_exactly_the_exit_code_enum() {
    let doc = prd_exit_codes();
    println!("documented exit codes: {doc:#?}");
    let code_set: BTreeSet<i32> = ExitCode::ALL.iter().map(|c| c.as_i32()).collect();
    let doc_set: BTreeSet<i32> = doc.keys().copied().collect();
    assert_eq!(code_set, doc_set);
    let mut fx = Fixture::new().unwrap();
    fx.checkpoint().unwrap();
    check(&fx);
}

/// A `scull mcp serve` configured for `roots`.
fn mcp_cli(roots: &[&Path]) -> Cli {
    let cli = Cli::new();
    let list: Vec<String> = roots
        .iter()
        .map(|r| format!("\"{}\"", toml_path(r)))
        .collect();
    cli.write_config(&format!(
        "[mcp]\nenabled = true\n\n[scan]\nroots = [{}]\n",
        list.join(", ")
    ));
    cli
}

/// A running `scull mcp serve` speaking JSON-RPC lines over its stdio.
struct Mcp {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(cli: &Cli) -> Self {
        Self::start_with(cli, &["mcp", "serve"])
    }

    /// `scull <args>` (e.g. `mcp serve --allow-destructive`), initialized.
    fn start_with(cli: &Cli, args: &[&str]) -> Self {
        Self::start_command(cli.command(args))
    }

    /// `command` (a `scull mcp serve`), initialized.
    fn start_command(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let mut m = Self {
            child,
            out,
            next: 1,
        };
        let init = m.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "scull-fixtures", "version": "0" }
            }),
        );
        assert!(init.get("result").is_some(), "{init}");
        m.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        m
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            assert!(
                self.out.read_line(&mut line).unwrap() > 0,
                "server closed stdout"
            );
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout carries only JSON-RPC ({e}): {line:?}"));
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    /// `tools/call` → the tool's structured result.
    fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let result = &r["result"];
        assert_ne!(result["isError"], true, "{name}: {r}");
        if let Some(sc) = result.get("structuredContent") {
            return sc.clone();
        }
        let text = result["content"][0]["text"].as_str().expect("text content");
        serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: not JSON ({e}): {text}"))
    }

    /// Close stdin and wait for the exit code.
    fn finish(mut self) -> Option<i32> {
        drop(self.child.stdin.take());
        self.child.wait().unwrap().code()
    }
}

/// Run `args`, assert the exit code `want` (and, for errors, the JSON error kind), and record
/// the condition under its code.
fn expect(
    seen: &mut BTreeMap<i32, Vec<String>>,
    schemas: &Schemas,
    cli: &Cli,
    args: &[&str],
    want: ExitCode,
    kind: Option<&str>,
) {
    let o = cli.run(args);
    assert_exit(&o, want.as_i32());
    if let Some(kind) = kind {
        let e = schemas.error(&o, &args.join(" "));
        assert_eq!(e["error"]["kind"], kind, "{e:#}");
    }
    seen.entry(want.as_i32())
        .or_default()
        .push(format!("scull {}", args.join(" ")));
}

/// FX-OUT-02 (end to end): every documented exit condition returns the documented code,
/// from the real binary: 0, 1, 2 (usage and config), 3 (an item skipped during `scull clean`),
/// 4 (a replayed plan), 5 (policy), 6 (execution lock held) and 130 (SIGINT on Unix, a console Ctrl-C on Windows).
#[test]
fn fx_out_02_binary_returns_the_documented_exit_codes() {
    let fx = tree(false);
    let r = s(fx.root());
    let schemas = Schemas::default();
    let doc = prd_exit_codes();
    let mut seen: BTreeMap<i32, Vec<String>> = BTreeMap::new();
    let cli = Cli::new();
    // 0: success.
    expect(
        &mut seen,
        &schemas,
        &cli,
        &["scan", &r, "--json"],
        ExitCode::Success,
        None,
    );
    expect(
        &mut seen,
        &schemas,
        &cli,
        &["doctor", "--json"],
        ExitCode::Success,
        None,
    );
    // 1: general error (a missing root; a command not built yet).
    let missing = s(&fx.root().join("missing"));
    expect(
        &mut seen,
        &schemas,
        &cli,
        &["scan", &missing, "--json"],
        ExitCode::General,
        Some("not_found"),
    );
    // 2: usage and configuration errors.
    expect(
        &mut seen,
        &schemas,
        &cli,
        &["scan", &r, "--bogus", "--json"],
        ExitCode::Usage,
        Some("usage"),
    );
    expect(
        &mut seen,
        &schemas,
        &cli,
        &["clean", "--confirm", "--json"],
        ExitCode::Usage,
        Some("usage"),
    );
    let broken = Cli::new();
    broken.write_config("garbage = [\n");
    expect(
        &mut seen,
        &schemas,
        &broken,
        &["scan", &r, "--json"],
        ExitCode::Usage,
        Some("config"),
    );
    // 5: blocked by safety policy (a broad root, CFG-04).
    let home_is_root = Cli::new().env("HOME", &r).env("USERPROFILE", &r);
    // `env` is applied after the defaults, so the fixture root *is* the home folder here.
    expect(
        &mut seen,
        &schemas,
        &home_is_root,
        &["scan", &r, "--json"],
        ExitCode::SafetyBlocked,
        Some("safety_blocked"),
    );
    // 3, 4, 6: `scull clean` on its own fixture.
    let (ex, ex_cli, ex_op) = exec_exit_codes(&schemas, &mut seen);
    // 130: cancelled by the user (Ctrl-C), Unix: SIGINT to a running `scull mcp serve`.
    #[cfg(unix)]
    {
        let mcp = mcp_cli(&[fx.root()]);
        let mut m = Mcp::start(&mcp);
        let pid = m.child.id().to_string();
        let k = Command::new("kill").args(["-INT", &pid]).status().unwrap();
        assert!(k.success());
        let status = m.child.wait().unwrap();
        drop(m.child.stdin.take());
        assert_eq!(
            status.code(),
            Some(ExitCode::Cancelled.as_i32()),
            "{status:?}"
        );
        seen.entry(130)
            .or_default()
            .push("SIGINT to scull mcp serve (after the MCP handshake)".into());
    }
    // 130 on Windows: a real console Ctrl-C to `scull mcp serve`, started with a console of its
    // own, sent by a helper process (`fx_out_02_ctrl_c_sender`).
    #[cfg(windows)]
    {
        windows_ctrl_c_exits_130(&fx);
        seen.entry(130)
            .or_default()
            .push("console Ctrl-C to scull mcp serve (after the MCP handshake)".into());
    }
    println!("--- exit codes observed (docs/user/CLI.md) ---");
    for (n, meaning) in &doc {
        match seen.get(n) {
            Some(cmds) => println!("  {n:>3} {meaning}: {}", cmds.join("; ")),
            None => println!("  {n:>3} {meaning}: NOT EXERCISED on this OS"),
        }
    }
    let unreached: BTreeSet<i32> = doc
        .keys()
        .copied()
        .filter(|n| !seen.contains_key(n))
        .collect();
    assert!(unreached.is_empty(), "codes not exercised: {unreached:?}");
    check(&fx);
    // I-4 on the partly failed operation, from the CLI's own database.
    let store =
        scull_storage::Store::open(&ex_cli.db(), &scull_storage::DefaultPermissions).unwrap();
    let id: scull_core::OperationId = serde_json::from_value(json!(ex_op)).unwrap();
    let report = ex.check_invariants(
        &InvariantInputs::new()
            .allow("p1/target")
            .allow("p2/.scull-protect")
            .audit(&store, &id),
    );
    println!("{report}");
    report.assert_ok();
    assert_eq!(
        report.outcome(InvariantId::I4),
        &scull_fixtures::Outcome::Passed
    );
}

/// Exit codes 3, 4 and 6 through `scull clean` on three idle Cargo targets (S0):
/// * 3: `p1` and `p2` planned, then `p2` is protected (`.scull-protect`): `p2` is skipped,
///   `p1` deleted, exit 3 (completed with partial failures);
/// * 4: the same plan run again: rejected (single use), exit 4;
/// * 6: `p3` planned while another operation holds the execution lock: exit 6, nothing runs.
fn exec_exit_codes(
    schemas: &Schemas,
    seen: &mut BTreeMap<i32, Vec<String>>,
) -> (Fixture, Cli, String) {
    let mut fx = Fixture::new().unwrap();
    for p in ["p1", "p2", "p3"] {
        fx.cargo_project(p, p)
            .unwrap()
            .cargo_target(p, p, 8 * 1024)
            .unwrap()
            .age(p, days_ago(5))
            .unwrap();
    }
    fx.checkpoint().unwrap();
    let cli = Cli::new().sandboxed(&fx);
    let r = s(fx.root());
    let plan = |file: &str, items: &[&str]| {
        let path = s(&fx.base().join(file));
        let mut args = vec![
            "clean".to_owned(),
            "--root".into(),
            r.clone(),
            "--allow-network-roots".into(),
            "--output".into(),
            path.clone(),
        ];
        for i in items {
            args.push("--item".into());
            args.push(s(&fx.root().join(i)));
        }
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        schemas.json(&cli, &a, "clean-plan");
        path
    };
    let run = |file: &str| {
        vec![
            "clean".to_owned(),
            "--plan".into(),
            file.to_owned(),
            "--confirm".into(),
            "--allow-network-roots".into(),
            "--json".into(),
        ]
    };

    let p12 = plan("p12.json", &["p1/target", "p2/target"]);
    std::fs::write(fx.root().join("p2/.scull-protect"), "").unwrap();
    let args = run(&p12);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let o = cli.run(&a);
    assert_exit(&o, ExitCode::PartialFailure.as_i32());
    let doc: Value = serde_json::from_slice(&o.stdout).unwrap();
    schemas.validate("clean-run.schema.json", &doc, "partial clean run");
    let op = doc["data"]["operation_id"].as_str().unwrap().to_owned();
    assert!(!fx.root().join("p1/target").exists());
    assert!(fx.root().join("p2/target/debug/p2").is_file(), "p2 skipped");
    seen.entry(3)
        .or_default()
        .push("scull clean --plan <file> --confirm (one item protected after planning)".into());

    let o = cli.run(&a);
    assert_exit(&o, ExitCode::PlanRejected.as_i32());
    let e = schemas.error(&o, "replayed plan");
    println!("replay: {}", e["error"]);
    assert!(
        e["error"]["kind"].as_str().unwrap().starts_with("plan_"),
        "{e:#}"
    );
    seen.entry(4)
        .or_default()
        .push("scull clean --plan <file> --confirm (the same plan again)".into());

    let p3 = plan("p3.json", &["p3/target"]);
    let lock = scull_cleaner::Executor::acquire(&scull_cleaner::LockOptions::at(
        cli.scull_home().join("state").join("execution.lock"),
    ))
    .unwrap();
    let args = run(&p3);
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let o = cli.run(&a);
    drop(lock);
    assert_exit(&o, ExitCode::LockHeld.as_i32());
    assert_eq!(schemas.error(&o, "lock held")["error"]["kind"], "lock_held");
    assert!(
        fx.root().join("p3/target/debug/p3").is_file(),
        "nothing ran"
    );
    seen.entry(6)
        .or_default()
        .push("scull clean --plan <file> --confirm (execution lock held)".into());
    (fx, cli, op)
}

// ------------------------------------------------------------------------------------------
// FX-OUT-03: CLI and MCP agree
// ------------------------------------------------------------------------------------------

/// (id, tier, category) of every candidate in a list of candidate objects.
/// FX-OUT-03 (plan contents): the same items, planned through the CLI binary
/// (`clean --item <id> --dry-run --json`) and through the MCP server the same binary serves in
/// destructive mode (`prepare_cleanup`), give the same plan: the same item IDs, categories,
/// tiers, actions and reclaimable bytes, in the same order, and the same total. Nothing is
/// executed on either side.
#[test]
fn fx_out_03_cli_and_mcp_plan_identical_contents() {
    // Idle artifacts (S0..S2, depending on what this machine can observe), so both sides
    // have several items to plan.
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 64 * 1024)
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .npm_project("nolock", "nolock", NodeLock::None)
        .unwrap()
        .node_modules("nolock", &["x"])
        .unwrap()
        .cargo_project("vault", "vault")
        .unwrap()
        .cargo_target("vault", "vault", 1024)
        .unwrap()
        .file("vault/.scull-protect", "")
        .unwrap();
    for d in ["app", "web", "nolock", "vault"] {
        fx.age(d, days_ago(30)).unwrap();
    }
    fx.checkpoint().unwrap();
    let cli = mcp_cli(&[fx.root()]);
    cli.write_config(&format!(
        "[mcp]\nenabled = true\ndestructive = true\n\n[scan]\nroots = [\"{}\"]\n",
        toml_path(fx.root())
    ));
    let r = s(fx.root());

    let mut m = Mcp::start_with(&cli, &["mcp", "serve", "--allow-destructive"]);
    if scull_platform::is_elevated().blocks_destructive() {
        // SAFE-14: elevated (GitHub's Windows runners), the server never offers the plan tools,
        // so there is no MCP plan to compare; the non-elevated Windows run is #65.
        let tools = m.request("tools/list", json!({}));
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"prepare_cleanup"), "{names:?}");
        assert_eq!(m.finish(), Some(0));
        eprintln!(
            "SKIP fx_out_03_cli_and_mcp_plan_identical_contents: running elevated, so scull mcp \
             serves read-only tools (SAFE-14)"
        );
        check(&fx);
        return;
    }
    let cands = m.tool("get_cleanup_candidates", json!({ "max_results": 1000 }));
    let ids: Vec<String> = cands["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["tier"] != "S3")
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    // app/target, web/node_modules and nolock/node_modules; vault is protected (S3).
    assert_eq!(ids.len(), 3, "{cands:#}");
    let prepared = m.tool("prepare_cleanup", json!({ "item_ids": ids }));
    let exit = m.finish();
    assert_eq!(exit, Some(0));
    let via_mcp: Vec<(String, String, String, String, u64)> = prepared["items"]
        .as_array()
        .unwrap_or_else(|| panic!("{prepared:#}"))
        .iter()
        .map(|i| {
            (
                i["id"].as_str().unwrap().to_owned(),
                i["category"].as_str().unwrap().to_owned(),
                i["tier"].as_str().unwrap().to_owned(),
                i["action"].as_str().unwrap().to_owned(),
                i["reclaimable_bytes"].as_u64().unwrap(),
            )
        })
        .collect();

    let mut args: Vec<&str> = vec!["clean", "--root", &r, "--allow-network-roots", "--dry-run"];
    for id in &ids {
        args.push("--item");
        args.push(id);
    }
    let doc = Schemas::default().json(&cli, &args, "clean-plan");
    let plan = &doc["data"]["plan"];
    let via_cli: Vec<(String, String, String, String, u64)> = plan["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["item_id"].as_str().unwrap().to_owned(),
                i["category"].as_str().unwrap().to_owned(),
                i["tier"].as_str().unwrap().to_owned(),
                i["action"]["type"].as_str().unwrap().to_owned(),
                i["reclaimable_bytes"].as_u64().unwrap(),
            )
        })
        .collect();

    println!("plan items via CLI and MCP ({}):", via_cli.len());
    for i in &via_cli {
        println!("  {i:?}");
    }
    assert_eq!(via_cli.len(), ids.len(), "{doc:#}");
    assert_eq!(
        via_mcp, via_cli,
        "prepare_cleanup differs from clean --item"
    );
    let total: u64 = via_cli.iter().map(|i| i.4).sum();
    assert_eq!(prepared["reclaimable_bytes"].as_u64(), Some(total));
    assert!(via_cli.iter().all(|i| i.2 != "S3"));
    check(&fx);
}

fn classified(cands: &Value) -> BTreeSet<(String, String, String)> {
    cands
        .as_array()
        .unwrap_or_else(|| panic!("not a list: {cands:#}"))
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().to_owned(),
                c["tier"].as_str().unwrap().to_owned(),
                c["category"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// FX-OUT-03: one tree through the CLI binary (`scan --json`) and through the MCP server the
/// same binary serves (`scan_directory`, `get_cleanup_candidates`): identical candidate IDs,
/// tiers and categories, and the same reclaimable total.
#[test]
fn fx_out_03_cli_and_mcp_give_identical_classifications() {
    let with_git = git_available();
    let fx = tree(with_git);
    let cli = mcp_cli(&[fx.root()]);
    let r = s(fx.root());
    let doc = Schemas::default().json(&cli, &["scan", &r, "--no-history"], "scan");
    let via_cli = classified(&doc["data"]["candidates"]);
    assert!(via_cli.len() >= 5, "{via_cli:#?}");

    let mut m = Mcp::start(&cli);
    // The server compares paths with its canonical roots before touching the disk, so the
    // path is given as scull reports it (on Windows the temp dir is an 8.3 short path).
    let canonical_root = s(&scull_engine::paths::canon(fx.root()));
    let scan = m.tool(
        "scan_directory",
        json!({ "path": canonical_root, "max_results": 1000 }),
    );
    assert_eq!(scan["truncated"], false, "{scan:#}");
    let via_mcp = classified(&scan["candidates"]);
    let cands = m.tool("get_cleanup_candidates", json!({ "max_results": 1000 }));
    let via_mcp_candidates = classified(&cands["candidates"]);
    let exit = m.finish();
    assert_eq!(exit, Some(0));

    println!("CLI and MCP candidates ({}):", via_cli.len());
    for (id, tier, cat) in &via_cli {
        println!("  {id} {tier} {cat}");
    }
    assert_eq!(via_mcp, via_cli, "scan_directory differs from scan --json");
    // get_cleanup_candidates lists the actionable ones (never S3), classified the same way.
    assert!(!via_mcp_candidates.is_empty());
    assert!(
        via_mcp_candidates.is_subset(&via_cli),
        "{via_mcp_candidates:#?}"
    );
    assert!(via_mcp_candidates.iter().all(|(_, t, _)| t != "S3"));
    let actionable: BTreeSet<_> = via_cli
        .iter()
        .filter(|(_, t, _)| t != "S3")
        .cloned()
        .collect();
    assert_eq!(via_mcp_candidates, actionable);
    check(&fx);
}

/// Environment variable carrying the process ID the Ctrl-C sender targets.
#[cfg(windows)]
const CTRL_C_PID_ENV: &str = "SCULL_TEST_CTRL_C_PID";

/// Helper process for [`windows_ctrl_c_exits_130`] (run by it, not directly): delivers a
/// console Ctrl-C to the process in `SCULL_TEST_CTRL_C_PID`, giving up its own console.
#[cfg(windows)]
#[test]
#[ignore = "helper process for fx_out_02 (run by it, not directly)"]
fn fx_out_02_ctrl_c_sender() {
    let Some(pid) = std::env::var(CTRL_C_PID_ENV)
        .ok()
        .and_then(|p| p.parse().ok())
    else {
        return;
    };
    scull_platform::console::send_ctrl_c_to_console_of(pid).expect("send Ctrl-C");
}

/// FX-OUT-02 on Windows: `scull mcp serve`, after the MCP handshake, gets a real console
/// Ctrl-C and exits 130 promptly (CLI-05), with stdin still open. It never changed anything
/// (the caller checks the invariants).
#[cfg(windows)]
fn windows_ctrl_c_exits_130(fx: &Fixture) {
    use std::os::windows::process::CommandExt;
    use std::time::{Duration, Instant};

    let mcp = mcp_cli(&[fx.root()]);
    let mut command = mcp.command(&["mcp", "serve"]);
    command.creation_flags(scull_platform::console::CREATE_NEW_CONSOLE);
    let mut m = Mcp::start_command(command);
    let pid = m.child.id();
    let sender = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fx_out_02_ctrl_c_sender",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CTRL_C_PID_ENV, pid.to_string())
        .output()
        .unwrap();
    assert!(
        sender.status.success(),
        "Ctrl-C sender failed: {}\n{}",
        String::from_utf8_lossy(&sender.stdout),
        String::from_utf8_lossy(&sender.stderr)
    );
    // Prompt: well within a few seconds, while stdin is still open.
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = m.child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = m.child.kill();
            panic!("scull mcp serve did not exit within 20 s of a console Ctrl-C");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    drop(m.child.stdin.take());
    assert_eq!(
        status.code(),
        Some(ExitCode::Cancelled.as_i32()),
        "{status:?}"
    );
}
