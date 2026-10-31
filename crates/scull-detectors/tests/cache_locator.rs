//! Package cache locator fixtures (P1-DET-04): FX-QC-01, -02, -03, -04, -07, plus every OS
//! layout, environment and config overrides, and QC-05 edges.
//!
//! Every test builds a fake home folder with `scull-fixtures` in a temp dir and passes it, a
//! fake environment and the OS layout to resolve through `CacheEnv`, so all three OS layouts are
//! checked on every CI runner. Nothing reads the real `$HOME` or environment, and no package
//! manager is ever run. The universal invariants (I-2 canary, I-3 tree unchanged, I-1 on the
//! would-be plan) are checked after each test.

#![allow(clippy::unwrap_used)]

mod common;

use common::{StdReader, check, identity_available, plan_inputs, rel};
use scull_core::{
    Action, Candidate, Category, Evidence, PlanId, PlanItem, PlanOrigin, PlanParams, ReasonCode,
    ScanId, Tier,
};
use scull_detectors::caches::{CacheProblem, CacheScan, CacheSource, CacheTool};
use scull_detectors::rules::Platform as Os;
use scull_detectors::{CacheEnv, CallerSignals, DetectContext, LocatedCache, locate_caches};
use scull_fixtures::{Fixture, LinkOutcome};
use scull_platform::{Capabilities, DirIter, Metadata, Native, Platform};
use scull_safety::{GitTracking, Tri};
use scull_scanner::{CancelToken, DirSummary, ScanEvent, ScanOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const ALL_OS: [Os; 3] = [Os::Linux, Os::Macos, Os::Windows];

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn home(fx: &Fixture) -> PathBuf {
    fx.root().join("home")
}

fn env(fx: &Fixture, os: Os, vars: &[(&str, String)]) -> CacheEnv {
    CacheEnv::with_vars(home(fx), os, vars.iter().map(|(k, v)| (*k, v.clone())))
}

fn abs(fx: &Fixture, rel_path: &str) -> String {
    fx.path(rel_path).unwrap().to_string_lossy().into_owned()
}

fn locate_on(env: &CacheEnv, platform: &dyn Platform) -> CacheScan {
    let reader = StdReader;
    locate_caches(
        env,
        platform,
        &DetectContext::new().with_config_reader(&reader),
    )
}

fn locate(env: &CacheEnv) -> CacheScan {
    locate_on(env, &Native::new())
}

/// Caller signals on a clean, idle machine. Recent activity is `Yes` on purpose: caches are
/// touched by every install and that must not raise their tier.
fn idle() -> CallerSignals {
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

/// `t`, or S3 where the platform reports no file identity (can't be revalidated, SAFE-05).
fn expected(t: Tier) -> Tier {
    if identity_available() { t } else { Tier::S3 }
}

fn found_rel(fx: &Fixture, scan: &CacheScan) -> Vec<(CacheTool, String)> {
    scan.found
        .iter()
        .map(|c| (c.tool, rel(fx.root(), &c.path)))
        .collect()
}

fn the(scan: &CacheScan, tool: CacheTool) -> &LocatedCache {
    let hits: Vec<_> = scan.found.iter().filter(|c| c.tool == tool).collect();
    assert_eq!(
        hits.len(),
        1,
        "exactly one {tool} cache in {:?}",
        scan.found
    );
    hits[0]
}

fn notes(c: &Candidate) -> Vec<&str> {
    c.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn codes(c: &Candidate) -> Vec<ReasonCode> {
    c.reasons.iter().map(|r| r.code).collect()
}

fn tool_action(tool: &str, args: &[&str]) -> Action {
    Action::ToolCommand {
        tool: tool.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    }
}

fn plan_params() -> PlanParams {
    PlanParams {
        id: PlanId::from_suffix("01J").unwrap(),
        scan_id: ScanId::from_suffix("01J").unwrap(),
        created_unix: 1_800_000_000,
        ttl_secs: None,
        machine_id: "machine-a".into(),
        user: "me".into(),
        origin: PlanOrigin::Cli,
    }
}

/// Scan `dir` with the real scanner and return its own summary.
fn summarize(dir: &Path) -> DirSummary {
    let out: Arc<Mutex<Vec<DirSummary>>> = Arc::default();
    let sink = {
        let out = Arc::clone(&out);
        move |e: ScanEvent| {
            if let ScanEvent::DirDone(s) = e {
                out.lock().unwrap().push(s);
            }
        }
    };
    scull_scanner::scan(
        Arc::new(Native::new()),
        dir,
        &ScanOptions::default(),
        &CancelToken::new(),
        sink,
    )
    .unwrap();
    let all = out.lock().unwrap();
    all.iter().find(|s| s.path == dir).cloned().unwrap()
}

// --- cache layouts as the tools write them --------------------------------------------------

fn npm_cache(fx: &mut Fixture, cacache: &str) {
    fx.file(
        format!("{cacache}/index-v5/1a/2b/3c4d"),
        "{\"key\":\"x\"}\n",
    )
    .unwrap()
    .file_of_size(format!("{cacache}/content-v2/sha512/ab/cd/ef01"), 4096)
    .unwrap();
}

fn pnpm_store(fx: &mut Fixture, store: &str, versions: &[&str]) {
    for v in versions {
        fx.file_of_size(format!("{store}/{v}/files/00/abcdef"), 4096)
            .unwrap()
            .file(format!("{store}/{v}/index/00/abcdef-index.json"), "{}")
            .unwrap();
    }
}

fn bun_cache(fx: &mut Fixture, dir: &str) {
    fx.file(format!("{dir}/left-pad@1.3.0@@@1/package.json"), "{}")
        .unwrap()
        .file(format!("{dir}/a1b2c3.npm"), "manifest")
        .unwrap();
}

fn yarn_classic(fx: &mut Fixture, dir: &str) {
    fx.file(
        format!("{dir}/v6/npm-left-pad-1.3.0-abc/node_modules/left-pad/package.json"),
        "{}",
    )
    .unwrap();
}

fn yarn_berry(fx: &mut Fixture, dir: &str) {
    fx.file_of_size(format!("{dir}/left-pad-npm-1.3.0-abc123-10c0.zip"), 2048)
        .unwrap();
}

fn pip_cache(fx: &mut Fixture, dir: &str) {
    fx.file_of_size(format!("{dir}/http-v2/a/b/c/d/e/abcdef.body"), 2048)
        .unwrap()
        .file_of_size(
            format!("{dir}/wheels/ab/cd/ef/requests-2.32.3-py3-none-any.whl"),
            1024,
        )
        .unwrap();
}

fn uv_cache(fx: &mut Fixture, dir: &str) {
    fx.file(format!("{dir}/CACHEDIR.TAG"), scull_fixtures::CACHEDIR_TAG)
        .unwrap()
        .file(format!("{dir}/.gitignore"), "*\n")
        .unwrap()
        .file_of_size(
            format!("{dir}/archive-v0/abc123/requests/__init__.py"),
            2048,
        )
        .unwrap()
        .file(format!("{dir}/simple-v16/pypi/requests.rkyv"), "index")
        .unwrap()
        .file(
            format!("{dir}/wheels-v5/pypi/requests/2.32.3-py3-none-any.msgpack"),
            "meta",
        )
        .unwrap();
}

fn conda_pkgs(fx: &mut Fixture, dir: &str) {
    fx.file(
        format!("{dir}/urls.txt"),
        "https://conda.anaconda.org/conda-forge/linux-64/numpy-2.1.2-py312h.conda\n",
    )
    .unwrap()
    .file_of_size(format!("{dir}/numpy-2.1.2-py312h.conda"), 4096)
    .unwrap()
    .file(format!("{dir}/numpy-2.1.2-py312h/info/index.json"), "{}")
    .unwrap();
}

fn build(fx: &mut Fixture, tool: CacheTool, dir: &str) {
    match tool {
        CacheTool::Npm => npm_cache(fx, dir),
        CacheTool::Pnpm => pnpm_store(fx, dir, &["v3", "v10"]),
        CacheTool::Bun => bun_cache(fx, dir),
        CacheTool::YarnClassic => yarn_classic(fx, dir),
        CacheTool::YarnBerry => yarn_berry(fx, dir),
        CacheTool::Pip => pip_cache(fx, dir),
        CacheTool::Uv => uv_cache(fx, dir),
        CacheTool::Conda => conda_pkgs(fx, dir),
    }
}

/// Default cache folders per OS, relative to the fixture root (home is `home/`, no env).
fn default_layout(os: Os) -> Vec<(CacheTool, &'static str)> {
    match os {
        Os::Linux => vec![
            (CacheTool::Npm, "home/.npm/_cacache"),
            (CacheTool::Pnpm, "home/.local/share/pnpm/store"),
            (CacheTool::Bun, "home/.bun/install/cache"),
            (CacheTool::YarnClassic, "home/.cache/yarn"),
            (CacheTool::YarnBerry, "home/.yarn/berry/cache"),
            (CacheTool::Pip, "home/.cache/pip"),
            (CacheTool::Uv, "home/.cache/uv"),
            (CacheTool::Conda, "home/miniconda3/pkgs"),
        ],
        Os::Macos => vec![
            (CacheTool::Npm, "home/.npm/_cacache"),
            (CacheTool::Pnpm, "home/Library/pnpm/store"),
            (CacheTool::Bun, "home/.bun/install/cache"),
            (CacheTool::YarnClassic, "home/Library/Caches/Yarn"),
            (CacheTool::YarnBerry, "home/.yarn/berry/cache"),
            (CacheTool::Pip, "home/Library/Caches/pip"),
            (CacheTool::Uv, "home/.cache/uv"),
            (CacheTool::Conda, "home/miniconda3/pkgs"),
        ],
        Os::Windows => vec![
            (CacheTool::Npm, "home/AppData/Local/npm-cache/_cacache"),
            (CacheTool::Pnpm, "home/AppData/Local/pnpm/store"),
            (CacheTool::Bun, "home/.bun/install/cache"),
            (CacheTool::YarnClassic, "home/AppData/Local/Yarn/Cache"),
            (CacheTool::YarnBerry, "home/AppData/Local/Yarn/Berry/cache"),
            (CacheTool::Pip, "home/AppData/Local/pip/Cache"),
            (CacheTool::Uv, "home/AppData/Local/uv/cache"),
            (CacheTool::Conda, "home/miniconda3/pkgs"),
        ],
    }
}

/// FX-QC-01 for one OS layout: every tool's default cache is found, validated and becomes an
/// S1 candidate with the preferred native command; the other OSs' folders are ignored.
fn default_layout_for(os: Os) {
    let mut fx = Fixture::new().unwrap();
    let mine = default_layout(os);
    for (tool, dir) in &mine {
        build(&mut fx, *tool, dir);
    }
    // Decoys: the other OSs' locations must not be picked up for this layout.
    for other in ALL_OS.into_iter().filter(|o| *o != os) {
        for (tool, dir) in default_layout(other) {
            if !mine.iter().any(|(_, d)| *d == dir) {
                build(&mut fx, tool, dir);
            }
        }
    }
    fx.checkpoint().unwrap();

    let scan = locate(&env(&fx, os, &[]));
    assert!(scan.refused.is_empty(), "{:?}", scan.refused);
    let want: Vec<(CacheTool, String)> = mine.iter().map(|(t, d)| (*t, (*d).to_owned())).collect();
    assert_eq!(found_rel(&fx, &scan), want, "{os:?} layout");

    let want_len = scan.found.len();
    assert_eq!(want_len, CacheTool::ALL.len(), "every tool found");
    let mut candidates = Vec::new();
    for c in scan.found {
        assert!(c.recognized, "{} layout recognized", c.tool);
        assert!(c.problems.is_empty(), "{:?}", c.problems);
        assert_eq!(c.source, CacheSource::Default);
        let tool = c.tool;
        let cand = c.into_candidate(&idle(), None);
        assert!(cand.is_consistent());
        assert_eq!(cand.category, Category::PackageCache);
        assert_eq!(cand.rule.id, tool.rule_id());
        assert_eq!(cand.tier, expected(Tier::S1), "{tool} is S1 (QC-02)");
        assert!(codes(&cand).contains(&ReasonCode::PackageCache));
        assert_eq!(
            cand.regeneration.instructions.as_deref(),
            Some(scull_detectors::caches::NOT_UNDOABLE_NOTE)
        );
        if identity_available() {
            let preferred = scull_detectors::commands_for_rule(tool.rule_id())
                .unwrap()
                .preferred
                .to_action();
            assert_eq!(cand.action, preferred);
            assert!(PlanItem::from_candidate(&cand).is_ok());
        }
        candidates.push(cand);
    }
    if identity_available() {
        let items: Vec<PlanItem> = candidates
            .iter()
            .map(|c| PlanItem::from_candidate(c).unwrap())
            .collect();
        assert_eq!(
            scull_core::Plan::new(plan_params(), items)
                .unwrap()
                .items()
                .len(),
            want_len
        );
    }
    check(&fx, &plan_inputs(&candidates));
}

// ---------------------------------------------------------------------------------------------
// FX-QC-01: caches found per OS, with overrides
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_qc_01_default_layout_linux() {
    default_layout_for(Os::Linux);
}

#[test]
fn fx_qc_01_default_layout_macos() {
    default_layout_for(Os::Macos);
}

#[test]
fn fx_qc_01_default_layout_windows() {
    default_layout_for(Os::Windows);
}

#[test]
fn fx_qc_01_npm_cache_commands_and_wipe_alternative() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "home/.npm/_cacache");
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::Npm).clone();
    assert_eq!(
        c.markers
            .iter()
            .map(|m| rel(fx.root(), m))
            .collect::<Vec<_>>(),
        [
            "home/.npm/_cacache/content-v2",
            "home/.npm/_cacache/index-v5"
        ]
    );
    let cand = c.into_candidate(&idle(), None);
    if identity_available() {
        assert_eq!(cand.action, tool_action("npm", &["cache", "verify"]));
    }
    assert!(
        notes(&cand)
            .iter()
            .any(|n| n.contains("`npm cache verify`") && n.contains("`npm cache clean --force`")),
        "wipe alternative recorded: {:?}",
        notes(&cand)
    );
    check(&fx, &plan_inputs(&[cand]));
}

