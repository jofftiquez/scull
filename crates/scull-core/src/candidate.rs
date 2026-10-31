//! Candidates: detected items with a tier, evidence and a proposed action (PRD §6, §10.2).

use crate::action::{Action, Restorability};
use crate::evidence::{Evidence, TierReason};
use crate::id::{FileIdentity, ItemId, RuleRef};
use crate::path::FsPath;
use crate::sizes::Sizes;
use crate::tier::Tier;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Broad category of a candidate, used for grouping in the CLI, charts and Quick Clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Category {
    /// Build output or dependencies inside a project (`target/`, `node_modules`, `.next`, …).
    ProjectArtifact,
    /// Global package-manager cache (Quick Clean, QC-01).
    PackageCache,
    /// A Git worktree, whole or stale registration.
    Worktree,
    /// ML/AI model or dataset cache.
    MlCache,
    /// Docker / container data.
    Container,
    /// Large file surfaced for inspection only.
    LargeFile,
    /// scull's own data (database, logs, quarantine; DATA-05).
    ScullData,
    /// Anything else.
    Other,
}

/// The project or repository that owns a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectRef {
    /// Project root directory.
    pub root: FsPath,
    /// Display name (usually the directory name or manifest name).
    pub name: String,
    /// Ecosystem, e.g. "rust", "node".
    pub ecosystem: String,
}

/// Risk flags shown with every candidate (SAFE-01). `None` means "not checked / unknown",
/// which is never treated as "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Flags {
    /// In use by a process or tool.
    pub active: Option<bool>,
    /// Contains hardlinks shared outside the candidate.
    pub hardlinked: Option<bool>,
    /// Crosses a mount/volume boundary.
    pub cross_mount: Option<bool>,
    /// Is or contains a symlink/junction.
    pub symlink: Option<bool>,
    /// Is or contains a cloud-sync placeholder.
    pub cloud_placeholder: Option<bool>,
}

/// How to get an item back after removal, and roughly what it costs (D-5: "unknown" unless
/// there is evidence).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Regeneration {
    /// Instructions, e.g. "cargo build".
    pub instructions: Option<String>,
    /// Estimated cost.
    pub cost: RegenerationCost,
}

/// Estimated cost of regenerating an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegenerationCost {
    /// No evidence to estimate from (default, D-5).
    Unknown,
    /// An estimate backed by evidence.
    Estimate {
        /// Approximate seconds to regenerate.
        seconds: u64,
        /// Whether regeneration needs network access.
        network: bool,
    },
    /// Cannot be regenerated (unique data).
    NotRegenerable,
}

impl Default for Regeneration {
    fn default() -> Self {
        Self {
            instructions: None,
            cost: RegenerationCost::Unknown,
        }
    }
}

/// A detected item with a risk tier and evidence. This is the unit the preview shows, plans
/// reference and the executor revalidates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// Stable ID (identity + path).
    pub id: ItemId,
    /// Exact path.
    pub path: FsPath,
    /// File identity at scan time; revalidation compares against it (SAFE-05).
    pub identity: FileIdentity,
    /// Category for grouping.
    pub category: Category,
    /// Human-readable artifact type, e.g. "Cargo build output".
    pub artifact: String,
    /// Rule that produced the candidate.
    pub rule: RuleRef,
    /// Owning project, if any.
    pub project: Option<ProjectRef>,
    /// Tier: always the most protective of `reasons` (see [`Candidate::is_consistent`]).
    pub tier: Tier,
    /// Every rubric row that fired.
    pub reasons: Vec<TierReason>,
    /// Detection evidence.
    pub evidence: Vec<Evidence>,
    /// Logical, allocated and reclaimable bytes.
    pub sizes: Sizes,
    /// Risk flags.
    pub flags: Flags,
    /// Regeneration instructions and cost.
    pub regeneration: Regeneration,
    /// Proposed action.
    pub action: Action,
}

impl Candidate {
    /// The tier implied by `reasons`: the most protective one. With no reasons the item is
    /// **S3**: a candidate nobody vouched for is never actionable.
    pub fn tier_from_reasons(reasons: &[TierReason]) -> Tier {
        Tier::most_protective(reasons.iter().map(|r| r.tier)).unwrap_or(Tier::S3)
    }

    /// Whether the candidate is internally consistent:
    /// * `tier` equals the most protective reason (or S3 with no reasons);
    /// * an S3 item proposes no mutating action (invariant I-1);
    /// * the action is allowed for the tier ([`Action::allowed_for`]): `Delete` only at S0,
    ///   irreversible tool/Git actions at S0/S1 only, S2 only quarantine/Trash (PRD §8.2, D-3).
    pub fn is_consistent(&self) -> bool {
        if self.tier != Self::tier_from_reasons(&self.reasons) {
            return false;
        }
        self.action.allowed_for(self.tier)
    }

    /// Whether the proposed action can be undone.
    pub fn restorability(&self) -> Restorability {
        self.action.restorability()
    }

    /// Raise the tier to at least `tier` for `reason`, never lowering it. The action is then
    /// constrained to the new tier ([`Action::constrained_to`]): S3 ⇒ `None`; at S2 a delete or
    /// worktree removal becomes `Quarantine` and tool commands become `None`.
    pub fn raise(&mut self, reason: TierReason) {
        self.tier = self.tier.max_protective(reason.tier);
        self.reasons.push(reason);
        let action = std::mem::replace(&mut self.action, Action::None);
        self.action = action.constrained_to(self.tier);
    }
}
