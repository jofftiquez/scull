//! Local insights computed from scull's own history (PRD §15, INS-02…05; `scull insights`).
//!
//! Everything here is **read-only analytics** over [`scull_storage`] reads: [`read`] gathers an
//! [`InsightsInput`] from a [`Store`], and [`compute`] (a pure function) turns it into an
//! [`InsightsReport`]. [`report`] does both. [`csv::to_csv`] flattens a report for
//! `scull insights export --format csv`. No network, ever; nothing leaves the machine.
//!
//! What a report holds (INS-03):
//!
//! * **Growth** ([`GrowthWindow`]): categories and projects that grew most over the last 7 days
//!   and over the report window. A project's bytes are the allocated bytes of its candidates
//!   (build outputs, dependency folders, worktrees), sized at the start and end of the window
//!   from item history ([`ItemTimeline::allocated_at`]), so scans of different roots stay
//!   comparable.
//! * **Regrowth** ([`RegrowthInsight`]): artifacts deleted and rebuilt in the window ("came
//!   back 4 times this month"), with how quickly they came back. Rebuilt three times or more:
//!   cleaning them only costs install time, so excluding them is suggested.
//! * **Duplication** ([`DuplicationInsight`]): the same kind of artifact (e.g. installed npm
//!   dependencies) present in [`DUPLICATION_MIN_PROJECTS`] or more projects, with a
//!   store-once suggestion (pnpm, a shared Cargo target dir, uv). Dependency versions are not
//!   recorded (INS-07 keeps history to paths, kinds and sizes), so this counts copies of an
//!   artifact kind, not identical packages.
//! * **Agent footprint** ([`AgentFootprint`]): worktrees created per week over the last
//!   [`AGENT_WEEKS`] weeks, average lifetime and bytes held. A worktree counts as an agent's
//!   when its path or branch carries an agent marker ([`agent_of`]); TTLs are not recorded yet.
//! * **Cleanup effectiveness** ([`Accuracy`]): predicted vs. measured bytes per operation and
//!   the median error; items rebuilt after a cleanup.
//! * **Ecosystem mix** ([`EcosystemMonth`]): bytes per ecosystem at the end of each of the
//!   last [`MIX_MONTHS`] months.
//! * **Free space** ([`VolumeTrend`]) per volume (one point per day) with its disk-full
//!   forecast (PRO-01, computed by [`scull_proactive::forecast()`], the same method as
//!   `scull check`), and **scull's own footprint** against the storage budget
//!   ([`OwnFootprint`], DATA-05, INS-02).

pub mod csv;
#[cfg(feature = "fixtures")]
pub mod fixtures;
pub mod time;

use schemars::JsonSchema;
use scull_core::{Category, FsPath};
use scull_proactive::{Forecast, ForecastPolicy};
use scull_storage::{
    Footprint, FootprintSample, ItemTimeline, OperationAccuracy, RegrowthEvent, Store, StoreError,
    VolumeSample,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use time::{DAY, add_months, day_start, iso_month, month_start, week_start};

/// Crate name, used in diagnostics.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Default report window, in days (the desktop Insights page opens on 30 days).
pub const DEFAULT_WINDOW_DAYS: u32 = 30;
/// The short growth window, in days (INS-03: "grew most in 7 and 30 days").
pub const SHORT_WINDOW_DAYS: u32 = 7;
/// Growers listed per window and dimension.
pub const TOP_GROWERS: usize = 10;
/// Rebuilds in the window from which excluding an artifact is suggested.
pub const REGROWTH_SUGGEST_MIN: u32 = 3;
/// Projects that must hold the same artifact kind for it to count as duplication.
pub const DUPLICATION_MIN_PROJECTS: u32 = 3;
/// Weeks of agent worktree history in a report.
pub const AGENT_WEEKS: u32 = 8;
/// Months of ecosystem mix in a report.
pub const MIX_MONTHS: u32 = 6;

/// Every category, for queries that take one category at a time.
const CATEGORIES: [Category; 8] = [
    Category::ProjectArtifact,
    Category::PackageCache,
    Category::Worktree,
    Category::MlCache,
    Category::Container,
    Category::LargeFile,
    Category::ScullData,
    Category::Other,
];

// ---------------------------------------------------------------------------------------------
// Report (JSON: `scull/insights@1`)
// ---------------------------------------------------------------------------------------------

/// Local insights over a window ending now (`scull insights --json`, `scull/insights@1`).
///
/// Byte counts are allocated bytes unless a field says otherwise. Times are Unix seconds;
/// day, week and month buckets are UTC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InsightsReport {
    /// When the report was computed.
    pub generated_at: i64,
    /// Start of the report window.
    pub since: i64,
    /// Length of the window in whole days (rounded up).
    pub window_days: u32,
    /// How much history the report rests on.
    pub coverage: Coverage,
    /// Top growers over the last [`SHORT_WINDOW_DAYS`] days and over the whole window
    /// (shortest first; one entry when they coincide).
    pub growth: Vec<GrowthWindow>,
    /// Artifacts that came back after being removed in the window, most rebuilds first.
    pub regrowth: Vec<RegrowthInsight>,
    /// Artifact kinds present in many projects now, largest first.
    pub duplication: Vec<DuplicationInsight>,
    /// Worktrees, and those created by AI agents.
    pub agent_footprint: AgentFootprint,
    /// Predicted vs. measured bytes of cleanups in the window.
    pub accuracy: Accuracy,
    /// Bytes per ecosystem at the end of each of the last [`MIX_MONTHS`] months, oldest first.
    pub ecosystem_mix: Vec<EcosystemMonth>,
    /// Free space per volume over the window.
    pub free_space: Vec<VolumeTrend>,
    /// scull's own disk use against its storage budget.
    pub own_footprint: OwnFootprint,
    /// Plain-language caveats (what is not included and why).
    pub notes: Vec<String>,
}