#[test]
fn fx_qc_01_npm_env_beats_npmrc_beats_default() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        npm_cache(&mut fx, "elsewhere/env-cache/_cacache");
        npm_cache(&mut fx, "home/rc-cache/_cacache");
        build(&mut fx, CacheTool::Npm, default_layout(os)[0].1);
        fx.file(
            "home/.npmrc",
            "; user npmrc\nregistry=https://registry.example\ncache = \"~/rc-cache\"\n",
        )
        .unwrap();
        fx.checkpoint().unwrap();

        let e = env(
            &fx,
            os,
            &[("npm_config_cache", abs(&fx, "elsewhere/env-cache"))],
        );
        let scan = locate(&e);
        let c = the(&scan, CacheTool::Npm);
        assert_eq!(rel(fx.root(), &c.path), "elsewhere/env-cache/_cacache");
        assert_eq!(
            c.source,
            CacheSource::Env {
                var: "npm_config_cache".into()
            }
        );
        // Outside home: checked from the configured folder's parent.
        assert_eq!(rel(fx.root(), &c.anchor), "elsewhere");

        let scan = locate(&env(&fx, os, &[]));
        let c = the(&scan, CacheTool::Npm);
        assert_eq!(rel(fx.root(), &c.path), "home/rc-cache/_cacache");
        assert!(matches!(&c.source, CacheSource::Config { key, .. } if key == "cache"));
        let cand = c.clone().into_candidate(&idle(), None);
        assert_eq!(cand.tier, expected(Tier::S1));
        assert!(notes(&cand).iter().any(|n| n.contains("`cache` in")));
        check(&fx, &plan_inputs(&[cand]));
    }
}

