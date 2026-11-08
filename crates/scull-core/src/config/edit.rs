use super::ConfigLayer;
use crate::{ErrorKind, ScullError};
use serde_json::Value;
use toml_edit::{Array, DocumentMut, Item, Table};

/// Change one desktop-editable setting, preserving unrelated values and comments.
/// File-only security switches and app preferences are deliberately not accepted.
pub fn edit_setting(text: &str, key: &str, value: &Value) -> Result<String, ScullError> {
    let invalid = || ScullError::new(ErrorKind::Config, format!("invalid value for {key}"));
    let (section, field, mut replacement) = match key {
        "mcp.enabled" | "mcp.redact_home" | "mcp.hash_names" => (
            "mcp",
            &key[4..],
            toml_edit::Value::from(value.as_bool().ok_or_else(invalid)?),
        ),
        "scan.activity_window" => (
            "scan",
            "activity_window",
            toml_edit::Value::from(value.as_str().ok_or_else(invalid)?),
        ),
        "scan.roots" | "scan.protect" => {
            let mut paths = Array::new();
            for path in value.as_array().ok_or_else(invalid)? {
                paths.push(path.as_str().ok_or_else(invalid)?);
            }
            ("scan", &key[5..], toml_edit::Value::Array(paths))
        }
        "mcp.destructive" => {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "mcp.destructive is file-only and also requires --allow-destructive",
            ));
        }
        _ => {
            return Err(ScullError::new(
                ErrorKind::UnsupportedPlatform,
                format!("{key} cannot be changed from the app yet"),
            ));
        }
    };
    ConfigLayer::from_toml(text)?;
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|e| ScullError::new(ErrorKind::Config, format!("invalid user config: {e}")))?;
    let table = doc
        .as_table_mut()
        .entry(section)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(invalid)?;
    if let Some(old) = table.get(field).and_then(Item::as_value) {
        *replacement.decor_mut() = old.decor().clone();
    }
    table.insert(field, Item::Value(replacement));
    let updated = doc.to_string();
    ConfigLayer::from_toml(&updated)?;
    Ok(updated)
}
