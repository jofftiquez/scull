//! `scull check` (PRD §12, PRO-01, PRO-07): sample free space, forecast every drive, raise
//! alerts. Meant to be run periodically by the OS scheduler.
//!
//! 1. **Sample.** Free space of every volume scull has sampled before (D-22: each drive its
//!    own forecast; a drive that is not connected right now is listed as not sampled) and of
//!    the volumes holding the configured scan roots. The samples are recorded in the history
//!    database like a scan's (INS-06: "every scan and every scheduled check").
//! 2. **Forecast** each volume from its last 30 days of samples ([`scull_proactive::forecast()`]).
//! 3. **Alert** on volumes below their reserve, or forecast to drop below it within 14 days
//!    with at least medium confidence ([`scull_proactive::forecast_alerts`]).
//!
//! Writes nothing but history rows and never deletes anything: an alert points to a reviewed
//! cleanup, it never performs one. Exit code 0 whether or not alerts fired (alerts are data).
//!
//! Not in this command yet: the light scan of known locations (PRO-07) and the ready-to-review
//! reclaim plan it keeps (PRO-05, #115); registering it with the OS scheduler and running at
//! low priority (`scull schedule enable|disable`, #114).

use super::forecast::ForecastReport;
use super::status::VolumeReport;
use super::{emit_machine, emit_text};
use crate::cmd::pipeline::load_config;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use schemars::JsonSchema;
use scull_core::{ErrorKind, ScullError};
use scull_proactive::{Alert, Confidence, ForecastPolicy, ForecastState, Severity};
use scull_storage::VolumeSample;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Schema of `scull check --json`.
pub const CHECK_SCHEMA: SchemaId = SchemaId::new("check", 1);

/// `scull check` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckReport {
    /// When the check ran (Unix seconds).
    pub now: i64,
    /// Free space sampled now (and recorded in history).
    pub sampled: Vec<VolumeReport>,
    /// Volumes known from history that could not be sampled now.
    pub not_sampled: Vec<NotSampled>,
    /// Disk-full forecast of every volume ever sampled, by volume id.
    pub forecasts: Vec<ForecastReport>,
    /// Alerts raised, most severe first.
    pub alerts: Vec<Alert>,
}

/// A volume known from history that was not sampled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotSampled {
    /// Volume identifier as recorded.
    pub volume_id: String,
    /// Why: e.g. `not connected`.
    pub reason: String,
}

/// `scull check`.
pub fn run(ui: &Ui) -> Result<(), ScullError> {
    let now = scull_engine::now_unix();
    let loaded = load_config(&[])?;
    let dirs = scull_platform::app_dirs().map_err(|e| {
        ScullError::new(
            ErrorKind::Io,
            format!("cannot locate scull's data directory: {e}"),
        )
    })?;
    let mut store = scull_engine::open_history(&dirs.data).map_err(store_error)?;
    let known: Vec<String> = store
        .status_summary()
        .map_err(store_error)?
        .volumes
        .into_iter()
        .map(|v| v.volume_id)
        .collect();
    let (samples, not_sampled) = sample_volumes(&known, &loaded.config.scan.roots, now);
    if !samples.is_empty() {
        store
            .record_free_space(&samples, None)
            .map_err(store_error)?;
    }
    let policy = ForecastPolicy::default();
    let forecasts = scull_proactive::forecast_all(&store, now, &policy).map_err(store_error)?;
    let alerts = scull_proactive::forecast_alerts(&forecasts, &policy, now);
    let report = CheckReport {
        now,
        sampled: samples
            .iter()
            .map(|v| VolumeReport {
                volume_id: v.volume_id.clone(),
                total_bytes: v.total,
                free_bytes: v.free,
                sampled_at: v.at,
            })
            .collect(),
        not_sampled,
        forecasts: forecasts.iter().map(ForecastReport::from).collect(),
        alerts,
    };
    if ui.format.is_machine() {
        return emit_machine(ui.format, CHECK_SCHEMA, "check", &report);
    }
    // Volume IDs come from the database and the OS: escape them, line by line (SAFE-17).
    let text: String = render(&report)
        .lines()
        .map(|l| escape_for_terminal(l) + "\n")
        .collect();
    emit_text(&text)
}

