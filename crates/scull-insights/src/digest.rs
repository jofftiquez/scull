//! The weekly local digest (INS-04, v1.x): a short summary of the last 7 days, shown as an
//! opt-in OS notification by the tray app and as the "This week" card on the desktop Insights
//! page.
//!
//! Built from the same local history as `scull insights` ([`crate::report`] over a 7-day
//! window), the alerts `scull check` recorded that week and the ready-to-review plan (PRO-05).
//! Read-only and local: nothing leaves the machine, and nothing here acts on what it reports
//! (the review plan is a proposal; running it is a reviewed `scull clean --plan`).
//!
//! When it is sent is decided by `scull_proactive::monitor` (default Monday 09:00 local
//! time, off until the user turns it on).

use crate::time::DAY;
use crate::{Grower, GrowthWindow, InsightsReport, VolumeTrend};
use schemars::JsonSchema;
use scull_proactive::alert::{Alert, Severity, format_bytes, plural};
use scull_proactive::review::{ReviewPlanSummary, latest_review_plan};
use scull_proactive::{Confidence, ForecastState};
use scull_storage::{Store, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Days a digest covers.
pub const DIGEST_DAYS: u32 = 7;
/// Growers named in a digest.
pub const DIGEST_GROWERS: usize = 3;
/// The digest's notification title.
pub const DIGEST_TITLE: &str = "Your week in scull";

/// The weekly digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WeeklyDigest {
    /// When it was computed (Unix seconds).
    pub generated_at: i64,
    /// Start of the week it covers (`generated_at` − 7 days).
    pub since: i64,
    /// Whether there is any recorded history in the week (free-space samples or tracked
    /// items). Without it the digest only says so.
    pub has_history: bool,
    /// Free space per volume: the preferred volume (the home folder's) first, then those below
    /// their reserve, then by days left, then by how much free space shrank this week.
    pub volumes: Vec<DigestVolume>,
    /// Net change of all tracked bytes over the week (negative after cleanups).
    pub net_bytes: i64,
    /// Sum of the growth of everything that grew.
    pub grown_bytes: u64,
    /// The projects (or, without projects, categories) that grew most, at most
    /// [`DIGEST_GROWERS`].
    pub top_growers: Vec<Grower>,
    /// Bytes the ready-to-review plan would reclaim, when one is current.
    pub reclaimable_bytes: Option<u64>,
    /// Items in that plan.
    pub review_items: Option<u64>,
    /// Bytes measured as freed by cleanups this week.
    pub reclaimed_bytes: u64,
    /// Cleanups with measured results this week.
    pub cleanups: u64,
    /// Distinct alerts raised this week, by severity.
    pub alerts: AlertCounts,
    /// The notification title ([`DIGEST_TITLE`]).
    pub title: String,
    /// One plain sentence per fact, most important first (the notification body is the first
    /// three).
    pub lines: Vec<String>,
}

/// One volume in the digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DigestVolume {
    /// Volume identifier (mount point).
    pub volume_id: String,
    /// Bytes free at the latest sample.
    pub free_bytes: u64,
    /// Volume size.
    pub total_bytes: u64,
    /// Change of free space over the week (negative: it shrank).
    pub change_bytes: i64,
    /// Whole days until the reserve, when a forecast with at least medium confidence says
    /// the volume is filling.
    pub days_left: Option<u32>,
    /// The forecast's reserve (bytes).
    pub reserve_bytes: u64,
    /// Whether it is already below the reserve.
    pub below_reserve: bool,
}

/// Alerts by severity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AlertCounts {
    /// Critical.
    pub critical: u32,
    /// Warning.
    pub warning: u32,
    /// Info.
    pub info: u32,
}

impl AlertCounts {
    /// All of them.
    pub fn total(&self) -> u32 {
        self.critical + self.warning + self.info
    }
}

