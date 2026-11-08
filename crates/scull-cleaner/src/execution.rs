//! Executing a whole plan: verify it, then revalidate and execute each item in turn
//! (SAFE-05, SAFE-11, P2-EXEC-03), recording each outcome in the operation's audit (SAFE-08),
//! with the quarantine → Trash → blocked chain (D-3, P2-EXEC-05/07) and the measured
//! free-space delta (PHY-02, P2-EXEC-10).

use crate::executor::{DeleteTarget, Executor};
use crate::journal::{StepPoint, at, fingerprint};
use crate::measure::{ItemExecution, MeasuredDelta, Tally, planned_sizes};
use crate::operation::Operation;
use crate::quarantine::{QuarantineRefusal, Quarantined};
use crate::revalidate::{Revalidated, Revalidation, Skip};
use crate::trash::TrashReport;
use scull_core::{
    Action, ErrorKind, FsPath, ItemId, Plan, PlanId, PlanOrigin, ScullError, Sizes, Tier,
};
use scull_engine::{CancelToken, now_unix};
use scull_platform::remove::{RemoveOutcome, RemoveReport, WalkEvent};
use scull_platform::{Native, Platform};
use scull_scanner::ActionReclaim;
use scull_storage::{
    ItemOutcome as AuditOutcome, ItemRecord, JournalAction, JournalIntent, WalkEnd, WalkSummary,
};

/// Who and when, for [`Plan::verify`].
#[derive(Debug, Clone, Copy)]
pub struct PlanContext<'a> {
    /// Now, Unix seconds (UTC).
    pub now_unix: i64,
    /// This machine's ID.
    pub machine_id: &'a str,
    /// The current user.
    pub user: &'a str,
}

/// What happened to one plan item.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ItemOutcome {
    /// The removal walker ran. Its report may be [`RemoveOutcome::Incomplete`] or
    /// [`RemoveOutcome::Aborted`]; see [`ItemReport::succeeded`].
    Deleted(RemoveReport),
    /// Removed the exact linked worktree through hardened Git, preserving all branches.
    WorktreeRemoved {
        /// Allocated bytes of the verified tree before removal.
        freed_bytes: u64,
    },
    /// Git started but did not prove a complete removal; its effects may be partial.
    WorktreeRemovalFailed {
        /// Why it did not finish.
        reason: String,
    },
    /// Moved into same-volume quarantine (restorable).
    Quarantined(Quarantined),
    /// Moved to the platform Trash because quarantine was impossible here (D-3: the
    /// fallback). Its bytes are moved, not freed (PRD §9).
    Trashed(TrashReport),
    /// Revalidation failed: the item was skipped and nothing was touched.
    Skipped(Skip),
    /// The item passed revalidation but the Executor refused it (nothing was removed), e.g.
    /// an action this build does not execute, or the walker's own last-instant check.
    Refused(ScullError),
    /// Its action is impossible here and no fallback is available: left in place, never
    /// deleted instead (D-3: quarantine → Trash → blocked).
    Blocked {
        /// Stable reason code (e.g. `quarantine_impossible`).
        code: &'static str,
        /// Why.
        message: String,
    },
    /// Not attempted: the operation was cancelled before this item.
    NotRun,
}

/// The result for one plan item.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ItemReport {
    /// The item.
    pub item_id: ItemId,
    /// Its planned path (display).
    pub path: FsPath,
    /// Tier at planning time.
    pub planned_tier: Tier,
    /// Tier the fresh classification gave at revalidation, when the item was re-derived.
    pub fresh_tier: Option<Tier>,
    /// What happened.
    pub outcome: ItemOutcome,
}

impl ItemReport {
    /// The tier the item was handled under: the more protective of the planned and fresh
    /// tiers, so it never decreases on revalidation (I-8).
    pub fn executed_tier(&self) -> Tier {
        self.fresh_tier
            .map_or(self.planned_tier, |f| self.planned_tier.max_protective(f))
    }

    /// Whether the item's action completed in full.
    pub fn succeeded(&self) -> bool {
        match &self.outcome {
            ItemOutcome::Deleted(r) => r.outcome == RemoveOutcome::Complete,
            ItemOutcome::WorktreeRemoved { .. }
            | ItemOutcome::Quarantined(_)
            | ItemOutcome::Trashed(_) => true,
            _ => false,
        }
    }

