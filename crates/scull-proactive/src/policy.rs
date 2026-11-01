//! Staleness policies, budgets and spike alerts (PRO-02, PRO-03, PRO-04, PRO-08), configured
//! by `[proactive]` in the user config ([`ProactiveConfig`]).
//!
//! Every function is pure and returns [`Alert`] records; nothing here removes anything. A
//! stale item, a budget overrun or a spike is a reason to *review* a cleanup, never to run
//! one (unattended policies are a separate, later opt-in, D-11).
//!
//! # Staleness (PRO-02, PRO-08)
//!
//! Input: one [`Activity`] per worktree, build output or cache, usually built from a scan's
//! candidates with [`activity_from_candidate`]. "Idle" is measured from the **newest
//! modification time** (source files, Git activity) and **never from access times**, which are
//! often disabled. Missing or bounded evidence never counts as inactivity (PRD §8.4): an item
//! whose last activity is unknown, or whose mtime sampling hit its bound, is never stale, and
//! nothing in use is ever stale. Agent worktrees past their registered TTL raise their own
//! alert (one per worktree). Other stale items are grouped into one alert per kind, naming the
//! largest.
//!
//! # Budgets (PRO-03)
//!
//! A [`Budget`] caps the allocated bytes of everything matching its filters (category,
//! ecosystem, path). Input: [`Usage`] rows, either one per candidate
//! ([`usage_from_candidates`], every filter works) or per category from history
//! ([`crate::history::category_usage`], only category filters can match: history rollups do
//! not keep paths, so path and ecosystem budgets need a scan's candidates).
//!
//! # Spikes (PRO-04)
//!
//! Input: a series of byte totals over time ([`SeriesPoint`]): a category's allocated bytes
//! per scan, or a volume's used bytes per free-space sample. The latest point is compared
//! with the earliest point in the spike window before it (the window is stretched by 25% so
//! daily scans a little more than 24 h apart still compare). Growth of at least `spike_gib`,
//! or of `spike_percent` % and at least `spike_min_gib`, is a spike. Shrinking (a cleanup) is
//! never a spike. Category series must come from scans of the same roots (a broader scan is
//! not growth); see [`crate::history::category_series`].

use crate::alert::{Alert, AlertKind, Severity, format_bytes, plural, sort_alerts};
use crate::forecast::{ForecastPolicy, GIB};
use schemars::JsonSchema;
use scull_core::config::{Budget, ProactiveConfig};
use scull_core::{Candidate, Category, Evidence};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The forecast policy with the user's reserve and alert window.
pub fn forecast_policy(cfg: &ProactiveConfig) -> ForecastPolicy {
    ForecastPolicy {
        reserve_bytes: u64::from(cfg.reserve_gib) * GIB,
        alert_days: cfg.alert_days,
        ..ForecastPolicy::default()
    }
}

/// What kind of item a staleness window applies to.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StaleKind {
    /// A Git worktree (`stale_worktree`).
    Worktree,
    /// Build output or dependencies of a project (`stale_build`).
    BuildOutput,
    /// A package-manager or ML cache (`stale_cache`).
    Cache,
}

impl StaleKind {
    /// `worktree`, `build_output`, `cache`.
    pub fn as_str(self) -> &'static str {
        match self {
            StaleKind::Worktree => "worktree",
            StaleKind::BuildOutput => "build_output",
            StaleKind::Cache => "cache",
        }
    }
}

/// When an item was last active, for the staleness policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    /// The item's path.
    pub path: String,
    /// Which staleness window applies.
    pub kind: StaleKind,
    /// Newest modification of the item or its project's sources / Git activity (Unix
    /// seconds). Never an access time. `None` when unknown: an unknown item is never stale.
    pub last_activity_at: Option<i64>,
    /// When a registered TTL expires (agent worktrees, PRO-08).
    pub ttl_expires_at: Option<i64>,
    /// Allocated bytes.
    pub bytes: u64,
    /// In use right now (open files, a running process, a lock): never stale.
    pub in_use: bool,
}

