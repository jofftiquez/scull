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
