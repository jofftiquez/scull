//! Worktree state analysis (GIT-02).
//!
//! For each worktree found by [`discover`](crate::discover), [`analyze_worktree`] reports what
//! the removal decision (GIT-03) needs: parent repository, path, branch/HEAD, upstream, lock
//! state and reason, last observed activity, uncommitted tracked changes, untracked
//! (non-ignored) files, ignored files by name, submodule dirtiness, commits not reachable from
//! any remote-tracking ref, whether HEAD is merged **and how that was determined**, and whether
//! a detached HEAD is reachable from any ref.
//!
//! # Unknown is never clean
//!
//! Every fact that needs Git is an [`Observed`] value. If a call fails for any reason (Git
//! missing, timeout, dubious ownership, unsafe config, unreadable repository, unexpected
//! output) the fact is [`Observed::Unknown`] with the reason, never an empty list or "clean".
//! The classifier treats every unknown protectively.
//!
//! # Never fetch (THREAT_MODEL T-02, FX-GIT-12)
//!
//! Only the local repository is read, through the hardened [`GitRunner`], which disables every
//! network transport. Remote-tracking refs are used as they are: when they are stale, commits
//! that were pushed later look unpushed, which errs on the protective side. No forge is
//! consulted (GIT-05, D-9).
//!
//! # Known limitations (all err towards "not clean")
//!
//! * Global excludes are disabled by the runner, so globally ignored files show as untracked.
//! * Filter drivers are disabled, so filtered files (e.g. Git LFS) may show as modified.
//! * A submodule's own unpushed commits are not inspected; a changed submodule commit shows up
//!   as a tracked change of the superproject, and a submodule's work tree is checked for local
//!   changes (FX-GIT-13).

use crate::activity::Activity;
use crate::refs::{ObjectId, RefEntry};
use crate::runner::GitRunner;
use crate::status::{IgnoredMode, Status, StatusEntry, StatusOptions, UntrackedMode};
use crate::worktree::{LockState, RepoWorktrees, Worktree, WorktreeHead, WorktreeKind};
use schemars::JsonSchema;
use scull_core::FsPath;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// A fact that may not have been observable. `Unknown` is never treated as the safe answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Observed<T> {
    /// The fact was observed.
    Known {
        /// The value.
        value: T,
    },
    /// The fact could not be observed.
    Unknown {
        /// Why, e.g. the Git error.
        reason: String,
    },
}

impl<T> Observed<T> {
    /// An observed value.
    pub fn known(value: T) -> Self {
        Observed::Known { value }
    }

    /// An unobservable fact.
    pub fn unknown(reason: impl Into<String>) -> Self {
        Observed::Unknown {
            reason: reason.into(),
        }
    }

    /// The value, if known.
    pub fn value(&self) -> Option<&T> {
        match self {
            Observed::Known { value } => Some(value),
            Observed::Unknown { .. } => None,
        }
    }

    /// The reason, if unknown.
    pub fn unknown_reason(&self) -> Option<&str> {
        match self {
            Observed::Known { .. } => None,
            Observed::Unknown { reason } => Some(reason),
        }
    }

    /// Whether the fact is unknown.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Observed::Unknown { .. })
    }

    fn from_result<E: std::fmt::Display>(r: Result<T, E>) -> Self {
        match r {
            Ok(v) => Observed::known(v),
            Err(e) => Observed::unknown(e.to_string()),
        }
    }
}

/// The repository a worktree belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ParentRepo {
    /// The common Git dir (identifies the repository).
    pub common_dir: FsPath,
    /// Whether the repository is bare.
    pub bare: bool,
    /// The main working tree, for a non-bare repository.
    pub main_worktree: Option<FsPath>,
}

/// What HEAD points at, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HeadState {
    /// A branch is checked out.
    Branch {
        /// Full ref name, e.g. `refs/heads/feature`.
        name: String,
        /// The commit it points at; `None` for an unborn branch (no commits yet).
        oid: Option<String>,
    },
    /// HEAD is detached.
    Detached {
        /// The commit.
        oid: String,
    },
}

