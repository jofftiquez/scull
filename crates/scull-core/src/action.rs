//! What scull would do with a candidate (PRD §10.2, §10.5, D-3).

use crate::tier::Tier;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The action proposed for a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Action {
    /// No action: the item is blocked (S3) or inspect-only.
    None,
    /// Delete through the safe executor (S0 default, D-3). Restore path is regeneration.
    Delete,
    /// Atomic same-volume move into `.scull-quarantine/<operation-id>/` (S1/S2 default).
    Quarantine,
    /// Move to the platform Trash / Recycle Bin (fallback when quarantine isn't possible).
    Trash,
    /// Run a tool's own cleanup command (package caches, Docker; SAFE-06, QC-03).
    ToolCommand {
        /// Tool name, e.g. "npm".
        tool: String,
        /// Structured arguments (no shell), e.g. `["cache", "verify"]`.
        args: Vec<String>,
    },
    /// `git worktree remove` (never `--force` unless authorized per item).
    GitWorktreeRemove,
    /// `git worktree prune` for a stale registration.
    GitWorktreePrune,
}

impl Action {
    /// Whether this action can be undone by `scull restore`.
    pub fn restorability(&self) -> Restorability {
        match self {
            Action::None => Restorability::NotApplicable,
            Action::Quarantine => Restorability::Restorable,
            Action::Trash => Restorability::ViaTrash,
            Action::Delete => Restorability::Regenerate,
            Action::ToolCommand { .. } | Action::GitWorktreeRemove | Action::GitWorktreePrune => {
                Restorability::NotRestorable
            }
        }
    }

    /// Whether the action removes or moves data (anything but [`Action::None`]).
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Action::None)
    }

    /// Whether `self` may be proposed for an item at `tier` (PRD §8.2, D-3):
    ///
    /// | Tier | Allowed |
    /// |---|---|
    /// | S0 | any action |
    /// | S1 | anything except `Delete` (quarantine, Trash, tool command, git operation) |
    /// | S2 | **only** `Quarantine` or `Trash` (restorable), or `None` |
    /// | S3 | only `None` |
    ///
    /// Irreversible actions (tool commands, `git worktree remove/prune`, delete) are never
    /// allowed for uncertain (S2) items.
    pub fn allowed_for(&self, tier: Tier) -> bool {
        match (tier, self) {
            (_, Action::None) => true,
            (Tier::S3, _) => false,
            (Tier::S2, a) => matches!(a, Action::Quarantine | Action::Trash),
            (Tier::S1, a) => !matches!(a, Action::Delete),
            (Tier::S0, _) => true,
        }
    }

    /// The most useful action allowed at `tier` that is no less safe than `self`, used when a
    /// tier is raised: `Delete`/`GitWorktreeRemove` become `Quarantine` at S1/S2 (restorable
    /// move); tool commands and `GitWorktreePrune` become `None` at S2 (skipped, QC-05); anything
    /// becomes `None` at S3.
    #[must_use]
    pub fn constrained_to(self, tier: Tier) -> Action {
        if self.allowed_for(tier) {
            return self;
        }
        match (tier, self) {
            (Tier::S3, _) => Action::None,
            (_, Action::Delete | Action::GitWorktreeRemove) => Action::Quarantine,
            _ => Action::None,
        }
    }

    /// The default action for a filesystem candidate at `tier` (D-3).
    pub fn default_for(tier: Tier) -> Action {
        match tier {
            Tier::S0 => Action::Delete,
            Tier::S1 | Tier::S2 => Action::Quarantine,
            Tier::S3 => Action::None,
        }
    }
}

/// How (or whether) an action can be undone. Shown in the preview (SAFE-01, SAFE-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Restorability {
    /// `scull restore` brings it back (quarantine).
    Restorable,
    /// Restorable from the platform Trash, outside scull's control.
    ViaTrash,
    /// Not restorable, but regenerable (rebuild / reinstall).
    Regenerate,
    /// Not restorable. The preview must say so plainly.
    NotRestorable,
    /// No action is taken.
    NotApplicable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_d3() {
        assert_eq!(Action::default_for(Tier::S0), Action::Delete);
        assert_eq!(Action::default_for(Tier::S1), Action::Quarantine);
        assert_eq!(Action::default_for(Tier::S2), Action::Quarantine);
        assert_eq!(Action::default_for(Tier::S3), Action::None);
        assert!(!Action::None.is_mutating());
    }

    #[test]
    fn s2_allows_only_restorable_moves() {
        let tool = Action::ToolCommand {
            tool: "npm".into(),
            args: vec!["cache".into(), "verify".into()],
        };
        for a in [
            Action::Delete,
            tool.clone(),
            Action::GitWorktreeRemove,
            Action::GitWorktreePrune,
        ] {
            assert!(!a.allowed_for(Tier::S2), "{a:?} must not be allowed at S2");
            assert!(!a.allowed_for(Tier::S3));
        }
        assert!(Action::Quarantine.allowed_for(Tier::S2));
        assert!(Action::Trash.allowed_for(Tier::S2));
        assert!(tool.allowed_for(Tier::S1));
        assert!(!Action::Delete.allowed_for(Tier::S1));
        assert_eq!(
            Action::GitWorktreeRemove.constrained_to(Tier::S2),
            Action::Quarantine
        );
        assert_eq!(tool.constrained_to(Tier::S2), Action::None);
        assert_eq!(Action::Quarantine.constrained_to(Tier::S3), Action::None);
        assert_eq!(Action::Delete.constrained_to(Tier::S1), Action::Quarantine);
    }

    #[test]
    fn tool_commands_are_not_restorable() {
        let a = Action::ToolCommand {
            tool: "npm".into(),
            args: vec!["cache".into(), "verify".into()],
        };
        assert_eq!(a.restorability(), Restorability::NotRestorable);
        assert_eq!(
            Action::Quarantine.restorability(),
            Restorability::Restorable
        );
    }
}
