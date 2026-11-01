//! The menu bar / tray widget (design 11, D-23; P4-UI-01).
//!
//! A tray icon (the macOS menu bar, the Windows notification area, a Linux app indicator) with a
//! small menu, and a 380 × 660 widget window (`menubar.html`) that a left click toggles next to
//! the icon. Linux app indicators report no clicks, so there the menu's "Show scull widget"
//! opens it in the top-right corner. The widget hides when it loses focus or on Escape.
//!
//! Read-only like the rest of the app (SAFE-10): the widget reads the same bridge commands as the
//! main window, and its only extra commands move windows: [`open_main_window`] (allowlisted
//! routes, see `scull_desktop_bridge::menubar::MAIN_ROUTES`) and [`hide_menu_bar`]. The widget
//! window is created on first use, so an app that never opens it pays nothing for it, and it is
//! closed with the main window: no process lingers in the tray (no compulsory daemon, PRD §15).
//! Where the widget goes is computed by `scull_desktop_bridge::menubar::popover_position`.

use scull_core::{ErrorKind, ScullError};
use scull_desktop_bridge::menubar::{
    self, NAVIGATE_EVENT, PxRect, WIDGET_LABEL, WIDGET_SIZE, WIDGET_URL,
};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, Rect, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, Window, WindowEvent,
};

/// The main window's label (tauri.conf.json).
pub const MAIN_LABEL: &str = "main";

/// A click on the tray icon this soon after the widget hid on blur is the click that blurred
/// it: it closes the widget instead of reopening it.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

/// When the widget last hid because it lost focus.
#[derive(Default)]
struct WidgetState {
    blurred_at: Mutex<Option<Instant>>,
}

/// Add the tray icon and its menu (called from the app's `setup`).
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    app.manage(WidgetState::default());
    let widget = MenuItem::with_id(app, "widget", "Show scull widget", true, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open scull", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit scull", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&widget, &open, &separator, &quit])?;
    let mut tray = TrayIconBuilder::with_id("scull")
        .tooltip("scull")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "widget" => report(toggle_widget(app, None)),
            "open" => report(show_main(app, "/")),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                report(toggle_widget(tray.app_handle(), Some(rect)));
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    // On Linux the tray needs libayatana-appindicator3 (or libappindicator3) at run time, and
    // the binding panics when neither is installed. The app must still start: without the
    // library there is simply no tray icon (and no widget).
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tray.build(app))) {
        Ok(built) => {
            built?;
        }
        Err(_) => eprintln!(
            "scull: no tray icon on this desktop (Linux needs libayatana-appindicator3); \
             the menu bar widget is unavailable, the app works without it"
        ),
    }
    // Development only: open the widget at start (screenshots without a tray host).
    if cfg!(debug_assertions) && std::env::var_os("SCULL_DEV_OPEN_WIDGET").is_some() {
        report(toggle_widget(app, None));
    }
    Ok(())
}

/// Window events of the app: closing the main window closes the widget too, so the app exits.
pub fn on_window_event(window: &Window, event: &WindowEvent) {
    if window.label() == MAIN_LABEL
        && matches!(event, WindowEvent::Destroyed)
        && let Some(w) = window.app_handle().get_webview_window(WIDGET_LABEL)
    {
        let _ = w.destroy();
    }
}

/// `open_main_window`: show and focus the main window on `route` (one of
/// `scull_desktop_bridge::menubar::MAIN_ROUTES`), then hide the widget.
#[tauri::command]
pub fn open_main_window(app: AppHandle, route: String) -> Result<(), ScullError> {
    let route = menubar::main_route(&route)?;
    show_main(&app, route).map_err(internal)?;
    if let Some(w) = app.get_webview_window(WIDGET_LABEL) {
        w.hide().map_err(internal)?;
    }
    Ok(())
}

/// `hide_menu_bar`: hide the widget (Escape in the widget).
#[tauri::command]
pub fn hide_menu_bar(app: AppHandle) -> Result<(), ScullError> {
    if let Some(w) = app.get_webview_window(WIDGET_LABEL) {
        w.hide().map_err(internal)?;
    }
    Ok(())
}

fn internal(e: tauri::Error) -> ScullError {
    ScullError::new(ErrorKind::Internal, format!("window: {e}"))
}

fn report(r: tauri::Result<()>) {
    if let Err(e) = r {
        eprintln!("scull: menu bar widget: {e}");
    }
}

fn show_main(app: &AppHandle, route: &str) -> tauri::Result<()> {
    if let Some(main) = app.get_webview_window(MAIN_LABEL) {
        main.unminimize()?;
        main.show()?;
        main.set_focus()?;
    }
    app.emit_to(MAIN_LABEL, NAVIGATE_EVENT, route)
}

fn toggle_widget(app: &AppHandle, anchor: Option<Rect>) -> tauri::Result<()> {
    let state = app.state::<WidgetState>();
    let just_blurred = state
        .blurred_at
        .lock()
        .map(|mut t| t.take().is_some_and(|at| at.elapsed() < REOPEN_GUARD))
        .unwrap_or(false);
    let w = match app.get_webview_window(WIDGET_LABEL) {
        Some(w) => {
            if w.is_visible()? {
                return w.hide();
            }
            if just_blurred {
                return Ok(());
            }
            w
        }
        None => create_widget(app)?,
    };
    place(&w, anchor)?;
    w.show()?;
    w.set_focus()
}

fn create_widget(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let w = WebviewWindowBuilder::new(app, WIDGET_LABEL, WebviewUrl::App(WIDGET_URL.into()))
        .title("scull")
        .inner_size(WIDGET_SIZE.0, WIDGET_SIZE.1)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()?;
    let handle = w.clone();
    let app = app.clone();
    w.on_window_event(move |e| {
        if let WindowEvent::Focused(false) = e
            && handle.is_visible().unwrap_or(false)
        {
            if let Ok(mut t) = app.state::<WidgetState>().blurred_at.lock() {
                *t = Some(Instant::now());
            }
            let _ = handle.hide();
        }
    });
    Ok(w)
}

/// Put the widget next to the tray icon (or in the top-right corner of the work area).
fn place(w: &WebviewWindow, anchor: Option<Rect>) -> tauri::Result<()> {
    let at = anchor.map(|r| r.position.to_physical::<f64>(1.0));
    let monitor = match at {
        Some(p) => w.monitor_from_point(p.x, p.y)?,
        None => None,
    };
    let Some(monitor) = monitor.or(w.primary_monitor()?) else {
        return Ok(());
    };
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let work = PxRect {
        x: area.position.x,
        y: area.position.y,
        width: area.size.width,
        height: area.size.height,
    };
    let anchor = anchor.map(|r| {
        let p = r.position.to_physical::<i32>(scale);
        let s = r.size.to_physical::<u32>(scale);
        PxRect {
            x: p.x,
            y: p.y,
            width: s.width,
            height: s.height,
        }
    });
    let size = (
        (WIDGET_SIZE.0 * scale).round() as u32,
        (WIDGET_SIZE.1 * scale).round() as u32,
    );
    let (x, y) = menubar::popover_position(anchor, work, size);
    w.set_position(PhysicalPosition::new(x, y))
}
