//! `scull status [--short]` (P1-CLI-04, PRO-09): one cached line for shell prompts, tmux or
//! scripts.
//!
//! Reads **only** the database: one indexed query ([`scull_storage::Store::status_summary`])
//! plus, per sampled volume, one indexed range query over its last 30 days of free-space
//! samples for the disk-full forecast ([`scull_proactive::forecast_all`], PRO-01). No scan, no
//! `git`, no network. The forecast uses the configured policy (`[proactive]` in config.toml,
//! [`scull_proactive::forecast_policy`]), the one every surface shares; an unreadable config
//! falls back to the defaults so a prompt line never fails. It never creates or migrates the
//! database (see [`crate::db`]).
//!
//! * `scull status`:
//!   `scull: 18.4 GiB reclaimable (12.1 GiB safe) · 41.0 GiB free on / · last scan 2h ago`
//! * `scull status --short`: `☠ 41G free · 2h · 18G reclaimable` (free space of the fullest
//!   volume sampled, age of the data, reclaimable bytes), meant to be embedded in a prompt.
//! * When a volume is forecast to drop below its reserve with at least medium confidence, both
//!   lines add it: `… · 41.0 GiB free on / · full in ~9 days · last scan 2h ago` and
//!   `☠ 41G free · full ~9d · 2h · 18G reclaimable` (the volume is named when it is not the
//!   fullest one: `/data full in ~9 days`, `/data full ~9d`).
//! * When `scull check` has prepared a ready-to-review cleanup plan that has not expired
//!   (PRO-05), the human line ends with it: `… · 12.0 GiB cleanup plan ready to review:
//!   scull clean --plan <file>` (one more indexed query; the prompt line is unchanged).
//! * `--json` / `--ndjson`: `scull/status@1` (with every volume's forecast in `forecasts`).
//!
//! **Prompt contract:** `--short` always exits 0 and prints nothing at all (not even a newline)
//! when there is nothing to show: no scan recorded yet, database missing, written by a newer
//! scull, not yet migrated, damaged, or scull's directories can't be located. A prompt never
//! shows error noise. Without `--short` the same situations print one explanatory line, still
//! with exit 0.

use super::forecast::{ForecastReport, headline};
use super::review_plan::ReviewPlanReport;
use super::{emit_machine, emit_text};
use crate::db::{self, NotOpened, Probe};
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use schemars::JsonSchema;
use scull_core::ScullError;
use scull_storage::{ScanStatus, StatusSummary, StoreError};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// Schema of `scull status --json`.
pub const STATUS_SCHEMA: SchemaId = SchemaId::new("status", 1);

/// The marker that starts the short line (PRO-09).
pub const SHORT_MARKER: &str = "\u{2620}";

/// Whether there is something to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    /// A usable scan is recorded.
    Ok,
    /// The database exists but holds no completed or partial scan yet.
    NoScan,
    /// No database yet (nothing was ever scanned).
    NoDatabase,
    /// The database can't be read by this command right now (newer schema, pending
    /// migration, damaged, not a scull database, or the directories can't be located).
    Unavailable,
}

/// `scull status` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    /// Whether there is something to show.
    pub state: StatusState,
    /// Why the status is unavailable, when it is.
    pub detail: Option<String>,
    /// When this report was produced (Unix seconds).
    pub now: i64,
    /// The latest completed or partial scan.
    pub last_scan: Option<LastScanReport>,
    /// Latest free-space sample of every volume ever sampled.
    pub volumes: Vec<VolumeReport>,
    /// Disk-full forecast of every volume ever sampled (PRO-01, D-22), by volume id.
    pub forecasts: Vec<ForecastReport>,
    /// The human line (`scull status`), empty when there is nothing to show.
    pub line: String,
    /// The prompt line (`scull status --short`), empty when there is nothing to show.
    pub short: String,
    /// The ready-to-review cleanup plan `scull check` prepared (PRO-05), while it is current.
    /// The human line mentions it; the prompt line does not.
    pub review_plan: Option<ReviewPlanReport>,
}

/// The latest usable scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LastScanReport {
    /// Scan identifier.
    pub scan_id: String,
    /// Start time (Unix seconds).
    pub started_at: i64,
    /// Finish time (Unix seconds).
    pub finished_at: i64,
    /// `completed` or `partial`.
    pub status: String,
    /// Seconds between the finish and `now` (0 if the clock went backwards).
    pub age_secs: u64,
    /// Candidates found.
    pub candidates: u64,
    /// Reclaimable bytes of all candidates.
    pub reclaimable_bytes: u64,
    /// Reclaimable bytes of S0 (regenerable) candidates.
    pub safe_reclaimable_bytes: u64,
}

