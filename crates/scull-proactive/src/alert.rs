//! Alert records (PRD §16; INS-06 "Alerts and responses").
//!
//! An [`Alert`] says what needs attention, how urgently, and about what. Alerts never act:
//! the fix is a reviewed cleanup (PRO-05), never an automatic deletion (PRO-08, D-11).
//! Every alert has a stable [`Alert::key`] (kind + subject) so a caller can de-duplicate
//! repeated checks and record the user's response (acted, dismissed, snoozed) against it.

use crate::forecast::{Confidence, Forecast, ForecastPolicy, ForecastState};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What an alert is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// A volume is forecast to drop below its reserve soon (PRO-01: time-based, default
    /// within 14 days).
    DiskFullForecast,
    /// A volume is already below its reserve (measured).
    LowSpace,
    /// Worktrees, build outputs or caches idle longer than their staleness window (PRO-02).
    Stale,
    /// An agent or script worktree is past its registered TTL (PRO-08).
    WorktreeTtlExpired,
    /// A category, ecosystem or path is over its size budget (PRO-03).
    BudgetExceeded,
    /// A category grew faster than the spike threshold (PRO-04).
    GrowthSpike,
    /// A volume lost free space faster than the spike threshold (PRO-04).
    FreeSpaceDrop,
}

impl AlertKind {
    /// The serde name (`disk_full_forecast`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::DiskFullForecast => "disk_full_forecast",
            AlertKind::LowSpace => "low_space",
            AlertKind::Stale => "stale",
            AlertKind::WorktreeTtlExpired => "worktree_ttl_expired",
            AlertKind::BudgetExceeded => "budget_exceeded",
            AlertKind::GrowthSpike => "growth_spike",
            AlertKind::FreeSpaceDrop => "free_space_drop",
        }
    }
}

/// How urgent an alert is. Ordered: `Info < Warning < Critical`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth knowing.
    Info,
    /// Act within days.
    Warning,
    /// Act now.
    Critical,
}

/// One alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Alert {
    /// What it is about.
    pub kind: AlertKind,
    /// How urgent it is.
    pub severity: Severity,
    /// Stable identity for de-duplication and responses: `<kind>:<subject>`.
    pub key: String,
    /// The volume, category or path it concerns.
    pub subject: String,
    /// One plain sentence for people (not terminal-escaped: callers escape on output).
    pub message: String,
    /// When it was raised (Unix seconds).
    pub raised_at: i64,
    /// The bytes concerned (stale total, budget usage, growth), when there is such a number.
    pub bytes: Option<u64>,
    /// The folders responsible (largest first, at most [`MAX_ALERT_PATHS`]); may be empty.
    pub paths: Vec<String>,
}

/// At most this many paths are named in one alert.
pub const MAX_ALERT_PATHS: usize = 5;

impl Alert {
    /// An alert with its key derived from `kind` and `subject`.
    pub fn new(
        kind: AlertKind,
        severity: Severity,
        subject: impl Into<String>,
        message: impl Into<String>,
        raised_at: i64,
    ) -> Alert {
        let subject = subject.into();
        Alert {
            key: format!("{}:{subject}", kind.as_str()),
            kind,
            severity,
            subject,
            message: message.into(),
            raised_at,
            bytes: None,
            paths: Vec::new(),
        }
    }

    /// With the bytes concerned.
    #[must_use]
    pub fn with_bytes(mut self, bytes: u64) -> Alert {
        self.bytes = Some(bytes);
        self
    }

    /// With the folders responsible (truncated to [`MAX_ALERT_PATHS`]).
    #[must_use]
    pub fn with_paths(mut self, mut paths: Vec<String>) -> Alert {
        paths.truncate(MAX_ALERT_PATHS);
        self.paths = paths;
        self
    }
}

/// Alerts from forecasts: a volume below its reserve is critical; one forecast to drop below
/// it within [`ForecastPolicy::alert_days`] (with at least medium confidence) is a warning,
/// critical within [`ForecastPolicy::critical_days`]. Low-confidence and "not enough
/// history" forecasts never alert (no guessing). Sorted most severe first, then by subject.
pub fn forecast_alerts(forecasts: &[Forecast], policy: &ForecastPolicy, now: i64) -> Vec<Alert> {
    let mut out: Vec<Alert> = forecasts
        .iter()
        .filter_map(|f| forecast_alert(f, policy, now))
        .collect();
    sort_alerts(&mut out);
    out
}

