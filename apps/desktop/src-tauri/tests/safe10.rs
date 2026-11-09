//! SAFE-10 on the resolved graph (PRD: deletion only through the core Executor): `cargo tree` of
//! the app crate shows `scull-cleaner` only under the bridge (P3-EXEC-01), and no Tauri plugin
//! that could touch the filesystem or spawn processes is linked.
//!
//! Complements apps/desktop/bridge/tests/safe10.rs (manifest walk, runs in the main
//! workspace). Uses only the local registry cache (`--offline --locked`): no network, so run
//! `cargo fetch --locked` once beforehand (every target's crates must be cached).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

fn tree(edges: &str) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let out = Command::new(cargo)
        .args(["tree", "--offline", "--locked", "--manifest-path"])
        .arg(&manifest)
        .args([
            "-p",
            "scull-desktop",
            "-e",
            edges,
            "--target",
            "all",
            "--prefix",
            "none",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cargo tree failed (run `cargo fetch --locked` first: --offline needs every target's \
         crates in the local registry): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// `cargo tree -i <krate>` of the app crate: what depends on `krate`, one level deep.
fn dependents(krate: &str) -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let out = Command::new(cargo)
        .args(["tree", "--offline", "--locked", "--manifest-path"])
        .arg(&manifest)
        .args([
            "-i", krate, "-e", "normal", "--target", "all", "--depth", "1", "--prefix", "depth",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix('1'))
        .map(|l| l.split_whitespace().next().unwrap_or("").to_owned())
        .collect()
}

/// PRD SAFE-10 (P3-EXEC-01): the app reaches the deleter only through the bridge, whose
/// `cleanup` module calls the Executor's API (bridge/tests/safe10.rs checks that module). No
/// other crate of the app depends on `scull-cleaner`, and `scull-platform`'s `delete` feature
/// comes only from it.
#[test]
fn app_reaches_the_cleaner_only_through_the_bridge() {
    let text = tree("features,normal,build");
    assert!(
        text.contains("scull-desktop-bridge") && text.contains("scull-cleaner"),
        "unexpected tree:\n{text}"
    );
    assert_eq!(dependents("scull-cleaner"), ["scull-desktop-bridge"]);
    // The app crate's own scull-platform dependency (startup_env) never enables `delete`.
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let line = manifest
        .lines()
        .find(|l| l.starts_with("scull-platform"))
        .unwrap();
    assert!(
        !line.contains("delete") && !line.contains("features"),
        "{line}"
    );
}

#[test]
fn no_filesystem_or_process_plugins() {
    let text = tree("normal");
    for plugin in [
        "tauri-plugin-fs",
        "tauri-plugin-shell",
        "tauri-plugin-process",
        "tauri-plugin-http",
        "tauri-plugin-updater",
        "trash ",
    ] {
        assert!(
            !text.lines().any(|l| l.starts_with(plugin)),
            "scull-desktop links {plugin}"
        );
    }
}

/// `allow-<command>` for each app command listed in build.rs's app manifest.
fn app_command_permissions() -> Vec<String> {
    let build =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs")).unwrap();
    let start = build.find("const COMMANDS").unwrap();
    let end = start + build[start..].find("];").unwrap();
    build[start..end]
        .lines()
        .filter_map(|l| l.split('"').nth(1))
        .map(|c| format!("allow-{}", c.replace('_', "-")))
        .collect()
}

#[test]
fn capabilities_grant_no_filesystem_access() {
    // Tauri core permissions, and scull's own app commands (their names are checked against
    // mutating words in bridge/tests/commands.rs); never a plugin's.
    let own = app_command_permissions();
    // The manifest parsed (the MVP set has 7 commands, #144).
    assert!(
        own.iter().any(|o| o == "allow-execute-desktop-plan"),
        "{own:?}"
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        n += 1;
        let text = std::fs::read_to_string(e.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            v.get("remote").is_none(),
            "{}: no remote URL may use a capability",
            e.path().display()
        );
        for p in v["permissions"].as_array().unwrap() {
            let id = p.as_str().or_else(|| p["identifier"].as_str()).unwrap();
            assert!(
                id.starts_with("core:") || own.iter().any(|o| o == id),
                "{}: permission {id} is outside Tauri core and scull's own commands",
                e.path().display()
            );
        }
    }
    assert!(n > 0);
}

#[test]
fn release_builds_have_no_web_inspector() {
    // P4-MCP-02 review: no devtools in any build profile (the inspector could read the
    // confirmation code and call the approval commands).
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for line in manifest
        .lines()
        .filter(|l| l.trim_start().starts_with("tauri "))
    {
        assert!(!line.contains("devtools"), "{line}");
    }
}
