//! The ready-to-review reclaim plan (PRO-05): a [`Plan`] of the latest scan's safe items, kept
//! fresh by `scull check` so a low-space alert can open straight into review.
//!
//! **A proposal, never an execution.** The plan is built exactly like `scull clean` builds a
//! CLI plan ([`scull_core::PlanOrigin::Cli`], bound to this machine and user, expiring in 24
//! hours) and goes through the same review, revalidation ([`scull_core::narrow`]) and
//! confirmation when someone runs it. Nothing here, nor in `scull check`, removes anything
//! (PRO-08, D-11: unattended cleanup is a separate, later opt-in).
//!
//! Selection matches `scull clean`'s defaults (PRD §8.2, D-28):
//!
//! * S0 items; S1 items only when the caller opts in; never S2 (named per item in a manual
//!   review only) and never S3;
//! * only items whose action `scull clean` executes from a plan (delete or quarantine) and
//!   whose tier, reasons and action agree. Package caches (cleaned with their tool's own
//!   command by `scull clean caches`) and Git worktree actions are left out.
//!
//! The prepared plan is announced with a [`EventKind::ReviewPlan`] event whose payload is a
//! [`ReviewPlanSummary`]; [`latest_review_plan`] reads it back for `scull status` and
//! `scull check`.

use schemars::JsonSchema;
use scull_core::{
    Action, Candidate, Plan, PlanId, PlanItem, PlanParams, ScullError, Tier, action_allowed_for,
};
use scull_storage::{EventKind, Store, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Whether `c` belongs in a review plan (see the module docs).
pub fn eligible(c: &Candidate, include_s1: bool) -> bool {
    let tier_ok = match c.tier {
        Tier::S0 => true,
        Tier::S1 => include_s1,
        _ => false,
    };
    tier_ok
        && matches!(c.action, Action::Delete | Action::Quarantine)
        && c.is_consistent()
        && action_allowed_for(c.tier, &c.action)
}

/// The plan items for `candidates`: every [`eligible`] candidate, in input order.
pub fn review_items(candidates: &[Candidate], include_s1: bool) -> Vec<PlanItem> {
    candidates
        .iter()
        .filter(|c| eligible(c, include_s1))
        .filter_map(|c| PlanItem::from_candidate(c).ok())
        .collect()
}

/// A review plan of `candidates`, or `None` when nothing qualifies. `params.origin` should be
/// [`scull_core::PlanOrigin::Cli`] (the plan is reviewed and run with `scull clean --plan`).
pub fn build_review_plan(
    params: PlanParams,
    candidates: &[Candidate],
    include_s1: bool,
) -> Result<Option<Plan>, ScullError> {
    let items = review_items(candidates, include_s1);
    if items.is_empty() {
        return Ok(None);
    }
    Plan::new(params, items).map(Some)
}

/// Whether two item lists propose the same work: the same items (ID and file identity) at the
/// same tiers with the same actions. Sizes may differ (they drift between scans).
pub fn same_work(a: &[PlanItem], b: &[PlanItem]) -> bool {
    fn keys(items: &[PlanItem]) -> Vec<(&str, scull_core::FileIdentity, Tier, &Action)> {
        let mut v: Vec<_> = items
            .iter()
            .map(|i| (i.item_id.as_str(), i.identity, i.tier, &i.action))
            .collect();
        v.sort_by(|x, y| x.0.cmp(y.0));
        v
    }
    a.len() == b.len() && keys(a) == keys(b)
}

/// What `scull check` announces about a prepared plan (the payload of its
/// [`EventKind::ReviewPlan`] event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewPlanSummary {
    /// The plan's ID (recorded in the database's `plans` table).
    pub plan_id: PlanId,
    /// The plan file (`scull clean --plan <file>` reviews it).
    pub file: String,
    /// Items in the plan.
    pub items: u64,
    /// Estimated reclaimable bytes of all items.
    pub reclaimable_bytes: u64,
    /// When it was prepared (Unix seconds).
    pub created_at: i64,
    /// When it expires (Unix seconds); after that it is ignored.
    pub expires_at: i64,
    /// Whether S1 (costly to recreate) items were included (opt-in).
    pub includes_s1: bool,
}

impl ReviewPlanSummary {
    /// The summary of `plan`, written to `file`.
    pub fn of(plan: &Plan, file: impl Into<String>, includes_s1: bool) -> Self {
        Self {
            plan_id: plan.id().clone(),
            file: file.into(),
            items: plan.items().len() as u64,
            reclaimable_bytes: plan
                .items()
                .iter()
                .fold(0u64, |s, i| s.saturating_add(i.reclaimable_bytes)),
            created_at: plan.created_unix(),
            expires_at: plan.expires_unix(),
            includes_s1,
        }
    }

    /// `scull clean --plan <file>`.
    pub fn review_command(&self) -> String {
        let file = if self.file.contains(char::is_whitespace) {
            format!("\"{}\"", self.file)
        } else {
            self.file.clone()
        };
        format!("scull clean --plan {file}")
    }
}

