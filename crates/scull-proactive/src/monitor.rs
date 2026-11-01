//! Tray monitoring (PRD §14.6, v1.x) and the weekly digest's schedule (INS-04): which alerts
//! become desktop notifications, when the next check and the next digest are due, pause,
//! cooldowns and exclusions.
//!
//! Pure and clock-free: every function takes `now` (Unix seconds), so the tray app, the tests
//! and any future caller decide time. Nothing here reads the disk, sends a notification or
//! acts on an alert: the tray only **reads and notifies**, and the fix for any alert stays a
//! reviewed cleanup (PRO-05, PRO-08, D-11).
//!
//! * [`MonitorSettings`]: what the user chose (interval, cooldown, minimum severity, muted
//!   kinds and subjects, pause, digest on/off and its weekly slot). Defaults are quiet: only
//!   warnings and critical alerts, at most once a day per alert, digest **off** (opt-in).
//! * [`MonitorMemory`]: what the monitor remembers between checks (what it notified, when the
//!   last check and digest ran). Plain data, so the app can keep it across restarts.
//! * [`notifications`]: the alerts worth a notification now, grouped into at most one
//!   notification per check (no alert storms).
//! * [`digest_due`] / [`next_digest`]: the weekly digest's slot in the user's local time
//!   (default Monday 09:00), sent once per week, never on the day it was turned on unless its
//!   slot is still ahead.

use crate::alert::{Alert, AlertKind, Severity, sort_alerts};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;

/// Default seconds between two tray checks: one hour (each check only reads free space and the
/// history `scull check` recorded; scanning stays with `scull check`, PRO-07).
pub const DEFAULT_INTERVAL_SECS: i64 = HOUR;
/// Shortest interval accepted: 15 minutes.
pub const MIN_INTERVAL_SECS: i64 = 15 * 60;
/// Longest interval accepted: one day.
pub const MAX_INTERVAL_SECS: i64 = DAY;
/// Default time before the same alert is notified again: one day.
pub const DEFAULT_COOLDOWN_SECS: i64 = DAY;
/// Shortest cooldown accepted: one hour.
pub const MIN_COOLDOWN_SECS: i64 = HOUR;
/// Longest cooldown accepted: 30 days.
pub const MAX_COOLDOWN_SECS: i64 = 30 * DAY;
/// Longest pause accepted: 7 days (a pause always ends by itself).
pub const MAX_PAUSE_SECS: i64 = 7 * DAY;
/// Notified alerts not seen again for this long are forgotten.
pub const FORGET_AFTER_SECS: i64 = 30 * DAY;
/// A grouped notification names at most this many alerts.
pub const MAX_NAMED: usize = 2;
/// At most this many muted kinds or subjects are kept.
pub const MAX_MUTED: usize = 64;
/// A notification body is cut to this many characters.
pub const MAX_BODY_CHARS: usize = 300;

/// When the weekly digest is sent, in the user's local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct DigestSchedule {
    /// Whether the digest is sent at all. **Off by default** (opt-in, INS-04).
    pub enabled: bool,
    /// Day of the week, `0` = Monday … `6` = Sunday.
    pub weekday: u8,
    /// Hour of the day, `0..=23`.
    pub hour: u8,
    /// The user's offset from UTC in seconds (east positive), as the app's clock reports it.
    pub utc_offset_secs: i32,
}

impl Default for DigestSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            weekday: 0,
            hour: 9,
            utc_offset_secs: 0,
        }
    }
}

