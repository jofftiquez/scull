//! The scull desktop app's Tauri shell.
//!
//! Each command is a thin wrapper over `scull_desktop_bridge`, which calls the scull crates.
//! Nothing here deletes, moves or modifies a file itself (SAFE-10 as the PRD states it:
//! deletion only through the core Executor): the cleanup commands (`prepare_desktop_plan`,
//! `execute_desktop_plan`, `restore_quarantined`, `quick_clean`; P3-EXEC-01) call the bridge's
//! `cleanup` module, which runs reviewed plans through the core Executor under the
//! execution lock with full revalidation, D-28 actions and the audit, after the human's
//! confirmation in the main window. They and the approval commands are granted to the main
//! window only (`capabilities/`), never to the menu bar widget, and none is reachable from MCP.
//! The window's capabilities grant no filesystem plugin. Errors reach the frontend as [`ScullError`] JSON; long operations stream
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
//!
//! # Approving an AI agent's plan (P4-MCP-02, PRD §13.3)
//!
//! The approval commands ([`main_window_handler`]) wrap [`ApprovalDesk`]: list the agent plans
//! waiting for the human, open one (the broker's challenge and its confirmation code stay in
//! this process; the webview gets the code only to display it), approve it with the code the
//! human typed, deny it, revoke an approval, and report what the agent did with it. Approving
//! records the human's decision; it never executes anything: the agent executes an approved
//! plan through `scull mcp serve` and the Executor (SAFE-10).
//!
//! Who can call them:
//!
//! * Only the main window's own page. The app manifest (`build.rs`) puts every command under
//!   the ACL; `capabilities/default.json` grants them to the `main` window and
//!   `capabilities/menubar.json` grants the menu bar widget everything except the approval
//!   commands. A page from any other origin is refused (`tests/approval_commands.rs`).
//! * Not through the web view's debugging: [`sanitize_browser_env`] runs first in [`run`] and
//!   removes every variable that could turn on remote debugging or an inspector server, or
//!   inject browser arguments ([`BROWSER_ENV`]: WebView2's `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`
//!   and friends, WebKitGTK's `WEBKIT_INSPECTOR_SERVER`, ...), before any web view or thread
//!   exists; the windows' browser arguments are set explicitly ([`BROWSER_ARGS`]). Release
//!   builds don't enable Tauri's `devtools` feature, so no inspector is reachable on macOS
//!   either.
//! * Not by the agent through MCP: `scull mcp serve` is a different process with no way into
//!   this app's IPC.
//!
//! What remains is the same-user desktop class (PRD §13.4, T-41): a process running as the
//! user can drive the GUI itself (synthetic input and accessibility APIs, reading the screen to
//! learn the code), inject code into this process (a debugger, `LD_PRELOAD`, DLL injection) or
//! set the per-user WebView2 policy in the registry. Like an agent with unrestricted shell
//! access, it can delete files without scull anyway; scull guarantees only that its own tools
//! never act without a human's approval in this UI or an interactive terminal.
//!
//! `scull://approve/<plan-id>` deep links (the agent tells the user to open one, or an MCP
//! client shows it) only bring the app forward on Cleanup Review for that plan. A link can't
//! approve anything: the human still reviews every item and types the code shown there. A
//! second launch (how Windows and Linux deliver a link) hands its URL to the running app.
//!
//! [`agent_requests`] posts an OS notification for each new agent request (design 12b), from
//! Rust only; it approves nothing.

pub mod agent_requests;
pub mod monitor;
pub mod onboarding;
mod settings_write;
pub mod tray;

