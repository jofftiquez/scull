//! Reading the inputs from scull's history store (read-only: only `SELECT` queries through
//! [`scull_storage::Store`]'s history API).

use crate::alert::Alert;
use crate::forecast::{DAY_SECS, Forecast, ForecastPolicy, Release, forecast};
use crate::policy::{SeriesPoint, Usage, free_space_drop_alert};
use scull_core::Category;
use scull_core::config::ProactiveConfig;
use scull_storage::{Store, StoreError};

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
/// comparable (a broader scan is not growth), and the store does not yet expose each scan's
/// roots, so only feed this to spike detection when the caller knows the scans cover the same
/// roots.
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
