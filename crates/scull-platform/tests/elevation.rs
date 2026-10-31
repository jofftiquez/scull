//! Elevation detection (P1-PLAT-04, SAFE-14) against an independent oracle per OS:
//! Unix `id -u`; Windows the integrity level in `whoami /groups` (an elevated token runs at
//! High or System integrity), plus the CI expectation (GitHub's Windows runners are elevated,
//! its Linux and macOS runners are not root). Read-only: nothing is created or modified.

use scull_platform::{Elevation, Native, Platform, Privileges, is_elevated};
use std::process::{Command, Stdio};

fn on_ci() -> bool {
    std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some()
}

/// Output of a command, or `None` if it could not run or failed.
fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn elevation_is_known_and_reported_in_capabilities() {
    assert_ne!(
        is_elevated(),
        Elevation::Unknown,
        "detection must work on every OS"
    );
    assert!(Native.capabilities().elevation_detection);
    // Stable across calls.
    assert_eq!(is_elevated(), is_elevated());
}

#[cfg(unix)]
#[test]
fn unix_elevation_matches_id_u() {
    let uid = output("id", &["-u"]).expect("`id -u` runs");
    let expected = if uid.trim() == "0" {
        Elevation::Elevated
    } else {
        Elevation::NotElevated
    };
    assert_eq!(is_elevated(), expected, "id -u = {}", uid.trim());
    if on_ci() && uid.trim() != "0" {
        // GitHub's Linux and macOS runners run tests as an ordinary user.
        assert_eq!(is_elevated(), Elevation::NotElevated);
    }
}

#[cfg(windows)]
#[test]
fn windows_elevation_matches_integrity_level() {
    /// Mandatory-label SIDs of the High and System integrity levels.
    const HIGH: &str = "S-1-16-12288";
    const SYSTEM: &str = "S-1-16-16384";
    /// Medium and Medium Plus.
    const MEDIUM: [&str; 2] = ["S-1-16-8192", "S-1-16-8448"];

    let got = is_elevated();
    match output("whoami", &["/groups", "/fo", "csv", "/nh"]) {
        Some(groups) if groups.contains(HIGH) || groups.contains(SYSTEM) => {
            assert_eq!(
                got,
                Elevation::Elevated,
                "token runs at high/system integrity"
            );
        }
        Some(groups) if MEDIUM.iter().any(|m| groups.contains(m)) => {
            assert_eq!(
                got,
                Elevation::NotElevated,
                "token runs at medium integrity"
            );
        }
        _ => eprintln!("whoami gave no integrity level; relying on the CI expectation"),
    }
    if on_ci() {
        // GitHub-hosted Windows runners run jobs as an elevated administrator.
        assert_eq!(got, Elevation::Elevated);
    } else {
        assert_ne!(got, Elevation::Unknown);
    }
}

#[test]
fn privileges_report_for_doctor() {
    let p = Privileges::current();
    assert_eq!(p.elevation, is_elevated());
    let json = serde_json::to_value(&p).unwrap();
    assert!(json["elevation"].is_string());
    assert!(json.get("sudo_user").is_some());
    if std::env::var_os("SUDO_USER").is_some_and(|v| !v.is_empty()) {
        assert!(p.sudo_user.is_some());
    }
}
