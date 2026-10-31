//! Quick Clean runner fixtures (P2-QC-01): FX-QC-01…08 plus the SAFE-12 process contract
//! (exact argv, environment and working directory; timeout; output escaping), QC-05 skips and
//! D-28.
//!
//! **No real package manager ever runs.** This test executable doubles as fake `npm`, `pnpm`,
//! `bun` and `yarn` binaries (see `fake.rs`), installed into each fixture's temp folder, and
//! Quick Clean's trusted search list points only at temp folders. A fake records how it was
//! started and only touches a fake cache inside the fixture that carries a guard file.
//!
//! Every test builds its fake home folder with `scull-fixtures` (canary beside the root),
//! locates caches with `scull_detectors::locate_caches` exactly as the engine does, and checks
//! the invariants afterwards: I-1 on the planned candidates, I-2 canary untouched, I-3 nothing
//! in the root changed except the caches the plan cleaned, and I-7 (only allowlisted tools,
//! spawned from the trusted folder) through the report's spawn records.
//!
//! The harness is custom (`harness = false`) because the executable must decide in `main`
//! whether it is a fake tool; tests run sequentially and print libtest-style lines.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fake;

use scull_cleaner::quickclean::{
    CleanMode, Ended, NOT_UNDOABLE_NOTE, QuickClean, QuickCleanOptions, QuickCleanPlan,
    QuickCleanReport, QuickCleanRequest, SkipReason, SkippedTool, Spawn, needs_escape,
};
use scull_core::{Candidate, Tier};
use scull_detectors::caches::{CacheProblem, CacheScan, CacheTool};
use scull_detectors::rules::Platform as Os;
use scull_detectors::{CacheEnv, CallerSignals, ConfigReader, DetectContext, locate_caches};
use scull_fixtures::{Fixture, InvariantId, InvariantInputs, PlanEntry};
use scull_platform::{Capabilities, DirIter, Elevation, Metadata, Native, Platform};
use scull_safety::{ActivityProbe, GitTracking, Holder, ProbeResult, Tri, Unavailable};
use scull_scanner::CancelToken;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

