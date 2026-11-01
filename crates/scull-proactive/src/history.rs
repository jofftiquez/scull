//! Reading the inputs from scull's history store (read-only: only `SELECT` queries through
//! [`scull_storage::Store`]'s history API).

use crate::alert::Alert;
use crate::forecast::{DAY_SECS, Forecast, ForecastPolicy, Release, forecast};
use crate::policy::{
    SeriesPoint, Usage, activity_from_candidate, budget_alerts, category_spike_alert,
    free_space_drop_alert, stale_alerts, usage_from_candidates,
};
use scull_core::config::ProactiveConfig;
use scull_core::{Candidate, Category, FsPath, ScanId};
use scull_storage::{Store, StoreError};
use std::collections::{BTreeSet, HashMap};

/// Forecast one volume from its recorded free-space samples in the policy window.
/// `releases` are bytes known to have been freed on that volume (may be empty).
pub fn forecast_volume(
    store: &Store,
    volume_id: &str,
    releases: &[Release],
    now: i64,
    policy: &ForecastPolicy,
) -> Result<Forecast, StoreError> {
    let since = now.saturating_sub(i64::from(policy.window_days).saturating_mul(DAY_SECS));
    let samples = store.free_space_history(volume_id, since)?;
    Ok(forecast(volume_id, &samples, releases, now, policy))
}

/// Forecast every volume that has ever been sampled (D-22: each drive its own forecast),
/// sorted by volume id. No releases are known from the store yet (cleanup operations live on
/// the Phase 2 branch), so cleanups are detected from the samples alone.
pub fn forecast_all(
    store: &Store,
    now: i64,
    policy: &ForecastPolicy,
) -> Result<Vec<Forecast>, StoreError> {
    let mut volumes: Vec<String> = store
        .status_summary()?
        .volumes
        .into_iter()
        .map(|v| v.volume_id)
        .collect();
    volumes.sort();
    volumes.dedup();
    volumes
        .iter()
        .map(|v| forecast_volume(store, v, &[], now, policy))
        .collect()
}

/// Every category, for per-category history queries.
pub const CATEGORIES: [Category; 8] = [
    Category::ProjectArtifact,
    Category::PackageCache,
    Category::Worktree,
    Category::MlCache,
    Category::Container,
    Category::LargeFile,
    Category::ScullData,
    Category::Other,
];

/// `category`'s allocated bytes per usable scan finished at or after `since`, oldest first
/// (input for [`crate::policy::spike`]). Points of scans with different roots are not
/// comparable (a broader scan is not growth): use [`category_spike_alerts`], which keeps only
/// scans of the same roots, for spike detection.
pub fn category_series(
    store: &Store,
    category: Category,
    since: i64,
) -> Result<Vec<SeriesPoint>, StoreError> {
    Ok(store
        .rollup_history(category, since)?
        .into_iter()
        .map(|p| SeriesPoint {
            at: p.at,
            bytes: p.allocated_bytes,
        })
        .collect())
}

/// Allocated bytes per category at the latest usable scan of the last 30 days (budget input;
/// category filters only, see [`crate::policy`]). Categories with nothing are omitted.
pub fn category_usage(store: &Store, now: i64) -> Result<Vec<Usage>, StoreError> {
    let since = now.saturating_sub(30 * DAY_SECS);
    let mut out = Vec::new();
    for c in CATEGORIES {
        if let Some(p) = store.rollup_history(c, since)?.last()
            && p.allocated_bytes > 0
        {
            out.push(Usage {
                category: Some(c),
                ecosystem: None,
                path: None,
                bytes: p.allocated_bytes,
            });
        }
    }
    Ok(out)
}

/// Free-space drop alerts (PRO-04) for every volume ever sampled.
pub fn free_space_drop_alerts(
    store: &Store,
    cfg: &ProactiveConfig,
    now: i64,
) -> Result<Vec<Alert>, StoreError> {
    let window = i64::try_from(cfg.spike_window.as_secs()).unwrap_or(i64::MAX);
    let since = now.saturating_sub(window.saturating_mul(2));
    let mut out = Vec::new();
    for v in store.status_summary()?.volumes {
        let samples = store.free_space_history(&v.volume_id, since)?;
        out.extend(free_space_drop_alert(&v.volume_id, &samples, cfg, now));
    }
    crate::alert::sort_alerts(&mut out);
    Ok(out)
}

/// The roots of the latest usable (`completed`/`partial`) scan, if any: the series that
/// category spikes compare when the caller did not just scan.
pub fn latest_scan_roots(store: &Store) -> Result<Option<Vec<FsPath>>, StoreError> {
    let Some(last) = store.status_summary()?.last_scan else {
        return Ok(None);
    };
    store.scan_roots(&last.scan_id)
}