impl WeeklyDigest {
    /// The notification body: the first three lines.
    pub fn body(&self) -> String {
        self.lines
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Count `alerts` once per key, at the most severe level seen.
pub fn count_alerts(alerts: &[Alert]) -> AlertCounts {
    let mut worst: BTreeMap<&str, Severity> = BTreeMap::new();
    for a in alerts {
        let e = worst.entry(a.key.as_str()).or_insert(a.severity);
        if a.severity > *e {
            *e = a.severity;
        }
    }
    let mut c = AlertCounts::default();
    for s in worst.values() {
        match s {
            Severity::Critical => c.critical += 1,
            Severity::Warning => c.warning += 1,
            Severity::Info => c.info += 1,
        }
    }
    c
}

/// The digest of `report` (a [`DIGEST_DAYS`] report ending now), the week's `alerts` and the
/// current review plan. Pure. `prefer_volume` is listed first when it was sampled.
pub fn weekly_digest(
    report: &InsightsReport,
    alerts: &[Alert],
    review: Option<&ReviewPlanSummary>,
    prefer_volume: Option<&str>,
) -> WeeklyDigest {
    let mut volumes: Vec<DigestVolume> =
        report.free_space.iter().filter_map(digest_volume).collect();
    volumes.sort_by(|a, b| {
        let pref = |v: &DigestVolume| Some(v.volume_id.as_str()) != prefer_volume;
        pref(a)
            .cmp(&pref(b))
            .then_with(|| b.below_reserve.cmp(&a.below_reserve))
            .then_with(|| {
                a.days_left
                    .unwrap_or(u32::MAX)
                    .cmp(&b.days_left.unwrap_or(u32::MAX))
            })
            .then_with(|| a.change_bytes.cmp(&b.change_bytes))
            .then_with(|| a.volume_id.cmp(&b.volume_id))
    });
    let week: Option<&GrowthWindow> = report.growth.first();
    let top_growers: Vec<Grower> = week
        .map(|w| {
            let list = if w.projects.is_empty() {
                &w.categories
            } else {
                &w.projects
            };
            list.iter().take(DIGEST_GROWERS).cloned().collect()
        })
        .unwrap_or_default();
    let has_history = !volumes.is_empty() || report.coverage.items > 0;
    let mut d = WeeklyDigest {
        generated_at: report.generated_at,
        since: report.since,
        has_history,
        volumes,
        net_bytes: week.map_or(0, |w| w.net_bytes),
        grown_bytes: week.map_or(0, |w| w.grown_bytes),
        top_growers,
        reclaimable_bytes: review.map(|r| r.reclaimable_bytes),
        review_items: review.map(|r| r.items),
        reclaimed_bytes: report.accuracy.measured_bytes,
        cleanups: report.accuracy.operations.len() as u64,
        alerts: count_alerts(alerts),
        title: DIGEST_TITLE.to_owned(),
        lines: Vec::new(),
    };
    d.lines = lines(&d);
    d
}

fn digest_volume(v: &VolumeTrend) -> Option<DigestVolume> {
    let f = &v.forecast;
    let free = f
        .free_bytes
        .or_else(|| v.points.last().map(|p| p.free_bytes))?;
    let days_left = match f.state {
        ForecastState::Filling if f.confidence >= Confidence::Medium => f.days_rounded(),
        _ => None,
    };
    Some(DigestVolume {
        volume_id: v.volume_id.clone(),
        free_bytes: free,
        total_bytes: f.total_bytes.unwrap_or(v.total_bytes),
        change_bytes: v.change_bytes,
        days_left,
        reserve_bytes: f.reserve_bytes,
        below_reserve: f.state == ForecastState::BelowReserve,
    })
}

/// A grower's name for people: the project folder, or a category in words.
pub fn grower_name(g: &Grower) -> String {
    let words = match g.key.as_str() {
        "project_artifact" => "build outputs and dependencies",
        "package_cache" => "package caches",
        "worktree" => "Git worktrees",
        "ml_cache" => "ML model caches",
        "container" => "containers",
        "large_file" => "large files",
        "scull_data" => "scull's own data",
        "other" => "other files",
        _ => return g.label.clone(),
    };
    words.to_owned()
}

fn signed(bytes: i64) -> String {
    let abs = format_bytes(bytes.unsigned_abs());
    match bytes {
        0 => "no change".to_owned(),
        b if b > 0 => format!("+{abs}"),
        _ => format!("−{abs}"),
    }
}

fn lines(d: &WeeklyDigest) -> Vec<String> {
    if !d.has_history {
        return vec![
            "No history this week yet: scull records free space with each scan and scheduled check."
                .to_owned(),
        ];
    }
    let mut out = Vec::new();
    if let Some(v) = d.volumes.first() {
        let mut s = format!(
            "{} free on {} ({} this week)",
            format_bytes(v.free_bytes),
            v.volume_id,
            signed(v.change_bytes)
        );
        if v.below_reserve {
            s.push_str(&format!(
                ", below the {} reserve",
                format_bytes(v.reserve_bytes)
            ));
        } else if let Some(days) = v.days_left {
            s.push_str(&format!(
                ", about {} until {} free",
                plural(days, "day"),
                format_bytes(v.reserve_bytes)
            ));
        }
        out.push(s);
    }
    if let (Some(bytes), Some(items)) = (d.reclaimable_bytes, d.review_items)
        && bytes > 0
    {
        out.push(format!(
            "{} safely reclaimable ({}), ready to review",
            format_bytes(bytes),
            plural(u32::try_from(items).unwrap_or(u32::MAX), "item")
        ));
    }
    if let Some(g) = d.top_growers.first() {
        out.push(format!(
            "Grew most: {} (+{})",
            grower_name(g),
            format_bytes(g.growth_bytes)
        ));
    }
    if d.cleanups > 0 {
        out.push(format!(
            "Cleaned {} in {}",
            format_bytes(d.reclaimed_bytes),
            plural(u32::try_from(d.cleanups).unwrap_or(u32::MAX), "cleanup")
        ));
    }
    let n = d.alerts.total();
    if n > 0 {
        let mut s = plural(n, "alert");
        s.push_str(" this week");
        if d.alerts.critical > 0 {
            s.push_str(&format!(" ({} critical)", d.alerts.critical));
        }
        out.push(s);
    }
    if out.len() == 1 && d.top_growers.is_empty() && n == 0 {
        out.push("A quiet week: nothing grew and nothing needs attention.".to_owned());
    }
    out
}

/// Read the week ending at `now` from `store` and build its digest. Read-only. Unreadable
/// alert or plan records are skipped (a digest never fails on one bad row).
pub fn read_weekly_digest(
    store: &Store,
    now: i64,
    prefer_volume: Option<&str>,
) -> Result<WeeklyDigest, StoreError> {
    let since = now.saturating_sub(i64::from(DIGEST_DAYS) * DAY);
    let report = crate::report(store, now, since, None)?;
    let alerts: Vec<Alert> = store
        .alerts(since)?
        .into_iter()
        .filter_map(|e| serde_json::from_value(e.alert).ok())
        .collect();
    let review = latest_review_plan(store, now).ok().flatten();
    Ok(weekly_digest(
        &report,
        &alerts,
        review.as_ref(),
        prefer_volume,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_proactive::AlertKind;

    fn empty_report(now: i64) -> InsightsReport {
        crate::compute(&crate::InsightsInput {
            now,
            since: now - 7 * DAY,
            items: Vec::new(),
            regrowth: Vec::new(),
            operations: Vec::new(),
            volumes: Vec::new(),
            footprint: scull_storage::Footprint::default(),
            footprint_history: Vec::new(),
            budget_used_bytes: 0,
            budget_bytes: None,
        })
    }

    #[test]
    fn no_history_says_so() {
        let d = weekly_digest(&empty_report(1_000_000), &[], None, None);
        assert!(!d.has_history);
        assert_eq!(d.title, DIGEST_TITLE);
        assert_eq!(d.lines.len(), 1);
        assert!(d.lines[0].starts_with("No history this week yet"));
        assert_eq!(d.body(), d.lines[0]);
    }

    #[test]
    fn alerts_count_once_per_key_at_their_worst() {
        let a = |k, s, subj: &str| Alert::new(k, s, subj, "m", 0);
        let c = count_alerts(&[
            a(AlertKind::LowSpace, Severity::Warning, "/"),
            a(AlertKind::LowSpace, Severity::Critical, "/"),
            a(AlertKind::Stale, Severity::Info, "x"),
            a(AlertKind::Stale, Severity::Info, "x"),
            a(AlertKind::BudgetExceeded, Severity::Warning, "docker"),
        ]);
        assert_eq!(
            c,
            AlertCounts {
                critical: 1,
                warning: 1,
                info: 1
            }
        );
        assert_eq!(c.total(), 3);
    }

    #[test]
    fn grower_names_are_words() {
        let g = |key: &str, label: &str| Grower {
            key: key.into(),
            label: label.into(),
            ecosystem: None,
            start_bytes: 0,
            end_bytes: 1,
            growth_bytes: 1,
            share_of_growth: 1.0,
        };
        assert_eq!(grower_name(&g("ml_cache", "ml_cache")), "ML model caches");
        assert_eq!(grower_name(&g("/home/dev/work/engine", "engine")), "engine");
    }

    #[test]
    fn signed_sizes() {
        assert_eq!(signed(0), "no change");
        assert_eq!(signed(1536), "+1.5 KiB");
        assert_eq!(signed(-(3 << 30)), "−3.0 GiB");
    }
}