type Test = (&'static str, fn());

const TESTS: &[Test] = &[
    (
        "fx_qc_01_npm_prune_exact_argv_env_cwd_and_measured",
        fx_qc_01_npm_prune_exact_argv_env_cwd_and_measured,
    ),
    (
        "fx_qc_01_npm_wipe_only_when_requested",
        fx_qc_01_npm_wipe_only_when_requested,
    ),
    (
        "fx_qc_02_pnpm_store_prune_only_with_partial_reclaim_explanation",
        fx_qc_02_pnpm_store_prune_only_with_partial_reclaim_explanation,
    ),
    (
        "fx_qc_03_tool_in_use_is_skipped_and_reported",
        fx_qc_03_tool_in_use_is_skipped_and_reported,
    ),
    (
        "fx_qc_03_in_use_at_run_time_is_skipped",
        fx_qc_03_in_use_at_run_time_is_skipped,
    ),
    (
        "fx_qc_03_real_process_inside_cache_is_detected",
        fx_qc_03_real_process_inside_cache_is_detected,
    ),
    (
        "fx_qc_04_symlinked_cache_is_skipped",
        fx_qc_04_symlinked_cache_is_skipped,
    ),
    (
        "fx_qc_04_cache_swapped_for_symlink_after_planning_is_skipped",
        fx_qc_04_cache_swapped_for_symlink_after_planning_is_skipped,
    ),
    (
        "qc_05_cache_on_other_volume_is_skipped",
        qc_05_cache_on_other_volume_is_skipped,
    ),
    (
        "fx_qc_05_uninstalled_tool_cache_is_reported_not_removed",
        fx_qc_05_uninstalled_tool_cache_is_reported_not_removed,
    ),
    (
        "fx_qc_06_untrusted_npm_binaries_never_run",
        fx_qc_06_untrusted_npm_binaries_never_run,
    ),
    (
        "fx_qc_06_only_untrusted_binaries_means_not_installed",
        fx_qc_06_only_untrusted_binaries_means_not_installed,
    ),
    (
        "fx_qc_06_trusted_symlink_into_node_modules_is_rejected",
        fx_qc_06_trusted_symlink_into_node_modules_is_rejected,
    ),
    (
        "fx_qc_07_project_npmrc_never_applies",
        fx_qc_07_project_npmrc_never_applies,
    ),
    (
        "fx_qc_07_user_npmrc_cache_at_home_is_refused",
        fx_qc_07_user_npmrc_cache_at_home_is_refused,
    ),
    (
        "fx_qc_08_unticked_tools_are_not_cleaned",
        fx_qc_08_unticked_tools_are_not_cleaned,
    ),
    (
        "bun_and_yarn_location_variables_target_the_located_cache",
        bun_and_yarn_location_variables_target_the_located_cache,
    ),
    (
        "yarn_version_must_match_the_cache",
        yarn_version_must_match_the_cache,
    ),
    ("timeout_kills_the_tool", timeout_kills_the_tool),
    ("output_is_escaped_and_capped", output_is_escaped_and_capped),
    (
        "failing_tool_reports_its_exit_status",
        failing_tool_reports_its_exit_status,
    ),
    (
        "d28_s2_cache_gets_no_tool_action",
        d28_s2_cache_gets_no_tool_action,
    ),
    (
        "elevated_process_refuses_unless_allowed",
        elevated_process_refuses_unless_allowed,
    ),
    (
        "cancelled_before_start_runs_nothing",
        cancelled_before_start_runs_nothing,
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
    let exact = args.iter().any(|a| a == "--exact");
    let selected: Vec<&Test> = TESTS
        .iter()
        .filter(|(n, _)| {
            filters.is_empty()
                || filters.iter().any(|f| {
                    if exact {
                        *n == f.as_str()
                    } else {
                        n.contains(f.as_str())
                    }
                })
        })
        .collect();

    let master = tempfile::Builder::new()
        .prefix("scull-qc-fake-")
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
    let filtered = TESTS.len() - selected.len();
    if !failed.is_empty() {
        println!("\nfailures:");
        for f in &failed {
            println!("    {f}");
        }
    }
    println!(
        "\ntest result: {}. {} passed; {} failed; 0 ignored; 0 measured; {filtered} filtered out; finished in {:.2}s\n",
        if failed.is_empty() { "ok" } else { "FAILED" },
        selected.len() - failed.len(),
        failed.len(),
        started.elapsed().as_secs_f64()
    );
    if !failed.is_empty() {
        std::process::exit(101);
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Reads small regular files with `std::fs` (test-only config reader).
struct StdReader;

impl ConfigReader for StdReader {
    fn read_text(&self, path: &Path, max_bytes: u64) -> Option<String> {
        let md = fs::symlink_metadata(path).ok()?;
        if !md.is_file() || md.len() > max_bytes {
            return None;
        }
        let mut s = String::new();
        fs::File::open(path)
            .ok()?
            .take(max_bytes)
            .read_to_string(&mut s)
            .ok()?;
        Some(s)
    }
}

/// A fake activity probe: everything at or below `held` is held open by a fake `npm`;
/// `unknown` makes every answer unknown.
#[derive(Default)]
struct Probe {
    held: Vec<PathBuf>,
    unknown: bool,
}

impl Probe {
    fn idle() -> Self {
        Self::default()
    }

    fn holding(p: PathBuf) -> Self {
        Self {
            held: vec![p],
            unknown: false,
        }
    }
}

impl ActivityProbe for Probe {
    fn process_cwds_under(&self, _dir: &Path) -> ProbeResult {
        if self.unknown {
            return ProbeResult::Unknown(Unavailable::Truncated);
        }
        ProbeResult::not_in_use()
    }

    fn open_files_under(&self, dir: &Path) -> ProbeResult {
        if self.unknown {
            return ProbeResult::Unknown(Unavailable::Truncated);
        }
        let holders: Vec<Holder> = self
            .held
            .iter()
            .filter(|h| h.starts_with(dir))
            .map(|h| Holder {
                pid: 4242,
                name: Some("npm".into()),
                path: h.clone(),
            })
            .collect();
        if holders.is_empty() {
            ProbeResult::not_in_use()
        } else {
            ProbeResult::InUse(holders)
        }
    }

    fn files_held_open(&self, _files: &[PathBuf]) -> ProbeResult {
        ProbeResult::not_in_use()
    }
}

/// Caller signals on a clean, idle machine.
fn idle_signals() -> CallerSignals {
    CallerSignals {
        git: GitTracking::NotTracked,
        in_protected_root: false,
        user_excluded: false,
        protect_marker: false,
        in_use: Tri::No,
        crosses_mount: false,
        link_escape: false,
        cloud_sync: false,
        recent_activity: Tri::Yes,
    }
}

fn identity_available() -> bool {
    Native::new().capabilities().file_identity
}

fn home(fx: &Fixture) -> PathBuf {
    fx.root().join("home")
}

/// The trusted tools folder (outside the root and the canary).
fn tools(fx: &Fixture) -> PathBuf {
    fx.base().join("tools")
}

fn cache_env(fx: &Fixture, vars: &[(&str, String)]) -> CacheEnv {
    CacheEnv::with_vars(
        home(fx),
        Os::Linux,
        vars.iter().map(|(k, v)| (*k, v.clone())),
    )
}

fn locate_on(env: &CacheEnv, platform: &dyn Platform) -> CacheScan {
    let reader = StdReader;
    locate_caches(
        env,
        platform,
        &DetectContext::new().with_config_reader(&reader),
    )
}

fn locate(fx: &Fixture) -> CacheScan {
    locate_on(&cache_env(fx, &[]), &Native::new())
}

fn candidates(scan: &CacheScan) -> Vec<Candidate> {
    scan.found
        .iter()
        .map(|c| c.clone().into_candidate(&idle_signals(), None))
        .collect()
}

/// Options whose search list is `dirs` (plus nothing else), sandbox inside the fixture base.
fn options(fx: &Fixture, dirs: &[PathBuf]) -> QuickCleanOptions {
    let mut o = QuickCleanOptions::new(std::env::join_paths(dirs).unwrap());
    o.cwd = Some(fx.root().to_path_buf());
    o.excluded_roots = vec![fx.root().to_path_buf()];
    o.sandbox_base = Some(sandbox_base(fx));
    o.timeout = Duration::from_secs(60);
    o
}

fn sandbox_base(fx: &Fixture) -> PathBuf {
    let p = fx.base().join("sandbox");
    fs::create_dir_all(&p).unwrap();
    p
}

fn quick(opts: QuickCleanOptions) -> QuickClean {
    QuickClean::new(opts, Arc::new(Native::new())).with_elevation(Elevation::NotElevated)
}

fn canonical(p: &Path) -> PathBuf {
    let c = fs::canonicalize(p).unwrap();
    let s = c.to_string_lossy().into_owned();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c,
    }
}

fn exe(tool: &str) -> String {
    format!("{tool}{}", std::env::consts::EXE_SUFFIX)
}

fn rel(fx: &Fixture, p: &Path) -> PathBuf {
    p.strip_prefix(fx.root()).unwrap().to_path_buf()
}

fn npm_cache(fx: &mut Fixture) -> PathBuf {
    let c = "home/.npm/_cacache";
    fx.file(format!("{c}/index-v5/1a/2b/3c4d"), "{\"key\":\"x\"}\n")
        .unwrap()
        .file_of_size(format!("{c}/content-v2/sha512/ab/cd/keep01"), 4096)
        .unwrap()
        .file_of_size(format!("{c}/content-v2/sha512/ab/cd/unused01"), 64 * 1024)
        .unwrap()
        .file(format!("{c}/{}", fake::GUARD), "")
        .unwrap();
    fx.root().join(c)
}

fn bun_cache(fx: &mut Fixture) -> PathBuf {
    let c = "home/.bun/install/cache";
    fx.file(format!("{c}/left-pad@1.3.0@@@1/package.json"), "{}")
        .unwrap()
        .file(format!("{c}/a1b2c3.npm"), "manifest")
        .unwrap()
        .file(format!("{c}/{}", fake::GUARD), "")
        .unwrap();
    fx.root().join(c)
}

fn pnpm_store(fx: &mut Fixture) -> PathBuf {
    let s = "home/.local/share/pnpm/store";
    fx.file_of_size(format!("{s}/v10/files/00/unused-abc"), 4096)
        .unwrap()
        .file(format!("{s}/v10/index/00/abc-index.json"), "{}")
        .unwrap()
        .file(format!("{s}/{}", fake::GUARD), "")
        .unwrap();
    fx.root().join(s)
}

fn yarn_classic(fx: &mut Fixture) -> PathBuf {
    let c = "home/.cache/yarn";
    fx.file(
        format!("{c}/v6/npm-left-pad-1.3.0-abc/node_modules/left-pad/package.json"),
        "{}",
    )
    .unwrap()
    .file(format!("{c}/{}", fake::GUARD), "")
    .unwrap();
    fx.root().join(c)
}

fn yarn_berry(fx: &mut Fixture) -> PathBuf {
    let c = "home/.yarn/berry/cache";
    fx.file_of_size(format!("{c}/left-pad-npm-1.3.0-abc123-10c0.zip"), 2048)
        .unwrap()
        .file(format!("{c}/{}", fake::GUARD), "")
        .unwrap();
    fx.root().join(c)
}

/// Files below `dir`, relative, sorted.
fn files_below(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, d: &Path, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                walk(base, &p, out);
            } else {
                out.push(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn skip_reason(plan_or_report: &[SkippedTool], tool: CacheTool) -> SkipReason {
    plan_or_report
        .iter()
        .find(|s| s.tool == tool)
        .unwrap_or_else(|| panic!("{tool:?} not skipped in {plan_or_report:?}"))
        .reason
        .clone()
}

fn planned_tools(plan: &QuickCleanPlan) -> Vec<CacheTool> {
    plan.tools.iter().map(|t| t.tool).collect()
}

/// I-7: every spawned process is an allowlisted tool from the trusted folder.
fn assert_spawns_allowlisted(fx: &Fixture, spawned: &[Spawn]) {
    let trusted = canonical(&tools(fx));
    for s in spawned {
        let stem = s.binary.file_stem().unwrap().to_str().unwrap();
        assert!(fake::TOOLS.contains(&stem), "not allowlisted: {s:?}");
        assert_eq!(
            s.binary.parent(),
            Some(trusted.as_path()),
            "untrusted: {s:?}"
        );
    }
}

/// Run the invariants: I-1 on the candidates of planned tools, I-3 allowing only `allowed`
/// (relative to the root). I-2, I-3 must have run and nothing may fail.
fn check(fx: &Fixture, plan: &QuickCleanPlan, cands: &[Candidate], allowed: &[PathBuf]) {
    let entries: Vec<PlanEntry> = cands
        .iter()
        .filter(|c| plan.tools.iter().any(|t| t.item_id == c.id))
        .map(PlanEntry::from)
        .collect();
    let mut inputs = InvariantInputs::new().plan(entries);
    for a in allowed {
        inputs = inputs.allow(a.clone());
    }
    let report = fx.check_invariants(&inputs);
    report.assert_ok();
    for id in [InvariantId::I1, InvariantId::I2, InvariantId::I3] {
        assert!(report.outcome(id).ran(), "{id} did not run: {report}");
    }
}

/// The sanitized environment's variable names (QuickClean module docs), plus `extra`.
fn expected_env_names(extra: &[&str]) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "APPDATA",
        "LOCALAPPDATA",
        "TMPDIR",
        "TEMP",
        "TMP",
        "LC_ALL",
        "LANG",
        "NO_COLOR",
        "npm_config_update_notifier",
        "npm_config_offline",
        "COREPACK_ENABLE_NETWORK",
        "COREPACK_ENABLE_DOWNLOAD_PROMPT",
        "COREPACK_ENABLE_AUTO_PIN",
        "YARN_IGNORE_PATH",
        "YARN_ENABLE_TELEMETRY",
        "YARN_ENABLE_NETWORK",
    ]
    .iter()
    .chain(extra)
    .map(|s| (*s).to_owned())
    .collect();
    if cfg!(windows) && std::env::var_os("SystemRoot").is_some() {
        names.insert("SystemRoot".into());
    }
    names
}

/// The recorded environment's names, minus what the OS itself may add to a child (macOS
/// CoreFoundation sets `__CF_USER_TEXT_ENCODING`).
fn env_names(env: &BTreeMap<String, String>) -> BTreeSet<String> {
    env.keys()
        .filter(|k| k.as_str() != "__CF_USER_TEXT_ENCODING")
        .cloned()
        .collect()
}

/// The single run of the fake `tool`.
fn only_call(fx: &Fixture, tool: &str) -> fake::Call {
    let calls = fake::calls(&tools(fx), tool);
    assert_eq!(calls.len(), 1, "{tool} calls: {calls:?}");
    calls.into_iter().next().unwrap()
}

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

fn path_arg(p: &Path) -> OsString {
    p.as_os_str().to_owned()
}

fn run_ok(q: &QuickClean, plan: &QuickCleanPlan) -> QuickCleanReport {
    q.run(plan, &Probe::idle(), &CancelToken::new())
}

// ---------------------------------------------------------------------------------------------
// FX-QC-01: npm, native command, exact process contract, measured result
// ---------------------------------------------------------------------------------------------

fn fx_qc_01_npm_prune_exact_argv_env_cwd_and_measured() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let cache = npm_cache(&mut fx);
    fx.checkpoint().unwrap();
    let npm = fake::install(&tools(&fx), "npm");
    let scan = locate(&fx);
    let cands = candidates(&scan);
    assert_eq!(cands[0].tier, Tier::S1);
    let q = quick(options(&fx, &[tools(&fx)]));
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    assert!(plan.spawned.is_empty(), "planning npm starts nothing");
    let t = &plan.tools[0];
    let location = home(&fx).join(".npm");
    assert_eq!(
        t.binary,
        canonical(&npm),
        "resolved binary shown in the preview"
    );
    assert_eq!(
        t.args,
        [
            os(&["cache", "verify", "--cache"]),
            vec![path_arg(&location)]
        ]
        .concat()
    );
    assert!(t.env.is_empty());
    assert_eq!(t.mode, CleanMode::Prune);
    assert!(!t.wipe_requested);
    assert_eq!(t.not_undoable, NOT_UNDOABLE_NOTE);
    assert_eq!(t.location, location);
    assert!(
        t.command_line()
            .starts_with(&*canonical(&npm).to_string_lossy())
    );
    assert!(
        t.command_line()
            .ends_with(&format!("--cache {}", location.display()))
    );

    let report = run_ok(&q, &plan);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    let r = &report.results[0];
    assert!(r.succeeded(), "{r:?}");
    assert_eq!(r.ended, Ended::Exited(Some(0)));
    assert!(
        r.stdout
            .lines
            .iter()
            .any(|l| l.contains("fake npm: cleaned"))
    );

    // Exactly the planned argv, from a neutral private folder, with the sanitized environment.
    let call = only_call(&fx, "npm");
    assert_eq!(
        call.argv,
        ["cache", "verify", "--cache", &location.to_string_lossy()]
    );
    let sandbox = canonical(&sandbox_base(&fx));
    assert!(call.cwd.starts_with(&sandbox), "cwd {:?}", call.cwd);
    assert_eq!(call.cwd.file_name().unwrap(), "cwd");
    assert_ne!(call.cwd, fx.root());
    assert_eq!(env_names(&call.env), expected_env_names(&[]));
    assert_eq!(
        call.env["PATH"],
        canonical(&tools(&fx)).to_string_lossy(),
        "PATH holds the trusted folders only"
    );
    for k in [
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "APPDATA",
    ] {
        assert!(Path::new(&call.env[k]).starts_with(&sandbox), "{k}");
    }
    assert_eq!(call.env["YARN_IGNORE_PATH"], "1");
    assert_eq!(call.env["npm_config_offline"], "true");
    // The sandbox is gone afterwards.
    assert_eq!(fs::read_dir(&sandbox).unwrap().count(), 0);

    // Prune: only unused entries went.
    let left = files_below(&cache);
    assert!(
        left.contains(&"content-v2/sha512/ab/cd/keep01".to_owned()),
        "{left:?}"
    );
    assert!(!left.iter().any(|f| f.contains("unused")), "{left:?}");
    // Measured (QC-06): sizes before/after, free space sampled on the cache's volume.
    let (before, after) = (r.size_before.unwrap(), r.size_after.unwrap());
    assert!(
        before.allocated > after.allocated,
        "{before:?} -> {after:?}"
    );
    assert!(r.cache_bytes_freed().unwrap() >= 64 * 1024 - 4096);
    assert!(r.free_before.is_some() && r.free_after.is_some());
    assert!(r.free_space_delta().is_some());
    assert_eq!(r.explanation, None);
    assert_spawns_allowlisted(&fx, &report.spawned);
    check(&fx, &plan, &cands, &[rel(&fx, &cache)]);
}

fn fx_qc_01_npm_wipe_only_when_requested() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let cache = npm_cache(&mut fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let q = quick(options(&fx, &[tools(&fx)]));
    let request = QuickCleanRequest {
        wipe: BTreeSet::from([CacheTool::Npm]),
        ..QuickCleanRequest::default()
    };
    let plan = q.plan(&scan, &cands, &request, &Probe::idle());
    let t = &plan.tools[0];
    let location = home(&fx).join(".npm");
    assert_eq!(t.mode, CleanMode::Wipe);
    assert!(t.wipe_requested);
    assert_eq!(
        t.args,
        [
            os(&["cache", "clean", "--force", "--cache"]),
            vec![path_arg(&location)]
        ]
        .concat()
    );
    let report = run_ok(&q, &plan);
    assert!(report.results[0].succeeded());
    assert_eq!(
        only_call(&fx, "npm").argv,
        [
            "cache",
            "clean",
            "--force",
            "--cache",
            &location.to_string_lossy()
        ]
    );
    assert_eq!(files_below(&cache), [fake::GUARD]);
    assert_spawns_allowlisted(&fx, &report.spawned);
    check(&fx, &plan, &cands, &[rel(&fx, &cache)]);
}

// ---------------------------------------------------------------------------------------------
// FX-QC-02: pnpm store referenced by projects
// ---------------------------------------------------------------------------------------------

fn fx_qc_02_pnpm_store_prune_only_with_partial_reclaim_explanation() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let store = pnpm_store(&mut fx);
    // Three packages hard-linked from a project's node_modules: still referenced.
    for i in 0..3 {
        let in_store = format!("home/.local/share/pnpm/store/v10/files/0{i}/pkg{i}");
        fx.file_of_size(&in_store, 32 * 1024).unwrap();
        fx.hardlink(
            format!("proj/node_modules/.pnpm/pkg{i}/index.js"),
            &in_store,
        )
        .unwrap();
    }
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "pnpm");
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let q = quick(options(&fx, &[tools(&fx)]));
    // Even a requested wipe runs the prune: the store is never removed directly.
    let request = QuickCleanRequest {
        wipe: BTreeSet::from([CacheTool::Pnpm]),
        ..QuickCleanRequest::default()
    };
    let plan = q.plan(&scan, &cands, &request, &Probe::idle());
    assert_eq!(planned_tools(&plan), [CacheTool::Pnpm]);
    let t = &plan.tools[0];
    assert_eq!(t.mode, CleanMode::Prune);
    assert_eq!(
        t.args,
        [
            os(&["store", "prune", "--store-dir"]),
            vec![path_arg(&store)]
        ]
        .concat()
    );
    assert!(
        t.notes.iter().any(|n| n.contains("no full wipe")),
        "{:?}",
        t.notes
    );
    assert!(t.notes.iter().any(|n| n.contains("never removed directly")));

    let report = run_ok(&q, &plan);
    let r = &report.results[0];
    assert!(r.succeeded(), "{r:?}");
    assert_eq!(
        only_call(&fx, "pnpm").argv,
        ["store", "prune", "--store-dir", &store.to_string_lossy()]
    );
    // The store folder and every referenced package are still there; only the unused went.
    let left = files_below(&store);
    assert!(store.is_dir());
    assert!(!left.iter().any(|f| f.contains("unused")), "{left:?}");
    assert_eq!(
        left.iter().filter(|f| f.contains("/pkg")).count(),
        3,
        "{left:?}"
    );
    let after = r.size_after.unwrap();
    assert!(after.shared_allocated >= 3 * 32 * 1024, "{after:?}");
    let why = r.explanation.as_deref().expect("partial reclaim explained");
    assert!(why.contains("no project references"), "{why}");
    assert_spawns_allowlisted(&fx, &report.spawned);
    check(&fx, &plan, &cands, &[rel(&fx, &store)]);
}

