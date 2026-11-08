//! JSON Schemas of every bridge payload, committed in `apps/desktop/schemas/` and compiled into
//! the frontend's TypeScript types (`pnpm --filter @scull/desktop types`). One source of truth:
//! the Rust types the commands return.
//!
//! `tests/schemas.rs` fails when the committed files are stale. Regenerate them with
//! `SCULL_UPDATE_SCHEMAS=1 cargo test -p scull-desktop-bridge --test schemas`.

use crate::approvals::{
    AgentApprovalRequest, AgentApproved, AgentDenied, AgentPlanStatus, AgentRequests, AgentRevoked,
};
use crate::cleanup::{
    CacheCleanChoice, DesktopCachePreview, DesktopCacheProgress, DesktopPlan, DesktopQuickClean,
    DesktopRestore, DesktopRun,
};
use crate::forecasts::ForecastsView;
use crate::history::HistoryView;
use crate::insights::{FootprintHistory, FreeSpaceHistory, RegrowthHistory, RollupHistory};
use crate::monitor::{DigestView, MonitorConfig, MonitorSnapshot, MonitorStatus};
use crate::settings::SettingsView;
use crate::views::{
    CapabilitiesReport, DesktopStatus, ProgressEvent, ProjectsView, ScanParams, WorktreesView,
};
use crate::{AppInfo, Drive};
use schemars::JsonSchema;
use scull_core::ScullError;
use scull_engine::{CachesReport, ScanReport};

/// `(file stem, schema)` for every payload, in a stable order. Files are
/// `apps/desktop/schemas/<stem>.schema.json`.
pub fn all() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("app-info", schema::<AppInfo>("app-info", "AppInfo")),
        ("drives", schema::<Vec<Drive>>("drives", "Drives")),
        (
            "scan-params",
            schema::<ScanParams>("scan-params", "ScanParams"),
        ),
        (
            "scan-report",
            schema::<ScanReport>("scan-report", "ScanReport"),
        ),
        (
            "progress-event",
            schema::<ProgressEvent>("progress-event", "ProgressEvent"),
        ),
        (
            "projects",
            schema::<ProjectsView>("projects", "ProjectsView"),
        ),
        (
            "worktrees",
            schema::<WorktreesView>("worktrees", "WorktreesView"),
        ),
        (
            "caches-report",
            schema::<CachesReport>("caches-report", "CachesReport"),
        ),
        ("status", schema::<DesktopStatus>("status", "DesktopStatus")),
        (
            "capabilities",
            schema::<CapabilitiesReport>("capabilities", "CapabilitiesReport"),
        ),
        ("history", schema::<HistoryView>("history", "HistoryView")),
        (
            "settings",
            schema::<SettingsView>("settings", "SettingsView"),
        ),
        (
            "free-space-history",
            schema::<FreeSpaceHistory>("free-space-history", "FreeSpaceHistory"),
        ),
        (
            "rollup-history",
            schema::<RollupHistory>("rollup-history", "RollupHistory"),
        ),
        (
            "regrowth",
            schema::<RegrowthHistory>("regrowth", "RegrowthHistory"),
        ),
        (
            "footprint-history",
            schema::<FootprintHistory>("footprint-history", "FootprintHistory"),
        ),
        (
            "forecasts",
            schema::<ForecastsView>("forecasts", "ForecastsView"),
        ),
        (
            "weekly-digest",
            schema::<DigestView>("weekly-digest", "DigestView"),
        ),
        (
            "monitor-status",
            schema::<MonitorStatus>("monitor-status", "MonitorStatus"),
        ),
        (
            "monitor-config",
            schema::<MonitorConfig>("monitor-config", "MonitorConfig"),
        ),
        (
            "monitor-snapshot",
            schema::<MonitorSnapshot>("monitor-snapshot", "MonitorSnapshot"),
        ),
        (
            "agent-requests",
            schema::<AgentRequests>("agent-requests", "AgentRequests"),
        ),
        (
            "agent-approval-request",
            schema::<AgentApprovalRequest>("agent-approval-request", "AgentApprovalRequest"),
        ),
        (
            "agent-approved",
            schema::<AgentApproved>("agent-approved", "AgentApproved"),
        ),
        (
            "agent-denied",
            schema::<AgentDenied>("agent-denied", "AgentDenied"),
        ),
        (
            "agent-revoked",
            schema::<AgentRevoked>("agent-revoked", "AgentRevoked"),
        ),
        (
            "agent-plan-status",
            schema::<AgentPlanStatus>("agent-plan-status", "AgentPlanStatus"),
        ),
        (
            "desktop-plan",
            schema::<DesktopPlan>("desktop-plan", "DesktopPlan"),
        ),
        (
            "desktop-run",
            schema::<DesktopRun>("desktop-run", "DesktopRun"),
        ),
        (
            "desktop-restore",
            schema::<DesktopRestore>("desktop-restore", "DesktopRestore"),
        ),
        (
            "desktop-cache-preview",
            schema::<DesktopCachePreview>("desktop-cache-preview", "DesktopCachePreview"),
        ),
        (
            "cache-clean-choice",
            schema::<CacheCleanChoice>("cache-clean-choice", "CacheCleanChoice"),
        ),
        (
            "desktop-quick-clean",
            schema::<DesktopQuickClean>("desktop-quick-clean", "DesktopQuickClean"),
        ),
        (
            "desktop-cache-progress",
            schema::<DesktopCacheProgress>("desktop-cache-progress", "DesktopCacheProgress"),
        ),
        ("error", schema::<ScullError>("error", "ScullError")),
    ]
}

/// The schema of `T`, generated exactly like the CLI's (`schemars::schema_for!`), so shared
/// definitions (`Candidate`, `ProjectSummary`, ...) are byte-for-byte the same.
fn schema<T: JsonSchema>(stem: &str, title: &str) -> serde_json::Value {
    let mut v = serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("title".into(), serde_json::Value::String(title.to_owned()));
        o.insert(
            "$id".into(),
            serde_json::Value::String(format!("scull-desktop/{stem}.schema.json")),
        );
    }
    v
}
