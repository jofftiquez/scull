//! FX-GIT fixture builders shared by the state-analysis and classifier tests.
//!
//! Every fixture is built with `scull-fixtures` (real `git`, isolated environment, everything
//! inside a temporary directory with a canary beside it). Tests skip (pass) when `git` is not
//! installed.

#![allow(dead_code)]

use scull_fixtures::{Fixture, GitRepo, InvariantInputs};
use scull_git::{
    AnalyzeOptions, GitRunner, Observed, WorktreeState, analyze_worktree, discover, sample_activity,
};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether a usable `git` is on `PATH`.
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A repository `repo` (one commit on `main`) inside a fixture, optionally with a bare remote
/// `remote.git` that `main` was pushed to (`origin/HEAD` -> `origin/main`).
pub struct World {
    pub fx: Fixture,
    pub repo: GitRepo,
    pub runner: GitRunner,
}

/// Runs a prepared command and returns trimmed stdout; panics with stderr on failure.
pub fn run(mut c: Command) -> String {
    let out = c.output().expect("spawn git");
    assert!(
        out.status.success(),
        "{c:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl World {
    /// A repository with a pushed `main` and a bare remote, or `None` without `git`.
    pub fn with_remote() -> Option<Self> {
        let mut w = Self::local()?;
        w.fx.dir("remote.git").expect("mkdir");
        w.git_in("remote.git", &["init", "--quiet", "--bare"]);
        let remote = slashed(&w.fx.path("remote.git").expect("path"));
        w.git(&["remote", "add", "origin", &remote]);
        w.git(&["push", "--quiet", "origin", "main"]);
        w.git(&["remote", "set-head", "origin", "main"]);
        Some(w)
    }

    /// A repository without any remote, or `None` without `git`.
    pub fn local() -> Option<Self> {
        if !git_available() {
            eprintln!("skipping: `git` not found on PATH");
            return None;
        }
        let mut fx = Fixture::new().expect("fixture");
        let repo = fx.git_repo("repo").expect("git repo");
        fx.file("repo/README.md", "hello\n").expect("write");
        fx.file("repo/src/lib.rs", "pub fn f() {}\n")
            .expect("write");
        repo.commit_all("initial").expect("commit");
        let runner = GitRunner::new().expect("trusted git");
        Some(Self { fx, repo, runner })
    }

    /// `git <args>` in the main repository.
    pub fn git(&self, args: &[&str]) -> String {
        self.repo.run(args).expect("git")
    }

    /// `git <args>` in `rel` (relative to the fixture root).
    pub fn git_in(&self, rel: &str, args: &[&str]) -> String {
        run(self.fx.git_command(rel, args).expect("git command"))
    }

    /// `git worktree add -b <branch> ../<name> <start>` (or `--detach` when `branch` is `None`).
    pub fn add_worktree(&self, name: &str, branch: Option<&str>, start: &str) -> PathBuf {
        let target = format!("../{name}");
        match branch {
            Some(b) => self.git(&["worktree", "add", "--quiet", "-b", b, &target, start]),
            None => self.git(&["worktree", "add", "--quiet", "--detach", &target, start]),
        };
        self.fx.path(name).expect("path")
    }

    /// Writes `contents` to `rel` and commits everything in worktree `wt` (relative to root).
    pub fn commit_in(&mut self, wt: &str, rel: &str, contents: &str, msg: &str) -> String {
        self.fx
            .file(format!("{wt}/{rel}"), contents)
            .expect("write");
        self.git_in(wt, &["add", "-A"]);
        self.git_in(wt, &["commit", "--quiet", "-m", msg]);
        self.git_in(wt, &["rev-parse", "HEAD"])
    }

    /// Records the root baseline (I-3) once the fixture is built.
    pub fn checkpoint(&mut self) {
        self.fx.checkpoint().expect("checkpoint");
    }

    /// Discovers the repository and analyses the worktree at `rel` (relative to the root).
    pub fn analyze(&self, rel: &str) -> WorktreeState {
        self.analyze_with(rel, &AnalyzeOptions::default())
    }

    /// Like [`World::analyze`] with options.
    pub fn analyze_with(&self, rel: &str, opts: &AnalyzeOptions) -> WorktreeState {
        let repo = discover(self.repo.path()).expect("discover");
        let want = canon(&self.fx.root().join(rel));
        let wt = repo
            .worktrees
            .iter()
            .find(|w| canon(&w.path.to_path_buf()) == want)
            .unwrap_or_else(|| panic!("{rel} not among {repo:#?}"));
        let activity = if wt.prunable.is_some() {
            Observed::unknown("folder missing")
        } else {
            sample_activity(&scull_platform::Native, &wt.path.to_path_buf(), 10_000)
        };
        analyze_worktree(&self.runner, &repo, wt, activity, opts)
    }

    /// Runs the safety invariants for a read-only fixture (nothing in the root may change: in
    /// particular nothing was fetched) and asserts they hold.
    pub fn assert_invariants(&self, inputs: &InvariantInputs) {
        let report = self.fx.check_invariants(inputs);
        report.assert_ok();
    }
}

/// `p` with forward slashes (Git accepts them on every OS).
pub fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Canonical form for comparisons (resolves macOS `/var` -> `/private/var` and Windows 8.3
/// short names). For a path that doesn't exist, its nearest existing ancestor is resolved.
pub fn canon(p: &Path) -> PathBuf {
    let c = match std::fs::canonicalize(p) {
        Ok(c) => c,
        Err(_) => match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) if parent != p => {
                return canon(parent).join(name);
            }
            _ => p.to_path_buf(),
        },
    };
    let s = c.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => c,
    }
}

