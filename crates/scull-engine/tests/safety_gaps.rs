//! P1-SAFE-05 (#203): one regression fixture per engine safety gap found by the independent
//! review of #56. Each test reproduces a candidate that ended up *less protective* than PRD
//! §8.3 requires, and pins the protective answer:
//!
//! 1. worktree candidates skipped the engine-side protections (`.scull-protect`,
//!    `scan.exclude`, ...) in `scan` and in `inspect_worktree`;
//! 2. an incomplete subtree (unreadable folder) counted as "nothing tracked" / complete;
//! 3. a cloud placeholder *inside* a candidate was only logged, not a signal;
//! 4. a failed volume list counted as "no mount inside";
//! 5. unresolvable `scan.protect` / `scan.exclude` entries were silently dropped;
//! 6. package caches discarded an *unknown* recent-activity answer.
//!
//! Conditions a temp dir can't produce portably are simulated behind the engine's injectable
//! seams: a [`Platform`] wrapper over [`Native`] that fails `read_dir` / `metadata` for chosen
//! paths or reports a macOS `SF_DATALESS` placeholder bit (like
//! `crates/scull-scanner/tests/common/graph.rs`), and a failing [`VolumeSource`]. Everything
//! runs in fixture temp dirs; real Git through the fixture helpers. Nothing is modified (I-2,
//! I-3 checked after every test).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::config::Config;
use scull_core::{Action, Candidate, Category, Evidence, ReasonCode, Tier};
use scull_detectors::CacheEnv;
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, FixedInUse, InUseSource, ScanReport, ScanRequest,
    VolumeSource, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs, NodeLock, PlanEntry};
use scull_git::GitRunner;
use scull_platform::{
    Capabilities, DirIter, Metadata, Native, Operation, Platform, PlatformError, VolumeInfo,
};
use scull_safety::Tri;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const THREE_DAYS: i64 = 3 * 24 * 3600;
/// macOS `SF_DATALESS` (`st_flags`): content evicted to the cloud.
const SF_DATALESS: u32 = 0x4000_0000;
/// Windows `FILE_ATTRIBUTE_RECALL_ON_OPEN`: cloud directory whose children aren't populated.
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;

// ---------------------------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------------------------

/// [`Native`] with injected faults, matched on the exact path the engine asks for.
#[derive(Default)]
struct Faulty {
    /// `read_dir` fails with "permission denied" (an unreadable folder).
    unreadable: Vec<PathBuf>,
    /// `metadata` fails with "permission denied".
    no_metadata: Vec<PathBuf>,
    /// `metadata` reports a cloud placeholder (`SF_DATALESS`), as on macOS iCloud.
    placeholders: Vec<PathBuf>,
    /// Listings of a placeholder (would hydrate it, PLT-04).
    violations: Mutex<Vec<PathBuf>>,
}

fn denied(op: Operation, path: &Path) -> PlatformError {
    PlatformError::from_io(op, path, io::Error::from(io::ErrorKind::PermissionDenied))
}

impl Platform for Faulty {
    fn capabilities(&self) -> Capabilities {
        Native.capabilities()
    }

    fn metadata(&self, path: &Path) -> scull_platform::Result<Metadata> {
        if self.no_metadata.iter().any(|p| p == path) {
            return Err(denied(Operation::Metadata, path));
        }
        let mut md = Native.metadata(path)?;
        if self.placeholders.iter().any(|p| p == path) {
            // The bits this OS would report: Windows attributes, else macOS flags.
            match md.file_attributes {
                Some(a) => md.file_attributes = Some(a | FILE_ATTRIBUTE_RECALL_ON_OPEN),
                None => md.bsd_flags = Some(md.bsd_flags.unwrap_or(0) | SF_DATALESS),
            }
        }
        Ok(md)
    }

    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<DirIter<'a>> {
        if self.placeholders.iter().any(|p| path.starts_with(p)) {
            self.violations.lock().unwrap().push(path.to_path_buf());
        }
        if self.unreadable.iter().any(|p| p == path) {
            return Err(denied(Operation::ReadDir, path));
        }
        Native.read_dir(path)
    }
}

/// A volume list that can't be read.
struct NoVolumes;

