//! The ready-to-review reclaim plan `scull check` keeps (PRO-05, P4-PRO-04).
//!
//! After every check that scanned, the latest scan's safe items (S0; S1 too with
//! `--include-s1`, D-28) become a CLI [`Plan`], exactly as `scull clean` would make one:
//! bound to this machine and user ([`scull_platform::machine_id`],
//! [`scull_platform::current_user`]), expiring in 24 hours, recorded in the database's
//! append-only `plans` table and written to [`REVIEW_PLAN_FILE`] in scull's data directory
//! (owner-only). `scull status` and `scull check` then say "A 12 GB cleanup plan is ready to
//! review: scull clean --plan …".
//!
//! **Never executed here.** The plan is a proposal: `scull clean --plan <file>` reviews it,
//! re-derives every item from the disk (revalidation narrows, never widens) and asks for
//! confirmation. In this build `scull clean` is not available yet (Phase 2), so the plan file
//! can only be inspected.
//!
//! A plan proposing the same work as the current one (same items, tiers and actions) that
//! still has more than [`REUSE_MIN_SECS`] to live is kept rather than recorded again, so the
//! append-only table grows by about one plan a day on a quiet machine, not one per check.

use schemars::JsonSchema;
use scull_core::{Candidate, ErrorKind, Plan, PlanId, PlanOrigin, PlanParams, ScanId, ScullError};
use scull_proactive::review::{ReviewPlanSummary, latest_review_plan, review_items, same_work};
use scull_storage::{Event, EventKind, Store};
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name of the review plan inside scull's data directory.
pub const REVIEW_PLAN_FILE: &str = "review-plan.json";

/// A current plan with at least this much time left (6 hours, the default check interval)
/// is reused when the work it proposes has not changed.
pub const REUSE_MIN_SECS: i64 = 6 * 60 * 60;

/// The review plan as reported by `scull check` / `scull status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewPlanReport {
    /// The plan's ID (in the database's `plans` table).
    pub plan_id: String,
    /// The plan file.
    pub file: String,
    /// Items in the plan.
    pub items: u64,
    /// Estimated reclaimable bytes.
    pub reclaimable_bytes: u64,
    /// When it was prepared (Unix seconds).
    pub created_at: i64,
    /// When it expires (Unix seconds).
    pub expires_at: i64,
    /// Whether S1 items were included (`--include-s1`).
    pub includes_s1: bool,
    /// The command that reviews it.
    pub review_command: String,
    /// True when this check kept the existing plan (same work) instead of making a new one.
    pub reused: bool,
}

impl ReviewPlanReport {
    /// From an announcement.
    pub fn from_summary(s: &ReviewPlanSummary, reused: bool) -> Self {
        Self {
            plan_id: s.plan_id.as_str().to_owned(),
            file: s.file.clone(),
            items: s.items,
            reclaimable_bytes: s.reclaimable_bytes,
            created_at: s.created_at,
            expires_at: s.expires_at,
            includes_s1: s.includes_s1,
            review_command: s.review_command(),
            reused,
        }
    }
}

/// Who the plan is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// This installation's machine ID.
    pub machine_id: String,
    /// The account scull runs as.
    pub user: String,
}

impl Binding {
    /// The real machine ID (created in `data_dir` on first use) and the current user.
    pub fn current(data_dir: &Path) -> Result<Self, ScullError> {
        let machine_id = scull_platform::machine_id(data_dir).map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot read or create this machine's ID: {e}"),
            )
        })?;
        let user = scull_platform::current_user().map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot tell which account scull runs as: {e}"),
            )
        })?;
        Ok(Self { machine_id, user })
    }
}

/// The scan a review plan is built from.
#[derive(Debug, Clone, Copy)]
pub struct ReviewScan<'a> {
    /// Its ID.
    pub id: &'a ScanId,
    /// Its candidates.
    pub candidates: &'a [Candidate],
    /// SAFE-09: the last operation recorded before it started (`Store::operations_watermark`).
    /// A review plan is only prepared when it could be read.
    pub operations_watermark: u64,
}

