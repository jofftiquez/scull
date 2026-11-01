//! End-to-end tests of the read-only pipeline over generated fixture trees (docs/FIXTURES.md):
//! tiers per FX-CLS-*, Git worktrees, repository policies, history, cancellation, and the
//! universal invariants (I-1: no S3 item is actionable; I-2/I-3: the scan changes nothing).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::config::Config;
use scull_core::{Action, Candidate, Category, Evidence, ReasonCode, Tier};
use scull_detectors::CacheEnv;
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, FixedInUse, HistoryOutcome, IN_USE_SIGNAL, InUseSource,
    ScanReport, ScanRequest, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs, NodeLock, PlanEntry};
use scull_git::GitRunner;
use scull_platform::Native;
use scull_safety::Tri;
use scull_storage::{DefaultPermissions, Store};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const THREE_DAYS: i64 = 3 * 24 * 3600;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The mixed fixture: Cargo, npm (with and without lockfile), a pnpm workspace, a
/// `.scull-protect` project, a `target/` without any Cargo marker, and (when `git` exists) a
/// repository with an ignored and a committed `target/`, a committed `dist/` and a linked
/// worktree.
fn mixed(with_git: bool) -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 4096)
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .npm_project("nolock", "nolock", NodeLock::None)
        .unwrap()
        .node_modules("nolock", &["x"])
        .unwrap()
        .pnpm_workspace("mono", "mono", &["a", "b"])
        .unwrap()
        .node_modules("mono", &["typescript"])
        .unwrap()
        .node_modules("mono/packages/a", &["lodash"])
        .unwrap()
        .cargo_project("guarded", "guarded")
        .unwrap()
        .cargo_target("guarded", "guarded", 1024)
        .unwrap()
        .file("guarded/.scull-protect", "")
        .unwrap()
        .file("photos/target/IMG_0001.jpg", "jpeg bytes")
        .unwrap();
    if with_git {
        let repo = fx.git_repo("repo").unwrap();
        fx.file("repo/.gitignore", "node_modules/\n/app/target/\n")
            .unwrap()
            .cargo_project("repo/app", "rapp")
            .unwrap()
            .cargo_target("repo/app", "rapp", 2048)
            .unwrap()
            .cargo_project("repo/tracked", "tracked")
            .unwrap()
            .cargo_target("repo/tracked", "tracked", 512)
            .unwrap()
            .npm_project("repo/web", "rweb", NodeLock::Npm)
            .unwrap()
            .node_modules("repo/web", &["react"])
            .unwrap()
            .file("repo/web/dist/index.js", "built\n")
            .unwrap();
        repo.commit_all("initial").unwrap();
        repo.run(["worktree", "add", "--quiet", "-b", "feature", "../repo-wt"])
            .unwrap();
    }
    fx.checkpoint().unwrap();
    fx
}

fn engine(in_use: Option<Tri>, with_git: bool, config: Config) -> Engine {
    let mut cfg = EngineConfig::new(config, Arc::new(Native));
    // Temp dirs are tmpfs on some machines; the D-22 volume policy is tested in scull-platform.
    cfg.allow_network_roots = true;
    if with_git {
        cfg.git = Some(GitRunner::new().expect("trusted git"));
    }
    cfg.in_use = match in_use {
        Some(t) => InUseSource::Probe(Arc::new(FixedInUse(t))),
        None => InUseSource::Unavailable,
    };
    Engine::new(cfg).unwrap()
}

