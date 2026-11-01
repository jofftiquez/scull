//! `scull check` (PRD §12, PRO-01..04, PRO-07): scan the configured roots, sample free space,
//! forecast every drive, raise alerts. Meant to be run periodically by the OS scheduler.
//!
//! 1. **Scan** the configured `scan.roots` (the same read-only scan as `scull scan`, recorded
//!    in history), unless `--no-scan` is given or no roots are configured. Roots that
//!    `scull scan` would refuse without a flag (a filesystem root, the home folder, a network
//!    volume; CFG-04, D-22) are not scanned unattended: the check notes why and uses recorded
//!    history instead.
//! 2. **Sample.** Free space of every volume scull has sampled before (D-22: each drive its
//!    own forecast; a drive that is not connected right now is listed as not sampled) and of
//!    the volumes holding the configured scan roots. The samples are recorded in the history
//!    database like a scan's (INS-06: "every scan and every scheduled check"); a volume the
//!    scan just sampled is not recorded twice.
//! 3. **Forecast** each volume from its last 30 days of samples ([`scull_proactive::forecast()`]).
//! 4. **Alert** ([`scull_proactive::check_alerts`]): volumes below their reserve or forecast to
//!    drop below it within `proactive.alert_days` (PRO-01); budgets (PRO-03; ecosystem and
//!    path budgets need the scan); stale worktrees, build outputs and caches (PRO-02; need the
//!    scan); category growth spikes over scans of the same roots, naming the largest items,
//!    and free-space drops (PRO-04). Thresholds come from `[proactive]` in the user config.
//!    Every alert raised is recorded in the history database's alert stream (INS-06).
//!
//! 5. **Prepare** the ready-to-review reclaim plan from the scan (PRO-05, D-28: S0 items, S1
//!    too with `--include-s1`): a CLI plan recorded in the database and written to scull's
//!    data directory, announced as "A 12 GB cleanup plan is ready to review: scull clean
//!    --plan …" (see [`super::review_plan`]). Without a scan, the current plan is shown.
//!
//! Writes nothing but history rows, the plan record and the plan file, and never deletes
//! anything: an alert points to a reviewed cleanup, it never performs one, and the plan is a
//! proposal that only `scull clean --plan` (with revalidation and confirmation) could run.
//! Exit code 0 whether or not alerts fired (alerts are data).
//!
//! Not in this command yet: registering it with the OS scheduler and running at low priority
//! (`scull schedule enable|disable`, #114).

use super::forecast::ForecastReport;
use super::pipeline::{self, CancelBridge};
use super::review_plan::{self, ReviewPlanReport};
use super::status::VolumeReport;
use super::{emit_machine, emit_text};
use crate::cancel;
use crate::cli::CheckArgs;
use crate::cmd::pipeline::load_config;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use schemars::JsonSchema;
use scull_core::{ErrorKind, FsPath, ScanId, ScullError};
use scull_engine::{ScanReport, ScanRequest};
use scull_proactive::{Alert, Confidence, ForecastState, ScanInput, Severity};
use scull_storage::{AlertEvent, VolumeSample};
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
    /// What was scanned.
    pub scan: CheckScan,
    /// Free space sampled now (and recorded in history).
    pub sampled: Vec<VolumeReport>,
    /// Volumes known from history that could not be sampled now.
    pub not_sampled: Vec<NotSampled>,
    /// Disk-full forecast of every volume ever sampled, by volume id.
    pub forecasts: Vec<ForecastReport>,
    /// Alerts raised, most severe first.
    pub alerts: Vec<Alert>,
    /// The ready-to-review reclaim plan (PRO-05), when one is current. Never executed.
    pub review_plan: Option<ReviewPlanReport>,
}

