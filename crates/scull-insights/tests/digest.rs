//! The weekly digest (INS-04) over seeded histories in temp dirs. The demo history describes
//! paths that do not exist; nothing reads or touches them.
#![allow(clippy::unwrap_used)]

use scull_insights::digest::{DIGEST_TITLE, read_weekly_digest};
use scull_insights::fixtures::{DEMO_VOLUME, seed_demo};
use scull_insights::time::DAY;
use scull_proactive::{Alert, AlertKind, ForecastPolicy, Severity};
use scull_storage::{AlertEvent, DefaultPermissions, Store, VolumeSample};

const GIB: u64 = 1024 * 1024 * 1024;
/// 2027-01-15 08:00 UTC.
const NOW: i64 = 1_800_000_000;

fn store(dir: &tempfile::TempDir) -> Store {
    Store::open(
        &dir.path().join(scull_storage::DB_FILE_NAME),
        &DefaultPermissions,
    )
    .unwrap()
}

fn record_alert(s: &mut Store, a: &Alert) {
    s.record_alert(&AlertEvent {
        event_id: 0,
        at: a.raised_at,
        source: "check".into(),
        key: a.key.clone(),
        alert: serde_json::to_value(a).unwrap(),
    })
    .unwrap();
}

#[test]
fn empty_database_digest_says_there_is_no_history() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(&dir);
    let d = read_weekly_digest(&s, NOW, None, &ForecastPolicy::default()).unwrap();
    assert!(!d.has_history);
    assert_eq!(d.since, NOW - 7 * DAY);
    assert!(d.volumes.is_empty());
    assert!(d.lines[0].starts_with("No history this week yet"));
}

#[test]
fn demo_week_digest_names_free_space_forecast_growth_cleanups_and_alerts() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(&dir);
    seed_demo(&mut s, NOW).unwrap();
    // Two alerts this week (one raised twice), one older than the week.
    let low = Alert::new(
        AlertKind::LowSpace,
        Severity::Critical,
        "/",
        "low",
        NOW - DAY,
    );
    record_alert(&mut s, &low);
    record_alert(
        &mut s,
        &Alert::new(
            AlertKind::LowSpace,
            Severity::Warning,
            "/",
            "low",
            NOW - 2 * DAY,
        ),
    );
    record_alert(
        &mut s,
        &Alert::new(
            AlertKind::Stale,
            Severity::Warning,
            "worktree",
            "w",
            NOW - 3 * DAY,
        ),
    );
    record_alert(
        &mut s,
        &Alert::new(
            AlertKind::Stale,
            Severity::Warning,
            "old",
            "o",
            NOW - 9 * DAY,
        ),
    );

    let d = read_weekly_digest(&s, NOW, Some(DEMO_VOLUME), &ForecastPolicy::default()).unwrap();
    assert!(d.has_history);
    assert_eq!(d.title, DIGEST_TITLE);
    assert_eq!(d.volumes.len(), 1);
    let v = &d.volumes[0];
    assert_eq!(v.volume_id, DEMO_VOLUME);
    // The demo loses 2.5 GiB a day and had a cleanup 6 days ago: the week's change is
    // negative, and the volume is filling.
    assert!(v.change_bytes < 0, "{v:?}");
    assert!(v.days_left.is_some(), "{v:?}");
    // One demo cleanup (6 days ago) falls in the week.
    assert_eq!(d.cleanups, 1);
    assert!(d.reclaimed_bytes > 0);
    assert_eq!(d.alerts.critical, 1);
    assert_eq!(d.alerts.warning, 1);
    assert_eq!(d.alerts.total(), 2);
    assert!(!d.top_growers.is_empty());
    assert!(d.top_growers.len() <= 3);

    assert!(d.lines[0].starts_with(&format!(
        "{} free on / (−",
        scull_proactive::alert::format_bytes(v.free_bytes)
    )));
    assert!(
        d.lines[0].contains("days until 10.0 GiB free"),
        "{:?}",
        d.lines
    );
    assert!(
        d.lines.iter().any(|l| l.starts_with("Grew most: ")),
        "{:?}",
        d.lines
    );
    assert!(
        d.lines
            .iter()
            .any(|l| l.starts_with("Cleaned ") && l.ends_with("in 1 cleanup"))
    );
    assert!(
        d.lines
            .contains(&"2 alerts this week (1 critical)".to_owned()),
        "{:?}",
        d.lines
    );
    assert_eq!(d.body().lines().count(), 3);

    // Same input, same digest (pure over the store).
    assert_eq!(
        read_weekly_digest(&s, NOW, Some(DEMO_VOLUME), &ForecastPolicy::default()).unwrap(),
        d
    );
}

#[test]
fn preferred_volume_comes_first_then_the_one_closest_to_full() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(&dir);
    let mut samples = Vec::new();
    for i in 0..8 {
        let at = NOW - (7 - i) * DAY;
        let i = u64::try_from(i).unwrap();
        // /data fills fast (3 GiB a day), / is steady, /full is below the reserve.
        samples.push(VolumeSample {
            volume_id: "/data".into(),
            total: 900 * GIB,
            free: (80 - 3 * i) * GIB,
            at,
        });
        samples.push(VolumeSample {
            volume_id: "/".into(),
            total: 500 * GIB,
            free: 200 * GIB,
            at,
        });
        samples.push(VolumeSample {
            volume_id: "/full".into(),
            total: 100 * GIB,
            free: 2 * GIB,
            at,
        });
    }
    s.record_free_space(&samples, None).unwrap();
    let d = read_weekly_digest(&s, NOW, None, &ForecastPolicy::default()).unwrap();
    let ids: Vec<&str> = d.volumes.iter().map(|v| v.volume_id.as_str()).collect();
    assert_eq!(ids, ["/full", "/data", "/"], "{:#?}", d.volumes);
    assert!(d.volumes[0].below_reserve);
    assert!(
        d.lines[0].ends_with("below the 10.0 GiB reserve"),
        "{:?}",
        d.lines
    );
    let d = read_weekly_digest(&s, NOW, Some("/"), &ForecastPolicy::default()).unwrap();
    let ids: Vec<&str> = d.volumes.iter().map(|v| v.volume_id.as_str()).collect();
    assert_eq!(ids, ["/", "/full", "/data"]);
    assert!(
        d.lines[0].starts_with("200.0 GiB free on / (no change this week)"),
        "{:?}",
        d.lines
    );
}
