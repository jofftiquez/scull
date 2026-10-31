//! Human-readable durations for config values such as the activity window (`"24h"`).

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// A whole number of seconds written as `<n><unit>`, where the unit is `s`, `m`, `h`, `d` or `w`
/// (e.g. `"90m"`, `"24h"`, `"7d"`). Serialized as that string.
///
/// ```
/// use scull_core::config::DurationSpec;
///
/// let d: DurationSpec = "2d".parse().unwrap();
/// assert_eq!(d.as_secs(), 172_800);
/// assert_eq!(d.to_string(), "48h");
/// assert!("24".parse::<DurationSpec>().is_err()); // a unit is required
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DurationSpec {
    secs: u64,
}

impl DurationSpec {
    /// From a number of seconds.
    pub const fn from_secs(secs: u64) -> Self {
        Self { secs }
    }

    /// From a number of hours (saturating).
    pub const fn from_hours(hours: u64) -> Self {
        Self {
            secs: hours.saturating_mul(3600),
        }
    }

    /// The duration in seconds.
    pub const fn as_secs(self) -> u64 {
        self.secs
    }
}

impl fmt::Display for DurationSpec {
    /// The canonical form: hours when the value is a whole number of hours, else minutes, else
    /// seconds (`86400` → `"24h"`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.secs;
        if s.is_multiple_of(3600) {
            write!(f, "{}h", s / 3600)
        } else if s.is_multiple_of(60) {
            write!(f, "{}m", s / 60)
        } else {
            write!(f, "{s}s")
        }
    }
}

impl FromStr for DurationSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        let split = t
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("duration `{s}` needs a unit (s, m, h, d or w), e.g. `24h`"))?;
        let (num, unit) = t.split_at(split);
        if num.is_empty() {
            return Err(format!(
                "duration `{s}` must start with a number, e.g. `24h`"
            ));
        }
        let n: u64 = num
            .parse()
            .map_err(|_| format!("duration `{s}` is too large"))?;
        let mult: u64 = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 604_800,
            _ => {
                return Err(format!(
                    "duration `{s}` has an unknown unit `{unit}` (use s, m, h, d or w)"
                ));
            }
        };
        n.checked_mul(mult)
            .map(Self::from_secs)
            .ok_or_else(|| format!("duration `{s}` is too large"))
    }
}

impl Serialize for DurationSpec {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DurationSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for DurationSpec {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "DurationSpec".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A duration: a whole number followed by s, m, h, d or w (e.g. \"24h\").",
            "pattern": "^\\s*[0-9]+[smhdw]\\s*$"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        for (s, secs) in [
            ("0s", 0),
            ("45s", 45),
            ("90m", 5400),
            ("24h", 86_400),
            ("7d", 604_800),
            ("1w", 604_800),
            (" 3h ", 10_800),
        ] {
            assert_eq!(s.parse::<DurationSpec>().unwrap().as_secs(), secs, "{s}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        for s in [
            "",
            "h",
            "24",
            "24x",
            "-1h",
            "1.5h",
            "24 h",
            "99999999999999999999h",
        ] {
            assert!(s.parse::<DurationSpec>().is_err(), "{s:?} should fail");
        }
        assert!(
            format!("{}h", u64::MAX / 2)
                .parse::<DurationSpec>()
                .is_err()
        );
    }

    #[test]
    fn display_round_trips() {
        for secs in [0, 59, 60, 61, 3600, 5400, 86_400, 90_061] {
            let d = DurationSpec::from_secs(secs);
            assert_eq!(d.to_string().parse::<DurationSpec>().unwrap(), d);
        }
        assert_eq!(DurationSpec::from_hours(24).to_string(), "24h");
    }
}