/// How much history a report rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    /// Candidates tracked at some point in the window.
    pub items: u64,
    /// Oldest recorded item event or free-space sample considered, if any.
    pub history_starts_at: Option<i64>,
    /// Whether recorded history reaches back to the start of the window. When it does not
    /// (scull is new here, or history was purged), growth compares against the oldest data.
    pub covers_window: bool,
}

/// Top growers over one window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrowthWindow {
    /// Window length in days.
    pub days: u32,
    /// Window start.
    pub start: i64,
    /// Net change of all tracked bytes over the window (negative after cleanups).
    pub net_bytes: i64,
    /// Sum of the growth of everything that grew.
    pub grown_bytes: u64,
    /// Categories that grew, most first (at most [`TOP_GROWERS`]).
    pub categories: Vec<Grower>,
    /// Projects that grew, most first (at most [`TOP_GROWERS`]).
    pub projects: Vec<Grower>,
}

/// One category or project that grew.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Grower {
    /// Category (snake_case, e.g. `project_artifact`) or project root path.
    pub key: String,
    /// Display name: the category key, or the project folder name.
    pub label: String,
    /// The project's ecosystem, when every one of its items agrees.
    pub ecosystem: Option<String>,
    /// Bytes at the window start.
    pub start_bytes: u64,
    /// Bytes now.
    pub end_bytes: u64,
    /// `end_bytes - start_bytes` (always positive here).
    pub growth_bytes: u64,
    /// Share of [`GrowthWindow::grown_bytes`] (0..=1).
    pub share_of_growth: f64,
}

/// An artifact that came back after being removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegrowthInsight {
    /// Where it lives.
    pub path: FsPath,
    /// Its category.
    pub category: Category,
    /// Artifact type, e.g. "Installed npm dependencies".
    pub artifact: String,
    /// Owning project root, when known.
    pub project_root: Option<FsPath>,
    /// Owning project folder name, when known.
    pub project: Option<String>,
    /// Times it was rebuilt (a new object at the same path) in the window.
    pub rebuilds: u32,
    /// Times the same object came back (e.g. restored from quarantine) in the window.
    pub restores: u32,
    /// Median upper bound on how fast it came back: from its last sighting before removal to
    /// its first sighting after (seconds).
    pub median_back_within_secs: Option<i64>,
    /// Last time it came back.
    pub last_back_at: i64,
    /// Whether excluding it from cleanup suggestions is advised (rebuilt at least
    /// [`REGROWTH_SUGGEST_MIN`] times).
    pub suggest_exclude: bool,
    /// Plain-language suggestion, if any.
    pub suggestion: Option<String>,
}

/// One artifact kind present in many projects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DuplicationInsight {
    /// Ecosystem, e.g. "node".
    pub ecosystem: String,
    /// Artifact type, e.g. "Installed npm dependencies".
    pub artifact: String,
    /// Projects holding one.
    pub projects: u32,
    /// Copies (live items).
    pub copies: u32,
    /// Bytes of all copies.
    pub total_bytes: u64,
    /// Bytes of the largest copy.
    pub largest_bytes: u64,
    /// Store-once suggestion, if one fits.
    pub suggestion: Option<String>,
}