/// Refresh the review plan from the candidates of scan `scan_id` (see the module docs). Returns the current plan, or
/// `None` when nothing qualifies (a previous plan is then withdrawn).
pub fn refresh(
    store: &mut Store,
    data_dir: &Path,
    scan: &ReviewScan<'_>,
    include_s1: bool,
    binding: &Binding,
    now: i64,
) -> Result<Option<ReviewPlanReport>, ScullError> {
    let (scan_id, candidates) = (scan.id, scan.candidates);
    let items = review_items(candidates, include_s1);
    let current = latest_review_plan(store, now).map_err(ScullError::from)?;
    if items.is_empty() {
        if current.is_some() {
            // Withdraw the announcement: a payload that is not a plan summary clears it.
            store
                .record_events(&[Event {
                    event_id: 0,
                    at: now,
                    kind: EventKind::ReviewPlan,
                    source: "check".into(),
                    payload: serde_json::json!({ "withdrawn_at": now }),
                }])
                .map_err(ScullError::from)?;
        }
        return Ok(None);
    }
    if let Some(s) = &current
        && s.includes_s1 == include_s1
        && s.expires_at - now > REUSE_MIN_SECS
        && Path::new(&s.file).is_file()
        && store
            .get_plan(&s.plan_id)
            .map_err(ScullError::from)?
            .is_some_and(|old| same_work(old.items(), &items))
    {
        return Ok(Some(ReviewPlanReport::from_summary(s, true)));
    }
    let plan = Plan::new(
        PlanParams {
            id: new_plan_id(now)?,
            scan_id: scan_id.clone(),
            created_unix: now,
            ttl_secs: None,
            machine_id: binding.machine_id.clone(),
            user: binding.user.clone(),
            origin: PlanOrigin::Cli,
            // SAFE-09: the last operation recorded before its scan started.
            operations_watermark: Some(scan.operations_watermark),
        },
        items,
    )?;
    store.insert_plan(&plan).map_err(ScullError::from)?;
    let file = write_plan_file(data_dir, &plan)?;
    let summary = ReviewPlanSummary::of(&plan, file.to_string_lossy(), include_s1);
    let payload = serde_json::to_value(&summary)
        .map_err(|e| ScullError::new(ErrorKind::Internal, e.to_string()))?;
    store
        .record_events(&[Event {
            event_id: 0,
            at: now,
            kind: EventKind::ReviewPlan,
            source: "check".into(),
            payload,
        }])
        .map_err(ScullError::from)?;
    Ok(Some(ReviewPlanReport::from_summary(&summary, false)))
}