/// What the user chose for tray monitoring. Every field has a default, so a partial JSON
/// object (or `{}`) is valid; [`MonitorSettings::normalized`] clamps out-of-range values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct MonitorSettings {
    /// Notify about alerts at all (the digest has its own switch).
    pub alerts: bool,
    /// Seconds between two checks ([`MIN_INTERVAL_SECS`]..=[`MAX_INTERVAL_SECS`]).
    pub interval_secs: i64,
    /// Seconds before the same alert (same key and severity) is notified again
    /// ([`MIN_COOLDOWN_SECS`]..=[`MAX_COOLDOWN_SECS`]). A worse severity notifies at once.
    pub cooldown_secs: i64,
    /// Alerts below this severity never notify (default: warning).
    pub min_severity: Severity,
    /// Alert kinds that never notify (e.g. growth spikes when "Sudden growth" is off).
    pub muted_kinds: Vec<AlertKind>,
    /// Subjects that never notify: a volume, category or folder; a folder also mutes
    /// everything below it.
    pub muted_subjects: Vec<String>,
    /// Days ahead a disk-full forecast alerts (overrides `[proactive] alert_days` for the
    /// tray's own forecast; `None`: the configured value).
    pub alert_days: Option<u32>,
    /// Nothing is notified before this time (Unix seconds); `None`: not paused.
    pub paused_until: Option<i64>,
    /// The weekly digest.
    pub digest: DigestSchedule,
}

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            alerts: true,
            interval_secs: DEFAULT_INTERVAL_SECS,
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
            min_severity: Severity::Warning,
            muted_kinds: Vec::new(),
            muted_subjects: Vec::new(),
            alert_days: None,
            paused_until: None,
            digest: DigestSchedule::default(),
        }
    }
}

impl MonitorSettings {
    /// The same settings with every value in range: interval and cooldown clamped, a pause no
    /// longer than [`MAX_PAUSE_SECS`] from `now` (an expired pause is dropped), weekday and
    /// hour wrapped, UTC offset within ±14 h, muted lists deduplicated and capped, empty muted
    /// subjects dropped, `alert_days` within 1..=365.
    #[must_use]
    pub fn normalized(mut self, now: i64) -> Self {
        self.interval_secs = self
            .interval_secs
            .clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
        self.cooldown_secs = self
            .cooldown_secs
            .clamp(MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS);
        self.paused_until = self
            .paused_until
            .filter(|&t| t > now)
            .map(|t| t.min(now.saturating_add(MAX_PAUSE_SECS)));
        self.digest.weekday %= 7;
        self.digest.hour %= 24;
        self.digest.utc_offset_secs = self.digest.utc_offset_secs.clamp(-14 * 3600, 14 * 3600);
        self.alert_days = self.alert_days.map(|d| d.clamp(1, 365));
        self.muted_kinds.sort_by_key(|k| k.as_str());
        self.muted_kinds.dedup();
        self.muted_kinds.truncate(MAX_MUTED);
        self.muted_subjects.retain(|s| !s.trim().is_empty());
        self.muted_subjects.sort();
        self.muted_subjects.dedup();
        self.muted_subjects.truncate(MAX_MUTED);
        self
    }

    /// Whether notifications are paused at `now`.
    pub fn paused(&self, now: i64) -> bool {
        self.paused_until.is_some_and(|t| t > now)
    }

    /// Whether `alert` may notify under these settings (severity, kind and subject; not the
    /// pause or the cooldown).
    pub fn admits(&self, alert: &Alert) -> bool {
        alert.severity >= self.min_severity
            && !self.muted_kinds.contains(&alert.kind)
            && !self
                .muted_subjects
                .iter()
                .any(|m| subject_matches(m, &alert.subject))
    }
}

/// Whether muted subject `muted` covers `subject`: equal, or a folder above it.
fn subject_matches(muted: &str, subject: &str) -> bool {
    let m = muted.trim_end_matches(['/', '\\']);
    if m.is_empty() {
        // A muted root (`/`) mutes only the root volume itself.
        return subject == muted;
    }
    subject == m
        || subject
            .strip_prefix(m)
            .is_some_and(|rest| rest.starts_with(['/', '\\']))
}

/// One alert the monitor notified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notified {
    /// When it was notified.
    pub at: i64,
    /// At which severity.
    pub severity: Severity,
}

/// What the monitor remembers between checks. Plain data: the app keeps it across restarts so
/// a restart doesn't repeat yesterday's notifications.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct MonitorMemory {
    /// When the last check ran.
    pub last_check: Option<i64>,
    /// Notified alerts by key.
    pub notified: BTreeMap<String, Notified>,
    /// When the last digest was sent.
    pub last_digest: Option<i64>,
    /// When the digest was found turned on without having been sent (its first slot is the
    /// next one after this).
    pub digest_since: Option<i64>,
}

