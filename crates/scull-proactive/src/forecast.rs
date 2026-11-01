//! Disk-full forecast per volume (PRO-01, D-22): days until free space drops below a reserve,
//! with a confidence level.
//!
//! # Method
//!
//! Input: the free-space samples of one volume ([`VolumeSample`], recorded by every scan and
//! every `scull check`), plus any **known releases** ([`Release`]: bytes a cleanup is known to
//! have freed, and when).
//!
//! 1. **Window.** Only samples taken in the last [`ForecastPolicy::window_days`] before `now`
//!    count (PRD: "fit free-space history over the last 14–30 days"). Samples from the future
//!    (clock skew) or with `free > total` are ignored; equal timestamps keep the later sample.
//! 2. **Below the reserve already?** If the latest free space (plus releases after it) is at or
//!    below [`ForecastPolicy::reserve_bytes`], the answer is [`ForecastState::BelowReserve`]:
//!    a measurement, not a forecast, so it needs a single sample.
//! 3. **Enough history?** Fewer than [`ForecastPolicy::min_samples`] samples, or a span shorter
//!    than [`ForecastPolicy::min_span_hours`], gives [`ForecastState::NotEnoughHistory`] with
//!    [`Confidence::Unknown`]. scull never extrapolates from a handful of points.
//! 4. **One-off spikes.** Each interval's change is compared with what the median rate
//!    predicts for its length (the *excess*). A sample whose excess is large in one direction
//!    and immediately cancelled by the next interval (a big file downloaded then deleted) is
//!    dropped as a transient outlier.
//! 5. **Cleanups and one-off steps.** Known releases are subtracted from their interval, and
//!    an unexplained *upward* step in free space of at least `jump_min_bytes` whose excess is
//!    above `max(jump_min_bytes, 5 σ)` is treated as an unrecorded cleanup. Both are removed
//!    by shifting every earlier sample by the step, so the series is continuous and the fit
//!    sees only the consumption rate. A *downward* step of the same size is a one-off
//!    consumption (an OS update) and is levelled the same way, but only while such drops are
//!    rare (at most one per 14 days of history): drops that keep happening are how the
//!    machine consumes space and stay in the trend. Levelling never changes the latest free
//!    space, which the days are counted from.
//! 6. **Robust trend.** A Theil–Sen line (median of pairwise slopes) over the levelled series
//!    gives the rate in bytes per day, using pairs at least 7 days apart (half the span for
//!    shorter histories) so busy weekdays and quiet weekends average out; residual spread is
//!    a robust σ (the larger of the MAD and IQR estimates).
//! 7. **Days.** From the latest measured free space (not the fitted value, so a cleanup shows
//!    immediately): `(free − reserve) / rate`. A volume whose free space is stable or growing,
//!    or that would take more than [`ForecastPolicy::horizon_days`] to fill, is
//!    [`ForecastState::NotFilling`].
//!
//! # Confidence
//!
//! The lowest of three caps:
//!
//! | | high | medium | low |
//! |---|---|---|---|
//! | history | ≥ 10 samples over ≥ 14 days | ≥ 6 samples over ≥ 7 days | otherwise |
//! | noise (σ ÷ change explained by the trend over the span) | ≤ 0.10 | ≤ 0.25 | otherwise |
//! | extrapolation (days ÷ span) | ≤ 2 | ≤ 6 (one level down) | otherwise |
//!
//! Status lines and alerts only use forecasts of at least [`Confidence::Medium`].
//!
//! # One forecast source
//!
//! This is the only disk-full forecast in scull (#36). `scull check`, `scull status`,
//! `scull insights`, the MCP server, the weekly digest, the tray monitor and every desktop
//! screen (through the bridge's `forecasts` command) call it with the configured policy
//! ([`crate::forecast_policy`]), so they report the same days and confidence for the same
//! history. The desktop draws the result; it does not fit a trend of its own. See
//! `docs/FORECAST.md`.

use crate::stats;
use schemars::JsonSchema;
use scull_storage::VolumeSample;
use serde::{Deserialize, Serialize};

/// Seconds per day.
pub const DAY_SECS: i64 = 86_400;
/// One gibibyte.
pub const GIB: u64 = 1024 * 1024 * 1024;
/// At most this many samples are fitted (evenly thinned, first and last kept): Theil–Sen is
/// quadratic in the number of points.
pub const MAX_FIT_POINTS: usize = 400;

