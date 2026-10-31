//! Forecast accuracy on recorded history (P4-PRO-01).
//!
//! A developer machine is simulated for ~3 months (irregular scan and check times, weekday
//! builds, quiet weekends, noise, two cleanups, a transient download, an OS update), every
//! sample is **recorded through `scull-storage`** into a temporary database exactly as scans
//! and `scull check` record them, and read back with the same history API the CLI uses.
//! The forecast is then back-tested: at each cutoff day it only sees history up to the
//! cutoff, and its "days until below 10 GiB" is compared with the day the recorded series
//! actually crossed the reserve.

use scull_proactive::forecast::{DAY_SECS, GIB};
use scull_proactive::{Confidence, ForecastPolicy, ForecastState, Release, forecast_volume};
use scull_storage::{DefaultPermissions, Store, VolumeSample};

const START: i64 = 1_790_000_000;
const TOTAL: u64 = 1024 * GIB;
const VOL: &str = "/";

/// xorshift64*: deterministic, dependency-free noise.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Recording {
    samples: Vec<VolumeSample>,
    /// Known cleanups (the second one is recorded as a release; the first is not).
    releases: Vec<Release>,
}

fn g(x: f64) -> f64 {
    x * GIB as f64
}

/// Simulate `days` days. Weekdays consume ~4 GiB/day, weekends ~1 GiB/day (true average
/// 22/7 ≈ 3.14 GiB/day), with ±50% per-interval noise.
fn simulate(seed: u64, days: i64) -> Recording {
    let mut rng = Rng(seed | 1);
    let mut free = g(200.0);
    let mut samples = Vec::new();
    let mut releases = Vec::new();
    let mut last_t = START;
    let mut transient: Option<f64> = None;
    for day in 0..days {
        // 1-4 samples per day at random times (a scan or two plus scheduled checks).
        let n = 1 + (rng.next() % 4) as usize;
        let mut times: Vec<i64> = (0..n)
            .map(|_| START + day * DAY_SECS + (rng.unit() * DAY_SECS as f64) as i64)
            .collect();
        times.sort_unstable();
        for t in times {
            let dt_days = (t - last_t) as f64 / DAY_SECS as f64;
            // Consumption over the interval, by the day of week of its end.
            let weekend = (day % 7) >= 5;
            let rate = if weekend { 1.0 } else { 4.0 };
            free -= g(rate * dt_days * (0.5 + rng.unit()));
            last_t = t;
            samples.push(VolumeSample {
                volume_id: VOL.to_owned(),
                total: TOTAL,
                free: free.max(0.0) as u64,
                at: t,
            });
            if let Some(size) = transient.take() {
                free += g(size); // the big download is deleted after one sample saw it
            }
        }
        // Events at the end of some days (show up in the next sample).
        match day {
            12 => free += g(45.0), // cleanup, not recorded as a release
            20 => {
                free -= g(30.0); // 30 GiB download, deleted by the next sample
                transient = Some(30.0);
            }
            26 => free -= g(8.0), // OS update: consumed for good
            33 => {
                free += g(20.0); // cleanup with a measured result
                releases.push(Release {
                    at: START + (day + 1) * DAY_SECS - 1,
                    bytes: 20 * GIB,
                });
            }
            _ => {}
        }
    }
    Recording { samples, releases }
}

/// Record the samples into a fresh store, as scans and checks do, and return it.
fn record(dir: &std::path::Path, rec: &Recording) -> Store {
    let mut store = Store::open(&dir.join("scull.db"), &DefaultPermissions).expect("open");
    for s in &rec.samples {
        store
            .record_free_space(std::slice::from_ref(s), None)
            .expect("record");
    }
    store
}