/// Worktrees and the share created by AI agents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentFootprint {
    /// Worktrees present now.
    pub worktrees: u32,
    /// Bytes they hold.
    pub worktree_bytes: u64,
    /// Agent worktrees present now.
    pub agent_worktrees: u32,
    /// Bytes held by agent worktrees present now.
    pub agent_bytes: u64,
    /// Agent worktrees present now, per agent, most first.
    pub by_agent: Vec<AgentCount>,
    /// Worktrees first seen per week, the last [`AGENT_WEEKS`] weeks, oldest first.
    pub weeks: Vec<WeekCount>,
    /// Average lifetime of agent worktrees that went away in those weeks (seconds).
    pub average_agent_lifetime_secs: Option<i64>,
}

/// Agent worktrees present now for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCount {
    /// Agent, e.g. `claude-code`, `codex`.
    pub agent: String,
    /// Worktrees.
    pub worktrees: u32,
    /// Bytes.
    pub bytes: u64,
}

/// Worktrees first seen in one week.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WeekCount {
    /// Monday 00:00 UTC.
    pub week_start: i64,
    /// All worktrees first seen that week.
    pub created: u32,
    /// Agent worktrees first seen that week.
    pub agent_created: u32,
}

/// Predicted vs. measured bytes of cleanups.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Accuracy {
    /// Operations in the window with measured results, oldest first.
    pub operations: Vec<OperationPoint>,
    /// Median of `|error|` over those operations (0..), if any.
    pub median_abs_error: Option<f64>,
    /// Bytes predicted, all operations.
    pub predicted_bytes: u64,
    /// Bytes measured, all operations.
    pub measured_bytes: u64,
    /// Items rebuilt after a cleanup (the rebuild cost paid afterwards).
    pub regrown_items: u64,
}

/// One operation's predicted vs. measured bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationPoint {
    /// Operation ID (as in `scull history`).
    pub operation_id: String,
    /// When it ran.
    pub at: i64,
    /// Items compared.
    pub items: u64,
    /// Bytes predicted.
    pub predicted_bytes: u64,
    /// Bytes measured.
    pub measured_bytes: u64,
    /// `(measured - predicted) / predicted`; negative when less was freed. `None` when
    /// nothing was predicted.
    pub error: Option<f64>,
    /// Items of this operation rebuilt afterwards.
    pub regrown_items: u64,
}

/// Bytes per ecosystem at the end of one month.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EcosystemMonth {
    /// `YYYY-MM`.
    pub month: String,
    /// The moment sampled: the month's end, or now for the current month.
    pub at: i64,
    /// All tracked bytes then.
    pub total_bytes: u64,
    /// Per ecosystem, largest first. Items outside any project count as `other`.
    pub ecosystems: Vec<EcosystemShare>,
}

/// One ecosystem's bytes in a month.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EcosystemShare {
    /// Ecosystem, e.g. "node", "rust", or `other`.
    pub ecosystem: String,
    /// Bytes.
    pub bytes: u64,
    /// Share of the month's total (0..=1).
    pub share: f64,
}

/// Free space of one volume over the window, with its forecast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeTrend {
    /// Volume identifier (mount point or volume GUID path).
    pub volume_id: String,
    /// Volume size at the latest sample.
    pub total_bytes: u64,
    /// The last sample of each UTC day, oldest first.
    pub points: Vec<FreePoint>,
    /// Change of free space over the window (latest minus first point).
    pub change_bytes: i64,
    /// Disk-full forecast from the last 30 days of samples (PRO-01; the same as `scull
    /// check`), independent of the report window.
    pub forecast: Forecast,
}

/// A free-space sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FreePoint {
    /// When it was measured.
    pub at: i64,
    /// Bytes free.
    pub free_bytes: u64,
}

/// scull's own disk use (DATA-05) against its storage budget (INS-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnFootprint {
    /// Database file.
    pub database_bytes: u64,
    /// `-wal` and `-shm` files.
    pub wal_bytes: u64,
    /// Migration backups.
    pub backup_bytes: u64,
    /// Damaged copies set aside.
    pub damaged_bytes: u64,
    /// All of the above.
    pub total_bytes: u64,
    /// Bytes counted against the budget (database pages in use, backups, damaged copies).
    pub budget_used_bytes: u64,
    /// The budget, if one applies.
    pub budget_bytes: Option<u64>,
    /// Recorded footprint totals in the window, last of each UTC day, oldest first.
    pub points: Vec<FootprintPoint>,
}