impl HeadState {
    /// The commit HEAD points at, if any.
    pub fn oid(&self) -> Option<&str> {
        match self {
            HeadState::Branch { oid, .. } => oid.as_deref(),
            HeadState::Detached { oid } => Some(oid),
        }
    }

    /// The short branch name, when a branch is checked out.
    pub fn branch(&self) -> Option<&str> {
        match self {
            HeadState::Branch { name, .. } => {
                Some(name.strip_prefix("refs/heads/").unwrap_or(name))
            }
            HeadState::Detached { .. } => None,
        }
    }
}

/// The configured upstream of the checked-out branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Upstream {
    /// Full ref name, e.g. `refs/remotes/origin/feature`.
    pub name: String,
    /// Whether the upstream ref exists locally (it is gone after the remote branch was deleted
    /// and pruned).
    pub exists: bool,
    /// Commits on HEAD not on the upstream, when known.
    pub ahead: Option<u64>,
    /// Commits on the upstream not on HEAD, when known.
    pub behind: Option<u64>,
}

/// A list of worktree-relative paths, capped for display.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct PathList {
    /// The paths (`/`-separated, relative to the worktree; a trailing `/` marks a whole folder
    /// Git collapsed into one entry). At most [`AnalyzeOptions::max_paths`].
    pub paths: Vec<String>,
    /// How many entries Git reported in total.
    pub total: u64,
    /// Whether `paths` was cut short.
    pub truncated: bool,
}

impl PathList {
    fn collect<'a>(entries: impl Iterator<Item = &'a StatusEntry>, max: usize) -> Self {
        let mut list = PathList::default();
        for e in entries {
            list.total += 1;
            if list.paths.len() < max {
                list.paths.push(e.path.to_string_lossy());
            } else {
                list.truncated = true;
            }
        }
        list
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

/// A submodule of the worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Submodule {
    /// Path relative to the worktree.
    pub path: String,
    /// Whether it is checked out (has a `.git`). An uninitialized submodule has no local files.
    pub initialized: bool,
    /// Whether its work tree (or a nested submodule's) has tracked changes or untracked files.
    pub dirty: Observed<bool>,
}

/// Commits reachable from HEAD but from no remote-tracking ref.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct CommitList {
    /// How many (a lower bound when `truncated`).
    pub count: u64,
    /// The newest ones, at most [`AnalyzeOptions::max_listed_commits`].
    pub oids: Vec<String>,
    /// Whether counting stopped at [`AnalyzeOptions::max_commits`].
    pub truncated: bool,
}

impl CommitList {
    /// Whether every commit on HEAD is on some remote-tracking ref.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

/// Whether HEAD is merged, and **how that was determined** (GIT-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum MergeStatus {
    /// Proven: HEAD is an ancestor of a default-branch ref.
    AncestorOfDefault {
        /// The default-branch ref, e.g. `refs/remotes/origin/main`.
        default_ref: String,
    },
    /// Proven pushed: HEAD is reachable from a remote-tracking ref (so every commit on it is).
    ReachableFromPushedRef {
        /// Some of the remote-tracking refs that contain HEAD (may be empty if listing them
        /// failed after reachability was established).
        refs: Vec<String>,
    },
    /// Heuristic only: every commit on HEAD missing from the default branch has a
    /// patch-equivalent commit there (`git cherry` / patch-id), as after a squash or rebase
    /// merge. Not proof (GIT-03: S2 "merge unproven").
    PatchIdOnly {
        /// The default-branch ref compared against.
        default_ref: String,
        /// How many commits were patch-equivalent.
        commits: u64,
    },
    /// The branch has no commits (unborn): nothing can be lost.
    NoCommits,
    /// No proof and no heuristic match against any default-branch ref.
    NotMerged {
        /// The default-branch refs checked.
        checked: Vec<String>,
    },
    /// Couldn't be determined (Git error, no default branch found, HEAD unknown).
    Unknown {
        /// Why.
        reason: String,
    },
}

