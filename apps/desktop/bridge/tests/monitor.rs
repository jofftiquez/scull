//! Tray monitoring and the weekly digest (P5-PRO-01; PRD §14.6, INS-04) over a real history
//! database in a temp dir, with a fake clock. The monitor only reads: a missing database stays
//! missing and an existing one is byte-for-byte unchanged after any number of checks. Paths and
//! volumes are imagined strings; nothing on disk is touched.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_desktop_bridge::monitor::{
    CONFIGURE_GRACE_SECS, HistoryRead, Monitor, MonitorConfig, digest_notification, live_samples,
};
use scull_desktop_bridge::{Session, Setup};
use scull_proactive::monitor::{DigestSchedule, MonitorMemory, MonitorSettings};
use scull_proactive::{Alert, AlertKind, Severity};
use scull_storage::{AlertEvent, VolumeSample};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const GIB: u64 = 1024 * 1024 * 1024;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;
/// Monday 2026-11-02 09:00 UTC.
const MON_9: i64 = 1_793_610_000;
const VOLUME: &str = "/";

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn validate(stem: &str, v: &Value) {
    let path = desktop()
        .join("schemas")
        .join(format!("{stem}.schema.json"));
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{stem}: {errors:?}\n{v:#}");
}

fn setup(data: &Path) -> Setup {
    Setup {
        home: None,
        config_file: None,
        data_dir: Some(data.to_path_buf()),
        env: Arc::new(|_| None),
        record_history: true,
        git: false,
        detect_synced_roots: false,
        allow_network_roots: true,
        in_use: false,
    }
}

/// 20 days of `/` losing 2 GiB a day down to 30 GiB free at `MON_9 - 2h`, and two alerts
/// `scull check` recorded: a stale worktree (warning) and an old forecast for `/` (which a
/// live measurement supersedes).
fn seed(data: &Path) {
    let mut s = scull_engine::open_history(data).unwrap();
    let samples: Vec<VolumeSample> = (0..20)
        .map(|i| VolumeSample {
            volume_id: VOLUME.into(),
            total: 500 * GIB,
            free: (30 + 2 * (19 - i)) * GIB,
            at: MON_9 - 2 * HOUR - (19 - i as i64) * DAY,
        })
        .collect();
    s.record_free_space(&samples, None).unwrap();
    for a in [
        Alert::new(
            AlertKind::Stale,
            Severity::Warning,
            "worktree",
            "3 worktrees idle for more than 14 days (12.0 GiB)",
            MON_9 - 3 * HOUR,
        ),
        Alert::new(
            AlertKind::DiskFullForecast,
            Severity::Warning,
            VOLUME,
            "recorded forecast",
            MON_9 - 3 * HOUR,
        ),
    ] {
        s.record_alert(&AlertEvent {
            event_id: 0,
            at: a.raised_at,
            source: "check".into(),
            key: a.key.clone(),
            alert: serde_json::to_value(&a).unwrap(),
        })
        .unwrap();
    }
}

fn live(free_gib: u64, at: i64) -> Vec<VolumeSample> {
    vec![VolumeSample {
        volume_id: VOLUME.into(),
        total: 500 * GIB,
        free: free_gib * GIB,
        at,
    }]
}

fn db_bytes(data: &Path) -> Vec<u8> {
    std::fs::read(scull_engine::history_db_path(data)).unwrap()
}

#[test]
fn digest_without_a_database_says_so_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let session = Session::new(setup(&data));
    let v = session.weekly_digest(MON_9);
    assert_eq!(v.state, HistoryRead::NoDatabase);
    let d = v.digest.as_ref().unwrap();
    assert!(!d.has_history);
    assert!(!data.exists(), "a digest never creates the data directory");
    validate("weekly-digest", &serde_json::to_value(&v).unwrap());

    // A check without a database: only live alerts, and still nothing created.
    let mut m = Monitor::new(MON_9);
    m.configure(MonitorConfig::default(), MON_9);
    let out = m.tick(&session, MON_9, &live(5, MON_9), false);
    assert!(out.checked);
    assert_eq!(out.notifications.len(), 1);
    assert_eq!(out.notifications[0].title, "Low disk space");
    assert!(!data.exists());
}