/// When the next check is due: right away before the first one, else one interval after the
/// last, and never before a pause ends.
pub fn next_check(settings: &MonitorSettings, memory: &MonitorMemory, now: i64) -> i64 {
    let due = memory
        .last_check
        .map_or(now, |t| t.saturating_add(settings.interval_secs));
    match settings.paused_until {
        Some(p) if p > due => p,
        _ => due,
    }
}

/// Whether a check is due at `now`.
pub fn check_due(settings: &MonitorSettings, memory: &MonitorMemory, now: i64) -> bool {
    next_check(settings, memory, now) <= now
}

/// A desktop notification to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    /// Short title.
    pub title: String,
    /// One to three plain lines (no control characters, at most [`MAX_BODY_CHARS`]).
    pub body: String,
    /// The most severe alert's severity.
    pub severity: Severity,
    /// Keys of the alerts it covers.
    pub alert_keys: Vec<String>,
    /// The main-window screen that answers it: Cleanup Review for low space (PRO-05), else
    /// Insights.
    pub route: String,
}

/// The notification for the alerts worth one at `now`, if any, and the memory updated.
///
/// An alert notifies when the settings [admit](MonitorSettings::admits) it and either it was
/// never notified, its severity got worse, or the cooldown passed **and** it was raised again
/// after the last notification. Nothing notifies while paused or with alerts off (and nothing
/// is marked, so a resumed monitor still tells). Several alerts make **one** notification
/// that names the most severe [`MAX_NAMED`]. Alerts with the same key count once (the most
/// severe, then the newest).
pub fn notifications(
    alerts: &[Alert],
    settings: &MonitorSettings,
    memory: &mut MonitorMemory,
    now: i64,
) -> Option<Notification> {
    memory.last_check = Some(now);
    memory
        .notified
        .retain(|_, n| now.saturating_sub(n.at) < FORGET_AFTER_SECS);
    if !settings.alerts || settings.paused(now) {
        return None;
    }
    let mut by_key: BTreeMap<&str, &Alert> = BTreeMap::new();
    for a in alerts.iter().filter(|a| settings.admits(a)) {
        by_key
            .entry(a.key.as_str())
            .and_modify(|cur| {
                if (a.severity, a.raised_at) > (cur.severity, cur.raised_at) {
                    *cur = a;
                }
            })
            .or_insert(a);
    }
    let mut due: Vec<Alert> = by_key
        .into_values()
        .filter(|a| match memory.notified.get(&a.key) {
            None => true,
            Some(n) => {
                a.severity > n.severity
                    || (now.saturating_sub(n.at) >= settings.cooldown_secs && a.raised_at > n.at)
            }
        })
        .cloned()
        .collect();
    if due.is_empty() {
        return None;
    }
    sort_alerts(&mut due);
    for a in &due {
        memory.notified.insert(
            a.key.clone(),
            Notified {
                at: now,
                severity: a.severity,
            },
        );
    }
    Some(compose(&due))
}

fn compose(due: &[Alert]) -> Notification {
    let top = &due[0];
    let route = if due
        .iter()
        .any(|a| matches!(a.kind, AlertKind::LowSpace | AlertKind::DiskFullForecast))
    {
        "/cleanup"
    } else {
        "/insights"
    };
    let (title, body) = if due.len() == 1 {
        (title_of(top.kind).to_owned(), top.message.clone())
    } else {
        let mut lines: Vec<String> = due
            .iter()
            .take(MAX_NAMED)
            .map(|a| a.message.clone())
            .collect();
        if due.len() > MAX_NAMED {
            lines.push(format!("and {} more in scull", due.len() - MAX_NAMED));
        }
        (
            format!("{} things need attention", due.len()),
            lines.join("\n"),
        )
    };
    Notification {
        title,
        body: plain(&body),
        severity: top.severity,
        alert_keys: due.iter().map(|a| a.key.clone()).collect(),
        route: route.to_owned(),
    }
}

/// The notification title for one alert of `kind`.
pub fn title_of(kind: AlertKind) -> &'static str {
    match kind {
        AlertKind::LowSpace => "Low disk space",
        AlertKind::DiskFullForecast => "Disk filling up",
        AlertKind::Stale => "Stale items to review",
        AlertKind::WorktreeTtlExpired => "Worktree past its TTL",
        AlertKind::BudgetExceeded => "Over budget",
        AlertKind::GrowthSpike => "Unusual growth",
        AlertKind::FreeSpaceDrop => "Free space dropped fast",
    }
}

