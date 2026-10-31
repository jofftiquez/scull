//! Small robust statistics: median, median absolute deviation, Theil–Sen line fit.
//!
//! All functions ignore nothing and allocate a sorted copy; inputs are at most a few hundred
//! points (see [`crate::forecast::MAX_FIT_POINTS`]).

/// Scale factor turning a median absolute deviation into a standard-deviation estimate for
/// normally distributed noise.
pub const MAD_TO_SIGMA: f64 = 1.4826;

/// Median of `values` (mean of the two middle values for an even count); `None` when empty
/// or when any value is NaN.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() || values.iter().any(|v| v.is_nan()) {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    })
}

/// Scale factor turning an interquartile range into a standard-deviation estimate for
/// normally distributed noise.
pub const IQR_TO_SIGMA: f64 = 1.0 / 1.349;

/// The `q` quantile (0 ≤ q ≤ 1) with linear interpolation; `None` when empty or NaN.
pub fn quantile(values: &[f64], q: f64) -> Option<f64> {
    if values.is_empty() || values.iter().any(|v| v.is_nan()) || !(0.0..=1.0).contains(&q) {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let pos = q * (v.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    Some(v[lo] + (v[hi] - v[lo]) * (pos - lo as f64))
}

/// Robust spread: the larger of [`MAD_TO_SIGMA`] × the median absolute deviation and
/// [`IQR_TO_SIGMA`] × the interquartile range. Each ignores a minority of outliers; the
/// interquartile range keeps two-valued data (alternating noise), whose MAD can be zero,
/// from looking noiseless.
pub fn robust_sigma(values: &[f64]) -> Option<f64> {
    let m = median(values)?;
    let dev: Vec<f64> = values.iter().map(|v| (v - m).abs()).collect();
    let mad = median(&dev)? * MAD_TO_SIGMA;
    let iqr = (quantile(values, 0.75)? - quantile(values, 0.25)?) * IQR_TO_SIGMA;
    Some(mad.max(iqr))
}

/// A fitted line `y = intercept + slope · x`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Line {
    /// Change of `y` per unit of `x`.
    pub slope: f64,
    /// `y` at `x = 0`.
    pub intercept: f64,
}

impl Line {
    /// `y` at `x`.
    pub fn at(&self, x: f64) -> f64 {
        self.intercept + self.slope * x
    }
}

/// Theil–Sen estimator: the slope is the median of the slopes of every pair of points with
/// distinct `x`, the intercept the median of `y - slope · x`. Tolerates up to ~29% outliers.
/// `None` with fewer than two distinct `x`.
pub fn theil_sen(points: &[(f64, f64)]) -> Option<Line> {
    theil_sen_gapped(points, 0.0)
}

/// [`theil_sen`] over the pairs at least `min_dx` apart (all pairs with distinct `x` if none
/// are). With `min_dx` of one cycle, a periodic pattern (busy weekdays, quiet weekends) no
/// longer pulls the slope toward its dominant phase: every pair spans a whole cycle, so its
/// slope is close to the average rate.
pub fn theil_sen_gapped(points: &[(f64, f64)], min_dx: f64) -> Option<Line> {
    let mut slopes = Vec::with_capacity(points.len() * points.len().saturating_sub(1) / 2);
    let mut near = Vec::new();
    for (i, &(xi, yi)) in points.iter().enumerate() {
        for &(xj, yj) in &points[i + 1..] {
            let dx = xj - xi;
            if dx.abs() >= min_dx && dx != 0.0 {
                slopes.push((yj - yi) / dx);
            } else if dx != 0.0 {
                near.push((yj - yi) / dx);
            }
        }
    }
    if slopes.is_empty() {
        slopes = near;
    }
    let slope = median(&slopes)?;
    let offsets: Vec<f64> = points.iter().map(|&(x, y)| y - slope * x).collect();
    let intercept = median(&offsets)?;
    Some(Line { slope, intercept })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medians() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&[1.0, f64::NAN]), None);
        assert_eq!(robust_sigma(&[1.0, 1.0, 1.0]), Some(0.0));
        let s = robust_sigma(&[1.0, 2.0, 3.0, 4.0, 100.0]).expect("sigma");
        assert!((s - 1.4826).abs() < 1e-3, "outlier ignored: {s}");
        // Alternating values: MAD is 0 for an odd count, the IQR is not.
        let alt: Vec<f64> = (0..21)
            .map(|i| if i % 2 == 0 { 8.0 } else { -8.0 })
            .collect();
        assert!(robust_sigma(&alt).expect("sigma") > 10.0);
        assert_eq!(quantile(&[1.0, 2.0, 3.0], 0.5), Some(2.0));
        assert_eq!(quantile(&[1.0, 2.0], 0.25), Some(1.25));
        assert_eq!(quantile(&[1.0], 2.0), None);
    }

    #[test]
    fn theil_sen_ignores_outliers() {
        let mut pts: Vec<(f64, f64)> = (0..20)
            .map(|i| (f64::from(i), 100.0 - 2.0 * f64::from(i)))
            .collect();
        pts[5].1 = -500.0;
        pts[12].1 = 900.0;
        let line = theil_sen(&pts).expect("fit");
        assert!((line.slope + 2.0).abs() < 1e-9, "{line:?}");
        assert!((line.intercept - 100.0).abs() < 1e-9, "{line:?}");
        assert!((line.at(10.0) - 80.0).abs() < 1e-9);
        assert_eq!(theil_sen(&[(1.0, 1.0), (1.0, 2.0)]), None);
        // Weekly pattern: 4/day on weekdays, 1/day on weekends (average 22/7). Plain
        // Theil–Sen leans toward the weekdays; pairs a week or more apart see the average.
        let mut y = 0.0;
        let pts: Vec<(f64, f64)> = (0..28)
            .map(|d| {
                let p = (f64::from(d), y);
                y -= if d % 7 >= 5 { 1.0 } else { 4.0 };
                p
            })
            .collect();
        let gapped = theil_sen_gapped(&pts, 7.0).expect("fit").slope;
        assert!((gapped + 22.0 / 7.0).abs() < 0.1, "{gapped}");
        assert_eq!(
            theil_sen_gapped(&[(0.0, 0.0), (1.0, 1.0)], 5.0)
                .expect("fallback")
                .slope,
            1.0
        );
        assert_eq!(theil_sen(&[]), None);
    }
}