/// A recorded footprint total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FootprintPoint {
    /// When it was measured.
    pub at: i64,
    /// Total bytes.
    pub total_bytes: u64,
}

// ---------------------------------------------------------------------------------------------
// Input and reading
// ---------------------------------------------------------------------------------------------

/// Everything [`compute`] needs, as read from the store by [`read`].
#[derive(Debug, Clone, PartialEq)]
pub struct InsightsInput {
    /// Report time.
    pub now: i64,
    /// Window start.
    pub since: i64,
    /// Items present at some point since [`lookback`]`(now, since)`.
    pub items: Vec<ItemTimeline>,
    /// Regrowth events in the window, with their category.
    pub regrowth: Vec<(Category, RegrowthEvent)>,
    /// Operations in the window with measured results.
    pub operations: Vec<OperationAccuracy>,
    /// Free-space samples per volume, from the window start or the forecast window start,
    /// whichever is earlier.
    pub volumes: Vec<Vec<VolumeSample>>,
    /// scull's own footprint now.
    pub footprint: Footprint,
    /// Footprint samples in the window.
    pub footprint_history: Vec<FootprintSample>,
    /// Bytes counted against the budget now.
    pub budget_used_bytes: u64,
    /// The storage budget, if any.
    pub budget_bytes: Option<u64>,
}

/// How far back item history is read: the window, the agent weeks and the ecosystem months,
/// whichever reaches furthest.
pub fn lookback(now: i64, since: i64) -> i64 {
    let weeks = week_start(now) - i64::from(AGENT_WEEKS - 1) * 7 * DAY;
    let months = add_months(now, -i64::from(MIX_MONTHS - 1));
    since.min(weeks).min(months)
}

