//! UTC calendar helpers (proleptic Gregorian, H. Hinnant's civil algorithms). No time zones:
//! every bucket in an insights report is a UTC day, ISO week (Monday start) or month.

/// Seconds per day.
pub const DAY: i64 = 86_400;

/// `(year, month 1..=12, day 1..=31)` of the UTC day containing Unix time `t`.
pub fn civil(t: i64) -> (i64, u32, u32) {
    let z = t.div_euclid(DAY) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Unix time of 00:00 UTC on `year-month-day`.
pub fn unix_of(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * DAY
}

/// 00:00 UTC of the day containing `t`.
pub fn day_start(t: i64) -> i64 {
    t - t.rem_euclid(DAY)
}

/// 00:00 UTC of the Monday of the week containing `t`.
pub fn week_start(t: i64) -> i64 {
    // 1970-01-01 was a Thursday: day 0 has weekday index 3 (Monday = 0).
    let days = t.div_euclid(DAY);
    (days - (days + 3).rem_euclid(7)) * DAY
}

/// 00:00 UTC on the first day of the month containing `t`.
pub fn month_start(t: i64) -> i64 {
    let (y, m, _) = civil(t);
    unix_of(y, m, 1)
}

/// 00:00 UTC on the first day of the month `n` months after the one containing `t` (`n` may
/// be negative).
pub fn add_months(t: i64, n: i64) -> i64 {
    let (y, m, _) = civil(t);
    let idx = y * 12 + i64::from(m) - 1 + n;
    unix_of(idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32, 1)
}

/// `YYYY-MM-DD` of the UTC day containing `t`.
pub fn iso_date(t: i64) -> String {
    let (y, m, d) = civil(t);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM` of the UTC month containing `t`.
pub fn iso_month(t: i64) -> String {
    let (y, m, _) = civil(t);
    format!("{y:04}-{m:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trips() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(unix_of(1970, 1, 1), 0);
        assert_eq!(iso_date(1_800_000_000), "2027-01-15");
        assert_eq!(unix_of(2000, 2, 29), 951_782_400);
        assert_eq!(civil(951_782_400), (2000, 2, 29));
        for t in (-10_000_000_000i64..10_000_000_000).step_by(7_777_777) {
            let (y, m, d) = civil(t);
            assert_eq!(unix_of(y, m, d), day_start(t), "{t}");
        }
    }

    #[test]
    fn buckets() {
        // 2027-01-15 is a Friday; its week starts Monday 2027-01-11.
        assert_eq!(iso_date(week_start(1_800_000_000)), "2027-01-11");
        assert_eq!(iso_date(week_start(unix_of(2027, 1, 11))), "2027-01-11");
        assert_eq!(iso_month(1_800_000_000), "2027-01");
        assert_eq!(iso_date(month_start(1_800_000_000)), "2027-01-01");
        assert_eq!(iso_date(add_months(1_800_000_000, -1)), "2026-12-01");
        assert_eq!(iso_date(add_months(1_800_000_000, 12)), "2028-01-01");
    }
}
