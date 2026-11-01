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
//! | `history` | [`Session::history`] | [`HistoryView`] (read-only) |
//! | `settings` | [`Session::settings`] | [`SettingsView`] (read-only) |
//! | `free_space_history` | [`Session::free_space_history`] | [`FreeSpaceHistory`] |
//! | `rollup_history` | [`Session::rollup_history`] | [`RollupHistory`] |
//! | `regrowth` | [`Session::regrowth`] | [`RegrowthHistory`] |
//! | `footprint_history` | [`Session::footprint_history`] | [`FootprintHistory`] |
//! | `forecasts` | [`Session::forecasts`] | [`ForecastsView`] (one forecast source, PRO-01) |
//! | `weekly_digest` | [`Session::weekly_digest`] | [`DigestView`] |
//! | `monitor_status` | [`Monitor::status`] | [`MonitorStatus`] |
//! | `configure_monitor` | [`Monitor::configure`] | [`MonitorStatus`] |
//! | `pause_monitor` | [`Monitor::pause`] | [`MonitorStatus`] |
//! | `send_digest_now` | [`Monitor::digest_now`] | [`DigestView`] |
//!
//! The menu bar widget's window logic (where it opens, which main-window routes it may show)
//! is in [`menubar`]; its data is `status` (free space, review plan, alerts), `drives`,
//! `caches`, `worktrees` and `free_space_history`.
//!
//! Tray monitoring and the weekly digest are in [`monitor`]: a [`Monitor`] the tray app ticks
//! from a background thread, which reads free space, history and recorded alerts and decides
//! the notifications (never a cleanup). Its settings and memory are app state, sent by the
//! main window (`configure_monitor`); nothing here writes a file.
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

// Forbidden at the root, so no local allow can lift it (SAFE-10; tests/safe10.rs).
#![forbid(unsafe_code)]

pub mod approvals;
pub mod cleanup;
pub mod db;
pub mod drives;
pub mod forecasts;
pub mod history;
pub mod insights;
pub mod menubar;
pub mod monitor;
pub mod schemas;
pub mod session;
pub mod settings;
pub mod views;

pub use approvals::{
    AgentApprovalRequest, AgentApproved, AgentDenied, AgentPlanStatus, AgentRequests, AgentRevoked,
    ApprovalDesk,
};
pub use cleanup::{CleanupDesk, DesktopPlan, DesktopQuickClean, DesktopRestore, DesktopRun};
pub use drives::{Drive, DriveKind, ScanPolicy, VolumeFacts, drives, select_drives};
pub use forecasts::ForecastsView;
pub use history::HistoryView;
pub use insights::{FootprintHistory, FreeSpaceHistory, RegrowthHistory, RollupHistory};
pub use monitor::{DigestView, Monitor, MonitorConfig, MonitorSnapshot, MonitorStatus};
pub use scull_core::ScullError;
pub use scull_engine::{CachesReport, ScanReport, now_unix};
pub use session::{Session, Setup};
pub use settings::SettingsView;
pub use views::{
    CapabilitiesReport, DesktopStatus, OpId, ProgressEvent, ProjectsView, ReviewPlanView,
    ScanParams, WorktreesView,
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
