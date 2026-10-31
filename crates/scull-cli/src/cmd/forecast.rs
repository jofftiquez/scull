//! Forecast views shared by `scull status` and `scull check` (PRO-01, PRO-09): the
//! `scull-proactive` forecast of one volume as it appears in JSON and in text.

use schemars::JsonSchema;
use scull_proactive::{Confidence, Forecast, ForecastState};
use serde::Serialize;

/// One volume's disk-full forecast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForecastReport {
    /// Volume identifier (mount point or volume GUID path).
    pub volume_id: String,
    /// `not_enough_history`, `not_filling`, `filling` or `below_reserve`.
    pub state: ForecastState,
    /// `unknown`, `low`, `medium` or `high`. Status lines and alerts use `medium` and up.
    pub confidence: Confidence,
    /// Latest free space (bytes), plus bytes known to be freed since.
    pub free_bytes: Option<u64>,
    /// The free space the forecast counts down to (bytes).
    pub reserve_bytes: u64,
    /// Whole days until free space drops below the reserve (`filling`: at least 1;
    /// `below_reserve`: 0).
    pub days_until_threshold: Option<u32>,
    /// When free space is forecast to drop below the reserve (Unix seconds).
    pub threshold_at: Option<i64>,
    /// Free space lost per day by the trend, in bytes (negative when free space grows).
    pub fill_bytes_per_day: Option<i64>,
    /// Samples the forecast used.
    pub samples: u32,
    /// Seconds between the first and the latest sample used.
    pub span_secs: i64,
    /// When the latest sample was taken (Unix seconds).
    pub as_of: Option<i64>,
}

impl From<&Forecast> for ForecastReport {
    fn from(f: &Forecast) -> Self {
        ForecastReport {
            volume_id: f.volume_id.clone(),
            state: f.state,
            confidence: f.confidence,
            free_bytes: f.free_bytes,
            reserve_bytes: f.reserve_bytes,
            days_until_threshold: f.days_rounded(),
            threshold_at: f.threshold_at,
            fill_bytes_per_day: f.fill_bytes_per_day.map(|r| r.round() as i64),
            samples: f.samples_used,
            span_secs: f.span_secs,
            as_of: f.as_of,
        }
    }
}

impl ForecastReport {
    /// Whether a status line may show it: filling with at least medium confidence.
    pub fn shows_in_status(&self) -> bool {
        self.state == ForecastState::Filling && self.confidence >= Confidence::Medium
    }

    /// `~1 day`, `~9 days` while filling with enough confidence.
    pub fn days_text(&self) -> Option<String> {
        if !self.shows_in_status() {
            return None;
        }
        self.days_until_threshold.map(|d| match d {
            1 => "~1 day".to_owned(),
            d => format!("~{d} days"),
        })
    }
}

/// The forecast a status line shows: the soonest to fill among those it may show.
pub fn headline(forecasts: &[ForecastReport]) -> Option<&ForecastReport> {
    forecasts
        .iter()
        .filter(|f| f.shows_in_status())
        .min_by_key(|f| {
            (
                f.days_until_threshold.unwrap_or(u32::MAX),
                f.volume_id.clone(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(
        volume: &str,
        state: ForecastState,
        conf: Confidence,
        days: Option<u32>,
    ) -> ForecastReport {
        ForecastReport {
            volume_id: volume.into(),
            state,
            confidence: conf,
            free_bytes: Some(1),
            reserve_bytes: 0,
            days_until_threshold: days,
            threshold_at: None,
            fill_bytes_per_day: None,
            samples: 10,
            span_secs: 0,
            as_of: None,
        }
    }

    #[test]
    fn headline_picks_the_soonest_confident_forecast() {
        let all = vec![
            report("/", ForecastState::Filling, Confidence::Medium, Some(20)),
            report("/a", ForecastState::Filling, Confidence::Low, Some(2)),
            report("/b", ForecastState::Filling, Confidence::High, Some(9)),
            report("/c", ForecastState::NotFilling, Confidence::High, None),
            report("/d", ForecastState::BelowReserve, Confidence::High, Some(0)),
        ];
        let h = headline(&all).expect("headline");
        assert_eq!(h.volume_id, "/b");
        assert_eq!(h.days_text().as_deref(), Some("~9 days"));
        assert_eq!(all[1].days_text(), None, "low confidence is never shown");
        let one = report("/", ForecastState::Filling, Confidence::High, Some(1));
        assert_eq!(one.days_text().as_deref(), Some("~1 day"));
        assert!(headline(&all[1..2]).is_none());
    }
}