/// Category growth spikes (PRO-04) over the scans of exactly `roots` (as a set) in the last
/// two spike windows: one [`crate::AlertKind::GrowthSpike`] warning per category whose
/// allocated bytes grew past the spike threshold. Scans of other roots are ignored (a broader
/// scan is not growth). `paths` names the folders responsible for a category (largest first),
/// when the caller knows them.
pub fn category_spike_alerts(
    store: &Store,
    roots: &[FsPath],
    cfg: &ProactiveConfig,
    now: i64,
    paths: impl Fn(Category) -> Vec<String>,
) -> Result<Vec<Alert>, StoreError> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let want: BTreeSet<&str> = roots.iter().map(FsPath::as_str).collect();
    let window = i64::try_from(cfg.spike_window.as_secs()).unwrap_or(i64::MAX);
    let since = now.saturating_sub(window.saturating_mul(2));
    let mut same: HashMap<ScanId, bool> = HashMap::new();
    let mut out = Vec::new();
    for c in CATEGORIES {
        let mut points = Vec::new();
        for p in store.rollup_history(c, since)? {
            let matches = match same.get(&p.scan_id) {
                Some(m) => *m,
                None => {
                    let m = store.scan_roots(&p.scan_id)?.is_some_and(|r| {
                        r.iter().map(FsPath::as_str).collect::<BTreeSet<_>>() == want
                    });
                    same.insert(p.scan_id.clone(), m);
                    m
                }
            };
            if matches {
                points.push(SeriesPoint {
                    at: p.at,
                    bytes: p.allocated_bytes,
                });
            }
        }
        if points.len() >= 2
            && let Some(a) = category_spike_alert(c, &points, paths(c), cfg, now)
        {
            out.push(a);
        }
    }
    crate::alert::sort_alerts(&mut out);
    Ok(out)
}

/// The scan a check just ran: its roots and candidates.
#[derive(Debug, Clone, Copy)]
pub struct ScanInput<'a> {
    /// The roots scanned (as recorded).
    pub roots: &'a [FsPath],
    /// Everything it found.
    pub candidates: &'a [Candidate],
}

/// Every alert a check raises (PRD §16), most severe first:
///
/// * disk-full forecasts and low space ([`crate::forecast_alerts`], PRO-01);
/// * budgets (PRO-03): over `scan`'s candidates when there is one (category, ecosystem and
///   path budgets), otherwise over the latest recorded scan's category totals (category
///   budgets only);
/// * staleness (PRO-02) of `scan`'s worktrees, build outputs and caches (needs a scan:
///   history keeps no modification times), and expired worktree TTLs (PRO-08): `ttls` are
///   registered `(worktree path, expires_at)` pairs; a scanned worktree gets its TTL, and a
///   registered worktree the scan did not see (outside the roots, or no scan) is still
///   reported when its TTL has run out (the caller passes only paths that still exist);
/// * category growth spikes over scans of the same roots as `scan` (or as the latest recorded
///   scan), naming the largest items of the category when `scan` is given, and free-space
///   drops of every volume (PRO-04).
///
/// Read-only: nothing is recorded or removed.
pub fn check_alerts(
    store: &Store,
    cfg: &ProactiveConfig,
    forecasts: &[Forecast],
    scan: Option<ScanInput<'_>>,
    ttls: &[(String, i64)],
    now: i64,
) -> Result<Vec<Alert>, StoreError> {
    let policy = crate::policy::forecast_policy(cfg);
    let mut alerts = crate::alert::forecast_alerts(forecasts, &policy, now);
    if !cfg.budgets.is_empty() {
        let usage = match scan {
            Some(s) => usage_from_candidates(s.candidates),
            None => category_usage(store, now)?,
        };
        alerts.extend(budget_alerts(&cfg.budgets, &usage, now));
    }
    let mut items: Vec<crate::policy::Activity> = scan
        .map(|s| {
            s.candidates
                .iter()
                .filter_map(activity_from_candidate)
                .collect()
        })
        .unwrap_or_default();
    for (path, expires_at) in ttls {
        match items
            .iter_mut()
            .find(|a| a.kind == crate::policy::StaleKind::Worktree && &a.path == path)
        {
            Some(a) => a.ttl_expires_at = Some(*expires_at),
            None => items.push(crate::policy::Activity {
                path: path.clone(),
                kind: crate::policy::StaleKind::Worktree,
                last_activity_at: None,
                ttl_expires_at: Some(*expires_at),
                bytes: 0,
                in_use: false,
            }),
        }
    }
    alerts.extend(stale_alerts(&items, cfg, now));
    let roots = match scan {
        Some(s) => Some(s.roots.to_vec()),
        None => latest_scan_roots(store)?,
    };
    if let Some(roots) = roots {
        let largest = |c: Category| -> Vec<String> {
            let mut of: Vec<&Candidate> = scan
                .map(|s| s.candidates.iter().filter(|x| x.category == c).collect())
                .unwrap_or_default();
            of.sort_by(|a, b| {
                b.sizes
                    .allocated
                    .cmp(&a.sizes.allocated)
                    .then_with(|| a.path.as_str().cmp(b.path.as_str()))
            });
            of.iter().map(|x| x.path.as_str().to_owned()).collect()
        };
        alerts.extend(category_spike_alerts(store, &roots, cfg, now, largest)?);
    }
    alerts.extend(free_space_drop_alerts(store, cfg, now)?);
    crate::alert::sort_alerts(&mut alerts);
    Ok(alerts)
}
