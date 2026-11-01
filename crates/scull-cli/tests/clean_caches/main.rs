//! `scull clean caches` end to end (P2-CLI-02, QC-04): the real binary over a fake home
//! folder built with `scull-fixtures`, with fake package managers.
//!
//! **No real package manager ever runs.** This test executable doubles as fake `npm`, `pnpm`,
//! `bun` and `yarn` (the Quick Clean runner's fakes, `scull-cleaner/tests/quickclean/fake.rs`,
//! reused as is): installed into the fixture's temp folder, which is the only entry of the
//! `PATH` scull sees. A fake only touches a cache that carries its guard file and records how
//! it was started. Cache locations are passed explicitly (`npm_config_cache`,
//! `BUN_INSTALL_CACHE_DIR`, `npm_config_store_dir`) so the layout is the same on every OS.
//!
//! Whether a tool may run depends on what the machine lets scull observe: if any process of
//! the test user can't be inspected, "in use" is unknown, caches classify S2 and Quick Clean
//! skips every tool (D-28, QC-05). The run-path tests then check that every tool is reported
//! with its reason and nothing ran, and say so; where the probe sees everything they check
//! the exact commands, the owner-only sandbox, the prune/wipe effect and the results.
//!
//! The harness is custom (`harness = false`) because the executable must decide in `main`
//! whether it is a fake tool. Regenerate the schema with
//! `SCULL_BLESS=1 cargo test -p scull-cli --test clean_caches`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)]
#[path = "../../../scull-cleaner/tests/quickclean/fake.rs"]
mod fake;

use scull_fixtures::{Fixture, InvariantInputs};
use serde_json::Value;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

type Test = (&'static str, fn());

const TESTS: &[Test] = &[
    (
        "committed_schema_matches_generated",
        committed_schema_matches_generated,
    ),
    (
        "preview_lists_tools_commands_and_note",
        preview_lists_tools_commands_and_note,
    ),
    ("untick_leaves_a_tool_alone", untick_leaves_a_tool_alone),
    (
        "non_interactive_never_runs_without_confirm",
        non_interactive_never_runs_without_confirm,
    ),
    (
        "confirm_prunes_with_trusted_tools_in_private_sandbox",
        confirm_prunes_with_trusted_tools_in_private_sandbox,
    ),
    ("wipe_only_when_requested", wipe_only_when_requested),
    ("elevated_run_is_refused", elevated_run_is_refused),
    (
        "run_report_with_observable_activity",
        run_report_with_observable_activity,
    ),
    (
        "fx_qc_05_orphaned_caches_removed_only_per_confirmed_tool",
        fx_qc_05_orphaned_caches_removed_only_per_confirmed_tool,
    ),
    (
        "fx_qc_05_remove_orphaned_flag_through_the_binary",
        fx_qc_05_remove_orphaned_flag_through_the_binary,
    ),
];

fn main() {
    if let Some(code) = fake::dispatch() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        for (name, _) in TESTS {
            println!("{name}: test");
        }
        return;
    }
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let selected: Vec<&Test> = TESTS
        .iter()
        .filter(|(n, _)| filters.is_empty() || filters.iter().any(|f| n.contains(f.as_str())))
        .collect();
    let master = tempfile::Builder::new()
        .prefix("scull-cli-qc-fake-")
        .tempdir()
        .expect("tempdir");
    fake::prepare_master(master.path());
    println!("\nrunning {} tests", selected.len());
    let started = Instant::now();
    let mut failed = Vec::new();
    for (name, f) in &selected {
        let ok = panic::catch_unwind(AssertUnwindSafe(f)).is_ok();
        println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            failed.push(*name);
        }
    }
    drop(master);
    if !failed.is_empty() {
        println!("\nfailures:");
        for f in &failed {
            println!("    {f}");
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed; 0 ignored; 0 measured; {} filtered out; finished in {:.2}s\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        selected.len() - failed.len(),
        failed.len(),
        TESTS.len() - selected.len(),
        started.elapsed().as_secs_f64()
    );
    if !failed.is_empty() {
        std::process::exit(101);
    }
}

// ---------------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------------

/// Variables that would point package managers (and so scull's locator) elsewhere.
const CACHE_VARS: &[&str] = &[
    "npm_config_cache",
    "NPM_CONFIG_CACHE",
    "npm_config_userconfig",
    "npm_config_store_dir",
    "NPM_CONFIG_STORE_DIR",
    "PNPM_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "BUN_INSTALL",
    "BUN_INSTALL_CACHE_DIR",
    "YARN_CACHE_FOLDER",
    "YARN_GLOBAL_FOLDER",
];

const NPM: &str = "home/.npm";
const BUN: &str = "home/.bun/install/cache";
const PNPM: &str = "home/.local/share/pnpm/store";

struct Env {
    fx: Fixture,
}