impl VolumeSource for NoVolumes {
    fn volumes(&self) -> Result<Vec<VolumeInfo>, PlatformError> {
        Err(PlatformError::from_io(
            Operation::ListVolumes,
            Path::new(""),
            io::Error::other("mount table unreadable (simulated)"),
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn engine_cfg(platform: Arc<dyn Platform>, config: Config, with_git: bool) -> EngineConfig {
    let mut cfg = EngineConfig::new(config, platform);
    // Temp dirs are tmpfs on some machines; the D-22 volume policy is tested in scull-platform.
    cfg.allow_network_roots = true;
    if with_git {
        cfg.git = Some(GitRunner::new().expect("trusted git"));
    }
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(Tri::No)));
    cfg
}

fn engine(platform: Arc<dyn Platform>, config: Config, with_git: bool) -> Engine {
    Engine::new(engine_cfg(platform, config, with_git)).unwrap()
}

fn native() -> Arc<dyn Platform> {
    Arc::new(Native)
}

fn scan(e: &Engine, root: &Path) -> ScanReport {
    e.scan(
        &[root.to_path_buf()],
        &ScanRequest::full()
            .history(false)
            .at(now_unix() + THREE_DAYS),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap()
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn at<'a>(r: &'a ScanReport, root: &Path, rel_path: &str) -> &'a Candidate {
    let hits: Vec<&Candidate> = r
        .candidates
        .iter()
        .filter(|c| rel(root, &c.path.to_path_buf()) == rel_path)
        .collect();
    let all: Vec<String> = r
        .candidates
        .iter()
        .map(|c| rel(root, &c.path.to_path_buf()))
        .collect();
    assert_eq!(hits.len(), 1, "{rel_path}: {all:#?}");
    hits[0]
}

fn codes(c: &Candidate) -> Vec<ReasonCode> {
    c.reasons.iter().map(|r| r.code).collect()
}

fn at_least(c: &Candidate, min: Tier) {
    assert!(
        c.tier >= min,
        "observed tier {} (want >= {min}) with action {:?} for {}\n{c:#?}",
        c.tier,
        c.action,
        c.path
    );
}

fn unavailable(c: &Candidate, signal: &str) -> bool {
    c.evidence
        .iter()
        .any(|e| matches!(e, Evidence::SignalUnavailable { signal: s, .. } if s == signal))
}

fn disclosed(r: &ScanReport, signal: &str) -> bool {
    r.signals_unavailable.iter().any(|s| s.signal == signal)
}

/// I-1 (an S3 item is never actionable; every action fits its tier), I-2 and I-3.
fn assert_read_only(fx: &Fixture, candidates: &[Candidate]) {
    for c in candidates {
        assert!(c.is_consistent(), "{c:#?}");
        if c.tier == Tier::S3 {
            assert_eq!(c.action, Action::None, "S3 must not be actionable: {c:#?}");
        }
    }
    let plan: Vec<PlanEntry> = candidates
        .iter()
        .filter(|c| c.action != Action::None)
        .map(PlanEntry::from)
        .collect();
    fx.check_invariants(&InvariantInputs::new().plan(plan))
        .assert_ok();
}

/// `zone/repo` (one commit) with a clean linked worktree `zone/repo-wt` on a branch at the
/// same commit (merged into the default branch: S1 "clean" by GIT-03).
fn worktree_fixture(protect_marker: bool) -> Fixture {
    let mut fx = Fixture::new().unwrap();
    let repo = fx.git_repo("zone/repo").unwrap();
    fx.file("zone/repo/README.md", "hello\n").unwrap();
    repo.commit_all("initial").unwrap();
    repo.run(["worktree", "add", "--quiet", "-b", "feature", "../repo-wt"])
        .unwrap();
    if protect_marker {
        fx.file("zone/.scull-protect", "").unwrap();
    }
    fx.checkpoint().unwrap();
    fx
}

// ---------------------------------------------------------------------------------------------
// Gap 1: worktree candidates go through the engine-side protections
// ---------------------------------------------------------------------------------------------

#[test]
fn gap1_baseline_clean_worktree_is_s1() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    let fx = worktree_fixture(false);
    let root = canon(fx.root());
    let r = scan(&engine(native(), Config::default(), true), &root);
    let wt = at(&r, &root, "zone/repo-wt");
    assert_eq!(wt.category, Category::Worktree);
    assert_eq!(wt.tier, Tier::S1, "{wt:#?}");
    assert_eq!(wt.action, Action::GitWorktreeRemove);
    assert_read_only(&fx, &r.candidates);
}

#[test]
fn gap1_worktree_under_scull_protect_is_s3() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    let fx = worktree_fixture(true);
    let root = canon(fx.root());
    let r = scan(&engine(native(), Config::default(), true), &root);
    let wt = at(&r, &root, "zone/repo-wt");
    assert_eq!(wt.tier, Tier::S3, "{wt:#?}");
    assert_eq!(wt.action, Action::None);
    assert!(codes(wt).contains(&ReasonCode::ProtectMarker), "{wt:#?}");
    let report = r
        .worktrees
        .iter()
        .find(|w| rel(&root, &w.path.to_path_buf()) == "zone/repo-wt")
        .expect("worktree reported");
    assert_eq!(
        report.decision.tier,
        Tier::S3,
        "report agrees with candidate"
    );
    assert_eq!(report.decision.action, Action::None);
    assert_read_only(&fx, &r.candidates);
}