impl MergeStatus {
    /// Whether this is proof that nothing on HEAD would be lost: merged into a default branch,
    /// reachable from a pushed ref, or no commits at all.
    pub fn is_proven(&self) -> bool {
        matches!(
            self,
            MergeStatus::AncestorOfDefault { .. }
                | MergeStatus::ReachableFromPushedRef { .. }
                | MergeStatus::NoCommits
        )
    }
}

/// Whether HEAD's commit is reachable from any ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reachability {
    /// Reachable from these refs (a branch HEAD is reachable from its branch).
    ReachableFrom {
        /// Some of the refs (at most [`AnalyzeOptions::max_paths`]).
        refs: Vec<String>,
        /// How many refs contain it.
        total: u64,
    },
    /// Reachable from no ref: only the worktree's detached HEAD keeps it (FX-GIT-07).
    Unreachable,
    /// HEAD has no commit (unborn branch).
    NoCommit,
}

/// Everything GIT-02 reports about one worktree. Serializable for `--json` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeState {
    /// The parent repository.
    pub repo: ParentRepo,
    /// Main, bare or linked.
    pub kind: WorktreeKind,
    /// Admin-dir name (linked worktrees).
    pub name: Option<String>,
    /// The worktree folder.
    pub path: FsPath,
    /// The worktree's Git dir.
    pub git_dir: FsPath,
    /// The worktree folder no longer exists: a stale registration that `git worktree prune`
    /// would remove (metadata only, FX-GIT-10). Nothing else was analysed.
    pub missing: bool,
    /// Branch or detached HEAD.
    pub head: Observed<HeadState>,
    /// Upstream of the checked-out branch (`None` when detached or not configured).
    pub upstream: Observed<Option<Upstream>>,
    /// Lock state and reason (FX-GIT-09).
    pub lock: LockState,
    /// Last observed activity (supplied by the caller, see [`sample_activity`]).
    ///
    /// [`sample_activity`]: crate::sample_activity
    pub activity: Observed<Activity>,
    /// Tracked paths with staged or unstaged changes, including conflicts and changed
    /// submodule commits (FX-GIT-02).
    pub tracked_changes: Observed<PathList>,
    /// Untracked, non-ignored paths (FX-GIT-03).
    pub untracked: Observed<PathList>,
    /// Ignored paths, by name (FX-GIT-04). Folders Git ignores as a whole appear once, as
    /// `dir/`.
    pub ignored: Observed<PathList>,
    /// Submodules and whether their work trees have local changes (FX-GIT-13).
    pub submodules: Observed<Vec<Submodule>>,
    /// Commits not reachable from any remote-tracking ref (FX-GIT-05, -11, -12).
    pub unpushed: Observed<CommitList>,
    /// Merge status and how it was determined (FX-GIT-01, -06).
    pub merge: MergeStatus,
    /// Whether HEAD's commit is reachable from any ref (FX-GIT-07, -08).
    pub head_reachability: Observed<Reachability>,
}

impl WorktreeState {
    /// Whether any submodule is dirty: `Some(true)` if one is, `None` if that's unknown for
    /// any, `Some(false)` otherwise.
    pub fn submodules_dirty(&self) -> Option<bool> {
        let subs = self.submodules.value()?;
        let mut unknown = false;
        for s in subs {
            match s.dirty.value() {
                Some(true) => return Some(true),
                Some(false) => {}
                None => unknown = true,
            }
        }
        (!unknown).then_some(false)
    }
}

