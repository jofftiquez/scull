//! The read-only worktree pipeline for end-to-end FX-GIT fixtures: the real scanner finds the
//! repositories under the scan root (any directory holding a `.git` entry, and bare
//! repositories), `scull-git` discovers and analyses every worktree (hardened `git`, never
//! fetching), the in-use signal comes from `scull-safety`, and the GIT-03 decision table
//! classifies. Sizes come from the scanner's summaries.
//!
//! Like [`super::Pipeline`], this stands in for `scull-engine` (#56) until it lands.

#![allow(dead_code)]

use super::{IdleProbe, InUseSource, scan};
use scull_core::{Candidate, FileIdentity, ItemId, Sizes, Tier};
use scull_detectors::rules::{MAX_UP, RuleSet, Surroundings};
use scull_fixtures::{Fixture, GitRepo};
use scull_git::{
    AnalyzeOptions, ArtifactMatcher, GitRunner, Observed, RepoWorktrees, WorktreeDecision,
    WorktreeState, analyze_repo, classify_worktree, discover, sample_activity, worktree_candidate,
};
use scull_platform::{EntryKind, Native, Platform};
use scull_safety::{ActivityProbe, NativeActivityProbe, check_in_use};
use scull_scanner::{DirSummary, EstimateContext, ScanEvent, Snapshots, VolumeFs, estimate};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Canonical form for comparing paths Git recorded with paths the scanner saw (macOS
/// `/var` → `/private/var`, Windows 8.3 short names, no `\\?\`). A missing path resolves its
/// nearest existing ancestor.
pub fn canon(p: &Path) -> PathBuf {
    let c = match std::fs::canonicalize(p) {
        Ok(c) => c,
        Err(_) => match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) if parent != p => return canon(parent).join(name),
            _ => p.to_path_buf(),
        },
    };
    let s = c.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => c,
    }
}

/// `p` with forward slashes (Git accepts them on every OS).
pub fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
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

/// A fixture with a repository `repo` (one commit on `main`), optionally pushed to a bare
/// `remote.git` (`origin/HEAD` → `origin/main`). Worktrees are added beside `repo`.
pub struct GitWorld {
    pub fx: Fixture,
    pub repo: GitRepo,
}

impl GitWorld {
    /// A repository without a remote.
    pub fn local() -> Self {
        let mut fx = Fixture::new().expect("fixture");
        let repo = fx.git_repo("repo").expect("git repo");
        fx.file("repo/README.md", "hello\n").expect("write");
        fx.file("repo/src/lib.rs", "pub fn f() {}\n")
            .expect("write");
        repo.commit_all("initial").expect("commit");
        Self { fx, repo }
    }

    /// A repository whose `main` is pushed to a bare remote.
    pub fn with_remote() -> Self {
        let mut w = Self::local();
        w.fx.dir("remote.git").expect("mkdir");
        w.git_in("remote.git", &["init", "--quiet", "--bare"]);
        let remote = slashed(&w.fx.path("remote.git").expect("path"));
        w.git(&["remote", "add", "origin", &remote]);
        w.git(&["push", "--quiet", "origin", "main"]);
        w.git(&["remote", "set-head", "origin", "main"]);
        w
    }

    /// `git <args>` in the main repository.
    pub fn git(&self, args: &[&str]) -> String {
        self.repo.run(args).expect("git")
    }

    /// `git <args>` in `rel` (relative to the fixture root).
    pub fn git_in(&self, rel: &str, args: &[&str]) -> String {
        run(self.fx.git_command(rel, args).expect("git command"))
    }

    /// `git worktree add -b <branch> ../<name> <start>` (`--detach` when `branch` is `None`).
    pub fn add_worktree(&self, name: &str, branch: Option<&str>, start: &str) -> PathBuf {
        let target = format!("../{name}");
        match branch {
            Some(b) => self.git(&["worktree", "add", "--quiet", "-b", b, &target, start]),
            None => self.git(&["worktree", "add", "--quiet", "--detach", &target, start]),
        };
        self.fx.path(name).expect("path")
    }

    /// Writes `contents` to `rel` in worktree `wt` and commits everything there.
    pub fn commit_in(&mut self, wt: &str, rel: &str, contents: &str, msg: &str) -> String {
        self.fx
            .file(format!("{wt}/{rel}"), contents)
            .expect("write");
        self.git_in(wt, &["add", "-A"]);
        self.git_in(wt, &["commit", "--quiet", "-m", msg]);
        self.git_in(wt, &["rev-parse", "HEAD"])
    }
}