/// The [`Activity`] of a scan candidate: worktrees, project artifacts and caches; `None` for
/// other categories. Last activity is the newest modification time the scanner observed
/// ([`Evidence::LastModified`]), unless sampling hit its bound (then unknown). In use when
/// any [`Evidence::InUse`] was recorded or the candidate is flagged active.
pub fn activity_from_candidate(c: &Candidate) -> Option<Activity> {
    let kind = match c.category {
        Category::Worktree => StaleKind::Worktree,
        Category::ProjectArtifact => StaleKind::BuildOutput,
        Category::PackageCache | Category::MlCache => StaleKind::Cache,
        _ => return None,
    };
    let mut last = None;
    let mut in_use = c.flags.active == Some(true);
    for e in &c.evidence {
        match e {
            Evidence::LastModified {
                newest_unix,
                sampled: false,
            } => last = Some(last.map_or(*newest_unix, |l: i64| l.max(*newest_unix))),
            Evidence::InUse { .. } => in_use = true,
            _ => {}
        }
    }
    // A bounded sample may have missed newer files: unknown, not old.
    if c.evidence
        .iter()
        .any(|e| matches!(e, Evidence::LastModified { sampled: true, .. }))
    {
        last = None;
    }
    Some(Activity {
        path: c.path.as_str().to_owned(),
        kind,
        last_activity_at: last,
        ttl_expires_at: None,
        bytes: c.sizes.allocated,
        in_use,
    })
}

fn window_secs(cfg: &ProactiveConfig, kind: StaleKind) -> i64 {
    let d = match kind {
        StaleKind::Worktree => cfg.stale_worktree,
        StaleKind::BuildOutput => cfg.stale_build,
        StaleKind::Cache => cfg.stale_cache,
    };
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// `3 days`, `12 hours`, `less than an hour`.
fn duration_text(secs: i64) -> String {
    let secs = u32::try_from(secs.max(0)).unwrap_or(u32::MAX);
    if secs >= 86_400 {
        plural(secs / 86_400, "day")
    } else if secs >= 3_600 {
        plural(secs / 3_600, "hour")
    } else {
        "less than an hour".to_owned()
    }
}

/// Staleness alerts (PRO-02, PRO-08): one [`AlertKind::WorktreeTtlExpired`] warning per
/// worktree past its TTL, and one [`AlertKind::Stale`] info alert per kind that has stale
/// items, with their total bytes and the largest paths.
pub fn stale_alerts(items: &[Activity], cfg: &ProactiveConfig, now: i64) -> Vec<Alert> {
    let mut out = Vec::new();
    let mut groups: std::collections::BTreeMap<StaleKind, Vec<&Activity>> = Default::default();
    for a in items.iter().filter(|a| !a.in_use) {
        if let Some(exp) = a.ttl_expires_at
            && exp <= now
        {
            out.push(
                Alert::new(
                    AlertKind::WorktreeTtlExpired,
                    Severity::Warning,
                    &a.path,
                    format!(
                        "{} is past its registered TTL (expired {} ago); review it, nothing was removed",
                        a.path,
                        duration_text(now - exp)
                    ),
                    now,
                )
                .with_bytes(a.bytes)
                .with_paths(vec![a.path.clone()]),
            );
            continue;
        }
        let Some(last) = a.last_activity_at else {
            continue;
        };
        if now.saturating_sub(last) > window_secs(cfg, a.kind) {
            groups.entry(a.kind).or_default().push(a);
        }
    }
    for (kind, mut stale) in groups {
        stale.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
        let total = stale.iter().fold(0u64, |s, a| s.saturating_add(a.bytes));
        let n = u32::try_from(stale.len()).unwrap_or(u32::MAX);
        let window = duration_text(window_secs(cfg, kind));
        let what = match kind {
            StaleKind::Worktree => format!(
                "{} idle for more than {window}",
                if n == 1 {
                    "1 worktree".to_owned()
                } else {
                    format!("{n} worktrees")
                }
            ),
            StaleKind::BuildOutput => format!(
                "{} of projects idle for more than {window}",
                if n == 1 {
                    "1 build output".to_owned()
                } else {
                    format!("{n} build outputs")
                }
            ),
            StaleKind::Cache => format!(
                "{} untouched for more than {window}",
                if n == 1 {
                    "1 cache".to_owned()
                } else {
                    format!("{n} caches")
                }
            ),
        };
        let verb = if n == 1 { "holds" } else { "hold" };
        out.push(
            Alert::new(
                AlertKind::Stale,
                Severity::Info,
                kind.as_str(),
                format!("{what} {verb} {}", format_bytes(total)),
                now,
            )
            .with_bytes(total)
            .with_paths(stale.iter().map(|a| a.path.clone()).collect()),
        );
    }
    sort_alerts(&mut out);
    out
}

/// Bytes attributed to a category / ecosystem / path, for budgets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// The category, when known.
    pub category: Option<Category>,
    /// The project ecosystem, when known.
    pub ecosystem: Option<String>,
    /// The item's path, when known (per-candidate rows).
    pub path: Option<String>,
    /// Allocated bytes.
    pub bytes: u64,
}