// ---------------------------------------------------------------------------------------------
// FX-QC-03: tool running
// ---------------------------------------------------------------------------------------------

fn fx_qc_03_tool_in_use_is_skipped_and_reported() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let npm = npm_cache(&mut fx);
    let bun = bun_cache(&mut fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    fake::install(&tools(&fx), "bun");
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let q = quick(options(&fx, &[tools(&fx)]));

    // An install in progress holds a file in the Bun cache.
    let probe = Probe::holding(bun.join("left-pad@1.3.0@@@1/package.json"));
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &probe);
    assert_eq!(planned_tools(&plan), [CacheTool::Npm]);
    match skip_reason(&plan.skipped, CacheTool::Bun) {
        SkipReason::InUse(by) => assert!(by[0].contains("npm (pid 4242)"), "{by:?}"),
        r => panic!("{r:?}"),
    }
    // Unknown counts as in use.
    let unknown = Probe {
        unknown: true,
        ..Probe::default()
    };
    let plan_unknown = q.plan(&scan, &cands, &QuickCleanRequest::default(), &unknown);
    assert!(plan_unknown.tools.is_empty());
    assert!(matches!(
        skip_reason(&plan_unknown.skipped, CacheTool::Npm),
        SkipReason::ActivityUnknown(_)
    ));

    let report = run_ok(&q, &plan);
    assert_eq!(report.results.len(), 1);
    assert!(matches!(
        skip_reason(&report.skipped, CacheTool::Bun),
        SkipReason::InUse(_)
    ));
    assert!(fake::calls(&tools(&fx), "bun").is_empty(), "bun never ran");
    assert!(files_below(&bun).contains(&"a1b2c3.npm".to_owned()));
    assert_spawns_allowlisted(&fx, &report.spawned);
    check(&fx, &plan, &cands, &[rel(&fx, &npm)]);
}