    /// Stable reason code for an item that did not succeed, for reports and the audit log.
    pub fn reason_code(&self) -> Option<&'static str> {
        match &self.outcome {
            ItemOutcome::Deleted(r) => match r.outcome {
                RemoveOutcome::Complete => None,
                RemoveOutcome::Incomplete => Some("incomplete"),
                _ => Some("aborted"),
            },
            ItemOutcome::WorktreeRemoved { .. }
            | ItemOutcome::Quarantined(_)
            | ItemOutcome::Trashed(_) => None,
            ItemOutcome::WorktreeRemovalFailed { .. } => Some("git_worktree_remove_failed"),
            ItemOutcome::Skipped(s) => Some(s.code()),
            ItemOutcome::Refused(e) => Some(e.kind.as_str()),
            ItemOutcome::Blocked { code, .. } => Some(code),
            ItemOutcome::NotRun => Some("cancelled"),
        }
    }

    /// The audit record for this outcome (`None` when the item was not attempted).
    pub(crate) fn audit_record(&self, at: i64) -> Option<ItemRecord> {
        let id = self.item_id.clone();
        Some(match &self.outcome {
            ItemOutcome::Deleted(r) => ItemRecord::from_walk(
                id,
                at,
                &WalkSummary {
                    end: match r.outcome {
                        RemoveOutcome::Complete => WalkEnd::Complete,
                        RemoveOutcome::Incomplete => WalkEnd::Incomplete,
                        _ => WalkEnd::Aborted,
                    },
                    entries_removed: r.removed.files
                        + r.removed.dirs
                        + r.removed.symlinks
                        + r.removed.special,
                    freed_bytes: r.removed.allocated_bytes,
                    entries_kept: r.skipped_total,
                    reason: None,
                },
            ),
            ItemOutcome::WorktreeRemoved { freed_bytes } => {
                let mut rec = ItemRecord::new(id, at, AuditOutcome::Removed)
                    .with_reason("git_worktree_remove");
                rec.freed_bytes = Some(*freed_bytes);
                rec
            }
            ItemOutcome::WorktreeRemovalFailed { reason } => {
                ItemRecord::new(id, at, AuditOutcome::Partial).with_reason(reason)
            }
            ItemOutcome::Quarantined(q) => {
                let mut rec = ItemRecord::new(id, at, AuditOutcome::Quarantined);
                rec.moved_bytes = Some(q.bytes);
                rec
            }
            ItemOutcome::Trashed(t) => {
                let mut rec = ItemRecord::new(id, at, AuditOutcome::MovedToTrash)
                    .with_reason("quarantine_impossible");
                rec.moved_bytes = Some(t.moved_bytes());
                rec
            }
            ItemOutcome::Skipped(s) => ItemRecord::skipped(id, at, s.code()),
            ItemOutcome::Refused(e) => ItemRecord::from_error(id, at, e),
            ItemOutcome::Blocked { code, .. } => {
                ItemRecord::new(id, at, AuditOutcome::Refused).with_reason(*code)
            }
            ItemOutcome::NotRun => return None,
        })
    }

    /// What was done, for the measured-delta tally.
    fn execution(&self) -> ItemExecution {
        match &self.outcome {
            ItemOutcome::Deleted(r) => {
                let removed = r.removed.allocated_bytes;
                if r.outcome == RemoveOutcome::Complete {
                    ItemExecution::Done { removed, moved: 0 }
                } else {
                    ItemExecution::Partial { removed, moved: 0 }
                }
            }
            ItemOutcome::WorktreeRemoved { freed_bytes } => ItemExecution::Done {
                removed: *freed_bytes,
                moved: 0,
            },
            ItemOutcome::WorktreeRemovalFailed { .. } => ItemExecution::Partial {
                removed: 0,
                moved: 0,
            },
            ItemOutcome::Quarantined(q) => ItemExecution::Done {
                removed: 0,
                moved: q.bytes,
            },
            ItemOutcome::Trashed(t) => ItemExecution::Done {
                removed: 0,
                moved: t.moved_bytes(),
            },
            _ => ItemExecution::NotExecuted,
        }
    }
}

/// The result of [`Executor::execute_plan`]: one report per plan item, in plan order.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ExecutionReport {
    /// The executed plan.
    pub plan_id: PlanId,
    /// One entry per plan item, in plan order.
    pub items: Vec<ItemReport>,
    /// Whether the operation was cancelled before every item was attempted.
    pub cancelled: bool,
    /// The accuracy report (PHY-02): predicted reclaim, bytes removed and moved, the measured
    /// free-space delta per volume and in total, the discrepancy and its explanations. `None`
    /// without an operation or with measurement turned off
    /// ([`Operation::without_measurement`]).
    pub measurement: Option<MeasuredDelta>,
}