/// One [`Usage`] row per candidate (allocated bytes; every budget filter applies).
pub fn usage_from_candidates(candidates: &[Candidate]) -> Vec<Usage> {
    candidates
        .iter()
        .map(|c| Usage {
            category: Some(c.category),
            ecosystem: c.project.as_ref().map(|p| p.ecosystem.clone()),
            path: Some(c.path.as_str().to_owned()),
            bytes: c.sizes.allocated,
        })
        .collect()
}

fn matches(b: &Budget, u: &Usage) -> bool {
    let cat = b.category.is_none_or(|c| u.category == Some(c));
    let eco = b.ecosystem.as_ref().is_none_or(|e| {
        u.ecosystem
            .as_ref()
            .is_some_and(|ue| ue.eq_ignore_ascii_case(e))
    });
    let path = b.path.as_ref().is_none_or(|bp| {
        u.path
            .as_ref()
            .is_some_and(|up| Path::new(up).starts_with(bp))
    });
    cat && eco && path
}

/// A readable name for a budget: `project_artifact · rust`, `~/Projects/big`, `everything`.
pub fn budget_label(b: &Budget) -> String {
    let mut parts = Vec::new();
    if let Some(c) = b.category {
        parts.push(category_name(c));
    }
    if let Some(e) = &b.ecosystem {
        parts.push(e.clone());
    }
    if let Some(p) = &b.path {
        parts.push(p.to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        "everything".to_owned()
    } else {
        parts.join(" \u{b7} ")
    }
}

/// The serde name of a category (`project_artifact`, ...).
pub fn category_name(c: Category) -> String {
    match serde_json::to_value(c) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{c:?}"),
    }
}

/// Budget alerts (PRO-03): one [`AlertKind::BudgetExceeded`] warning per budget whose
/// matching usage is over its cap, naming the largest matching paths.
pub fn budget_alerts(budgets: &[Budget], usage: &[Usage], now: i64) -> Vec<Alert> {
    let mut out = Vec::new();
    for b in budgets {
        let mut rows: Vec<&Usage> = usage.iter().filter(|u| matches(b, u)).collect();
        let total = rows.iter().fold(0u64, |s, u| s.saturating_add(u.bytes));
        let cap = u64::from(b.max_gib) * GIB;
        if total <= cap {
            continue;
        }
        rows.sort_by_key(|u| std::cmp::Reverse(u.bytes));
        let label = budget_label(b);
        out.push(
            Alert::new(
                AlertKind::BudgetExceeded,
                Severity::Warning,
                &label,
                format!(
                    "{label} uses {}, over its {} budget",
                    format_bytes(total),
                    format_bytes(cap)
                ),
                now,
            )
            .with_bytes(total)
            .with_paths(rows.iter().filter_map(|u| u.path.clone()).collect()),
        );
    }
    sort_alerts(&mut out);
    out
}