/// Write `plan` to [`REVIEW_PLAN_FILE`] in `data_dir`: an owner-only temporary file named
/// after the plan, then renamed over the previous plan file (readers never see half a plan).
pub fn write_plan_file(data_dir: &Path, plan: &Plan) -> Result<PathBuf, ScullError> {
    scull_platform::create_dir_owner_only(data_dir)
        .map_err(|e| ScullError::new(ErrorKind::Io, e.to_string()))?;
    let target = data_dir.join(REVIEW_PLAN_FILE);
    let tmp = data_dir.join(format!(".{}.tmp", plan.id().as_str()));
    let json = serde_json::to_string_pretty(plan)
        .map_err(|e| ScullError::new(ErrorKind::Internal, e.to_string()))?;
    let written = scull_platform::create_file_owner_only(&tmp)
        .map_err(|e| ScullError::new(ErrorKind::Io, e.to_string()))
        .and_then(|mut f| {
            f.write_all(json.as_bytes())
                .and_then(|()| f.write_all(b"\n"))
                .and_then(|()| f.sync_all())
                .map_err(|e| ScullError::from_io(&e))
        })
        .and_then(|()| std::fs::rename(&tmp, &target).map_err(|e| ScullError::from_io(&e)));
    if let Err(e) = written {
        // Our own temporary file, never anything else.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(target)
}

/// A new plan ID: Unix seconds then 64 random bits, in hex (sorts roughly by time; the same
/// shape as `scull clean`'s plan IDs).
pub fn new_plan_id(now: i64) -> Result<PlanId, ScullError> {
    use std::hash::{BuildHasher, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let random = RandomState::new().hash_one((nanos, n, std::process::id()));
    PlanId::from_suffix(&format!(
        "{:x}{random:016x}",
        u64::try_from(now).unwrap_or(0)
    ))
    .map_err(|e| ScullError::new(ErrorKind::Internal, e))
}

/// The current review plan for `scull status`, if any (never fails the status line).
pub fn current(store: &Store, now: i64) -> Option<ReviewPlanReport> {
    latest_review_plan(store, now)
        .ok()
        .flatten()
        .map(|s| ReviewPlanReport::from_summary(&s, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_storage::DefaultPermissions;

    const NOW: i64 = 1_800_000_000;

    fn candidate(path: &str) -> Candidate {
        let mut c: Candidate = serde_json::from_str(include_str!(
            "../../../../schemas/v1/examples/candidate.json"
        ))
        .expect("example candidate");
        c.path = scull_core::FsPath::from(path);
        c.identity = scull_core::FileIdentity::new(7, path.len() as u128);
        c.id = scull_core::ItemId::derive(c.identity, path.as_bytes());
        c
    }

    fn scanned<'a>(id: &'a ScanId, candidates: &'a [Candidate]) -> ReviewScan<'a> {
        ReviewScan {
            id,
            candidates,
            operations_watermark: 0,
        }
    }

    fn binding() -> Binding {
        Binding {
            machine_id: "a".repeat(32),
            user: "uid:1000".into(),
        }
    }

    #[test]
    fn prepares_reuses_refreshes_and_withdraws_the_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        let mut store = Store::open(&dir.path().join("scull.db"), &DefaultPermissions).expect("db");
        let scan = ScanId::from_suffix("s1").expect("id");
        let one = vec![candidate("/p/a/target")];
        assert_eq!(one[0].tier, scull_core::Tier::S0);

        // First check: a new plan, recorded, written owner-only, announced.
        let p = refresh(
            &mut store,
            &data,
            &scanned(&scan, &one),
            false,
            &binding(),
            NOW,
        )
        .expect("refresh")
        .expect("plan");
        assert!(!p.reused);
        assert_eq!(p.items, 1);
        assert_eq!(p.expires_at - p.created_at, scull_core::CLI_PLAN_TTL_SECS);
        let file = data.join(REVIEW_PLAN_FILE);
        assert_eq!(p.file, file.to_string_lossy());
        assert!(scull_platform::ensure_owner_only(&file).is_ok());
        let plan: Plan =
            serde_json::from_slice(&std::fs::read(&file).expect("file")).expect("plan");
        assert_eq!(plan.id().as_str(), p.plan_id);
        assert_eq!(plan.origin(), PlanOrigin::Cli);
        assert!(plan.verify(NOW + 60, &"a".repeat(32), "uid:1000").is_ok());
        assert_eq!(store.get_plan(plan.id()).expect("get"), Some(plan.clone()));
        assert_eq!(
            current(&store, NOW + 60).expect("current").plan_id,
            p.plan_id
        );

        // Same work an hour later: kept.
        let again = refresh(
            &mut store,
            &data,
            &scanned(&scan, &one),
            false,
            &binding(),
            NOW + 3_600,
        )
        .expect("refresh")
        .expect("plan");
        assert!(again.reused);
        assert_eq!(again.plan_id, p.plan_id);

        // Same work but under 6 hours left: refreshed.
        let late = NOW + scull_core::CLI_PLAN_TTL_SECS - REUSE_MIN_SECS + 1;
        let fresh = refresh(
            &mut store,
            &data,
            &scanned(&scan, &one),
            false,
            &binding(),
            late,
        )
        .expect("refresh")
        .expect("plan");
        assert!(!fresh.reused);
        assert_ne!(fresh.plan_id, p.plan_id);

        // Different work: a new plan replaces the file.
        let two = vec![candidate("/p/a/target"), candidate("/p/bb/target")];
        let bigger = refresh(
            &mut store,
            &data,
            &scanned(&scan, &two),
            false,
            &binding(),
            late + 60,
        )
        .expect("refresh")
        .expect("plan");
        assert!(!bigger.reused);
        assert_eq!(bigger.items, 2);
        let on_disk: Plan =
            serde_json::from_slice(&std::fs::read(&file).expect("file")).expect("plan");
        assert_eq!(on_disk.id().as_str(), bigger.plan_id);

        // Opting in to S1 is a different proposal, even with the same items.
        let s1 = refresh(
            &mut store,
            &data,
            &scanned(&scan, &two),
            true,
            &binding(),
            late + 120,
        )
        .expect("refresh")
        .expect("plan");
        assert!(!s1.reused && s1.includes_s1);

        // Nothing eligible: no plan, and the announcement is withdrawn.
        let mut protected = candidate("/p/v/target");
        protected.tier = scull_core::Tier::S3;
        protected.action = scull_core::Action::None;
        assert!(
            refresh(
                &mut store,
                &data,
                &scanned(&scan, &[protected]),
                false,
                &binding(),
                late + 180
            )
            .expect("refresh")
            .is_none()
        );
        assert!(current(&store, late + 180).is_none());
        // No temporary file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&data)
            .expect("read_dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn an_expired_plan_is_not_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        let mut store = Store::open(&dir.path().join("scull.db"), &DefaultPermissions).expect("db");
        let scan = ScanId::from_suffix("s1").expect("id");
        refresh(
            &mut store,
            &data,
            &scanned(&scan, &[candidate("/p/a/target")]),
            false,
            &binding(),
            NOW,
        )
        .expect("refresh")
        .expect("plan");
        assert!(current(&store, NOW + scull_core::CLI_PLAN_TTL_SECS - 1).is_some());
        assert!(current(&store, NOW + scull_core::CLI_PLAN_TTL_SECS).is_none());
        let a = new_plan_id(NOW).expect("id");
        let b = new_plan_id(NOW).expect("id");
        assert_ne!(a, b);
    }
}
