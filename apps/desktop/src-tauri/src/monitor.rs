//! Tray monitoring and the weekly digest (P5-PRO-01; PRD §14.6, INS-04).
//!
//! One background thread wakes every [`POLL`] (or when a command wakes it), asks the bridge's
//! [`Monitor`] whether a check or the digest is due, and shows what it decides as OS
//! notifications through Tauri's notification plugin. Everything it decides is in
//! `scull_desktop_bridge::monitor` (tested there with a fake clock and a temp database).
//!
//! Reads and notifies only (SAFE-10): a check reads free space, the history and the alerts
//! `scull check` recorded. It never scans, writes the history, or runs a cleanup; a low-space
//! notification leads to Cleanup Review, where every cleanup is still reviewed. Idle cost: one
//! sleeping thread (resource budget: < 1% CPU, < 50 MB RSS). It lives and dies with the app:
//! no daemon is installed (PRO-07 schedules `scull check` separately).
//!
//! The main window keeps the monitor's settings and memory in its local storage: it sends them
//! with `configure_monitor` at start, and receives [`MONITOR_EVENT`] snapshots to keep.

use crate::tray::MAIN_LABEL;
use scull_core::ScullError;
use scull_desktop_bridge::monitor::{
    DigestView, MONITOR_EVENT, Monitor, MonitorConfig, MonitorStatus, Notification,
    PAUSE_HOUR_SECS, VolumeSample, live_samples, tooltip,
};
use scull_desktop_bridge::{Session, now_unix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use tauri::menu::MenuItem;
use tauri::{AppHandle, Emitter, Manager, State, Wry};
use tauri_plugin_notification::NotificationExt;

/// The longest the thread sleeps between two looks at the clock.
pub const POLL: Duration = Duration::from_secs(60);

/// The monitor shared by the thread, the commands and the tray menu.
pub struct MonitorHandle {
    monitor: Mutex<Monitor>,
    wake: Condvar,
    force: AtomicBool,
    pause_item: Mutex<Option<MenuItem<Wry>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

type Handle<'a> = State<'a, Arc<MonitorHandle>>;
type SharedSession<'a> = State<'a, Arc<Session>>;

/// Start the monitor thread (called from the app's `setup`, after the tray).
pub fn start(app: &AppHandle) -> std::io::Result<()> {
    let handle = Arc::new(MonitorHandle {
        monitor: Mutex::new(Monitor::new(now_unix())),
        wake: Condvar::new(),
        force: AtomicBool::new(false),
        pause_item: Mutex::new(None),
    });
    app.manage(Arc::clone(&handle));
    let app = app.clone();
    std::thread::Builder::new()
        .name("scull-monitor".into())
        .spawn(move || run(&app, &handle))?;
    Ok(())
}

/// Let the tray menu's "Pause alerts" item follow the pause.
pub fn set_pause_item(app: &AppHandle, item: MenuItem<Wry>) {
    if let Some(h) = app.try_state::<Arc<MonitorHandle>>() {
        *lock(&h.pause_item) = Some(item);
        refresh_menu(&h, now_unix());
    }
}

fn run(app: &AppHandle, h: &MonitorHandle) {
    loop {
        {
            let guard = lock(&h.monitor);
            if !h.force.load(Ordering::Relaxed) {
                let _ = h.wake.wait_timeout(guard, POLL);
            }
        }
        let force = h.force.swap(false, Ordering::Relaxed);
        let now = now_unix();
        if !force && !lock(&h.monitor).due(now) {
            continue;
        }
        let session = app.state::<Arc<Session>>();
        let live = live_samples(
            &session.known_volumes(),
            session.setup().home.as_deref(),
            now,
        );
        let outcome = lock(&h.monitor).tick(&session, now, &live, force);
        for n in &outcome.notifications {
            notify(app, n);
        }
        if outcome.checked {
            set_tooltip(app, &session, &live);
        }
        emit_snapshot(app, h);
        refresh_menu(h, now);
    }
}

fn notify(app: &AppHandle, n: &Notification) {
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

fn set_tooltip(app: &AppHandle, session: &Session, live: &[VolumeSample]) {
    let text = tooltip(session.home_volume().as_deref(), live);
    if let Some(tray) = app.tray_by_id("scull") {
        let _ = tray.set_tooltip(Some(text));
    }
}

fn emit_snapshot(app: &AppHandle, h: &MonitorHandle) {
    let snapshot = lock(&h.monitor).snapshot();
    let _ = app.emit_to(MAIN_LABEL, MONITOR_EVENT, snapshot);
}

fn refresh_menu(h: &MonitorHandle, now: i64) {
    let paused = lock(&h.monitor).settings().paused(now);
    if let Some(item) = lock(&h.pause_item).as_ref() {
        let _ = item.set_text(if paused {
            "Resume alerts"
        } else {
            "Pause alerts for 1 hour"
        });
    }
}

fn wake(h: &MonitorHandle) {
    h.wake.notify_all();
}

/// Tray menu "Check now": run a check at once.
pub fn check_now(app: &AppHandle) {
    if let Some(h) = app.try_state::<Arc<MonitorHandle>>() {
        h.force.store(true, Ordering::Relaxed);
        wake(&h);
    }
}

/// Tray menu "Pause alerts for 1 hour" / "Resume alerts".
pub fn toggle_pause(app: &AppHandle) {
    if let Some(h) = app.try_state::<Arc<MonitorHandle>>() {
        let now = now_unix();
        {
            let mut m = lock(&h.monitor);
            let secs = (!m.settings().paused(now)).then_some(PAUSE_HOUR_SECS);
            m.pause(secs, now);
        }
        emit_snapshot(app, &h);
        refresh_menu(&h, now);
        wake(&h);
    }
}

/// `monitor_status`: what the monitor is doing.
#[tauri::command]
pub fn monitor_status(handle: Handle<'_>) -> MonitorStatus {
    lock(&handle.monitor).status(now_unix())
}

/// `configure_monitor`: the main window's settings (and, at start, its saved memory).
#[tauri::command]
pub fn configure_monitor(handle: Handle<'_>, config: MonitorConfig) -> MonitorStatus {
    let now = now_unix();
    let status = {
        let mut m = lock(&handle.monitor);
        m.configure(config, now);
        m.status(now)
    };
    refresh_menu(&handle, now);
    wake(&handle);
    status
}

/// `pause_monitor`: pause notifications for `secs` (at most 7 days), or resume with `null`.
#[tauri::command]
pub fn pause_monitor(app: AppHandle, handle: Handle<'_>, secs: Option<i64>) -> MonitorStatus {
    let now = now_unix();
    let status = {
        let mut m = lock(&handle.monitor);
        m.pause(secs, now);
        m.status(now)
    };
    emit_snapshot(&app, &handle);
    refresh_menu(&handle, now);
    wake(&handle);
    status
}

/// `send_digest_now`: show this week's digest as a notification now (Settings' "Send now").
/// Doesn't move the weekly slot.
#[tauri::command]
pub async fn send_digest_now(
    app: AppHandle,
    session: SharedSession<'_>,
    handle: Handle<'_>,
) -> Result<DigestView, ScullError> {
    let session = Arc::clone(&session);
    let handle = Arc::clone(&handle);
    let app2 = app.clone();
    crate::blocking(move || {
        let (view, n) = lock(&handle.monitor).digest_now(&session, now_unix());
        if let Some(n) = n {
            notify(&app2, &n);
        }
        Ok(view)
    })
    .await
}