#[test]
fn monitor_waits_for_the_app_then_notifies_once_per_cooldown_and_only_reads() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    seed(&data);
    let before = db_bytes(&data);
    let session = Session::new(setup(&data));
    let start = MON_9 - HOUR;
    let mut m = Monitor::new(start);

    // Not configured: the first check waits for the main window's settings.
    assert!(
        !m.tick(&session, start + 10, &live(28, start + 10), false)
            .checked
    );
    assert_eq!(m.next_check(start), start + CONFIGURE_GRACE_SECS);
    m.configure(MonitorConfig::default(), start + 20);
    let out = m.tick(&session, start + 20, &live(28, start + 20), false);
    assert!(out.checked);
    assert_eq!(out.notifications.len(), 1, "one grouped notification");
    let n = &out.notifications[0];
    // 28 GiB free, losing 2 GiB a day: below the 10 GiB reserve in about 9 days (live
    // forecast), plus the recorded stale worktrees. The recorded forecast is superseded.
    assert_eq!(n.title, "2 things need attention");
    assert_eq!(n.route, "/cleanup");
    assert!(
        n.body
            .starts_with("/ will drop below 10.0 GiB free in about 9 days"),
        "{}",
        n.body
    );
    assert!(n.body.contains("3 worktrees idle"), "{}", n.body);
    assert!(!n.body.contains("recorded forecast"), "{}", n.body);
    assert_eq!(
        n.alert_keys,
        vec![
            "disk_full_forecast:/".to_owned(),
            "stale:worktree".to_owned()
        ]
    );

    // Not due again before the interval; due after, but nothing new to say.
    assert!(
        !m.tick(&session, start + 30 * 60, &live(28, start + 1800), false)
            .checked
    );
    let out = m.tick(&session, start + 20 + HOUR, &live(28, start + HOUR), false);
    assert!(out.checked && out.notifications.is_empty());

    // Worse: below the reserve now -> critical, notified at once.
    let t = start + 20 + 2 * HOUR;
    let out = m.tick(&session, t, &live(4, t), false);
    assert_eq!(out.notifications.len(), 1);
    assert_eq!(out.notifications[0].title, "Low disk space");
    assert_eq!(
        out.notifications[0].body,
        "/ has only 4.0 GiB free, below the 10.0 GiB reserve"
    );

    // Paused: nothing until the pause ends, then it tells (after the cooldown).
    m.pause(Some(HOUR), t);
    assert!(m.status(t).paused);
    let out = m.tick(&session, t + 10, &live(4, t + 10), true);
    assert!(out.checked && out.notifications.is_empty());
    m.pause(None, t + 20);
    assert!(!m.status(t + 20).paused);

    // The database is byte-for-byte what it was.
    assert_eq!(
        db_bytes(&data),
        before,
        "the monitor never writes the history"
    );
    validate(
        "monitor-status",
        &serde_json::to_value(m.status(t + 20)).unwrap(),
    );
    validate(
        "monitor-snapshot",
        &serde_json::to_value(m.snapshot()).unwrap(),
    );
}

#[test]
fn a_restart_with_the_saved_memory_does_not_repeat_notifications() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    seed(&data);
    let session = Session::new(setup(&data));
    let t = MON_9 - HOUR;
    let mut first = Monitor::new(t);
    first.configure(MonitorConfig::default(), t);
    assert_eq!(
        first
            .tick(&session, t, &live(28, t), false)
            .notifications
            .len(),
        1
    );
    let saved = first.snapshot();

    // The app restarts ten minutes later and sends what it kept.
    let mut second = Monitor::new(t + 600);
    second.configure(
        MonitorConfig {
            settings: saved.settings.clone(),
            memory: Some(saved.memory.clone()),
        },
        t + 600,
    );
    // Its first check is one interval after the previous run's.
    assert_eq!(second.next_check(t + 600), t + HOUR);
    let out = second.tick(&session, t + HOUR, &live(28, t + HOUR), false);
    assert!(out.checked);
    assert!(out.notifications.is_empty(), "{:?}", out.notifications);

    // A later settings change keeps the memory (never rewinds it).
    second.configure(
        MonitorConfig {
            settings: MonitorSettings::default(),
            memory: Some(MonitorMemory::default()),
        },
        t + HOUR + 1,
    );
    assert_eq!(second.memory().notified.len(), 2);

    // Without the saved memory, the same alerts would notify again.
    let mut fresh = Monitor::new(t + 600);
    fresh.configure(MonitorConfig::default(), t + 600);
    assert_eq!(
        fresh
            .tick(&session, t + HOUR, &live(28, t + HOUR), false)
            .notifications
            .len(),
        1
    );
}

#[test]
fn exclusions_and_alert_days_shape_what_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    seed(&data);
    let session = Session::new(setup(&data));
    let t = MON_9 - HOUR;
    let mut m = Monitor::new(t);
    m.configure(
        MonitorConfig {
            settings: MonitorSettings {
                muted_kinds: vec![AlertKind::Stale],
                // About 9 days left is outside a 7-day window: no forecast alert.
                alert_days: Some(7),
                ..MonitorSettings::default()
            },
            memory: None,
        },
        t,
    );
    let out = m.tick(&session, t, &live(28, t), false);
    assert!(out.checked);
    assert!(out.notifications.is_empty(), "{:?}", out.notifications);
    // With 14 days it alerts, and only the forecast (stale stays muted).
    let alerts = session.monitor_alerts(
        t,
        &MonitorSettings {
            alert_days: Some(14),
            ..MonitorSettings::default()
        },
        &live(28, t),
    );
    let keys: Vec<&str> = alerts.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["disk_full_forecast:/", "stale:worktree"]);
}

