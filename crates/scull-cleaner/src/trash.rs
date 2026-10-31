//! [`Executor::trash`]: move one verified plan item to the platform Trash / Recycle Bin
//! (P2-EXEC-07; PRD §10.6, §9, D-3, D-28).
//!
//! Trash is the **fallback** for S1/S2 items whose same-volume quarantine is impossible
//! (P2-EXEC-05, #71, chains to this). If no Trash can take the item either
//! ([`TrashError::is_unavailable`]), the item is **blocked**: this never deletes, and the
//! platform layer never copies across volumes. A trashed item's bytes stay on the volume:
//! [`TrashReport`] reports them as **moved**, never as freed (§9, PHY-02).

use crate::executor::{DeleteTarget, Executor};
use scull_core::{
    Action, ErrorKind, FsPath, ItemId, PlanItem, Restorability, ScullError, action_allowed_for,
};
use scull_platform::trash::{self, TrashError, TrashRequest};
use scull_platform::{EntryKind, native_path_bytes};
use std::path::Path;

pub use scull_platform::trash::{MovedSize, TrashKind, TrashLocations, TrashOutcome};

/// The result of one item moved to the Trash.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TrashReport {
    /// The item.
    pub item_id: ItemId,
    /// Where it went and what was moved.
    pub outcome: TrashOutcome,
}

impl TrashReport {
    /// How the bytes are reported: always `"moved"` (§9: Trash moves bytes, it doesn't free
    /// them).
    pub const DISPOSITION: &'static str = "moved";

    /// Bytes moved into the Trash (allocated size).
    pub fn moved_bytes(&self) -> u64 {
        self.outcome.moved_bytes()
    }

    /// Bytes freed by this item: always 0. The space comes back only when the user empties the
    /// Trash, outside scull's control; the measured free-space delta (PHY-02) shows the truth.
    pub fn freed_bytes(&self) -> u64 {
        0
    }

    /// How the item can be restored: from the platform Trash.
    pub fn restorability(&self) -> Restorability {
        self.outcome.restorable_via
    }
}

impl Executor {
    /// Moves one plan item to the platform Trash (the fallback where quarantine is impossible).
    ///
    /// * The item must be actionable, planned for [`Action::Trash`] or [`Action::Quarantine`]
    ///   (Trash as quarantine's fallback), and Trash must be allowed for its tier (D-28: S0-S2;
    ///   S3 never). Anything else is refused with [`ErrorKind::SafetyBlocked`].
    /// * `target.path` must be absolute, strictly below `target.anchor`, and hash with the
    ///   item's identity to its [`ItemId`] ([`ErrorKind::PlanInvalidated`] otherwise).
    /// * The platform layer then re-checks identity and kind without following links and
    ///   renames the item into a trash directory on its own volume ([`scull_platform::trash`]).
    ///
    /// `Err` means nothing was moved and nothing was deleted: the item stays where it is and is
    /// reported as blocked. The caller never escalates to deletion (PRD §10.6). In particular
    /// [`ErrorKind::SafetyBlocked`] covers "no usable Trash on this volume" (headless
    /// machines, FX-LNX-01) and "the only Trash is on another volume".
    pub fn trash(
        &self,
        item: &PlanItem,
        target: &DeleteTarget<'_>,
        locations: &TrashLocations,
    ) -> Result<TrashReport, ScullError> {
        self.trash_checked(item, target, locations, None)
    }

    /// [`Executor::trash`] that also requires the item to be of `expected_kind` (as observed at
    /// revalidation).
    pub fn trash_checked(
        &self,
        item: &PlanItem,
        target: &DeleteTarget<'_>,
        locations: &TrashLocations,
        expected_kind: Option<EntryKind>,
    ) -> Result<TrashReport, ScullError> {
        check_trash_item(item)?;
        let relative = check_target(item, target)?;
        // Journaling (SAFE-19) and the audit row (SAFE-08) are done by `execute_plan`, which
        // owns the operation; a bare `trash` call has no operation.
        let req = TrashRequest {
            anchor: target.anchor,
            anchor_identity: target.anchor_identity,
            relative,
            expected: item.identity,
            expected_kind,
            locations,
        };
        match trash::move_to_trash(&req) {
            Ok(outcome) => Ok(TrashReport {
                item_id: item.item_id.clone(),
                outcome,
            }),
            Err(e) => Err(refusal(&e)
                .with_item(item.item_id.clone())
                .with_path(item.path.clone())),
        }
    }
}

/// Tier and action checks (I-1, D-3, D-28).
fn check_trash_item(item: &PlanItem) -> Result<(), ScullError> {
    let blocked = |msg: String| {
        Err(ScullError::new(ErrorKind::SafetyBlocked, msg)
            .with_item(item.item_id.clone())
            .with_path(item.path.clone()))
    };
    if !item.tier.is_actionable() {
        return blocked("S3 (protected) items are never executed (I-1)".into());
    }
    if !matches!(item.action, Action::Trash | Action::Quarantine) {
        return blocked(format!(
            "action {:?} is not moved to the Trash: only Trash items and quarantine items whose \
             quarantine is impossible are (D-3)",
            item.action
        ));
    }
    if !action_allowed_for(item.tier, &Action::Trash) {
        return blocked(format!(
            "Trash is not allowed for tier {} (D-28)",
            item.tier
        ));
    }
    Ok(())
}

/// Binds the native path to the item and returns it relative to the anchor. Same binding as
/// [`Executor::delete`] applies.
fn check_target<'a>(item: &PlanItem, target: &DeleteTarget<'a>) -> Result<&'a Path, ScullError> {
    let invalid = |kind: ErrorKind, msg: &str| {
        Err(ScullError::new(kind, msg)
            .with_item(item.item_id.clone())
            .with_path(item.path.clone()))
    };
    if !target.anchor.is_absolute() || !target.path.is_absolute() {
        return invalid(
            ErrorKind::SafetyBlocked,
            "the anchor and the item path must be absolute",
        );
    }
    let Ok(relative) = target.path.strip_prefix(target.anchor) else {
        return invalid(
            ErrorKind::SafetyBlocked,
            "the item path is not below the anchor (scan root)",
        );
    };
    if relative.as_os_str().is_empty() {
        return invalid(
            ErrorKind::SafetyBlocked,
            "the anchor (scan root) itself is never moved",
        );
    }
    if ItemId::derive(item.identity, &native_path_bytes(target.path)) != item.item_id
        || FsPath::from_path(target.path) != item.path
    {
        return invalid(
            ErrorKind::PlanInvalidated,
            "the given path is not the planned item's path",
        );
    }
    Ok(relative)
}

/// A refused move (nothing moved, nothing deleted) as a shared error.
fn refusal(e: &TrashError) -> ScullError {
    let kind = match e {
        TrashError::AnchorUnavailable { source, .. }
        | TrashError::TargetUnavailable { source }
        | TrashError::MoveFailed { source } => {
            return ScullError::new(ScullError::from_io(source).kind, e.to_string());
        }
        TrashError::TargetMissing => ErrorKind::NotFound,
        TrashError::AnchorChanged { .. }
        | TrashError::IdentityChanged { .. }
        | TrashError::KindMismatch { .. }
        | TrashError::ReplacedDuringMove { .. } => ErrorKind::PlanInvalidated,
        TrashError::InvalidRequest { .. }
        | TrashError::PathNotTraversable { .. }
        | TrashError::TargetIsMountPoint
        | TrashError::CrossVolume { .. }
        | TrashError::Unavailable { .. } => ErrorKind::SafetyBlocked,
        _ => ErrorKind::Internal,
    };
    ScullError::new(kind, e.to_string())
}