fn scan(e: &Engine, root: &Path, now: i64) -> ScanReport {
    e.scan(
        &[root.to_path_buf()],
        &ScanRequest::full().history(false).at(now),
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
    assert_eq!(hits.len(), 1, "{rel_path}: {:#?}", paths(r, root));
    hits[0]
}

fn paths(r: &ScanReport, root: &Path) -> Vec<String> {
    let mut v: Vec<String> = r
        .candidates
        .iter()
        .map(|c| rel(root, &c.path.to_path_buf()))
        .collect();
    v.sort();
    v
}

fn codes(c: &Candidate) -> Vec<ReasonCode> {
    c.reasons.iter().map(|r| r.code).collect()
}

/// I-1 (no S3 in an executable plan: every actionable candidate is offered), I-2 and I-3 (the
/// scan changed nothing at all).
fn assert_read_only(fx: &Fixture, r: &ScanReport) {
    for c in &r.candidates {
        assert!(c.is_consistent(), "{c:#?}");
        if c.tier == Tier::S3 {
            assert_eq!(c.action, Action::None, "S3 must not be actionable: {c:#?}");
        }
    }
    let plan: Vec<PlanEntry> = r
        .candidates
        .iter()
        .filter(|c| c.action != Action::None)
        .map(PlanEntry::from)
        .collect();
    fx.check_invariants(&InvariantInputs::new().plan(plan))
        .assert_ok();
}

#[test]
fn fx_cls_fixture_tiers_with_idle_signals() {
    let with_git = git_available();
    let fx = mixed(with_git);
    let root = canon(fx.root());
    let e = engine(Some(Tri::No), with_git, Config::default());
    let r = scan(&e, &root, now_unix() + THREE_DAYS);

    // FX-CLS-01: idle target with marker and lockfile evidence.
    let app = at(&r, &root, "app/target");
    assert_eq!(app.tier, Tier::S0, "{app:#?}");
    assert!(
        app.evidence
            .iter()
            .any(|e| matches!(e, Evidence::Marker { .. }))
    );
    assert!(
        app.evidence
            .iter()
            .any(|e| matches!(e, Evidence::Lockfile { .. }))
    );
    assert!(
        app.sizes.allocated > 0 || app.sizes.logical > 4000,
        "{app:#?}"
    );
    assert_eq!(app.action, Action::Delete);
    // FX-CLS-05: with a lockfile S0, without S2.
    assert_eq!(at(&r, &root, "web/node_modules").tier, Tier::S0);
    let nolock = at(&r, &root, "nolock/node_modules");
    assert_eq!(nolock.tier, Tier::S2);
    assert!(codes(nolock).contains(&ReasonCode::NoLockfile));
    // FX-CLS-08: every workspace node_modules found once.
    assert_eq!(at(&r, &root, "mono/node_modules").tier, Tier::S0);
    assert_eq!(at(&r, &root, "mono/packages/a/node_modules").tier, Tier::S0);
    // FX-CLS-11: everything inside a `.scull-protect` project is S3.
    let guarded = at(&r, &root, "guarded/target");
    assert_eq!(guarded.tier, Tier::S3);
    assert!(codes(guarded).contains(&ReasonCode::ProtectMarker));
    // FX-CLS-02: a folder named `target/` without a Cargo marker is not a candidate.
    assert!(!paths(&r, &root).iter().any(|p| p.starts_with("photos")));
    // Nothing inside a detected artifact is detected again.
    assert!(
        !paths(&r, &root)
            .iter()
            .any(|p| p.contains("node_modules/") || p.contains("target/"))
    );
    // One candidate per item.
    let mut ids: Vec<_> = r.candidates.iter().map(|c| c.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), r.candidates.len());

    if with_git {
        // Ignored target inside a repository: not tracked, S0.
        assert_eq!(at(&r, &root, "repo/app/target").tier, Tier::S0);
        // FX-CLS-03: committed build output is S3 "git-tracked" and never actionable; a
        // committed `dist/` (no rule claims it) is not a candidate at all.
        let tracked = at(&r, &root, "repo/tracked/target");
        assert_eq!(tracked.tier, Tier::S3, "{tracked:#?}");
        assert!(codes(tracked).contains(&ReasonCode::GitTracked));
        assert_eq!(tracked.action, Action::None);
        assert!(!paths(&r, &root).iter().any(|p| p.ends_with("dist")));
        // The linked worktree is a candidate, never S0 (GIT-04); the main one is reported only.
        let wt = at(&r, &root, "repo-wt");
        assert_eq!(wt.category, Category::Worktree);
        assert!(wt.tier >= Tier::S1, "{wt:#?}");
        let main = r
            .worktrees
            .iter()
            .find(|w| rel(&root, &w.path.to_path_buf()) == "repo")
            .expect("main worktree reported");
        assert_eq!(main.kind, scull_git::WorktreeKind::Main);
        assert!(main.candidate.is_none());
        assert_eq!(r.worktrees.len(), 2, "{:#?}", r.worktrees);
    }

    // Totals add up to the candidates.
    let s0: u64 = r
        .candidates
        .iter()
        .filter(|c| c.tier == Tier::S0 && c.category != Category::Worktree)
        .map(|c| c.sizes.allocated)
        .sum();
    assert_eq!(r.totals.allocated.s0, s0);
    // Projects: markers and candidates.
    let app_project = r
        .projects
        .iter()
        .find(|p| rel(&root, &p.root.to_path_buf()) == "app")
        .expect("app project");
    assert_eq!(app_project.ecosystems, ["rust"]);
    assert_eq!(app_project.candidates, 1);
    assert_eq!(app_project.badge, Some(Tier::S0));
    // Every signal was available for this tree. On Windows a built-in protected root that
    // refuses even an attribute-only open (`C:\System Volume Information`) is still disclosed
    // run-wide, but its location is decided, so it affects no candidate here.
    assert!(
        r.signals_unavailable
            .iter()
            .all(|g| cfg!(windows) && g.signal == "protection" && g.affected == 0),
        "{:#?}",
        r.signals_unavailable
    );
    assert!(!r.cancelled);
    assert_read_only(&fx, &r);
}