/// Bounds for [`analyze_worktree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalyzeOptions {
    /// Paths listed per [`PathList`] (and refs per [`Reachability`]).
    pub max_paths: usize,
    /// Unpushed commits counted before stopping.
    pub max_commits: usize,
    /// Unpushed commit ids listed.
    pub max_listed_commits: usize,
    /// Submodule nesting depth inspected; deeper initialized submodules make the result
    /// unknown.
    pub max_submodule_depth: usize,
    /// Run the patch-id heuristic (`git cherry`) for unpushed branches. Local only; off makes
    /// such branches [`MergeStatus::NotMerged`] (more protective).
    pub patch_id: bool,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            max_paths: 1000,
            max_commits: 10_000,
            max_listed_commits: 20,
            max_submodule_depth: 4,
            patch_id: true,
        }
    }
}

/// Branch names tried as the default branch, in order, after the remotes' `HEAD` symrefs.
const DEFAULT_BRANCH_NAMES: &[&str] = &["main", "master"];

/// Analyses every worktree of `repo` (problems from discovery are not worktrees and are
/// skipped; callers report them as protected). `activity` supplies each worktree's last
/// observed activity, e.g. with [`sample_activity`](crate::sample_activity).
pub fn analyze_repo(
    runner: &GitRunner,
    repo: &RepoWorktrees,
    mut activity: impl FnMut(&Worktree) -> Observed<Activity>,
    opts: &AnalyzeOptions,
) -> Vec<WorktreeState> {
    repo.worktrees
        .iter()
        .map(|wt| analyze_worktree(runner, repo, wt, activity(wt), opts))
        .collect()
}

/// Analyses one worktree of `repo` (see the module docs). Read-only; never fetches.
pub fn analyze_worktree(
    runner: &GitRunner,
    repo: &RepoWorktrees,
    wt: &Worktree,
    activity: Observed<Activity>,
    opts: &AnalyzeOptions,
) -> WorktreeState {
    let parent = ParentRepo {
        common_dir: repo.common_dir.clone(),
        bare: repo.bare,
        main_worktree: repo
            .worktrees
            .iter()
            .find(|w| w.kind == WorktreeKind::Main)
            .map(|w| w.path.clone()),
    };
    let mut st = WorktreeState {
        repo: parent,
        kind: wt.kind,
        name: wt.name.clone(),
        path: wt.path.clone(),
        git_dir: wt.git_dir.clone(),
        missing: wt.prunable.is_some(),
        head: Observed::unknown("not analysed"),
        upstream: Observed::unknown("not analysed"),
        lock: wt.lock.clone(),
        activity,
        tracked_changes: Observed::unknown("not analysed"),
        untracked: Observed::unknown("not analysed"),
        ignored: Observed::unknown("not analysed"),
        submodules: Observed::unknown("not analysed"),
        unpushed: Observed::unknown("not analysed"),
        merge: MergeStatus::Unknown {
            reason: "not analysed".into(),
        },
        head_reachability: Observed::unknown("not analysed"),
    };
    if st.missing {
        let why = "worktree folder is missing (stale registration)";
        st.set_all_unknown(why);
        return st;
    }
    let dir = wt.path.to_path_buf();

    // Work-tree facts.
    if wt.kind == WorktreeKind::Bare {
        let why = "bare repository: no working tree";
        st.tracked_changes = Observed::unknown(why);
        st.untracked = Observed::unknown(why);
        st.ignored = Observed::unknown(why);
        st.submodules = Observed::unknown(why);
        st.analyse_history(runner, &dir, wt, None, opts);
    } else {
        let status = runner.status(
            &dir,
            &StatusOptions {
                untracked: UntrackedMode::Normal,
                ignored: IgnoredMode::Traditional,
            },
        );
        match &status {
            Ok(s) => {
                st.tracked_changes =
                    Observed::known(PathList::collect(s.tracked_changes(), opts.max_paths));
                st.untracked = Observed::known(PathList::collect(s.untracked(), opts.max_paths));
                st.ignored = Observed::known(PathList::collect(s.ignored(), opts.max_paths));
            }
            Err(e) => {
                let why = format!("git status failed: {e}");
                st.tracked_changes = Observed::unknown(why.clone());
                st.untracked = Observed::unknown(why.clone());
                st.ignored = Observed::unknown(why);
            }
        }
        st.submodules = submodules(runner, &dir, opts.max_submodule_depth);
        st.analyse_history(runner, &dir, wt, status.as_ref().ok(), opts);
    }
    st
}