/// Tuning of the forecast. The defaults follow PRD §16 (PRO-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ForecastPolicy {
    /// Only samples from this many days before `now` are fitted (PRD: 14–30 days).
    pub window_days: u32,
    /// "Full" means less free space than this (PRD example: "below 10 GB free"; the desktop
    /// uses 10 GiB).
    pub reserve_bytes: u64,
    /// No forecast from fewer samples than this.
    pub min_samples: u32,
    /// No forecast from samples spanning less than this many hours.
    pub min_span_hours: u32,
    /// Alert when the volume is forecast to drop below the reserve within this many days
    /// (PRD default: 14).
    pub alert_days: u32,
    /// A forecast alert within this many days is critical rather than a warning.
    pub critical_days: u32,
    /// A volume that would take longer than this to fill is "not filling".
    pub horizon_days: u32,
    /// An unexplained rise in free space smaller than this is never treated as a cleanup.
    pub jump_min_bytes: u64,
}

impl Default for ForecastPolicy {
    fn default() -> Self {
        ForecastPolicy {
            window_days: 30,
            reserve_bytes: 10 * GIB,
            min_samples: 4,
            min_span_hours: 48,
            alert_days: 14,
            critical_days: 3,
            horizon_days: 365,
            jump_min_bytes: GIB,
        }
    }
}

/// Bytes known to have been freed on the volume at a moment (e.g. a cleanup's measured
/// result). Lets the forecast ignore the jump instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// When the bytes were freed (Unix seconds).
    pub at: i64,
    /// How many bytes were freed.
    pub bytes: u64,
}

/// How much to trust a forecast. Ordered: `Unknown < Low < Medium < High`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
// Named apart from the reclaim estimate's `Confidence` (scull-core) in shared JSON Schemas, so
// payloads that carry both (the desktop's) have one definition per name.
#[schemars(rename = "ForecastConfidence")]
pub enum Confidence {
    /// Not enough history to say anything.
    Unknown,
    /// Short history, noisy data or a long extrapolation; not shown in status lines.
    Low,
    /// Shown in status lines and may alert.
    Medium,
    /// Long, regular history and a short extrapolation.
    High,
}

impl Confidence {
    /// One level lower, never below [`Confidence::Low`] (unknown stays unknown).
    pub fn lower(self) -> Confidence {
        match self {
            Confidence::High => Confidence::Medium,
            Confidence::Medium | Confidence::Low => Confidence::Low,
            Confidence::Unknown => Confidence::Unknown,
        }
    }

    /// `high`, `medium`, `low`, `unknown`.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Unknown => "unknown",
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
        }
    }
}

/// What the forecast concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForecastState {
    /// Too few samples or too short a span in the window; no extrapolation.
    NotEnoughHistory,
    /// Free space is stable or growing, or would take longer than the horizon to run out.
    NotFilling,
    /// Free space is shrinking: see `days_until_threshold`.
    Filling,
    /// Free space is already at or below the reserve (measured, not forecast).
    BelowReserve,
}

/// The forecast of one volume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Forecast {
    /// Volume identifier, as recorded in history (mount point or volume GUID path).
    pub volume_id: String,
    /// What the forecast concluded.
    pub state: ForecastState,
    /// How much to trust it.
    pub confidence: Confidence,
    /// When the latest sample in the window was taken (Unix seconds).
    pub as_of: Option<i64>,
    /// Latest free space, plus releases recorded after that sample (bytes).
    pub free_bytes: Option<u64>,
    /// Volume size at the latest sample (bytes).
    pub total_bytes: Option<u64>,
    /// The reserve the forecast counts down to (bytes).
    pub reserve_bytes: u64,
    /// Free space lost per day by the trend (negative when free space grows). Present once
    /// there is enough history.
    pub fill_bytes_per_day: Option<f64>,
    /// Days from `now` until free space drops below the reserve (`Filling`; 0 for
    /// `BelowReserve`).
    pub days_until_threshold: Option<f64>,
    /// When free space is forecast to drop below the reserve (Unix seconds).
    pub threshold_at: Option<i64>,
    /// Samples fitted (after dropping outliers).
    pub samples_used: u32,
    /// Seconds between the first and the latest sample in the window.
    pub span_secs: i64,
    /// Transient outliers dropped.
    pub outliers_ignored: u32,
    /// Steps levelled out of the trend: known releases plus detected cleanups and one-off
    /// drops.
    pub jumps_ignored: u32,
    /// Residual spread relative to the change the trend explains over the span (lower is
    /// steadier).
    pub noise_ratio: Option<f64>,
}