fn fx_qc_03_in_use_at_run_time_is_skipped() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let npm = npm_cache(&mut fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let q = quick(options(&fx, &[tools(&fx)]));
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert_eq!(planned_tools(&plan), [CacheTool::Npm]);
    // Between preview and confirmation an `npm install` started.
    let busy = Probe::holding(npm.join("index-v5/1a/2b/3c4d"));
    let report = q.run(&plan, &busy, &CancelToken::new());
    assert!(report.results.is_empty());
    assert!(matches!(
        skip_reason(&report.skipped, CacheTool::Npm),
        SkipReason::InUse(_)
    ));
    assert!(report.spawned.is_empty());
    assert!(!fake::ran_in(&tools(&fx)));
    check(&fx, &plan, &cands, &[]);
}

/// A real process with its working directory inside the cache, seen by the native probe.
fn fx_qc_03_real_process_inside_cache_is_detected() {
    if !identity_available() || cfg!(windows) {
        return; // Windows reports open files via Restart Manager; covered by the fake probe.
    }
    let mut fx = Fixture::new().unwrap();
    let npm = npm_cache(&mut fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let q = quick(options(&fx, &[tools(&fx)]));
    // A fake npm "install" sleeping with its working directory inside the cache. It is started
    // under another name so it records nothing in the trusted folder.
    let holder_dir = fx.base().join("holder");
    let holder = fake::install(&holder_dir, "npm");
    fake::behave(&holder_dir, "npm", &[("sleep_ms", "20000")]);
    let mut child = std::process::Command::new(&holder)
        .arg("--hold")
        .current_dir(npm.join("index-v5"))
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let probe = scull_safety::NativeActivityProbe::capture(Default::default())
        .with_policy(scull_safety::GapPolicy::Disclose);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &probe);
    let _ = child.kill();
    let _ = child.wait();
    assert!(plan.tools.is_empty(), "{plan:?}");
    match skip_reason(&plan.skipped, CacheTool::Npm) {
        SkipReason::InUse(by) => assert!(by.iter().any(|b| b.contains("process cwd")), "{by:?}"),
        r => panic!("{r:?}"),
    }
    check(&fx, &plan, &cands, &[]);
}