impl WorktreeState {
    fn set_all_unknown(&mut self, why: &str) {
        self.head = Observed::unknown(why);
        self.upstream = Observed::unknown(why);
        self.tracked_changes = Observed::unknown(why);
        self.untracked = Observed::unknown(why);
        self.ignored = Observed::unknown(why);
        self.submodules = Observed::unknown(why);
        self.unpushed = Observed::unknown(why);
        self.merge = MergeStatus::Unknown { reason: why.into() };
        self.head_reachability = Observed::unknown(why);
    }

    /// HEAD, upstream, unpushed commits, merge status and reachability.
    fn analyse_history(
        &mut self,
        runner: &GitRunner,
        dir: &Path,
        wt: &Worktree,
        status: Option<&Status>,
        opts: &AnalyzeOptions,
    ) {
        // HEAD.
        let head: Result<(HeadState, Option<ObjectId>), String> = match &wt.head {
            WorktreeHead::Invalid { detail } => Err(format!("HEAD is invalid: {detail}")),
            WorktreeHead::Branch { name } => {
                match runner.rev_parse(dir, &format!("{name}^{{commit}}")) {
                    Ok(oid) => Ok((
                        HeadState::Branch {
                            name: name.clone(),
                            oid: oid.as_ref().map(|o| o.to_string()),
                        },
                        oid,
                    )),
                    Err(e) => Err(format!("resolving {name}: {e}")),
                }
            }
            WorktreeHead::Detached { oid } => match ObjectId::parse(oid) {
                None => Err(format!("HEAD is not an object id: {oid:?}")),
                Some(parsed) => match runner.rev_parse(dir, &format!("{parsed}^{{commit}}")) {
                    Ok(Some(o)) => Ok((HeadState::Detached { oid: o.to_string() }, Some(o))),
                    Ok(None) => Err(format!(
                        "HEAD commit {parsed} is missing from the repository"
                    )),
                    Err(e) => Err(format!("resolving HEAD: {e}")),
                },
            },
        };
        let (head, oid) = match head {
            Ok(h) => h,
            Err(why) => {
                self.head = Observed::unknown(why.clone());
                self.upstream = Observed::unknown(why.clone());
                self.unpushed = Observed::unknown(why.clone());
                self.merge = MergeStatus::Unknown {
                    reason: why.clone(),
                };
                self.head_reachability = Observed::unknown(why);
                return;
            }
        };
        self.head = Observed::known(head.clone());

        // Refs (one call): upstream and default-branch candidates.
        let refs = runner.for_each_ref(dir, &[]);
        self.upstream = match (&head, &refs) {
            (HeadState::Detached { .. }, _) => Observed::known(None),
            (HeadState::Branch { .. }, Err(e)) => Observed::unknown(format!("listing refs: {e}")),
            (HeadState::Branch { name, .. }, Ok(refs)) => {
                let up = refs
                    .iter()
                    .find(|r| &r.name == name)
                    .and_then(|r| r.upstream.clone());
                Observed::known(up.map(|up| {
                    let exists = refs.iter().any(|r| r.name == up);
                    let b = status.map(|s| &s.branch);
                    Upstream {
                        exists,
                        ahead: b.and_then(|b| b.ahead).filter(|_| exists),
                        behind: b.and_then(|b| b.behind).filter(|_| exists),
                        name: up,
                    }
                }))
            }
        };

        // Unpushed commits and reachability.
        let Some(oid) = oid else {
            self.unpushed = Observed::known(CommitList::default());
            self.merge = MergeStatus::NoCommits;
            self.head_reachability = Observed::known(Reachability::NoCommit);
            return;
        };
        self.unpushed = Observed::from_result(
            runner
                .commits_not_on_remotes(dir, &oid, Some(opts.max_commits.saturating_add(1)))
                .map(|c| CommitList {
                    count: c.len().min(opts.max_commits) as u64,
                    truncated: c.len() > opts.max_commits,
                    oids: c
                        .iter()
                        .take(opts.max_listed_commits)
                        .map(ToString::to_string)
                        .collect(),
                }),
        );
        self.head_reachability = match &head {
            HeadState::Branch { name, .. } => Observed::known(Reachability::ReachableFrom {
                refs: vec![name.clone()],
                total: 1,
            }),
            HeadState::Detached { .. } => {
                Observed::from_result(runner.refs_containing(dir, &oid, &[]).map(|r| {
                    if r.is_empty() {
                        Reachability::Unreachable
                    } else {
                        Reachability::ReachableFrom {
                            total: r.len() as u64,
                            refs: r.into_iter().take(opts.max_paths).map(|r| r.name).collect(),
                        }
                    }
                }))
            }
        };

        // Merge status.
        self.merge = match &refs {
            Err(e) => MergeStatus::Unknown {
                reason: format!("listing refs: {e}"),
            },
            Ok(refs) => merge_status(runner, dir, &head, &oid, refs, &self.unpushed, opts),
        };
    }
}

