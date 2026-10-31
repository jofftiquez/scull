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
//!
//! See `docs/PRD.md` and `HANDOVER.md` for responsibilities and boundaries.

mod meta;
mod refs;
mod resolve;
mod runner;
mod status;
mod worktree;

pub use refs::{ObjectId, RefEntry};
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