// ---------------------------------------------------------------------------------------------
// FX-QC-04: symlinked cache; QC-05 other volume
// ---------------------------------------------------------------------------------------------

fn fx_qc_04_symlinked_cache_is_skipped() {
    let mut fx = Fixture::new().unwrap();
    // The real cache lives elsewhere; ~/.npm is a link to it.
    fx.file("elsewhere/npm/_cacache/index-v5/1a/2b/3c4d", "{}")
        .unwrap()
        .file(format!("elsewhere/npm/_cacache/{}", fake::GUARD), "")
        .unwrap()
        .dir("home")
        .unwrap();
    let target = fx.root().join("elsewhere/npm");
    if !fx.symlink("home/.npm", &target).unwrap().is_created() {
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    }
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let q = quick(options(&fx, &[tools(&fx)]));
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert!(plan.tools.is_empty());
    assert!(matches!(
        skip_reason(&plan.skipped, CacheTool::Npm),
        SkipReason::Problem(CacheProblem::Link { .. })
    ));
    let report = run_ok(&q, &plan);
    assert!(report.results.is_empty() && report.spawned.is_empty());
    assert!(!fake::ran_in(&tools(&fx)));
    check(&fx, &plan, &cands, &[]);
}

fn fx_qc_04_cache_swapped_for_symlink_after_planning_is_skipped() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let cache = npm_cache(&mut fx);
    fx.dir("elsewhere").unwrap();
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let q = quick(options(&fx, &[tools(&fx)]));
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert_eq!(planned_tools(&plan), [CacheTool::Npm]);
    // After confirmation, ~/.npm is swapped for a link to another folder.
    let npm_dir = home(&fx).join(".npm");
    let moved = fx.root().join("elsewhere/npm");
    fs::rename(&npm_dir, &moved).unwrap();
    if !fx.symlink("home/.npm", &moved).unwrap().is_created() {
        fs::rename(&moved, &npm_dir).unwrap();
        check(&fx, &plan, &cands, &[]);
        return;
    }
    let report = run_ok(&q, &plan);
    assert!(report.results.is_empty(), "{report:?}");
    assert!(report.spawned.is_empty());
    let reason = skip_reason(&report.skipped, CacheTool::Npm);
    assert!(
        matches!(
            reason,
            SkipReason::Problem(CacheProblem::Link { .. }) | SkipReason::Changed(_)
        ),
        "{reason:?}"
    );
    assert!(!fake::ran_in(&tools(&fx)));
    assert!(
        files_below(&moved.join("_cacache"))
            .iter()
            .any(|f| f.contains("unused"))
    );
    // Restore the layout so I-3 only sees the swap we made ourselves.
    fs::remove_file(&npm_dir)
        .or_else(|_| fs::remove_dir(&npm_dir))
        .unwrap();
    fs::rename(&moved, &npm_dir).unwrap();
    let _ = cache;
    check(&fx, &plan, &cands, &[]);
}

/// A platform that reports everything under `under` as being on another volume.
struct OtherVolume {
    inner: Native,
    under: PathBuf,
}

impl Platform for OtherVolume {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn metadata(&self, path: &Path) -> scull_platform::Result<Metadata> {
        let mut md = self.inner.metadata(path)?;
        if path.starts_with(&self.under)
            && let Some(id) = md.identity.as_mut()
        {
            id.volume = id.volume.wrapping_add(1);
        }
        Ok(md)
    }

    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<DirIter<'a>> {
        self.inner.read_dir(path)
    }
}

fn qc_05_cache_on_other_volume_is_skipped() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let npm = npm_cache(&mut fx);
    bun_cache(&mut fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    fake::install(&tools(&fx), "bun");
    let platform = Arc::new(OtherVolume {
        inner: Native::new(),
        under: home(&fx).join(".bun"),
    });
    let scan = locate_on(&cache_env(&fx, &[]), platform.as_ref());
    let cands = candidates(&scan);
    let q = QuickClean::new(options(&fx, &[tools(&fx)]), platform)
        .with_elevation(Elevation::NotElevated);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert_eq!(planned_tools(&plan), [CacheTool::Npm]);
    assert!(matches!(
        skip_reason(&plan.skipped, CacheTool::Bun),
        SkipReason::Problem(CacheProblem::OtherVolume { .. })
    ));
    let report = run_ok(&q, &plan);
    assert_eq!(report.results.len(), 1);
    assert!(fake::calls(&tools(&fx), "bun").is_empty());
    assert_spawns_allowlisted(&fx, &report.spawned);
    check(&fx, &plan, &cands, &[rel(&fx, &npm)]);
}

