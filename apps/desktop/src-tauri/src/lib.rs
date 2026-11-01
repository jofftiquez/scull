//! The scull desktop app's Tauri shell.
//!
//! Each command is a thin wrapper over `scull_desktop_bridge`, which calls the read-only scull
//! crates. Nothing here (or in the bridge) can delete, move or modify a file (SAFE-10): the
//! crate graph has no `scull-cleaner`, and the window's capabilities grant no filesystem
//! plugin. Errors reach the frontend as [`ScullError`] JSON; long operations stream
//! [`ProgressEvent`]s on `scull://progress` and stop on `cancel_operation`.
//!
//! Frontend side: `apps/desktop/src/bridge/tauri.ts` (types generated from the same Rust
//! types, see `apps/desktop/schemas/`).
//!
//! The menu bar / tray widget lives in [`tray`]: a tray icon, a small widget window and two
//! window commands (`open_main_window`, `hide_menu_bar`); it never runs a cleanup.
//!
//! Tray monitoring and the weekly digest live in [`monitor`]: a background thread that reads
//! free space, history and recorded alerts and shows OS notifications through Tauri's
//! notification plugin (the plugin is used from Rust only; the windows get no notification
//! permission). It never runs a cleanup either.

pub mod monitor;
pub mod tray;

use scull_core::{ErrorKind, ScullError};
use scull_desktop_bridge::{
    AppInfo, CachesReport, CapabilitiesReport, DesktopStatus, DigestView, Drive, FootprintHistory,
    FreeSpaceHistory, HistoryView, OpId, ProgressEvent, ProjectsView, RegrowthHistory,
    RollupHistory, ScanParams, ScanReport, Session, SettingsView, Setup, WorktreesView, now_unix,
};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// The session shared by every command.
type Shared<'a> = State<'a, Arc<Session>>;

/// `app_info`: scull version and OS.
#[tauri::command]
fn app_info() -> AppInfo {
    scull_desktop_bridge::app_info()
}

/// `drives`: mounted volumes for the sidebar's drives widget.
#[tauri::command]
async fn drives() -> Result<Vec<Drive>, ScullError> {
    blocking(scull_desktop_bridge::drives).await
}

/// Emit progress to the window; a failed emit (window closing) is not an error of the scan.
fn emitter(app: AppHandle) -> impl Fn(ProgressEvent) + Send + Sync + 'static {
    move |e| {
        let _ = app.emit(ProgressEvent::NAME, e);
    }
}

/// `scan`: scan and classify (read-only), streaming progress tagged with `op_id`.
#[tauri::command]
async fn scan(
    app: AppHandle,
    session: Shared<'_>,
    op_id: String,
    params: ScanParams,
) -> Result<ScanReport, ScullError> {
    let op = OpId::parse(&op_id)?;
    let session = Arc::clone(&session);
    blocking(move || session.scan(&op, &params, emitter(app))).await
}

/// `caches`: locate and classify package-manager caches, streaming progress.
#[tauri::command]
async fn caches(
    app: AppHandle,
    session: Shared<'_>,
    op_id: String,
) -> Result<CachesReport, ScullError> {
    let op = OpId::parse(&op_id)?;
    let session = Arc::clone(&session);
    blocking(move || session.caches(&op, emitter(app))).await
}

/// `cancel_operation`: stop a running scan or caches lookup. `true` when it was running.
#[tauri::command]
fn cancel_operation(session: Shared<'_>, op_id: String) -> Result<bool, ScullError> {
    Ok(session.cancel(&OpId::parse(&op_id)?))
}

/// `projects`: projects of the last complete scan.
#[tauri::command]
fn projects(session: Shared<'_>) -> ProjectsView {
    session.projects()
}

/// `worktrees`: Git worktrees of the last complete scan.
#[tauri::command]
fn worktrees(session: Shared<'_>) -> WorktreesView {
    session.worktrees()
}

/// `status`: last recorded scan and free-space samples.
#[tauri::command]
async fn status(session: Shared<'_>) -> Result<DesktopStatus, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.status(now_unix()))).await
}

/// `capabilities`: what this machine and build can observe.
#[tauri::command]
async fn capabilities(session: Shared<'_>) -> Result<CapabilitiesReport, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.capabilities())).await
}

/// `history`: recorded cleanup operations with their audit logs (read-only).
#[tauri::command]
async fn history(session: Shared<'_>) -> Result<HistoryView, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.history(now_unix()))).await
}

/// `settings`: effective configuration with origins and scull's own data (read-only).
#[tauri::command]
async fn settings(session: Shared<'_>) -> Result<SettingsView, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.settings())).await
}

/// `free_space_history`: one volume's free-space samples since `since` (read-only).
#[tauri::command]
async fn free_space_history(
    session: Shared<'_>,
    since: i64,
    volume_id: Option<String>,
) -> Result<FreeSpaceHistory, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.free_space_history(since, volume_id.as_deref()))).await
}

/// `rollup_history`: bytes per category per usable scan since `since` (read-only).
#[tauri::command]
async fn rollup_history(session: Shared<'_>, since: i64) -> Result<RollupHistory, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.rollup_history(since))).await
}

/// `regrowth`: artifacts that came back since `since` (read-only).
#[tauri::command]
async fn regrowth(session: Shared<'_>, since: i64) -> Result<RegrowthHistory, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.regrowth(since))).await
}

/// `footprint_history`: scull's own footprint since `since` (read-only).
#[tauri::command]
async fn footprint_history(
    session: Shared<'_>,
    since: i64,
) -> Result<FootprintHistory, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.footprint_history(since))).await
}

/// `weekly_digest`: the last 7 days in a few lines (Insights "This week"; read-only).
#[tauri::command]
async fn weekly_digest(session: Shared<'_>) -> Result<DigestView, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.weekly_digest(now_unix()))).await
}

/// Run a blocking bridge call on Tauri's blocking pool.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, ScullError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ScullError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| ScullError::new(ErrorKind::Internal, format!("bridge task failed: {e}")))?
}

/// Start the app.
pub fn run() {
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(Arc::new(Session::new(Setup::native())))
        .setup(|app| {
            monitor::start(app.handle())?;
            tray::install(app.handle())?;
            Ok(())
        })
        .on_window_event(tray::on_window_event)
        .invoke_handler(tauri::generate_handler![
            app_info,
            drives,
            scan,
            caches,
            cancel_operation,
            projects,
            worktrees,
            status,
            capabilities,
            history,
            settings,
            free_space_history,
            rollup_history,
            regrowth,
            footprint_history,
            weekly_digest,
            monitor::monitor_status,
            monitor::configure_monitor,
            monitor::pause_monitor,
            monitor::send_digest_now,
            tray::open_main_window,
            tray::hide_menu_bar
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("scull: the desktop app failed to start: {e}");
        std::process::exit(1);
    }
}