/// Read the history a report over `[since, now]` needs. Read-only.
pub fn read(
    store: &Store,
    now: i64,
    since: i64,
    budget_bytes: Option<u64>,
) -> Result<InsightsInput, StoreError> {
    let mut regrowth = Vec::new();
    for c in CATEGORIES {
        regrowth.extend(store.regrowth(c, since)?.into_iter().map(|e| (c, e)));
    }
    let forecast_since = now.saturating_sub(i64::from(ForecastPolicy::default().window_days) * DAY);
    let volumes = store
        .volume_ids()?
        .iter()
        .map(|v| store.free_space_history(v, since.min(forecast_since)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(InsightsInput {
        now,
        since,
        items: store.item_timelines(lookback(now, since))?,
        regrowth,
        operations: store.operation_accuracy(since)?,
        volumes,
        footprint: store.footprint(),
        footprint_history: store.footprint_history(since)?,
        budget_used_bytes: store.budget_bytes_used()?,
        budget_bytes,
    })
}

/// [`read`] then [`compute`].
pub fn report(
    store: &Store,
    now: i64,
    since: i64,
    budget_bytes: Option<u64>,
) -> Result<InsightsReport, StoreError> {
    Ok(compute(&read(store, now, since, budget_bytes)?))
}

// ---------------------------------------------------------------------------------------------
// Computation (pure)
// ---------------------------------------------------------------------------------------------

/// Compute a report from `input`. Pure: same input, same report.
pub fn compute(input: &InsightsInput) -> InsightsReport {
    let now = input.now;
    let since = input.since.min(now);
    let window_days = u32::try_from((now - since + DAY - 1).div_euclid(DAY).max(0)).unwrap_or(0);
    let mut windows: Vec<u32> = vec![SHORT_WINDOW_DAYS.min(window_days), window_days];
    windows.dedup();
    let growth = windows
        .into_iter()
        .map(|d| growth_window(&input.items, now, d))
        .collect();

    let mut notes = vec![
        "Duplication counts copies of the same artifact kind across projects; dependency \
         versions are not recorded, so identical packages are not identified."
            .to_owned(),
        "Agent worktrees are recognized by agent folder or branch names (e.g. .claude/worktrees, \
         codex/); worktree TTLs are not recorded yet (#111)."
            .to_owned(),
    ];
    let coverage = coverage(input, since);
    if !coverage.covers_window {
        notes.push(
            "Recorded history does not reach the start of the window; growth compares against \
             the oldest data available."
                .to_owned(),
        );
    }

    InsightsReport {
        generated_at: now,
        since,
        window_days,
        coverage,
        growth,
        regrowth: regrowth(input, since),
        duplication: duplication(&input.items, now),
        agent_footprint: agent_footprint(&input.items, now),
        accuracy: accuracy(&input.operations),
        ecosystem_mix: ecosystem_mix(&input.items, now),
        free_space: free_space(&input.volumes, since, now),
        own_footprint: own_footprint(input),
        notes,
    }
}

fn coverage(input: &InsightsInput, since: i64) -> Coverage {
    let oldest_event = input
        .items
        .iter()
        .filter_map(|i| i.events.first().map(|e| e.at))
        .min();
    let oldest_sample = input
        .volumes
        .iter()
        .filter_map(|v| v.iter().map(|s| s.at).filter(|t| *t >= since).min())
        .min();
    let history_starts_at = match (oldest_event, oldest_sample) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let items = input
        .items
        .iter()
        .filter(|i| i.disappeared_at.is_none_or(|d| d >= since))
        .count() as u64;
    Coverage {
        items,
        history_starts_at,
        covers_window: oldest_event.is_some_and(|t| t <= since),
    }
}

fn folder_name(p: &FsPath) -> String {
    let s = p.as_str().trim_end_matches(['/', '\\']);
    s.rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(s)
        .to_owned()
}

fn category_key(c: Category) -> String {
    match c {
        Category::ProjectArtifact => "project_artifact",
        Category::PackageCache => "package_cache",
        Category::Worktree => "worktree",
        Category::MlCache => "ml_cache",
        Category::Container => "container",
        Category::LargeFile => "large_file",
        Category::ScullData => "scull_data",
        _ => "other",
    }
    .to_owned()
}

#[derive(Default)]
struct Acc {
    start: u64,
    end: u64,
    ecosystems: BTreeSet<Option<String>>,
}

fn growth_window(items: &[ItemTimeline], now: i64, days: u32) -> GrowthWindow {
    let start = now - i64::from(days) * DAY;
    let mut cats: BTreeMap<String, Acc> = BTreeMap::new();
    let mut projects: BTreeMap<String, Acc> = BTreeMap::new();
    let (mut total_start, mut total_end) = (0u64, 0u64);
    for i in items {
        let (a, b) = (i.allocated_at(start), i.allocated_at(now));
        if a == 0 && b == 0 {
            continue;
        }
        total_start += a;
        total_end += b;
        let c = cats.entry(category_key(i.category)).or_default();
        c.start += a;
        c.end += b;
        if let Some(root) = &i.project_root {
            let p = projects.entry(root.as_str().to_owned()).or_default();
            p.start += a;
            p.end += b;
            p.ecosystems.insert(i.ecosystem.clone());
        }
    }
    let grown: u64 = cats.values().map(|a| a.end.saturating_sub(a.start)).sum();
    let growers = |m: BTreeMap<String, Acc>, project: bool| -> Vec<Grower> {
        let mut v: Vec<Grower> = m
            .into_iter()
            .filter(|(_, a)| a.end > a.start)
            .map(|(key, a)| {
                let growth = a.end - a.start;
                let ecosystem = match (project, a.ecosystems.len()) {
                    (true, 1) => a.ecosystems.into_iter().next().flatten(),
                    _ => None,
                };
                Grower {
                    label: if project {
                        folder_name(&FsPath::from(key.as_str()))
                    } else {
                        key.clone()
                    },
                    key,
                    ecosystem,
                    start_bytes: a.start,
                    end_bytes: a.end,
                    growth_bytes: growth,
                    share_of_growth: ratio(growth, grown),
                }
            })
            .collect();
        v.sort_by(|a, b| b.growth_bytes.cmp(&a.growth_bytes).then(a.key.cmp(&b.key)));
        v.truncate(TOP_GROWERS);
        v
    };
    GrowthWindow {
        days,
        start,
        net_bytes: i64::try_from(total_end).unwrap_or(i64::MAX)
            - i64::try_from(total_start).unwrap_or(i64::MAX),
        grown_bytes: grown,
        categories: growers(cats, false),
        projects: growers(projects, true),
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        // Rounded to 4 decimals so reports are stable across platforms.
        ((part as f64 / whole as f64) * 10_000.0).round() / 10_000.0
    }
}

fn median_i64(mut v: Vec<i64>) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2
    })
}