impl Forecast {
    /// Whole days until the reserve is reached, as shown to people: at least 1 while
    /// filling, 0 when already below the reserve.
    pub fn days_rounded(&self) -> Option<u32> {
        match self.state {
            ForecastState::BelowReserve => Some(0),
            ForecastState::Filling => self
                .days_until_threshold
                .map(|d| d.round().clamp(1.0, f64::from(u32::MAX)) as u32),
            _ => None,
        }
    }

    /// Whether a status line or alert may show this forecast: filling with at least medium
    /// confidence, or already below the reserve.
    pub fn is_actionable(&self) -> bool {
        match self.state {
            ForecastState::BelowReserve => true,
            ForecastState::Filling => self.confidence >= Confidence::Medium,
            _ => false,
        }
    }
}

/// Forecast `volume_id` from its free-space `samples` (samples of other volumes are
/// ignored; any order) and the known `releases` on that volume. Pure: `now` is injected.
pub fn forecast(
    volume_id: &str,
    samples: &[VolumeSample],
    releases: &[Release],
    now: i64,
    policy: &ForecastPolicy,
) -> Forecast {
    let since = now.saturating_sub(i64::from(policy.window_days).saturating_mul(DAY_SECS));
    let mut pts: Vec<&VolumeSample> = samples
        .iter()
        .filter(|s| s.volume_id == volume_id && s.at >= since && s.at <= now && s.free <= s.total)
        .collect();
    pts.sort_by_key(|s| s.at);
    // Equal timestamps: keep the later one (storage returns samples in insertion order).
    let mut uniq: Vec<&VolumeSample> = Vec::with_capacity(pts.len());
    for s in pts {
        match uniq.last_mut() {
            Some(last) if last.at == s.at => *last = s,
            _ => uniq.push(s),
        }
    }

    let mut out = Forecast {
        volume_id: volume_id.to_owned(),
        state: ForecastState::NotEnoughHistory,
        confidence: Confidence::Unknown,
        as_of: None,
        free_bytes: None,
        total_bytes: None,
        reserve_bytes: policy.reserve_bytes,
        fill_bytes_per_day: None,
        days_until_threshold: None,
        threshold_at: None,
        samples_used: 0,
        span_secs: 0,
        outliers_ignored: 0,
        jumps_ignored: 0,
        noise_ratio: None,
    };
    let (Some(first), Some(last)) = (uniq.first().copied(), uniq.last().copied()) else {
        return out;
    };
    let later_releases: u64 = releases
        .iter()
        .filter(|r| r.at > last.at && r.at <= now)
        .fold(0u64, |a, r| a.saturating_add(r.bytes));
    let anchor = last.free.saturating_add(later_releases).min(last.total);
    out.as_of = Some(last.at);
    out.free_bytes = Some(anchor);
    out.total_bytes = Some(last.total);
    out.span_secs = last.at - first.at;
    out.samples_used = u32::try_from(uniq.len()).unwrap_or(u32::MAX);

    if anchor <= policy.reserve_bytes {
        out.state = ForecastState::BelowReserve;
        out.confidence = Confidence::High;
        out.days_until_threshold = Some(0.0);
        out.threshold_at = Some(last.at);
        return out;
    }
    if uniq.len() < policy.min_samples.max(2) as usize
        || out.span_secs < i64::from(policy.min_span_hours) * 3_600
        || out.span_secs <= 0
    {
        return out;
    }

    let uniq = thin(&uniq, MAX_FIT_POINTS);
    let series: Vec<Point> = uniq
        .iter()
        .map(|s| Point {
            at: s.at,
            t: (s.at - last.at) as f64 / DAY_SECS as f64,
            y: s.free as f64,
        })
        .collect();
    let Some(fit) = fit_levelled(&series, releases, policy) else {
        return out;
    };
    out.samples_used = u32::try_from(fit.points).unwrap_or(u32::MAX);
    out.outliers_ignored = u32::try_from(fit.outliers).unwrap_or(u32::MAX);
    out.jumps_ignored = u32::try_from(fit.jumps).unwrap_or(u32::MAX);

    let span_days = out.span_secs as f64 / DAY_SECS as f64;
    let fill = -fit.slope;
    out.fill_bytes_per_day = Some(fill);
    let history_cap = if fit.points >= 10 && span_days >= 14.0 {
        Confidence::High
    } else if fit.points >= 6 && span_days >= 7.0 {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    out.state = ForecastState::NotFilling;
    out.confidence = history_cap;
    if fill <= 0.0 {
        return out;
    }
    let days_from_last = (anchor - policy.reserve_bytes) as f64 / fill;
    if !days_from_last.is_finite() || days_from_last > f64::from(policy.horizon_days) {
        return out;
    }

    let noise = fit.sigma / (fit.slope.abs() * span_days).max(1.0);
    out.noise_ratio = Some(noise);
    let noise_cap = if noise <= 0.10 {
        Confidence::High
    } else if noise <= 0.25 {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    let mut confidence = history_cap.min(noise_cap);
    let reach = days_from_last / span_days;
    if reach > 6.0 {
        confidence = confidence.min(Confidence::Low);
    } else if reach > 2.0 {
        confidence = confidence.lower();
    }

    let threshold_at = last
        .at
        .saturating_add((days_from_last * DAY_SECS as f64).round() as i64);
    out.state = ForecastState::Filling;
    out.confidence = confidence;
    out.threshold_at = Some(threshold_at);
    out.days_until_threshold =
        Some((threshold_at.saturating_sub(now)).max(0) as f64 / DAY_SECS as f64);
    out
}

/// One sample on the fitting axis: `t` in days relative to the latest sample (≤ 0).
#[derive(Debug, Clone, Copy)]
struct Point {
    at: i64,
    t: f64,
    y: f64,
}

/// The robust trend of a levelled series.
#[derive(Debug, Clone, Copy)]
struct Fit {
    /// Bytes of free space gained per day (negative when filling).
    slope: f64,
    /// Robust residual spread (bytes).
    sigma: f64,
    points: usize,
    outliers: usize,
    jumps: usize,
}

/// Keep at most `max` samples, evenly spaced by index, always keeping the first and last.
fn thin<'a>(pts: &[&'a VolumeSample], max: usize) -> Vec<&'a VolumeSample> {
    if pts.len() <= max || max < 2 {
        return pts.to_vec();
    }
    let n = pts.len();
    (0..max).map(|i| pts[i * (n - 1) / (max - 1)]).collect()
}

