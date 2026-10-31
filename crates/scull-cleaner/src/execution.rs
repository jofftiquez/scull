//! Executing a whole plan: verify it, then revalidate and execute each item in turn
//! (SAFE-05, SAFE-11, P2-EXEC-03).

use crate::executor::{DeleteTarget, Executor};
use crate::revalidate::{Revalidation, Skip};
use scull_core::{ErrorKind, FsPath, ItemId, Plan, PlanId, ScullError, Tier};
use scull_engine::CancelToken;
use scull_platform::remove::{RemoveOutcome, RemoveReport};

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
    /// Revalidation failed: the item was skipped and nothing was touched.
    Skipped(Skip),
    /// The item passed revalidation but the Executor refused it (nothing was removed), e.g.
    /// an action this build does not execute, or the walker's own last-instant check.
    Refused(ScullError),
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
        matches!(&self.outcome, ItemOutcome::Deleted(r) if r.outcome == RemoveOutcome::Complete)
    }

    /// Stable reason code for an item that did not succeed, for reports and the audit log.
    pub fn reason_code(&self) -> Option<&'static str> {
        match &self.outcome {
            ItemOutcome::Deleted(r) => match r.outcome {
                RemoveOutcome::Complete => None,
                RemoveOutcome::Incomplete => Some("incomplete"),
                _ => Some("aborted"),
            },
            ItemOutcome::Skipped(s) => Some(s.code()),
            ItemOutcome::Refused(e) => Some(e.kind.as_str()),
            ItemOutcome::NotRun => Some("cancelled"),
        }
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
    /// some items were skipped, refused or incomplete; each says why).
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
    /// Execute `plan`: verify it ([`Plan::verify`]: edited, expired or foreign plans are
    /// rejected with an exit-code-4 error and nothing is touched), then for **each** item in
    /// turn, immediately before acting on it, revalidate it against the disk
    /// ([`Revalidation::revalidate`]: bounded re-derivation, [`scull_core::narrow`], identity,
    /// kind, tracking, in-use, tier) and only then execute it. An item that fails
    /// revalidation is skipped and reported; the others continue (SAFE-05).
    ///
    /// Only [`scull_core::Action::Delete`] of S0 items is executed by this build; other
    /// actions are refused per item (quarantine #71, Trash #73, tool-native #78).
    pub fn execute_plan(
        &self,
        plan: &Plan,
        ctx: &PlanContext<'_>,
        revalidation: &Revalidation<'_>,
        cancel: &CancelToken,
    ) -> Result<ExecutionReport, ScullError> {
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
            };
            report.items.push(entry);
        }
        Ok(report)
    }
}