#[test]
fn fx_qc_01_npm_userconfig_and_variable_expansion() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "data/npm/_cacache");
    fx.file("cfg/npmrc", "cache=${DATA}/npm\n").unwrap();
    // `~/.npmrc` is ignored when `npm_config_userconfig` names another file.
    fx.file("home/.npmrc", "cache=~/ignored\n").unwrap();
    fx.checkpoint().unwrap();
    let e = env(
        &fx,
        Os::Linux,
        &[
            ("npm_config_userconfig", abs(&fx, "cfg/npmrc")),
            ("DATA", abs(&fx, "data")),
        ],
    );
    let scan = locate(&e);
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::Npm).path),
        "data/npm/_cacache"
    );
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn fx_qc_01_pnpm_store_overrides() {
    // `store-dir` in the user npmrc.
    let mut fx = Fixture::new().unwrap();
    pnpm_store(&mut fx, "home/stores/pnpm", &["v10"]);
    pnpm_store(&mut fx, "home/.local/share/pnpm/store", &["v3"]);
    fx.file("home/.npmrc", "store-dir=~/stores/pnpm\n").unwrap();
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::Pnpm);
    assert_eq!(rel(fx.root(), &c.path), "home/stores/pnpm");
    assert!(c.notes.iter().any(|n| n.contains("v10")));
    // `npm_config_store_dir` beats the npmrc.
    let e = env(
        &fx,
        Os::Linux,
        &[(
            "npm_config_store_dir",
            abs(&fx, "home/.local/share/pnpm/store"),
        )],
    );
    assert_eq!(
        rel(fx.root(), &the(&locate(&e), CacheTool::Pnpm).path),
        "home/.local/share/pnpm/store"
    );
    check(&fx, &plan_inputs(&[]));

    // `PNPM_HOME`, then `XDG_DATA_HOME`, before the OS default; the legacy store is also found.
    let mut fx = Fixture::new().unwrap();
    pnpm_store(&mut fx, "pnpm-home/store", &["v10"]);
    pnpm_store(&mut fx, "xdg-data/pnpm/store", &["v10"]);
    pnpm_store(&mut fx, "home/.pnpm-store", &["v3"]);
    fx.checkpoint().unwrap();
    for os in ALL_OS {
        let e = env(
            &fx,
            os,
            &[
                ("PNPM_HOME", abs(&fx, "pnpm-home")),
                ("XDG_DATA_HOME", abs(&fx, "xdg-data")),
            ],
        );
        let got: Vec<String> = locate(&e)
            .found
            .iter()
            .filter(|c| c.tool == CacheTool::Pnpm)
            .map(|c| rel(fx.root(), &c.path))
            .collect();
        assert_eq!(got, ["pnpm-home/store", "home/.pnpm-store"], "{os:?}");
        let e = env(&fx, os, &[("XDG_DATA_HOME", abs(&fx, "xdg-data"))]);
        let first = locate(&e)
            .found
            .into_iter()
            .find(|c| c.tool == CacheTool::Pnpm)
            .unwrap();
        assert_eq!(rel(fx.root(), &first.path), "xdg-data/pnpm/store");
    }
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn fx_qc_01_bun_cache_overrides() {
    let mut fx = Fixture::new().unwrap();
    bun_cache(&mut fx, "bun-cache");
    bun_cache(&mut fx, "bun-install/install/cache");
    bun_cache(&mut fx, "home/.bun/install/cache");
    fx.checkpoint().unwrap();
    for os in ALL_OS {
        let e = env(
            &fx,
            os,
            &[
                ("BUN_INSTALL_CACHE_DIR", abs(&fx, "bun-cache")),
                ("BUN_INSTALL", abs(&fx, "bun-install")),
            ],
        );
        assert_eq!(
            rel(fx.root(), &the(&locate(&e), CacheTool::Bun).path),
            "bun-cache"
        );
        let e = env(&fx, os, &[("BUN_INSTALL", abs(&fx, "bun-install"))]);
        assert_eq!(
            rel(fx.root(), &the(&locate(&e), CacheTool::Bun).path),
            "bun-install/install/cache"
        );
        let c = the(&locate(&env(&fx, os, &[])), CacheTool::Bun).clone();
        assert_eq!(rel(fx.root(), &c.path), "home/.bun/install/cache");
        let cand = c.into_candidate(&idle(), None);
        if identity_available() {
            assert_eq!(cand.action, tool_action("bun", &["pm", "cache", "rm"]));
        }
    }
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn fx_qc_01_yarn_classic_and_berry_overrides() {
    let mut fx = Fixture::new().unwrap();
    yarn_classic(&mut fx, "yarn-env");
    yarn_classic(&mut fx, "home/yarnrc-cache");
    yarn_classic(&mut fx, "xdg-cache/yarn");
    yarn_berry(&mut fx, "berry-global/cache");
    yarn_berry(&mut fx, "home/berry-rc/cache");
    yarn_berry(&mut fx, "xdg-data/yarn/berry/cache");
    fx.file(
        "home/.yarnrc",
        "# yarn classic\ncache-folder \"~/yarnrc-cache\"\n",
    )
    .unwrap()
    .file(
        "home/.yarnrc.yml",
        "globalFolder: ~/berry-rc # comment\nenableGlobalCache: false\n",
    )
    .unwrap();
    fx.checkpoint().unwrap();

    // Environment beats the rc files.
    let e = env(
        &fx,
        Os::Linux,
        &[
            ("YARN_CACHE_FOLDER", abs(&fx, "yarn-env")),
            ("YARN_GLOBAL_FOLDER", abs(&fx, "berry-global")),
        ],
    );
    let scan = locate(&e);
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::YarnClassic).path),
        "yarn-env"
    );
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::YarnBerry).path),
        "berry-global/cache"
    );

    // rc files, read as data; `enableGlobalCache: false` is noted.
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let classic = the(&scan, CacheTool::YarnClassic).clone();
    let berry = the(&scan, CacheTool::YarnBerry).clone();
    assert_eq!(rel(fx.root(), &classic.path), "home/yarnrc-cache");
    assert_eq!(rel(fx.root(), &berry.path), "home/berry-rc/cache");
    assert!(berry.notes.iter().any(|n| n.contains("enableGlobalCache")));
    let (cc, bc) = (
        classic.into_candidate(&idle(), None),
        berry.into_candidate(&idle(), None),
    );
    if identity_available() {
        assert_eq!(cc.action, tool_action("yarn", &["cache", "clean"]));
        assert_eq!(
            bc.action,
            tool_action("yarn", &["cache", "clean", "--mirror"])
        );
    }
    check(&fx, &plan_inputs(&[cc, bc]));
}