impl Env {
    /// A fake home with an npm cache, a Bun cache and a pnpm store (pnpm not installed); fake
    /// `npm` and `bun` in the tools folder.
    fn new() -> Self {
        let mut fx = Fixture::new().unwrap();
        let c = format!("{NPM}/_cacache");
        fx.file(format!("{c}/index-v5/1a/2b/3c4d"), "{\"key\":\"x\"}\n")
            .unwrap()
            .file_of_size(format!("{c}/content-v2/sha512/ab/cd/keep01"), 4096)
            .unwrap()
            .file_of_size(format!("{c}/content-v2/sha512/ab/cd/unused01"), 64 * 1024)
            .unwrap()
            .file(format!("{c}/{}", fake::GUARD), "")
            .unwrap()
            .file(format!("{BUN}/left-pad@1.3.0@@@1/package.json"), "{}")
            .unwrap()
            .file(format!("{BUN}/a1b2c3.npm"), "manifest")
            .unwrap()
            .file(format!("{BUN}/{}", fake::GUARD), "")
            .unwrap()
            .file_of_size(format!("{PNPM}/v10/files/00/unused-abc"), 4096)
            .unwrap()
            .file(format!("{PNPM}/v10/index/00/abc-index.json"), "{}")
            .unwrap()
            .file(format!("{PNPM}/{}", fake::GUARD), "")
            .unwrap();
        fx.checkpoint().unwrap();
        let tools = fx.base().join("tools");
        fake::install(&tools, "npm");
        fake::install(&tools, "bun");
        Self { fx }
    }

    fn tools(&self) -> PathBuf {
        self.fx.base().join("tools")
    }

    fn scull_home(&self) -> PathBuf {
        self.fx.base().join("scull-home")
    }

    fn home(&self) -> PathBuf {
        self.fx.root().join("home")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(args)
            .current_dir(self.fx.base())
            .stdin(Stdio::null())
            .env("TERM", "dumb")
            .env("NO_COLOR", "1");
        for (k, _) in std::env::vars() {
            if k.starts_with("SCULL_") {
                c.env_remove(k);
            }
        }
        for v in CACHE_VARS {
            c.env_remove(v);
        }
        let home = self.home();
        c.env("SCULL_HOME", self.scull_home())
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("LOCALAPPDATA", home.join("AppData").join("Local"))
            .env("APPDATA", home.join("AppData").join("Roaming"))
            .env("PATH", self.tools())
            .env("npm_config_cache", self.fx.root().join(NPM))
            .env("BUN_INSTALL_CACHE_DIR", self.fx.root().join(BUN))
            .env("npm_config_store_dir", self.fx.root().join(PNPM));
        // P2-EXEC-13: nothing outside the fixture's temp dir.
        self.fx.sandbox(&mut c);
        c
    }

    /// Runs `args` (with `--allow-root` when this test runs elevated: [`allow_root_if_elevated`]).
    fn run(&self, args: &[&str]) -> Output {
        self.cmd(&allow_root_if_elevated(args)).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let mut all = vec!["clean", "caches", "--json", "--no-progress"];
        all.extend_from_slice(args);
        let o = self.run(&all);
        let v: Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|_| panic!("JSON on stdout; stderr: {}", stderr(&o)));
        validate(&v);
        (o.status.code().unwrap(), v)
    }

    fn check(&self, allowed: &[&str], spawned: &[String]) {
        let mut inputs = InvariantInputs::new();
        for a in allowed {
            inputs = inputs.allow(*a);
        }
        let inputs = inputs
            .spawned(spawned.iter().map(PathBuf::from))
            .trusted_dir(std::fs::canonicalize(self.tools()).unwrap());
        let report = self.fx.check_invariants(&inputs);
        println!("{report}");
        report.assert_ok();
    }
}

/// GitHub's Windows runners run the tests as an elevated administrator, where scull refuses
/// to change anything without `--allow-root` (SAFE-14). When this test process itself runs
/// elevated, `clean caches` gets `--allow-root`, as a user running elevated would have to
/// pass; everywhere else the arguments are unchanged. The refusal itself is tested with
/// `SCULL_TEST_ASSUME_ELEVATED` through [`Env::cmd`], which never adds the flag.
fn allow_root_if_elevated<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut all = args.to_vec();
    if args.first() == Some(&"clean") && needs_allow_root() {
        all.push("--allow-root");
    }
    all
}

/// Set by CI's standard-user run on Windows (`scripts/ci/run-as-standard-user.ps1`, #65): the
/// test process must then be non-elevated, so `--allow-root` is never added and the refusal
/// paths of a normal user run for real; an elevated process fails instead of quietly passing
/// the flag.
fn needs_allow_root() -> bool {
    let elevated = scull_platform::is_elevated().blocks_destructive();
    if std::env::var_os("SCULL_TEST_REQUIRE_NOT_ELEVATED").is_some_and(|v| !v.is_empty()) {
        assert!(
            !elevated,
            "SCULL_TEST_REQUIRE_NOT_ELEVATED is set but this process runs elevated"
        );
        static MARK: std::sync::Once = std::sync::Once::new();
        MARK.call_once(|| {
            eprintln!(
                "RUN {}: not elevated, no --allow-root (standard user)",
                env!("CARGO_CRATE_NAME")
            );
        });
    }
    elevated
}

/// `path` without Windows' verbatim prefix (`\\?\C:\...` → `C:\...`), so a canonicalized
/// path and one scull reports compare equal whichever spelling each side used (it depends on
/// whether the temp folder had 8.3 short names to expand, #65). Unchanged elsewhere.
fn plain(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/clean-caches.schema.json")
}

fn validate(v: &Value) {
    let schema: Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path()).expect("schema")).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

fn tool<'a>(v: &'a Value, list: &str, name: &str) -> Option<&'a Value> {
    v["data"][list]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tool"] == name)
}

