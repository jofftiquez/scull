//! Python detector fixtures (P4-DET-02): FX-CLS-07 (`.venv` with `uv.lock` S1, without a
//! lockfile S2), every lockfile kind, pinned `requirements*.txt`, `__pycache__`, the pytest /
//! mypy / Ruff caches, tox and nox, and CLS-03 marker gating (a venv outside a project is never
//! a candidate). Every test builds its tree with `scull-fixtures`, detects read-only through
//! `scull-platform`, and checks the universal invariants (I-2 canary, I-3 tree unchanged, I-1 on
//! the would-be plan).

#![allow(clippy::unwrap_used)]

mod common;

use common::{age_tree, check, classify_expect, clean_caller, plan_inputs, rel, scan};
use scull_core::{Action, Category, Evidence, ReasonCode, Tier};
use scull_detectors::{DetectContext, DetectedArtifact, Detectors};
use scull_fixtures::{CACHEDIR_TAG, Fixture, ago, days_ago};
use scull_platform::Native;
use scull_safety::{Regen, Tri};
use std::path::Path;
use std::time::Duration;

const PYPROJECT: &str = "[project]\nname = \"app\"\nversion = \"0.1.0\"\n";

fn lockfiles(a: &DetectedArtifact, root: &Path) -> Vec<String> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Lockfile { path } => Some(rel(root, &path.to_path_buf())),
            _ => None,
        })
        .collect()
}

fn markers(a: &DetectedArtifact, root: &Path) -> Vec<String> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Marker { path } => Some(rel(root, &path.to_path_buf())),
            _ => None,
        })
        .collect()
}