#[test]
fn fx_qc_01_xdg_defaults_on_linux() {
    let mut fx = Fixture::new().unwrap();
    yarn_classic(&mut fx, "xdg-cache/yarn");
    yarn_berry(&mut fx, "xdg-data/yarn/berry/cache");
    yarn_classic(&mut fx, "home/.cache/yarn");
    fx.checkpoint().unwrap();
    let e = env(
        &fx,
        Os::Linux,
        &[
            ("XDG_CACHE_HOME", abs(&fx, "xdg-cache")),
            ("XDG_DATA_HOME", abs(&fx, "xdg-data")),
        ],
    );
    let scan = locate(&e);
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::YarnClassic).path),
        "xdg-cache/yarn"
    );
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::YarnBerry).path),
        "xdg-data/yarn/berry/cache"
    );
    // Relative XDG values are ignored (XDG spec): back to the default.
    let e = env(
        &fx,
        Os::Linux,
        &[("XDG_CACHE_HOME", "relative/cache".into())],
    );
    assert_eq!(
        rel(fx.root(), &the(&locate(&e), CacheTool::YarnClassic).path),
        "home/.cache/yarn"
    );
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn fx_qc_01_windows_local_app_data_variable() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "lad/npm-cache/_cacache");
    pnpm_store(&mut fx, "lad/pnpm/store", &["v10"]);
    yarn_classic(&mut fx, "lad/Yarn/Cache");
    yarn_berry(&mut fx, "lad/Yarn/Berry/cache");
    fx.checkpoint().unwrap();
    let e = env(&fx, Os::Windows, &[("LOCALAPPDATA", abs(&fx, "lad"))]);
    let got: Vec<String> = locate(&e)
        .found
        .iter()
        .map(|c| rel(fx.root(), &c.path))
        .collect();
    assert_eq!(
        got,
        [
            "lad/npm-cache/_cacache",
            "lad/pnpm/store",
            "lad/Yarn/Cache",
            "lad/Yarn/Berry/cache"
        ]
    );
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn missing_caches_are_simply_absent() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("home").unwrap().checkpoint().unwrap();
    for os in ALL_OS {
        let scan = locate(&env(&fx, os, &[]));
        assert!(scan.found.is_empty() && scan.refused.is_empty(), "{scan:?}");
    }
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn unrecognized_layout_is_s2() {
    let mut fx = Fixture::new().unwrap();
    fx.file("home/.npm/_cacache/notes.txt", "not a cache")
        .unwrap()
        .file("home/.cache/yarn/photo.jpg", "jpg")
        .unwrap()
        .checkpoint()
        .unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let mut cands = Vec::new();
    for c in scan.found {
        assert!(!c.recognized);
        let cand = c.into_candidate(&idle(), None);
        assert_eq!(cand.tier, expected(Tier::S2));
        assert!(cand.is_consistent());
        cands.push(cand);
    }
    assert_eq!(cands.len(), 2);
    check(&fx, &plan_inputs(&cands));
}

// ---------------------------------------------------------------------------------------------
// FX-QC-02: pnpm store referenced by projects
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_qc_02_pnpm_store_prune_only_with_shared_bytes() {
    let mut fx = Fixture::new().unwrap();
    let store = "home/.local/share/pnpm/store";
    pnpm_store(&mut fx, store, &["v10"]);
    fx.file_of_size(format!("{store}/v10/files/11/unreferenced"), 64 * 1024)
        .unwrap()
        .file_of_size(format!("{store}/v10/files/22/referenced"), 64 * 1024)
        .unwrap()
        .pnpm_workspace("home/proj", "proj", &[])
        .unwrap()
        .hardlink(
            "home/proj/node_modules/.pnpm/left-pad@1.3.0/node_modules/left-pad/index.js",
            format!("{store}/v10/files/22/referenced"),
        )
        .unwrap()
        .checkpoint()
        .unwrap();

    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::Pnpm).clone();
    let summary = summarize(&c.path);
    let cand = c.into_candidate(&idle(), Some(&summary));
    assert_eq!(cand.tier, expected(Tier::S1));
    if identity_available() {
        assert_eq!(cand.action, tool_action("pnpm", &["store", "prune"]));
        assert!(
            summary.shared_allocated > 0,
            "hard link seen by the scanner"
        );
        assert!(cand.evidence.contains(&Evidence::Hardlinked {
            shared_bytes: summary.shared_allocated
        }));
        assert_eq!(cand.flags.hardlinked, Some(true));
        assert_eq!(
            cand.sizes.reclaimable.shared_bytes,
            summary.shared_allocated
        );
    }
    assert!(
        !matches!(
            &cand.action,
            Action::Delete | Action::Quarantine | Action::Trash
        ),
        "the store is never removed directly"
    );
    let n = notes(&cand);
    assert!(n.iter().any(|t| t.contains("no full wipe")), "{n:?}");
    assert!(n.iter().any(|t| t.contains("hard-link")), "{n:?}");
    check(&fx, &plan_inputs(&[cand]));
}

#[test]
fn last_use_and_sizes_come_from_the_scanner() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "home/.npm/_cacache");
    fx.checkpoint().unwrap();
    let c = the(&locate(&env(&fx, Os::Macos, &[])), CacheTool::Npm).clone();
    let summary = summarize(&c.path);
    let cand = c.clone().into_candidate(&idle(), Some(&summary));
    assert_eq!(cand.sizes.allocated, summary.totals.allocated);
    assert_eq!(cand.sizes.logical, summary.totals.logical);
    assert!(cand.sizes.logical >= 4096);
    assert!(cand.evidence.iter().any(|e| matches!(
        e,
        Evidence::LastModified { newest_unix, sampled: false } if Some(*newest_unix) == summary.newest_mtime
    )));
    // A summary for another folder is ignored.
    let other = summarize(&home(&fx));
    let cand2 = c.into_candidate(&idle(), Some(&other));
    assert_eq!(cand2.sizes, scull_core::Sizes::default());
    check(&fx, &plan_inputs(&[cand, cand2]));
}

/// P1-SAFE-05 gap 6 (#203): "recently used" is deliberately not a risk signal for a cache,
/// but *unknown* activity (part of the cache could not be measured) is missing evidence and
/// stays protective: S2, so the tool command is withheld.
#[test]
fn unknown_activity_is_not_ignored_like_recent_use() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "home/.npm/_cacache");
    fx.checkpoint().unwrap();
    let c = the(&locate(&env(&fx, Os::Macos, &[])), CacheTool::Npm).clone();
    let unknown = CallerSignals {
        recent_activity: Tri::Unknown,
        ..idle()
    };
    let cand = c.into_candidate(&unknown, None);
    assert!(cand.is_consistent());
    assert_eq!(cand.tier, expected(Tier::S2), "{cand:#?}");
    assert_eq!(cand.action, Action::None, "no tool command at S2");
    assert!(codes(&cand).contains(&ReasonCode::SignalUnavailable));
    check(&fx, &plan_inputs(&[cand]));
}

