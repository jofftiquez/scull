//! The scull desktop app's Tauri shell (the MVP command set, #144).
//!
//! Each command is a thin wrapper over `scull_desktop_bridge`, which calls the scull crates.
//! Nothing here deletes, moves or modifies a file itself (SAFE-10 as the PRD states it:
//! deletion only through the core Executor): `prepare_desktop_plan` records a plan from the
//! last scan and `execute_desktop_plan` runs the items the human confirmed through the bridge's
//! `cleanup` module, i.e. the core Executor under the execution lock with full revalidation,
//! D-28 actions and the audit. The window's capabilities grant no filesystem, shell or process
//! plugin. Errors reach the frontend as [`ScullError`] JSON; long operations stream
//! [`ProgressEvent`]s on `scull://progress` and stop on `cancel_operation`.
//!
//! Frontend side: `apps/desktop/src/bridge/tauri.ts` and `tauriPlans.ts` (types generated from
//! the same Rust types, see `apps/desktop/schemas/`).
//!
//! Who can call the commands: only the app's own page in the `main` window. The app manifest
//! (`build.rs`) puts every command under the ACL and `capabilities/default.json` grants them to
//! `main`; a page from any other origin is refused (`tests/commands.rs`). Web view debugging
//! can't be turned on from outside: [`sanitize_browser_env`] runs first in [`run`] and removes
//! every variable that could turn on remote debugging or an inspector server, or inject browser
//! arguments ([`BROWSER_ENV`]), and the windows' browser arguments are set explicitly
//! ([`BROWSER_ARGS`]). Release builds don't enable Tauri's `devtools` feature.

use scull_core::{ErrorKind, ScullError};
use scull_desktop_bridge::{
    CachesReport, CleanupDesk, DesktopPlan, DesktopRun, OpId, ProgressEvent, ScanParams,
    ScanReport, Session, SettingsView, Setup, now_unix,
};
use std::sync::Arc;
use tauri::ipc::Invoke;
use tauri::{AppHandle, Emitter, Runtime, State};

/// The session shared by every command.
type Shared<'a> = State<'a, Arc<Session>>;

/// The cleanup runner shared by the cleanup commands (P3-EXEC-01).
type Cleanup<'a> = State<'a, Arc<CleanupDesk>>;

/// Variables that could turn on a web view's remote debugging or inspector, or inject browser
/// arguments or another browser build. [`sanitize_browser_env`] removes them at start.
pub const BROWSER_ENV: &[&str] = &[
    // WebView2 (Windows).
    "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
    "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
    "WEBVIEW2_USER_DATA_FOLDER",
    "WEBVIEW2_RELEASE_CHANNEL_PREFERENCE",
    "WEBVIEW2_CHANNEL_SEARCH_KIND",
    "WEBVIEW2_PIPE_FOR_SCRIPT_DEBUGGER",
    "WEBVIEW2_WAIT_FOR_SCRIPT_DEBUGGER",
    // WebKitGTK (Linux): remote inspector servers.
    "WEBKIT_INSPECTOR_SERVER",
    "WEBKIT_INSPECTOR_HTTP_SERVER",
    "WEBKIT_INSPECTOR_PIPE",
    // GTK's own interactive inspector.
    "GTK_DEBUG",
];

/// The web views' browser arguments, set explicitly so nothing else adds any (Tauri's own
/// default for WebView2; ignored by the other web views).
pub const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

/// Remove [`BROWSER_ENV`] from the environment; returns the names that were set. Must run
/// before any web view or other thread exists: [`run`] calls it first.
pub fn sanitize_browser_env() -> Vec<String> {
    scull_platform::startup_env::remove_vars_before_threads(BROWSER_ENV)
}

/// Emit progress to the window; a failed emit (window closing) is not an error of the scan.
fn emitter<R: Runtime>(app: AppHandle<R>) -> impl Fn(ProgressEvent) + Send + Sync + 'static {
    move |e| {
        let _ = app.emit(ProgressEvent::NAME, e);
    }
}

/// `scan`: scan and classify (read-only), streaming progress tagged with `op_id`.
#[tauri::command]
async fn scan<R: Runtime>(
    app: AppHandle<R>,
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
async fn caches<R: Runtime>(
    app: AppHandle<R>,
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

/// `settings`: effective configuration with origins and scull's own data (read-only).
#[tauri::command]
async fn settings(session: Shared<'_>) -> Result<SettingsView, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.settings())).await
}

/// `prepare_desktop_plan`: a Cleanup Review plan from the last scan (every eligible item, or
/// `items`). Recorded, never executed.
#[tauri::command]
async fn prepare_desktop_plan(
    session: Shared<'_>,
    cleanup: Cleanup<'_>,
    items: Option<Vec<String>>,
) -> Result<DesktopPlan, ScullError> {
    let (session, cleanup) = (Arc::clone(&session), Arc::clone(&cleanup));
    blocking(move || cleanup.prepare(&session, items.as_deref(), now_unix())).await
}

/// `execute_desktop_plan`: run the items the human chose and confirmed in Cleanup Review,
/// through the core Executor (lock, revalidation, D-28, audit). The main window only.
#[tauri::command]
async fn execute_desktop_plan(
    session: Shared<'_>,
    cleanup: Cleanup<'_>,
    plan_id: String,
    content_hash: String,
    items: Vec<String>,
    typed_count: Option<u64>,
) -> Result<DesktopRun, ScullError> {
    let (session, cleanup) = (Arc::clone(&session), Arc::clone(&cleanup));
    let result = blocking(move || {
        cleanup.execute(
            &session,
            &plan_id,
            &content_hash,
            &items,
            typed_count,
            now_unix(),
        )
    })
    .await;
    if let Err(error) = &result {
        let message: String = error.message.chars().take(2048).collect();
        eprintln!(
            "scull: execute_desktop_plan rejected ({:?}): {}",
            error.kind,
            message.escape_debug()
        );
    }
    result
}

/// Check the live execution lock without recovering or running an operation.
pub fn execution_busy_at(path: &std::path::Path) -> Result<bool, ScullError> {
    scull_desktop_bridge::cleanup::execution_busy_at(path)
}

/// `execution_busy`: an advisory live lock check for the main window's cleanup queue.
#[tauri::command]
async fn execution_busy() -> Result<bool, ScullError> {
    blocking(scull_desktop_bridge::cleanup::execution_busy).await
}

/// Every command of the app, for any runtime (the Tauri command tests mount them on a mock app
/// with the real capabilities); [`run`] registers the same handler.
pub fn handler<R: Runtime>() -> impl Fn(Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        settings,
        scan,
        caches,
        cancel_operation,
        prepare_desktop_plan,
        execute_desktop_plan,
        execution_busy
    ]
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
    // First, before any web view or thread exists (see the crate docs).
    let cleared = sanitize_browser_env();
    if !cleared.is_empty() {
        eprintln!("scull: ignored {cleared:?}: web view debugging can't be turned on from outside");
    }
    let result = tauri::Builder::default()
        .manage(Arc::new(Session::new(Setup::native())))
        .manage(Arc::new(CleanupDesk::new()))
        .invoke_handler(handler())
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("scull: the desktop app failed to start: {e}");
        std::process::exit(1);
    }
}
