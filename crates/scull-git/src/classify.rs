//! The worktree removal decision table (GIT-03, GIT-04).
//!
//! [`classify_worktree`] maps a [`WorktreeState`] (GIT-02), an in-use signal and an artifact
//! matcher to a tier, the [`TierReason`]s that fired, the whole-worktree action and whether
//! artifact-only cleanup is allowed. It is **pure**: no Git, filesystem or clock access beyond
//! what the matcher does. [`worktree_candidate`] turns the result into a [`Candidate`].
//!
//! # The table
//!
//! Every row that applies fires; the tier is the most protective of them.
//!
//! | Worktree state | Whole-worktree removal | Code | Artifact-only cleanup |
//! |---|---|---|---|
//! | Folder missing (stale registration) | `git worktree prune`: **S0** (metadata only) | [`ReasonCode::StaleWorktreeRegistration`] | n/a |
//! | Locked | S3 (reason shown) | [`ReasonCode::WorktreeLocked`] | Allowed |
//! | Uncommitted tracked changes, untracked (non-ignored) files, or a dirty submodule | S3 | [`ReasonCode::WorktreeDirty`] | Allowed |
//! | Ignored files not matched by an artifact rule (e.g. `.env`, `*.db`) | ≥ S2, listed by name | [`ReasonCode::WorktreeUniqueIgnored`] | Allowed |
//! | Commits not reachable from any remote-tracking ref and not provably merged | S3 | [`ReasonCode::UnpushedCommits`] | Allowed |
//! | Detached HEAD at a commit reachable from no ref | S3 | [`ReasonCode::OrphanHead`] | Allowed |
//! | Merge detected only by heuristic (patch-id) | S2 | [`ReasonCode::MergeUnproven`] | Allowed |
//! | Clean, unlocked, all commits reachable from a pushed ref or merged into the default branch | **S1, never S0** | [`ReasonCode::WorktreeClean`] | Allowed |
//! | In active use | S3 for this run | [`ReasonCode::InUse`] | Blocked for this run |
//!
//! Additionally, the main working tree and a bare repository's entry are never removable (S3,
//! [`ReasonCode::ProtectedRoot`]): `git worktree remove` only removes linked worktrees.
//!
//! # Actions (PRD §8.2, D-3)
//!
//! | Tier | Whole-worktree action |
//! |---|---|
//! | S0 (stale registration only) | [`Action::GitWorktreePrune`] (metadata only) |
//! | S1 (clean, pushed or merged) | [`Action::GitWorktreeRemove`] (never `--force`) |
//! | S2 | [`Action::Quarantine`]: the executor moves the worktree folder into same-volume quarantine (restorable), then prunes the registration. Irreversible Git actions are never proposed at S2. |
//! | S3 | [`Action::None`] |
//!
//! Branches are never deleted by any of these.
//!
//! "Provably merged" means: HEAD is an ancestor of a default-branch ref, the branch has no
//! commits, or every commit on HEAD is reachable from a remote-tracking ref. The patch-id
//! heuristic is never proof: it turns the S3 "unpushed" row into the S2 "merge unproven" row,
//! nothing lower (FX-GIT-06). A worktree is never S0 because it is old or because `git status`
//! is clean (GIT-04); only a stale registration is S0, and pruning one touches metadata only.
//! Branches are never deleted. Last observed activity is shown as evidence; it is not a row of
//! the table and never lowers a tier.
//!
//! # Unknowns
//!
//! An [`Observed::Unknown`] fact never counts as the safe answer. It fires
//! [`ReasonCode::SignalUnavailable`] at the tier of the row it would decide: **S3** for
//! uncommitted/untracked changes, submodules, HEAD, detached-HEAD reachability and unpushed
//! commits (unless the merge is proven another way); **S2** for ignored files (including a
//! truncated list). In-use unknown is **S2**, the same answer as the PRD §8.3 rubric
//! (`scull_safety::rubric`): it is disclosed, the item is never preselected and needs per-item
//! opt-in, and the executor re-checks use before acting (SAFE-05).