// ---------------------------------------------------------------------------------------------
// FX-QC-03: tool running
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_qc_03_tool_running_is_skipped() {
    let mut fx = Fixture::new().unwrap();
    pnpm_store(&mut fx, "home/Library/pnpm/store", &["v10"]);
    fx.checkpoint().unwrap();
    let c = the(&locate(&env(&fx, Os::Macos, &[])), CacheTool::Pnpm).clone();
    assert!(c.activity_processes().contains(&"pnpm"));

    let running = CallerSignals {
        in_use: Tri::Yes,
        ..idle()
    };
    let cand = c.clone().into_candidate(&running, None);
    assert_eq!(cand.tier, Tier::S3);
    assert_eq!(cand.action, Action::None);
    assert!(codes(&cand).contains(&ReasonCode::InUse));
    assert!(PlanItem::from_candidate(&cand).is_err(), "never in a plan");

    // Unknown is not idle: S2, where tool commands aren't allowed (only restorable moves), so
    // the tool is skipped as well.
    let unknown = CallerSignals {
        in_use: Tri::Unknown,
        ..idle()
    };
    let cand2 = c.into_candidate(&unknown, None);
    assert_eq!(cand2.tier, expected(Tier::S2));
    assert_eq!(cand2.action, Action::None);
    assert!(PlanItem::from_candidate(&cand2).is_err());
    // Nothing observed at all: blocked.
    let blocked = the(&locate(&env(&fx, Os::Macos, &[])), CacheTool::Pnpm)
        .clone()
        .into_candidate(&CallerSignals::unobserved(), None);
    assert_eq!(blocked.tier, Tier::S3);
    check(&fx, &plan_inputs(&[cand, cand2, blocked]));
}

// ---------------------------------------------------------------------------------------------
// FX-QC-04: symlinked cache outside the expected location
// ---------------------------------------------------------------------------------------------

/// A link step was not possible here (recorded by the fixture): still check the invariants.
fn skip(fx: &mut Fixture) {
    eprintln!("link not created here; skipped: {:?}", fx.skipped());
    fx.checkpoint().unwrap();
    check(fx, &plan_inputs(&[]));
}

fn assert_link_skipped(c: &LocatedCache, link_rel: &str, root: &Path) -> Candidate {
    assert!(
        c.problems
            .iter()
            .any(|p| matches!(p, CacheProblem::Link { path } if rel(root, path) == link_rel)),
        "{:?}",
        c.problems
    );
    let cand = c.clone().into_candidate(&idle(), None);
    assert_eq!(cand.tier, Tier::S3);
    assert_eq!(cand.action, Action::None);
    assert_eq!(cand.flags.symlink, Some(true));
    assert!(codes(&cand).contains(&ReasonCode::LinkEscape));
    assert!(PlanItem::from_candidate(&cand).is_err());
    cand
}

#[test]
fn fx_qc_04_cache_symlinked_to_unexpected_location() {
    let mut fx = Fixture::new().unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    fx.dir("home/.npm").unwrap();
    if fx.symlink("home/.npm/_cacache", &canary).unwrap() != LinkOutcome::Created {
        return skip(&mut fx);
    }
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::Npm);
    assert!(!c.recognized, "a link is never listed through");
    let cand = assert_link_skipped(c, "home/.npm/_cacache", fx.root());
    check(&fx, &plan_inputs(&[cand]));
}

#[test]
fn fx_qc_04_folder_above_cache_symlinked_elsewhere() {
    let mut fx = Fixture::new().unwrap();
    yarn_classic(&mut fx, "elsewhere/cache/yarn");
    fx.dir("home").unwrap();
    let target = fx.path("elsewhere/cache").unwrap();
    if fx.symlink("home/.cache", &target).unwrap() != LinkOutcome::Created {
        return skip(&mut fx);
    }
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::YarnClassic);
    assert_eq!(rel(fx.root(), &c.path), "home/.cache/yarn");
    let cand = assert_link_skipped(c, "home/.cache", fx.root());
    check(&fx, &plan_inputs(&[cand]));
}

#[test]
fn fx_qc_04_cache_junction_to_unexpected_location() {
    let mut fx = Fixture::new().unwrap();
    let target = fx.canary().junction_target.clone();
    fx.dir("home/AppData/Local/npm-cache").unwrap();
    if fx
        .junction("home/AppData/Local/npm-cache/_cacache", &target)
        .unwrap()
        != LinkOutcome::Created
    {
        return skip(&mut fx); // junctions are Windows-only
    }
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Windows, &[]));
    let cand = assert_link_skipped(
        the(&scan, CacheTool::Npm),
        "home/AppData/Local/npm-cache/_cacache",
        fx.root(),
    );
    check(&fx, &plan_inputs(&[cand]));
}

// ---------------------------------------------------------------------------------------------
// QC-05: different volume than expected
// ---------------------------------------------------------------------------------------------

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