#[test]
fn fx_cls_09_recent_activity_bumps_one_tier() {
    let fx = mixed(false);
    let root = canon(fx.root());
    let e = engine(Some(Tri::No), false, Config::default());
    // Everything was just written: within the 24 h window.
    let r = scan(&e, &root, now_unix());
    let app = at(&r, &root, "app/target");
    assert_eq!(app.tier, Tier::S1, "{app:#?}");
    assert!(codes(app).contains(&ReasonCode::RecentActivity));
    assert_read_only(&fx, &r);
}

#[test]
fn without_in_use_probe_everything_is_at_least_s2_and_disclosed() {
    let fx = mixed(false);
    let root = canon(fx.root());
    let e = engine(None, false, Config::default());
    let r = scan(&e, &root, now_unix() + THREE_DAYS);
    assert!(!r.candidates.is_empty());
    for c in &r.candidates {
        assert!(c.tier >= Tier::S2, "{c:#?}");
        assert!(c.evidence.iter().any(
            |e| matches!(e, Evidence::SignalUnavailable { signal, .. } if signal == IN_USE_SIGNAL)
        ));
    }
    let s = r
        .signals_unavailable
        .iter()
        .find(|s| s.signal == IN_USE_SIGNAL)
        .expect("in_use disclosed");
    assert_eq!(s.affected, r.candidates.len() as u64);
    assert_read_only(&fx, &r);
}

#[test]
fn fx_cfg_04_user_protect_and_min_tier_are_honored() {
    let fx = mixed(false);
    let root = canon(fx.root());
    let mut config = Config::default();
    config.scan.protect.insert(root.join("web"));
    config
        .scan
        .min_tier
        .insert(root.join("mono").join("packages"), Tier::S2);
    let e = engine(Some(Tri::No), false, config);
    let r = scan(&e, &root, now_unix() + THREE_DAYS);
    let web = at(&r, &root, "web/node_modules");
    assert_eq!(web.tier, Tier::S3);
    assert!(codes(web).contains(&ReasonCode::ProtectedRoot));
    assert_eq!(at(&r, &root, "mono/packages/a/node_modules").tier, Tier::S2);
    assert_eq!(at(&r, &root, "mono/node_modules").tier, Tier::S0);
    assert_read_only(&fx, &r);
}