use crate::state::{MergeStatus, Observed, Reachability, WorktreeState};
use crate::worktree::{LockState, WorktreeKind};
use schemars::JsonSchema;
use scull_core::{
    Action, Candidate, Category, Evidence, FileIdentity, Flags, ItemId, ProjectRef, ReasonCode,
    Regeneration, RegenerationCost, RuleRef, Sizes, Tier, TierReason,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Rule id of worktree candidates.
pub const WORKTREE_RULE_ID: &str = "git.worktree";
/// Version of the worktree decision table; bumps whenever its tiering changes.
pub const WORKTREE_RULE_VERSION: u32 = 1;

/// Names listed in a reason message before "and N more" (the full list is in
/// [`WorktreeDecision::unique_ignored`]).
const MAX_NAMES_IN_MESSAGE: usize = 20;

/// A three-valued observation; `Unknown` is never treated as `No` (PRD §8.4).
///
/// Mirrors `scull_safety::Tri`, which this crate can't use (scull-safety depends on
/// scull-git).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    /// Observed: no.
    No,
    /// Observed: yes.
    Yes,
    /// Could not be observed.
    Unknown,
}

impl Tri {
    /// All values, for exhaustive tests.
    pub const ALL: [Tri; 3] = [Tri::No, Tri::Yes, Tri::Unknown];

    /// `Some(true)` / `Some(false)` / `None`, matching [`scull_core::Flags`].
    pub fn as_option(self) -> Option<bool> {
        match self {
            Tri::No => Some(false),
            Tri::Yes => Some(true),
            Tri::Unknown => None,
        }
    }
}

/// Decides whether an ignored path inside a worktree is a regenerable artifact recognised by a
/// detector rule (e.g. `target/` beside `Cargo.toml`). Ignored paths no rule matches are
/// treated as possibly unique data (`.env`, `dev.sqlite`; THREAT_MODEL T-03).
pub trait ArtifactMatcher {
    /// `worktree` is the worktree folder; `rel` the ignored entry as listed in
    /// [`WorktreeState::ignored`] (`/`-separated, a trailing `/` for a whole folder). Returns
    /// the matching rule's id, or `None`.
    fn match_ignored(&self, worktree: &Path, rel: &str) -> Option<String>;
}

impl<F: Fn(&Path, &str) -> Option<String>> ArtifactMatcher for F {
    fn match_ignored(&self, worktree: &Path, rel: &str) -> Option<String> {
        self(worktree, rel)
    }
}

/// An [`ArtifactMatcher`] that matches nothing: every ignored file counts as unique (the most
/// protective choice).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoArtifactRules;

impl ArtifactMatcher for NoArtifactRules {
    fn match_ignored(&self, _: &Path, _: &str) -> Option<String> {
        None
    }
}

/// Whether the worktree's regenerable artifacts may be cleaned on their own (GIT-03, last
/// column). Each artifact is still classified by the PRD §8.3 rubric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactCleanup {
    /// Allowed.
    Allowed,
    /// The worktree is in use: blocked for this run.
    BlockedForThisRun,
    /// The worktree folder is missing: nothing to clean.
    NotApplicable,
}

/// An ignored entry that a detector rule recognised as a regenerable artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MatchedArtifact {
    /// The entry, relative to the worktree.
    pub path: String,
    /// The rule that matched.
    pub rule: String,
}

/// The decision for one worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeDecision {
    /// The most protective tier of `reasons`.
    pub tier: Tier,
    /// Every row that fired.
    pub reasons: Vec<TierReason>,
    /// The whole-worktree action: [`Action::GitWorktreePrune`] for a stale registration (S0),
    /// [`Action::GitWorktreeRemove`] (never `--force`) at S1, [`Action::Quarantine`] at S2,
    /// [`Action::None`] at S3. Never deletes a branch.
    pub action: Action,
    /// Whether artifact-only cleanup is allowed.
    pub artifact_cleanup: ArtifactCleanup,
    /// Ignored entries no artifact rule matched, by name (the preview must list them).
    pub unique_ignored: Vec<String>,
    /// Ignored entries recognised as regenerable artifacts.
    pub artifacts: Vec<MatchedArtifact>,
    /// Facts to show under "why" (activity, unavailable signals, in-use).
    pub evidence: Vec<Evidence>,
}