#[test]
fn qc_05_cache_on_other_volume_is_skipped() {
    if !identity_available() {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    bun_cache(&mut fx, "home/.bun/install/cache");
    npm_cache(&mut fx, "home/.npm/_cacache");
    fx.checkpoint().unwrap();
    let platform = OtherVolume {
        inner: Native::new(),
        under: fx.path("home/.bun").unwrap(),
    };
    let scan = locate_on(&env(&fx, Os::Linux, &[]), &platform);
    let bun = the(&scan, CacheTool::Bun);
    assert!(bun.volume_checked);
    assert!(matches!(
        bun.problems.as_slice(),
        [CacheProblem::OtherVolume { .. }]
    ));
    let cand = bun.clone().into_candidate(&idle(), None);
    assert_eq!(cand.tier, Tier::S3);
    assert_eq!(cand.action, Action::None);
    assert_eq!(cand.flags.cross_mount, Some(true));
    assert!(cand.evidence.contains(&Evidence::CrossesMount));
    assert!(codes(&cand).contains(&ReasonCode::CrossesMount));
    // The npm cache on the home volume is unaffected.
    let npm = the(&scan, CacheTool::Npm);
    assert!(npm.problems.is_empty() && npm.volume_checked);
    let npm_cand = npm.clone().into_candidate(&idle(), None);
    assert_eq!(npm_cand.tier, Tier::S1);
    check(&fx, &plan_inputs(&[cand, npm_cand]));
}

// ---------------------------------------------------------------------------------------------
// FX-QC-07 / T-23: config redirects to unexpected places are refused
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_qc_07_npmrc_cache_at_home_is_refused() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "home/.npm/_cacache");
    pnpm_store(&mut fx, "home/.local/share/pnpm/store", &["v10"]);
    fx.file("home/_cacache/index-v5/x", "{}")
        .unwrap()
        .file("home/.npmrc", "cache=~\nstore-dir=~/.cache\n")
        .unwrap()
        .checkpoint()
        .unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert!(
        scan.found
            .iter()
            .all(|c| !matches!(c.tool, CacheTool::Npm | CacheTool::Pnpm)),
        "a refused tool offers nothing, not even its default location: {:?}",
        scan.found
    );
    let reasons: Vec<(CacheTool, &str)> = scan
        .refused
        .iter()
        .map(|r| (r.tool, r.reason.as_str()))
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert_eq!(reasons[0].0, CacheTool::Npm);
    assert!(reasons[0].1.contains("home folder"), "{reasons:?}");
    assert_eq!(reasons[1].0, CacheTool::Pnpm);
    assert!(reasons[1].1.contains("shared folder"), "{reasons:?}");
    assert!(matches!(&scan.refused[0].source, CacheSource::Config { key, .. } if key == "cache"));
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn fx_qc_07_env_redirects_to_home_or_root_are_refused() {
    let mut fx = Fixture::new().unwrap();
    bun_cache(&mut fx, "home/.bun/install/cache");
    yarn_classic(&mut fx, "home/.cache/yarn");
    fx.checkpoint().unwrap();
    let root: PathBuf = fx.root().ancestors().last().unwrap().into();
    let e = env(
        &fx,
        Os::Linux,
        &[
            ("BUN_INSTALL_CACHE_DIR", abs(&fx, "home")),
            ("YARN_CACHE_FOLDER", root.to_string_lossy().into_owned()),
            ("npm_config_cache", "relative/npm".into()),
            ("npm_config_store_dir", "${UNSET_VAR}/store".into()),
        ],
    );
    let scan = locate(&e);
    let tools: Vec<CacheTool> = scan.refused.iter().map(|r| r.tool).collect();
    assert_eq!(
        tools,
        [
            CacheTool::Npm,
            CacheTool::Pnpm,
            CacheTool::Bun,
            CacheTool::YarnClassic
        ]
    );
    assert!(scan.refused[0].reason.contains("absolute"));
    assert!(scan.refused[1].reason.contains("UNSET_VAR"));
    assert!(scan.refused[2].reason.contains("home folder"));
    assert!(scan.refused[3].reason.contains("filesystem root"));
    assert!(scan.found.is_empty(), "{:?}", scan.found);
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn fx_qc_07_project_npmrc_is_never_read() {
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "home/.npm/_cacache");
    fx.npm_project("home/code/app", "app", scull_fixtures::NodeLock::Npm)
        .unwrap()
        .file("home/code/app/.npmrc", "cache=~\n")
        .unwrap()
        .file("home/code/app/.yarnrc.yml", "globalFolder: ~\n")
        .unwrap()
        .checkpoint()
        .unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert!(scan.refused.is_empty(), "{:?}", scan.refused);
    let c = the(&scan, CacheTool::Npm);
    assert_eq!(rel(fx.root(), &c.path), "home/.npm/_cacache");
    assert_eq!(c.source, CacheSource::Default);
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn unreadable_user_config_refuses_the_tool() {
    // `~/.npmrc` is a directory: its effective settings are unknown, so npm and pnpm are
    // refused rather than assumed to use their defaults.
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "home/.npm/_cacache");
    pnpm_store(&mut fx, "home/.local/share/pnpm/store", &["v10"]);
    fx.dir("home/.npmrc").unwrap().checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let tools: Vec<CacheTool> = scan.refused.iter().map(|r| r.tool).collect();
    assert_eq!(tools, [CacheTool::Npm, CacheTool::Pnpm]);
    assert!(scan.refused[0].reason.contains("can't be read"));
    check(&fx, &plan_inputs(&[]));

    // Without a config reader, an existing npmrc can't be read either; an absent one is fine.
    let mut fx = Fixture::new().unwrap();
    npm_cache(&mut fx, "home/.npm/_cacache");
    fx.file("home/.npmrc", "registry=https://r\n")
        .unwrap()
        .checkpoint()
        .unwrap();
    let e = env(&fx, Os::Linux, &[]);
    let scan = locate_caches(&e, &Native::new(), &DetectContext::new());
    assert_eq!(scan.refused.len(), 2);
    let mut fx2 = Fixture::new().unwrap();
    npm_cache(&mut fx2, "home/.npm/_cacache");
    fx2.checkpoint().unwrap();
    let scan2 = locate_caches(
        &env(&fx2, Os::Linux, &[]),
        &Native::new(),
        &DetectContext::new(),
    );
    assert!(scan2.refused.is_empty());
    assert_eq!(scan2.found.len(), 1);
    check(&fx, &plan_inputs(&[]));
    check(&fx2, &plan_inputs(&[]));
}

#[test]
fn same_folder_reached_twice_is_reported_once() {
    let mut fx = Fixture::new().unwrap();
    yarn_berry(&mut fx, "shared-yarn/cache");
    yarn_classic(&mut fx, "shared-yarn/cache");
    fx.checkpoint().unwrap();
    let e = env(
        &fx,
        Os::Linux,
        &[
            ("YARN_CACHE_FOLDER", abs(&fx, "shared-yarn/cache")),
            ("YARN_GLOBAL_FOLDER", abs(&fx, "shared-yarn")),
        ],
    );
    let scan = locate(&e);
    assert_eq!(scan.found.len(), 1);
    assert_eq!(scan.found[0].tool, CacheTool::YarnClassic);
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// pip and uv (P4-DET-02, PRD §10.5 / QC-03)
// ---------------------------------------------------------------------------------------------

#[test]
fn pip_and_uv_commands_and_notes() {
    let mut fx = Fixture::new().unwrap();
    pip_cache(&mut fx, "home/.cache/pip");
    uv_cache(&mut fx, "home/.cache/uv");
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert!(scan.refused.is_empty(), "{:?}", scan.refused);
    let pip = the(&scan, CacheTool::Pip).clone();
    assert_eq!(
        pip.markers
            .iter()
            .map(|m| rel(fx.root(), m))
            .collect::<Vec<_>>(),
        ["home/.cache/pip/http-v2", "home/.cache/pip/wheels"]
    );
    let uv = the(&scan, CacheTool::Uv).clone();
    assert_eq!(
        uv.markers
            .iter()
            .map(|m| rel(fx.root(), m))
            .collect::<Vec<_>>(),
        [
            "home/.cache/uv/archive-v0",
            "home/.cache/uv/simple-v16",
            "home/.cache/uv/wheels-v5"
        ]
    );
    assert_eq!(pip.activity_processes(), ["pip", "pip3"]);
    assert_eq!(uv.activity_processes(), ["uv", "uvx"]);

    let pc = pip.into_candidate(&idle(), None);
    let uc = uv.into_candidate(&idle(), None);
    for c in [&pc, &uc] {
        assert!(c.is_consistent());
        assert_eq!(c.category, Category::PackageCache);
        assert_eq!(c.tier, expected(Tier::S1), "{} is S1 (QC-02)", c.artifact);
        assert!(codes(c).contains(&ReasonCode::PackageCache));
    }
    assert_eq!(pc.rule.id, "pip.cache");
    assert_eq!(uc.rule.id, "uv.cache");
    if identity_available() {
        assert_eq!(pc.action, tool_action("pip", &["cache", "purge"]));
        assert_eq!(uc.action, tool_action("uv", &["cache", "prune"]));
    }
    assert!(
        notes(&pc)
            .iter()
            .any(|n| n.contains("`pip cache purge` (removes everything)"))
    );
    assert!(notes(&uc).iter().any(|n| n.contains(
        "`uv cache prune` (removes unused entries); full wipe alternative: `uv cache clean`"
    )));
    check(&fx, &plan_inputs(&[pc, uc]));
}

/// pip: `PIP_CACHE_DIR` beats the user config, which beats the default, on every OS layout.
#[test]
fn pip_env_beats_user_config_beats_default() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        let default = default_layout(os)
            .into_iter()
            .find(|(t, _)| *t == CacheTool::Pip)
            .unwrap()
            .1;
        pip_cache(&mut fx, default);
        pip_cache(&mut fx, "from-config/pip");
        pip_cache(&mut fx, "from-env/pip");
        let conf = match os {
            Os::Linux => "home/.config/pip/pip.conf",
            Os::Macos => "home/Library/Application Support/pip/pip.conf",
            Os::Windows => "home/AppData/Roaming/pip/pip.ini",
        };
        fx.file(
            conf,
            format!(
                "[install]\ncache-dir = /never/this\n[global]\ncache-dir = {}\n",
                abs(&fx, "from-config/pip")
            ),
        )
        .unwrap()
        .checkpoint()
        .unwrap();

        let e = env(&fx, os, &[("PIP_CACHE_DIR", abs(&fx, "from-env/pip"))]);
        let c = the(&locate(&e), CacheTool::Pip).clone();
        assert_eq!(rel(fx.root(), &c.path), "from-env/pip", "{os:?}");
        assert_eq!(
            c.source,
            CacheSource::Env {
                var: "PIP_CACHE_DIR".into()
            }
        );

        let c = the(&locate(&env(&fx, os, &[])), CacheTool::Pip).clone();
        assert_eq!(rel(fx.root(), &c.path), "from-config/pip", "{os:?}");
        assert_eq!(
            c.source,
            CacheSource::Config {
                file: fx.path(conf).unwrap(),
                key: "cache-dir".into()
            }
        );

        // `PIP_CONFIG_FILE=/dev/null` loads no file at all: back to the default.
        let devnull = if os == Os::Windows {
            "nul"
        } else {
            "/dev/null"
        };
        let e = env(&fx, os, &[("PIP_CONFIG_FILE", devnull.into())]);
        let c = the(&locate(&e), CacheTool::Pip).clone();
        assert_eq!(rel(fx.root(), &c.path), default, "{os:?}");
        assert_eq!(c.source, CacheSource::Default);
        check(&fx, &plan_inputs(&[]));
    }
}

