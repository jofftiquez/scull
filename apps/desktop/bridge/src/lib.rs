//! The desktop app's bridge to the scull core.
//!
//! Every Tauri command in `apps/desktop/src-tauri` is a thin wrapper over a function here, so
//! the logic builds and is tested in the main workspace on every OS (no WebKit needed), and the
//! app crate stays a shell.
//!
//! | Command | Here | Returns |
//! |---|---|---|
//! | `app_info` | [`app_info`] | [`AppInfo`] |
//! | `drives` | [`drives()`] | `Vec<`[`Drive`]`>` |
//! | `scan` | [`Session::scan`] | [`ScanReport`], with [`ProgressEvent`]s |
//! | `cancel_operation` | [`Session::cancel`] | `bool` |
//! | `projects` | [`Session::projects`] | [`ProjectsView`] (last scan) |
//! | `worktrees` | [`Session::worktrees`] | [`WorktreesView`] (last scan) |
//! | `caches` | [`Session::caches`] | [`CachesReport`], with [`ProgressEvent`]s |
//! | `status` | [`Session::status`] | [`DesktopStatus`] |
//! | `capabilities` | [`Session::capabilities`] | [`CapabilitiesReport`] |
//!
//! Every error is a [`ScullError`] (the same JSON as the CLI's `scull/error@1` payload).
//! Payload schemas: [`schemas::all`], committed in `apps/desktop/schemas/`.
//!
//! # Read-only by construction (SAFE-10)
//!
//! Nothing here can delete, move or modify a file. The crate doesn't depend on
//! `scull-cleaner` (the only deleter) and doesn't enable `scull-platform`'s `delete` feature;
//! `tests/safe10.rs` fails the build if either changes. Cleanups reach the Executor through a
//! plan and a human approval, never through the desktop bridge.

pub mod drives;
pub mod schemas;
pub mod session;
pub mod views;

pub use drives::{Drive, DriveKind, ScanPolicy, VolumeFacts, drives, select_drives};
pub use scull_core::ScullError;
pub use scull_engine::{CachesReport, ScanReport, now_unix};
pub use session::{Session, Setup};
pub use views::{
    CapabilitiesReport, DesktopStatus, OpId, ProgressEvent, ProjectsView, ScanParams, WorktreesView,
};

use schemars::JsonSchema;
use serde::Serialize;

/// The scull version this bridge was built from.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which bridge answers, and about what (`app_info` command).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppInfo {
    /// scull version of the core.
    pub version: String,
    /// Always `tauri` from this crate; the frontend's mock answers `mock`.
    pub bridge: BridgeKind,
    /// Operating system the core runs on.
    pub os: Os,
}

/// Which bridge implementation answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BridgeKind {
    /// The Tauri app (this crate).
    Tauri,
    /// The frontend's in-browser mock.
    Mock,
}

/// Operating system family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    /// macOS.
    Macos,
    /// Windows.
    Windows,
    /// Linux.
    Linux,
    /// Anything else.
    Unknown,
}

impl Os {
    /// The OS this binary was built for.
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Os::Macos,
            "windows" => Os::Windows,
            "linux" => Os::Linux,
            _ => Os::Unknown,
        }
    }
}

/// `app_info`: version and OS.
pub fn app_info() -> AppInfo {
    AppInfo {
        version: VERSION.to_owned(),
        bridge: BridgeKind::Tauri,
        os: Os::current(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_info_serializes_like_the_frontend_type() {
        let v = serde_json::to_value(app_info()).unwrap();
        assert_eq!(v["bridge"], "tauri");
        assert_eq!(v["version"], VERSION);
        assert!(["macos", "windows", "linux", "unknown"].contains(&v["os"].as_str().unwrap()));
    }
}
