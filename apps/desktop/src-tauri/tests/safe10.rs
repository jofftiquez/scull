//! SAFE-10 on the resolved graph: `cargo tree` of the app crate (normal + build edges, every
//! target, with features) never shows `scull-cleaner` or `scull-platform`'s `delete` feature,
//! and no Tauri plugin that could touch the filesystem or spawn processes is linked.
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

#[test]
fn app_graph_has_no_cleaner_and_no_delete_feature() {
    let text = tree("features,normal,build");
    assert!(
        text.contains("scull-desktop-bridge") && text.contains("scull-platform"),
        "unexpected tree:\n{text}"
    );
    let bad: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("scull-cleaner") || l.contains("scull-platform feature \"delete\""))
        .collect();
    assert!(bad.is_empty(), "scull-desktop reaches the deleter: {bad:?}");
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

#[test]
fn capabilities_grant_no_filesystem_access() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        n += 1;
        let text = std::fs::read_to_string(e.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        for p in v["permissions"].as_array().unwrap() {
            let id = p.as_str().or_else(|| p["identifier"].as_str()).unwrap();
            assert!(
                id.starts_with("core:"),
                "{}: permission {id} is outside Tauri core",
                e.path().display()
            );
        }
    }
    assert!(n > 0);
}
