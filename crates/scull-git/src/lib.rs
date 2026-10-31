//! Hardened, read-only Git worktree analysis (GIT-01…05, SAFE-15).
//!
//! * [`discover`] / [`list_worktrees`]: enumerate a repository's worktrees from its Git
//!   metadata, without running `git` (GIT-01). Handles bare repositories, locked worktrees
//!   (FX-GIT-09) and stale registrations (FX-GIT-10), and validates every metadata pointer so a
//!   crafted repository can't make an unrelated folder look like a worktree (FX-GIT-16).
//! * [`GitRunner`]: hardened access to the system `git` for state that needs Git itself
//!   (status incl. untracked/ignored files, refs, upstreams, reachability, merge bases). The
//!   binary comes from trusted `PATH` entries only, runs with a sanitized environment from a
//!   neutral folder, and repository config can't execute anything: fsmonitor, hooks, filter
//!   drivers, external diff, pagers, credential helpers and every network transport are
//!   disabled (SAFE-12, SAFE-15, T-21, T-22, FX-GIT-15).
//! * [`analyze_worktree`] / [`analyze_repo`]: everything GIT-02 reports about a worktree, as a
//!   serializable [`WorktreeState`]: HEAD, upstream, lock, activity ([`sample_activity`]),
//!   tracked changes, untracked and ignored files, submodules, unpushed commits, merge status
//!   and how it was determined, detached-HEAD reachability. Any Git failure makes the affected
//!   fact [`Observed::Unknown`], never "clean". Never fetches.
//! * [`classify_worktree`] / [`worktree_candidate`]: the GIT-03 removal decision table over a
//!   [`WorktreeState`], an in-use signal and an [`ArtifactMatcher`]: tier, reasons,
//!   whole-worktree action (S0 prune / S1 remove / S2 quarantine / S3 none) and whether
//!   artifact-only cleanup is allowed. A present worktree is never S0 (GIT-04); branches are
//!   never deleted.
//!
//! See `docs/PRD.md` and `HANDOVER.md` for responsibilities and boundaries.

mod activity;
mod classify;
mod meta;
mod refs;
mod resolve;
mod runner;
mod state;
mod status;
mod worktree;

pub use activity::{Activity, DEFAULT_ACTIVITY_SAMPLE, sample_activity};
pub use classify::{
    ArtifactCleanup, ArtifactMatcher, MatchedArtifact, NoArtifactRules, Tri, WORKTREE_RULE_ID,
    WORKTREE_RULE_VERSION, WorktreeDecision, classify_worktree, worktree_candidate,
};
pub use state::{
    AnalyzeOptions, CommitList, HeadState, MergeStatus, Observed, ParentRepo, PathList,
    Reachability, Submodule, Upstream, WorktreeState, analyze_repo, analyze_worktree,
};

pub use refs::{CherryCommit, ObjectId, RefEntry};
pub use runner::{
    DEFAULT_MAX_OUTPUT, DEFAULT_TIMEOUT, GitError, GitRunner, GitVersion, MIN_GIT_VERSION,
    RunnerOptions,
};
pub use status::{
    BranchInfo, GitPath, IgnoredMode, Status, StatusEntry, StatusKind, StatusOptions,
    SubmoduleState, UntrackedMode,
};

pub use worktree::{
    DiscoverError, LockState, ProblemKind, PruneReason, RepoWorktrees, Worktree, WorktreeHead,
    WorktreeKind, WorktreeProblem, discover, find_common_dir, list_worktrees,
};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-git");
    }
}