/// An [`ArtifactMatcher`] over detector rules: an ignored folder is an artifact when a rule's
/// name, marker and exclusion requirements hold for it, judged from the entry names around it.
pub struct RuleMatcher(pub RuleSet);

fn names_in(dir: &Path) -> Option<Vec<String>> {
    let it = Native.read_dir(dir).ok()?;
    Some(
        it.filter_map(|e| e.ok()?.file_name.into_string().ok())
            .collect(),
    )
}

impl ArtifactMatcher for RuleMatcher {
    fn match_ignored(&self, worktree: &Path, rel: &str) -> Option<String> {
        let rel = rel.strip_suffix('/')?; // only whole folders can be artifacts
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

/// One classified worktree.
#[derive(Debug, Clone)]
pub struct WtItem {
    /// GIT-02 state.
    pub state: WorktreeState,
    /// GIT-03 decision.
    pub decision: WorktreeDecision,
    /// The candidate (sizes from the scan).
    pub candidate: Candidate,
    /// The in-use signal it was classified with.
    pub in_use: scull_git::Tri,
}

impl WtItem {
    /// Whether a reason with `code` fired at exactly `tier`.
    pub fn has_reason(&self, code: scull_core::ReasonCode, tier: Tier) -> bool {
        self.decision
            .reasons
            .iter()
            .any(|r| r.code == code && r.tier == tier)
    }
}

/// Everything one worktree-pipeline run produced.
#[derive(Debug)]
pub struct WtRun {
    /// The scan root (canonical).
    pub root: PathBuf,
    /// Every worktree of every repository found, main/bare entries included.
    pub items: Vec<WtItem>,
    /// Repositories found (by common dir).
    pub repos: Vec<RepoWorktrees>,
    /// Wall-clock time of scan + discovery + analysis + classification.
    pub elapsed: Duration,
}

impl WtRun {
    /// The worktree whose folder is `root/rel` (canonical comparison).
    pub fn at(&self, rel: &str) -> &WtItem {
        let want = canon(&self.root.join(rel));
        let hits: Vec<&WtItem> = self
            .items
            .iter()
            .filter(|i| canon(&i.state.path.to_path_buf()) == want)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "expected one worktree at {rel}: {:#?}",
            self.paths()
        );
        hits[0]
    }

    /// Worktree folders, canonical.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.items
            .iter()
            .map(|i| canon(&i.state.path.to_path_buf()))
            .collect()
    }

    /// The executable plan: candidates with a mutating action.
    pub fn plan(&self) -> Vec<&Candidate> {
        self.items
            .iter()
            .map(|i| &i.candidate)
            .filter(|c| c.action.is_mutating())
            .collect()
    }

    /// Tier of every item by ID.
    pub fn tiers(&self) -> BTreeMap<ItemId, Tier> {
        self.items
            .iter()
            .map(|i| (i.candidate.id.clone(), i.candidate.tier))
            .collect()
    }
}

/// The worktree pipeline configuration.
pub struct WorktreePipeline {
    root: PathBuf,
    in_use: InUseSource,
    rules: RuleSet,
}