/// Applies the GIT-03 decision table (see the module docs).
pub fn classify_worktree(
    state: &WorktreeState,
    in_use: Tri,
    artifacts: &dyn ArtifactMatcher,
) -> WorktreeDecision {
    let mut d = Decider::default();

    if state.missing {
        d.fire(
            Tier::S0,
            ReasonCode::StaleWorktreeRegistration,
            "registered worktree folder no longer exists: `git worktree prune` removes the stale \
             metadata only"
                .into(),
        );
    } else {
        classify_present(state, artifacts, &mut d);
    }

    if let LockState::Locked { reason } = &state.lock {
        d.fire(
            Tier::S3,
            ReasonCode::WorktreeLocked,
            match reason {
                Some(r) if !r.trim().is_empty() => format!("worktree is locked: {}", r.trim()),
                _ => "worktree is locked (no reason given)".into(),
            },
        );
    }

    match in_use {
        Tri::Yes => {
            d.fire(
                Tier::S3,
                ReasonCode::InUse,
                "worktree is in use: protected for this run".into(),
            );
            d.evidence.push(Evidence::InUse {
                signal: "worktree".into(),
                by: None,
            });
        }
        Tri::Unknown if !state.missing => d.unknown(
            Tier::S2,
            "in use",
            "in-use detection unavailable; re-checked before execution",
        ),
        _ => {}
    }

    if !state.missing
        && let Some(a) = state.activity.value()
        && let Some(newest_unix) = a.newest_unix
    {
        d.evidence.push(Evidence::LastModified {
            newest_unix,
            sampled: !a.complete,
        });
    } else if let Some(reason) = state.activity.unknown_reason()
        && !state.missing
    {
        d.evidence.push(Evidence::SignalUnavailable {
            signal: "last activity".into(),
            reason: reason.into(),
        });
    }

    // Clean row: nothing fired on a present worktree (every other row is S2 or above).
    if !state.missing && d.reasons.is_empty() {
        d.fire(
            Tier::S1,
            ReasonCode::WorktreeClean,
            format!(
                "clean and unlocked; {}; removal is S1, never S0 (GIT-04)",
                proof_text(state)
            ),
        );
    }

    let tier = d.tier();
    // Irreversible Git actions only at S0/S1; S2 is quarantine only (PRD §8.2, D-3).
    let action = match tier {
        Tier::S3 => Action::None,
        Tier::S2 => Action::Quarantine,
        Tier::S0 | Tier::S1 if state.missing => Action::GitWorktreePrune,
        Tier::S0 | Tier::S1 => Action::GitWorktreeRemove,
    };
    let artifact_cleanup = if state.missing {
        ArtifactCleanup::NotApplicable
    } else if in_use == Tri::Yes {
        ArtifactCleanup::BlockedForThisRun
    } else {
        ArtifactCleanup::Allowed
    };
    WorktreeDecision {
        tier,
        reasons: d.reasons,
        action,
        artifact_cleanup,
        unique_ignored: d.unique_ignored,
        artifacts: d.artifacts,
        evidence: d.evidence,
    }
}

#[derive(Default)]
struct Decider {
    reasons: Vec<TierReason>,
    evidence: Vec<Evidence>,
    unique_ignored: Vec<String>,
    artifacts: Vec<MatchedArtifact>,
}

impl Decider {
    fn fire(&mut self, tier: Tier, code: ReasonCode, message: String) {
        self.reasons.push(TierReason::new(tier, code, message));
    }

    fn unknown(&mut self, tier: Tier, signal: &str, reason: &str) {
        self.fire(
            tier,
            ReasonCode::SignalUnavailable,
            format!("{signal} unknown ({reason}): assumed the protective answer"),
        );
        self.evidence.push(Evidence::SignalUnavailable {
            signal: signal.into(),
            reason: reason.into(),
        });
    }

    fn tier(&self) -> Tier {
        Candidate::tier_from_reasons(&self.reasons)
    }
}