#[test]
fn weekly_digest_is_opt_in_and_goes_out_at_its_local_slot() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    seed(&data);
    let before = db_bytes(&data);
    let session = Session::new(setup(&data));
    let wed = MON_9 - 5 * DAY;
    let mut m = Monitor::new(wed);
    let quiet = MonitorSettings {
        alerts: false,
        ..MonitorSettings::default()
    };
    // Off by default: no digest, ever.
    m.configure(
        MonitorConfig {
            settings: quiet.clone(),
            memory: None,
        },
        wed,
    );
    assert!(m.tick(&session, wed, &[], false).digest.is_none());
    assert_eq!(m.status(wed).next_digest, None);

    // Turned on (Monday 10:00 in UTC+1, i.e. 09:00 UTC): nothing at once.
    let on = MonitorSettings {
        digest: DigestSchedule {
            enabled: true,
            weekday: 0,
            hour: 10,
            utc_offset_secs: 3600,
        },
        ..quiet
    };
    m.configure(
        MonitorConfig {
            settings: on,
            memory: None,
        },
        wed + 10,
    );
    let out = m.tick(&session, wed + HOUR, &[], false);
    assert!(out.checked && out.digest.is_none());
    assert_eq!(m.status(wed + HOUR).next_digest, Some(MON_9));
    // Sunday evening: still not.
    assert!(
        m.tick(&session, MON_9 - 12 * HOUR, &[], false)
            .digest
            .is_none()
    );
    // Monday at the slot: the digest.
    let out = m.tick(&session, MON_9, &[], false);
    let d = out.digest.expect("digest due");
    assert_eq!(out.notifications.len(), 1);
    let n = &out.notifications[0];
    assert_eq!(n.title, "Your week in scull");
    assert_eq!(n.route, "/insights");
    assert_eq!(n.body, d.body());
    assert!(n.body.starts_with("30.0 GiB free on / ("), "{}", n.body);
    // The stale worktrees and the recorded forecast.
    assert!(n.body.ends_with("\n2 alerts this week"), "{}", n.body);
    assert_eq!(m.memory().last_digest, Some(MON_9));
    // Once a week.
    assert!(m.tick(&session, MON_9 + HOUR, &[], false).digest.is_none());
    assert_eq!(m.status(MON_9 + HOUR).next_digest, Some(MON_9 + 7 * DAY));

    // "Send now" doesn't move the slot.
    let (view, now_n) = m.digest_now(&session, MON_9 + 2 * HOUR);
    assert_eq!(view.state, HistoryRead::Ok);
    assert_eq!(
        now_n.unwrap(),
        digest_notification(view.digest.as_ref().unwrap())
    );
    assert_eq!(m.memory().last_digest, Some(MON_9));

    assert_eq!(db_bytes(&data), before);
    validate("weekly-digest", &serde_json::to_value(&view).unwrap());
}

#[test]
fn config_payloads_are_lenient_but_typo_proof() {
    let c: MonitorConfig = serde_json::from_str(r#"{"settings":{"digest":{"enabled":true}}}"#)
        .expect("partial settings take defaults");
    assert!(c.settings.digest.enabled);
    assert_eq!(c.settings.digest.hour, 9);
    assert!(c.memory.is_none());
    assert!(serde_json::from_str::<MonitorConfig>(r#"{"setings":{}}"#).is_err());
    validate("monitor-config", &serde_json::to_value(&c).unwrap());
}

#[test]
fn live_samples_measure_existing_folders_and_skip_missing_ones() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir
        .path()
        .join("not-mounted")
        .to_string_lossy()
        .into_owned();
    let s = live_samples(&[missing], Some(dir.path()), 42);
    // The temp dir's volume is measured once; the missing path is skipped.
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0].at, 42);
    assert!(s[0].free <= s[0].total && s[0].total > 0);
}

#[test]
fn the_digest_does_not_wait_for_a_long_check_interval() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    seed(&data);
    let session = Session::new(setup(&data));
    let t = MON_9 - 2 * HOUR;
    let mut m = Monitor::new(t);
    m.configure(
        MonitorConfig {
            settings: MonitorSettings {
                interval_secs: DAY,
                digest: DigestSchedule {
                    enabled: true,
                    ..DigestSchedule::default()
                },
                ..MonitorSettings::default()
            },
            memory: None,
        },
        t,
    );
    assert!(m.tick(&session, t, &live(28, t), false).checked);
    // At the slot the daily check isn't due, but the digest goes out.
    assert!(m.due(MON_9));
    let out = m.tick(&session, MON_9, &live(28, MON_9), false);
    assert!(!out.checked);
    assert!(out.digest.is_some());
    assert_eq!(out.notifications.len(), 1);
    assert!(!m.due(MON_9 + 60));
}

#[test]
fn tooltip_names_the_home_volume() {
    use scull_desktop_bridge::monitor::tooltip;
    let mut samples = live(28, 0);
    samples.push(VolumeSample {
        volume_id: "/data".into(),
        total: 900 * GIB,
        free: 400 * GIB,
        at: 0,
    });
    assert_eq!(tooltip(Some("/data"), &samples), "scull · 400.0 GiB free");
    assert_eq!(tooltip(None, &samples), "scull · 28.0 GiB free");
    assert_eq!(tooltip(Some("/x"), &[]), "scull");
}
