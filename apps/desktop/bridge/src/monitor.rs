//! Tray monitoring and the weekly digest (PRD §14.6, INS-04; P5-PRO-01).
//!
//! The tray app (`apps/desktop/src-tauri/src/monitor.rs`) owns one [`Monitor`] and calls
//! [`Monitor::tick`] from a background thread; everything it decides is here, so it is tested
//! on every OS with a fake clock and a temp database.
//!
//! **Reads and notifies, nothing else.** A check reads free space (the platform's volume
//! queries), the free-space history and the alerts `scull check` recorded, then decides which
//! notifications to show ([`scull_proactive::monitor`]). It never scans, never writes the
//! history database (it is not even created when missing), never prepares or runs a plan.
//! A low-space notification points at Cleanup Review, where every cleanup is still reviewed.
//!
//! What it reads per check:
//!
//! * **Live free space** of every volume the history knows plus the home folder's
//!   ([`live_samples`]), forecast together with the recorded samples (PRO-01) into low-space
//!   and disk-full alerts, so the tray warns even between scheduled checks.
//! * **Recorded alerts** from the last day ([`crate::views::RECENT_ALERTS_SECS`]): staleness,
//!   budgets, spikes, TTLs (PRO-02/03/04/08). Recorded forecast alerts of a volume measured
//!   live are superseded by the live ones.
//!
//! Quiet by default: warnings and worse, at most once a day per alert, one notification per
//! check, the weekly digest off until the user turns it on. Settings and memory live in the
//! app (the main window keeps them in its local storage and sends them with
//! `configure_monitor`); nothing here writes a file. No network, no telemetry.

use crate::insights::Source;
use crate::session::Session;
use crate::views::RECENT_ALERTS_SECS;
use schemars::JsonSchema;
use scull_insights::digest::{WeeklyDigest, read_weekly_digest};
use scull_proactive::alert::format_bytes;
use scull_proactive::alert::{Alert, AlertKind, Severity, forecast_alerts, sort_alerts};
use scull_proactive::forecast::forecast;
use scull_proactive::monitor::{
    self as rules, digest_due, mark_digest_sent, next_check, next_digest,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

pub use crate::insights::HistoryRead;
pub use scull_proactive::monitor::{DigestSchedule, MonitorMemory, MonitorSettings, Notification};
pub use scull_storage::VolumeSample;

/// The event the tray app emits to the main window after the monitor's settings or memory
/// changed (payload: [`MonitorSnapshot`]), so the window can keep them.
pub const MONITOR_EVENT: &str = "scull://monitor";

/// Before the main window sends its settings, the first check waits this long (seconds), so
/// a restart doesn't repeat notifications the previous run already showed.
pub const CONFIGURE_GRACE_SECS: i64 = 90;

/// The longest pause the tray menu offers: one hour; Settings offers more.
pub const PAUSE_HOUR_SECS: i64 = 60 * 60;

/// `weekly_digest`: the digest of the last 7 days, read from the history database.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DigestView {
    /// Whether the database answered.
    pub state: HistoryRead,
    /// Why it didn't, when it didn't.
    pub detail: Option<String>,
    /// The digest (also without a database: it then says there is no history).
    pub digest: Option<WeeklyDigest>,
}

/// `configure_monitor`'s argument: the user's settings and, on the first call after start,
/// the memory the app kept from its previous run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorConfig {
    /// Settings (partial objects take defaults; values are clamped).
    #[serde(default)]
    pub settings: MonitorSettings,
    /// Memory from the previous run; ignored after the first `configure_monitor`, so a
    /// settings change never rewinds what was already notified.
    #[serde(default)]
    pub memory: Option<MonitorMemory>,
}

/// What the main window keeps between runs ([`MONITOR_EVENT`]'s payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorSnapshot {
    /// Current settings (a pause from the tray menu changes them).
    pub settings: MonitorSettings,
    /// Current memory.
    pub memory: MonitorMemory,
}