fn classify_present(state: &WorktreeState, artifacts: &dyn ArtifactMatcher, d: &mut Decider) {
    if state.kind != WorktreeKind::Linked {
        d.fire(
            Tier::S3,
            ReasonCode::ProtectedRoot,
            match state.kind {
                WorktreeKind::Bare => "bare repository: not a removable worktree",
                _ => "main working tree of the repository: only linked worktrees are removable",
            }
            .into(),
        );
        return;
    }

    // Uncommitted / untracked changes and dirty submodules.
    for (what, list) in [
        ("uncommitted change", &state.tracked_changes),
        ("untracked file", &state.untracked),
    ] {
        match list {
            Observed::Known { value } if !value.is_empty() => d.fire(
                Tier::S3,
                ReasonCode::WorktreeDirty,
                format!(
                    "{} {what}{}: {}",
                    value.total,
                    plural(value.total),
                    names(&value.paths, value.total)
                ),
            ),
            Observed::Known { .. } => {}
            Observed::Unknown { reason } => d.unknown(Tier::S3, &format!("{what}s"), reason),
        }
    }
    match &state.submodules {
        Observed::Known { value } => {
            for s in value {
                match &s.dirty {
                    Observed::Known { value: true } => d.fire(
                        Tier::S3,
                        ReasonCode::WorktreeDirty,
                        format!("submodule {} has local changes", s.path),
                    ),
                    Observed::Known { value: false } => {}
                    Observed::Unknown { reason } => {
                        d.unknown(Tier::S3, &format!("submodule {} state", s.path), reason);
                    }
                }
            }
        }
        Observed::Unknown { reason } => d.unknown(Tier::S3, "submodules", reason),
    }

    // Ignored files not matched by an artifact rule.
    match &state.ignored {
        Observed::Known { value } => {
            let root = state.path.to_path_buf();
            for p in &value.paths {
                match artifacts.match_ignored(&root, p) {
                    Some(rule) => d.artifacts.push(MatchedArtifact {
                        path: p.clone(),
                        rule,
                    }),
                    None => d.unique_ignored.push(p.clone()),
                }
            }
            if !d.unique_ignored.is_empty() {
                let n = d.unique_ignored.len() as u64;
                let listed = names(&d.unique_ignored, n);
                d.fire(
                    Tier::S2,
                    ReasonCode::WorktreeUniqueIgnored,
                    format!(
                        "{n} ignored file{} not matched by an artifact rule: {listed}",
                        plural(n)
                    ),
                );
            }
            if value.truncated {
                let unchecked = value.total.saturating_sub(value.paths.len() as u64);
                d.fire(
                    Tier::S2,
                    ReasonCode::WorktreeUniqueIgnored,
                    format!(
                        "{unchecked} more ignored entries were not checked against artifact rules"
                    ),
                );
            }
        }
        Observed::Unknown { reason } => d.unknown(Tier::S2, "ignored files", reason),
    }

    // History: HEAD, orphan detached HEAD, unpushed / merged.
    let head = match &state.head {
        Observed::Known { value } => value,
        Observed::Unknown { reason } => {
            d.unknown(Tier::S3, "HEAD", reason);
            return;
        }
    };
    if matches!(head, crate::state::HeadState::Detached { .. }) {
        match &state.head_reachability {
            Observed::Known {
                value: Reachability::Unreachable,
            } => d.fire(
                Tier::S3,
                ReasonCode::OrphanHead,
                format!(
                    "detached HEAD at {} is reachable from no ref",
                    short(head.oid().unwrap_or("?"))
                ),
            ),
            Observed::Known { .. } => {}
            Observed::Unknown { reason } => {
                d.unknown(Tier::S3, "detached HEAD reachability", reason)
            }
        }
    }
    if history_proven(state) {
        return;
    }
    match (&state.unpushed, &state.merge) {
        (Observed::Known { value }, MergeStatus::PatchIdOnly { default_ref, .. }) => d.fire(
            Tier::S2,
            ReasonCode::MergeUnproven,
            format!(
                "{} commit{} not on any remote; patch-equivalent to {default_ref} (heuristic \
                 only, merge unproven)",
                value.count,
                plural(value.count)
            ),
        ),
        (Observed::Known { value }, merge) => d.fire(
            Tier::S3,
            ReasonCode::UnpushedCommits,
            format!(
                "{}{} commit{} not reachable from any remote-tracking ref and not provably \
                 merged ({})",
                value.count,
                if value.truncated { "+" } else { "" },
                plural(value.count),
                merge_text(merge)
            ),
        ),
        (Observed::Unknown { reason }, _) => d.unknown(Tier::S3, "unpushed commits", reason),
    }
}

