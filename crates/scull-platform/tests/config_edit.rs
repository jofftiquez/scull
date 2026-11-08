//! Atomic configuration persistence and rejected edits that leave user files untouched.
#![allow(clippy::unwrap_used)]

use scull_platform::config::save_setting;
use serde_json::json;

#[test]
fn saves_and_reloads_without_changing_unrelated_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config/config.toml");
    save_setting(&path, "mcp.enabled", &json!(true), Some(dir.path())).unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("enabled = true")
    );
    std::fs::write(&path, "# keep\n[mcp]\ndestructive = true\n").unwrap();
    save_setting(&path, "mcp.hash_names", &json!(true), Some(dir.path())).unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("# keep\n[mcp]\ndestructive = true\n")
    );
}

#[test]
fn invalid_changes_preserve_the_original_bytes_and_create_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let old = "[mcp]\nenabled = false\n";
    std::fs::write(&path, old).unwrap();
    for (key, value) in [
        ("mcp.destructive", json!(true)),
        ("scan.roots", json!(["relative"])),
        ("scan.roots", json!([dir.path().to_string_lossy()])),
        ("mcp.enabled", json!(42)),
    ] {
        assert!(save_setting(&path, key, &value, Some(dir.path())).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    std::fs::write(&path, "[broken").unwrap();
    assert!(save_setting(&path, "mcp.enabled", &json!(true), Some(dir.path())).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "[broken");
}

#[test]
fn ordinary_roots_save_and_the_final_root_can_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let path = dir.path().join("config.toml");
    save_setting(&path, "scan.roots", &json!([project]), Some(dir.path())).unwrap();
    save_setting(&path, "scan.roots", &json!([]), Some(dir.path())).unwrap();
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("roots = []")
    );
}

#[cfg(unix)]
#[test]
fn root_links_cannot_bypass_broad_root_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
    let path = dir.path().join("config.toml");
    assert!(save_setting(&path, "scan.roots", &json!([alias]), Some(dir.path())).is_err());
    assert!(!path.exists());
}

#[cfg(unix)]
#[test]
fn refuses_links_without_touching_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, "[mcp]\nenabled = false\n").unwrap();
    let path = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(save_setting(&path, "mcp.enabled", &json!(true), Some(dir.path())).is_err());
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "[mcp]\nenabled = false\n"
    );
}