/// Helper: the paths in a known path list.
pub fn paths(o: &Observed<scull_git::PathList>) -> Vec<String> {
    o.value()
        .unwrap_or_else(|| panic!("expected a known list, got {o:?}"))
        .paths
        .clone()
}

/// An [`scull_git::ArtifactMatcher`] backed by the bundled detector rules: an ignored folder
/// is an artifact when a rule's name, marker and exclusion requirements hold for it, judged
/// from the entry names around it (as the scanner would supply them).
pub struct DetectorRules(pub scull_detectors::rules::RuleSet);

impl DetectorRules {
    pub fn bundled() -> Self {
        Self(scull_detectors::rules::RuleSet::bundled().expect("bundled rules"))
    }
}

fn names_in(dir: &Path) -> Option<Vec<String>> {
    let it = std::fs::read_dir(dir).ok()?;
    Some(
        it.filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect(),
    )
}

impl scull_git::ArtifactMatcher for DetectorRules {
    fn match_ignored(&self, worktree: &Path, rel: &str) -> Option<String> {
        use scull_detectors::rules::{MAX_UP, Surroundings};
        // Only whole folders can be artifacts.
        let rel = rel.strip_suffix('/')?;
        let dir = rel
            .split('/')
            .fold(worktree.to_path_buf(), |p, c| p.join(c));
        let name = dir.file_name()?.to_str()?.to_owned();
        let mut s = Surroundings::new(name);
        if let Some(inside) = names_in(&dir) {
            s = s.with_inside(inside);
        }
        let mut ancestors = Vec::new();
        let mut cur = dir.parent();
        for level in 1..=MAX_UP {
            let Some(p) = cur.filter(|p| p.starts_with(worktree)) else {
                break;
            };
            if let Some(entries) = names_in(p) {
                s = s.with_level(level, entries);
            }
            if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                ancestors.push(n.to_owned());
            }
            cur = p.parent();
        }
        let s = s.with_ancestor_names(ancestors);
        self.0
            .enabled()
            .find(|r| r.check_markers(&s).matched().is_some())
            .map(|r| r.id().to_owned())
    }
}
