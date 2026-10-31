//! Evidence a detector or safety check produced, and the reasons that set a candidate's tier
//! (PRD §8.1, §8.3, §10.2).

use crate::path::FsPath;
use crate::tier::Tier;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A fact observed about a candidate. Evidence is what the preview shows under "why".
///
/// Missing evidence never counts as evidence of inactivity (PRD §8.4): when a signal can't be
/// read, record [`Evidence::SignalUnavailable`] instead of omitting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Evidence {
    /// A project marker at the expected relative location (e.g. `Cargo.toml` beside `target/`).
    /// CLS-03: a directory name alone is never enough.
    Marker {
        /// Path of the marker file.
        path: FsPath,
    },
    /// A lockfile or manifest the artifact can be reproduced from.
    Lockfile {
        /// Path of the lockfile.
        path: FsPath,
    },
    /// The artifact type is known to a rule.
    KnownArtifact {
        /// Human-readable artifact type, e.g. "Cargo build output".
        artifact: String,
    },
    /// Files inside the candidate are tracked by Git.
    GitTracked {
        /// Number of tracked files found (may be a lower bound).
        files: u64,
    },
    /// The path is inside a protected root (SAFE-03).
    ProtectedRoot {
        /// The protected root.
        root: FsPath,
    },
    /// The user excluded this path in config.
    UserExclusion {
        /// The exclusion entry that matched.
        rule: String,
    },
    /// A `.scull-protect` marker covers this path.
    ProtectMarker {
        /// Path of the marker.
        path: FsPath,
    },
    /// The candidate is in use right now.
    InUse {
        /// What showed it, e.g. "open handle", "process cwd", "cargo lock".
        signal: String,
        /// Process name or tool, if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// Newest modification inside the candidate, sampled with a bound.
    LastModified {
        /// Unix timestamp (seconds, UTC) of the newest observed mtime.
        newest_unix: i64,
        /// Whether sampling hit its bound (the true newest may be later).
        sampled: bool,
    },
    /// The candidate crosses a mount or volume boundary.
    CrossesMount,
    /// Reached through a symlink or junction pointing outside the scan root.
    LinkEscape {
        /// Where the link points.
        target: FsPath,
    },
    /// Cloud-sync placeholder or synced folder (iCloud, OneDrive, Dropbox). Never hydrated.
    CloudSync {
        /// Provider name.
        provider: String,
    },
    /// Hardlinks shared with something outside the candidate (e.g. pnpm store).
    Hardlinked {
        /// Bytes shared with other links.
        shared_bytes: u64,
    },
    /// A signal could not be read on this platform or with current permissions.
    SignalUnavailable {
        /// Which signal, e.g. "open files".
        signal: String,
        /// Why, e.g. "needs Full Disk Access".
        reason: String,
    },
    /// Free-form note from a detector (shown, never used for tiering).
    Note {
        /// The note.
        text: String,
    },
}

/// Why a candidate got (at least) a given tier. One per rubric row that fired (PRD §8.3); the
/// candidate's tier is the most protective of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TierReason {
    /// The minimum tier this reason imposes.
    pub tier: Tier,
    /// Stable machine-readable code.
    pub code: ReasonCode,
    /// Short human explanation shown in the preview.
    pub message: String,
}

/// Stable codes for tier reasons. Matches the rows of the rubric (PRD §8.3) and the worktree
/// decision table (GIT-03). New codes may be added; existing ones never change meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReasonCode {
    /// Contains Git-tracked files → S3.
    GitTracked,
    /// Inside a protected root → S3.
    ProtectedRoot,
    /// Matches a user exclusion → S3.
    UserExclusion,
    /// Covered by `.scull-protect` → S3.
    ProtectMarker,
    /// In use (open handles, process cwd, held lock) → S3 for this run.
    InUse,
    /// Crosses a mount/volume boundary → S3.
    CrossesMount,
    /// Reached via a link outside the scan root → S3.
    LinkEscape,
    /// Cloud placeholder or synced folder → S2.
    CloudSync,
    /// Marker found but nothing to reproduce from → S2.
    NoLockfile,
    /// Modified within the activity window → one tier more protective.
    RecentActivity,
    /// A required signal was unavailable, so the engine assumed the protective answer.
    SignalUnavailable,
    /// Reproducible, but needs network or a long build → S1.
    CostlyToRegenerate,
    /// Reproducible from local inputs or quickly → S0.
    Regenerable,
    /// Worktree is locked → S3.
    WorktreeLocked,
    /// Worktree has uncommitted or untracked changes → S3.
    WorktreeDirty,
    /// Worktree has ignored files not matched by an artifact rule → ≥ S2.
    WorktreeUniqueIgnored,
    /// Commits not reachable from any remote-tracking ref and not provably merged → S3.
    UnpushedCommits,
    /// Detached HEAD at a commit reachable from no ref → S3.
    OrphanHead,
    /// Merge detected only heuristically → S2.
    MergeUnproven,
    /// Clean, unlocked, everything pushed or merged → S1 (never S0).
    WorktreeClean,
    /// Registered worktree whose directory is gone → S0 (metadata only).
    StaleWorktreeRegistration,
    /// Package cache: re-downloadable → S1 (QC-02).
    PackageCache,
    /// Large file surfaced for inspection only → S2.
    InspectOnly,
}

impl TierReason {
    /// Convenience constructor.
    pub fn new(tier: Tier, code: ReasonCode, message: impl Into<String>) -> Self {
        Self {
            tier,
            code,
            message: message.into(),
        }
    }
}