/// The scan a check ran (or why it did not).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CheckScan {
    /// No `scan.roots` configured: only history is used.
    NotConfigured,
    /// `--no-scan`: only history is used.
    Disabled,
    /// The roots were not scanned; recorded history is used instead.
    Skipped {
        /// Why (e.g. a broad root that needs `scull scan --allow-broad-roots`).
        reason: String,
    },
    /// The roots were scanned (and the scan recorded in history).
    Scanned {
        /// The scan's ID.
        scan_id: ScanId,
        /// The roots scanned.
        roots: Vec<FsPath>,
        /// Candidates found.
        candidates: u64,
        /// Entries counted.
        entries_scanned: u64,
        /// Allocated bytes of everything found.
        allocated_bytes: u64,
    },
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
pub fn run(args: &CheckArgs, ui: &Ui) -> Result<(), ScullError> {
    let loaded = load_config(&[])?;
    let (scan_state, scanned) = light_scan(&loaded, args, ui)?;
    let now = scull_engine::now_unix();
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
    // The scan recorded the free space of the volumes it covered: don't sample them twice.
    let mut to_record = Vec::new();
    for s in &samples {
        let by_scan = match &scanned {
            Some(r) => !store
                .free_space_history(&s.volume_id, r.started_at)
                .map_err(store_error)?
                .is_empty(),
            None => false,
        };
        if !by_scan {
            to_record.push(s.clone());
        }
    }
    if !to_record.is_empty() {
        store
            .record_free_space(&to_record, None)
            .map_err(store_error)?;
    }
    let cfg = &loaded.config.proactive;
    let policy = scull_proactive::forecast_policy(cfg);
    let forecasts = scull_proactive::forecast_all(&store, now, &policy).map_err(store_error)?;
    let input = scanned.as_ref().map(|r| ScanInput {
        roots: &r.roots,
        candidates: &r.candidates,
    });
    // Worktree TTLs registered by agents and scripts (PRO-08), for worktrees that still exist.
    let ttls: Vec<(String, i64)> =
        match scull_mcp::registry::Registry::in_data_dir(&dirs.data).load() {
            Ok(regs) => regs
                .into_iter()
                .filter(|r| Path::new(&r.path).is_dir())
                .map(|r| (r.path, r.expires_at))
                .collect(),
            Err(e) => {
                pipeline::warn(ui, &format!("worktree TTLs could not be read: {e}"));
                Vec::new()
            }
        };
    let alerts = scull_proactive::check_alerts(&store, cfg, &forecasts, input, &ttls, now)
        .map_err(store_error)?;
    if let Err(e) = store.record_alerts(&alert_events(&alerts)) {
        pipeline::warn(ui, &format!("alerts were not recorded in history: {e}"));
    }
    let review_plan = match &scanned {
        Some(r) => match review_plan::Binding::current(&dirs.data).and_then(|binding| {
            review_plan::refresh(
                &mut store,
                &dirs.data,
                &r.scan_id,
                &r.candidates,
                args.include_s1,
                &binding,
                now,
            )
        }) {
            Ok(p) => p,
            Err(e) => {
                pipeline::warn(ui, &format!("no cleanup plan was prepared: {}", e.message));
                None
            }
        },
        None => review_plan::current(&store, now),
    };
    let report = CheckReport {
        now,
        scan: scan_state,
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
        review_plan,
    };
    if ui.format.is_machine() {
        return emit_machine(ui.format, CHECK_SCHEMA, "check", &report);
    }
    // Volume IDs and paths come from the database and the OS: escape them, line by line
    // (SAFE-17).
    let text: String = render(&report)
        .lines()
        .map(|l| escape_for_terminal(l) + "\n")
        .collect();
    emit_text(&text)
}

/// The alerts as rows of the history database's alert stream.
pub fn alert_events(alerts: &[Alert]) -> Vec<AlertEvent> {
    alerts
        .iter()
        .filter_map(|a| {
            Some(AlertEvent {
                event_id: 0,
                at: a.raised_at,
                source: "check".to_owned(),
                key: a.key.clone(),
                alert: serde_json::to_value(a).ok()?,
            })
        })
        .collect()
}

