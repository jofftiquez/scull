//! The scull desktop app's Tauri shell.
//!
//! Each command is a thin wrapper over `scull_desktop_bridge`, which calls the read-only scull
//! crates. Nothing here (or in the bridge) can delete, move or modify a scanned file
//! (SAFE-10): the crate graph has no `scull-cleaner`, and the window's capabilities grant no
//! filesystem plugin. The only writes are to scull's own database: the human's decisions on AI
//! agent plans, below. Errors reach the frontend as [`ScullError`] JSON; long operations stream
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
//! The approval commands ([`approval_handler`]) wrap [`ApprovalDesk`]: list the agent plans
//! waiting for the human, open one (the broker's challenge and its confirmation code stay in
//! this process; the webview gets the code only to display it), approve it with the code the
//! human typed, deny it, revoke an approval, and report what the agent did with it. They are
//! invoked by the webview only, from the human's clicks in Cleanup Review: an AI agent talks to
//! `scull mcp serve`, a different process with no way into this app's IPC, and nothing here is
//! callable from a remote origin (the window loads only the bundled app, and the capability
//! grants no remote URL). Approving records the human's decision; it never executes anything:
//! the agent executes an approved plan through `scull mcp serve` and the Executor (SAFE-10).
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
pub mod tray;

use scull_core::{ErrorKind, PlanId, ScullError};
use scull_desktop_bridge::{
    AgentApprovalRequest, AgentApproved, AgentDenied, AgentPlanStatus, AgentRequests, AgentRevoked,
    AppInfo, ApprovalDesk, CachesReport, CapabilitiesReport, DesktopStatus, DigestView, Drive,
    FootprintHistory, ForecastsView, FreeSpaceHistory, HistoryView, OpId, ProgressEvent,
    ProjectsView,
    RegrowthHistory, RollupHistory, ScanParams, ScanReport, Session, SettingsView, Setup,
    WorktreesView, now_unix,
};
use std::sync::{Arc, Mutex};
use tauri::ipc::Invoke;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

/// The session shared by every command.
type Shared<'a> = State<'a, Arc<Session>>;

/// The approval desk shared by the approval commands.
type Desk<'a> = State<'a, Arc<ApprovalDesk>>;

/// The plan a `scull://approve/<plan-id>` link asked for and the window hasn't taken yet.
#[derive(Default)]
pub struct DeepLinks(Mutex<Option<String>>);

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

/// `take_deep_link`: the plan a `scull://approve/<plan-id>` link opened the app with, once.
#[tauri::command]
fn take_deep_link(links: State<'_, DeepLinks>) -> Option<String> {
    links.0.lock().unwrap_or_else(|e| e.into_inner()).take()
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
fn open_links<R: Runtime>(app: &AppHandle<R>, urls: impl IntoIterator<Item = String>) {
    for plan in urls.into_iter().filter_map(|u| approve_link_plan(&u)) {
        if let Some(links) = app.try_state::<DeepLinks>() {
            *links.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(plan.clone());
        }
        let _ = app.emit(OPEN_REQUEST_EVENT, plan);
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// The approval commands alone, for any runtime (the Tauri command tests mount them on a mock
/// app). [`run`] registers the same commands.
pub fn approval_handler<R: Runtime>() -> impl Fn(Invoke<R>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        approval_requests,
        open_approval_request,
        approve_agent_plan,
        deny_agent_plan,
        revoke_agent_approval,
        agent_plan_status,
        take_deep_link
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
            take_deep_link
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("scull: the desktop app failed to start: {e}");
        std::process::exit(1);
    }
}
