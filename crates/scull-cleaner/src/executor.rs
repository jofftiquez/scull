//! [`Executor`]: delete one verified plan item through the platform removal walker.

use crate::journal::StepHook;
use crate::lock::{ExecutionLock, LockOptions};

use scull_core::{
    Action, ErrorKind, FileIdentity, FsPath, ItemId, PlanItem, ScullError, action_allowed_for,
};
use scull_platform::remove::{self, RemoveError, RemoveReport, RemoveRequest, WalkEvent};
use scull_platform::{EntryKind, native_path_bytes};
use std::path::Path;

/// Where a plan item lives on disk, in native form.
///
/// A plan's [`FsPath`] is display-only and may be lossy, so the Executor never deletes by it:
/// the caller passes the native path of the freshly re-derived candidate, and the Executor
/// checks that this path, together with the item's identity, hashes to the item's
/// [`ItemId`] (the same derivation the scanner used), so a path cannot be swapped in.
#[derive(Debug, Clone, Copy)]
pub struct DeleteTarget<'a> {
    /// Trusted directory the walk starts from: the scan root the item was found under. Every
    /// component between it and the item is opened no-follow; the anchor itself is never
    /// removed.
    pub anchor: &'a Path,
    /// Identity the anchor must have, if known (recommended).
    pub anchor_identity: Option<FileIdentity>,
    /// The item's native, absolute path, strictly below `anchor`.
    pub path: &'a Path,
}

/// The result of one executed item.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeleteReport {
    /// The item.
    pub item_id: ItemId,
    /// What the walker removed, kept (with reasons) or stopped at.
    pub report: RemoveReport,
}

/// The only code path in scull that deletes (SAFE-10). See the crate docs for what is still
/// missing before it may be wired into any interface.
///
/// Plans are executed only by an Executor made with [`Executor::acquire`], which holds the
/// per-user execution lock (SAFE-09) for its whole lifetime.
#[derive(Default)]
#[non_exhaustive]
pub struct Executor {
    lock: Option<ExecutionLock>,
    hook: Option<StepHook>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("lock", &self.lock)
            .field("step_hook", &self.hook.is_some())
            .finish()
    }
}

impl Executor {
    /// An Executor **without** the execution lock: it can run the single-item primitives
    /// ([`Executor::delete`]) but [`Executor::execute_plan`] refuses to run. Use
    /// [`Executor::acquire`] to execute plans.
    pub fn new() -> Self {
        Self::default()
    }

    /// An Executor holding the per-user execution lock (SAFE-09) until it is dropped. Fails
    /// with [`ErrorKind::LockHeld`] (exit code 6) if another executor holds it, after waiting
    /// up to [`LockOptions::wait`].
    pub fn acquire(opts: &LockOptions) -> Result<Self, ScullError> {
        Ok(Self {
            lock: Some(ExecutionLock::acquire(opts)?),
            hook: None,
        })
    }

    /// Calls `hook` at every [`StepPoint`](crate::StepPoint) of plan execution (journal
    /// intent, removal-walk events, quarantine move, journal close, audit). For fault-injection
    /// tests: the crash-recovery kill tests (SAFE-19) stop the process at a chosen point.
    /// Production code installs no hook.
    #[must_use]
    pub fn with_step_hook(mut self, hook: StepHook) -> Self {
        self.hook = Some(hook);
        self
    }

    /// The step hook, if any.
    pub(crate) fn hook(&self) -> Option<&StepHook> {
        self.hook.as_ref()
    }

    /// The execution lock this Executor holds, if any.
    pub fn lock(&self) -> Option<&ExecutionLock> {
        self.lock.as_ref()
    }

    /// Deletes one plan item. `item` must come from [`scull_core::narrow`] (verified plan,
    /// re-derived candidate); this method re-checks what it can on its own:
    ///
    /// * the item is not S3 and its action is [`Action::Delete`], allowed for its tier (D-3,
    ///   D-28: only S0 may be deleted); anything else is refused with
    ///   [`ErrorKind::SafetyBlocked`]; quarantine, Trash and tool actions are not executed by
    ///   this build at all;
    /// * `target.path` is absolute, strictly below `target.anchor`, and hashes with the item's
    ///   identity to its [`ItemId`] ([`ErrorKind::PlanInvalidated`] otherwise);
    /// * the walker then verifies device + inode at open time (SAFE-05) and removes the tree
    ///   without following links or crossing mounts.
    ///
    /// `Err` means nothing was removed. `Ok` carries the walker's report, which may be
    /// [`remove::RemoveOutcome::Incomplete`] (entries kept, each with a reason) or
    /// [`remove::RemoveOutcome::Aborted`] (an anomaly stopped the walk).
    pub fn delete(
        &self,
        item: &PlanItem,
        target: &DeleteTarget<'_>,
    ) -> Result<DeleteReport, ScullError> {
        self.run(item, target, None, None)
    }

