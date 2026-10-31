//! Reading the inputs from scull's history store (read-only: only `SELECT` queries through
//! [`scull_storage::Store`]'s history API).

use crate::forecast::{DAY_SECS, Forecast, ForecastPolicy, Release, forecast};
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