impl WorktreePipeline {
    /// Scan `root` with the bundled artifact rules and [`InUseSource::IdleFixture`].
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            in_use: InUseSource::IdleFixture,
            rules: RuleSet::bundled().expect("bundled rules"),
        }
    }

    /// Where the in-use signal comes from.
    pub fn in_use(mut self, source: InUseSource) -> Self {
        self.in_use = source;
        self
    }

    /// Run scan → find repositories → discover → analyse → in-use → classify. Read-only.
    pub fn run(&self) -> WtRun {
        let started = Instant::now();
        let (_, events) = scan(&self.root);
        let mut summaries: HashMap<PathBuf, DirSummary> = HashMap::new();
        let mut repo_dirs: Vec<PathBuf> = Vec::new();
        for e in events {
            match e {
                ScanEvent::Listing(l) => {
                    let names: Vec<&str> =
                        l.entries.iter().filter_map(|e| e.name.to_str()).collect();
                    let has_git = l.entries.iter().any(|e| {
                        e.name == ".git" && matches!(e.kind, EntryKind::Dir | EntryKind::File)
                    });
                    let bare = ["HEAD", "objects", "refs"]
                        .iter()
                        .all(|n| names.contains(n));
                    if has_git || bare {
                        repo_dirs.push(l.path.clone());
                    }
                }
                ScanEvent::DirDone(s) => {
                    summaries.insert(canon(&s.path), s);
                }
                _ => {}
            }
        }

        // One entry per repository, by common dir.
        let mut repos: Vec<RepoWorktrees> = Vec::new();
        repo_dirs.sort();
        for d in repo_dirs {
            let Ok(r) = discover(&d) else { continue };
            if !repos.iter().any(|x| x.common_dir == r.common_dir) {
                repos.push(r);
            }
        }

        let runner = GitRunner::new().expect("trusted git");
        let probe: Box<dyn ActivityProbe> = match self.in_use {
            InUseSource::Native => Box::new(NativeActivityProbe::capture(Default::default())),
            InUseSource::IdleFixture => Box::new(IdleProbe),
        };
        let matcher = RuleMatcher(self.rules.clone());
        let mut items = Vec::new();
        for repo in &repos {
            let states = analyze_repo(
                &runner,
                repo,
                |wt| {
                    if wt.prunable.is_some() {
                        Observed::unknown("folder missing")
                    } else {
                        sample_activity(&Native, &wt.path.to_path_buf(), 10_000)
                    }
                },
                &AnalyzeOptions::default(),
            );
            for st in states {
                let in_use = if st.missing {
                    scull_git::Tri::Unknown
                } else {
                    match check_in_use(&st.path.to_path_buf(), &[], probe.as_ref()).in_use {
                        scull_safety::Tri::No => scull_git::Tri::No,
                        scull_safety::Tri::Yes => scull_git::Tri::Yes,
                        scull_safety::Tri::Unknown => scull_git::Tri::Unknown,
                    }
                };
                let decision = classify_worktree(&st, in_use, &matcher);
                let at = if st.missing {
                    st.git_dir.to_path_buf()
                } else {
                    st.path.to_path_buf()
                };
                let identity = Native
                    .metadata(&at)
                    .ok()
                    .and_then(|m| m.identity)
                    .unwrap_or(FileIdentity::new(0, 0));
                let sizes = match (st.missing, summaries.get(&canon(&at))) {
                    (false, Some(s)) => estimate(
                        s,
                        &EstimateContext::new(VolumeFs::Unknown, Snapshots::Unknown),
                    ),
                    _ => Sizes::default(),
                };
                let candidate = worktree_candidate(&st, &decision, in_use, identity, sizes);
                assert!(candidate.is_consistent(), "{candidate:#?}");
                assert_eq!(candidate.tier, decision.tier);
                items.push(WtItem {
                    state: st,
                    decision,
                    candidate,
                    in_use,
                });
            }
        }
        WtRun {
            root: canon(&self.root),
            items,
            repos,
            elapsed: started.elapsed(),
        }
    }
}

/// Run the universal invariants for a worktree fixture: I-1 on the plan, I-2, I-3 (only
/// `allowed` may differ) and I-8 between `scan` and `revalidated`; each must run and pass.
pub fn check_wt(
    fx: &Fixture,
    scan: &WtRun,
    revalidated: &WtRun,
    allowed: &[&str],
) -> scull_fixtures::InvariantReport {
    let mut inputs = scull_fixtures::InvariantInputs::new()
        .plan(scan.plan().into_iter().map(scull_fixtures::PlanEntry::from))
        .tiers(scan.tiers(), revalidated.tiers());
    for a in allowed {
        inputs = inputs.allow(*a);
    }
    let report = fx.check_invariants(&inputs);
    report.assert_ok();
    let ran = report.ran();
    for id in [
        scull_fixtures::InvariantId::I1,
        scull_fixtures::InvariantId::I2,
        scull_fixtures::InvariantId::I3,
        scull_fixtures::InvariantId::I6,
        scull_fixtures::InvariantId::I8,
    ] {
        assert!(ran.contains(&id), "{id} ran");
    }
    report
}

/// Run `pipeline` twice (scan, revalidation), check the invariants with nothing allowed to
/// change, and return the scan-time run.
pub fn run_wt_checked(fx: &Fixture, pipeline: &WorktreePipeline) -> WtRun {
    let scan = pipeline.run();
    let revalidated = pipeline.run();
    check_wt(fx, &scan, &revalidated, &[]);
    scan
}
