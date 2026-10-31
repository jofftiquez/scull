//! Elevation detection (P1-PLAT-04; SAFE-14, T-35).
//!
//! Destructive operations refuse to run as root or administrator unless `--allow-root`, and
//! scull never requests elevation itself (SAFE-14). This module only *detects*; the refusal
//! lives in the executor (FX-EXEC-12).
//!
//! * **Unix (Linux, macOS):** elevated when the effective user ID is 0 (`geteuid`), which
//!   covers `sudo`, `su`, `doas` and setuid-root launches. Detection cannot fail, so
//!   [`Elevation::Unknown`] never occurs.
//! * **Windows:** `GetTokenInformation(TokenElevation)` on the process token: true for an
//!   elevated administrator ("Run as administrator", UAC disabled, the built-in
//!   Administrator) and for services running as SYSTEM. A UAC-filtered administrator token is
//!   not elevated. If the token can't be queried the result is [`Elevation::Unknown`].
//!
//! [`Elevation::Unknown`] must be treated like [`Elevation::Elevated`] for destructive
//! operations ([`Elevation::blocks_destructive`]): when in doubt, refuse.

use serde::Serialize;
use std::ffi::OsString;
use std::fmt;

/// Whether this process runs with root/administrator rights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Elevation {
    /// Root (effective UID 0) or an elevated Windows token.
    Elevated,
    /// An ordinary user.
    NotElevated,
    /// The OS could not tell (Windows token query failed). Treated as elevated for safety.
    Unknown,
}

impl Elevation {
    /// Whether destructive operations must be refused without an explicit `--allow-root`:
    /// true for [`Elevation::Elevated`] and [`Elevation::Unknown`] (SAFE-14).
    pub fn blocks_destructive(self) -> bool {
        !matches!(self, Elevation::NotElevated)
    }

    /// Stable lowercase name, as used in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Elevation::Elevated => "elevated",
            Elevation::NotElevated => "not_elevated",
            Elevation::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Elevation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether this process is elevated (see the [module docs](self)).
pub fn is_elevated() -> Elevation {
    crate::native::elevation()
}

/// The process's privilege context, for `scull doctor` (PLT-09) and the executor's
/// root check (SAFE-14).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub struct Privileges {
    /// Whether the process is elevated.
    pub elevation: Elevation,
    /// The invoking user when started through `sudo` (`SUDO_USER`) or `doas` (`DOAS_USER`),
    /// so `doctor` can say "you ran scull with sudo as alice". Taken from the environment as
    /// a hint only: it is never used to decide anything.
    pub sudo_user: Option<String>,
}

impl Privileges {
    /// The current process's privileges.
    pub fn current() -> Self {
        Self::from_parts(is_elevated(), |var| std::env::var_os(var))
    }

    /// Builds the report from an [`Elevation`] and an environment lookup, without reading the
    /// real environment (for tests).
    pub fn from_parts(elevation: Elevation, env: impl Fn(&str) -> Option<OsString>) -> Self {
        let sudo_user = ["SUDO_USER", "DOAS_USER"]
            .into_iter()
            .filter_map(&env)
            .map(|v| v.to_string_lossy().into_owned())
            .find(|v| !v.is_empty());
        Self {
            elevation,
            sudo_user,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_blocks_like_elevated() {
        assert!(Elevation::Elevated.blocks_destructive());
        assert!(Elevation::Unknown.blocks_destructive());
        assert!(!Elevation::NotElevated.blocks_destructive());
    }

    #[test]
    fn json_names_are_stable() {
        for e in [
            Elevation::Elevated,
            Elevation::NotElevated,
            Elevation::Unknown,
        ] {
            assert_eq!(
                serde_json::to_value(e).unwrap(),
                serde_json::json!(e.as_str())
            );
            assert_eq!(e.to_string(), e.as_str());
        }
    }

    #[test]
    fn sudo_user_from_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        let p = Privileges::from_parts(Elevation::Elevated, env(&[("SUDO_USER", "alice")]));
        assert_eq!(p.sudo_user.as_deref(), Some("alice"));
        assert_eq!(p.elevation, Elevation::Elevated);

        let p = Privileges::from_parts(Elevation::Elevated, env(&[("DOAS_USER", "bob")]));
        assert_eq!(p.sudo_user.as_deref(), Some("bob"));

        let p = Privileges::from_parts(
            Elevation::NotElevated,
            env(&[("SUDO_USER", ""), ("DOAS_USER", "bob")]),
        );
        assert_eq!(
            p.sudo_user.as_deref(),
            Some("bob"),
            "empty SUDO_USER is unset"
        );

        let p = Privileges::from_parts(Elevation::NotElevated, env(&[]));
        assert_eq!(p.sudo_user, None);
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({ "elevation": "not_elevated", "sudo_user": null })
        );
    }

    #[test]
    fn capabilities_report_elevation_detection() {
        use crate::{Native, Platform};
        assert!(Native.capabilities().elevation_detection);
    }
}