/// A byte total at a moment (a category's allocated bytes at a scan, a volume's used bytes
/// at a sample).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SeriesPoint {
    /// When (Unix seconds).
    pub at: i64,
    /// Bytes.
    pub bytes: u64,
}

/// Growth of a series within the spike window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Growth {
    /// Bytes gained.
    pub bytes: u64,
    /// Bytes at the start of the comparison.
    pub from: u64,
    /// Seconds between the two points compared.
    pub over_secs: i64,
}

/// The spike, if any, in `points` (any order) as of `now`: see the module docs.
pub fn spike(points: &[SeriesPoint], cfg: &ProactiveConfig, now: i64) -> Option<Growth> {
    let mut pts: Vec<SeriesPoint> = points.iter().copied().filter(|p| p.at <= now).collect();
    pts.sort_by_key(|p| p.at);
    let latest = *pts.last()?;
    let window = i64::try_from(cfg.spike_window.as_secs()).unwrap_or(i64::MAX);
    let stretched = window.saturating_add(window / 4);
    let base = pts
        .iter()
        .find(|p| p.at < latest.at && p.at >= latest.at.saturating_sub(stretched))?;
    let growth = latest.bytes.checked_sub(base.bytes)?;
    let absolute = growth >= u64::from(cfg.spike_gib) * GIB && cfg.spike_gib > 0;
    let relative = cfg.spike_percent > 0
        && growth >= u64::from(cfg.spike_min_gib) * GIB
        && u128::from(growth) * 100 >= u128::from(cfg.spike_percent) * u128::from(base.bytes);
    (growth > 0 && (absolute || relative)).then_some(Growth {
        bytes: growth,
        from: base.bytes,
        over_secs: latest.at - base.at,
    })
}

/// A category's spike as an [`AlertKind::GrowthSpike`] warning; `paths` names the folders
/// responsible when the caller knows them (e.g. the largest items of the category).
pub fn category_spike_alert(
    category: Category,
    points: &[SeriesPoint],
    paths: Vec<String>,
    cfg: &ProactiveConfig,
    now: i64,
) -> Option<Alert> {
    let g = spike(points, cfg, now)?;
    let name = category_name(category);
    Some(
        Alert::new(
            AlertKind::GrowthSpike,
            Severity::Warning,
            &name,
            format!(
                "{name} grew by {} in {} (from {})",
                format_bytes(g.bytes),
                duration_text(g.over_secs),
                format_bytes(g.from)
            ),
            now,
        )
        .with_bytes(g.bytes)
        .with_paths(paths),
    )
}