#[test]
fn back_test_days_until_reserve() {
    let reserve = 10 * GIB;
    for seed in [1u64, 7, 42, 2026, 31_337] {
        let rec = simulate(seed, 110);
        let dir = tempfile::tempdir().expect("tempdir");
        let store = record(dir.path(), &rec);
        // The day the recorded series first dropped below the reserve.
        let crossing = rec
            .samples
            .iter()
            .find(|s| s.free < reserve)
            .map(|s| s.at)
            .expect("the simulated disk fills up");
        let policy = ForecastPolicy::default();
        let mut checked = 0;
        for cutoff_day in 40..110 {
            let now = START + cutoff_day * DAY_SECS;
            if now >= crossing {
                break;
            }
            let known: Vec<Release> = rec
                .releases
                .iter()
                .copied()
                .filter(|r| r.at <= now)
                .collect();
            let f = forecast_volume(&store, VOL, &known, now, &policy).expect("forecast");
            let actual = (crossing - now) as f64 / DAY_SECS as f64;
            assert_eq!(
                f.state,
                ForecastState::Filling,
                "seed {seed} day {cutoff_day}: {f:?}"
            );
            let rate = f.fill_bytes_per_day.expect("rate") / GIB as f64;
            assert!(
                (rate - 22.0 / 7.0).abs() <= 0.25 * 22.0 / 7.0,
                "seed {seed} day {cutoff_day}: rate {rate:.2} GiB/day"
            );
            let predicted = f.days_until_threshold.expect("days");
            // Within 20% (or 2 days when close): good enough to warn days ahead.
            let tolerance = (0.20 * actual).max(2.0);
            assert!(
                (predicted - actual).abs() <= tolerance,
                "seed {seed} day {cutoff_day}: predicted {predicted:.1} d, actual {actual:.1} d ({f:?})"
            );
            // Whenever an alert would matter (< 14 days), the forecast is confident enough
            // to raise it.
            if actual < 14.0 {
                assert!(
                    f.confidence >= Confidence::Medium,
                    "seed {seed} day {cutoff_day}: {f:?}"
                );
            }
            checked += 1;
        }
        assert!(checked >= 20, "seed {seed}: only {checked} cutoffs");
    }
}

#[test]
fn recorded_cleanups_are_levelled() {
    let rec = simulate(9, 45);
    let dir = tempfile::tempdir().expect("tempdir");
    let store = record(dir.path(), &rec);
    let now = START + 45 * DAY_SECS;
    let f = forecast_volume(&store, VOL, &rec.releases, now, &ForecastPolicy::default())
        .expect("forecast");
    // Window = days 15..45: the recorded release (day 33); the day-12 cleanup is outside.
    assert!(f.jumps_ignored >= 1, "{f:?}");
    assert!(
        f.outliers_ignored >= 1,
        "the transient download is dropped: {f:?}"
    );
}

#[test]
fn forecast_all_covers_every_sampled_volume() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open(&dir.path().join("scull.db"), &DefaultPermissions).expect("open");
    let now = START + 20 * DAY_SECS;
    for day in 0..=20 {
        let at = START + day * DAY_SECS;
        store
            .record_free_space(
                &[
                    VolumeSample {
                        volume_id: "/".into(),
                        total: TOTAL,
                        free: (100 - 4 * day as u64) * GIB,
                        at,
                    },
                    VolumeSample {
                        volume_id: "/data".into(),
                        total: TOTAL,
                        free: 400 * GIB,
                        at,
                    },
                ],
                None,
            )
            .expect("record");
    }
    let all = scull_proactive::forecast_all(&store, now, &ForecastPolicy::default()).expect("all");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].volume_id, "/");
    assert_eq!(all[0].state, ForecastState::Filling);
    assert_eq!(all[0].days_rounded(), Some(3));
    assert_eq!(all[1].volume_id, "/data");
    assert_eq!(all[1].state, ForecastState::NotFilling);
    let alerts = scull_proactive::forecast_alerts(&all, &ForecastPolicy::default(), now);
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].key, "disk_full_forecast:/");
}