/// One volume's latest free-space sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeReport {
    /// Volume identifier (mount point or volume GUID path).
    pub volume_id: String,
    /// Volume size in bytes.
    pub total_bytes: u64,
    /// Bytes available to the user.
    pub free_bytes: u64,
    /// When it was sampled (Unix seconds).
    pub sampled_at: i64,
}

/// `scull status [--short]`.
pub fn run(ui: &Ui, short: bool) -> Result<(), ScullError> {
    let report = collect(now());
    if ui.format.is_machine() {
        return emit_machine(ui.format, STATUS_SCHEMA, "status", &report);
    }
    let text = if short { &report.short } else { &report.line };
    if text.is_empty() {
        return Ok(());
    }
    // Volume IDs come from the database: escape before they reach the terminal (SAFE-17).
    emit_text(&format!("{}\n", escape_for_terminal(text)))
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The forecast policy from the user config (the one `scull check` uses), or the defaults when
/// the config can't be read: the status line never fails on it.
fn configured_forecast_policy() -> scull_proactive::ForecastPolicy {
    super::pipeline::load_config(&[])
        .map(|l| scull_proactive::forecast_policy(&l.config.proactive))
        .unwrap_or_default()
}

/// Read the status from the database in scull's data directory.
pub fn collect(now: i64) -> StatusReport {
    let data_dir = match scull_platform::app_dirs() {
        Ok(d) => d.data,
        Err(e) => return unavailable(now, StatusState::Unavailable, Some(e.to_string())),
    };
    let path = db::db_path(&data_dir);
    match db::open_current(&path) {
        Ok(store) => match store.status_summary() {
            Ok(summary) => {
                // A forecast that can't be read never hides the rest of the status.
                let forecasts =
                    scull_proactive::forecast_all(&store, now, &configured_forecast_policy())
                        .unwrap_or_default();
                let mut r = from_parts(&summary, &forecasts, now);
                if let Some(p) = super::review_plan::current(&store, now) {
                    add_review_plan(&mut r, p);
                }
                r
            }
            Err(e) => unavailable(now, StatusState::Unavailable, Some(e.to_string())),
        },
        Err(NotOpened::Probe(Probe::Missing)) => unavailable(now, StatusState::NoDatabase, None),
        Err(NotOpened::Probe(Probe::Uninitialized)) => unavailable(now, StatusState::NoScan, None),
        Err(NotOpened::Probe(p)) => {
            let why = match p {
                Probe::Scull { version } if version > scull_storage::SCHEMA_VERSION => {
                    "the database was written by a newer scull".to_owned()
                }
                Probe::Scull { .. } => {
                    "the database needs a migration; run any other scull command".to_owned()
                }
                Probe::NotAFile | Probe::NotSqlite | Probe::Foreign { .. } => {
                    "the database file is not a scull database; see `scull doctor`".to_owned()
                }
                Probe::Missing | Probe::Uninitialized => String::new(),
            };
            unavailable(now, StatusState::Unavailable, Some(why))
        }
        Err(NotOpened::Io(e)) => unavailable(now, StatusState::Unavailable, Some(e.to_string())),
        Err(NotOpened::Store(e)) => {
            let why = match e {
                StoreError::Corrupt { .. } => "the database is damaged; see `scull doctor`".into(),
                other => other.to_string(),
            };
            unavailable(now, StatusState::Unavailable, Some(why))
        }
    }
}

fn unavailable(now: i64, state: StatusState, detail: Option<String>) -> StatusReport {
    let line = match state {
        StatusState::NoDatabase | StatusState::NoScan => {
            "scull: no scan recorded yet; run `scull scan <path>`".to_owned()
        }
        _ => format!(
            "scull: status unavailable: {}",
            detail.as_deref().unwrap_or("unknown reason")
        ),
    };
    StatusReport {
        state,
        detail,
        now,
        last_scan: None,
        volumes: Vec::new(),
        forecasts: Vec::new(),
        line,
        short: String::new(),
        review_plan: None,
    }
}

/// Build the report from a store summary without forecasts (pure; `now` is injected).
pub fn from_summary(s: &StatusSummary, now: i64) -> StatusReport {
    from_parts(s, &[], now)
}

/// Build the report from a store summary and the volumes' forecasts (pure; `now` is
/// injected for tests).
pub fn from_parts(
    s: &StatusSummary,
    forecasts: &[scull_proactive::Forecast],
    now: i64,
) -> StatusReport {
    let forecasts: Vec<ForecastReport> = forecasts.iter().map(ForecastReport::from).collect();
    let volumes: Vec<VolumeReport> = s
        .volumes
        .iter()
        .map(|v| VolumeReport {
            volume_id: v.volume_id.clone(),
            total_bytes: v.total,
            free_bytes: v.free,
            sampled_at: v.at,
        })
        .collect();
    let Some(scan) = &s.last_scan else {
        let mut r = unavailable(now, StatusState::NoScan, None);
        r.volumes = volumes;
        r.forecasts = forecasts;
        return r;
    };
    let age_secs = u64::try_from(now.saturating_sub(scan.finished_at)).unwrap_or(0);
    let last = LastScanReport {
        scan_id: scan.scan_id.as_str().to_owned(),
        started_at: scan.started_at,
        finished_at: scan.finished_at,
        status: match scan.status {
            ScanStatus::Completed => "completed",
            ScanStatus::Partial => "partial",
            ScanStatus::Failed => "failed",
            ScanStatus::Cancelled => "cancelled",
            _ => "unknown",
        }
        .to_owned(),
        age_secs,
        candidates: scan.candidates,
        reclaimable_bytes: scan.reclaimable_bytes,
        safe_reclaimable_bytes: scan.safe_reclaimable_bytes,
    };
    // The fullest volume is the one worth a prompt's attention; the soonest-filling
    // forecast (medium confidence or better) is added next to it.
    let fullest = volumes.iter().min_by_key(|v| v.free_bytes);
    let soonest = headline(&forecasts).and_then(|f| {
        let days = f.days_text()?;
        let named = fullest.is_none_or(|v| v.volume_id != f.volume_id);
        Some((
            named.then(|| f.volume_id.clone()),
            days,
            f.days_until_threshold,
        ))
    });

    let mut line = format!(
        "scull: {} reclaimable ({} safe)",
        format_bytes(last.reclaimable_bytes),
        format_bytes(last.safe_reclaimable_bytes)
    );
    if let Some(v) = fullest {
        line.push_str(&format!(
            " \u{b7} {} free on {}",
            format_bytes(v.free_bytes),
            v.volume_id
        ));
    }
    if let Some((named, days, _)) = &soonest {
        match named {
            Some(id) => line.push_str(&format!(" \u{b7} {id} full in {days}")),
            None => line.push_str(&format!(" \u{b7} full in {days}")),
        }
    }
    line.push_str(&format!(" \u{b7} last scan {} ago", age(age_secs)));
    if last.status == "partial" {
        line.push_str(" (partial)");
    }

    let mut short = SHORT_MARKER.to_owned();
    if let Some(v) = fullest {
        short.push_str(&format!(" {} free \u{b7}", compact_bytes(v.free_bytes)));
    }
    if let Some((named, _, Some(d))) = &soonest {
        match named {
            Some(id) => short.push_str(&format!(" {id} full ~{d}d \u{b7}")),
            None => short.push_str(&format!(" full ~{d}d \u{b7}")),
        }
    }
    short.push_str(&format!(
        " {} \u{b7} {} reclaimable",
        age(age_secs),
        compact_bytes(last.reclaimable_bytes)
    ));

    StatusReport {
        state: StatusState::Ok,
        detail: None,
        now,
        last_scan: Some(last),
        volumes,
        forecasts,
        line,
        short,
        review_plan: None,
    }
}

/// Mention a current review plan in the human line (only when a scan is shown).
pub fn add_review_plan(r: &mut StatusReport, p: ReviewPlanReport) {
    if r.state == StatusState::Ok {
        r.line.push_str(&format!(
            " \u{b7} {} cleanup plan ready to review: {}",
            format_bytes(p.reclaimable_bytes),
            p.review_command
        ));
    }
    r.review_plan = Some(p);
}

/// A duration as its largest unit: `<1m`, `5m`, `2h`, `9d`.
pub fn age(secs: u64) -> String {
    match secs {
        0..60 => "<1m".to_owned(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A byte count for a prompt, binary units with a one-letter suffix: `0B`, `512K`, `3.2G`,
/// `18G`. One decimal below 10, none above.
pub fn compact_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["K", "M", "G", "T", "P", "E"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1023.5 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 9.95 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::ScanId;
    use scull_storage::{LastScan, VolumeSample};

    const GIB: u64 = 1024 * 1024 * 1024;

    fn summary(volumes: Vec<VolumeSample>) -> StatusSummary {
        StatusSummary {
            last_scan: Some(LastScan {
                scan_id: ScanId::from_suffix("a1").expect("id"),
                started_at: 1_000,
                finished_at: 1_100,
                status: ScanStatus::Completed,
                candidates: 3,
                reclaimable_bytes: 18 * GIB + GIB / 2,
                safe_reclaimable_bytes: 12 * GIB,
            }),
            volumes,
        }
    }

    #[test]
    fn lines_show_reclaimable_free_and_age() {
        let vols = vec![
            VolumeSample {
                volume_id: "/".into(),
                total: 500 * GIB,
                free: 41 * GIB,
                at: 1_100,
            },
            VolumeSample {
                volume_id: "/data".into(),
                total: 900 * GIB,
                free: 400 * GIB,
                at: 1_100,
            },
        ];
        let r = from_summary(&summary(vols), 1_100 + 2 * 3_600 + 5);
        assert_eq!(r.state, StatusState::Ok);
        assert_eq!(
            r.line,
            "scull: 18.5 GiB reclaimable (12.0 GiB safe) \u{b7} 41.0 GiB free on / \u{b7} last scan 2h ago"
        );
        assert_eq!(
            r.short,
            "\u{2620} 41G free \u{b7} 2h \u{b7} 18G reclaimable"
        );
        assert_eq!(r.last_scan.as_ref().expect("scan").age_secs, 7_205);
    }

    #[test]
    fn without_volumes_or_scans() {
        let r = from_summary(&summary(Vec::new()), 1_000);
        assert_eq!(r.short, "\u{2620} <1m \u{b7} 18G reclaimable");
        assert_eq!(r.last_scan.expect("scan").age_secs, 0, "clock skew clamps");
        let empty = from_summary(&StatusSummary::default(), 5);
        assert_eq!(empty.state, StatusState::NoScan);
        assert!(empty.short.is_empty());
        assert!(empty.line.contains("no scan recorded yet"));
    }

    /// Daily samples of `volume` over 20 days ending at `end`, losing `per_day` GiB a day
    /// down to `free_now` GiB.
    fn filling(volume: &str, end: i64, free_now: u64, per_day: u64) -> Vec<VolumeSample> {
        (0..=20)
            .map(|i| VolumeSample {
                volume_id: volume.into(),
                total: 500 * GIB,
                free: (free_now + per_day * (20 - i)) * GIB,
                at: end - i64::try_from(20 - i).expect("i") * 86_400,
            })
            .collect()
    }

    #[test]
    fn lines_show_a_confident_forecast() {
        let now = 1_100 + 2 * 3_600;
        let policy = scull_proactive::ForecastPolicy::default();
        // 46 GiB free on /, losing 4 GiB a day: (46 - 10) / 4 = 9 days.
        let root = filling("/", 1_100, 46, 4);
        let f_root = scull_proactive::forecast("/", &root, &[], now, &policy);
        let vols = vec![root.last().expect("sample").clone()];
        let r = from_parts(&summary(vols), std::slice::from_ref(&f_root), now);
        assert_eq!(
            r.line,
            "scull: 18.5 GiB reclaimable (12.0 GiB safe) \u{b7} 46.0 GiB free on / \u{b7} full in ~9 days \u{b7} last scan 2h ago"
        );
        assert_eq!(
            r.short,
            "\u{2620} 46G free \u{b7} full ~9d \u{b7} 2h \u{b7} 18G reclaimable"
        );
        assert_eq!(r.forecasts.len(), 1);
        assert_eq!(r.forecasts[0].days_until_threshold, Some(9));

        // The forecast is on another volume than the fullest one: it is named.
        let data = filling("/data", 1_100, 60, 5);
        let f_data = scull_proactive::forecast("/data", &data, &[], now, &policy);
        let flat = VolumeSample {
            volume_id: "/".into(),
            total: 500 * GIB,
            free: 41 * GIB,
            at: 1_100,
        };
        let vols = vec![flat, data.last().expect("sample").clone()];
        let r = from_parts(&summary(vols), &[f_data], now);
        assert!(
            r.line
                .contains("41.0 GiB free on / \u{b7} /data full in ~10 days"),
            "{}",
            r.line
        );
        assert!(
            r.short.contains("41G free \u{b7} /data full ~10d \u{b7}"),
            "{}",
            r.short
        );

        // Not enough history: nothing added.
        let short = &filling("/", 1_100, 46, 4)[18..];
        let f = scull_proactive::forecast("/", short, &[], now, &policy);
        let r = from_parts(&summary(vec![short[2].clone()]), &[f], now);
        assert!(!r.line.contains("full in"), "{}", r.line);
        assert_eq!(
            r.forecasts[0].state,
            scull_proactive::ForecastState::NotEnoughHistory
        );
    }

    #[test]
    fn compact_units() {
        assert_eq!(compact_bytes(0), "0B");
        assert_eq!(compact_bytes(1023), "1023B");
        assert_eq!(compact_bytes(1024), "1.0K");
        assert_eq!(compact_bytes(3 * GIB + GIB / 5), "3.2G");
        assert_eq!(compact_bytes(18 * GIB), "18G");
        assert_eq!(compact_bytes(1023 * 1024 * 1024), "1023M");
        assert_eq!(compact_bytes(1024 * 1024 * 1024 - 1), "1.0G");
        assert_eq!(age(59), "<1m");
        assert_eq!(age(60), "1m");
        assert_eq!(age(3_600 * 23), "23h");
        assert_eq!(age(86_400 * 9 + 5), "9d");
    }
}