#[test]
fn pip_config_file_variable_and_legacy_file() {
    let mut fx = Fixture::new().unwrap();
    pip_cache(&mut fx, "home/.cache/pip");
    pip_cache(&mut fx, "legacy/pip");
    pip_cache(&mut fx, "envfile/pip");
    let (legacy, envfile) = (abs(&fx, "legacy/pip"), abs(&fx, "envfile/pip"));
    fx.file(
        "home/.pip/pip.conf",
        format!("[global]\ncache_dir = {legacy}\n"),
    )
    .unwrap()
    .file(
        "conf/custom.conf",
        format!("[global]\ncache-dir = {envfile}\n"),
    )
    .unwrap()
    .file("conf/empty.conf", "[global]\ntimeout = 5\n")
    .unwrap()
    .checkpoint()
    .unwrap();
    // Only the legacy file sets it.
    let c = the(&locate(&env(&fx, Os::Linux, &[])), CacheTool::Pip).clone();
    assert_eq!(rel(fx.root(), &c.path), "legacy/pip");
    // An existing PIP_CONFIG_FILE is the only file pip reads.
    let e = env(
        &fx,
        Os::Linux,
        &[("PIP_CONFIG_FILE", abs(&fx, "conf/custom.conf"))],
    );
    assert_eq!(
        rel(fx.root(), &the(&locate(&e), CacheTool::Pip).path),
        "envfile/pip"
    );
    let e = env(
        &fx,
        Os::Linux,
        &[("PIP_CONFIG_FILE", abs(&fx, "conf/empty.conf"))],
    );
    assert_eq!(
        rel(fx.root(), &the(&locate(&e), CacheTool::Pip).path),
        "home/.cache/pip",
        "the per-user files are skipped"
    );
    // A missing one is ignored (the per-user files apply); a relative one is unknown: refused.
    let e = env(
        &fx,
        Os::Linux,
        &[("PIP_CONFIG_FILE", abs(&fx, "conf/missing.conf"))],
    );
    assert_eq!(
        rel(fx.root(), &the(&locate(&e), CacheTool::Pip).path),
        "legacy/pip"
    );
    let e = env(&fx, Os::Linux, &[("PIP_CONFIG_FILE", "pip.conf".into())]);
    let scan = locate(&e);
    assert!(scan.found.iter().all(|c| c.tool != CacheTool::Pip));
    assert_eq!(scan.refused[0].tool, CacheTool::Pip);
    check(&fx, &plan_inputs(&[]));
}

/// uv: `UV_CACHE_DIR` beats `cache-dir` in the user `uv.toml`, which beats the default.
#[test]
fn uv_env_beats_user_config_beats_default() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        let default = default_layout(os)
            .into_iter()
            .find(|(t, _)| *t == CacheTool::Uv)
            .unwrap()
            .1;
        uv_cache(&mut fx, default);
        uv_cache(&mut fx, "from-config/uv");
        uv_cache(&mut fx, "from-env/uv");
        let conf = match os {
            Os::Linux | Os::Macos => "home/.config/uv/uv.toml",
            Os::Windows => "home/AppData/Roaming/uv/uv.toml",
        };
        // A TOML literal string: Windows paths need no escaping.
        fx.file(
            conf,
            format!(
                "cache-dir = '{}'\n[pip]\nindex-url = 'https://example.invalid/simple'\n",
                abs(&fx, "from-config/uv")
            ),
        )
        .unwrap()
        .file("other/uv.toml", "native-tls = true\n")
        .unwrap()
        .checkpoint()
        .unwrap();

        let e = env(&fx, os, &[("UV_CACHE_DIR", abs(&fx, "from-env/uv"))]);
        assert_eq!(
            rel(fx.root(), &the(&locate(&e), CacheTool::Uv).path),
            "from-env/uv",
            "{os:?}"
        );
        let c = the(&locate(&env(&fx, os, &[])), CacheTool::Uv).clone();
        assert_eq!(rel(fx.root(), &c.path), "from-config/uv", "{os:?}");
        assert_eq!(
            c.source,
            CacheSource::Config {
                file: fx.path(conf).unwrap(),
                key: "cache-dir".into()
            }
        );
        // UV_CONFIG_FILE replaces the per-user file.
        let e = env(&fx, os, &[("UV_CONFIG_FILE", abs(&fx, "other/uv.toml"))]);
        assert_eq!(
            rel(fx.root(), &the(&locate(&e), CacheTool::Uv).path),
            default,
            "{os:?}"
        );
        check(&fx, &plan_inputs(&[]));
    }
}

#[test]
fn uv_unparsable_or_unreadable_config_refuses_uv() {
    let mut fx = Fixture::new().unwrap();
    uv_cache(&mut fx, "home/.cache/uv");
    pip_cache(&mut fx, "home/.cache/pip");
    fx.file("home/.config/uv/uv.toml", "cache-dir = [\n")
        .unwrap()
        .checkpoint()
        .unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert_eq!(
        scan.refused.iter().map(|r| r.tool).collect::<Vec<_>>(),
        [CacheTool::Uv]
    );
    assert!(scan.refused[0].reason.contains("can't be parsed"));
    assert!(
        scan.found.iter().any(|c| c.tool == CacheTool::Pip),
        "other tools are unaffected"
    );
    check(&fx, &plan_inputs(&[]));

    let mut fx = Fixture::new().unwrap();
    uv_cache(&mut fx, "home/.cache/uv");
    fx.dir("home/.config/uv/uv.toml")
        .unwrap()
        .checkpoint()
        .unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert_eq!(scan.refused[0].tool, CacheTool::Uv);
    assert!(scan.found.is_empty());
    check(&fx, &plan_inputs(&[]));
}