/// Every one of npm, bun and pnpm is either planned or skipped with a reason.
fn every_tool_accounted_for(v: &Value) {
    for name in ["npm", "bun", "pnpm"] {
        let planned = tool(v, "tools", name).is_some();
        let skipped = tool(v, "skipped", name);
        assert!(
            planned != skipped.is_some(),
            "{name}: planned {planned}, skipped {skipped:?}"
        );
        if let Some(s) = skipped {
            assert!(!s["message"].as_str().unwrap().is_empty());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

fn committed_schema_matches_generated() {
    use scull_cli::cmd::clean_caches::{CLEAN_CACHES_SCHEMA, CleanCachesReport};
    let schema = scull_cli::output::json::document_schema::<CleanCachesReport>(CLEAN_CACHES_SCHEMA);
    let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(schema_path(), &text).unwrap();
    }
    let committed = std::fs::read_to_string(schema_path()).unwrap();
    assert_eq!(
        committed.replace("\r\n", "\n"),
        text,
        "schemas/clean-caches.schema.json is stale; rerun with SCULL_BLESS=1"
    );
}

/// QC-04: the dry run lists one line per tool with the exact command (resolved binary from
/// the trusted folder), the mode and the not-undoable note; pnpm (not installed) is reported
/// with its folder left for manual removal, or with the reason it isn't eligible. Nothing runs.
fn preview_lists_tools_commands_and_note() {
    let env = Env::new();
    let (code, v) = env.json(&[]);
    assert_eq!(code, 0);
    let d = &v["data"];
    assert_eq!(d["mode"], "preview");
    assert_eq!(
        d["not_undoable"],
        "Not undoable. Packages re-download on next install; offline installs may fail until then."
    );
    every_tool_accounted_for(&v);
    for t in d["tools"].as_array().unwrap() {
        let bin = plain(Path::new(t["binary"].as_str().unwrap()));
        assert!(
            bin.starts_with(plain(&std::fs::canonicalize(env.tools()).unwrap())),
            "{t:#}"
        );
        assert!(
            t["command_line"]
                .as_str()
                .unwrap()
                .contains(t["binary"].as_str().unwrap())
        );
    }
    if let Some(npm) = tool(&v, "tools", "npm") {
        assert_eq!(npm["mode"], "prune");
        assert_eq!(npm["args"][0], "cache");
        assert_eq!(npm["args"][1], "verify");
    }
    if let Some(bun) = tool(&v, "tools", "bun") {
        assert_eq!(bun["mode"], "wipe", "Bun has no prune");
        assert_eq!(bun["wipe_requested"], false);
    }
    let pnpm = tool(&v, "skipped", "pnpm").expect("pnpm can't run: not installed");
    assert!(
        matches!(
            pnpm["reason"].as_str().unwrap(),
            "not_installed" | "not_eligible" | "activity_unknown"
        ),
        "{pnpm:#}"
    );
    if pnpm["reason"] == "not_installed" {
        assert!(pnpm["manual_removal"].as_str().unwrap().ends_with("store"));
    }
    if d["tools"].as_array().unwrap().is_empty() {
        println!(
            "NOTE: no tool is runnable on this machine: {}",
            d["skipped"]
        );
    }

    let o = env.run(&["clean", "caches", "--no-progress"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(
        text.contains("Quick Clean: package caches (nothing has been run)"),
        "{text}"
    );
    assert!(text.contains("Not run"), "{text}");
    if !d["tools"].as_array().unwrap().is_empty() {
        assert!(text.contains("Not undoable."), "{text}");
        assert!(text.contains("scull clean caches --confirm"), "{text}");
    }
    assert!(!fake::ran_in(&env.tools()), "a preview runs nothing");
    env.check(&[], &[]);
}

/// FX-QC-08 through the CLI: `--untick npm` leaves npm alone.
fn untick_leaves_a_tool_alone() {
    let env = Env::new();
    let (code, v) = env.json(&["--untick", "npm", "--confirm"]);
    assert!(matches!(code, 0 | 3), "{v:#}");
    let npm = tool(&v, "skipped", "npm").expect("npm skipped");
    assert_eq!(npm["reason"], "unticked");
    assert!(tool(&v, "results", "npm").is_none());
    assert!(fake::calls(&env.tools(), "npm").is_empty(), "npm never ran");
    let spawned: Vec<String> = v["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["binary"].as_str().unwrap().to_owned())
        .collect();
    let allowed: &[&str] = if spawned.is_empty() { &[] } else { &[BUN] };
    env.check(allowed, &spawned);
}

/// CLI-03: without a terminal and without `--confirm`, nothing runs.
fn non_interactive_never_runs_without_confirm() {
    let env = Env::new();
    let o = env.run(&["clean", "caches", "--no-progress"]);
    assert_eq!(o.status.code(), Some(0));
    let o = env.run(&["clean", "caches", "--wipe", "--no-progress"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(!fake::ran_in(&env.tools()));
    env.check(&[], &[]);
}

/// QC-04/QC-06: `--confirm` runs each planned tool from the trusted folder with the exact
/// prune command, in a private sandbox under scull's state folder (not a shared /tmp), and
/// reports measured results; unused entries go, kept entries stay. Where no tool is runnable,
/// every tool is reported and nothing runs.
fn confirm_prunes_with_trusted_tools_in_private_sandbox() {
    let env = Env::new();
    let (code, v) = env.json(&["--confirm"]);
    let d = &v["data"];
    every_tool_accounted_for_after_run(&v);
    let results = d["results"].as_array().unwrap();
    if results.is_empty() {
        println!(
            "NOTE: run path not exercised on this machine: {}",
            d["skipped"]
        );
        assert_eq!(code, 0, "{v:#}");
        assert!(!fake::ran_in(&env.tools()));
        env.check(&[], &[]);
        return;
    }
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(d["mode"], "run");
    let sandbox_base = std::fs::canonicalize(env.scull_home().join("state").join("quickclean"))
        .expect("sandbox base exists");
    assert!(scull_platform::ensure_owner_only(&sandbox_base).is_ok());
    let mut spawned = Vec::new();
    let mut allowed = Vec::new();
    for r in results {
        assert_eq!(r["succeeded"], true, "{r:#}");
        spawned.push(r["binary"].as_str().unwrap().to_owned());
        assert!(r["free_space_delta"].is_i64() || r["free_space_delta"].is_null());
        assert!(r["cache_bytes_before"].is_u64());
    }
    if tool(&v, "results", "npm").is_some() {
        let calls = fake::calls(&env.tools(), "npm");
        assert_eq!(calls.len(), 1);
        let c = &calls[0];
        assert_eq!(c.argv[..2], ["cache".to_owned(), "verify".to_owned()]);
        assert_eq!(c.argv[2], "--cache");
        assert!(
            plain(&c.cwd).starts_with(plain(&sandbox_base)),
            "sandbox under scull's state: {:?}",
            c.cwd
        );
        let cache = env
            .fx
            .root()
            .join(NPM)
            .join("_cacache/content-v2/sha512/ab/cd");
        assert!(
            !cache.join("unused01").exists(),
            "prune removed unused entries"
        );
        assert!(cache.join("keep01").exists(), "prune kept the rest");
        allowed.push(NPM);
    }
    if tool(&v, "results", "bun").is_some() {
        allowed.push(BUN);
    }
    env.check(&allowed, &spawned);
}

fn every_tool_accounted_for_after_run(v: &Value) {
    for name in ["npm", "bun", "pnpm"] {
        let ran = tool(v, "results", name).is_some();
        let skipped = tool(v, "skipped", name).is_some();
        assert!(
            ran != skipped,
            "{name}: ran {ran}, skipped {skipped}: {v:#}"
        );
    }
}

/// QC-03: the wipe runs only with `--wipe` (npm: `cache clean --force`).
fn wipe_only_when_requested() {
    let env = Env::new();
    let (_, v) = env.json(&["--wipe"]);
    if let Some(npm) = tool(&v, "tools", "npm") {
        assert_eq!(npm["mode"], "wipe");
        assert_eq!(npm["wipe_requested"], true);
        assert_eq!(npm["args"][1], "clean");
    }
    let (code, v) = env.json(&["--confirm", "--wipe"]);
    let results = v["data"]["results"].as_array().unwrap();
    let spawned: Vec<String> = results
        .iter()
        .map(|r| r["binary"].as_str().unwrap().to_owned())
        .collect();
    let mut allowed = Vec::new();
    if tool(&v, "results", "npm").is_some() {
        assert_eq!(code, 0, "{v:#}");
        let c = &fake::calls(&env.tools(), "npm")[0];
        assert_eq!(
            c.argv[..3],
            ["cache".to_owned(), "clean".to_owned(), "--force".to_owned()]
        );
        allowed.push(NPM);
    } else {
        println!("NOTE: run path not exercised on this machine");
    }
    if tool(&v, "results", "bun").is_some() {
        allowed.push(BUN);
    }
    env.check(&allowed, &spawned);
}

/// SAFE-14: run as root/administrator (simulated), nothing runs without `--allow-root`
/// (exit 5); the preview still works.
fn elevated_run_is_refused() {
    let env = Env::new();
    let o = env
        .cmd(&["clean", "caches", "--confirm", "--json"])
        .env("SCULL_TEST_ASSUME_ELEVATED", "1")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    let e: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!(e["error"]["kind"], "safety_blocked");
    let o = env
        .cmd(&["clean", "caches", "--json"])
        .env("SCULL_TEST_ASSUME_ELEVATED", "1")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!fake::ran_in(&env.tools()));
    env.check(&[], &[]);
}

/// An activity probe that sees every process and finds nothing (stands in for a machine where
/// scull can inspect all of the user's processes). Test-only: the binary never has one.
struct Idle;

impl scull_safety::ActivityProbe for Idle {
    fn process_cwds_under(&self, _: &Path) -> scull_safety::ProbeResult {
        scull_safety::ProbeResult::not_in_use()
    }
    fn open_files_under(&self, _: &Path) -> scull_safety::ProbeResult {
        scull_safety::ProbeResult::not_in_use()
    }
    fn files_held_open(&self, _: &[PathBuf]) -> scull_safety::ProbeResult {
        scull_safety::ProbeResult::not_in_use()
    }
}

/// The run path of the command's report (the part the CLI adds around the Quick Clean runner),
/// in process, with fully observable activity: the same caches classified by the engine with
/// an idle in-use probe, planned and run with the fake tools. npm is pruned (unused entries
/// go), Bun wiped (its only cleanup), pnpm (not installed) left for manual removal; the report
/// carries measured sizes and validates against the schema; nothing else changes.
fn run_report_with_observable_activity() {
    use scull_cleaner::quickclean::{QuickClean, QuickCleanOptions, QuickCleanRequest};
    use scull_cli::cmd::clean_caches::{apply_run, preview, render};
    use scull_detectors::rules::Platform as Os;
    use scull_detectors::{CacheEnv, DetectContext, locate_caches};
    use scull_engine::{Engine, EngineConfig, FixedInUse, FsConfigReader, InUseSource};
    use std::sync::Arc;

    let mut env = Env::new();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
    env.fx.age("home", old).unwrap();
    env.fx.checkpoint().unwrap();
    let root = env.fx.root().to_path_buf();
    let vars: Vec<(&str, PathBuf)> = vec![
        ("npm_config_cache", root.join(NPM)),
        ("BUN_INSTALL_CACHE_DIR", root.join(BUN)),
        ("npm_config_store_dir", root.join(PNPM)),
    ];
    let cenv = CacheEnv::new(env.home(), Os::current().unwrap(), move |k| {
        vars.iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    });
    let mut cfg = EngineConfig::new(
        scull_core::config::Config::default(),
        Arc::new(scull_platform::Native),
    );
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(scull_safety::Tri::No)));
    cfg.allow_network_roots = true;
    let engine = Engine::new(cfg).unwrap();
    let caches = engine.caches(&cenv, 1, &scull_scanner::CancelToken::new(), |_| {});
    let reader = FsConfigReader;
    let located = locate_caches(
        &cenv,
        &scull_platform::Native,
        &DetectContext::new().with_config_reader(&reader),
    );

    let sandbox = env.fx.base().join("sandbox");
    scull_platform::create_dir_owner_only(&sandbox).unwrap();
    let mut opts = QuickCleanOptions::new(env.tools().into_os_string());
    opts.sandbox_base = Some(sandbox.clone());
    let qc = QuickClean::new(opts, Arc::new(scull_platform::Native))
        .with_elevation(scull_platform::Elevation::NotElevated);
    let plan = qc.plan(
        &located,
        &caches.candidates,
        &QuickCleanRequest::default(),
        &Idle,
    );
    let mut report = preview(&plan, Vec::new());
    let names: Vec<&str> = report.tools.iter().map(|t| t.tool.as_str()).collect();
    assert_eq!(names, ["npm", "bun"], "{report:#?}");
    let pnpm = report.skipped.iter().find(|s| s.tool == "pnpm").unwrap();
    assert_eq!(pnpm.reason, "not_installed", "{pnpm:#?}");
    assert!(pnpm.manual_removal.is_some());
    let text = render(&report, false);
    assert!(text.contains("Commands (no shell"), "{text}");
    assert!(text.contains("Not undoable."), "{text}");
    println!("{text}");

    // Run as one audited operation (SAFE-08, P2-EXEC-12), as `scull clean caches` does; the
    // store and the lock live beside the fixture root.
    let now = scull_engine::now_unix();
    let audit_plan = plan
        .audit_plan(
            &std::collections::BTreeSet::new(),
            scull_core::PlanParams {
                id: scull_core::PlanId::from_suffix("QC1").unwrap(),
                scan_id: scull_core::ScanId::from_suffix("QC1").unwrap(),
                created_unix: now,
                ttl_secs: None,
                machine_id: "qc-machine".into(),
                user: "tester".into(),
                origin: scull_core::PlanOrigin::Cli,
                operations_watermark: None,
            },
        )
        .unwrap();
    let mut store = scull_storage::Store::open(
        &env.fx.base().join("data").join("scull.db"),
        &scull_engine::PlatformOwnerOnly,
    )
    .unwrap();
    store.insert_plan(&audit_plan).unwrap();
    let executor = scull_cleaner::Executor::acquire(&scull_cleaner::LockOptions::at(
        env.fx.base().join("state").join("execution.lock"),
    ))
    .unwrap();
    let op_id = scull_core::OperationId::from_suffix("QC1").unwrap();
    let mut op = executor
        .begin_operation(
            &mut store,
            &audit_plan,
            &scull_storage::OperationStart {
                operation_id: op_id.clone(),
                plan_id: audit_plan.id().clone(),
                content_hash: audit_plan.content_hash().clone(),
                approver: "tester".into(),
                method: scull_storage::ApprovalMethod::CliInteractive,
                started_at: now,
            },
        )
        .unwrap();
    let ran = qc
        .run(&mut op, &plan, &Idle, &scull_scanner::CancelToken::new())
        .unwrap();
    op.finish(scull_storage::OperationEnd::Normal, now + 1)
        .unwrap();
    assert!(store.verify_chain(&op_id).unwrap().is_intact());
    assert_eq!(store.audit_items(&op_id).unwrap().len(), 2);
    let failed = apply_run(&mut report, &plan, &ran);
    assert_eq!(failed, 0, "{report:#?}");
    assert_eq!(report.results.len(), 2);
    for r in &report.results {
        assert!(r.succeeded, "{r:#?}");
        assert!(r.cache_bytes_before.is_some() && r.cache_bytes_after.is_some());
    }
    let npm = fake::calls(&env.tools(), "npm");
    assert_eq!(npm.len(), 1);
    // The tool ran inside the sandbox, compared canonically (the tool's private working folder
    // is gone by now; on Windows the sandbox may be spelled with an 8.3 short name).
    let plain = scull_engine::paths::canon;
    assert!(
        plain(&npm[0].cwd).starts_with(plain(&sandbox)),
        "{:?} outside {sandbox:?}",
        npm[0].cwd
    );
    let content = root.join(NPM).join("_cacache/content-v2/sha512/ab/cd");
    assert!(!content.join("unused01").exists() && content.join("keep01").exists());
    assert!(
        !root.join(BUN).join("a1b2c3.npm").exists(),
        "Bun's only cleanup is a wipe"
    );

    let mut doc = Vec::new();
    scull_cli::output::json::write_document(
        &mut doc,
        scull_cli::cmd::clean_caches::CLEAN_CACHES_SCHEMA,
        &report,
    )
    .unwrap();
    let v: Value = serde_json::from_slice(&doc).unwrap();
    validate(&v);
    assert_eq!(v["data"]["mode"], "run");
    let text = render(&report, false);
    assert!(text.contains(" ok "), "{text}");
    println!("{text}");
    let spawned: Vec<String> = report
        .results
        .iter()
        .map(|r| r.binary.as_str().to_owned())
        .collect();
    env.check(&[NPM, BUN], &spawned);
}

/// The same caches classified with an idle in-use probe, planned against a tools folder that
/// holds only npm: the pnpm store and the Bun cache are orphaned (their tools aren't
/// installed). For the D-36 test.
fn orphaned_setup(
    env: &mut Env,
) -> (
    scull_cleaner::quickclean::QuickClean,
    scull_cleaner::quickclean::QuickCleanPlan,
) {
    use scull_cleaner::quickclean::{QuickClean, QuickCleanOptions, QuickCleanRequest};
    use scull_detectors::rules::Platform as Os;
    use scull_detectors::{CacheEnv, DetectContext, locate_caches};
    use scull_engine::{Engine, EngineConfig, FixedInUse, FsConfigReader, InUseSource};
    use std::sync::Arc;

    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
    env.fx.age("home", old).unwrap();
    env.fx.checkpoint().unwrap();
    let root = env.fx.root().to_path_buf();
    let vars: Vec<(&str, PathBuf)> = vec![
        ("npm_config_cache", root.join(NPM)),
        ("BUN_INSTALL_CACHE_DIR", root.join(BUN)),
        ("npm_config_store_dir", root.join(PNPM)),
    ];
    let cenv = CacheEnv::new(env.home(), Os::current().unwrap(), move |k| {
        vars.iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string_lossy().into_owned())
    });
    let mut cfg = EngineConfig::new(
        scull_core::config::Config::default(),
        Arc::new(scull_platform::Native),
    );
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(scull_safety::Tri::No)));
    cfg.allow_network_roots = true;
    let engine = Engine::new(cfg).unwrap();
    let caches = engine.caches(&cenv, 1, &scull_scanner::CancelToken::new(), |_| {});
    let reader = FsConfigReader;
    let located = locate_caches(
        &cenv,
        &scull_platform::Native,
        &DetectContext::new().with_config_reader(&reader),
    );
    let npm_only = env.fx.base().join("tools-npm-only");
    fake::install(&npm_only, "npm");
    let sandbox = env.fx.base().join("sandbox");
    scull_platform::create_dir_owner_only(&sandbox).unwrap();
    let mut opts = QuickCleanOptions::new(npm_only.into_os_string());
    opts.sandbox_base = Some(sandbox);
    let qc = QuickClean::new(opts, Arc::new(scull_platform::Native))
        .with_elevation(scull_platform::Elevation::NotElevated);
    let plan = qc.plan(
        &located,
        &caches.candidates,
        &QuickCleanRequest::default(),
        &Idle,
    );
    (qc, plan)
}

/// D-36 through the command's report, in process with fully observable activity: pnpm and
/// Bun are not installed, so their caches are listed as orphaned with the not-undoable note
/// and nothing is removed without a confirmation. The terminal asks once per tool (`n`, any
/// other answer or end of input declines). Confirming pnpm only removes the pnpm store through
/// the Executor (under the execution lock) and leaves the Bun cache and npm's cache untouched;
/// the result carries the freed bytes and the measured free-space delta (I-5) and validates
/// against the schema.
fn fx_qc_05_orphaned_caches_removed_only_per_confirmed_tool() {
    use scull_cli::cmd::clean_caches::{
        apply_orphaned, ask_orphaned, preview, render, with_orphaned,
    };
    use scull_detectors::caches::CacheTool;
    use std::collections::BTreeSet;

    let mut env = Env::new();
    let (qc, plan) = orphaned_setup(&mut env);
    let mut orphaned: Vec<CacheTool> = plan.orphaned.iter().map(|o| o.tool).collect();
    orphaned.sort();
    let mut want = vec![CacheTool::Pnpm, CacheTool::Bun];
    want.sort();
    assert_eq!(orphaned, want, "{plan:#?}");
    assert!(plan.tools.iter().all(|t| t.tool == CacheTool::Npm));

    // Preview with pnpm named (--remove-orphaned pnpm): both listed, only pnpm confirmed.
    let named: BTreeSet<CacheTool> = [CacheTool::Pnpm].into();
    let mut report = preview(&plan, Vec::new());
    with_orphaned(&mut report, &plan, &named);
    assert!(report.orphaned_unmatched.is_empty());
    assert_eq!(report.orphaned.len(), 2);
    for o in &report.orphaned {
        assert_eq!(o.selected, o.tool == "pnpm", "{o:#?}");
        assert_eq!(
            o.not_undoable,
            "Not undoable. Packages re-download on next install; offline installs may fail until then."
        );
        assert!(o.reclaimable_bytes > 0, "{o:#?}");
        let s = report.skipped.iter().find(|s| s.tool == o.tool).unwrap();
        assert_eq!(s.reason, "not_installed");
        assert!(s.message.contains("--remove-orphaned"), "{s:#?}");
    }
    let text = render(&report, false);
    assert!(
        text.contains("Orphaned caches (tool not installed"),
        "{text}"
    );
    assert!(text.contains("yes (--remove-orphaned)"), "{text}");
    assert!(text.contains("no: add --remove-orphaned bun"), "{text}");
    assert!(
        text.contains("Removing an orphaned cache deletes the folder. Not undoable."),
        "{text}"
    );
    println!("{text}");

    // The terminal: one prompt per tool not already named; only `y` confirms.
    let mut err = Vec::new();
    let asked = ask_orphaned(&plan, &named, &mut "n\n".as_bytes(), &mut err).unwrap();
    assert!(asked.is_empty());
    let prompts = String::from_utf8(err).unwrap();
    assert_eq!(prompts.matches("[y/N]").count(), 1, "{prompts}");
    assert!(prompts.contains("bun is not installed"), "{prompts}");
    assert!(prompts.contains("Not undoable."), "{prompts}");
    let first = plan.orphaned[0].tool;
    let asked = ask_orphaned(
        &plan,
        &BTreeSet::new(),
        &mut "y\nno\n".as_bytes(),
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(asked, [first].into());
    let asked = ask_orphaned(&plan, &BTreeSet::new(), &mut "".as_bytes(), &mut Vec::new()).unwrap();
    assert!(asked.is_empty(), "end of input declines");

    // As `scull clean caches` does: only what was confirmed (here the pnpm orphan, no tool
    // run) is recorded as the plan of one audited operation (SAFE-08).
    let mut approved = plan.clone();
    approved.tools.clear();
    approved.orphaned.retain(|o| named.contains(&o.tool));
    let now = scull_engine::now_unix();
    let audit_plan = approved
        .audit_plan(
            &named,
            scull_core::PlanParams {
                id: scull_core::PlanId::from_suffix("D36").unwrap(),
                scan_id: scull_core::ScanId::from_suffix("D36").unwrap(),
                created_unix: now,
                ttl_secs: None,
                machine_id: "qc-machine".into(),
                user: "tester".into(),
                origin: scull_core::PlanOrigin::Cli,
                operations_watermark: None,
            },
        )
        .unwrap();
    assert_eq!(audit_plan.items().len(), 1, "only the confirmed orphan");
    let mut store = scull_storage::Store::open(
        &env.fx.base().join("data").join("scull.db"),
        &scull_engine::PlatformOwnerOnly,
    )
    .unwrap();
    store.insert_plan(&audit_plan).unwrap();
    let exec = scull_cleaner::Executor::acquire(&scull_cleaner::LockOptions::at(
        env.fx.base().join("state").join("execution.lock"),
    ))
    .unwrap();
    let op_id = scull_core::OperationId::from_suffix("D36").unwrap();
    let mut op = exec
        .begin_operation(
            &mut store,
            &audit_plan,
            &scull_storage::OperationStart {
                operation_id: op_id.clone(),
                plan_id: audit_plan.id().clone(),
                content_hash: audit_plan.content_hash().clone(),
                approver: "tester".into(),
                method: scull_storage::ApprovalMethod::CliFlag,
                started_at: now,
            },
        )
        .unwrap();
    let cancel = scull_scanner::CancelToken::new();
    // Nothing confirmed: nothing removed.
    let none = qc
        .remove_orphaned(&exec, &mut op, &approved, &BTreeSet::new(), &Idle, &cancel)
        .unwrap();
    assert!(none.removed.is_empty() && none.skipped.is_empty());
    let root = env.fx.root().to_path_buf();
    assert!(root.join(PNPM).exists() && root.join(BUN).exists());

    let removed = qc
        .remove_orphaned(&exec, &mut op, &approved, &named, &Idle, &cancel)
        .unwrap();
    op.finish(scull_storage::OperationEnd::Normal, now + 1)
        .unwrap();
    drop(exec);
    assert!(store.verify_chain(&op_id).unwrap().is_intact());
    assert_eq!(store.audit_items(&op_id).unwrap().len(), 1);
    let failed = apply_orphaned(&mut report, &removed);
    assert_eq!(failed, 0, "{report:#?}");
    assert_eq!(report.orphaned_results.len(), 1);
    let r = report.orphaned_results[0].clone();
    assert_eq!((r.tool.as_str(), r.outcome.as_str()), ("pnpm", "removed"));
    assert!(r.succeeded && r.reason.is_none(), "{r:#?}");
    assert!(r.freed_bytes.unwrap() > 0 && r.entries_removed.unwrap() > 0);
    assert_eq!(r.entries_kept, Some(0));
    assert!(r.cache_bytes_before.unwrap() > 0);
    let note = |t: &str| {
        report
            .skipped
            .iter()
            .find(|s| s.tool == t)
            .unwrap()
            .message
            .clone()
    };
    assert!(note("pnpm").contains("handled by scull"), "{report:#?}");
    assert!(note("bun").contains("--remove-orphaned bun"), "{report:#?}");
    assert!(!root.join(PNPM).exists(), "the pnpm store is gone");
    assert!(
        root.join(BUN).join("a1b2c3.npm").exists(),
        "Bun was not confirmed: untouched"
    );
    assert!(
        root.join(NPM)
            .join("_cacache/content-v2/sha512/ab/cd/unused01")
            .exists(),
        "npm's cache untouched"
    );

    let mut doc = Vec::new();
    scull_cli::output::json::write_document(
        &mut doc,
        scull_cli::cmd::clean_caches::CLEAN_CACHES_SCHEMA,
        &report,
    )
    .unwrap();
    let v: Value = serde_json::from_slice(&doc).unwrap();
    validate(&v);
    assert_eq!(v["data"]["mode"], "run");
    assert_eq!(v["data"]["orphaned_results"][0]["outcome"], "removed");
    let text = render(&report, false);
    assert!(text.contains("Orphaned caches:"), "{text}");
    assert!(text.contains("removed "), "{text}");
    println!("{text}");

    // I-4: the audited removal matches the disk; I-5: the operation records the predicted
    // bytes and the measured free-space delta.
    let inputs = InvariantInputs::new()
        .allow(PNPM)
        .audit(&store, &op_id)
        .operation_bytes(&store, &op_id)
        .spawned(plan.spawned.iter().map(|s| s.binary.clone()))
        .trusted_dir(std::fs::canonicalize(env.fx.base().join("tools-npm-only")).unwrap());
    let invariants = env.fx.check_invariants(&inputs);
    println!("{invariants}");
    invariants.assert_ok();
}

/// `--remove-orphaned TOOL` through the binary. The preview marks the named orphaned cache as
/// confirmed and removes nothing; a named tool without a removable orphaned cache (npm, which
/// is installed; or pnpm where "in use" can't be observed and the store is S2) is listed in
/// `orphaned_unmatched` and, with `--confirm`, makes the run exit 3 with nothing removed.
/// Where the pnpm store qualifies, `--confirm --remove-orphaned pnpm` removes exactly it.
fn fx_qc_05_remove_orphaned_flag_through_the_binary() {
    let env = Env::new();
    let (code, v) = env.json(&["--remove-orphaned", "pnpm"]);
    assert_eq!(code, 0, "{v:#}");
    let d = &v["data"];
    assert_eq!(d["mode"], "preview");
    let listed = d["orphaned"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["tool"] == "pnpm")
        .cloned();
    match &listed {
        Some(o) => {
            assert_eq!(o["selected"], true);
            assert!(
                d["next_step"]
                    .as_str()
                    .unwrap()
                    .contains("--confirm --remove-orphaned pnpm"),
                "{v:#}"
            );
        }
        None => {
            assert_eq!(
                d["orphaned_unmatched"],
                serde_json::json!(["pnpm"]),
                "{v:#}"
            );
            // The removal half below only runs where activity is observable: a failure in CI.
            scull_fixtures::skip::skip(
                "fx_qc_05_remove_orphaned_flag_through_the_binary",
                &format!(
                    "the pnpm store is not removable here (activity not observable): {}",
                    tool(&v, "skipped", "pnpm").unwrap()
                ),
            );
        }
    }
    assert!(
        env.fx.root().join(PNPM).exists(),
        "a preview removes nothing"
    );

    // npm is installed: never an orphaned cache, whatever the machine observes.
    let (code, v) = env.json(&[
        "--confirm",
        "--untick",
        "npm",
        "--untick",
        "bun",
        "--remove-orphaned",
        "npm",
    ]);
    assert_eq!(code, 3, "{v:#}");
    assert_eq!(v["data"]["orphaned_unmatched"], serde_json::json!(["npm"]));
    assert!(v["data"]["orphaned_results"].as_array().unwrap().is_empty());
    assert!(env.fx.root().join(PNPM).exists());

    let (code, v) = env.json(&[
        "--confirm",
        "--untick",
        "npm",
        "--untick",
        "bun",
        "--remove-orphaned",
        "pnpm",
    ]);
    let allowed: &[&str] = if listed.is_some() {
        assert_eq!(code, 0, "{v:#}");
        assert_eq!(v["data"]["mode"], "run");
        let r = &v["data"]["orphaned_results"][0];
        assert_eq!(r["tool"], "pnpm");
        assert_eq!(r["outcome"], "removed", "{v:#}");
        assert!(!env.fx.root().join(PNPM).exists());
        &[PNPM]
    } else {
        assert_eq!(code, 3, "{v:#}");
        assert_eq!(v["data"]["orphaned_unmatched"], serde_json::json!(["pnpm"]));
        assert!(env.fx.root().join(PNPM).exists());
        &[]
    };
    assert!(!fake::ran_in(&env.tools()), "no tool ran");
    env.check(allowed, &[]);
}
