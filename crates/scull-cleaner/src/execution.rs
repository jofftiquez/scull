//! Executing a whole plan: verify it, then revalidate and execute each item in turn
//! (SAFE-05, SAFE-11, P2-EXEC-03), recording each outcome in the operation's audit (SAFE-08).

use crate::executor::{DeleteTarget, Executor};
use crate::operation::Operation;
use crate::quarantine::{QuarantineRefusal, Quarantined};
use crate::revalidate::{Revalidated, Revalidation, Skip};
use scull_core::{Action, ErrorKind, FsPath, ItemId, Plan, PlanId, ScullError, Tier};
use scull_engine::{CancelToken, now_unix};
use scull_platform::remove::{RemoveOutcome, RemoveReport};
use scull_storage::{ItemOutcome as AuditOutcome, ItemRecord, WalkEnd, WalkSummary};

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
    /// Moved into same-volume quarantine (restorable).
    Quarantined(Quarantined),
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
            ItemOutcome::Quarantined(_) => true,
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
            ItemOutcome::Quarantined(_) => None,
            ItemOutcome::Skipped(s) => Some(s.code()),
            ItemOutcome::Refused(e) => Some(e.kind.as_str()),
            ItemOutcome::Blocked { code, .. } => Some(code),
            ItemOutcome::NotRun => Some("cancelled"),
        }
    }

    /// The audit record for this outcome (`None` when the item was not attempted).
    fn audit_record(&self, at: i64) -> Option<ItemRecord> {
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
            ItemOutcome::Quarantined(q) => {
                let mut rec = ItemRecord::new(id, at, AuditOutcome::Quarantined);
                rec.moved_bytes = Some(q.bytes);
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
}

/// The result of [`Executor::execute_plan`]: one report per plan item, in plan order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExecutionReport {
    /// The executed plan.
    pub plan_id: PlanId,
    /// One entry per plan item, in plan order.
    pub items: Vec<ItemReport>,
    /// Whether the operation was cancelled before every item was attempted.
    pub cancelled: bool,
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
    /// With an `operation` ([`Executor::begin_operation`]), every item's outcome is recorded
    /// in its audit log right after it is handled (SAFE-08), and quarantine is available:
    ///
    /// * [`Action::Delete`] (S0): the removal walker;
    /// * [`Action::Quarantine`] (S1/S2): a same-volume, no-replace move into the operation's
    ///   quarantine folder, recorded in the manifest of record (D-3, SAFE-16); where that is
    ///   impossible the item is [`ItemOutcome::Blocked`] (the Trash fallback is #73), never
    ///   deleted;
    /// * anything else is refused per item (Trash #73, tool-native #78).
    ///
    /// If the audit can't be written the run stops at that item and the error is returned.
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
        let mut report = ExecutionReport {
            plan_id: plan.id().clone(),
            items: Vec::with_capacity(plan.items().len()),
            cancelled: false,
        };
        for item in plan.items() {
            let mut entry = ItemReport {
                item_id: item.item_id.clone(),
                path: item.path.clone(),
                planned_tier: item.tier,
                fresh_tier: None,
                outcome: ItemOutcome::NotRun,
            };
            if report.cancelled || cancel.is_cancelled() {
                report.cancelled = true;
                report.items.push(entry);
                continue;
            }
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
                    self.act(&ok, operation.as_deref_mut())
                }
            };
            if let Some(op) = operation.as_deref_mut()
                && let Some(rec) = entry.audit_record(now_unix())
            {
                op.record(&rec)?;
            }
            report.items.push(entry);
        }
        Ok(report)
    }

    /// Executes one revalidated item.
    fn act(&self, ok: &Revalidated, operation: Option<&mut Operation<'_>>) -> ItemOutcome {
        match (&ok.item.action, operation) {
            (Action::Quarantine, Some(op)) => {
                if let Err(e) = crate::executor::check_item_for(&ok.item, &Action::Quarantine) {
                    return ItemOutcome::Refused(e);
                }
                match op.quarantine(ok, now_unix()) {
                    Ok(q) => ItemOutcome::Quarantined(q),
                    Err(QuarantineRefusal::Impossible(why)) => ItemOutcome::Blocked {
                        code: "quarantine_impossible",
                        message: format!(
                            "quarantine is impossible here ({why}) and the Trash fallback is \
                             not available in this build (#73): the item was left in place, \
                             never deleted instead"
                        ),
                    },
                    Err(QuarantineRefusal::Failed(e)) => ItemOutcome::Refused(
                        e.with_item(ok.item.item_id.clone())
                            .with_path(ok.item.path.clone()),
                    ),
                }
            }
            (Action::Quarantine, None) => ItemOutcome::Refused(
                ScullError::new(
                    ErrorKind::SafetyBlocked,
                    "quarantine needs an operation record (the manifest of record, SAFE-16)",
                )
                .with_item(ok.item.item_id.clone()),
            ),
            _ => {
                let target = DeleteTarget {
                    anchor: &ok.anchor,
                    anchor_identity: ok.anchor_identity,
                    path: &ok.path,
                };
                match self.run(&ok.item, &target, Some(ok.kind), None) {
                    Ok(r) => ItemOutcome::Deleted(r.report),
                    Err(e) => ItemOutcome::Refused(e),
                }
            }
        }
    }
}