/// `monitor_status`: what the monitor is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorStatus {
    /// Effective settings.
    pub settings: MonitorSettings,
    /// Whether the main window sent its settings yet.
    pub configured: bool,
    /// Whether notifications are paused now.
    pub paused: bool,
    /// When the last check ran.
    pub last_check: Option<i64>,
    /// When the next check is due.
    pub next_check: i64,
    /// When the last digest was sent.
    pub last_digest: Option<i64>,
    /// When the next digest goes out (`None`: the digest is off).
    pub next_digest: Option<i64>,
    /// Alerts currently remembered as notified.
    pub notified: u32,
}

/// What a check produced: at most one alert notification and the digest, when one was due.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TickOutcome {
    /// Whether a check ran (it was due, or forced).
    pub checked: bool,
    /// Notifications to show, in order (alerts first, then the digest).
    pub notifications: Vec<Notification>,
    /// The digest, when one was sent.
    pub digest: Option<WeeklyDigest>,
}

/// The tray's monitor: settings, memory, and whether the app configured it.
#[derive(Debug, Clone)]
pub struct Monitor {
    settings: MonitorSettings,
    memory: MonitorMemory,
    configured: bool,
    started_at: i64,
}

impl Monitor {
    /// A monitor started at `now`, with default settings, waiting for `configure_monitor`.
    pub fn new(now: i64) -> Self {
        Self {
            settings: MonitorSettings::default(),
            memory: MonitorMemory::default(),
            configured: false,
            started_at: now,
        }
    }

    /// Apply the app's settings (clamped) and, the first time, its saved memory. Turning the
    /// digest on is noted here, so its first slot is the next one from now.
    pub fn configure(&mut self, config: MonitorConfig, now: i64) {
        self.settings = config.settings.normalized(now);
        if !self.configured {
            if let Some(m) = config.memory {
                self.memory = m;
            }
            self.configured = true;
        }
        if !self.settings.digest.enabled || self.memory.last_digest.is_none() {
            // Only records (or forgets) when the digest was turned on; never sends.
            let _ = digest_due(&self.settings, &mut self.memory, now);
        }
    }

    /// Pause notifications for `secs` from `now` (at most 7 days), or resume with `None`.
    pub fn pause(&mut self, secs: Option<i64>, now: i64) {
        self.settings.paused_until = secs.map(|s| now.saturating_add(s.max(0)));
        self.settings = std::mem::take(&mut self.settings).normalized(now);
    }

    /// Current settings.
    pub fn settings(&self) -> &MonitorSettings {
        &self.settings
    }

    /// Current memory.
    pub fn memory(&self) -> &MonitorMemory {
        &self.memory
    }

    /// What the app should keep.
    pub fn snapshot(&self) -> MonitorSnapshot {
        MonitorSnapshot {
            settings: self.settings.clone(),
            memory: self.memory.clone(),
        }
    }

    /// The `monitor_status` payload at `now`.
    pub fn status(&self, now: i64) -> MonitorStatus {
        MonitorStatus {
            settings: self.settings.clone(),
            configured: self.configured,
            paused: self.settings.paused(now),
            last_check: self.memory.last_check,
            next_check: self.next_check(now),
            last_digest: self.memory.last_digest,
            next_digest: next_digest(&self.settings, &self.memory, now),
            notified: u32::try_from(self.memory.notified.len()).unwrap_or(u32::MAX),
        }
    }

    /// When the next check is due (after the configure grace period when not configured).
    pub fn next_check(&self, now: i64) -> i64 {
        let due = next_check(&self.settings, &self.memory, now);
        if self.configured {
            due
        } else {
            due.max(self.started_at.saturating_add(CONFIGURE_GRACE_SECS))
        }
    }

    /// Whether a check or the digest is due at `now`.
    pub fn due(&self, now: i64) -> bool {
        self.next_check(now) <= now || self.digest_pending(now)
    }

