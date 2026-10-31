//! End-to-end FX-QC fixtures (docs/FIXTURES.md §3.7) with every invariant, including I-5
//! (each tool's predicted reclaim and its volume's measured free-space delta) and I-7 (every
//! process Quick Clean started is an allowlisted tool from the trusted folder).
//!
//! **No real package manager runs.** The fake tools are small `/bin/sh` scripts installed in a
//! trusted folder beside the root (Unix only: on Windows Quick Clean runs only real
//! executables, never `.cmd` shims; the Windows runs of FX-QC-01…08 are the custom-harness
//! fixtures in `crates/scull-cleaner/tests/quickclean/`, whose test binary doubles as the fake).
//! A fake only touches a cache that carries its guard file, and every planted (untrusted) fake
//! drops a sentinel inside the root if it ever runs, which I-3 would also report.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_cleaner::quickclean::{
    QuickClean, QuickCleanOptions, QuickCleanReport, QuickCleanRequest,
};
use scull_core::Candidate;
use scull_detectors::caches::CacheScan;
use scull_detectors::rules::Platform as Os;
use scull_detectors::{CacheEnv, CallerSignals, ConfigReader, DetectContext, locate_caches};
use scull_fixtures::{
    Fixture, InvariantId, InvariantInputs, InvariantReport, Outcome, PlanEntry, SpawnRecord,
};
use scull_platform::{Elevation, Native, Platform};
use scull_safety::{ActivityProbe, GitTracking, ProbeResult, Tri};
use scull_scanner::{CancelToken, NullSink, ScanOptions};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const GUARD: &str = "FAKE_CACHE";
const CACHE: &str = "home/.npm/_cacache";

/// The fake `npm`: logs its arguments next to itself, and for exactly `cache verify --cache
/// <dir>` on a guarded cache removes the one "unused" entry (a prune). Uses only shell
/// builtins and absolute paths: Quick Clean's `PATH` holds the trusted folder only.
const FAKE_NPM: &str = r#"#!/bin/sh
echo "$*" >> "${0%/*}/npm.calls"
if [ "$1" != cache ] || [ "$2" != verify ] || [ "$3" != --cache ]; then
  echo "fake npm: unexpected arguments: $*" >&2
  exit 2
fi
c="$4/_cacache"
if [ ! -f "$c/FAKE_CACHE" ]; then
  echo "fake npm: no guard file in $c" >&2
  exit 3
fi
/bin/rm -f "$c/content-v2/sha512/ab/cd/unused01"
echo "fake npm: verified"
"#;

/// A planted, untrusted `npm`: drops a sentinel beside itself if it ever runs.
const PLANTED_NPM: &str = "#!/bin/sh\n: > \"${0%/*}/SENTINEL\"\n";

fn install(dir: &Path, name: &str, script: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, script).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    p
}

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

/// Nothing holds any cache.
struct Idle;

impl ActivityProbe for Idle {
    fn process_cwds_under(&self, _dir: &Path) -> ProbeResult {
        ProbeResult::not_in_use()
    }
    fn open_files_under(&self, _dir: &Path) -> ProbeResult {
        ProbeResult::not_in_use()
    }
    fn files_held_open(&self, _files: &[PathBuf]) -> ProbeResult {
        ProbeResult::not_in_use()
    }
}