// ---------------------------------------------------------------------------------------------
// FX-QC-05: tool uninstalled, cache left behind
// ---------------------------------------------------------------------------------------------

/// QC-03 asks for removal through the safe executor, but D-28 allows `Delete` only at S0 and
/// caches are S1: the cache is reported for manual removal and nothing is touched (see the PR
/// for the proposed decision).
fn fx_qc_05_uninstalled_tool_cache_is_reported_not_removed() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let bun = bun_cache(&mut fx);
    fx.checkpoint().unwrap();
    fs::create_dir_all(tools(&fx)).unwrap(); // trusted folder without bun
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let q = quick(options(&fx, &[tools(&fx)]));
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert!(plan.tools.is_empty());
    let reason = skip_reason(&plan.skipped, CacheTool::Bun);
    assert_eq!(
        reason,
        SkipReason::NotInstalled {
            manual_removal: bun.clone()
        }
    );
    assert!(reason.to_string().contains("manual removal"));
    let report = run_ok(&q, &plan);
    assert!(report.results.is_empty() && report.spawned.is_empty());
    assert!(bun.join("a1b2c3.npm").is_file());
    check(&fx, &plan, &cands, &[]);
}

// ---------------------------------------------------------------------------------------------
// FX-QC-06: tool hijack (T-22)
// ---------------------------------------------------------------------------------------------

/// Fake `npm`s in the current directory, a scan root, a project `node_modules/.bin` and behind
/// a relative entry, all earlier in the search list than the trusted folder. They live beside
/// the fixture root (hashing three copies of this executable for every I-3 snapshot would be
/// slow); their sentinels are checked directly.
fn hijack_dirs(fx: &Fixture) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let cwd = fx.base().join("untrusted/cwd");
    let scan_root = fx.base().join("untrusted/scanroot/bin");
    let node_bin = fx.base().join("untrusted/proj/node_modules/.bin");
    let untrusted = vec![cwd, scan_root, node_bin];
    for d in &untrusted {
        fake::install(d, "npm");
    }
    let mut search = untrusted.clone();
    search.insert(0, PathBuf::from("tools-relative"));
    search.insert(0, PathBuf::from("."));
    (untrusted, search)
}

fn hijack_options(fx: &Fixture, search: &[PathBuf]) -> QuickCleanOptions {
    let mut o = options(fx, search);
    o.cwd = Some(fx.base().join("untrusted/cwd"));
    o.excluded_roots = vec![fx.base().join("untrusted/scanroot")];
    o
}

fn fx_qc_06_untrusted_npm_binaries_never_run() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let cache = npm_cache(&mut fx);
    let (untrusted, mut search) = hijack_dirs(&fx);
    fx.checkpoint().unwrap();
    let npm = fake::install(&tools(&fx), "npm");
    search.push(tools(&fx));
    let q = quick(hijack_options(&fx, &search));
    assert_eq!(q.trusted_dirs().dirs(), [canonical(&tools(&fx))]);
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    let t = &plan.tools[0];
    assert_eq!(t.binary, canonical(&npm));
    assert!(
        t.command_line()
            .contains(&*canonical(&npm).to_string_lossy())
    );
    let report = run_ok(&q, &plan);
    assert!(report.results[0].succeeded());
    for d in &untrusted {
        assert!(!fake::ran_in(d), "sentinel in {}", d.display());
    }
    assert_eq!(fake::calls(&tools(&fx), "npm").len(), 1);
    assert_spawns_allowlisted(&fx, &report.spawned);
    check(&fx, &plan, &cands, &[rel(&fx, &cache)]);
}

fn fx_qc_06_only_untrusted_binaries_means_not_installed() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let cache = npm_cache(&mut fx);
    let (untrusted, search) = hijack_dirs(&fx);
    fx.checkpoint().unwrap();
    let q = quick(hijack_options(&fx, &search));
    assert!(q.trusted_dirs().dirs().is_empty());
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert_eq!(
        skip_reason(&plan.skipped, CacheTool::Npm),
        SkipReason::NotInstalled {
            manual_removal: cache
        }
    );
    let report = run_ok(&q, &plan);
    assert!(report.spawned.is_empty());
    for d in &untrusted {
        assert!(!fake::ran_in(d), "sentinel in {}", d.display());
    }
    check(&fx, &plan, &cands, &[]);
}

/// A symlink in a trusted folder pointing at a project's `node_modules/.bin/npm` is rejected
/// after resolving it.
fn fx_qc_06_trusted_symlink_into_node_modules_is_rejected() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx);
    fx.checkpoint().unwrap();
    let evil = fake::install(&fx.base().join("proj/node_modules/.bin"), "npm");
    let link = tools(&fx).join(exe("npm"));
    fs::create_dir_all(tools(&fx)).unwrap();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(&evil, &link).is_ok();
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(&evil, &link).is_ok();
    if !made {
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    }
    let q = quick(options(&fx, &[tools(&fx)]));
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert!(matches!(
        skip_reason(&plan.skipped, CacheTool::Npm),
        SkipReason::NotInstalled { .. }
    ));
    let report = run_ok(&q, &plan);
    assert!(report.spawned.is_empty());
    assert!(!fake::ran_in(evil.parent().unwrap()));
    check(&fx, &plan, &cands, &[]);
}

// ---------------------------------------------------------------------------------------------
// FX-QC-07: tool config redirect (T-23)
// ---------------------------------------------------------------------------------------------

/// A project `.npmrc` with `cache=<home>` in the user's current directory never applies: the
/// command runs from a neutral folder and is told the validated location explicitly.
fn fx_qc_07_project_npmrc_never_applies() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let cache = npm_cache(&mut fx);
    let home_s = home(&fx).to_string_lossy().into_owned();
    fx.file("proj/.npmrc", format!("cache={home_s}\n"))
        .unwrap()
        .file("proj/package.json", "{}")
        .unwrap();
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    let mut o = options(&fx, &[tools(&fx)]);
    o.cwd = Some(fx.root().join("proj"));
    let q = quick(o);
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    let report = run_ok(&q, &plan);
    assert!(report.results[0].succeeded());
    let call = only_call(&fx, "npm");
    assert_ne!(call.cwd, fx.root().join("proj"));
    assert!(call.cwd.starts_with(canonical(&sandbox_base(&fx))));
    let location = home(&fx).join(".npm");
    assert_eq!(call.argv[3], location.to_string_lossy());
    assert_ne!(call.argv[3], home_s);
    assert!(
        !call
            .env
            .keys()
            .any(|k| k.eq_ignore_ascii_case("npm_config_cache"))
    );
    check(&fx, &plan, &cands, &[rel(&fx, &cache)]);
}