/// Default-branch candidates, most authoritative first: the remotes' `HEAD` targets, then
/// `<remote>/main|master`, then local `main|master` (never the worktree's own branch).
fn default_refs<'a>(refs: &'a [RefEntry], own_branch: Option<&str>) -> Vec<&'a RefEntry> {
    let mut out: Vec<&RefEntry> = Vec::new();
    let mut push = |r: &'a RefEntry| {
        if Some(r.name.as_str()) != own_branch
            && r.object_type == "commit"
            && !out.iter().any(|o| o.name == r.name)
        {
            out.push(r);
        }
    };
    let find = |name: &str| refs.iter().find(|r| r.name == name);
    for r in refs {
        if r.is_remote_tracking()
            && r.name.ends_with("/HEAD")
            && let Some(target) = r.symref.as_deref().and_then(find)
        {
            push(target);
        }
    }
    for r in refs {
        if let Some(rest) = r.name.strip_prefix("refs/remotes/")
            && let Some((_, branch)) = rest.rsplit_once('/')
            && DEFAULT_BRANCH_NAMES.contains(&branch)
        {
            push(r);
        }
    }
    for b in DEFAULT_BRANCH_NAMES {
        if let Some(r) = find(&format!("refs/heads/{b}")) {
            push(r);
        }
    }
    out
}

