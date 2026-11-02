//! Tauri build script: embeds tauri.conf.json, the capabilities and the icons.
//!
//! The app manifest puts every app command under the ACL, so a window can call only what its
//! capability grants (`allow-<command>`): `capabilities/default.json` grants everything to the
//! main window, `capabilities/menubar.json` everything but the approval and cleanup commands to
//! the menu bar widget (P4-MCP-02, P3-EXEC-01). `bridge/tests/commands.rs` checks these lists against `src/lib.rs`.

/// Every app command registered in `src/lib.rs` (`run`).
const COMMANDS: &[&str] = &[
    "app_info",
    "drives",
    "scan",
    "caches",
    "cancel_operation",
    "projects",
    "worktrees",
    "status",
    "capabilities",
    "history",
    "settings",
    "free_space_history",
    "rollup_history",
    "regrowth",
    "footprint_history",
    "forecasts",
    "weekly_digest",
    "monitor_status",
    "configure_monitor",
    "pause_monitor",
    "send_digest_now",
    "open_main_window",
    "hide_menu_bar",
    // Approving an AI agent's plan: the main window only.
    "approval_requests",
    "open_approval_request",
    "approve_agent_plan",
    "deny_agent_plan",
    "revoke_agent_approval",
    "agent_plan_status",
    "take_deep_link",
    // Cleaning through the core Executor (P3-EXEC-01): the main window only.
    "prepare_desktop_plan",
    "execute_desktop_plan",
    "restore_quarantined",
    "quick_clean",
    "onboarding_context",
    "pick_project_folders",
];

fn main() {
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));
    if let Err(e) = tauri_build::try_build(attributes) {
        panic!("tauri build failed: {e:#}");
    }
}
