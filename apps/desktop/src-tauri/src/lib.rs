//! The scull desktop app's Tauri shell.
//!
//! Each command is a thin wrapper over `scull_desktop_bridge`, which calls the read-only scull
//! crates. Nothing here (or in the bridge) can delete, move or modify a file (SAFE-10): the
//! crate graph has no `scull-cleaner`, and the window's capabilities grant no filesystem
//! plugin. Errors reach the frontend as [`ScullError`] JSON.

use scull_core::{ErrorKind, ScullError};
use scull_desktop_bridge::{AppInfo, Drive};

/// `app_info`: scull version and OS.
#[tauri::command]
fn app_info() -> AppInfo {
    scull_desktop_bridge::app_info()
}

/// `drives`: mounted volumes for the sidebar's drives widget. Reads the mount table off the
/// UI thread.
#[tauri::command]
async fn drives() -> Result<Vec<Drive>, ScullError> {
    blocking(scull_desktop_bridge::drives).await
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
        .invoke_handler(tauri::generate_handler![app_info, drives])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("scull: the desktop app failed to start: {e}");
        std::process::exit(1);
    }
}