impl ExecutionReport {
    /// `None` when every item succeeded; otherwise the error kind for the exit code:
    /// [`ErrorKind::Cancelled`] when cancelled, else [`ErrorKind::PartialFailure`] (exit 3:
    /// some items were skipped, refused, blocked or incomplete; each says why).
    pub fn error_kind(&self) -> Option<ErrorKind> {
        if self.cancelled {
            Some(ErrorKind::Cancelled)
        } else if self.items.iter().all(ItemReport::succeeded) {
            None
        } else {
            Some(ErrorKind::PartialFailure)
        }
    }

    /// The reports of skipped items.
    pub fn skipped(&self) -> impl Iterator<Item = (&ItemReport, &Skip)> {
        self.items.iter().filter_map(|r| match &r.outcome {
            ItemOutcome::Skipped(s) => Some((r, s)),
            _ => None,
        })
    }
}

impl Executor {
    /// Execute `plan` while holding the execution lock (an Executor made with
    /// [`Executor::acquire`]; otherwise [`ErrorKind::SafetyBlocked`]). Verify the plan
    /// ([`Plan::verify`]: edited, expired or foreign plans are rejected with an exit-code-4
    /// error and nothing is touched), then for **each** item in turn, immediately before
    /// acting on it, revalidate it against the disk ([`Revalidation::revalidate`]: bounded
    /// re-derivation, [`scull_core::narrow`], identity, kind, tracking, in-use, tier) and only
    /// then execute it. An item that fails revalidation is skipped and reported; the others
    /// continue (SAFE-05).
    ///
    /// A plan prepared by the MCP server ([`PlanOrigin::Mcp`]) is refused
    /// ([`ErrorKind::SafetyBlocked`]) unless `operation` is the one
    /// [`Executor::begin_operation`] started for it, which consumed its human approval
    /// (MCP-04). An auto-policy plan ([`PlanOrigin::Policy`], D-11) likewise runs only inside
    /// the operation started for it, and holds only S0 quarantines.
    ///
    /// With an `operation` ([`Executor::begin_operation`]), every mutating step is announced
    /// first in the operation's write-ahead journal and closed after it (SAFE-19, see
    /// [`crate::journal`]; [`Executor::recover`] reconciles what a crash left open), every
    /// item's outcome is recorded in its audit log right after it is handled (SAFE-08), and
    /// quarantine is available:
    ///
    /// * [`Action::Delete`] (S0): the removal walker;
    /// * [`Action::Quarantine`] (S1/S2): a same-volume, no-replace move into the operation's
    ///   quarantine folder, recorded in the manifest of record (D-3, SAFE-16); **only** where
    ///   that is impossible (another volume, no writable or private quarantine location,
    ///   read-only, no no-replace rename) it falls back to the Trash
    ///   ([`Operation::with_trash`], [`ItemOutcome::Trashed`]), and where that is impossible
    ///   too (or no Trash is configured) the item is [`ItemOutcome::Blocked`]. It is **never**
    ///   deleted as a fallback. A quarantine refused because the item changed is not retried
    ///   in the Trash ([`ItemOutcome::Refused`]);
    /// * [`Action::Trash`]: a move to the configured Trash, else blocked;
    /// * [`Action::GitWorktreeRemove`]: only an eligible S1 linked worktree, in an operation,
    ///   through fixed hardened Git arguments, fresh identity/registration/status checks, no force
    ///   and no branch deletion;
    /// * anything else is refused per item (tool-native #78); a Docker command
    ///   ([`crate::DOCKER_UNSUPPORTED`]) is refused before revalidation, without touching the
    ///   disk.
    ///
    /// With an operation, the free-space delta of the affected volumes (the scan roots) is
    /// measured before and after ([`Operation::with_meter`]; PHY-02): the accuracy report is
    /// [`ExecutionReport::measurement`] and [`Operation::finish`] records the measured delta
    /// next to the predicted bytes (invariant I-5).
    ///
    /// If the journal or the audit can't be written the run stops at that item and the error
    /// is returned (an intent that can't be journaled means the step is not run at all).
    pub fn execute_plan(
        &self,
        plan: &Plan,
        ctx: &PlanContext<'_>,
        revalidation: &Revalidation<'_>,
        mut operation: Option<&mut Operation<'_>>,
        cancel: &CancelToken,
    ) -> Result<ExecutionReport, ScullError> {
        if self.lock().is_none() {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "plans are executed only while holding the execution lock (SAFE-09): use \
                 Executor::acquire",
            ));
        }
        plan.verify(ctx.now_unix, ctx.machine_id, ctx.user)?;
        // MCP-04: an agent's plan runs only inside the operation that consumed its human
        // approval (`begin_operation`), never on its own.
        if plan.origin() == PlanOrigin::Mcp
            && !operation.as_deref().is_some_and(|op| op.is_for(plan))
        {
            // Audited where there is a store to record it in (another plan's operation).
            if let Some(op) = operation.as_deref_mut() {
                crate::operation::audit_refusal(
                    op.store,
                    plan,
                    "outside_operation",
                    self.refusal_source(),
                );
            }
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!(
                    "plan {} was prepared by an AI agent: it runs only inside the operation that \
                     consumed its human approval (MCP-04); nothing was changed",
                    plan.id()
                ),
            ));
        }
        // D-11: an auto-policy plan runs only inside its own recorded operation (audit,
        // journal, quarantine manifest). `Plan::verify` above already refused any item that is
        // not an S0 quarantine.
        if plan.origin() == PlanOrigin::Policy
            && !operation.as_deref().is_some_and(|op| op.is_for(plan))
        {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!(
                    "auto-policy plan {} runs only inside its own recorded operation (D-11); \
                     nothing was changed",
                    plan.id()
                ),
            ));
        }
        let mut report = ExecutionReport {
            plan_id: plan.id().clone(),
            items: Vec::with_capacity(plan.items().len()),
            cancelled: false,
            measurement: None,
        };
        let baseline = operation
            .as_deref()
            .and_then(|op| op.meter.as_ref())
            .map(|m| m.before(&revalidation.root_paths()));
        let mut tally = Tally::new();
        for item in plan.items() {
            let mut entry = ItemReport {
                item_id: item.item_id.clone(),
                path: item.path.clone(),
                planned_tier: item.tier,
                fresh_tier: None,
                outcome: ItemOutcome::NotRun,
            };
            let mut sizes: Sizes = planned_sizes(item);
            if report.cancelled || cancel.is_cancelled() {
                report.cancelled = true;
                tally.add(
                    &ActionReclaim::for_action(&sizes, &item.action),
                    &[],
                    ItemExecution::NotExecuted,
                );
                report.items.push(entry);
                continue;
            }
            // Docker items are refused before revalidation, with their own reason: no
            // Executor path runs `docker` (P5-DET-03 is read-only).
            if let Err(e) = crate::executor::refuse_docker(item) {
                entry.outcome = ItemOutcome::Refused(e);
            } else {
                entry.outcome = match revalidation.revalidate(plan, item, cancel) {
                    Err(Skip::Cancelled) => {
                        report.cancelled = true;
                        ItemOutcome::NotRun
                    }
                    Err(skip) => {
                        entry.fresh_tier = skip.fresh_tier();
                        ItemOutcome::Skipped(skip)
                    }
                    Ok(ok) => {
                        entry.fresh_tier = Some(ok.fresh_tier);
                        sizes = ok.sizes;
                        self.act(&ok, operation.as_deref_mut(), cancel)
                    }
                };
            }
            tally.add(
                &ActionReclaim::for_action(&sizes, &item.action),
                &[],
                entry.execution(),
            );
            if let Some(op) = operation.as_deref_mut()
                && let Some(rec) = entry.audit_record(now_unix())
            {
                let journaled = op.open_step.is_some();
                op.journal_close(Some(&rec), rec.recorded_at)?;
                if journaled {
                    at(self.hook(), StepPoint::AfterClose(&item.item_id));
                }
                op.record(&rec)?;
                at(self.hook(), StepPoint::AfterAudit(&item.item_id));
            }
            report.items.push(entry);
        }
        if let (Some(op), Some(baseline)) = (operation, baseline)
            && let Some(meter) = op.meter.as_ref()
        {
            let delta = meter.after(baseline, &tally);
            op.measured = delta.measured_delta_bytes();
            report.measurement = Some(delta);
        }
        Ok(report)
    }

    /// Executes one revalidated item.
    fn act(
        &self,
        ok: &Revalidated,
        operation: Option<&mut Operation<'_>>,
        cancel: &CancelToken,
    ) -> ItemOutcome {
        match (&ok.item.action, operation) {
            (Action::GitWorktreeRemove, Some(op)) => {
                if let Err(e) =
                    crate::executor::check_item_for(&ok.item, &Action::GitWorktreeRemove)
                        .and_then(|()| journal_delete(op, ok, self))
                {
                    return ItemOutcome::Refused(e);
                }
                match crate::worktree::remove(ok, cancel, self.hook()) {
                    Ok(r) if r.succeeded => ItemOutcome::WorktreeRemoved {
                        freed_bytes: r.bytes,
                    },
                    Ok(r) => ItemOutcome::WorktreeRemovalFailed {
                        reason: r
                            .reason
                            .unwrap_or_else(|| "worktree removal did not complete".into()),
                    },
                    Err(e) => ItemOutcome::Refused(e),
                }
            }
            (Action::GitWorktreeRemove, None) => ItemOutcome::Refused(ScullError::new(
                ErrorKind::SafetyBlocked,
                "worktree removal requires an audited operation",
            )),
            (Action::Quarantine, Some(op)) => {
                if let Err(e) = crate::executor::check_item_for(&ok.item, &Action::Quarantine) {
                    return ItemOutcome::Refused(e);
                }
                match op.quarantine(ok, now_unix(), self.hook()) {
                    Ok(q) => ItemOutcome::Quarantined(q),
                    Err(QuarantineRefusal::Impossible(why)) => self.trash_fallback(ok, op, &why),
                    Err(QuarantineRefusal::Blocked { code, message }) => {
                        ItemOutcome::Blocked { code, message }
                    }
                    Err(QuarantineRefusal::Failed(e)) => ItemOutcome::Refused(
                        e.with_item(ok.item.item_id.clone())
                            .with_path(ok.item.path.clone()),
                    ),
                }
            }
            (Action::Trash, Some(op)) => self.trash_step(ok, op, None),
            (Action::Quarantine, None) => ItemOutcome::Refused(
                ScullError::new(
                    ErrorKind::SafetyBlocked,
                    "quarantine needs an operation record (the manifest of record, SAFE-16)",
                )
                .with_item(ok.item.item_id.clone()),
            ),
            (_, operation) => {
                let target = DeleteTarget {
                    anchor: &ok.anchor,
                    anchor_identity: ok.anchor_identity,
                    path: &ok.path,
                };
                if let Some(op) = operation {
                    // Refusals that need no disk access are not journaled.
                    if let Err(e) = crate::executor::check_item_for(&ok.item, &Action::Delete) {
                        return ItemOutcome::Refused(e);
                    }
                    if let Err(e) = journal_delete(op, ok, self) {
                        return ItemOutcome::Refused(e.with_item(ok.item.item_id.clone()));
                    }
                }
                let id = &ok.item.item_id;
                let mut events = 0u64;
                let mut observe = |_: &WalkEvent<'_>| {
                    events += 1;
                    at(self.hook(), StepPoint::Walk(id, events));
                };
                let observer: Option<&mut dyn FnMut(&WalkEvent<'_>)> = if self.hook().is_some() {
                    Some(&mut observe)
                } else {
                    None
                };
                match self.run(&ok.item, &target, Some(ok.kind), observer) {
                    Ok(r) => ItemOutcome::Deleted(r.report),
                    Err(e) => ItemOutcome::Refused(e),
                }
            }
        }
    }
}