/// Per-interval analysis: known releases, and the change beyond what the median rate
/// predicts for the interval's length.
struct Intervals {
    /// Bytes known to have been freed in `(at[k], at[k+1]]`.
    released: Vec<f64>,
    /// Change of free space over interval `k`, known releases subtracted.
    change: Vec<f64>,
    /// Unexplained change of interval `k` beyond the median rate.
    excess: Vec<f64>,
    /// A step larger than this is a jump.
    threshold: f64,
}

fn intervals(series: &[Point], releases: &[Release], policy: &ForecastPolicy) -> Option<Intervals> {
    let mut released = Vec::with_capacity(series.len().saturating_sub(1));
    let mut change = Vec::with_capacity(series.len().saturating_sub(1));
    let mut rates = Vec::with_capacity(series.len().saturating_sub(1));
    for w in series.windows(2) {
        let (a, b) = (w[0], w[1]);
        let r: u64 = releases
            .iter()
            .filter(|r| r.at > a.at && r.at <= b.at)
            .fold(0u64, |acc, r| acc.saturating_add(r.bytes));
        let r = r as f64;
        let d = b.y - a.y - r;
        released.push(r);
        change.push((d, b.t - a.t));
        rates.push(d / (b.t - a.t));
    }
    let rate = stats::median(&rates)?;
    let excess: Vec<f64> = change.iter().map(|&(d, dt)| d - rate * dt).collect();
    let sigma = stats::robust_sigma(&excess)?;
    let threshold = (policy.jump_min_bytes as f64).max(5.0 * sigma);
    Some(Intervals {
        released,
        change: change.iter().map(|&(d, _)| d).collect(),
        excess,
        threshold,
    })
}