    /// [`Executor::delete`] with an observer called at each [`WalkEvent`], for fault-injection
    /// tests (FX-RACE-02). The walker re-verifies everything after each call.
    pub fn delete_observed(
        &self,
        item: &PlanItem,
        target: &DeleteTarget<'_>,
        observer: &mut dyn FnMut(&WalkEvent<'_>),
    ) -> Result<DeleteReport, ScullError> {
        self.run(item, target, None, Some(observer))
    }

    pub(crate) fn run(
        &self,
        item: &PlanItem,
        target: &DeleteTarget<'_>,
        expected_kind: Option<EntryKind>,
        observer: Option<&mut dyn FnMut(&WalkEvent<'_>)>,
    ) -> Result<DeleteReport, ScullError> {
        check_item(item)?;
        let relative = check_target(item, target)?;
        // Journaling (SAFE-19) and auditing (SAFE-08) happen in `execute_plan`, which owns the
        // operation record: a bare `delete` has no operation and no database. The measured
        // free-space delta (PHY-02) is taken around the whole plan there too.
        self.walk(
            item,
            target.anchor,
            target.anchor_identity,
            relative,
            expected_kind,
            observer,
        )
    }

    /// The removal walk itself, for an item whose tier, action and path binding the caller
    /// has already checked ([`Executor::run`], or the D-36 path in
    /// [`Executor::delete_orphaned_cache`]).
    pub(crate) fn walk(
        &self,
        item: &PlanItem,
        anchor: &Path,
        anchor_identity: Option<FileIdentity>,
        relative: &Path,
        expected_kind: Option<EntryKind>,
        observer: Option<&mut dyn FnMut(&WalkEvent<'_>)>,
    ) -> Result<DeleteReport, ScullError> {
        let req = RemoveRequest {
            anchor,
            anchor_identity,
            relative,
            expected: item.identity,
            // `PlanItem` records no kind, and its category does not imply one (a project
            // artifact or cache may be a directory or a single file; `LargeFile` items are
            // inspect-only and never planned). The identity already pins the object, whose
            // type cannot change; revalidation passes the kind it observed so the walker
            // refuses any other (SAFE-05). The walker refuses hard-linked single files itself.
            expected_kind,
        };
        let result = match observer {
            Some(observer) => remove::remove_tree_observed(&req, observer),
            None => remove::remove_tree(&req),
        };
        match result {
            Ok(report) => Ok(DeleteReport {
                item_id: item.item_id.clone(),
                report,
            }),
            Err(e) => Err(refusal(&e)
                .with_item(item.item_id.clone())
                .with_path(item.path.clone())),
        }
    }
}

/// Why a tool command outside Quick Clean's package caches (e.g. `ollama rm` for Ollama
/// models, P4-DET-01) is skipped: no trusted-tool runner allow-lists it yet.
pub const TOOL_NATIVE_UNSUPPORTED: &str = "tool-native ML removal not supported yet";

/// Whether `tool` is one of the package managers Quick Clean runs (resolved from trusted
/// locations, SAFE-12). Any other tool command is never executed, including the binaries of
/// inspect-only caches (Gradle's `gradle`, Maven's `mvn`: no cleanup command).
pub fn is_quick_clean_tool(tool: &str) -> bool {
    scull_detectors::caches::CacheTool::ALL.iter().any(|t| {
        t.binary() == tool && scull_detectors::caches::commands_for_rule(t.rule_id()).is_some()
    })
}

/// Tier and action checks (I-1, D-3, D-28) for the walker: only `Delete`.
fn check_item(item: &PlanItem) -> Result<(), ScullError> {
    check_item_for(item, &Action::Delete)
}

/// Tier and action checks (I-1, D-3, D-28) for an item about to be executed with `executing`,
/// which must be the item's own action.
pub(crate) fn check_item_for(item: &PlanItem, executing: &Action) -> Result<(), ScullError> {
    let blocked = |msg: String| {
        Err(ScullError::new(ErrorKind::SafetyBlocked, msg)
            .with_item(item.item_id.clone())
            .with_path(item.path.clone()))
    };
    if !item.tier.is_actionable() {
        return blocked("S3 (protected) items are never executed (I-1)".into());
    }
    if !action_allowed_for(item.tier, &item.action) {
        return blocked(format!(
            "action {:?} is not allowed for tier {} (D-3, D-28)",
            item.action, item.tier
        ));
    }
    if let Action::ToolCommand { tool, .. } = &item.action
        && !is_quick_clean_tool(tool)
    {
        return blocked(format!(
            "`{tool}` commands are never run by scull: {TOOL_NATIVE_UNSUPPORTED}"
        ));
    }
    if item.action != *executing {
        return blocked(format!(
            "action {:?} is not executed by this path: the walker deletes (S0 only); \
             quarantine and the Trash fallback need an operation (Executor::execute_plan), and \
             tool-native commands run through Quick Clean (quickclean::QuickClean::run)",
            item.action
        ));
    }
    Ok(())
}

/// Binds the native path to the item and returns it relative to the anchor.
pub(crate) fn check_target<'a>(
    item: &PlanItem,
    target: &DeleteTarget<'a>,
) -> Result<&'a Path, ScullError> {
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
            "the anchor (scan root) itself is never deleted",
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

/// A refused removal (nothing removed) as a shared error.
fn refusal(e: &RemoveError) -> ScullError {
    let kind = match e {
        RemoveError::AnchorUnavailable { source, .. }
        | RemoveError::TargetUnavailable { source } => {
            return ScullError::new(ScullError::from_io(source).kind, e.to_string());
        }
        RemoveError::TargetMissing => ErrorKind::NotFound,
        RemoveError::AnchorChanged { .. }
        | RemoveError::IdentityMismatch { .. }
        | RemoveError::KindMismatch { .. }
        | RemoveError::TargetChanged { .. } => ErrorKind::PlanInvalidated,
        RemoveError::PathNotTraversable { .. }
        | RemoveError::InvalidRequest { .. }
        | RemoveError::TargetIsMountPoint { .. }
        | RemoveError::HardLinked { .. } => ErrorKind::SafetyBlocked,
        RemoveError::Unsupported => ErrorKind::UnsupportedPlatform,
        _ => ErrorKind::Internal,
    };
    ScullError::new(kind, e.to_string())
}