fn notes(a: &DetectedArtifact) -> Vec<&str> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A virtual environment at `dir` as `python -m venv` / uv lay it out (POSIX and Windows
/// folders both present; detection doesn't depend on either), with the interpreter links where
/// the platform allows them.
fn venv(fx: &mut Fixture, dir: &str) {
    fx.file(
        format!("{dir}/pyvenv.cfg"),
        "home = /usr/bin\ninclude-system-site-packages = false\nversion = 3.12.7\n",
    )
    .unwrap()
    .file_of_size(
        format!("{dir}/lib/python3.12/site-packages/requests/__init__.py"),
        4096,
    )
    .unwrap()
    .file_of_size(
        format!("{dir}/lib/python3.12/site-packages/requests/__pycache__/__init__.cpython-312.pyc"),
        2048,
    )
    .unwrap()
    .file(format!("{dir}/bin/activate"), "# activate\n")
    .unwrap()
    .file(format!("{dir}/.gitignore"), "*\n")
    .unwrap();
    // Links inside the environment are recorded, never followed.
    fx.symlink(format!("{dir}/bin/python"), "python3").unwrap();
    fx.symlink(format!("{dir}/lib64"), "lib").unwrap();
}

/// A Python project at `dir` with `pyproject.toml`, a package and the given extra files.
fn project(fx: &mut Fixture, dir: &str, extra: &[(&str, &str)]) {
    fx.file(format!("{dir}/pyproject.toml"), PYPROJECT)
        .unwrap()
        .file(format!("{dir}/src/app/__init__.py"), "")
        .unwrap();
    for (name, body) in extra {
        fx.file(format!("{dir}/{name}"), body).unwrap();
    }
}

/// Bytecode for `module.py` in `pkg_dir` (the source is written too unless `sourceless`).
fn pycache(fx: &mut Fixture, pkg_dir: &str, module: &str, sourceless: bool) {
    if !sourceless {
        fx.file(format!("{pkg_dir}/{module}.py"), "x = 1\n")
            .unwrap();
    }
    fx.file_of_size(
        format!("{pkg_dir}/__pycache__/{module}.cpython-312.pyc"),
        1024,
    )
    .unwrap();
}

/// A tagged tool cache directory (pytest, mypy and Ruff all write `CACHEDIR.TAG`).
fn tool_cache(fx: &mut Fixture, dir: &str, tagged: bool) {
    fx.file(format!("{dir}/.gitignore"), "# Created by the tool\n*\n")
        .unwrap()
        .file(format!("{dir}/v/cache/lastfailed"), "{}\n")
        .unwrap();
    if tagged {
        fx.file(format!("{dir}/CACHEDIR.TAG"), CACHEDIR_TAG)
            .unwrap();
    }
}

fn detect_one(root: &Path, rel_path: &str) -> Option<DetectedArtifact> {
    let detectors = Detectors::bundled().unwrap();
    detectors.detect(
        &root.join(rel_path),
        &Native::new(),
        &DetectContext::new().with_config_reader(&common::StdReader),
    )
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-07: `.venv` with `uv.lock` vs. without a lockfile

#[test]
fn fx_cls_07_venv_with_uv_lock_is_s1() {
    let mut fx = Fixture::new().unwrap();
    project(&mut fx, "app", &[("uv.lock", "version = 1\n")]);
    venv(&mut fx, "app/.venv");
    age_tree(&mut fx, "app", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        ["app/.venv"],
        "nothing inside is offered"
    );
    let a = s.at(&root, "app/.venv");
    assert_eq!(a.rule.id, "python.venv");
    assert_eq!(a.category, Category::ProjectArtifact);
    assert_eq!(a.projects[0].name, "app");
    assert_eq!(a.projects[0].ecosystem, "python");
    assert_eq!(
        markers(a, &root),
        ["app/.venv/pyvenv.cfg", "app/pyproject.toml"]
    );
    assert_eq!(lockfiles(a, &root), ["app/uv.lock"]);
    assert!(a.signals.lockfile_present);
    assert_eq!(a.signals.regen, Regen::NetworkOrLong, "reinstall downloads");
    assert!(a.activity_processes.iter().any(|p| p == "uv"));
    let links = std::fs::symlink_metadata(root.join("app/.venv/lib64"))
        .is_ok_and(|m| m.file_type().is_symlink());
    if links {
        assert_eq!(a.contains_links, Some(true), "bin/python, lib64 recorded");
    }
    assert!(
        a.regeneration
            .instructions
            .as_deref()
            .unwrap()
            .contains("uv sync")
    );

    let caller = clean_caller(a);
    assert_eq!(caller.recent_activity, Tri::No);
    let c = classify_expect(a, &caller, Tier::S1);
    if c.tier == Tier::S1 {
        assert_eq!(
            c.action,
            Action::default_for(Tier::S1),
            "quarantine, never delete"
        );
        assert!(
            c.reasons
                .iter()
                .any(|r| r.code == ReasonCode::CostlyToRegenerate)
        );
    }
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn fx_cls_07_venv_without_lockfile_is_s2() {
    let mut fx = Fixture::new().unwrap();
    project(&mut fx, "app", &[]);
    venv(&mut fx, "app/.venv");
    age_tree(&mut fx, "app", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["app/.venv"]);
    let a = s.at(&root, "app/.venv");
    assert!(!a.signals.lockfile_present);
    assert!(lockfiles(a, &root).is_empty());
    let c = classify_expect(a, &clean_caller(a), Tier::S2);
    if c.tier == Tier::S2 {
        assert!(c.reasons.iter().any(|r| r.code == ReasonCode::NoLockfile));
        assert_ne!(c.action, Action::Delete);
    }
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn fx_cls_07_every_lockfile_kind_makes_s1() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("uv", &[("uv.lock", "version = 1\n")]),
        ("poetry", &[("poetry.lock", "# poetry\n")]),
        (
            "pipenv",
            &[("Pipfile", "[packages]\n"), ("Pipfile.lock", "{}\n")],
        ),
        ("pdm", &[("pdm.lock", "# pdm\n")]),
        ("pylock", &[("pylock.toml", "lock-version = '1.0'\n")]),
        (
            "pylock-named",
            &[("pylock.dev.toml", "lock-version = '1.0'\n")],
        ),
        ("rye", &[("requirements.lock", "requests==2.32.3\n")]),
    ];
    let mut fx = Fixture::new().unwrap();
    for (name, extra) in cases {
        project(&mut fx, name, extra);
        venv(&mut fx, &format!("{name}/.venv"));
        age_tree(&mut fx, name, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    let mut cands = Vec::new();
    for (name, extra) in cases {
        let a = s.at(&root, &format!("{name}/.venv"));
        let lock = extra.last().unwrap().0;
        assert_eq!(lockfiles(a, &root), [format!("{name}/{lock}")], "{name}");
        cands.push(classify_expect(a, &clean_caller(a), Tier::S1));
    }
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn venv_named_venv_and_setup_py_or_requirements_projects() {
    let mut fx = Fixture::new().unwrap();
    fx.file("legacy/setup.py", "from setuptools import setup\nsetup()\n")
        .unwrap()
        .file("legacy/poetry.lock", "# lock\n")
        .unwrap();
    venv(&mut fx, "legacy/venv");
    fx.file("script/requirements.txt", "requests>=2\n").unwrap();
    venv(&mut fx, "script/.venv");
    age_tree(&mut fx, "legacy", days_ago(3));
    age_tree(&mut fx, "script", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["legacy/venv", "script/.venv"]);
    let legacy = s.at(&root, "legacy/venv");
    let script = s.at(&root, "script/.venv");
    assert_eq!(markers(legacy, &root)[1], "legacy/setup.py");
    assert_eq!(markers(script, &root)[1], "script/requirements.txt");
    let c1 = classify_expect(legacy, &clean_caller(legacy), Tier::S1);
    // A ranged requirements.txt doesn't reproduce the environment: no lockfile.
    let c2 = classify_expect(script, &clean_caller(script), Tier::S2);
    check(&fx, &plan_inputs(&[c1, c2]));
}

#[test]
fn pinned_requirements_txt_counts_as_the_lockfile() {
    let mut fx = Fixture::new().unwrap();
    project(
        &mut fx,
        "pinned",
        &[
            ("requirements.in", "requests\n"),
            (
                "requirements.txt",
                "# autogenerated by pip-compile\ncertifi==2024.8.30 \\\n    \
                 --hash=sha256:abc\n    # via requests\nrequests==2.32.3\n",
            ),
        ],
    );
    venv(&mut fx, "pinned/.venv");
    project(
        &mut fx,
        "mixed",
        &[
            ("requirements.txt", "requests>=2\n"),
            ("requirements-lock.txt", "requests==2.32.3\nidna==3.10\n"),
        ],
    );
    venv(&mut fx, "mixed/.venv");
    project(
        &mut fx,
        "ranged",
        &[("requirements.txt", "requests~=2.32\n")],
    );
    venv(&mut fx, "ranged/.venv");
    for p in ["pinned", "mixed", "ranged"] {
        age_tree(&mut fx, p, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);

    let pinned = s.at(&root, "pinned/.venv");
    assert_eq!(lockfiles(pinned, &root), ["pinned/requirements.txt"]);
    assert!(
        notes(pinned)
            .iter()
            .any(|n| n.contains("pinned to one version"))
    );
    let mixed = s.at(&root, "mixed/.venv");
    assert_eq!(lockfiles(mixed, &root), ["mixed/requirements-lock.txt"]);
    let ranged = s.at(&root, "ranged/.venv");
    assert!(lockfiles(ranged, &root).is_empty());
    let cands = vec![
        classify_expect(pinned, &clean_caller(pinned), Tier::S1),
        classify_expect(mixed, &clean_caller(mixed), Tier::S1),
        classify_expect(ranged, &clean_caller(ranged), Tier::S2),
    ];

    // Without a config reader the contents are unknown: no lockfile (protective).
    let a = Detectors::bundled()
        .unwrap()
        .detect(
            &root.join("pinned/.venv"),
            &Native::new(),
            &DetectContext::new(),
        )
        .unwrap();
    assert!(!a.signals.lockfile_present);
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn recent_venv_is_bumped_one_tier() {
    let mut fx = Fixture::new().unwrap();
    project(&mut fx, "app", &[("uv.lock", "version = 1\n")]);
    venv(&mut fx, "app/.venv");
    age_tree(&mut fx, "app", days_ago(3));
    fx.set_mtime("app/.venv/pyvenv.cfg", ago(Duration::from_secs(3600)))
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    let a = s.at(&root, "app/.venv");
    let caller = clean_caller(a);
    assert_eq!(caller.recent_activity, Tri::Yes);
    let c = classify_expect(a, &caller, Tier::S2);
    check(&fx, &plan_inputs(&[c]));
}

// ---------------------------------------------------------------------------------------------
// CLS-03: a venv outside a project is never a candidate

#[test]
fn venv_outside_a_project_is_never_a_candidate() {
    let mut fx = Fixture::new().unwrap();
    // virtualenvwrapper / pyenv-virtualenv style environments: no project beside them.
    venv(&mut fx, "home/.virtualenvs/work");
    venv(&mut fx, "home/.venv");
    venv(&mut fx, "home/venv");
    // A `venv` folder that is not an environment (no pyvenv.cfg): e.g. a source package.
    project(&mut fx, "tool", &[("uv.lock", "version = 1\n")]);
    fx.file("tool/venv/__init__.py", "").unwrap();
    // A manifest beside it but the folder is a link: never a candidate.
    project(&mut fx, "linked", &[("uv.lock", "version = 1\n")]);
    fx.symlink("linked/.venv", "../home/.venv").unwrap();
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert!(
        s.found.iter().all(|a| a.rule.id != "python.venv"),
        "{:?}",
        s.rel_paths(&root)
    );
    // Bytecode inside those environments' site-packages is not offered either.
    assert!(
        s.found.iter().all(|a| a.rule.id != "python.pycache"),
        "{:?}",
        s.rel_paths(&root)
    );
    for p in [
        "home/.virtualenvs/work",
        "home/.venv",
        "home/venv",
        "tool/venv",
        "linked/.venv",
    ] {
        assert!(detect_one(&root, p).is_none(), "{p}");
    }
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// __pycache__

#[test]
fn pycache_in_a_project_is_s0() {
    let mut fx = Fixture::new().unwrap();
    project(&mut fx, "app", &[("uv.lock", "version = 1\n")]);
    pycache(&mut fx, "app/src/app", "core", false);
    pycache(&mut fx, "app/src/app/sub", "util", false);
    pycache(&mut fx, "app/tests", "test_core", false);
    age_tree(&mut fx, "app", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        [
            "app/src/app/__pycache__",
            "app/src/app/sub/__pycache__",
            "app/tests/__pycache__"
        ]
    );
    let mut cands = Vec::new();
    for a in &s.found {
        assert_eq!(a.rule.id, "python.pycache");
        assert_eq!(
            a.projects[0].name, "app",
            "project root is the manifest's folder"
        );
        assert_eq!(a.signals.regen, Regen::Local);
        assert_eq!(markers(a, &root).last().unwrap(), "app/pyproject.toml");
        let c = classify_expect(a, &clean_caller(a), Tier::S0);
        if c.tier == Tier::S0 {
            assert_eq!(c.action, Action::Delete);
        }
        cands.push(c);
    }
    let sub = s.at(&root, "app/src/app/sub/__pycache__");
    assert_eq!(lockfiles(sub, &root), ["app/src/app/sub/util.py"]);
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn pycache_without_sources_or_outside_a_project() {
    let mut fx = Fixture::new().unwrap();
    // Sourceless bytecode: `secret.pyc` has no `secret.py` (the package's `__init__.py` doesn't
    // count for it), so the .pyc may be the only copy of that code.
    project(&mut fx, "dist", &[]);
    pycache(&mut fx, "dist/src/app", "secret", true);
    pycache(&mut fx, "dist/src/app", "__init__", false);
    // Something that is not bytecode inside `__pycache__`.
    project(&mut fx, "odd", &[]);
    pycache(&mut fx, "odd/src/app", "core", false);
    fx.file("odd/src/app/__pycache__/notes.txt", "mine")
        .unwrap();
    // No manifest within reach: not a candidate.
    pycache(&mut fx, "scratch/notebooks", "helpers", false);
    pycache(&mut fx, "deep/a/b/c/d/e", "m", false);
    fx.file("deep/pyproject.toml", PYPROJECT).unwrap();
    // A vendored tree named site-packages belongs to its environment.
    project(&mut fx, "vend", &[]);
    pycache(&mut fx, "vend/site-packages/pkg", "mod", false);
    // A `__pycache__` without any bytecode is not one.
    fx.file("dist/src/other/__pycache__/notes.txt", "x")
        .unwrap();
    fx.file("dist/src/other/x.py", "").unwrap();
    age_tree(&mut fx, "dist", days_ago(3));
    age_tree(&mut fx, "odd", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        ["dist/src/app/__pycache__", "odd/src/app/__pycache__"]
    );
    let dist = s.at(&root, "dist/src/app/__pycache__");
    assert!(!dist.signals.lockfile_present);
    assert!(lockfiles(dist, &root).is_empty());
    assert!(
        notes(dist)
            .iter()
            .any(|n| n.contains("secret.cpython-312.pyc") && n.contains("only copy"))
    );
    let odd = s.at(&root, "odd/src/app/__pycache__");
    assert!(!odd.signals.lockfile_present);
    assert!(notes(odd).iter().any(|n| n.contains("`notes.txt`")));
    let cands = vec![
        classify_expect(dist, &clean_caller(dist), Tier::S2),
        classify_expect(odd, &clean_caller(odd), Tier::S2),
    ];
    check(&fx, &plan_inputs(&cands));
}

// ---------------------------------------------------------------------------------------------
// Tool caches, tox, nox

#[test]
fn tool_caches_in_a_project_are_s0() {
    let mut fx = Fixture::new().unwrap();
    project(&mut fx, "app", &[]);
    tool_cache(&mut fx, "app/.pytest_cache", true);
    tool_cache(&mut fx, "app/.mypy_cache", true);
    tool_cache(&mut fx, "app/.ruff_cache", true);
    // Config-only projects count too.
    fx.file("lint/ruff.toml", "line-length = 100\n").unwrap();
    tool_cache(&mut fx, "lint/.ruff_cache", true);
    fx.file("typed/mypy.ini", "[mypy]\n").unwrap();
    tool_cache(&mut fx, "typed/.mypy_cache", true);
    fx.file("tests-only/pytest.ini", "[pytest]\n").unwrap();
    tool_cache(&mut fx, "tests-only/.pytest_cache", true);
    for p in ["app", "lint", "typed", "tests-only"] {
        age_tree(&mut fx, p, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        [
            "app/.mypy_cache",
            "app/.pytest_cache",
            "app/.ruff_cache",
            "lint/.ruff_cache",
            "tests-only/.pytest_cache",
            "typed/.mypy_cache"
        ]
    );
    let ids: Vec<&str> = ["app/.mypy_cache", "app/.pytest_cache", "app/.ruff_cache"]
        .iter()
        .map(|p| s.at(&root, p).rule.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "python.mypy-cache",
            "python.pytest-cache",
            "python.ruff-cache"
        ]
    );
    let cands: Vec<_> = s
        .found
        .iter()
        .map(|a| classify_expect(a, &clean_caller(a), Tier::S0))
        .collect();
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn tool_caches_need_their_tag_and_a_project() {
    let mut fx = Fixture::new().unwrap();
    project(&mut fx, "app", &[]);
    tool_cache(&mut fx, "app/.pytest_cache", false); // untagged: unknown writer
    tool_cache(&mut fx, "loose/.mypy_cache", true); // no project marker beside it
    tool_cache(&mut fx, "loose/.ruff_cache", true);
    fx.dir("app/.tox/py312").unwrap(); // `.tox` needs tox.ini/pyproject/setup.cfg: app has one
    fx.dir("bare/.tox/py312").unwrap();
    fx.dir("bare/.nox/tests").unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["app/.tox"]);
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn tox_and_nox_environments_are_s1() {
    let mut fx = Fixture::new().unwrap();
    fx.file("lib/tox.ini", "[tox]\nenv_list = py312\n").unwrap();
    venv(&mut fx, "lib/.tox/py312");
    fx.file("svc/noxfile.py", "import nox\n").unwrap();
    venv(&mut fx, "svc/.nox/tests");
    age_tree(&mut fx, "lib", days_ago(3));
    age_tree(&mut fx, "svc", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["lib/.tox", "svc/.nox"]);
    let tox = s.at(&root, "lib/.tox");
    let nox = s.at(&root, "svc/.nox");
    assert_eq!(tox.rule.id, "python.tox");
    assert_eq!(nox.rule.id, "python.nox");
    assert_eq!(markers(tox, &root), ["lib/tox.ini"]);
    assert_eq!(markers(nox, &root), ["svc/noxfile.py"]);
    let cands = vec![
        classify_expect(tox, &clean_caller(tox), Tier::S1),
        classify_expect(nox, &clean_caller(nox), Tier::S1),
    ];
    check(&fx, &plan_inputs(&cands));
}

/// A realistic project: one candidate per artifact, nothing found inside the environment, and
/// the sources, manifest and lockfile are never candidates.
#[test]
fn full_python_project_scan() {
    let mut fx = Fixture::new().unwrap();
    project(
        &mut fx,
        "app",
        &[("uv.lock", "version = 1\n"), ("tox.ini", "[tox]\n")],
    );
    venv(&mut fx, "app/.venv");
    pycache(&mut fx, "app/src/app", "core", false);
    tool_cache(&mut fx, "app/.pytest_cache", true);
    tool_cache(&mut fx, "app/.mypy_cache", true);
    tool_cache(&mut fx, "app/.ruff_cache", true);
    venv(&mut fx, "app/.tox/py312");
    age_tree(&mut fx, "app", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        [
            "app/.mypy_cache",
            "app/.pytest_cache",
            "app/.ruff_cache",
            "app/.tox",
            "app/.venv",
            "app/src/app/__pycache__"
        ]
    );
    assert!(
        s.visited
            .iter()
            .all(|v| !v.starts_with(root.join("app/.venv/lib"))),
        "the walk never descends into a detected environment"
    );
    let want = |p: &str| match p {
        "app/.tox" | "app/.venv" => Tier::S1,
        _ => Tier::S0,
    };
    let cands: Vec<_> = s
        .found
        .iter()
        .map(|a| classify_expect(a, &clean_caller(a), want(&rel(&root, &a.path))))
        .collect();
    check(&fx, &plan_inputs(&cands));
}
