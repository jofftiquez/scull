//! Main-window-only context and native project-folder selection. No config is written.

use crate::Shared;
use scull_core::{ErrorKind, ScullError};
use serde::Serialize;
use tauri::{Runtime, WebviewWindow};

/// Native context independent of previous scans and cache discovery.
#[derive(Serialize)]
pub struct OnboardingContext {
    home: Option<String>,
}

/// The actual home directory used by the core's scan-root validation.
#[tauri::command]
pub fn onboarding_context(session: Shared<'_>) -> Result<OnboardingContext, ScullError> {
    let home = session
        .setup()
        .home
        .as_ref()
        .map(|p| {
            p.to_str().map(str::to_owned).ok_or_else(|| {
                ScullError::new(
                    ErrorKind::Usage,
                    "The home directory cannot be represented as text.",
                )
            })
        })
        .transpose()?;
    Ok(OnboardingContext { home })
}

/// Select directories without granting the webview filesystem or save-dialog access.
#[tauri::command]
pub async fn pick_project_folders<R: Runtime>(
    window: WebviewWindow<R>,
) -> Result<Option<Vec<String>>, ScullError> {
    let dialog = rfd::FileDialog::new()
        .set_parent(&window)
        .set_title("Choose project folders")
        .set_can_create_directories(false);
    crate::blocking(move || {
        dialog
            .pick_folders()
            .map(|paths| {
                paths
                    .into_iter()
                    .map(|p| {
                        p.to_str().map(str::to_owned).ok_or_else(|| {
                            ScullError::new(
                                ErrorKind::Usage,
                                "A selected folder cannot be represented as text.",
                            )
                        })
                    })
                    .collect()
            })
            .transpose()
    })
    .await
}