/// Drop transient outliers, level cleanups, fit Theil–Sen.
fn fit_levelled(series: &[Point], releases: &[Release], policy: &ForecastPolicy) -> Option<Fit> {
    // Transient outliers: free space moves a lot one way and straight back (the changes
    // themselves flip sign, not just their excess over the median rate, so a quiet weekend
    // between busy weekdays is not an outlier).
    let iv = intervals(series, releases, policy)?;
    let min_step = policy.jump_min_bytes as f64;
    let mut keep = vec![true; series.len()];
    let mut k = 0;
    while k + 1 < iv.excess.len() {
        let (a, b) = (iv.excess[k], iv.excess[k + 1]);
        let (da, db) = (iv.change[k], iv.change[k + 1]);
        let small = a.abs().min(b.abs());
        if a.signum() != b.signum()
            && da.signum() != db.signum()
            && da.abs().min(db.abs()) >= min_step
            && small >= iv.threshold
            && (a + b).abs() <= 0.5 * small
        {
            keep[k + 1] = false;
            k += 2;
        } else {
            k += 1;
        }
    }
    let cleaned: Vec<Point> = series
        .iter()
        .zip(&keep)
        .filter_map(|(p, &k)| k.then_some(*p))
        .collect();
    let outliers = series.len() - cleaned.len();

    // Level known releases and unexplained steps: free space actually moved by at least
    // `jump_min_bytes` and by far more than the median rate explains. Upward steps are
    // cleanups. Downward steps are one-off consumption (an OS update, a VM image) only while
    // they are rare, at most one per 14 days of history; more often than that they are how
    // this machine consumes space, and they stay in the trend.
    let iv = intervals(&cleaned, releases, policy)?;
    let is_drop = |k: usize| iv.change[k] <= -min_step && iv.excess[k] <= -iv.threshold;
    let span_days = cleaned.last()?.t - cleaned.first()?.t;
    let drops_allowed = ((span_days / 14.0).floor() as usize).max(1);
    let level_drops = (0..iv.excess.len()).filter(|&k| is_drop(k)).count() <= drops_allowed;
    let mut levelled: Vec<(f64, f64)> = cleaned.iter().map(|p| (p.t, p.y)).collect();
    let mut shift = 0.0;
    let mut jumps = 0;
    for k in (0..iv.excess.len()).rev() {
        let mut step = iv.released[k];
        if step > 0.0 {
            jumps += 1;
        }
        let rise = iv.change[k] >= min_step && iv.excess[k] >= iv.threshold;
        if rise || (level_drops && is_drop(k)) {
            step += iv.excess[k];
            jumps += 1;
        }
        shift += step;
        levelled[k].1 += shift;
    }

    // Pairs at least a week apart (half the span for shorter histories), so the weekly rhythm
    // of development work averages out instead of biasing the slope toward busy days.
    let span = levelled.last()?.0 - levelled.first()?.0;
    let line = stats::theil_sen_gapped(&levelled, (span / 2.0).min(7.0))?;
    let residuals: Vec<f64> = levelled.iter().map(|&(t, y)| y - line.at(t)).collect();
    let sigma = stats::robust_sigma(&residuals)?;
    Some(Fit {
        slope: line.slope,
        sigma,
        points: levelled.len(),
        outliers,
        jumps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const NOW: i64 = 1_800_000_000;
    const VOL: &str = "/";

    fn sample(at: i64, free: f64) -> VolumeSample {
        VolumeSample {
            volume_id: VOL.to_owned(),
            total: 1_000 * GIB,
            free: free.clamp(0.0, (1_000 * GIB) as f64) as u64,
            at,
        }
    }

    /// One sample per day for `days` days ending at `NOW`, free = f(day index from start).
    fn daily(days: i64, f: impl Fn(i64) -> f64) -> Vec<VolumeSample> {
        (0..=days)
            .map(|i| sample(NOW - (days - i) * DAY_SECS, f(i)))
            .collect()
    }

    fn g(x: f64) -> f64 {
        x * GIB as f64
    }

    fn run(samples: &[VolumeSample]) -> Forecast {
        forecast(VOL, samples, &[], NOW, &ForecastPolicy::default())
    }

    #[test]
    fn linear_fill_is_exact_and_high_confidence() {
        // 200 GiB free at the start, losing 2 GiB/day for 20 days: 160 GiB free now,
        // (160 - 10) / 2 = 75 days to go... too far for high; use a faster fill.
        let s = daily(20, |i| g(100.0) - g(4.0) * i as f64);
        let f = run(&s);
        assert_eq!(f.state, ForecastState::Filling);
        assert!((f.fill_bytes_per_day.expect("rate") - g(4.0)).abs() < 1.0);
        // 20 GiB free now, 10 GiB reserve, 4 GiB/day: 2.5 days.
        assert!(
            (f.days_until_threshold.expect("days") - 2.5).abs() < 1e-6,
            "{f:?}"
        );
        assert_eq!(f.days_rounded(), Some(3));
        assert_eq!(f.threshold_at, Some(NOW + DAY_SECS * 5 / 2));
        assert_eq!(f.confidence, Confidence::High);
        assert!(f.is_actionable());
        assert_eq!(f.free_bytes, Some(20 * GIB));
        assert_eq!(f.samples_used, 21);
        assert_eq!(f.jumps_ignored, 0);
    }

    #[test]
    fn long_extrapolation_lowers_confidence() {
        // 14 days observed, ~45 days to go: reach > 2 → one level down.
        let s = daily(14, |i| g(150.0) - g(2.0) * i as f64);
        let f = run(&s);
        assert_eq!(f.state, ForecastState::Filling);
        assert_eq!(f.confidence, Confidence::Medium, "{f:?}");
        // ~ 300 days to go from 14 days observed: low.
        let s = daily(14, |i| g(330.0) - g(1.0) * i as f64);
        let f = run(&s);
        assert_eq!(f.state, ForecastState::Filling);
        assert_eq!(f.confidence, Confidence::Low, "{f:?}");
        assert!(!f.is_actionable());
    }

    #[test]
    fn cleanup_drops_are_ignored() {
        // 3 GiB/day fill with two cleanups (+40 GiB on day 8, +25 GiB on day 15).
        let s = daily(20, |i| {
            let mut free = g(150.0) - g(3.0) * i as f64;
            if i >= 8 {
                free += g(40.0);
            }
            if i >= 15 {
                free += g(25.0);
            }
            free
        });
        let f = run(&s);
        assert_eq!(f.state, ForecastState::Filling);
        assert_eq!(f.jumps_ignored, 2);
        assert!(
            (f.fill_bytes_per_day.expect("rate") - g(3.0)).abs() < g(0.01),
            "{f:?}"
        );
        // Without levelling a plain fit would see free space growing.
        let naive = stats::theil_sen(
            &s.iter()
                .map(|x| (x.at as f64, x.free as f64))
                .collect::<Vec<_>>(),
        )
        .expect("fit");
        assert!(
            naive.slope * DAY_SECS as f64 > -g(3.0) * 0.8,
            "jumps distort a naive fit"
        );
    }

    #[test]
    fn known_releases_are_levelled_even_when_small() {
        // A 0.6 GiB cleanup (below jump_min_bytes) is still levelled when recorded.
        let s = daily(20, |i| {
            let mut free = g(100.0) - g(0.2) * i as f64;
            if i >= 10 {
                free += g(0.6);
            }
            free
        });
        let at = s[10].at - 3_600;
        let with = forecast(
            VOL,
            &s,
            &[Release {
                at,
                bytes: g(0.6) as u64,
            }],
            NOW,
            &ForecastPolicy::default(),
        );
        assert_eq!(with.jumps_ignored, 1);
        assert!(
            (with.fill_bytes_per_day.expect("rate") - g(0.2)).abs() < 1.0,
            "{with:?}"
        );
    }

    #[test]
    fn release_after_last_sample_adds_to_free_space() {
        let s = daily(20, |i| g(100.0) - g(4.0) * i as f64);
        let r = Release {
            at: NOW,
            bytes: 8 * GIB,
        };
        let f = forecast(VOL, &s[..20], &[r], NOW, &ForecastPolicy::default());
        // Last sample (day 19) had 24 GiB; +8 GiB freed since.
        assert_eq!(f.free_bytes, Some(32 * GIB));
    }

    #[test]
    fn transient_spike_is_an_outlier() {
        let s = daily(20, |i| {
            let free = g(200.0) - g(5.0) * i as f64;
            if i == 9 { free - g(60.0) } else { free }
        });
        let f = run(&s);
        assert_eq!(f.outliers_ignored, 1, "{f:?}");
        assert_eq!(f.jumps_ignored, 0, "{f:?}");
        assert!(
            (f.fill_bytes_per_day.expect("rate") - g(5.0)).abs() < 1.0,
            "{f:?}"
        );
    }

    #[test]
    fn rare_drops_are_one_off_but_regular_drops_are_the_trend() {
        // One 25 GiB OS update on day 10 of a steady 3 GiB/day fill: levelled.
        let s = daily(20, |i| {
            let free = g(200.0) - g(3.0) * i as f64;
            if i >= 10 { free - g(25.0) } else { free }
        });
        let f = run(&s);
        assert_eq!(f.jumps_ignored, 1, "{f:?}");
        assert!(
            (f.fill_bytes_per_day.expect("rate") - g(3.0)).abs() < g(0.01),
            "{f:?}"
        );
        assert_eq!(
            f.free_bytes,
            Some(115 * GIB),
            "the drop still counts in free space"
        );
        // 8 GiB pulled every other day, nothing in between: that is the consumption.
        let s = daily(20, |i| g(200.0) - g(8.0) * (i / 2) as f64);
        let f = run(&s);
        assert_eq!(f.jumps_ignored, 0, "{f:?}");
        assert!(
            (f.fill_bytes_per_day.expect("rate") - g(4.0)).abs() < g(0.5),
            "{f:?}"
        );
    }

    #[test]
    fn flat_and_growing_are_not_filling() {
        let f = run(&daily(20, |_| g(41.0)));
        assert_eq!(f.state, ForecastState::NotFilling);
        assert_eq!(f.days_until_threshold, None);
        assert_eq!(f.confidence, Confidence::High);
        assert!(!f.is_actionable());
        let f = run(&daily(20, |i| g(41.0) + g(1.0) * i as f64));
        assert_eq!(f.state, ForecastState::NotFilling);
        assert!(f.fill_bytes_per_day.expect("rate") < 0.0);
        // Slow enough to take more than a year: not filling either.
        let f = run(&daily(20, |i| g(500.0) - g(0.5) * i as f64));
        assert_eq!(f.state, ForecastState::NotFilling);
    }

    #[test]
    fn not_enough_history() {
        let p = ForecastPolicy::default();
        // Three samples (needs four).
        let f = run(&daily(2, |i| g(100.0) - g(9.0) * i as f64));
        assert_eq!(f.state, ForecastState::NotEnoughHistory);
        assert_eq!(f.confidence, Confidence::Unknown);
        assert_eq!(f.days_until_threshold, None);
        assert_eq!(f.free_bytes, Some(82 * GIB));
        // Ten samples within a day (needs two days).
        let s: Vec<_> = (0..10)
            .map(|i| sample(NOW - (9 - i) * 3_600, g(100.0) - g(1.0) * i as f64))
            .collect();
        assert_eq!(run(&s).state, ForecastState::NotEnoughHistory);
        // Plenty of samples, but all older than the window.
        let old: Vec<_> = (0..20)
            .map(|i| sample(NOW - (60 - i) * DAY_SECS, g(100.0) - g(1.0) * i as f64))
            .collect();
        let f = run(&old);
        assert_eq!(f.state, ForecastState::NotEnoughHistory);
        assert_eq!(f.as_of, None);
        // No samples at all.
        let f = forecast(VOL, &[], &[], NOW, &p);
        assert_eq!(f.state, ForecastState::NotEnoughHistory);
        assert_eq!(f.samples_used, 0);
    }

    #[test]
    fn below_reserve_needs_one_sample() {
        let f = run(&[sample(NOW - 60, g(6.0))]);
        assert_eq!(f.state, ForecastState::BelowReserve);
        assert_eq!(f.days_rounded(), Some(0));
        assert!(f.is_actionable());
    }

    #[test]
    fn other_volumes_future_and_invalid_samples_are_ignored() {
        let mut s = daily(20, |i| g(100.0) - g(4.0) * i as f64);
        s.push(VolumeSample {
            volume_id: "/data".into(),
            total: GIB,
            free: 0,
            at: NOW - 10,
        });
        s.push(sample(NOW + 3_600, g(1.0)));
        s.push(VolumeSample {
            volume_id: VOL.into(),
            total: GIB,
            free: 2 * GIB,
            at: NOW - 5,
        });
        let f = run(&s);
        assert_eq!(f.samples_used, 21);
        assert_eq!(f.free_bytes, Some(20 * GIB));
    }

    #[test]
    fn noisy_data_is_low_confidence() {
        // Tiny trend buried in ±8 GiB noise.
        let s = daily(20, |i| {
            let noise = if i % 2 == 0 { g(8.0) } else { -g(8.0) };
            g(60.0) - g(0.5) * i as f64 + noise
        });
        let f = run(&s);
        assert_eq!(f.state, ForecastState::Filling, "{f:?}");
        assert_eq!(f.confidence, Confidence::Low, "{f:?}");
        assert!(f.noise_ratio.expect("noise") > 0.25, "{f:?}");
        assert!(!f.is_actionable());
    }

    #[test]
    fn serializes_snake_case() {
        let f = run(&daily(20, |i| g(100.0) - g(4.0) * i as f64));
        let v = serde_json::to_value(&f).expect("json");
        assert_eq!(v["state"], "filling");
        assert_eq!(v["confidence"], "high");
        let p: ForecastPolicy = serde_json::from_str("{\"alert_days\": 7}").expect("policy");
        assert_eq!(p.alert_days, 7);
        assert_eq!(p.reserve_bytes, 10 * GIB);
        assert!(Confidence::Unknown < Confidence::Low && Confidence::Medium < Confidence::High);
    }

    /// Small deterministic noise in [-1, 1].
    fn jitter(seed: u64, i: i64) -> f64 {
        let mut x = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        (x % 2_001) as f64 / 1_000.0 - 1.0
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        /// Linear fill with bounded noise and random cleanups: the rate is recovered within
        /// 10%, and the days follow from it.
        ///
        /// Cleanups are generated in the regime the method promises to handle (step 5): each
        /// one is an *upward* step in free space of at least `jump_min_bytes` even after the
        /// day's consumption and the worst-case noise of both samples. A cleanup smaller than
        /// a day's consumption leaves free space still falling over that interval; with daily
        /// samples it is indistinguishable from a quiet day and biases any trend (two such
        /// cleanups of 5 and 6.3 GiB at 8.6 GiB/day once read as 7.6 GiB/day).
        #[test]
        fn recovers_the_rate(
            rate in 1.0f64..12.0,
            days in 14i64..30,
            noise_frac in 0.0f64..0.15,
            seed in any::<u64>(),
            cleanups in proptest::collection::vec((2i64..28, 1.0f64..60.0), 0..3),
        ) {
            let start = g(rate * days as f64 + 40.0);
            // Day's consumption + noise swing of two samples + jump_min (1 GiB) + margin.
            let visible = rate * (1.0 + 2.0 * noise_frac) + 1.0;
            let cleanups: Vec<(i64, f64)> =
                cleanups.iter().map(|&(d, extra)| (d, visible + extra)).collect();
            let s = daily(days, |i| {
                let mut free = start - g(rate) * i as f64 + g(rate) * noise_frac * jitter(seed, i);
                for &(d, freed) in &cleanups {
                    if i >= d.min(days) {
                        free += g(freed);
                    }
                }
                free
            });
            let f = run(&s);
            let got = f.fill_bytes_per_day.expect("rate");
            prop_assert!((got - g(rate)).abs() <= 0.10 * g(rate), "rate {} vs {}: {:?}", got / g(1.0), rate, f);
            prop_assert!(matches!(f.state, ForecastState::Filling | ForecastState::NotFilling));
            if let (ForecastState::Filling, Some(d)) = (f.state, f.days_until_threshold) {
                let expect = (f.free_bytes.expect("free") as f64 - g(10.0)) / got;
                prop_assert!((d - expect).abs() < 0.01, "{} vs {}", d, expect);
            }
        }

        /// Order does not matter; days are never negative; confidence is unknown exactly
        /// when there is not enough history.
        #[test]
        fn invariants(
            frees in proptest::collection::vec(0.0f64..500.0, 0..40),
            step in 1_800i64..(3 * DAY_SECS),
            rot in 0usize..40,
        ) {
            let n = frees.len() as i64;
            let s: Vec<_> = frees.iter().enumerate()
                .map(|(i, &f)| sample(NOW - (n - 1 - i as i64) * step, g(f)))
                .collect();
            let a = run(&s);
            let mut shuffled = s.clone();
            if !shuffled.is_empty() {
                let r = rot % shuffled.len();
                shuffled.rotate_left(r);
                shuffled.reverse();
            }
            let b = run(&shuffled);
            prop_assert_eq!(&a, &b);
            if let Some(d) = a.days_until_threshold {
                prop_assert!(d >= 0.0);
            }
            prop_assert_eq!(a.confidence == Confidence::Unknown, a.state == ForecastState::NotEnoughHistory);
            if a.state == ForecastState::Filling {
                prop_assert!(a.fill_bytes_per_day.expect("rate") > 0.0);
                prop_assert!(a.threshold_at.is_some());
            }
        }

        /// Free space that never shrinks is never "filling".
        #[test]
        fn growing_is_never_filling(
            frees in proptest::collection::vec(11.0f64..500.0, 4..40),
        ) {
            let mut sorted = frees.clone();
            sorted.sort_by(f64::total_cmp);
            let n = sorted.len() as i64;
            let s: Vec<_> = sorted.iter().enumerate()
                .map(|(i, &f)| sample(NOW - (n - 1 - i as i64) * DAY_SECS, g(f)))
                .collect();
            prop_assert_ne!(run(&s).state, ForecastState::Filling);
        }
    }
}