#[test]
fn fx_cfg_01_03_repo_policy_only_adds_protection() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("a", "a")
        .unwrap()
        .cargo_target("a", "a", 64)
        .unwrap()
        .file(
            "a/.scull.toml",
            "[scan]\nmin_tier = \"S2\"\nroots = [\"/\"]\n[scan.min_tier_extra]\n",
        )
        .unwrap()
        .cargo_project("b", "b")
        .unwrap()
        .cargo_target("b", "b", 64)
        .unwrap()
        .file(
            "b/.scull.toml",
            "[scan]\nmin_tier = \"S0\"\nprotect = [\"target\"]\n",
        )
        .unwrap()
        .cargo_project("c", "c")
        .unwrap()
        .cargo_target("c", "c", 64)
        .unwrap()
        .file("c/.scull.toml", "this is = = not toml")
        .unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    let e = engine(Some(Tri::No), false, Config::default());
    let r = scan(&e, &root, now_unix() + THREE_DAYS);
    // FX-CFG-03: protection added by the repository is honored.
    assert_eq!(at(&r, &root, "a/target").tier, Tier::S2);
    assert_eq!(at(&r, &root, "b/target").tier, Tier::S3);
    assert_eq!(
        at(&r, &root, "c/target").tier,
        Tier::S3,
        "invalid TOML protects"
    );
    // FX-CFG-01/02: lowering a tier or adding a root is ignored with a warning.
    let warned: Vec<String> = r
        .config_warnings
        .iter()
        .map(|w| w.warning.key.clone())
        .collect();
    assert!(warned.iter().any(|k| k == "scan.roots"), "{warned:?}");
    assert!(
        warned.iter().any(|k| k.starts_with("scan.min_tier")),
        "{warned:?}"
    );
    assert_read_only(&fx, &r);
}

#[test]
fn fx_cls_04_shared_target_counted_once() {
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
        .file_of_size("shared-target/debug/two", 2048)
        .unwrap();
    fx.checkpoint().unwrap();
    let root = canon(fx.root());
    let e = engine(Some(Tri::No), false, Config::default());
    let r = scan(&e, &root, now_unix() + THREE_DAYS);
    let shared = at(&r, &root, "shared-target");
    assert_eq!(shared.rule.id, "rust.shared-target");
    assert!(shared.sizes.logical >= 4096, "{shared:#?}");
    assert_eq!(r.candidates.len(), 1, "{:#?}", paths(&r, &root));
    assert_eq!(r.totals.logical.total(), shared.sizes.logical);
    assert_read_only(&fx, &r);
}

#[test]
fn cancellation_returns_quickly_and_marks_the_report() {
    let fx = mixed(false);
    let root = canon(fx.root());
    let e = engine(Some(Tri::No), false, Config::default());
    let cancel = CancelToken::new();
    cancel.cancel();
    let t = Instant::now();
    let r = e
        .scan(
            std::slice::from_ref(&root),
            &ScanRequest::full().history(false),
            &cancel,
            |_| {},
        )
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(r.cancelled);
    assert!(r.worktrees.is_empty());
    assert_read_only(&fx, &r);
}

#[test]
fn history_is_recorded_and_a_cancelled_scan_marks_nothing_gone() {
    let fx = mixed(false);
    let root = canon(fx.root());
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("scull.db");
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    cfg.allow_network_roots = true;
    cfg.store = Some(Store::open(&db, &DefaultPermissions).unwrap());
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(Tri::No)));
    let e = Engine::new(cfg).unwrap();
    let r = e
        .scan(
            std::slice::from_ref(&root),
            &ScanRequest::full(),
            &CancelToken::new(),
            |_| {},
        )
        .unwrap();
    let HistoryOutcome::Recorded { new_items, .. } = r.history else {
        panic!("not recorded: {:?}", r.history);
    };
    assert_eq!(new_items, r.candidates.len() as u64);

    let cancel = CancelToken::new();
    cancel.cancel();
    let r2 = e
        .scan(
            std::slice::from_ref(&root),
            &ScanRequest::full(),
            &cancel,
            |_| {},
        )
        .unwrap();
    assert!(r2.cancelled);
    assert_eq!(
        r2.history,
        HistoryOutcome::Recorded {
            new_items: 0,
            disappeared: 0
        }
    );
    // Disabled history writes nothing.
    let r3 = e
        .scan(
            std::slice::from_ref(&root),
            &ScanRequest::full().history(false),
            &CancelToken::new(),
            |_| {},
        )
        .unwrap();
    assert_eq!(r3.history, HistoryOutcome::Disabled);
    assert_read_only(&fx, &r);
}