    fn digest_pending(&self, now: i64) -> bool {
        self.configured && next_digest(&self.settings, &self.memory, now).is_some_and(|t| t <= now)
    }

    /// Run a check when one is due (or `force`d, the tray's "Check now"): read alerts from
    /// `session` with the `live` free-space samples and decide the notification; and send the
    /// digest when its slot came (independently of the check interval). Read-only.
    pub fn tick(
        &mut self,
        session: &Session,
        now: i64,
        live: &[VolumeSample],
        force: bool,
    ) -> TickOutcome {
        let mut out = TickOutcome::default();
        let check = force || self.next_check(now) <= now;
        if !check && !self.digest_pending(now) {
            return out;
        }
        if check {
            out.checked = true;
            let alerts = session.monitor_alerts(now, &self.settings, live);
            if let Some(n) = rules::notifications(&alerts, &self.settings, &mut self.memory, now) {
                out.notifications.push(n);
            }
        }
        if self.configured && digest_due(&self.settings, &mut self.memory, now) {
            let view = session.weekly_digest(now);
            mark_digest_sent(&mut self.memory, now);
            // A week without history says nothing worth a notification.
            if let Some(d) = view.digest.filter(|d| d.has_history) {
                out.notifications.push(digest_notification(&d));
                out.digest = Some(d);
            }
        }
        out
    }

    /// The digest notification right now (Settings' "Send now"), whatever the schedule.
    /// Doesn't move the weekly slot.
    pub fn digest_now(&self, session: &Session, now: i64) -> (DigestView, Option<Notification>) {
        let view = session.weekly_digest(now);
        let n = view.digest.as_ref().map(digest_notification);
        (view, n)
    }
}

/// The notification for a digest: its title and first three lines, opening Insights.
pub fn digest_notification(d: &WeeklyDigest) -> Notification {
    Notification {
        title: d.title.clone(),
        body: rules::plain(&d.body()),
        severity: Severity::Info,
        alert_keys: Vec::new(),
        route: "/insights".to_owned(),
    }
}

/// The tray icon's tooltip after a check: `scull · 41.0 GiB free` for the home folder's volume
/// (else the first one measured), `scull` when nothing was measured.
pub fn tooltip(home_volume: Option<&str>, live: &[VolumeSample]) -> String {
    let pick = live
        .iter()
        .find(|s| Some(s.volume_id.as_str()) == home_volume)
        .or(live.first());
    match pick {
        Some(s) => format!("scull · {} free", format_bytes(s.free)),
        None => "scull".to_owned(),
    }
}

/// Free space right now of every volume in `known` (volume IDs: mount points) and the volume
/// holding `home`, through the platform's volume queries. Volumes that aren't mounted or
/// report no size are skipped. Read-only.
pub fn live_samples(known: &[String], home: Option<&Path>, now: i64) -> Vec<VolumeSample> {
    let mut out: Vec<VolumeSample> = Vec::new();
    let mut paths: Vec<&Path> = known.iter().map(Path::new).collect();
    if let Some(h) = home {
        paths.push(h);
    }
    for p in paths {
        let Ok(v) = scull_platform::volume_of(p) else {
            continue;
        };
        let id = v.mount_point.to_string_lossy().into_owned();
        let free = scull_platform::free_space(p).ok().or(v.free_bytes);
        let (Some(total), Some(free)) = (v.total_bytes, free) else {
            continue;
        };
        if id.is_empty() || free > total || out.iter().any(|s| s.volume_id == id) {
            continue;
        }
        out.push(VolumeSample {
            volume_id: id,
            total,
            free,
            at: now,
        });
    }
    out
}