impl Executor {
    /// Quarantine was impossible (`why`): the Trash fallback, if one is configured, else
    /// blocked. Never a deletion.
    fn trash_fallback(&self, ok: &Revalidated, op: &mut Operation<'_>, why: &str) -> ItemOutcome {
        if op.trash.is_none() {
            return ItemOutcome::Blocked {
                code: "quarantine_impossible",
                message: format!(
                    "quarantine is impossible here ({why}) and no Trash fallback is configured \
                     for this operation: the item was left in place, never deleted instead"
                ),
            };
        }
        // A quarantine attempt that reached the journal changed nothing: close it so the Trash
        // step is the item's current one (SAFE-19).
        if let Err(e) = op.journal_close(None, now_unix()) {
            return ItemOutcome::Refused(e.with_item(ok.item.item_id.clone()));
        }
        self.trash_step(ok, op, Some(why))
    }

    /// Moves a revalidated item to the operation's Trash, journaled. `quarantine_why`: the
    /// item was planned for quarantine and this is its fallback.
    fn trash_step(
        &self,
        ok: &Revalidated,
        op: &mut Operation<'_>,
        quarantine_why: Option<&str>,
    ) -> ItemOutcome {
        let Some(locations) = op.trash.clone() else {
            return ItemOutcome::Blocked {
                code: "trash_unavailable",
                message: "no Trash is configured for this operation: the item was left in \
                          place, never deleted instead"
                    .into(),
            };
        };
        if let Some(why) = trash_outside_test_sandbox(&locations) {
            return ItemOutcome::Blocked {
                code: "outside_test_sandbox",
                message: format!("{why}: the item was left in place, never deleted instead"),
            };
        }
        let id = &ok.item.item_id;
        at(self.hook(), StepPoint::BeforeIntent(id));
        let intent = JournalIntent {
            operation_id: op.id().clone(),
            item_id: id.clone(),
            action: JournalAction::Trash,
            source_path: FsPath::from_path(&ok.path),
            source_identity: ok.item.identity,
            source_kind: ok.kind.as_str().to_owned(),
            source_parent: parent_identity(&ok.path),
            destination: None,
            destination_dir: None,
            tree: None,
            recorded_at: now_unix(),
        };
        if let Err(e) = op.journal_intent(&intent) {
            return ItemOutcome::Refused(e.with_item(id.clone()));
        }
        at(self.hook(), StepPoint::AfterIntent(id));
        let target = DeleteTarget {
            anchor: &ok.anchor,
            anchor_identity: ok.anchor_identity,
            path: &ok.path,
        };
        match self.trash_checked(&ok.item, &target, &locations, Some(ok.kind)) {
            Ok(t) => {
                at(self.hook(), StepPoint::AfterMove(id));
                ItemOutcome::Trashed(t)
            }
            // Nothing moved, nothing deleted. "No usable Trash here" is the end of the chain:
            // blocked. An item that changed is refused like any other.
            Err(e)
                if e.kind == ErrorKind::SafetyBlocked || e.kind == ErrorKind::PermissionDenied =>
            {
                ItemOutcome::Blocked {
                    code: if quarantine_why.is_some() {
                        "quarantine_and_trash_impossible"
                    } else {
                        "trash_unavailable"
                    },
                    message: match quarantine_why {
                        Some(why) => format!(
                            "quarantine is impossible here ({why}) and so is the Trash ({}): the \
                             item was left in place, never deleted instead",
                            e.message
                        ),
                        None => format!(
                            "the Trash is unavailable ({}): the item was left in place, never \
                             deleted instead",
                            e.message
                        ),
                    },
                }
            }
            Err(e) => ItemOutcome::Refused(e),
        }
    }
}