use scull_core::{ErrorKind, PlanId, ScullError};
use scull_desktop_bridge::{
    AgentApprovalRequest, AgentApproved, AgentDenied, AgentPlanStatus, AgentRequests, AgentRevoked,
    AppInfo, ApprovalDesk, CacheCleanChoice, CachesReport, CapabilitiesReport, CleanupDesk,
    DesktopCachePreview, DesktopCacheProgress, DesktopPlan, DesktopQuickClean, DesktopRestore,
    DesktopRun, DesktopStatus, DigestView, Drive, FootprintHistory, ForecastsView,
    FreeSpaceHistory, HistoryView, OpId, ProgressEvent, ProjectsView, RegrowthHistory,
    RollupHistory, ScanParams, ScanReport, Session, SettingsView, Setup, WorktreesView, now_unix,
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tauri::ipc::Invoke;
use tauri::{AppHandle, Emitter, EventTarget, Manager, Runtime, State, WebviewWindow};

/// The session shared by every command.
type Shared<'a> = State<'a, Arc<Session>>;

/// The approval desk shared by the approval commands.
type Desk<'a> = State<'a, Arc<ApprovalDesk>>;

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

/// `scull://approve/<plan-id>` links: the plan the window hasn't taken yet, and the plans that
/// already brought the window forward (each does once, so repeated links can't keep stealing
/// focus).
#[derive(Default)]
pub struct DeepLinks {
    pending: Mutex<Option<String>>,
    focused: Mutex<HashSet<String>>,
}

impl DeepLinks {
    /// Whether a link to `plan` should bring the window forward: only the first one.
    pub fn should_focus(&self, plan: &str) -> bool {
        self.focused
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(plan.to_owned())
    }
}

/// The event telling the window to open an agent request (payload: the plan ID).
pub const OPEN_REQUEST_EVENT: &str = "scull://open-request";

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

/// Cache output belongs only to the webview that requested the approved cleanup.
fn cache_emitter<R: Runtime>(
    window: WebviewWindow<R>,
) -> impl Fn(DesktopCacheProgress) + Send + Sync + 'static {
    move |event| {
        let _ = window.emit_to(
            EventTarget::webview_window(window.label()),
            DesktopCacheProgress::NAME,
            event,
        );
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

/// `forecasts`: every sampled volume's disk-full forecast (PRO-01; the one source of days left
/// for Insights, the Overview alert and the menu bar widget; read-only).
#[tauri::command]
async fn forecasts(session: Shared<'_>) -> Result<ForecastsView, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.forecasts(now_unix()))).await
}

/// `weekly_digest`: the last 7 days in a few lines (Insights "This week"; read-only).
#[tauri::command]
async fn weekly_digest(session: Shared<'_>) -> Result<DigestView, ScullError> {
    let session = Arc::clone(&session);
    blocking(move || Ok(session.weekly_digest(now_unix()))).await
}

/// `approval_requests`: AI agent plans waiting for the human, newest first.
#[tauri::command]
async fn approval_requests(desk: Desk<'_>) -> Result<AgentRequests, ScullError> {
    let desk = Arc::clone(&desk);
    blocking(move || desk.awaiting(now_unix())).await
}

/// `open_approval_request`: open agent plan `plan_id` for review. Starts a fresh challenge
/// (new confirmation code, kept in this process); the code in the answer is for display only.
#[tauri::command]
async fn open_approval_request(
    session: Shared<'_>,
    desk: Desk<'_>,
    plan_id: String,
) -> Result<AgentApprovalRequest, ScullError> {
    let (session, desk) = (Arc::clone(&session), Arc::clone(&desk));
    blocking(move || desk.open(&plan_id, session.last_scan().as_deref(), now_unix())).await
}

/// `approve_agent_plan`: the human approves the opened plan with the code they typed. The
/// backend checks the code and the reviewed hash; the approval is single use and lapses with
/// the plan.
#[tauri::command]
async fn approve_agent_plan(
    desk: Desk<'_>,
    plan_id: String,
    content_hash: String,
    code: String,
) -> Result<AgentApproved, ScullError> {
    let desk = Arc::clone(&desk);
    blocking(move || desk.approve(&plan_id, &content_hash, &code, now_unix())).await
}

/// `deny_agent_plan`: the human denies the plan (audited; it is not offered again).
#[tauri::command]
async fn deny_agent_plan(desk: Desk<'_>, plan_id: String) -> Result<AgentDenied, ScullError> {
    let desk = Arc::clone(&desk);
    blocking(move || desk.deny(&plan_id, now_unix())).await
}

/// `revoke_agent_approval`: the human takes an unused approval back.
#[tauri::command]
async fn revoke_agent_approval(
    desk: Desk<'_>,
    plan_id: String,
) -> Result<AgentRevoked, ScullError> {
    let desk = Arc::clone(&desk);
    blocking(move || desk.revoke(&plan_id, now_unix())).await
}

/// `agent_plan_status`: where an agent plan stands, with the agent's execution once started.
#[tauri::command]
async fn agent_plan_status(desk: Desk<'_>, plan_id: String) -> Result<AgentPlanStatus, ScullError> {
    let desk = Arc::clone(&desk);
    blocking(move || desk.status(&plan_id, now_unix())).await
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

/// `restore_quarantined`: put a quarantined item back (never overwriting). The main window only.
#[tauri::command]
async fn restore_quarantined(
    session: Shared<'_>,
    cleanup: Cleanup<'_>,
    operation_id: String,
    item_id: String,
) -> Result<DesktopRestore, ScullError> {
    let (session, cleanup) = (Arc::clone(&session), Arc::clone(&cleanup));
    blocking(move || cleanup.restore(&session, &operation_id, &item_id, now_unix())).await
}

/// `quick_clean`: run the trusted tools of the ticked cache chips. The main window only.
#[tauri::command]
async fn quick_clean<R: Runtime>(
    window: WebviewWindow<R>,
    session: Shared<'_>,
    cleanup: Cleanup<'_>,
    items: Vec<String>,
    op_id: Option<String>,
) -> Result<DesktopQuickClean, ScullError> {
    let op = op_id.as_deref().map(OpId::parse).transpose()?;
    let (session, cleanup) = (Arc::clone(&session), Arc::clone(&cleanup));
    blocking(move || match op {
        Some(op) => cleanup.quick_clean_with_progress(
            &session,
            &items,
            &op,
            now_unix(),
            &cache_emitter(window),
        ),
        None => cleanup.quick_clean(&session, &items, now_unix()),
    })
    .await
}

/// `quick_clean_plan`: exact native commands and skip reasons for review, main window only.
#[tauri::command]
async fn quick_clean_plan(
    session: Shared<'_>,
    cleanup: Cleanup<'_>,
) -> Result<DesktopCachePreview, ScullError> {
    let (session, cleanup) = (Arc::clone(&session), Arc::clone(&cleanup));
    blocking(move || cleanup.quick_clean_plan(&session)).await
}

/// `quick_clean_run`: the selected IDs, modes and commands confirmed in the Caches screen.
#[tauri::command]
async fn quick_clean_run<R: Runtime>(
    window: WebviewWindow<R>,
    session: Shared<'_>,
    cleanup: Cleanup<'_>,
    choices: Vec<CacheCleanChoice>,
    op_id: Option<String>,
) -> Result<DesktopQuickClean, ScullError> {
    let op = op_id.as_deref().map(OpId::parse).transpose()?;
    let (session, cleanup) = (Arc::clone(&session), Arc::clone(&cleanup));
    blocking(move || match op {
        Some(op) => cleanup.quick_clean_run_with_progress(
            &session,
            &choices,
            &op,
            now_unix(),
            &cache_emitter(window),
        ),
        None => cleanup.quick_clean_run(&session, &choices, now_unix()),
    })
    .await
}

/// `take_deep_link`: the plan a `scull://approve/<plan-id>` link opened the app with, once.
#[tauri::command]
fn take_deep_link(links: State<'_, DeepLinks>) -> Option<String> {
    links
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// The plan ID of a `scull://approve/<plan-id>` link; anything else is ignored.
pub fn approve_link_plan(url: &str) -> Option<String> {
    let rest = url.strip_prefix("scull://approve/")?;
    let id = rest.trim_end_matches('/');
    PlanId::try_from(id.to_owned())
        .ok()
        .map(|p| p.as_str().to_owned())
}

/// Bring the window forward on Cleanup Review for the plan of each approve link in `urls`.
/// Each plan brings the window forward once; later links to it only tell the window (which
/// doesn't leave a request the human has open, see `src/App.vue`).
fn open_links<R: Runtime>(app: &AppHandle<R>, urls: impl IntoIterator<Item = String>) {
    let mut focus = false;
    for plan in urls.into_iter().filter_map(|u| approve_link_plan(&u)) {
        if let Some(links) = app.try_state::<DeepLinks>() {
            *links.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(plan.clone());
            focus |= links.should_focus(&plan);
        }
        let _ = app.emit(OPEN_REQUEST_EVENT, plan);
    }
    if focus && let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// The main window's own commands, granted to no other window: approving an AI agent's plan
/// and running cleanups through the core Executor. For any runtime (the Tauri command tests
/// mount them on a mock app with the real capabilities); [`run`] registers the same commands.
pub fn main_window_handler<R: Runtime>() -> impl Fn(Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        approval_requests,
        open_approval_request,
        approve_agent_plan,
        deny_agent_plan,
        revoke_agent_approval,
        agent_plan_status,
        take_deep_link,
        prepare_desktop_plan,
        execute_desktop_plan,
        execution_busy,
        restore_quarantined,
        quick_clean,
        quick_clean_plan,
        quick_clean_run,
        onboarding::onboarding_context,
        settings_write::save_setting,
        onboarding::pick_project_folders
    ]
}

/// The state the approval commands need: the session (its last scan), the desk and the
/// pending deep link.
pub fn approval_state(setup: Setup) -> (Arc<Session>, Arc<ApprovalDesk>, DeepLinks) {
    let desk = Arc::new(ApprovalDesk::new(setup.data_dir.clone()));
    (Arc::new(Session::new(setup)), desk, DeepLinks::default())
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
    let (session, desk, links) = approval_state(Setup::native());
    let result = tauri::Builder::default()
        // First, so a second launch (a deep link on Windows or Linux) hands its URL to this
        // instance instead of opening another window.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            open_links(app, argv);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_deep_link::init())
        .manage(session)
        .manage(desk)
        .manage(Arc::new(CleanupDesk::new()))
        .manage(links)
        .setup(|app| {
            monitor::start(app.handle())?;
            tray::install(app.handle())?;
            agent_requests::start(app.handle())?;
            use tauri_plugin_deep_link::DeepLinkExt;
            let handle = app.handle().clone();
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                open_links(&handle, urls.into_iter().map(|u| u.to_string()));
            }
            app.deep_link().on_open_url(move |event| {
                open_links(&handle, event.urls().into_iter().map(|u| u.to_string()));
            });
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
            forecasts,
            weekly_digest,
            monitor::monitor_status,
            monitor::configure_monitor,
            monitor::pause_monitor,
            monitor::send_digest_now,
            tray::open_main_window,
            tray::hide_menu_bar,
            approval_requests,
            open_approval_request,
            approve_agent_plan,
            deny_agent_plan,
            revoke_agent_approval,
            agent_plan_status,
            take_deep_link,
            prepare_desktop_plan,
            execute_desktop_plan,
            execution_busy,
            restore_quarantined,
            quick_clean,
            quick_clean_plan,
            quick_clean_run,
            onboarding::onboarding_context,
            settings_write::save_setting,
            onboarding::pick_project_folders
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("scull: the desktop app failed to start: {e}");
        std::process::exit(1);
    }
}