#[test]
fn missing_root_is_an_error() {
    let e = engine(None, false, Config::default());
    let err = e
        .scan(
            &[PathBuf::from("/definitely/not/here/scull")],
            &ScanRequest::full().history(false),
            &CancelToken::new(),
            |_| {},
        )
        .unwrap_err();
    assert_eq!(err.kind, scull_core::ErrorKind::NotFound);
}

#[test]
fn caches_are_located_sized_and_classified() {
    let mut fx = Fixture::new().unwrap();
    fx.file(
        "home/.npm/_cacache/index-v5/1a/2b/3c4d",
        "{\"key\":\"x\"}\n",
    )
    .unwrap()
    .file_of_size("home/.npm/_cacache/content-v2/sha512/ab/cd/ef01", 4096)
    .unwrap();
    fx.checkpoint().unwrap();
    let home = fx.root().join("home");
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
    let e = engine(Some(Tri::No), false, Config::default());
    let r = e.caches(&env, 0, &CancelToken::new(), |_| {});
    assert!(!r.cancelled);
    if cfg!(windows) {
        // npm's Windows default is %LocalAppData%\npm-cache, which this fixture doesn't have.
        assert!(r.candidates.is_empty(), "{:#?}", r.candidates);
    } else {
        assert_eq!(r.candidates.len(), 1, "{:#?}", r.candidates);
        let c = &r.candidates[0];
        assert_eq!(c.category, Category::PackageCache);
        assert!(c.tier >= Tier::S1, "{c:#?}");
        assert!(c.sizes.logical >= 4096, "{c:#?}");
        assert_eq!(r.totals.count.total(), 1);
    }
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn native_in_use_probe_classifies_protectively() {
    let fx = mixed(false);
    let root = canon(fx.root());
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Native(scull_safety::GapPolicy::default());
    let e = Engine::new(cfg).unwrap();
    let r = scan(&e, &root, now_unix() + THREE_DAYS);
    // Whatever this machine lets us see, the answer is never lower than with an idle probe,
    // and anything not fully observed is disclosed.
    let app = at(&r, &root, "app/target");
    let observed_idle = !app.evidence.iter().any(|e| {
        matches!(
            e,
            Evidence::SignalUnavailable { .. } | Evidence::InUse { .. }
        )
    });
    if observed_idle {
        assert_eq!(app.tier, Tier::S0, "{app:#?}");
    } else {
        assert!(app.tier >= Tier::S2, "{app:#?}");
    }
    assert_read_only(&fx, &r);
}

#[test]
fn candidates_in_synced_folders_are_at_least_s2() {
    use scull_platform::{CloudProvider, SyncMechanism, SyncedRoot, SyncedRoots};
    let fx = mixed(false);
    let root = canon(fx.root());
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(Tri::No)));
    // `web` is a synced folder (found un-canonicalized, as SyncedRoots::detect reports it).
    cfg.synced_roots = SyncedRoots::from_roots(vec![SyncedRoot::new(
        fx.root().join("web"),
        CloudProvider::Dropbox,
        SyncMechanism::Mirror,
    )]);
    let e = Engine::new(cfg).unwrap();
    let r = scan(&e, &root, now_unix() + THREE_DAYS);
    let web = at(&r, &root, "web/node_modules");
    assert_eq!(web.tier, Tier::S2, "{web:#?}");
    assert!(codes(web).contains(&ReasonCode::CloudSync));
    assert_eq!(web.flags.cloud_placeholder, Some(true));
    assert_eq!(at(&r, &root, "app/target").tier, Tier::S0);
    assert_read_only(&fx, &r);
}
