//! Main-window-only configuration edits through the validated core/platform writer.

use crate::Shared;
use scull_core::ScullError;
use serde_json::Value;

/// Save a setting through the session's trusted config location.
#[tauri::command]
pub async fn save_setting(
    session: Shared<'_>,
    key: String,
    value: Value,
) -> Result<(), ScullError> {
    let session = session.inner().clone();
    crate::blocking(move || session.save_setting(&key, &value)).await
}
