//! Validated edits to scull's own configuration; no arbitrary desktop file writes.

use scull_core::config::{ConfigLayer, edit_setting, expand_home, validate_roots};
use scull_core::{ErrorKind, FsPath, ScullError};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static WRITE_LOCK: Mutex<()> = Mutex::new(());
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn read(path: &Path) -> Result<Option<String>, ScullError> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => {
            let (mut file, _) = crate::open_file_no_follow(path).map_err(|e| {
                ScullError::new(ErrorKind::Io, format!("cannot read config file: {e}"))
            })?;
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            Ok(Some(text))
        }
        Ok(_) => Err(ScullError::new(
            ErrorKind::Config,
            "config.toml must be a regular file, not a symbolic link or directory",
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn check_paths(text: &str, key: &str, home: Option<&Path>) -> Result<(), ScullError> {
    if !matches!(key, "scan.roots" | "scan.protect") {
        return Ok(());
    }
    let layer = ConfigLayer::from_toml(text)?;
    let paths: Vec<PathBuf> = if key == "scan.roots" {
        layer.scan.roots.unwrap_or_default()
    } else {
        layer.scan.protect.unwrap_or_default().into_iter().collect()
    };
    let mut expanded = Vec::new();
    for path in paths {
        let path = home.map(|h| expand_home(&path, h)).unwrap_or(path);
        if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
            return Err(ScullError::new(
                ErrorKind::Config,
                "folders must be absolute paths or start with ~/ and must not contain ..",
            ));
        }
        expanded.push(if key == "scan.roots" {
            let root = std::fs::canonicalize(path)?;
            if !root.is_dir() {
                return Err(ScullError::new(
                    ErrorKind::Config,
                    "scan roots must be directories",
                ));
            }
            root
        } else {
            path
        });
    }
    if key == "scan.roots" && !expanded.is_empty() {
        let home = home.ok_or_else(|| {
            ScullError::new(
                ErrorKind::Config,
                "cannot validate roots without a home directory",
            )
        })?;
        let home = std::fs::canonicalize(home)?;
        let mounts = crate::volumes().map_err(|e| {
            ScullError::new(ErrorKind::Io, format!("cannot validate scan volumes: {e}"))
        })?;
        let mounts: Vec<_> = mounts.into_iter().map(|v| v.mount_point).collect();
        if !validate_roots(&expanded, &home, &mounts)?.is_empty() {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "broad scan roots need explicit confirmation; set them through the command line",
            ));
        }
    }
    Ok(())
}

/// Save one allowlisted setting. The caller supplies scull's trusted config location.
pub fn save_setting(
    path: &Path,
    key: &str,
    value: &Value,
    home: Option<&Path>,
) -> Result<(), ScullError> {
    let _lock = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let save = || -> Result<(), ScullError> {
        let before = read(path)?;
        let after = edit_setting(before.as_deref().unwrap_or(""), key, value)?;
        check_paths(&after, key, home)?;
        if before.as_deref() == Some(after.as_str()) {
            return Ok(());
        }
        let parent = path.parent().ok_or_else(|| {
            ScullError::new(ErrorKind::Config, "config file has no parent directory")
        })?;
        crate::create_dir_owner_only(parent).map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot create config directory: {e}"),
            )
        })?;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_file_name(format!(".{name}.tmp-{}-{id}", std::process::id()));
        let mut file = crate::owner_only::create_file(&temp)?;
        let result = (|| -> Result<(), ScullError> {
            file.write_all(after.as_bytes())?;
            file.sync_all()?;
            drop(file);
            if read(path)? != before {
                return Err(ScullError::new(
                    ErrorKind::Config,
                    "config.toml changed while saving; retry the change",
                ));
            }
            std::fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    };
    save().map_err(|e| e.with_path(FsPath::from_path(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn failed_temp_creation_preserves_the_original_and_existing_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let old = "[mcp]\nenabled = false\n";
        std::fs::write(&path, old).unwrap();
        let id = TEMP_ID.load(Ordering::Relaxed);
        let temp = dir
            .path()
            .join(format!(".config.toml.tmp-{}-{id}", std::process::id()));
        std::fs::write(&temp, "existing temporary file").unwrap();
        assert!(save_setting(&path, "mcp.enabled", &Value::Bool(true), Some(dir.path())).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(temp).unwrap(),
            "existing temporary file"
        );
    }
}
