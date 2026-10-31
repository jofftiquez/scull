//! The read-only scull pipeline, shared by the CLI, the MCP server and the desktop app
//! (decision D-29): **scan → detect → Git → signals → classify → candidates**.
//!
//! * [`Engine::scan`] walks the roots with `scull-scanner`, runs the bundled detectors on
//!   every directory as it is listed (never inside an artifact already detected), sizes each
//!   artifact from the scanner's hard-link-aware subtree totals, observes the PRD §8.3 signals
//!   (Git tracking, protected roots and exclusions from the config, `.scull-protect` and
//!   `.scull.toml` repository policies, mount boundaries, cloud-sync folders, recent activity,
//!   in-use), classifies with the rubric, analyses every Git repository's worktrees
//!   (GIT-02/03) and returns a [`ScanReport`]. With a store attached it records history.
//! * [`Engine::caches`] locates package-manager caches and classifies them (Quick Clean).
//! * [`Engine::inspect_worktree`] analyses one worktree.
//!
//! # Read-only by construction
//!
//! This crate never depends on `scull-cleaner` (SAFE-10): nothing here can delete, move or
//! modify a file. The only write is the history database, and only when the caller attaches
//! a [`scull_storage::Store`].
//!
//! # Unobservable signals are protective
//!
//! A signal the engine can't observe is never read as the safe answer (PRD §8.4): in-use
//! without an [`InUseProbe`] is unknown (at least S2), Git tracking inside a repository
//! without `git` is unknown (S2), a subtree the scanner could not fully read (or a folder inside
//! it that was not descended) is unknown (S2), an unreadable mount table counts as "crosses a
//! mount" (S3), and an unresolvable protection entry protects every candidate it might cover.
//! Every such gap is listed in [`ScanReport::signals_unavailable`] and in the candidate's
//! evidence.
//!
//! # One protection step
//!
//! Every candidate the engine emits (project artifacts, shared Cargo target directories,
//! package caches and Git worktrees, in [`Engine::scan`], [`Engine::caches`] and
//! [`Engine::inspect_worktree`]) passes through the same protection step: protected roots,
//! `scan.protect` / `scan.exclude`, repository policies, `.scull-protect` markers, minimum
//! tiers, mount crossings, cloud sync (including placeholders inside the candidate), nested
//! repositories and completeness. It only ever raises tiers, through
//! [`scull_core::Candidate::raise`], so the action follows the tier (D-28).

mod collect;
mod engine;
mod git;
mod history;
mod id;
pub mod paths;
mod reader;
pub mod report;
pub mod signals;

pub use collect::{EngineProgress, PROJECT_MARKERS, ProgressFn};
pub use engine::{Engine, EngineConfig, ScanRequest, now_unix};
pub use git::GIT_SIGNAL;
pub use history::{PlatformOwnerOnly, history_db_path, open_history};
pub use id::new_scan_id;
pub use reader::FsConfigReader;
pub use report::{
    CachesReport, HistoryOutcome, Issue, IssueKind, ProjectSummary, RefusedCacheReport,
    RepoConfigWarning, ScanReport, SignalUnavailable, Totals, WorktreeProblemReport,
    WorktreeReport, WorktreeTotals,
};
pub use scull_scanner::CancelToken;
pub use signals::{
    ActivityInUse, COMPLETENESS_SIGNAL, FixedInUse, IN_USE_SIGNAL, InUseAnswer, InUseProbe,
    InUseSource, MOUNTS_SIGNAL, NativeVolumes, PROTECTION_SIGNAL, VolumeSource,
};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