impl Session {
    /// The weekly digest of the 7 days before `now` (Insights "This week", the digest
    /// notification). Never creates or migrates the database: without one, the digest says
    /// there is no history yet.
    pub fn weekly_digest(&self, now: i64) -> DigestView {
        let prefer = self.home_volume();
        let empty = || {
            let report = scull_insights::compute(&scull_insights::InsightsInput {
                now,
                since: now.saturating_sub(i64::from(scull_insights::digest::DIGEST_DAYS) * DAY),
                items: Vec::new(),
                regrowth: Vec::new(),
                operations: Vec::new(),
                volumes: Vec::new(),
                footprint: scull_storage::Footprint::default(),
                footprint_history: Vec::new(),
                budget_used_bytes: 0,
                budget_bytes: None,
                forecast_policy: self.forecast_policy(),
            });
            scull_insights::digest::weekly_digest(&report, &[], None, None)
        };
        match self.history_source() {
            Source::Store(store) => {
                match read_weekly_digest(&store, now, prefer.as_deref(), &self.forecast_policy()) {
                    Ok(d) => DigestView {
                        state: HistoryRead::Ok,
                        detail: None,
                        digest: Some(d),
                    },
                    Err(e) => DigestView {
                        state: HistoryRead::Unavailable,
                        detail: Some(e.to_string()),
                        digest: None,
                    },
                }
            }
            Source::Missing => DigestView {
                state: HistoryRead::NoDatabase,
                detail: None,
                digest: Some(empty()),
            },
            Source::Failed(e) => DigestView {
                state: HistoryRead::Unavailable,
                detail: Some(e),
                digest: None,
            },
        }
    }

    /// The home folder's volume (mount point), when known.
    pub fn home_volume(&self) -> Option<String> {
        self.setup()
            .home
            .as_deref()
            .and_then(|h| scull_platform::volume_of(h).ok())
            .map(|v| v.mount_point.to_string_lossy().into_owned())
    }

    /// Volume IDs the history has samples for (none without a database).
    pub fn known_volumes(&self) -> Vec<String> {
        match self.history_source() {
            Source::Store(store) => store
                .status_summary()
                .map(|s| s.volumes.into_iter().map(|v| v.volume_id).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// The alerts a tray check considers at `now`: forecasts of the `live` samples together
    /// with the recorded free-space history (PRO-01, with `settings.alert_days` when set),
    /// then the alerts `scull check` recorded in the last day, minus recorded forecast alerts
    /// of volumes measured live. Most severe first. Read-only; a missing or unreadable
    /// database just means fewer alerts.
    pub fn monitor_alerts(
        &self,
        now: i64,
        settings: &MonitorSettings,
        live: &[VolumeSample],
    ) -> Vec<Alert> {
        let mut policy = self.forecast_policy();
        if let Some(days) = settings.alert_days {
            policy.alert_days = days;
            policy.critical_days = policy.critical_days.min(days);
        }
        let store = match self.history_source() {
            Source::Store(s) => Some(s),
            _ => None,
        };
        let since = now.saturating_sub(i64::from(policy.window_days).saturating_mul(DAY));
        let mut forecasts = Vec::new();
        let mut measured: BTreeSet<&str> = BTreeSet::new();
        for s in live {
            let mut samples = store
                .as_ref()
                .and_then(|st| st.free_space_history(&s.volume_id, since).ok())
                .unwrap_or_default();
            samples.push(s.clone());
            forecasts.push(forecast(&s.volume_id, &samples, &[], now, &policy));
            measured.insert(s.volume_id.as_str());
        }
        let mut alerts = forecast_alerts(&forecasts, &policy, now);
        if let Some(st) = &store
            && let Ok(recorded) = st.alerts(now.saturating_sub(RECENT_ALERTS_SECS))
        {
            alerts.extend(
                recorded
                    .into_iter()
                    .filter_map(|e| serde_json::from_value::<Alert>(e.alert).ok())
                    .filter(|a| {
                        !(matches!(a.kind, AlertKind::LowSpace | AlertKind::DiskFullForecast)
                            && measured.contains(a.subject.as_str()))
                    }),
            );
        }
        sort_alerts(&mut alerts);
        alerts
    }
}

const DAY: i64 = 24 * 60 * 60;