/// T-23 / QC-05: a pip or uv location pointing at home or a shared folder refuses the tool.
#[test]
fn pip_and_uv_redirects_to_home_or_shared_folders_are_refused() {
    let mut fx = Fixture::new().unwrap();
    pip_cache(&mut fx, "home/.cache/pip");
    uv_cache(&mut fx, "home/.cache/uv");
    fx.file(
        "home/.config/uv/uv.toml",
        format!("cache-dir = '{}'\n", abs(&fx, "home/.cache")),
    )
    .unwrap()
    .checkpoint()
    .unwrap();
    let e = env(&fx, Os::Linux, &[("PIP_CACHE_DIR", abs(&fx, "home"))]);
    let scan = locate(&e);
    assert!(scan.found.is_empty(), "{:?}", scan.found);
    let refused: Vec<(CacheTool, &str)> = scan
        .refused
        .iter()
        .map(|r| (r.tool, r.reason.as_str()))
        .collect();
    assert_eq!(refused.len(), 2);
    assert_eq!(refused[0].0, CacheTool::Pip);
    assert!(refused[0].1.contains("home folder"));
    assert_eq!(refused[1].0, CacheTool::Uv);
    assert!(refused[1].1.contains("shared folder"));
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn pip_and_uv_xdg_and_windows_variables() {
    let mut fx = Fixture::new().unwrap();
    pip_cache(&mut fx, "xdg-cache/pip");
    uv_cache(&mut fx, "xdg-cache/uv");
    pip_cache(&mut fx, "lad/pip/Cache");
    uv_cache(&mut fx, "lad/uv/cache");
    fx.checkpoint().unwrap();
    let e = env(&fx, Os::Linux, &[("XDG_CACHE_HOME", abs(&fx, "xdg-cache"))]);
    let scan = locate(&e);
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::Pip).path),
        "xdg-cache/pip"
    );
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::Uv).path),
        "xdg-cache/uv"
    );
    // macOS: uv honours XDG_CACHE_HOME too; pip uses ~/Library/Caches.
    let scan = locate(&env(
        &fx,
        Os::Macos,
        &[("XDG_CACHE_HOME", abs(&fx, "xdg-cache"))],
    ));
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::Uv).path),
        "xdg-cache/uv"
    );
    assert!(scan.found.iter().all(|c| c.tool != CacheTool::Pip));
    let scan = locate(&env(&fx, Os::Windows, &[("LOCALAPPDATA", abs(&fx, "lad"))]));
    assert_eq!(
        found_rel(&fx, &scan),
        [
            (CacheTool::Pip, "lad/pip/Cache".to_owned()),
            (CacheTool::Uv, "lad/uv/cache".to_owned())
        ]
    );
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// conda package cache (P4-DET-01)
// ---------------------------------------------------------------------------------------------

#[test]
fn conda_default_roots_command_and_note() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        conda_pkgs(&mut fx, "home/miniforge3/pkgs");
        conda_pkgs(&mut fx, "home/.conda/pkgs");
        conda_pkgs(&mut fx, "home/AppData/Local/conda/conda/pkgs");
        fx.file("home/anaconda3/pkgs/readme.txt", "not a package cache")
            .unwrap()
            .checkpoint()
            .unwrap();
        let scan = locate(&env(&fx, os, &[]));
        assert!(scan.refused.is_empty(), "{:?}", scan.refused);
        let conda: Vec<&LocatedCache> = scan
            .found
            .iter()
            .filter(|c| c.tool == CacheTool::Conda)
            .collect();
        let mut want = vec![
            "home/anaconda3/pkgs",
            "home/miniforge3/pkgs",
            "home/.conda/pkgs",
        ];
        if os == Os::Windows {
            want.push("home/AppData/Local/conda/conda/pkgs");
        }
        assert_eq!(
            conda
                .iter()
                .map(|c| rel(fx.root(), &c.path))
                .collect::<Vec<_>>(),
            want,
            "{os:?}"
        );
        let mut cands = Vec::new();
        for c in conda {
            let cand = c.clone().into_candidate(&idle(), None);
            assert!(cand.is_consistent());
            if rel(fx.root(), &c.path) == "home/anaconda3/pkgs" {
                assert!(!c.recognized, "no urls.txt or package archive");
                assert_eq!(cand.tier, expected(Tier::S2));
                assert_eq!(cand.action, Action::None);
            } else {
                assert!(c.recognized);
                assert_eq!(
                    c.markers
                        .iter()
                        .map(|m| m.file_name().unwrap().to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                    ["urls.txt", "numpy-2.1.2-py312h.conda"]
                );
                assert_eq!(cand.tier, expected(Tier::S1));
                assert_eq!(cand.rule.id, "conda.pkgs");
                if identity_available() {
                    assert_eq!(
                        cand.action,
                        tool_action("conda", &["clean", "--packages", "--tarballs", "--yes"])
                    );
                }
                assert!(
                    notes(&cand)
                        .iter()
                        .any(|n| n.contains("hard-link into this package cache"))
                );
            }
            cands.push(cand);
        }
        check(&fx, &plan_inputs(&cands));
    }
}

#[test]
fn conda_env_beats_condarc_beats_default() {
    let mut fx = Fixture::new().unwrap();
    conda_pkgs(&mut fx, "home/miniconda3/pkgs");
    conda_pkgs(&mut fx, "rc/a");
    conda_pkgs(&mut fx, "rc/b");
    conda_pkgs(&mut fx, "envs/x");
    conda_pkgs(&mut fx, "envs/y");
    let (a, b) = (abs(&fx, "rc/a"), abs(&fx, "rc/b"));
    fx.file(
        "home/.condarc",
        format!("channels:\n  - conda-forge\npkgs_dirs:\n  - {a}\n  - '{b}'\n"),
    )
    .unwrap()
    .checkpoint()
    .unwrap();
    let paths = |scan: &CacheScan| -> Vec<String> {
        scan.found
            .iter()
            .filter(|c| c.tool == CacheTool::Conda)
            .map(|c| rel(fx.root(), &c.path))
            .collect()
    };
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert_eq!(
        paths(&scan),
        ["rc/a", "rc/b"],
        "condarc replaces the defaults"
    );
    assert!(matches!(
        &scan.found[0].source,
        CacheSource::Config { key, .. } if key == "pkgs_dirs"
    ));
    let joined = format!("{},{}", abs(&fx, "envs/x"), abs(&fx, "envs/y"));
    let scan = locate(&env(&fx, Os::Macos, &[("CONDA_PKGS_DIRS", joined)]));
    assert_eq!(paths(&scan), ["envs/x", "envs/y"]);
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn conda_unreadable_or_unexpected_locations_are_refused() {
    // A condarc `pkgs_dirs` this parser can't read: the location is unknown.
    let mut fx = Fixture::new().unwrap();
    conda_pkgs(&mut fx, "home/miniconda3/pkgs");
    fx.file("home/.condarc", "pkgs_dirs: /scalar/not/a/list\n")
        .unwrap()
        .checkpoint()
        .unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert!(scan.found.iter().all(|c| c.tool != CacheTool::Conda));
    assert_eq!(scan.refused[0].tool, CacheTool::Conda);
    assert!(scan.refused[0].reason.contains("is not a list"));
    check(&fx, &plan_inputs(&[]));

    // One bad entry (home itself) refuses the whole tool: `conda clean` acts on every entry.
    let mut fx = Fixture::new().unwrap();
    conda_pkgs(&mut fx, "pk");
    fx.checkpoint().unwrap();
    let joined = format!("{},{}", abs(&fx, "pk"), abs(&fx, "home"));
    let scan = locate(&env(&fx, Os::Windows, &[("CONDA_PKGS_DIRS", joined)]));
    assert!(scan.found.is_empty());
    assert!(scan.refused[0].reason.contains("home folder"));
    check(&fx, &plan_inputs(&[]));
}
