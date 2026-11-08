//! Preserving edits and the desktop configuration allowlist.
#![allow(clippy::unwrap_used)]

use scull_core::config::edit_setting;
use serde_json::json;

#[test]
fn edits_one_setting_preserving_comments_and_security_settings() {
    let old = "# user configuration\n[mcp]\n# explicit opt-in\ndestructive = true\nhash_names = false # names\n[scan]\nthreads = 3\n";
    let new = edit_setting(old, "mcp.hash_names", &json!(true)).unwrap();
    assert!(new.contains("# user configuration"));
    assert!(new.contains("# explicit opt-in\ndestructive = true"));
    assert!(new.contains("hash_names = true # names"));
    assert!(new.contains("threads = 3"));
}

#[test]
fn rejects_unknown_keys_wrong_types_and_invalid_existing_configuration() {
    for (key, value) in [
        ("mcp.destructive", json!(true)),
        ("mcp.max_pending_plans", json!(100)),
        ("app.open_at_login", json!(true)),
        ("scan.roots", json!("/tmp")),
        ("mcp.enabled", json!("true")),
        ("scan.activity_window", json!("nonsense")),
    ] {
        assert!(edit_setting("", key, &value).is_err(), "{key}");
    }
    assert!(edit_setting("[scan]\nrootz = []\n", "mcp.enabled", &json!(true)).is_err());
}