/// `text` safe for a notification: control characters other than newlines become spaces
/// (paths may hold any byte), at most [`MAX_BODY_CHARS`] characters (cut with `…`).
pub fn plain(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= MAX_BODY_CHARS {
        return cleaned;
    }
    let mut out: String = cleaned.chars().take(MAX_BODY_CHARS - 1).collect();
    out.push('…');
    out
}

/// The latest digest slot at or before `now` (Unix seconds): the configured weekday and hour
/// in the user's local time.
pub fn digest_slot(schedule: &DigestSchedule, now: i64) -> i64 {
    let offset = i64::from(schedule.utc_offset_secs);
    let local = now.saturating_add(offset);
    let days = local.div_euclid(DAY);
    // 1970-01-01 was a Thursday: day 0 has weekday 3 (Monday = 0).
    let weekday = (days + 3).rem_euclid(7);
    let back = (weekday - i64::from(schedule.weekday % 7)).rem_euclid(7);
    let mut slot = (days - back) * DAY + i64::from(schedule.hour % 24) * HOUR;
    if slot > local {
        slot -= 7 * DAY;
    }
    slot - offset
}

/// Whether the digest is due at `now`, updating `memory`.
///
/// Off or paused: never. The first time it is found on, the moment is remembered and the
/// first digest goes out at the next slot (turning it on never sends one by surprise; the
/// app's "Send now" is the explicit way). After that: once per slot, the first check at or
/// after it (a computer that was asleep on Monday morning gets it when it wakes, the same
/// week).
pub fn digest_due(settings: &MonitorSettings, memory: &mut MonitorMemory, now: i64) -> bool {
    if !settings.digest.enabled {
        memory.digest_since = None;
        return false;
    }
    let Some(since) = memory.last_digest.or(memory.digest_since) else {
        memory.digest_since = Some(now);
        return false;
    };
    !settings.paused(now) && since < digest_slot(&settings.digest, now)
}

/// Record that the digest was sent at `now`.
pub fn mark_digest_sent(memory: &mut MonitorMemory, now: i64) {
    memory.last_digest = Some(now);
    memory.digest_since = None;
}