#[test]
fn gap1_worktree_in_scan_exclude_is_s3() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    let fx = worktree_fixture(false);
    let root = canon(fx.root());
    let mut config = Config::default();
    config
        .scan
        .exclude
        .insert(root.join("zone").join("repo-wt"));
    let r = scan(&engine(native(), config, true), &root);
    let wt = at(&r, &root, "zone/repo-wt");
    assert_eq!(wt.tier, Tier::S3, "{wt:#?}");
    assert_eq!(wt.action, Action::None);
    assert!(codes(wt).contains(&ReasonCode::UserExclusion), "{wt:#?}");
    assert_read_only(&fx, &r.candidates);
}

#[test]
fn gap1_inspect_worktree_under_scull_protect_is_s3() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    let fx = worktree_fixture(true);
    let root = canon(fx.root());
    let e = engine(native(), Config::default(), true);
    let report = e
        .inspect_worktree(&root.join("zone").join("repo-wt"))
        .unwrap();
    assert_eq!(report.decision.tier, Tier::S3, "{:#?}", report.decision);
    assert_eq!(report.decision.action, Action::None);
    assert!(
        report
            .decision
            .reasons
            .iter()
            .any(|r| r.code == ReasonCode::ProtectMarker),
        "{:#?}",
        report.decision
    );
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

// ---------------------------------------------------------------------------------------------
// Gap 2: an incomplete subtree is Unknown (>= S2), never "nothing tracked" / complete
// ---------------------------------------------------------------------------------------------

#[test]
fn gap2_unreadable_folder_inside_an_artifact_is_at_least_s2() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 1024)
        .unwrap()
        .file("app/target/release/hidden/data.bin", "?")
        .unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    let platform = Arc::new(Faulty {
        unreadable: vec![root.join("app/target/release/hidden")],
        ..Faulty::default()
    });
    let r = scan(&engine(platform, Config::default(), false), &root);
    let t = at(&r, &root, "app/target");
    at_least(t, Tier::S2);
    assert!(codes(t).contains(&ReasonCode::SignalUnavailable), "{t:#?}");
    assert!(unavailable(t, "completeness"), "{t:#?}");
    assert!(
        disclosed(&r, "completeness"),
        "{:#?}",
        r.signals_unavailable
    );
    assert!(!matches!(t.action, Action::Delete));
    assert_read_only(&fx, &r.candidates);
}

#[test]
fn gap2_unreadable_folder_hiding_tracked_files_is_not_untracked() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    // A committed file lives in a folder the scanner can't read: the scanner sees no files,
    // but Git tracks one. "No files seen" must not mean "nothing tracked".
    // (`node_modules` holds only the unreadable package folder, so the scanner counts zero
    // files in it.)
    let mut fx = Fixture::new().unwrap();
    let repo = fx.git_repo("g").unwrap();
    fx.npm_project("g/web", "web", NodeLock::Npm)
        .unwrap()
        .file("g/web/node_modules/vendored/index.js", "committed\n")
        .unwrap();
    repo.commit_all("initial").unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    let platform = Arc::new(Faulty {
        unreadable: vec![root.join("g/web/node_modules/vendored")],
        ..Faulty::default()
    });
    let r = scan(&engine(platform, Config::default(), true), &root);
    let t = at(&r, &root, "g/web/node_modules");
    at_least(t, Tier::S2);
    assert!(unavailable(t, scull_engine::GIT_SIGNAL), "{t:#?}");
    assert!(!matches!(t.action, Action::Delete));
    assert_read_only(&fx, &r.candidates);
}

