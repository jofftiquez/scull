//! `forecasts`: the disk-full forecast of every sampled volume (PRO-01), for every desktop
//! surface that shows days left: the Insights tile and chart, the Overview alert and the menu
//! bar widget.
//!
//! **One forecast source.** These are computed by [`scull_proactive::forecast_all`] with the
//! configured policy ([`Session::forecast_policy`], `[proactive]` in config.toml): the same
//! method, samples window and policy as `scull check`, `scull status`, the MCP server
//! (`check_capacity`), `scull insights` and the weekly digest. The frontend draws them; it does
//! not fit its own trend. Read-only: never creates or migrates the database.

use crate::insights::{HistoryRead, Source};
use crate::session::Session;
use schemars::JsonSchema;
use scull_proactive::{Forecast, ForecastPolicy, forecast_all, forecast_policy};
use serde::Serialize;

/// `forecasts`: every sampled volume's forecast.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForecastsView {
    /// Whether the database answered.
    pub state: HistoryRead,
    /// Why it didn't, when it didn't.
    pub detail: Option<String>,
    /// When they were computed (Unix seconds).
    pub now: i64,
    /// The reserve every forecast counts down to (bytes; `[proactive] reserve_gib`).
    pub reserve_bytes: u64,
    /// Forecasts are shown as alerts within this many days (`[proactive] alert_days`).
    pub alert_days: u32,
    /// One per sampled volume, sorted by volume ID.
    pub forecasts: Vec<Forecast>,
}

impl Session {
    /// The disk-full forecast policy every desktop surface uses: the configured one
    /// (`[proactive]`), or the defaults when the config can't be read.
    pub fn forecast_policy(&self) -> ForecastPolicy {
        self.config()
            .map(|c| forecast_policy(&c.proactive))
            .unwrap_or_default()
    }

    /// Every sampled volume's forecast at `now` (see the module docs).
    pub fn forecasts(&self, now: i64) -> ForecastsView {
        let policy = self.forecast_policy();
        let src = self.history_source();
        let (mut state, mut detail) = src.state();
        let mut forecasts = Vec::new();
        if let Source::Store(store) = &src {
            match forecast_all(store, now, &policy) {
                Ok(f) => forecasts = f,
                Err(e) => {
                    state = HistoryRead::Unavailable;
                    detail = Some(e.to_string());
                }
            }
        }
        ForecastsView {
            state,
            detail,
            now,
            reserve_bytes: policy.reserve_bytes,
            alert_days: policy.alert_days,
            forecasts,
        }
    }
}