/// When the next digest goes out (`None` when it is off): the current slot when one is
/// already due, else the next slot after `now`.
pub fn next_digest(settings: &MonitorSettings, memory: &MonitorMemory, now: i64) -> Option<i64> {
    if !settings.digest.enabled {
        return None;
    }
    let slot = digest_slot(&settings.digest, now);
    let since = memory.last_digest.or(memory.digest_since).unwrap_or(now);
    let next = if since < slot { slot } else { slot + 7 * DAY };
    Some(match settings.paused_until {
        Some(p) if p > next => p,
        _ => next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monday 2026-11-02 09:00 UTC.
    const MON_9: i64 = 1_793_610_000;

    fn alert(kind: AlertKind, severity: Severity, subject: &str, at: i64) -> Alert {
        Alert::new(
            kind,
            severity,
            subject,
            format!("{subject} needs a look"),
            at,
        )
    }

    #[test]
    fn fixed_point_is_a_monday_morning() {
        // 2026-11-02 is a Monday; the constant is 09:00 UTC that day.
        assert_eq!(MON_9.rem_euclid(DAY), 9 * HOUR);
        assert_eq!((MON_9.div_euclid(DAY) + 3).rem_euclid(7), 0);
    }

    #[test]
    fn defaults_are_quiet() {
        let s = MonitorSettings::default();
        assert!(s.alerts);
        assert!(!s.digest.enabled, "the digest is opt-in");
        assert_eq!(s.min_severity, Severity::Warning);
        assert_eq!(s.interval_secs, HOUR);
        assert_eq!(s.cooldown_secs, DAY);
        // An empty object is the defaults.
        let parsed: MonitorSettings = serde_json::from_str("{}").expect("parse");
        assert_eq!(parsed, s);
        // Unknown fields are refused (a typo never silently turns something on).
        assert!(serde_json::from_str::<MonitorSettings>(r#"{"digestt":true}"#).is_err());
    }

    #[test]
    fn normalized_clamps_everything() {
        let now = 1_000_000;
        let s = MonitorSettings {
            interval_secs: 1,
            cooldown_secs: i64::MAX,
            paused_until: Some(now + 365 * DAY),
            muted_kinds: vec![AlertKind::Stale, AlertKind::Stale, AlertKind::GrowthSpike],
            muted_subjects: vec![" ".into(), "/a".into(), "/a".into()],
            alert_days: Some(0),
            digest: DigestSchedule {
                enabled: true,
                weekday: 9,
                hour: 25,
                utc_offset_secs: 99 * 3600,
            },
            ..MonitorSettings::default()
        }
        .normalized(now);
        assert_eq!(s.interval_secs, MIN_INTERVAL_SECS);
        assert_eq!(s.cooldown_secs, MAX_COOLDOWN_SECS);
        assert_eq!(s.paused_until, Some(now + MAX_PAUSE_SECS));
        assert_eq!(
            s.muted_kinds,
            vec![AlertKind::GrowthSpike, AlertKind::Stale]
        );
        assert_eq!(s.muted_subjects, vec!["/a".to_owned()]);
        assert_eq!(s.alert_days, Some(1));
        assert_eq!((s.digest.weekday, s.digest.hour), (2, 1));
        assert_eq!(s.digest.utc_offset_secs, 14 * 3600);
        // An expired pause is dropped.
        let s = MonitorSettings {
            paused_until: Some(now - 1),
            ..MonitorSettings::default()
        }
        .normalized(now);
        assert_eq!(s.paused_until, None);
    }

    #[test]
    fn checks_are_due_by_interval_and_after_a_pause() {
        let mut s = MonitorSettings::default();
        let mut m = MonitorMemory::default();
        assert!(check_due(&s, &m, 100), "first check runs at once");
        m.last_check = Some(100);
        assert!(!check_due(&s, &m, 100 + HOUR - 1));
        assert!(check_due(&s, &m, 100 + HOUR));
        s.paused_until = Some(100 + 3 * HOUR);
        assert_eq!(next_check(&s, &m, 100 + HOUR), 100 + 3 * HOUR);
        assert!(!check_due(&s, &m, 100 + 2 * HOUR));
        assert!(check_due(&s, &m, 100 + 3 * HOUR));
    }

    #[test]
    fn notifies_once_then_waits_for_the_cooldown_and_a_new_raise() {
        let s = MonitorSettings::default();
        let mut m = MonitorMemory::default();
        let t0 = MON_9;
        let a = alert(AlertKind::DiskFullForecast, Severity::Warning, "/", t0);
        let n = notifications(std::slice::from_ref(&a), &s, &mut m, t0).expect("first time");
        assert_eq!(n.title, "Disk filling up");
        assert_eq!(n.body, "/ needs a look");
        assert_eq!(n.route, "/cleanup");
        assert_eq!(n.alert_keys, vec!["disk_full_forecast:/".to_owned()]);
        assert_eq!(m.last_check, Some(t0));

        // The same alert an hour later (re-raised by the next check): within the cooldown.
        let again = alert(
            AlertKind::DiskFullForecast,
            Severity::Warning,
            "/",
            t0 + HOUR,
        );
        assert!(notifications(std::slice::from_ref(&again), &s, &mut m, t0 + HOUR).is_none());
        // After the cooldown, but the record is the old one (not raised again): still quiet.
        assert!(notifications(std::slice::from_ref(&a), &s, &mut m, t0 + DAY).is_none());
        // After the cooldown and raised again: notifies.
        let later = alert(
            AlertKind::DiskFullForecast,
            Severity::Warning,
            "/",
            t0 + DAY,
        );
        assert!(notifications(std::slice::from_ref(&later), &s, &mut m, t0 + DAY).is_some());
    }

    #[test]
    fn a_worse_severity_notifies_at_once() {
        let s = MonitorSettings::default();
        let mut m = MonitorMemory::default();
        let warn = alert(AlertKind::DiskFullForecast, Severity::Warning, "/", 10);
        assert!(notifications(&[warn], &s, &mut m, 10).is_some());
        let crit = alert(AlertKind::DiskFullForecast, Severity::Critical, "/", 20);
        let n = notifications(&[crit], &s, &mut m, 20).expect("escalated");
        assert_eq!(n.severity, Severity::Critical);
        assert_eq!(
            m.notified["disk_full_forecast:/"].severity,
            Severity::Critical
        );
        // Back to a warning: no news.
        let warn = alert(AlertKind::DiskFullForecast, Severity::Warning, "/", 30);
        assert!(notifications(&[warn], &s, &mut m, 30).is_none());
    }

    #[test]
    fn exclusions_pause_and_switch() {
        let mut s = MonitorSettings {
            muted_kinds: vec![AlertKind::GrowthSpike],
            muted_subjects: vec!["/Users/ada/scratch".into()],
            ..MonitorSettings::default()
        };
        let mut m = MonitorMemory::default();
        let info = alert(AlertKind::Stale, Severity::Info, "worktree", 1);
        let spike = alert(AlertKind::GrowthSpike, Severity::Warning, "cache", 1);
        let muted = alert(
            AlertKind::BudgetExceeded,
            Severity::Warning,
            "/Users/ada/scratch/big",
            1,
        );
        let sibling = alert(
            AlertKind::BudgetExceeded,
            Severity::Warning,
            "/Users/ada/scratch-2",
            1,
        );
        let n = notifications(&[info, spike, muted, sibling.clone()], &s, &mut m, 1)
            .expect("only the sibling folder");
        assert_eq!(n.alert_keys, vec![sibling.key.clone()]);
        assert_eq!(n.route, "/insights");

        // Paused: nothing, and nothing marked, so the alert still notifies after the pause.
        let mut m = MonitorMemory::default();
        s.paused_until = Some(100);
        assert!(notifications(std::slice::from_ref(&sibling), &s, &mut m, 50).is_none());
        assert!(m.notified.is_empty());
        assert_eq!(m.last_check, Some(50));
        assert!(notifications(std::slice::from_ref(&sibling), &s, &mut m, 100).is_some());

        // Alerts off.
        let mut m = MonitorMemory::default();
        s.alerts = false;
        assert!(notifications(&[sibling], &s, &mut m, 200).is_none());
    }

    #[test]
    fn muted_subjects_match_folders_not_prefixes() {
        assert!(subject_matches("/a/b", "/a/b"));
        assert!(subject_matches("/a/b/", "/a/b/c"));
        assert!(subject_matches(r"C:\code", r"C:\code\x"));
        assert!(!subject_matches("/a/b", "/a/bc"));
        assert!(subject_matches("/", "/"));
        assert!(!subject_matches("/", "/data"));
    }

    #[test]
    fn several_alerts_make_one_grouped_notification() {
        let s = MonitorSettings::default();
        let mut m = MonitorMemory::default();
        let alerts = vec![
            alert(AlertKind::GrowthSpike, Severity::Warning, "cache", 5),
            alert(AlertKind::LowSpace, Severity::Critical, "/", 5),
            alert(AlertKind::BudgetExceeded, Severity::Warning, "docker", 5),
            // Same key twice: counted once, the most severe kept.
            alert(AlertKind::BudgetExceeded, Severity::Critical, "docker", 4),
        ];
        let n = notifications(&alerts, &s, &mut m, 5).expect("grouped");
        assert_eq!(n.title, "3 things need attention");
        assert_eq!(n.severity, Severity::Critical);
        assert_eq!(n.route, "/cleanup");
        assert_eq!(
            n.body,
            // Both critical: by kind, then subject (sort_alerts).
            "docker needs a look\n/ needs a look\nand 1 more in scull"
        );
        assert_eq!(n.alert_keys.len(), 3);
        assert_eq!(m.notified.len(), 3);
        assert_eq!(
            m.notified["budget_exceeded:docker"].severity,
            Severity::Critical
        );
    }

    #[test]
    fn memory_forgets_old_entries_and_round_trips() {
        let s = MonitorSettings::default();
        let mut m = MonitorMemory::default();
        let a = alert(AlertKind::Stale, Severity::Warning, "x", 0);
        notifications(&[a], &s, &mut m, 0).expect("notified");
        assert!(notifications(&[], &s, &mut m, FORGET_AFTER_SECS).is_none());
        assert!(m.notified.is_empty());
        m.last_digest = Some(7);
        let json = serde_json::to_string(&m).expect("json");
        let back: MonitorMemory = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, m);
        assert_eq!(
            serde_json::from_str::<MonitorMemory>("{}").expect("parse"),
            MonitorMemory::default()
        );
    }

    #[test]
    fn bodies_are_plain_and_short() {
        assert_eq!(plain("a\u{1b}[31mb\tc\nd"), "a [31mb c\nd");
        let long = "x".repeat(MAX_BODY_CHARS + 50);
        let p = plain(&long);
        assert_eq!(p.chars().count(), MAX_BODY_CHARS);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn digest_slots_follow_local_time() {
        let utc = DigestSchedule {
            enabled: true,
            ..DigestSchedule::default()
        };
        assert_eq!(digest_slot(&utc, MON_9), MON_9);
        assert_eq!(digest_slot(&utc, MON_9 - 1), MON_9 - 7 * DAY);
        assert_eq!(digest_slot(&utc, MON_9 + 3 * DAY), MON_9);
        // UTC+8 (Manila): Monday 09:00 local is Monday 01:00 UTC.
        let manila = DigestSchedule {
            utc_offset_secs: 8 * 3600,
            ..utc
        };
        assert_eq!(digest_slot(&manila, MON_9), MON_9 - 8 * HOUR);
        // UTC−7: Monday 09:00 local is Monday 16:00 UTC; at 09:00 UTC it is last week's.
        let pacific = DigestSchedule {
            utc_offset_secs: -7 * 3600,
            ..utc
        };
        assert_eq!(digest_slot(&pacific, MON_9), MON_9 + 7 * HOUR - 7 * DAY);
        // Sunday 20:00.
        let sunday = DigestSchedule {
            weekday: 6,
            hour: 20,
            ..utc
        };
        assert_eq!(digest_slot(&sunday, MON_9), MON_9 - 13 * HOUR);
    }

    #[test]
    fn digest_is_opt_in_weekly_and_never_a_surprise() {
        let mut s = MonitorSettings::default();
        let mut m = MonitorMemory::default();
        // Off: never due.
        assert!(!digest_due(&s, &mut m, MON_9));
        assert_eq!(next_digest(&s, &m, MON_9), None);

        // Turned on on a Wednesday: not sent now; the next Monday it is.
        s.digest.enabled = true;
        let wed = MON_9 + 2 * DAY;
        assert!(!digest_due(&s, &mut m, wed));
        assert_eq!(m.digest_since, Some(wed));
        assert_eq!(next_digest(&s, &m, wed), Some(MON_9 + 7 * DAY));
        assert!(!digest_due(&s, &mut m, MON_9 + 7 * DAY - 1));
        assert!(digest_due(&s, &mut m, MON_9 + 7 * DAY));
        mark_digest_sent(&mut m, MON_9 + 7 * DAY);
        assert_eq!(m.digest_since, None);
        // Once per week.
        assert!(!digest_due(&s, &mut m, MON_9 + 7 * DAY + HOUR));
        assert!(!digest_due(&s, &mut m, MON_9 + 13 * DAY));
        // Asleep on Monday morning: sent when it wakes on Tuesday.
        assert!(digest_due(&s, &mut m, MON_9 + 15 * DAY));
        // Paused: held back, then sent after the pause (same week).
        mark_digest_sent(&mut m, MON_9 + 15 * DAY);
        s.paused_until = Some(MON_9 + 21 * DAY + 2 * HOUR);
        assert!(!digest_due(&s, &mut m, MON_9 + 21 * DAY));
        assert_eq!(
            next_digest(&s, &m, MON_9 + 21 * DAY),
            Some(MON_9 + 21 * DAY + 2 * HOUR)
        );
        assert!(digest_due(&s, &mut m, MON_9 + 21 * DAY + 2 * HOUR));

        // Turned on on Monday before the slot: today's slot counts.
        s.paused_until = None;
        let mut m = MonitorMemory::default();
        assert!(!digest_due(&s, &mut m, MON_9 - HOUR));
        assert!(digest_due(&s, &mut m, MON_9));
        // Turned off: forgets when it was turned on.
        s.digest.enabled = false;
        assert!(!digest_due(&s, &mut m, MON_9));
        assert_eq!(m.digest_since, None);
    }
}