// ---------------------------------------------------------------------------------------------
// Gap 3: a cloud placeholder inside a candidate is a cloud-sync signal (>= S2)
// ---------------------------------------------------------------------------------------------

#[test]
fn gap3_cloud_placeholder_inside_a_candidate_is_at_least_s2() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad", "online-only"])
        .unwrap()
        .npm_project("other", "other", NodeLock::Npm)
        .unwrap()
        .node_modules("other", &["left-pad"])
        .unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    let placeholder = root.join("web/node_modules/online-only");
    let platform = Arc::new(Faulty {
        placeholders: vec![placeholder.clone()],
        ..Faulty::default()
    });
    let r = scan(&engine(platform.clone(), Config::default(), false), &root);
    let web = at(&r, &root, "web/node_modules");
    at_least(web, Tier::S2);
    assert!(codes(web).contains(&ReasonCode::CloudSync), "{web:#?}");
    assert_eq!(web.flags.cloud_placeholder, Some(true));
    assert!(!matches!(web.action, Action::Delete));
    // Unaffected neighbour stays as it was.
    assert_eq!(at(&r, &root, "other/node_modules").tier, Tier::S0);
    // The placeholder was never listed (PLT-04).
    assert!(platform.violations.lock().unwrap().is_empty());
    assert_read_only(&fx, &r.candidates);
}

// ---------------------------------------------------------------------------------------------
// Gap 4: a failed volume list is disclosed and protective
// ---------------------------------------------------------------------------------------------

#[test]
fn gap4_volume_list_failure_is_disclosed_and_protective() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 1024)
        .unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    let mut cfg = engine_cfg(native(), Config::default(), false);
    cfg.volumes = Arc::new(NoVolumes);
    let r = scan(&Engine::new(cfg).unwrap(), &root);
    let t = at(&r, &root, "app/target");
    // A bind mount keeps the volume ID, so without the mount table "crosses a mount" is
    // unknown: the protective value of the boolean signal (rubric docs) is `true`.
    assert_eq!(t.tier, Tier::S3, "{t:#?}");
    assert_eq!(t.action, Action::None);
    assert!(codes(t).contains(&ReasonCode::CrossesMount), "{t:#?}");
    assert!(unavailable(t, "mounts"), "{t:#?}");
    assert!(disclosed(&r, "mounts"), "{:#?}", r.signals_unavailable);
    assert_read_only(&fx, &r.candidates);
}

// ---------------------------------------------------------------------------------------------
// Gap 5: unresolvable scan.protect / scan.exclude entries are disclosed and protective
// ---------------------------------------------------------------------------------------------

#[test]
fn gap5_unresolved_protect_entry_is_disclosed_and_protective() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 1024)
        .unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    // The entry as the user spelled it can't be looked up (permission denied); the walk
    // itself never uses this spelling, so the tree is scanned normally.
    let entry = root.join("web").join("..").join("web");
    let platform = Arc::new(Faulty {
        no_metadata: vec![entry.clone()],
        ..Faulty::default()
    });
    let mut config = Config::default();
    config.scan.protect.insert(entry);
    let r = scan(&engine(platform, config, false), &root);
    let web = at(&r, &root, "web/node_modules");
    assert_eq!(web.tier, Tier::S3, "{web:#?}");
    assert_eq!(web.action, Action::None);
    assert!(unavailable(web, "protection"), "{web:#?}");
    assert!(disclosed(&r, "protection"), "{:#?}", r.signals_unavailable);
    // Where the entry really is can be decided (it resolves to `web`), so a candidate that
    // can't overlap it is left alone.
    assert_eq!(at(&r, &root, "app/target").tier, Tier::S0);
    assert_read_only(&fx, &r.candidates);
}

