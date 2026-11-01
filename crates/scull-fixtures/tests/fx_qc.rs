//! End-to-end FX-QC fixtures (docs/FIXTURES.md §3.7) with every invariant, including I-4 (the
//! Quick Clean ran inside an audited operation, P2-EXEC-12: every tool run has exactly one audit
//! row, consistent with the disk, and the hash chain verifies), I-5 (the operation records the
//! predicted bytes and the measured free-space delta; so does each tool's result) and I-7
//! (every process Quick Clean started is an allowlisted tool from the trusted folder).
//!
//! **No real package manager runs.** The fake tools are small `/bin/sh` scripts installed in a
//! trusted folder beside the root (Unix only; the Windows runs of FX-QC-01…08, and the `.cmd`
//! shim resolution of P2-QC-02, are the custom-harness fixtures in
//! `crates/scull-cleaner/tests/quickclean/`, whose test binary doubles as the fake tools and
//! `node`).
//! A fake only touches a cache that carries its guard file, and every planted (untrusted) fake
//! drops a sentinel inside the root if it ever runs, which I-3 would also report.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_cleaner::quickclean::{
    QuickClean, QuickCleanOptions, QuickCleanReport, QuickCleanRequest,
};
use scull_cleaner::{Executor, LockOptions};
use scull_core::{Action, Candidate, OperationId, PlanId, PlanOrigin, PlanParams, ScanId};
use scull_detectors::caches::CacheScan;
use scull_detectors::rules::Platform as Os;
use scull_detectors::{CacheEnv, CallerSignals, ConfigReader, DetectContext, locate_caches};
use scull_engine::{PlatformOwnerOnly, now_unix};
use scull_fixtures::{
    Fixture, InvariantId, InvariantInputs, InvariantReport, Outcome, PlanEntry, SpawnRecord,
};
use scull_platform::{Elevation, Native, Platform};
use scull_safety::{ActivityProbe, GitTracking, ProbeResult, Tri};
use scull_scanner::{CancelToken, NullSink, ScanOptions};
use scull_storage::{
    ApprovalMethod, ItemOutcome, OperationEnd, OperationStart, OperationStatus, Store,
};
use std::collections::BTreeSet;
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

/// Plan, run inside an audited operation (P2-EXEC-12), and check every invariant: I-1 on the
/// planned candidates, I-3 allowing only the cache, I-4 on the operation's audit, I-5 on the
/// operation's totals and per tool run (planned reclaimable bytes vs measured free-space
/// delta), I-7 on every process started while planning and running. The audit's hash chain
/// must verify, and each tool must have one `tool_run` row holding its predicted and measured
/// bytes, with the resolved binary and exact arguments in the plan of record.
fn plan_run_check(fx: &Fixture, q: &QuickClean) -> (QuickCleanReport, InvariantReport) {
    let (scan, cands) = locate(fx);
    let plan = q.plan(&scan, &cands, &QuickCleanRequest::default(), &Idle);
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    assert_eq!(plan.tools.len(), 1, "{plan:?}");
    println!("preview: {}", plan.tools[0].command_line());

    // The confirmed preview becomes the plan of record and an operation (SAFE-08).
    let now = now_unix();
    let audit_plan = plan
        .audit_plan(
            &BTreeSet::new(),
            PlanParams {
                id: PlanId::from_suffix("QC01").unwrap(),
                scan_id: ScanId::from_suffix("QC01").unwrap(),
                created_unix: now,
                ttl_secs: None,
                machine_id: "fx-machine".into(),
                user: "tester".into(),
                origin: PlanOrigin::Cli,
                operations_watermark: None,
            },
        )
        .unwrap();
    let mut store =
        Store::open(&fx.base().join("data").join("scull.db"), &PlatformOwnerOnly).unwrap();
    store.insert_plan(&audit_plan).unwrap();
    let op_id = OperationId::from_suffix("QC01").unwrap();
    let exec = Executor::acquire(&LockOptions::at(
        fx.base().join("state").join("execution.lock"),
    ))
    .unwrap();
    let mut op = exec
        .begin_operation(
            &mut store,
            &audit_plan,
            &OperationStart {
                operation_id: op_id.clone(),
                plan_id: audit_plan.id().clone(),
                content_hash: audit_plan.content_hash().clone(),
                approver: "tester".into(),
                method: ApprovalMethod::CliInteractive,
                started_at: now,
            },
        )
        .unwrap();
    let report = q.run(&mut op, &plan, &Idle, &CancelToken::new()).unwrap();
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    assert!(!report.cancelled);
    let summary = op.finish(OperationEnd::Normal, now_unix()).unwrap();
    drop(exec);
    assert_eq!(summary.status, OperationStatus::Completed, "{summary:?}");
    let chain = store.verify_chain(&op_id).unwrap();
    assert!(chain.is_intact(), "{chain:?}");
    println!(
        "audit {op_id}: {} | predicted {} B, freed {} B, measured delta {:?} B | chain intact \
         ({} rows)",
        summary.status.as_str(),
        summary.totals.predicted_bytes,
        summary.totals.freed_bytes,
        summary.totals.measured_delta_bytes,
        chain.rows
    );
    let rows = store.audit_items(&op_id).unwrap();
    assert_eq!(rows.len(), plan.tools.len(), "{rows:#?}");
    for (t, row) in plan.tools.iter().zip(&rows) {
        println!(
            "  {} {} [{}] predicted {} B, freed {:?} B",
            row.outcome.as_str(),
            row.path,
            row.action,
            row.predicted_bytes,
            row.freed_bytes
        );
        assert_eq!(row.item_id, t.item_id);
        assert_eq!(row.outcome, ItemOutcome::ToolRun);
        assert_eq!(row.predicted_bytes, t.reclaimable_bytes);
        assert!(row.freed_bytes.is_some_and(|b| b > 0), "{row:?}");
        let recorded = audit_plan
            .items()
            .iter()
            .find(|i| i.item_id == t.item_id)
            .unwrap();
        let expected_args: Vec<String> = t
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            recorded.action,
            Action::ToolCommand {
                tool: t.binary.to_string_lossy().into_owned(),
                args: expected_args,
            }
        );
    }

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
        .trusted_dir(tools(fx))
        .audit(&store, &op_id)
        .operation_bytes(&store, &op_id);
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
        InvariantId::I4,
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
    if !scull_fixtures::skip::require(
        identity_available(),
        "fx_qc_01_npm_cache_native_prune_measured_all_invariants",
        "no file identity on this platform",
    ) {
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
    if !scull_fixtures::skip::require(
        identity_available(),
        "fx_qc_06_planted_npm_never_runs_all_invariants",
        "no file identity on this platform",
    ) {
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