/// Scan the configured roots for the check (step 1 in the module docs). A root `scull scan`
/// would refuse without a flag, or a scan that fails, is reported as skipped; Ctrl-C stops the
/// check.
fn light_scan(
    loaded: &pipeline::Loaded,
    args: &CheckArgs,
    ui: &Ui,
) -> Result<(CheckScan, Option<ScanReport>), ScullError> {
    if loaded.config.scan.roots.is_empty() {
        return Ok((CheckScan::NotConfigured, None));
    }
    if args.no_scan {
        return Ok((CheckScan::Disabled, None));
    }
    let skipped = |e: ScullError| CheckScan::Skipped { reason: e.message };
    let roots = match pipeline::roots(loaded, false) {
        Ok(r) => r,
        Err(e) => return Ok((skipped(e), None)),
    };
    let engine = match pipeline::engine(loaded, &roots, true, ui) {
        Ok(e) => e,
        Err(e) => return Ok((skipped(e), None)),
    };
    let bridge = CancelBridge::start();
    let mut req = ScanRequest::full().history(true);
    req.threads = loaded.config.scan.threads as usize;
    let result = engine.scan(&roots, &req, bridge.token(), pipeline::progress(ui));
    drop(bridge);
    drop(engine);
    let report = match result {
        Ok(r) => r,
        Err(e) => return Ok((skipped(e), None)),
    };
    if report.cancelled || cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    let state = CheckScan::Scanned {
        scan_id: report.scan_id.clone(),
        roots: report.roots.clone(),
        candidates: report.candidates.len() as u64,
        entries_scanned: report.entries_scanned,
        allocated_bytes: report.totals.allocated.total(),
    };
    Ok((state, Some(report)))
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
    match &r.scan {
        CheckScan::NotConfigured | CheckScan::Disabled => {}
        CheckScan::Skipped { reason } => {
            out.push_str(&format!("scan skipped: {reason}\n"));
        }
        CheckScan::Scanned {
            roots,
            candidates,
            allocated_bytes,
            ..
        } => {
            let roots: Vec<&str> = roots.iter().map(FsPath::as_str).collect();
            out.push_str(&format!(
                "scanned {}: {candidates} item{} ({})\n",
                roots.join(", "),
                if *candidates == 1 { "" } else { "s" },
                format_bytes(*allocated_bytes)
            ));
        }
    }
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
        for p in &a.paths {
            out.push_str(&format!("    {p}\n"));
        }
    }
    if r.alerts.is_empty() {
        out.push_str("no alerts\n");
    }
    if let Some(p) = &r.review_plan {
        out.push_str(&review_plan_line(p));
        out.push('\n');
    }
    out
}

/// `A 12.0 GiB cleanup plan is ready to review: scull clean --plan … (3 items, nothing was
/// removed)`.
pub fn review_plan_line(p: &ReviewPlanReport) -> String {
    format!(
        "A {} cleanup plan is ready to review: {} ({} item{}{}, nothing was removed)",
        format_bytes(p.reclaimable_bytes),
        p.review_command,
        p.items,
        if p.items == 1 { "" } else { "s" },
        if p.includes_s1 { ", S0 and S1" } else { ", S0" }
    )
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
            scan: CheckScan::NotConfigured,
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
            review_plan: None,
        };
        let text = render(&r);
        let mut with_plan = r.clone();
        with_plan.review_plan = Some(ReviewPlanReport {
            plan_id: "plan_x".into(),
            file: "/d/review-plan.json".into(),
            items: 3,
            reclaimable_bytes: 12 * GIB,
            created_at: 0,
            expires_at: 86_400,
            includes_s1: false,
            review_command: "scull clean --plan /d/review-plan.json".into(),
            reused: false,
        });
        assert!(
            render(&with_plan).ends_with(
                "A 12.0 GiB cleanup plan is ready to review: scull clean --plan \
                 /d/review-plan.json (3 items, S0, nothing was removed)\n"
            ),
            "{}",
            render(&with_plan)
        );
        assert_eq!(
            text,
            "scull check: sampled 1 volume\n\
             \x20 /      41.0 GiB free \u{b7} below 10.0 GiB free in ~9 days (high confidence)\n\
             \x20 /data  41.0 GiB free \u{b7} not enough history for a forecast (3 samples over 1 day)\n\
             \x20 /x     41.0 GiB free \u{b7} below 10.0 GiB free in ~1 day (low confidence, not alerting)\n\
             \x20 /media/usb: not sampled (not connected)\n\
             warning: / will drop below 10.0 GiB free in about 9 days (high confidence)\n"
        );
        let mut empty = CheckReport {
            now: 0,
            scan: CheckScan::Disabled,
            sampled: vec![],
            not_sampled: vec![],
            forecasts: vec![],
            alerts: vec![],
            review_plan: None,
        };
        assert!(render(&empty).contains("no volumes to check yet"));
        empty.scan = CheckScan::Skipped {
            reason: "/ is a filesystem root".into(),
        };
        assert!(render(&empty).starts_with("scan skipped: / is a filesystem root\n"));
        empty.scan = CheckScan::Scanned {
            scan_id: ScanId::from_suffix("x1").expect("id"),
            roots: vec![FsPath::from("/p")],
            candidates: 1,
            entries_scanned: 9,
            allocated_bytes: 2 * GIB,
        };
        assert!(render(&empty).starts_with("scanned /p: 1 item (2.0 GiB)\n"));
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