#[cfg(unix)]
#[test]
fn gap5_undecidable_unresolved_entry_makes_every_candidate_at_least_s2() {
    use std::os::unix::fs::PermissionsExt;
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 1024)
        .unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    // `locked/inner` lives outside the scan root under a folder nobody can search: whether
    // `inner` is a link into the scanned tree can't be known.
    let outside = tempfile::tempdir().unwrap();
    let locked = outside.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let bypass = std::fs::read_dir(&locked).is_ok();
    let mut config = Config::default();
    config.scan.exclude.insert(locked.join("inner"));
    let r = (!bypass).then(|| scan(&engine(native(), config, false), &root));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let Some(r) = r else {
        eprintln!("skipping: running with permission override (root)");
        return;
    };
    let t = at(&r, &root, "app/target");
    at_least(t, Tier::S2);
    assert!(unavailable(t, "protection"), "{t:#?}");
    assert!(disclosed(&r, "protection"), "{:#?}", r.signals_unavailable);
    assert_read_only(&fx, &r.candidates);
}

// ---------------------------------------------------------------------------------------------
// Gap 6: a package cache with an unmeasured subtree is S2 without a tool command
// ---------------------------------------------------------------------------------------------

#[test]
fn gap6_cache_with_unmeasured_subtree_is_s2_without_tool_command() {
    let npm = if cfg!(windows) {
        "home/AppData/Local/npm-cache/_cacache"
    } else {
        "home/.npm/_cacache"
    };
    let mut fx = Fixture::new().unwrap();
    fx.file(format!("{npm}/index-v5/1a/2b/3c4d"), "{\"key\":\"x\"}\n")
        .unwrap()
        .file_of_size(format!("{npm}/content-v2/sha512/ab/cd/ef01"), 4096)
        .unwrap()
        .file_of_size(format!("{npm}/content-v2/sha512/ff/ee/dd00"), 4096)
        .unwrap();
    // Idle for ten days: what *was* read says "old", but part of the cache wasn't read.
    let old = SystemTime::now() - Duration::from_secs(10 * 24 * 3600);
    fx.age("home", old).unwrap();
    fx.checkpoint().unwrap();
    let home = fx.root().join("home");
    let unreadable = fx.root().join(npm).join("content-v2/sha512/ff");
    let os = scull_detectors::rules::Platform::current().unwrap();
    let env = CacheEnv::with_vars(
        &home,
        os,
        [(
            "LOCALAPPDATA",
            home.join("AppData")
                .join("Local")
                .to_string_lossy()
                .into_owned(),
        )],
    );
    let platform = Arc::new(Faulty {
        unreadable: vec![unreadable],
        ..Faulty::default()
    });
    let e = engine(platform, Config::default(), false);
    let r = e.caches(&env, 0, &CancelToken::new(), |_| {});
    assert_eq!(r.candidates.len(), 1, "{:#?}", r.candidates);
    let c = &r.candidates[0];
    assert_eq!(c.category, Category::PackageCache);
    assert_eq!(c.tier, Tier::S2, "{c:#?}");
    assert_eq!(c.action, Action::None, "no tool command at S2: {c:#?}");
    assert_read_only(&fx, &r.candidates);
}

// ---------------------------------------------------------------------------------------------
// One protection path: shared Cargo target directories too
// ---------------------------------------------------------------------------------------------

#[test]
fn shared_target_goes_through_the_same_protection_step() {
    let mut fx = Fixture::new().unwrap();
    for p in ["one", "two"] {
        fx.cargo_project(p, p)
            .unwrap()
            .file(
                format!("{p}/.cargo/config.toml"),
                "[build]\ntarget-dir = \"../shared-target\"\n",
            )
            .unwrap();
    }
    fx.file("shared-target/CACHEDIR.TAG", scull_fixtures::CACHEDIR_TAG)
        .unwrap()
        .file_of_size("shared-target/debug/one", 2048)
        .unwrap()
        .file("shared-target/debug/locked/x", "?")
        .unwrap()
        .file("shared-target/debug/.scull-protect", "")
        .unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    let platform = Arc::new(Faulty {
        unreadable: vec![root.join("shared-target/debug/locked")],
        ..Faulty::default()
    });
    let r = scan(&engine(platform, Config::default(), false), &root);
    let shared = at(&r, &root, "shared-target");
    assert_eq!(shared.rule.id, "rust.shared-target");
    // A marker *inside* it, and a folder inside it that couldn't be read.
    assert_eq!(shared.tier, Tier::S3, "{shared:#?}");
    assert!(codes(shared).contains(&ReasonCode::ProtectMarker));
    assert!(unavailable(shared, "completeness"), "{shared:#?}");
    assert_read_only(&fx, &r.candidates);
}