/// The latest review plan announced, if it has not expired by `now` and its record is still in
/// the database. Unreadable payloads are ignored (an announcement never breaks a status line).
pub fn latest_review_plan(
    store: &Store,
    now: i64,
) -> Result<Option<ReviewPlanSummary>, StoreError> {
    let Some(event) = store.latest_event(EventKind::ReviewPlan)? else {
        return Ok(None);
    };
    let Ok(summary) = serde_json::from_value::<ReviewPlanSummary>(event.payload) else {
        return Ok(None);
    };
    if summary.expires_at <= now {
        return Ok(None);
    }
    Ok(Some(summary))
}

/// The item IDs of a plan, for display and tests.
pub fn item_ids(plan: &Plan) -> BTreeSet<String> {
    plan.items()
        .iter()
        .map(|i| i.item_id.as_str().to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::{PlanOrigin, ReasonCode, ScanId, TierReason};

    fn example() -> Candidate {
        serde_json::from_str(include_str!("../../../schemas/v1/examples/candidate.json"))
            .expect("example candidate")
    }

    /// A consistent candidate at `tier` with `action`, at `path`.
    fn cand(path: &str, tier: Tier, action: Action, bytes: u64) -> Candidate {
        let mut c = example();
        c.path = scull_core::FsPath::from(path);
        c.identity = scull_core::FileIdentity::new(1, path.len() as u128 + bytes as u128);
        c.id = scull_core::ItemId::derive(c.identity, path.as_bytes());
        c.tier = tier;
        let code = match tier {
            Tier::S0 => ReasonCode::Regenerable,
            _ => ReasonCode::CostlyToRegenerate,
        };
        c.reasons = vec![TierReason::new(tier, code, "test")];
        c.action = action;
        c.sizes.reclaimable.bytes = bytes;
        c.sizes.allocated = bytes;
        c
    }

    fn params(n: &str) -> PlanParams {
        PlanParams {
            id: PlanId::from_suffix(n).expect("id"),
            scan_id: ScanId::from_suffix("s1").expect("id"),
            created_unix: 1_800_000_000,
            ttl_secs: None,
            machine_id: "m".into(),
            user: "u".into(),
            origin: PlanOrigin::Cli,
        }
    }

    #[test]
    fn selects_s0_and_opt_in_s1_with_executable_actions_only() {
        let cands = vec![
            cand("/p/a/target", Tier::S0, Action::Delete, 10),
            cand("/p/b/node_modules", Tier::S1, Action::Quarantine, 20),
            cand("/p/c/thing", Tier::S2, Action::Quarantine, 30),
            cand("/p/d/vault", Tier::S3, Action::None, 40),
            cand("/p/e/view", Tier::S0, Action::None, 50),
        ];
        for c in &cands {
            assert!(c.is_consistent(), "fixture must be consistent: {}", c.path);
        }
        let s0: Vec<String> = review_items(&cands, false)
            .iter()
            .map(|i| i.path.as_str().to_owned())
            .collect();
        assert_eq!(s0, vec!["/p/a/target"]);
        let with_s1: Vec<String> = review_items(&cands, true)
            .iter()
            .map(|i| i.path.as_str().to_owned())
            .collect();
        assert_eq!(with_s1, vec!["/p/a/target", "/p/b/node_modules"]);

        let plan = build_review_plan(params("r1"), &cands, true)
            .expect("plan")
            .expect("some");
        assert_eq!(plan.origin(), PlanOrigin::Cli);
        assert!(plan.verify(1_800_000_060, "m", "u").is_ok());
        let s = ReviewPlanSummary::of(&plan, "/d/review-plan.json", true);
        assert_eq!((s.items, s.reclaimable_bytes), (2, 30));
        assert_eq!(s.expires_at - s.created_at, scull_core::CLI_PLAN_TTL_SECS);
        assert_eq!(s.review_command(), "scull clean --plan /d/review-plan.json");
        let spaced = ReviewPlanSummary::of(&plan, "/My Data/review-plan.json", true);
        assert_eq!(
            spaced.review_command(),
            "scull clean --plan \"/My Data/review-plan.json\""
        );

        // Nothing eligible: no plan.
        assert!(
            build_review_plan(params("r2"), &cands[2..], true)
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn same_work_ignores_sizes_and_order_but_not_items_or_actions() {
        let a = cand("/p/a/target", Tier::S0, Action::Delete, 10);
        let b = cand("/p/b/target", Tier::S0, Action::Delete, 20);
        let items = review_items(&[a.clone(), b.clone()], false);
        let mut grown = b.clone();
        grown.sizes.reclaimable.bytes = 99;
        let reordered = review_items(&[grown, a.clone()], false);
        assert!(same_work(&items, &reordered));
        assert!(!same_work(
            &items,
            &review_items(std::slice::from_ref(&a), false)
        ));
        let mut quarantined = b;
        quarantined.action = Action::Quarantine;
        assert!(!same_work(&items, &review_items(&[a, quarantined], false)));
    }
}