fn store_error(e: scull_storage::StoreError) -> ScullError {
    ScullError::new(
        ErrorKind::Io,
        format!("cannot use the history database: {e}"),
    )
}

/// Sample the free space of the `known` volumes (mount points recorded in history) and of
/// the volumes holding `roots`. A known volume whose mount point is missing or no longer a
/// mount point (a drive that is not connected) is reported, not sampled.
pub fn sample_volumes(
    known: &[String],
    roots: &[PathBuf],
    now: i64,
) -> (Vec<VolumeSample>, Vec<NotSampled>) {
    let mut samples: Vec<VolumeSample> = Vec::new();
    let mut missing = Vec::new();
    let push = |samples: &mut Vec<VolumeSample>, path: &Path| -> Result<(), String> {
        let v = scull_platform::volume_of(path).map_err(|_| "not connected".to_owned())?;
        let id = v.mount_point.to_string_lossy().into_owned();
        let free = scull_platform::free_space(path).ok().or(v.free_bytes);
        let (Some(total), Some(free)) = (v.total_bytes, free) else {
            return Err("size unknown".to_owned());
        };
        if id.is_empty() || free > total {
            return Err("size unknown".to_owned());
        }
        if !samples.iter().any(|s| s.volume_id == id) {
            samples.push(VolumeSample {
                volume_id: id,
                total,
                free,
                at: now,
            });
        }
        Ok(())
    };
    for id in known {
        let path = Path::new(id);
        let mounted = scull_platform::volume_of(path)
            .is_ok_and(|v| v.mount_point.to_string_lossy() == id.as_str());
        let outcome = if mounted {
            push(&mut samples, path)
        } else {
            Err("not connected".to_owned())
        };
        if let Err(reason) = outcome {
            missing.push(NotSampled {
                volume_id: id.clone(),
                reason,
            });
        }
    }
    for root in roots {
        if root.exists() {
            // A root whose volume can't be read is reported by `scull scan`, not here.
            let _ = push(&mut samples, root);
        }
    }
    samples.sort_by(|a, b| a.volume_id.cmp(&b.volume_id));
    (samples, missing)
}