/// Proof that removing the worktree loses no commit: merged into a default branch, no
/// commits, or everything reachable from a remote-tracking ref.
fn history_proven(state: &WorktreeState) -> bool {
    matches!(
        state.merge,
        MergeStatus::AncestorOfDefault { .. } | MergeStatus::NoCommits
    ) || state.unpushed.value().is_some_and(|u| u.is_empty())
}

fn proof_text(state: &WorktreeState) -> String {
    match &state.merge {
        MergeStatus::AncestorOfDefault { default_ref } => format!("merged into {default_ref}"),
        MergeStatus::NoCommits => "no commits".into(),
        _ => "every commit reachable from a remote-tracking ref".into(),
    }
}

fn merge_text(m: &MergeStatus) -> String {
    match m {
        MergeStatus::NotMerged { checked } if checked.is_empty() => "no default branch".into(),
        MergeStatus::NotMerged { checked } => format!("not merged into {}", checked.join(", ")),
        MergeStatus::Unknown { reason } => format!("merge status unknown: {reason}"),
        MergeStatus::ReachableFromPushedRef { .. } => "pushed-ref listing contradicts".into(),
        MergeStatus::PatchIdOnly { default_ref, .. } => {
            format!("patch-equivalent to {default_ref}")
        }
        MergeStatus::AncestorOfDefault { default_ref } => format!("merged into {default_ref}"),
        MergeStatus::NoCommits => "no commits".into(),
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn names(paths: &[String], total: u64) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(MAX_NAMES_IN_MESSAGE)
        .map(String::as_str)
        .collect();
    let more = total.saturating_sub(shown.len() as u64);
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

fn short(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

/// Builds the [`Candidate`] for a classified worktree (`Category::Worktree`).
///
/// * `identity`: the file identity of the worktree folder, or for a stale registration of its
///   admin dir ([`WorktreeState::git_dir`]), which then is also the candidate's path (the folder
///   is gone; pruning removes the admin dir's metadata).
/// * `sizes`: the worktree's sizes (zero for a stale registration).
///
/// The result is always consistent ([`Candidate::is_consistent`]).
pub fn worktree_candidate(
    state: &WorktreeState,
    decision: &WorktreeDecision,
    in_use: Tri,
    identity: FileIdentity,
    sizes: Sizes,
) -> Candidate {
    let path = if state.missing {
        state.git_dir.clone()
    } else {
        state.path.clone()
    };
    let id = ItemId::derive(
        identity,
        &scull_platform::native_path_bytes(&path.to_path_buf()),
    );
    let project_root = state
        .repo
        .main_worktree
        .clone()
        .unwrap_or_else(|| state.repo.common_dir.clone());
    let project_name = project_root
        .to_path_buf()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| project_root.as_str().to_owned());
    let branch = state
        .head
        .value()
        .and_then(|h| h.branch().map(str::to_owned));
    let regeneration = match (decision.tier, &branch) {
        (Tier::S1, Some(b)) => Regeneration {
            instructions: Some(format!(
                "`git worktree add {} {b}` recreates it from the preserved branch",
                state.path
            )),
            cost: RegenerationCost::Unknown,
        },
        _ => Regeneration::default(),
    };
    let artifact = if state.missing {
        "Stale Git worktree registration".to_owned()
    } else {
        match &branch {
            Some(b) => format!("Git worktree ({b})"),
            None => "Git worktree (detached HEAD)".to_owned(),
        }
    };
    Candidate {
        id,
        path,
        identity,
        category: Category::Worktree,
        artifact,
        rule: RuleRef {
            id: WORKTREE_RULE_ID.into(),
            version: WORKTREE_RULE_VERSION,
        },
        project: Some(ProjectRef {
            root: project_root,
            name: project_name,
            ecosystem: "git".into(),
        }),
        tier: decision.tier,
        reasons: decision.reasons.clone(),
        evidence: decision.evidence.clone(),
        sizes,
        flags: Flags {
            active: in_use.as_option(),
            ..Flags::default()
        },
        regeneration,
        action: decision.action.clone(),
    }
}