/// A user `.npmrc` with `cache=<home>` is refused by the locator: npm is skipped and reported.
fn fx_qc_07_user_npmrc_cache_at_home_is_refused() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx);
    let home_s = home(&fx).to_string_lossy().into_owned();
    fx.file("home/.npmrc", format!("cache={home_s}\n")).unwrap();
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    let q = quick(options(&fx, &[tools(&fx)]));
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert!(plan.tools.is_empty());
    match skip_reason(&plan.skipped, CacheTool::Npm) {
        SkipReason::Refused(r) => assert!(r.contains("home folder"), "{r}"),
        r => panic!("{r:?}"),
    }
    let report = run_ok(&q, &plan);
    assert!(report.spawned.is_empty());
    assert!(!fake::ran_in(&tools(&fx)));
    check(&fx, &plan, &cands, &[]);
}

// ---------------------------------------------------------------------------------------------
// FX-QC-08: unticked tools
// ---------------------------------------------------------------------------------------------

fn fx_qc_08_unticked_tools_are_not_cleaned() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let npm = npm_cache(&mut fx);
    let pnpm = pnpm_store(&mut fx);
    let bun = bun_cache(&mut fx);
    let yarn = yarn_classic(&mut fx);
    fx.checkpoint().unwrap();
    for t in fake::TOOLS {
        fake::install(&tools(&fx), t);
    }
    let q = quick(options(&fx, &[tools(&fx)]));
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let request = QuickCleanRequest {
        unticked: BTreeSet::from([CacheTool::Pnpm, CacheTool::Bun]),
        ..QuickCleanRequest::default()
    };
    let plan = q.plan(&scan, &cands, &request, &Probe::idle());
    assert_eq!(
        planned_tools(&plan),
        [CacheTool::Npm, CacheTool::YarnClassic]
    );
    assert_eq!(
        skip_reason(&plan.skipped, CacheTool::Pnpm),
        SkipReason::Unticked
    );
    assert_eq!(
        skip_reason(&plan.skipped, CacheTool::Bun),
        SkipReason::Unticked
    );
    let pnpm_before = files_below(&pnpm);
    let bun_before = files_below(&bun);
    let report = run_ok(&q, &plan);
    assert_eq!(report.results.len(), 2);
    assert!(report.results.iter().all(|r| r.succeeded()), "{report:?}");
    assert!(fake::calls(&tools(&fx), "pnpm").is_empty());
    assert!(fake::calls(&tools(&fx), "bun").is_empty());
    assert_eq!(files_below(&pnpm), pnpm_before);
    assert_eq!(files_below(&bun), bun_before);
    assert_eq!(files_below(&yarn), [fake::GUARD]);
    // The yarn version probe plus the two tools: all allowlisted, all trusted.
    assert_spawns_allowlisted(&fx, &plan.spawned);
    assert_spawns_allowlisted(&fx, &report.spawned);
    check(&fx, &plan, &cands, &[rel(&fx, &npm), rel(&fx, &yarn)]);
}

// ---------------------------------------------------------------------------------------------
// Bun and Yarn: location variables
// ---------------------------------------------------------------------------------------------

fn bun_and_yarn_location_variables_target_the_located_cache() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let bun = bun_cache(&mut fx);
    let yarn = yarn_classic(&mut fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "bun");
    fake::install(&tools(&fx), "yarn");
    let q = quick(options(&fx, &[tools(&fx)]));
    let scan = locate(&fx);
    let cands = candidates(&scan);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert_eq!(
        planned_tools(&plan),
        [CacheTool::Bun, CacheTool::YarnClassic]
    );
    // Bun and Yarn have no prune: their only cleanup is the wipe, not requested by the user.
    for t in &plan.tools {
        assert_eq!(t.mode, CleanMode::Wipe);
        assert!(!t.wipe_requested);
    }
    assert_eq!(plan.tools[0].args, os(&["pm", "cache", "rm"]));
    assert_eq!(
        plan.tools[0].env,
        [("BUN_INSTALL_CACHE_DIR".to_owned(), path_arg(&bun))]
    );
    assert_eq!(plan.tools[1].args, os(&["cache", "clean"]));
    assert_eq!(
        plan.tools[1].env,
        [("YARN_CACHE_FOLDER".to_owned(), path_arg(&yarn))]
    );
    let report = run_ok(&q, &plan);
    assert!(report.results.iter().all(|r| r.succeeded()), "{report:?}");
    let bun_call = only_call(&fx, "bun");
    assert_eq!(bun_call.argv, ["pm", "cache", "rm"]);
    assert_eq!(bun_call.env["BUN_INSTALL_CACHE_DIR"], bun.to_string_lossy());
    assert_eq!(
        env_names(&bun_call.env),
        expected_env_names(&["BUN_INSTALL_CACHE_DIR"])
    );
    let yarn_calls = fake::calls(&tools(&fx), "yarn");
    assert_eq!(yarn_calls.len(), 2, "version probe, then the clean");
    assert_eq!(yarn_calls[0].argv, ["--version"]);
    assert_eq!(env_names(&yarn_calls[0].env), expected_env_names(&[]));
    assert_eq!(yarn_calls[1].argv, ["cache", "clean"]);
    assert_eq!(
        env_names(&yarn_calls[1].env),
        expected_env_names(&["YARN_CACHE_FOLDER"])
    );
    assert_eq!(files_below(&bun), [fake::GUARD]);
    assert_eq!(files_below(&yarn), [fake::GUARD]);
    check(&fx, &plan, &cands, &[rel(&fx, &bun), rel(&fx, &yarn)]);
}

