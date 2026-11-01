//! `scull scan` picks up the bundled Python rules (P4-DET-02): FX-CLS-07 end to end through the
//! engine (`.venv` with `uv.lock` S1, without a lockfile S2), `__pycache__` and tool caches S0,
//! and a virtual environment outside any project is never a candidate. The scan changes nothing
//! (I-1/I-2/I-3).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::config::Config;
use scull_core::{Action, Candidate, ReasonCode, Tier};
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, FixedInUse, InUseSource, ScanReport, ScanRequest, now_unix,
};
use scull_fixtures::{CACHEDIR_TAG, Fixture, InvariantInputs, PlanEntry};
use scull_platform::Native;
use scull_safety::Tri;
use std::path::Path;
use std::sync::Arc;

const THREE_DAYS: i64 = 3 * 24 * 3600;

fn venv(fx: &mut Fixture, dir: &str) {
    fx.file(format!("{dir}/pyvenv.cfg"), "version = 3.12.7\n")
        .unwrap()
        .file_of_size(
            format!("{dir}/lib/python3.12/site-packages/requests/__init__.py"),
            4096,
        )
        .unwrap()
        .file_of_size(
            format!(
                "{dir}/lib/python3.12/site-packages/requests/__pycache__/__init__.cpython-312.pyc"
            ),
            2048,
        )
        .unwrap();
}

fn project(fx: &mut Fixture, dir: &str, lock: bool) {
    fx.file(format!("{dir}/pyproject.toml"), "[project]\nname = \"x\"\n")
        .unwrap()
        .file(format!("{dir}/src/x/core.py"), "x = 1\n")
        .unwrap()
        .file_of_size(
            format!("{dir}/src/x/__pycache__/core.cpython-312.pyc"),
            1024,
        )
        .unwrap()
        .file(format!("{dir}/.pytest_cache/CACHEDIR.TAG"), CACHEDIR_TAG)
        .unwrap()
        .file(format!("{dir}/.pytest_cache/v/cache/nodeids"), "[]\n")
        .unwrap();
    if lock {
        fx.file(format!("{dir}/uv.lock"), "version = 1\n").unwrap();
    }
    venv(fx, &format!("{dir}/.venv"));
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

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
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

#[test]
fn scan_picks_up_python_rules() {
    let mut fx = Fixture::new().unwrap();
    project(&mut fx, "locked", true);
    project(&mut fx, "unlocked", false);
    venv(&mut fx, "home/.virtualenvs/work");
    fx.checkpoint().unwrap();
    let root = canon(fx.root());

    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(Tri::No)));
    let e = Engine::new(cfg).unwrap();
    let r = e
        .scan(
            std::slice::from_ref(&root),
            &ScanRequest::full()
                .history(false)
                .at(now_unix() + THREE_DAYS),
            &CancelToken::new(),
            |_| {},
        )
        .unwrap();

    assert_eq!(
        paths(&r, &root),
        [
            "locked/.pytest_cache",
            "locked/.venv",
            "locked/src/x/__pycache__",
            "unlocked/.pytest_cache",
            "unlocked/.venv",
            "unlocked/src/x/__pycache__",
        ],
        "nothing inside an environment, nothing outside a project"
    );
    // FX-CLS-07.
    let locked = at(&r, &root, "locked/.venv");
    assert_eq!(locked.rule.id, "python.venv");
    assert_eq!(locked.tier, Tier::S1, "{locked:#?}");
    assert_eq!(locked.action, Action::default_for(Tier::S1));
    assert!(locked.sizes.logical > 4000, "{locked:#?}");
    let unlocked = at(&r, &root, "unlocked/.venv");
    assert_eq!(unlocked.tier, Tier::S2, "{unlocked:#?}");
    assert!(
        unlocked
            .reasons
            .iter()
            .any(|x| x.code == ReasonCode::NoLockfile)
    );
    for p in [
        "locked/src/x/__pycache__",
        "locked/.pytest_cache",
        "unlocked/src/x/__pycache__",
        "unlocked/.pytest_cache",
    ] {
        let c = at(&r, &root, p);
        assert_eq!(c.tier, Tier::S0, "{c:#?}");
        assert_eq!(c.action, Action::Delete);
    }
    let python = r
        .projects
        .iter()
        .find(|p| rel(&root, &p.root.to_path_buf()) == "locked")
        .expect("locked project");
    assert_eq!(python.ecosystems, ["python"]);

    for c in &r.candidates {
        assert!(c.is_consistent(), "{c:#?}");
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