/// A volume's free-space drop as an [`AlertKind::FreeSpaceDrop`] warning. `samples` are the
/// volume's free-space samples; growth is measured on used bytes (`total − free`).
pub fn free_space_drop_alert(
    volume_id: &str,
    samples: &[scull_storage::VolumeSample],
    cfg: &ProactiveConfig,
    now: i64,
) -> Option<Alert> {
    let used: Vec<SeriesPoint> = samples
        .iter()
        .filter(|s| s.volume_id == volume_id && s.free <= s.total)
        .map(|s| SeriesPoint {
            at: s.at,
            bytes: s.total - s.free,
        })
        .collect();
    let g = spike(&used, cfg, now)?;
    Some(
        Alert::new(
            AlertKind::FreeSpaceDrop,
            Severity::Warning,
            volume_id,
            format!(
                "{volume_id} lost {} of free space in {}",
                format_bytes(g.bytes),
                duration_text(g.over_secs)
            ),
            now,
        )
        .with_bytes(g.bytes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::FsPath;
    use scull_core::config::DurationSpec;
    use scull_storage::VolumeSample;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn cfg() -> ProactiveConfig {
        ProactiveConfig::default()
    }

    fn act(path: &str, kind: StaleKind, idle_days: Option<i64>, gib: u64) -> Activity {
        Activity {
            path: path.into(),
            kind,
            last_activity_at: idle_days.map(|d| NOW - d * DAY),
            ttl_expires_at: None,
            bytes: gib * GIB,
            in_use: false,
        }
    }

    fn candidate() -> Candidate {
        serde_json::from_str(include_str!("../../../schemas/v1/examples/candidate.json"))
            .expect("example candidate")
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration_text(90), "less than an hour");
        assert_eq!(duration_text(3 * 3_600), "3 hours");
        assert_eq!(duration_text(DAY), "1 day");
    }

    #[test]
    fn stale_items_alert_per_kind_by_window() {
        let items = vec![
            act("/p/a/target", StaleKind::BuildOutput, Some(45), 4),
            act("/p/b/target", StaleKind::BuildOutput, Some(31), 9),
            act("/p/c/target", StaleKind::BuildOutput, Some(29), 50), // within 30 days
            act("/wt/x", StaleKind::Worktree, Some(15), 2),
            act("/wt/y", StaleKind::Worktree, Some(13), 2), // within 14 days
            act("/cache/npm", StaleKind::Cache, Some(89), 7), // within 90 days
            act("/p/d/target", StaleKind::BuildOutput, None, 80), // unknown: never stale
        ];
        let a = stale_alerts(&items, &cfg(), NOW);
        assert_eq!(a.len(), 2, "{a:#?}");
        let build = a
            .iter()
            .find(|x| x.subject == "build_output")
            .expect("build");
        assert_eq!(build.kind, AlertKind::Stale);
        assert_eq!(build.severity, Severity::Info);
        assert_eq!(build.key, "stale:build_output");
        assert_eq!(build.bytes, Some(13 * GIB));
        assert_eq!(
            build.paths,
            vec!["/p/b/target", "/p/a/target"],
            "largest first"
        );
        assert_eq!(
            build.message,
            "2 build outputs of projects idle for more than 30 days hold 13.0 GiB"
        );
        let wt = a
            .iter()
            .find(|x| x.subject == "worktree")
            .expect("worktree");
        assert_eq!(
            wt.message,
            "1 worktree idle for more than 14 days holds 2.0 GiB"
        );
    }

    #[test]
    fn in_use_is_never_stale_and_windows_are_configurable() {
        let mut busy = act("/p/a/target", StaleKind::BuildOutput, Some(400), 4);
        busy.in_use = true;
        assert!(stale_alerts(&[busy], &cfg(), NOW).is_empty());
        let mut c = cfg();
        c.stale_cache = DurationSpec::from_hours(7 * 24);
        let a = stale_alerts(&[act("/cache/npm", StaleKind::Cache, Some(8), 7)], &c, NOW);
        assert_eq!(a.len(), 1);
        assert_eq!(
            a[0].message,
            "1 cache untouched for more than 7 days holds 7.0 GiB"
        );
    }

    #[test]
    fn expired_ttl_raises_one_warning_per_worktree() {
        let mut w = act("/wt/agent-1", StaleKind::Worktree, Some(1), 3);
        w.ttl_expires_at = Some(NOW - 2 * DAY);
        let mut fresh = act("/wt/agent-2", StaleKind::Worktree, Some(1), 3);
        fresh.ttl_expires_at = Some(NOW + DAY);
        let a = stale_alerts(&[w, fresh], &cfg(), NOW);
        assert_eq!(a.len(), 1, "{a:#?}");
        assert_eq!(a[0].kind, AlertKind::WorktreeTtlExpired);
        assert_eq!(a[0].severity, Severity::Warning);
        assert_eq!(a[0].key, "worktree_ttl_expired:/wt/agent-1");
        assert!(
            a[0].message.contains("expired 2 days ago"),
            "{}",
            a[0].message
        );
        assert!(a[0].message.contains("nothing was removed"));
    }

    #[test]
    fn activity_from_candidates_uses_mtime_never_guesses() {
        let c = candidate();
        let a = activity_from_candidate(&c).expect("project artifact");
        assert_eq!(a.kind, StaleKind::BuildOutput);
        assert_eq!(a.last_activity_at, Some(1_761_600_000));
        assert_eq!(a.bytes, c.sizes.allocated);
        assert!(!a.in_use);

        let mut sampled = c.clone();
        for e in &mut sampled.evidence {
            if let Evidence::LastModified { sampled, .. } = e {
                *sampled = true;
            }
        }
        assert_eq!(
            activity_from_candidate(&sampled)
                .expect("a")
                .last_activity_at,
            None
        );

        let mut busy = c.clone();
        busy.evidence.push(Evidence::InUse {
            signal: "open handle".into(),
            by: None,
        });
        assert!(activity_from_candidate(&busy).expect("a").in_use);

        let mut other = c.clone();
        other.category = Category::LargeFile;
        assert!(activity_from_candidate(&other).is_none());
        let mut wt = c;
        wt.category = Category::Worktree;
        assert_eq!(
            activity_from_candidate(&wt).expect("a").kind,
            StaleKind::Worktree
        );
    }

    #[test]
    fn budgets_match_category_ecosystem_and_path() {
        let mut rust = candidate();
        rust.sizes.allocated = 30 * GIB;
        let mut rust2 = candidate();
        rust2.path = FsPath::from("/Users/dev/Projects/web/target");
        rust2.sizes.allocated = 25 * GIB;
        let mut node = candidate();
        node.path = FsPath::from("/Users/dev/Projects/web/node_modules");
        if let Some(p) = node.project.as_mut() {
            p.ecosystem = "node".into();
        }
        node.sizes.allocated = 8 * GIB;
        let usage = usage_from_candidates(&[rust, rust2, node]);
        let budgets = vec![
            Budget {
                category: Some(Category::ProjectArtifact),
                ecosystem: Some("rust".into()),
                path: None,
                max_gib: 50,
            },
            Budget {
                category: None,
                ecosystem: None,
                path: Some("/Users/dev/Projects/web".into()),
                max_gib: 40,
            },
            Budget {
                category: Some(Category::PackageCache),
                ecosystem: None,
                path: None,
                max_gib: 1,
            },
        ];
        let a = budget_alerts(&budgets, &usage, NOW);
        assert_eq!(a.len(), 1, "{a:#?}");
        assert_eq!(a[0].kind, AlertKind::BudgetExceeded);
        assert_eq!(a[0].subject, "project_artifact \u{b7} rust");
        assert_eq!(a[0].bytes, Some(55 * GIB));
        assert_eq!(
            a[0].message,
            "project_artifact \u{b7} rust uses 55.0 GiB, over its 50.0 GiB budget"
        );
        assert_eq!(a[0].paths[0], "/Users/dev/Projects/api/target");
        // Path budget: web holds 33 GiB, under 40; lower the cap and it fires.
        let mut tight = budgets[1].clone();
        tight.max_gib = 30;
        let a = budget_alerts(&[tight], &usage, NOW);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].bytes, Some(33 * GIB));
        assert_eq!(a[0].paths.len(), 2);
        // A category-only usage row (history) never matches an ecosystem budget.
        let hist = vec![Usage {
            category: Some(Category::ProjectArtifact),
            ecosystem: None,
            path: None,
            bytes: 900 * GIB,
        }];
        assert!(budget_alerts(&budgets[..1], &hist, NOW).is_empty());
        assert_eq!(
            budget_label(&Budget {
                category: None,
                ecosystem: None,
                path: None,
                max_gib: 1
            }),
            "everything"
        );
    }

    fn series(points: &[(i64, u64)]) -> Vec<SeriesPoint> {
        points
            .iter()
            .map(|&(hours_ago, gib)| SeriesPoint {
                at: NOW - hours_ago * 3_600,
                bytes: gib * GIB,
            })
            .collect()
    }

    #[test]
    fn spikes_absolute_relative_and_not_cleanups() {
        let c = cfg();
        // +22 GiB in 24 h: absolute spike.
        let g = spike(&series(&[(72, 100), (24, 103), (0, 125)]), &c, NOW).expect("spike");
        assert_eq!(g.bytes, 22 * GIB);
        assert_eq!(g.over_secs, 24 * 3_600);
        // +6 GiB on 10 GiB (60%): relative spike.
        assert!(spike(&series(&[(20, 10), (0, 16)]), &c, NOW).is_some());
        // +4 GiB on 4 GiB: 100% but under spike_min_gib.
        assert!(spike(&series(&[(20, 4), (0, 8)]), &c, NOW).is_none());
        // +10 GiB on 100 GiB in a day: neither.
        assert!(spike(&series(&[(20, 100), (0, 110)]), &c, NOW).is_none());
        // A cleanup (shrinking) is never a spike.
        assert!(spike(&series(&[(20, 100), (0, 20)]), &c, NOW).is_none());
        // Growth spread over more than the (stretched) window is not a spike.
        assert!(spike(&series(&[(31, 100), (0, 140)]), &c, NOW).is_none());
        // 29 h apart still compares (window stretched by 25%).
        assert!(spike(&series(&[(29, 100), (0, 140)]), &c, NOW).is_some());
        // One point: nothing to compare.
        assert!(spike(&series(&[(0, 900)]), &c, NOW).is_none());
        // Turned off.
        let mut off = c.clone();
        off.spike_gib = 0;
        off.spike_percent = 0;
        assert!(spike(&series(&[(20, 1), (0, 900)]), &off, NOW).is_none());
    }

    #[test]
    fn spike_alerts_name_category_volume_and_paths() {
        let c = cfg();
        let a = category_spike_alert(
            Category::Container,
            &series(&[(23, 40), (0, 64)]),
            vec!["/var/lib/docker".into()],
            &c,
            NOW,
        )
        .expect("alert");
        assert_eq!(a.kind, AlertKind::GrowthSpike);
        assert_eq!(a.key, "growth_spike:container");
        assert_eq!(
            a.message,
            "container grew by 24.0 GiB in 23 hours (from 40.0 GiB)"
        );
        assert_eq!(a.paths, vec!["/var/lib/docker"]);

        let samples: Vec<VolumeSample> = [(30, 200u64), (12, 190), (0, 165)]
            .iter()
            .map(|&(h, free)| VolumeSample {
                volume_id: "/".into(),
                total: 500 * GIB,
                free: free * GIB,
                at: NOW - h * 3_600,
            })
            .collect();
        let a = free_space_drop_alert("/", &samples, &c, NOW).expect("drop");
        assert_eq!(a.kind, AlertKind::FreeSpaceDrop);
        assert_eq!(a.message, "/ lost 35.0 GiB of free space in 1 day");
        assert_eq!(a.bytes, Some(35 * GIB));
        assert!(free_space_drop_alert("/data", &samples, &c, NOW).is_none());
    }

    #[test]
    fn forecast_policy_follows_config() {
        let mut c = cfg();
        c.reserve_gib = 25;
        c.alert_days = 7;
        let p = forecast_policy(&c);
        assert_eq!(p.reserve_bytes, 25 * GIB);
        assert_eq!(p.alert_days, 7);
        assert_eq!(p.window_days, ForecastPolicy::default().window_days);
        assert_eq!(category_name(Category::MlCache), "ml_cache");
    }
}