/// The human output: one line per volume, then the alerts.
pub fn render(r: &CheckReport) -> String {
    let mut out = String::new();
    if r.forecasts.is_empty() && r.sampled.is_empty() {
        out.push_str(
            "scull check: no volumes to check yet; run `scull scan <path>` or set `scan.roots`\n",
        );
        return out;
    }
    out.push_str(&format!(
        "scull check: sampled {} volume{}\n",
        r.sampled.len(),
        if r.sampled.len() == 1 { "" } else { "s" }
    ));
    let width = r
        .forecasts
        .iter()
        .map(|f| f.volume_id.chars().count())
        .max()
        .unwrap_or(0);
    for f in &r.forecasts {
        let free = f
            .free_bytes
            .map(|b| format!("{} free", format_bytes(b)))
            .unwrap_or_else(|| "free space unknown".to_owned());
        let state = match f.state {
            ForecastState::NotEnoughHistory => format!(
                "not enough history for a forecast ({} sample{} over {})",
                f.samples,
                if f.samples == 1 { "" } else { "s" },
                span(f.span_secs)
            ),
            ForecastState::NotFilling => "not filling up".to_owned(),
            ForecastState::BelowReserve => {
                format!("below the {} reserve", format_bytes(f.reserve_bytes))
            }
            ForecastState::Filling => {
                let days = f.days_until_threshold.unwrap_or(0);
                let when = if days == 1 {
                    "~1 day".to_owned()
                } else {
                    format!("~{days} days")
                };
                let quiet = if f.confidence < Confidence::Medium {
                    ", not alerting"
                } else {
                    ""
                };
                format!(
                    "below {} free in {when} ({} confidence{quiet})",
                    format_bytes(f.reserve_bytes),
                    f.confidence.as_str()
                )
            }
        };
        let vol = &f.volume_id;
        out.push_str(&format!("  {vol:<width$}  {free} \u{b7} {state}\n"));
    }
    for m in &r.not_sampled {
        out.push_str(&format!("  {}: not sampled ({})\n", m.volume_id, m.reason));
    }
    for a in &r.alerts {
        let level = match a.severity {
            Severity::Critical => "critical",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        out.push_str(&format!("{level}: {}\n", a.message));
    }
    if r.alerts.is_empty() {
        out.push_str("no alerts\n");
    }
    out
}

/// `3 days`, `5 hours`, `<1 hour`.
fn span(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 86_400 {
        let d = secs / 86_400;
        format!("{d} day{}", if d == 1 { "" } else { "s" })
    } else if secs >= 3_600 {
        let h = secs / 3_600;
        format!("{h} hour{}", if h == 1 { "" } else { "s" })
    } else {
        "<1 hour".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn fr(
        volume: &str,
        state: ForecastState,
        conf: Confidence,
        days: Option<u32>,
    ) -> ForecastReport {
        ForecastReport {
            volume_id: volume.into(),
            state,
            confidence: conf,
            free_bytes: Some(41 * GIB),
            reserve_bytes: 10 * GIB,
            days_until_threshold: days,
            threshold_at: None,
            fill_bytes_per_day: None,
            samples: 3,
            span_secs: 86_400,
            as_of: None,
        }
    }

    #[test]
    fn render_lines() {
        let r = CheckReport {
            now: 0,
            sampled: vec![VolumeReport {
                volume_id: "/".into(),
                total_bytes: 500 * GIB,
                free_bytes: 41 * GIB,
                sampled_at: 0,
            }],
            not_sampled: vec![NotSampled {
                volume_id: "/media/usb".into(),
                reason: "not connected".into(),
            }],
            forecasts: vec![
                fr("/", ForecastState::Filling, Confidence::High, Some(9)),
                fr(
                    "/data",
                    ForecastState::NotEnoughHistory,
                    Confidence::Unknown,
                    None,
                ),
                fr("/x", ForecastState::Filling, Confidence::Low, Some(1)),
            ],
            alerts: vec![Alert::new(
                scull_proactive::AlertKind::DiskFullForecast,
                Severity::Warning,
                "/",
                "/ will drop below 10.0 GiB free in about 9 days (high confidence)",
                0,
            )],
        };
        let text = render(&r);
        assert_eq!(
            text,
            "scull check: sampled 1 volume\n\
             \x20 /      41.0 GiB free \u{b7} below 10.0 GiB free in ~9 days (high confidence)\n\
             \x20 /data  41.0 GiB free \u{b7} not enough history for a forecast (3 samples over 1 day)\n\
             \x20 /x     41.0 GiB free \u{b7} below 10.0 GiB free in ~1 day (low confidence, not alerting)\n\
             \x20 /media/usb: not sampled (not connected)\n\
             warning: / will drop below 10.0 GiB free in about 9 days (high confidence)\n"
        );
        let empty = CheckReport {
            now: 0,
            sampled: vec![],
            not_sampled: vec![],
            forecasts: vec![],
            alerts: vec![],
        };
        assert!(render(&empty).contains("no volumes to check yet"));
        assert_eq!(span(30), "<1 hour");
        assert_eq!(span(7_200), "2 hours");
    }

    #[test]
    fn sampling_reports_disconnected_volumes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("not-mounted-here");
        let (samples, missing) = sample_volumes(
            &[gone.to_string_lossy().into_owned()],
            &[dir.path().to_path_buf()],
            42,
        );
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].reason, "not connected");
        // The temp dir's volume is sampled through the root (when the OS reports sizes).
        for s in &samples {
            assert_eq!(s.at, 42);
            assert!(s.free <= s.total);
        }
    }
}