fn regrowth(input: &InsightsInput, since: i64) -> Vec<RegrowthInsight> {
    let project_of: BTreeMap<&str, &FsPath> = input
        .items
        .iter()
        .filter_map(|i| i.project_root.as_ref().map(|r| (i.item_id.as_str(), r)))
        .collect();
    let mut by_path: BTreeMap<(&str, String), Vec<&(Category, RegrowthEvent)>> = BTreeMap::new();
    for ev in input
        .regrowth
        .iter()
        .filter(|(_, e)| e.reappeared_at >= since)
    {
        by_path
            .entry((ev.1.path.as_str(), category_key(ev.0)))
            .or_default()
            .push(ev);
    }
    let mut out: Vec<RegrowthInsight> = by_path
        .into_values()
        .map(|evs| {
            let (category, last) = evs
                .iter()
                .max_by_key(|(_, e)| e.reappeared_at)
                .map(|(c, e)| (*c, e))
                .unwrap_or((evs[0].0, &evs[0].1));
            let rebuilds = evs.iter().filter(|(_, e)| !e.same_item()).count() as u32;
            let restores = evs.len() as u32 - rebuilds;
            let back = median_i64(
                evs.iter()
                    .map(|(_, e)| (e.reappeared_at - e.previous_last_seen_at).max(0))
                    .collect(),
            );
            let project_root = evs
                .iter()
                .find_map(|(_, e)| {
                    project_of
                        .get(e.item_id.as_str())
                        .or_else(|| project_of.get(e.previous_item_id.as_str()))
                })
                .map(|r| (*r).clone());
            let suggest_exclude = rebuilds >= REGROWTH_SUGGEST_MIN;
            let suggestion = suggest_exclude.then(|| {
                let mut s = format!(
                    "Rebuilt {rebuilds} times in this window: cleaning it only costs rebuild \
                     time. Consider excluding it from cleanup suggestions"
                );
                if last.artifact == "Installed npm dependencies" {
                    s.push_str(", or switch the project to pnpm");
                }
                s.push('.');
                s
            });
            RegrowthInsight {
                path: last.path.clone(),
                category,
                artifact: last.artifact.clone(),
                project: project_root.as_ref().map(folder_name),
                project_root,
                rebuilds,
                restores,
                median_back_within_secs: back,
                last_back_at: last.reappeared_at,
                suggest_exclude,
                suggestion,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (b.rebuilds + b.restores)
            .cmp(&(a.rebuilds + a.restores))
            .then(b.last_back_at.cmp(&a.last_back_at))
            .then(a.path.as_str().cmp(b.path.as_str()))
    });
    out
}

/// A store-once suggestion for copies of `artifact` across projects, if one fits.
pub fn duplication_suggestion(ecosystem: &str, artifact: &str, projects: u32) -> Option<String> {
    match (ecosystem, artifact) {
        ("node", "Installed npm dependencies") => Some(format!(
            "pnpm stores each package version once and links it into all {projects} projects."
        )),
        ("rust", "Cargo build output") => Some(format!(
            "A shared CARGO_TARGET_DIR lets {projects} projects reuse compiled dependencies."
        )),
        ("python", a) if a.to_ascii_lowercase().contains("virtual environment") => Some(format!(
            "uv keeps one global package cache and hardlinks it into {projects} environments."
        )),
        _ => None,
    }
}

fn duplication(items: &[ItemTimeline], now: i64) -> Vec<DuplicationInsight> {
    #[derive(Default)]
    struct D {
        projects: BTreeSet<String>,
        copies: u32,
        total: u64,
        largest: u64,
    }
    let mut groups: BTreeMap<(String, String), D> = BTreeMap::new();
    for i in items.iter().filter(|i| {
        i.category == Category::ProjectArtifact && i.disappeared_at.is_none() && i.alive_at(now)
    }) {
        let (Some(eco), Some(root)) = (&i.ecosystem, &i.project_root) else {
            continue;
        };
        let bytes = i.allocated_at(now);
        let d = groups.entry((eco.clone(), i.artifact.clone())).or_default();
        d.projects.insert(root.as_str().to_owned());
        d.copies += 1;
        d.total += bytes;
        d.largest = d.largest.max(bytes);
    }
    let mut out: Vec<DuplicationInsight> = groups
        .into_iter()
        .filter(|(_, d)| d.projects.len() as u32 >= DUPLICATION_MIN_PROJECTS)
        .map(|((ecosystem, artifact), d)| {
            let projects = d.projects.len() as u32;
            DuplicationInsight {
                suggestion: duplication_suggestion(&ecosystem, &artifact, projects),
                ecosystem,
                artifact,
                projects,
                copies: d.copies,
                total_bytes: d.total,
                largest_bytes: d.largest,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.total_bytes
            .cmp(&a.total_bytes)
            .then(a.artifact.cmp(&b.artifact))
    });
    out
}

/// The AI agent that created a worktree, from agent markers in its path (`.claude/worktrees/`,
/// `.codex/`, `.cursor/`, `.gemini/`) or in its branch (`Git worktree (claude/…)`, `codex/…`).
/// A display heuristic only: never used for any safety decision.
pub fn agent_of(path: &FsPath, artifact: &str) -> Option<&'static str> {
    const AGENTS: [(&str, &str, &str); 4] = [
        (".claude", "claude/", "claude-code"),
        (".codex", "codex/", "codex"),
        (".cursor", "cursor/", "cursor"),
        (".gemini", "gemini/", "gemini-cli"),
    ];
    let segments: Vec<&str> = path.as_str().split(['/', '\\']).collect();
    let branch = artifact
        .strip_prefix("Git worktree (")
        .and_then(|b| b.strip_suffix(')'))
        .unwrap_or("");
    AGENTS
        .iter()
        .find(|(dir, prefix, _)| segments.contains(dir) || branch.starts_with(prefix))
        .map(|(_, _, name)| *name)
}

fn agent_footprint(items: &[ItemTimeline], now: i64) -> AgentFootprint {
    let first_week = week_start(now) - i64::from(AGENT_WEEKS - 1) * 7 * DAY;
    let mut weeks: Vec<WeekCount> = (0..i64::from(AGENT_WEEKS))
        .map(|w| WeekCount {
            week_start: first_week + w * 7 * DAY,
            created: 0,
            agent_created: 0,
        })
        .collect();
    let mut out = AgentFootprint {
        worktrees: 0,
        worktree_bytes: 0,
        agent_worktrees: 0,
        agent_bytes: 0,
        by_agent: Vec::new(),
        weeks: Vec::new(),
        average_agent_lifetime_secs: None,
    };
    let mut by_agent: BTreeMap<&str, (u32, u64)> = BTreeMap::new();
    let mut lifetimes = Vec::new();
    for i in items.iter().filter(|i| i.category == Category::Worktree) {
        let agent = agent_of(&i.path, &i.artifact);
        if i.first_seen_at >= first_week {
            let idx = ((week_start(i.first_seen_at) - first_week) / (7 * DAY)) as usize;
            if let Some(w) = weeks.get_mut(idx) {
                w.created += 1;
                w.agent_created += u32::from(agent.is_some());
            }
        }
        if i.alive_at(now) && i.disappeared_at.is_none() {
            let bytes = i.allocated_at(now);
            out.worktrees += 1;
            out.worktree_bytes += bytes;
            if let Some(a) = agent {
                out.agent_worktrees += 1;
                out.agent_bytes += bytes;
                let e = by_agent.entry(a).or_default();
                e.0 += 1;
                e.1 += bytes;
            }
        } else if let (Some(_), Some(gone)) = (agent, i.disappeared_at)
            && gone >= first_week
        {
            lifetimes.push(gone - i.first_seen_at);
        }
    }
    out.by_agent = by_agent
        .into_iter()
        .map(|(agent, (worktrees, bytes))| AgentCount {
            agent: agent.to_owned(),
            worktrees,
            bytes,
        })
        .collect();
    out.by_agent
        .sort_by(|a, b| b.worktrees.cmp(&a.worktrees).then(a.agent.cmp(&b.agent)));
    out.weeks = weeks;
    out.average_agent_lifetime_secs =
        (!lifetimes.is_empty()).then(|| lifetimes.iter().sum::<i64>() / lifetimes.len() as i64);
    out
}

fn accuracy(ops: &[OperationAccuracy]) -> Accuracy {
    let operations: Vec<OperationPoint> = ops
        .iter()
        .map(|o| OperationPoint {
            operation_id: o.operation_id.clone(),
            at: o.started_at,
            items: o.items_compared,
            predicted_bytes: o.predicted_bytes,
            measured_bytes: o.measured_bytes,
            error: (o.predicted_bytes > 0).then(|| {
                let e =
                    (o.measured_bytes as f64 - o.predicted_bytes as f64) / o.predicted_bytes as f64;
                (e * 10_000.0).round() / 10_000.0
            }),
            regrown_items: o.regrown_items,
        })
        .collect();
    let mut errs: Vec<f64> = operations
        .iter()
        .filter_map(|o| o.error.map(f64::abs))
        .collect();
    errs.sort_by(f64::total_cmp);
    let median_abs_error = match errs.len() {
        0 => None,
        n if n % 2 == 1 => Some(errs[n / 2]),
        n => Some(((errs[n / 2 - 1] + errs[n / 2]) / 2.0 * 10_000.0).round() / 10_000.0),
    };
    Accuracy {
        predicted_bytes: ops.iter().map(|o| o.predicted_bytes).sum(),
        measured_bytes: ops.iter().map(|o| o.measured_bytes).sum(),
        regrown_items: ops.iter().map(|o| o.regrown_items).sum(),
        operations,
        median_abs_error,
    }
}

fn ecosystem_mix(items: &[ItemTimeline], now: i64) -> Vec<EcosystemMonth> {
    (0..i64::from(MIX_MONTHS))
        .rev()
        .map(|back| {
            let start = add_months(month_start(now), -back);
            let at = (add_months(start, 1) - 1).min(now);
            let mut per: BTreeMap<String, u64> = BTreeMap::new();
            for i in items {
                let b = i.allocated_at(at);
                if b > 0 {
                    *per.entry(i.ecosystem.clone().unwrap_or_else(|| "other".to_owned()))
                        .or_default() += b;
                }
            }
            let total: u64 = per.values().sum();
            let mut ecosystems: Vec<EcosystemShare> = per
                .into_iter()
                .map(|(ecosystem, bytes)| EcosystemShare {
                    ecosystem,
                    bytes,
                    share: ratio(bytes, total),
                })
                .collect();
            ecosystems.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.ecosystem.cmp(&b.ecosystem)));
            EcosystemMonth {
                month: iso_month(start),
                at,
                total_bytes: total,
                ecosystems,
            }
        })
        .collect()
}