fn signals() -> CallerSignals {
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

fn tools(fx: &Fixture) -> PathBuf {
    fx.base().join("tools")
}

/// A fake npm cache with one entry in use and one unused.
fn npm_cache(fx: &mut Fixture) {
    fx.file(format!("{CACHE}/index-v5/1a/2b/3c4d"), "{\"key\":\"x\"}\n")
        .unwrap()
        .file_of_size(format!("{CACHE}/content-v2/sha512/ab/cd/keep01"), 4096)
        .unwrap()
        .file_of_size(
            format!("{CACHE}/content-v2/sha512/ab/cd/unused01"),
            64 * 1024,
        )
        .unwrap()
        .file(format!("{CACHE}/{GUARD}"), "")
        .unwrap();
}

fn locate(fx: &Fixture) -> (CacheScan, Vec<Candidate>) {
    let env = CacheEnv::with_vars(
        fx.root().join("home"),
        Os::Linux,
        std::iter::empty::<(&str, String)>(),
    );
    let reader = StdReader;
    let scan = locate_caches(
        &env,
        &Native::new(),
        &DetectContext::new().with_config_reader(&reader),
    );
    let cands = scan
        .found
        .iter()
        .map(|c| {
            // The cache's scanned size, as the engine passes it (the planned reclaim).
            let summary = scull_scanner::scan(
                Arc::new(Native::new()),
                &c.path,
                &ScanOptions::default(),
                &CancelToken::new(),
                NullSink,
            )
            .ok()
            .and_then(|o| o.root);
            c.clone().into_candidate(&signals(), summary.as_ref())
        })
        .collect();
    (scan, cands)
}

fn options(fx: &Fixture, search: &[PathBuf], cwd: PathBuf) -> QuickCleanOptions {
    let mut o = QuickCleanOptions::new(std::env::join_paths(search).unwrap());
    o.cwd = Some(cwd);
    o.excluded_roots = vec![fx.root().to_path_buf()];
    let sandbox = fx.base().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    o.sandbox_base = Some(sandbox);
    o.timeout = Duration::from_secs(60);
    o
}

fn identity_available() -> bool {
    Native::new().capabilities().file_identity
}

/// Plan, run, and check every invariant: I-1 on the planned candidates, I-3 allowing only the
/// cache, I-5 per tool run (planned reclaimable bytes vs measured free-space delta), I-7 on
/// every process started while planning and running.
fn plan_run_check(fx: &Fixture, q: &QuickClean) -> (QuickCleanReport, InvariantReport) {
    let (scan, cands) = locate(fx);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Idle);
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    assert_eq!(plan.tools.len(), 1, "{plan:?}");
    println!("preview: {}", plan.tools[0].command_line());
    let report = q.run(&plan, &Idle, &CancelToken::new());
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let mut inputs = InvariantInputs::new()
        .plan(
            cands
                .iter()
                .filter(|c| plan.tools.iter().any(|t| t.item_id == c.id))
                .map(PlanEntry::from),
        )
        .allow(CACHE)
        .spawned(
            plan.spawned
                .iter()
                .chain(&report.spawned)
                .map(|s| SpawnRecord::new(&s.binary, s.args.clone())),
        )
        .trusted_dir(tools(fx));
    for r in &report.results {
        let planned = plan.tools.iter().find(|t| t.tool == r.tool).unwrap();
        let measured = r.free_space_delta().and_then(|d| i64::try_from(d).ok());
        println!(
            "{:?}: predicted {} B, cache shrank {:?} B, free-space delta {measured:?} B",
            r.tool,
            planned.reclaimable_bytes,
            r.cache_bytes_freed()
        );
        inputs = inputs.bytes(
            format!("{:?}", r.tool),
            Some(planned.reclaimable_bytes),
            measured,
        );
    }
    let inv = fx.check_invariants(&inputs);
    println!("{inv}");
    inv.assert_ok();
    for id in [
        InvariantId::I1,
        InvariantId::I2,
        InvariantId::I3,
        InvariantId::I5,
        InvariantId::I7,
    ] {
        assert_eq!(inv.outcome(id), &Outcome::Passed, "{id}: {inv}");
    }
    (report, inv)
}

fn quick(o: QuickCleanOptions) -> QuickClean {
    QuickClean::new(o, Arc::new(Native::new())).with_elevation(Elevation::NotElevated)
}

/// FX-QC-01: npm cache present. The tool's own command runs (resolved binary in the preview,
/// structured arguments, neutral directory); the prune removes only the unused entry; the
/// cache's size and the volume's free space are measured.
#[test]
fn fx_qc_01_npm_cache_native_prune_measured_all_invariants() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx);
    fx.checkpoint().unwrap();
    let npm = install(&tools(&fx), "npm", FAKE_NPM);
    let q = quick(options(&fx, &[tools(&fx)], fx.root().to_path_buf()));
    let (report, _) = plan_run_check(&fx, &q);

    let r = &report.results[0];
    assert!(r.succeeded(), "{r:?}");
    assert_eq!(r.binary, fs::canonicalize(&npm).unwrap());
    let cache = fx.root().join(CACHE);
    assert!(cache.join("content-v2/sha512/ab/cd/keep01").is_file());
    assert!(!cache.join("content-v2/sha512/ab/cd/unused01").exists());
    assert!(r.cache_bytes_freed().unwrap() >= 64 * 1024 - 4096, "{r:?}");
    let calls = fs::read_to_string(tools(&fx).join("npm.calls")).unwrap();
    assert_eq!(calls.lines().count(), 1, "{calls}");
    assert!(calls.starts_with("cache verify --cache "), "{calls}");
}

/// FX-QC-06: fake `npm`s in a project's `node_modules/.bin`, in the current directory and
/// behind a relative `PATH` entry, all earlier in the search list than the trusted folder.
/// None of them runs (no sentinel, I-3 clean); the trusted one does, and I-7 holds.
#[test]
fn fx_qc_06_planted_npm_never_runs_all_invariants() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx);
    fx.file(
        "proj/package.json",
        "{\"name\":\"proj\",\"version\":\"1.0.0\"}\n",
    )
    .unwrap();
    let in_project = fx.root().join("proj/node_modules/.bin");
    let cwd = fx.base().join("cwd");
    let planted = [
        install(&in_project, "npm", PLANTED_NPM),
        install(&cwd, "npm", PLANTED_NPM),
    ];
    fx.checkpoint().unwrap();
    install(&tools(&fx), "npm", FAKE_NPM);
    let search = vec![
        in_project.clone(),
        cwd.clone(),
        PathBuf::from("."),
        tools(&fx),
    ];
    let q = quick(options(&fx, &search, cwd.clone()));
    let (report, _) = plan_run_check(&fx, &q);
    let r = &report.results[0];
    assert!(r.succeeded(), "{r:?}");
    assert_eq!(r.binary, fs::canonicalize(tools(&fx).join("npm")).unwrap());
    for p in &planted {
        let sentinel = p.with_file_name("SENTINEL");
        assert!(!sentinel.exists(), "planted {} ran", p.display());
    }
    assert!(
        !fx.root()
            .join(CACHE)
            .join("content-v2/sha512/ab/cd/unused01")
            .exists()
    );
}
