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

use scull_core::{ErrorKind, ScullError};
use scull_desktop_bridge::{
    AppInfo, CachesReport, CapabilitiesReport, DesktopStatus, Drive, OpId, ProgressEvent,
    ProjectsView, ScanParams, ScanReport, Session, Setup, WorktreesView, now_unix,
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

/// Run a blocking bridge call on Tauri's blocking pool.
async fn blocking<T, F>(f: F) -> Result<T, ScullError>
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
        .manage(Arc::new(Session::new(Setup::native())))
        .invoke_handler(tauri::generate_handler![
            app_info,
            drives,
            scan,
            caches,
            cancel_operation,
            projects,
            worktrees,
            status,
            capabilities
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("scull: the desktop app failed to start: {e}");
        std::process::exit(1);
    }
}
