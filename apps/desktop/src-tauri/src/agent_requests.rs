//! Agent request notifications (P4-MCP-02, design 12b; PRD §13.3 step 2a): while the app runs,
//! an AI agent's new cleanup plan shows an OS notification posted by scull itself.
//!
//! One background thread looks every [`POLL`] at the plans waiting for the human
//! ([`ApprovalDesk::awaiting`], read-only) and announces each new one once
//! ([`request_notices`]). The notification has no Approve or Deny: approving needs the full plan
//! on screen in Cleanup Review, every item reviewed and the typed code (the main window's
//! in-app notice opens it). It is posted from Rust only; no window gets the notification
//! permission. Reads only: it never approves, records or runs anything.

use scull_desktop_bridge::approvals::request_notices;
use scull_desktop_bridge::{ApprovalDesk, now_unix};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

/// How often the thread looks for new agent requests (a request lives 15 minutes).
pub const POLL: Duration = Duration::from_secs(5);

/// Start the thread (called from the app's `setup`).
pub fn start(app: &AppHandle) -> std::io::Result<()> {
    let app = app.clone();
    std::thread::Builder::new()
        .name("scull-agent-requests".into())
        .spawn(move || run(&app))?;
    Ok(())
}

fn run(app: &AppHandle) {
    let mut announced = HashSet::new();
    loop {
        std::thread::sleep(POLL);
        let Some(desk) = app.try_state::<Arc<ApprovalDesk>>() else {
            continue;
        };
        let now = now_unix();
        let Ok(pending) = desk.awaiting(now) else {
            // The database is busy or unreadable: look again next time.
            continue;
        };
        for n in request_notices(&pending, &mut announced, now) {
            if let Err(e) = app
                .notification()
                .builder()
                .title(&n.title)
                .body(&n.body)
                .show()
            {
                eprintln!("scull: notification not shown: {e}");
            }
        }
    }
}