fn yarn_version_must_match_the_cache() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let berry = yarn_berry(&mut fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "yarn");
    let q = quick(options(&fx, &[tools(&fx)]));
    let scan = locate(&fx);
    let cands = candidates(&scan);
    // Yarn 1 must never clean the Berry cache (it would ignore `--mirror`).
    fake::behave(&tools(&fx), "yarn", &[("version", "1.22.22")]);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert!(plan.tools.is_empty());
    assert_eq!(
        skip_reason(&plan.skipped, CacheTool::YarnBerry),
        SkipReason::WrongVersion {
            reported: "1.22.22".into()
        }
    );
    assert_eq!(plan.spawned.len(), 1);
    assert_eq!(plan.spawned[0].args, os(&["--version"]));
    // Yarn 4: the global folder (parent of `cache/`) is passed.
    fake::behave(&tools(&fx), "yarn", &[("version", "4.5.0")]);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    let t = &plan.tools[0];
    let global = home(&fx).join(".yarn/berry");
    assert_eq!(t.args, os(&["cache", "clean", "--mirror"]));
    assert_eq!(
        t.env,
        [("YARN_GLOBAL_FOLDER".to_owned(), path_arg(&global))]
    );
    let report = run_ok(&q, &plan);
    assert!(report.results[0].succeeded(), "{report:?}");
    assert_eq!(files_below(&berry), [fake::GUARD]);
    assert_spawns_allowlisted(&fx, &plan.spawned);
    check(&fx, &plan, &cands, &[rel(&fx, &berry)]);
}

// ---------------------------------------------------------------------------------------------
// Process contract: timeout, output, exit status
// ---------------------------------------------------------------------------------------------

fn fx_npm_planned(
    fx: &mut Fixture,
    behavior: &[(&str, &str)],
) -> (PathBuf, CacheScan, Vec<Candidate>) {
    let cache = npm_cache(fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(fx), "npm");
    fake::behave(&tools(fx), "npm", behavior);
    let scan = locate(fx);
    let cands = candidates(&scan);
    (cache, scan, cands)
}

fn timeout_kills_the_tool() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let (_cache, scan, cands) = fx_npm_planned(&mut fx, &[("sleep_ms", "6000")]);
    let mut o = options(&fx, &[tools(&fx)]);
    o.timeout = Duration::from_millis(700);
    let q = quick(o);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    let started = Instant::now();
    let report = run_ok(&q, &plan);
    let r = &report.results[0];
    assert_eq!(r.ended, Ended::TimedOut(Duration::from_millis(700)));
    assert!(!r.succeeded());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // Killed: it never finishes its sleep, so it never cleans.
    std::thread::sleep(Duration::from_millis(6500));
    assert!(
        !tools(&fx).join("npm.finished").exists(),
        "the tool kept running"
    );
    check(&fx, &plan, &cands, &[]);
}

fn output_is_escaped_and_capped() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let (cache, scan, cands) = fx_npm_planned(&mut fx, &[("evil", "1"), ("spam", "200000")]);
    let mut o = options(&fx, &[tools(&fx)]);
    o.max_output = 4096;
    let q = quick(o);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    let report = run_ok(&q, &plan);
    let r = &report.results[0];
    assert!(r.succeeded(), "{r:?}");
    assert_eq!(r.stdout.lines[0], "ok\\x1b[2Jcleared\\u{202e}txt.exe");
    assert!(r.stdout.truncated);
    assert!(r.stdout.lines.iter().map(|l| l.len() + 1).sum::<usize>() <= 4096 + 64);
    assert_eq!(r.stderr.lines[0], "warn\\x07");
    assert!(!r.stderr.truncated);
    for l in r.stdout.lines.iter().chain(&r.stderr.lines) {
        assert!(!l.chars().any(needs_escape), "{l:?}");
    }
    check(&fx, &plan, &cands, &[rel(&fx, &cache)]);
}

fn failing_tool_reports_its_exit_status() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let (cache, scan, cands) = fx_npm_planned(&mut fx, &[("exit", "3")]);
    let q = quick(options(&fx, &[tools(&fx)]));
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    let report = run_ok(&q, &plan);
    let r = &report.results[0];
    assert_eq!(r.ended, Ended::Exited(Some(3)));
    assert!(!r.succeeded());
    check(&fx, &plan, &cands, &[rel(&fx, &cache)]);
}

// ---------------------------------------------------------------------------------------------
// D-28, SAFE-14, cancellation
// ---------------------------------------------------------------------------------------------

fn d28_s2_cache_gets_no_tool_action() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx);
    fx.checkpoint().unwrap();
    fake::install(&tools(&fx), "npm");
    let q = quick(options(&fx, &[tools(&fx)]));
    let scan = locate(&fx);
    // In-use unknown at scan time: S2, no tool command (D-26, D-28).
    let caller = CallerSignals {
        in_use: Tri::Unknown,
        ..idle_signals()
    };
    let cands: Vec<Candidate> = scan
        .found
        .iter()
        .map(|c| c.clone().into_candidate(&caller, None))
        .collect();
    assert!(cands[0].tier >= Tier::S2);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    assert!(plan.tools.is_empty());
    assert!(matches!(
        skip_reason(&plan.skipped, CacheTool::Npm),
        SkipReason::NotEligible { .. } | SkipReason::VolumeUnverified
    ));
    // No candidate at all: not classified, not run.
    let plan = q.plan(&scan, &[], &QuickCleanRequest::default(), &Probe::idle());
    assert!(plan.tools.is_empty());
    let report = run_ok(&q, &plan);
    assert!(report.spawned.is_empty());
    check(&fx, &plan, &cands, &[]);
}

fn elevated_process_refuses_unless_allowed() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let (cache, scan, cands) = fx_npm_planned(&mut fx, &[]);
    let q = quick(options(&fx, &[tools(&fx)])).with_elevation(Elevation::Elevated);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    let report = run_ok(&q, &plan);
    assert!(report.results.is_empty());
    assert_eq!(
        skip_reason(&report.skipped, CacheTool::Npm),
        SkipReason::Elevated(Elevation::Elevated)
    );
    assert!(!fake::ran_in(&tools(&fx)));
    let mut o = options(&fx, &[tools(&fx)]);
    o.allow_elevated = true;
    let q = quick(o).with_elevation(Elevation::Elevated);
    let report = run_ok(&q, &plan);
    assert!(report.results[0].succeeded());
    check(&fx, &plan, &cands, &[rel(&fx, &cache)]);
}

fn cancelled_before_start_runs_nothing() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let (_cache, scan, cands) = fx_npm_planned(&mut fx, &[]);
    let q = quick(options(&fx, &[tools(&fx)]));
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Probe::idle());
    let cancel = CancelToken::new();
    cancel.cancel();
    let report = q.run(&plan, &Probe::idle(), &cancel);
    assert!(report.results.is_empty() && report.spawned.is_empty());
    assert_eq!(
        skip_reason(&report.skipped, CacheTool::Npm),
        SkipReason::Cancelled
    );
    assert!(!fake::ran_in(&tools(&fx)));
    check(&fx, &plan, &cands, &[]);
}