/// Under the test harness, why the Trash at `locations` may not be used: its home trash or its
/// volume top directory is outside the sandbox, or the top directory is left to the platform
/// (the item's mount point, the real per-volume Trash / Recycle Bin).
fn trash_outside_test_sandbox(locations: &crate::trash::TrashLocations) -> Option<String> {
    crate::quarantine::test_sandbox()?;
    let Some(top) = &locations.topdir else {
        return Some(
            "running under the test harness: the real per-volume Trash is never used there".into(),
        );
    };
    locations
        .home
        .iter()
        .chain(std::iter::once(top))
        .find_map(|p| crate::quarantine::outside_test_sandbox(p))
}

/// Identity of the directory holding `path`, looked up no-follow.
fn parent_identity(path: &std::path::Path) -> Option<scull_core::FileIdentity> {
    path.parent()
        .and_then(|p| Native.metadata(p).ok())
        .and_then(|m| m.identity)
}

/// Journals the intent of a removal walk (SAFE-19): path, identity, kind, parent identity and
/// the tree's fingerprint, so recovery can tell an untouched tree from a partly removed one.
fn journal_delete(
    op: &mut Operation<'_>,
    ok: &Revalidated,
    exec: &Executor,
) -> Result<(), ScullError> {
    let id = &ok.item.item_id;
    at(exec.hook(), StepPoint::BeforeIntent(id));
    let intent = JournalIntent {
        operation_id: op.id().clone(),
        item_id: id.clone(),
        action: JournalAction::Delete,
        source_path: FsPath::from_path(&ok.path),
        source_identity: ok.item.identity,
        source_kind: ok.kind.as_str().to_owned(),
        source_parent: parent_identity(&ok.path),
        destination: None,
        destination_dir: None,
        tree: Some(fingerprint(&ok.path)),
        recorded_at: now_unix(),
    };
    op.journal_intent(&intent)?;
    at(exec.hook(), StepPoint::AfterIntent(id));
    Ok(())
}