fn forecast_alert(f: &Forecast, policy: &ForecastPolicy, now: i64) -> Option<Alert> {
    let reserve = format_bytes(f.reserve_bytes);
    match f.state {
        ForecastState::BelowReserve => Some(Alert::new(
            AlertKind::LowSpace,
            Severity::Critical,
            &f.volume_id,
            format!(
                "{} has only {} free, below the {reserve} reserve",
                f.volume_id,
                format_bytes(f.free_bytes.unwrap_or(0))
            ),
            now,
        )),
        ForecastState::Filling if f.confidence >= Confidence::Medium => {
            let days = f.days_rounded()?;
            if days >= policy.alert_days {
                return None;
            }
            let severity = if days < policy.critical_days {
                Severity::Critical
            } else {
                Severity::Warning
            };
            Some(Alert::new(
                AlertKind::DiskFullForecast,
                severity,
                &f.volume_id,
                format!(
                    "{} will drop below {reserve} free in about {} ({} confidence)",
                    f.volume_id,
                    plural(days, "day"),
                    f.confidence.as_str()
                ),
                now,
            ))
        }
        _ => None,
    }
}

/// Most severe first, then by kind and subject (stable output).
pub fn sort_alerts(alerts: &mut [Alert]) {
    alerts.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
            .then_with(|| a.subject.cmp(&b.subject))
    });
}

/// `1 day`, `9 days`.
pub fn plural(n: u32, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// Binary units with one decimal, like the CLI: `512 B`, `1.5 KiB`, `41.0 GiB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::GIB;

    fn fc(state: ForecastState, confidence: Confidence, days: Option<f64>) -> Forecast {
        Forecast {
            volume_id: "/".into(),
            state,
            confidence,
            as_of: Some(0),
            free_bytes: Some(25 * GIB),
            total_bytes: Some(500 * GIB),
            reserve_bytes: 10 * GIB,
            fill_bytes_per_day: Some(2.0 * GIB as f64),
            days_until_threshold: days,
            threshold_at: None,
            samples_used: 20,
            span_secs: 20 * 86_400,
            outliers_ignored: 0,
            jumps_ignored: 0,
            noise_ratio: Some(0.0),
        }
    }

    #[test]
    fn forecast_alerts_fire_by_time_and_confidence() {
        let p = ForecastPolicy::default();
        let a = forecast_alerts(
            &[fc(ForecastState::Filling, Confidence::High, Some(8.6))],
            &p,
            7,
        );
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, AlertKind::DiskFullForecast);
        assert_eq!(a[0].severity, Severity::Warning);
        assert_eq!(a[0].key, "disk_full_forecast:/");
        assert_eq!(a[0].raised_at, 7);
        assert_eq!(
            a[0].message,
            "/ will drop below 10.0 GiB free in about 9 days (high confidence)"
        );
        let a = forecast_alerts(
            &[fc(ForecastState::Filling, Confidence::Medium, Some(1.2))],
            &p,
            0,
        );
        assert_eq!(a[0].severity, Severity::Critical);
        assert!(a[0].message.contains("about 1 day "), "{}", a[0].message);
        // Beyond the alert window, low confidence, or no forecast: no alert.
        for f in [
            fc(ForecastState::Filling, Confidence::High, Some(14.0)),
            fc(ForecastState::Filling, Confidence::Low, Some(2.0)),
            fc(ForecastState::NotFilling, Confidence::High, None),
            fc(ForecastState::NotEnoughHistory, Confidence::Unknown, None),
        ] {
            assert!(
                forecast_alerts(std::slice::from_ref(&f), &p, 0).is_empty(),
                "{f:?}"
            );
        }
    }

    #[test]
    fn below_reserve_is_critical_and_sorted_first() {
        let p = ForecastPolicy::default();
        let mut low = fc(ForecastState::BelowReserve, Confidence::High, Some(0.0));
        low.volume_id = "/data".into();
        low.free_bytes = Some(3 * GIB);
        let soon = fc(ForecastState::Filling, Confidence::High, Some(5.0));
        let a = forecast_alerts(&[soon, low], &p, 0);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].kind, AlertKind::LowSpace);
        assert_eq!(a[0].severity, Severity::Critical);
        assert_eq!(
            a[0].message,
            "/data has only 3.0 GiB free, below the 10.0 GiB reserve"
        );
        assert_eq!(a[1].kind, AlertKind::DiskFullForecast);
    }

    #[test]
    fn units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(41 * GIB), "41.0 GiB");
        assert_eq!(plural(1, "day"), "1 day");
        assert_eq!(plural(0, "day"), "0 days");
    }
}