fn merge_status(
    runner: &GitRunner,
    dir: &Path,
    head: &HeadState,
    oid: &ObjectId,
    refs: &[RefEntry],
    unpushed: &Observed<CommitList>,
    opts: &AnalyzeOptions,
) -> MergeStatus {
    let own = match head {
        HeadState::Branch { name, .. } => Some(name.as_str()),
        HeadState::Detached { .. } => None,
    };
    let defaults = default_refs(refs, own);
    let mut errors: Vec<String> = Vec::new();

    // 1. Proof: ancestor of a default branch.
    for d in &defaults {
        match runner.is_ancestor(dir, oid, &d.oid) {
            Ok(true) => {
                return MergeStatus::AncestorOfDefault {
                    default_ref: d.name.clone(),
                };
            }
            Ok(false) => {}
            Err(e) => errors.push(format!("{}: {e}", d.name)),
        }
    }

    // 2. Proof: reachable from a remote-tracking ref.
    if unpushed.value().is_some_and(CommitList::is_empty) {
        let refs = runner
            .refs_containing(dir, oid, &["refs/remotes/"])
            .map(|r| r.into_iter().take(opts.max_paths).map(|r| r.name).collect())
            .unwrap_or_default();
        return MergeStatus::ReachableFromPushedRef { refs };
    }

    // 3. Heuristic: patch-equivalent to a default branch (squash/rebase merge).
    if opts.patch_id {
        for d in &defaults {
            match runner.cherry(dir, &d.oid, oid) {
                Ok(c) if !c.is_empty() && c.iter().all(|c| c.equivalent) => {
                    return MergeStatus::PatchIdOnly {
                        default_ref: d.name.clone(),
                        commits: c.len() as u64,
                    };
                }
                Ok(_) => {}
                Err(e) => errors.push(format!("{} (patch-id): {e}", d.name)),
            }
        }
    }

    if !errors.is_empty() {
        return MergeStatus::Unknown {
            reason: errors.join("; "),
        };
    }
    if defaults.is_empty() {
        return MergeStatus::Unknown {
            reason: "no default branch ref found (remote HEAD, main or master)".into(),
        };
    }
    MergeStatus::NotMerged {
        checked: defaults.iter().map(|d| d.name.clone()).collect(),
    }
}

/// Every submodule of the worktree at `dir` and whether its work tree has local changes.
fn submodules(runner: &GitRunner, dir: &Path, depth: usize) -> Observed<Vec<Submodule>> {
    let links = match runner.gitlinks(dir) {
        Ok(l) => l,
        Err(e) => return Observed::unknown(format!("listing submodules: {e}")),
    };
    Observed::known(
        links
            .iter()
            .map(|p| {
                let path = p.to_string_lossy();
                let Some(sub) = p.to_path(dir) else {
                    return Submodule {
                        path,
                        initialized: true,
                        dirty: Observed::unknown("submodule path is not valid UTF-8"),
                    };
                };
                if fs::symlink_metadata(sub.join(".git")).is_err() {
                    // Uninitialized: Git looks inside neither from the superproject nor from a
                    // submodule, so files someone put in the folder would go unseen.
                    let empty = match fs::read_dir(&sub) {
                        Ok(mut it) => it.next().is_none(),
                        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
                    };
                    return Submodule {
                        path,
                        initialized: false,
                        dirty: if empty {
                            Observed::known(false)
                        } else {
                            Observed::unknown("uninitialized submodule folder contains files")
                        },
                    };
                }
                let dirty = match depth.checked_sub(1) {
                    None => Observed::unknown("submodules nested too deeply to inspect"),
                    Some(left) => submodule_dirty(runner, &sub, left),
                };
                Submodule {
                    path,
                    initialized: true,
                    dirty,
                }
            })
            .collect(),
    )
}

/// Whether the submodule work tree at `dir`, or any initialized submodule nested in it (up to
/// `depth` more levels), has tracked changes or untracked files.
fn submodule_dirty(runner: &GitRunner, dir: &Path, depth: usize) -> Observed<bool> {
    let opts = StatusOptions {
        untracked: UntrackedMode::Normal,
        ignored: IgnoredMode::No,
    };
    match runner.status(dir, &opts) {
        Err(e) => return Observed::unknown(format!("git status in submodule failed: {e}")),
        Ok(s) if !s.is_clean() => return Observed::known(true),
        Ok(_) => {}
    }
    let nested = match submodules(runner, dir, depth) {
        Observed::Known { value } => value,
        Observed::Unknown { reason } => return Observed::unknown(reason),
    };
    let mut unknown: Option<String> = None;
    for n in &nested {
        match &n.dirty {
            Observed::Known { value: true } => return Observed::known(true),
            Observed::Known { value: false } => {}
            Observed::Unknown { reason } => unknown = Some(reason.clone()),
        }
    }
    match unknown {
        Some(r) => Observed::unknown(r),
        None => Observed::known(false),
    }
}