/// The last point of each UTC day, keyed by `at`.
fn daily_last<T: Copy>(points: impl IntoIterator<Item = (i64, T)>) -> Vec<(i64, T)> {
    let mut by_day: BTreeMap<i64, (i64, T)> = BTreeMap::new();
    for (at, v) in points {
        let e = by_day.entry(day_start(at)).or_insert((at, v));
        if at >= e.0 {
            *e = (at, v);
        }
    }
    by_day.into_values().collect()
}

fn free_space(volumes: &[Vec<VolumeSample>], since: i64, now: i64) -> Vec<VolumeTrend> {
    volumes
        .iter()
        .filter_map(|samples| {
            let last = samples.iter().max_by_key(|s| s.at)?;
            let points: Vec<FreePoint> = daily_last(
                samples
                    .iter()
                    .filter(|s| s.at >= since && s.at <= now)
                    .map(|s| (s.at, s.free)),
            )
            .into_iter()
            .map(|(at, free_bytes)| FreePoint { at, free_bytes })
            .collect();
            let first = points.first().map_or(0, |p| p.free_bytes);
            let latest = points.last().map_or(0, |p| p.free_bytes);
            Some(VolumeTrend {
                volume_id: last.volume_id.clone(),
                total_bytes: last.total,
                change_bytes: i64::try_from(latest).unwrap_or(i64::MAX)
                    - i64::try_from(first).unwrap_or(i64::MAX),
                points,
                forecast: scull_proactive::forecast(
                    &last.volume_id,
                    samples,
                    &[],
                    now,
                    &ForecastPolicy::default(),
                ),
            })
        })
        .collect()
}

fn own_footprint(input: &InsightsInput) -> OwnFootprint {
    let f = input.footprint;
    OwnFootprint {
        database_bytes: f.database_bytes,
        wal_bytes: f.wal_bytes,
        backup_bytes: f.backup_bytes,
        damaged_bytes: f.damaged_bytes,
        total_bytes: f.total(),
        budget_used_bytes: input.budget_used_bytes,
        budget_bytes: input.budget_bytes,
        points: daily_last(
            input
                .footprint_history
                .iter()
                .map(|s| (s.at, s.footprint.total())),
        )
        .into_iter()
        .map(|(at, total_bytes)| FootprintPoint { at, total_bytes })
        .collect(),
    }
}
