//! SAFE-10: the removal walker (`scull-platform`'s `delete` feature) is enabled by
//! `scull-cleaner` only. Any other crate that enabled it could compile a deletion code path
//! outside the Executor. Reads every workspace manifest (no network, no build).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Manifests in the workspace: the root, every crate, and any app crate.
fn manifests() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut out = vec![root.join("Cargo.toml")];
    for dir in ["crates", "apps"] {
        let Ok(rd) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for e in rd.flatten() {
            for candidate in [
                e.path().join("Cargo.toml"),
                e.path().join("src-tauri/Cargo.toml"),
            ] {
                if candidate.is_file() {
                    out.push(candidate);
                }
            }
        }
    }
    out
}

/// Whether a manifest enables the `delete` feature of `scull-platform` anywhere (inline table,
/// dotted key or `[dependencies.scull-platform]` section, any dependency kind).
fn enables_delete(text: &str) -> bool {
    let mut in_platform_section = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_platform_section = line.trim_end_matches(']').ends_with("scull-platform");
            continue;
        }
        let mentions_feature = line.contains("\"delete\"") || line.contains("'delete'");
        if mentions_feature && (line.starts_with("scull-platform") || in_platform_section) {
            return true;
        }
        // A feature of the manifest's own that forwards to it: `x = ["scull-platform/delete"]`.
        if line.contains("scull-platform/delete") || line.contains("scull-platform?/delete") {
            return true;
        }
    }
    false
}

#[test]
fn only_the_cleaner_enables_the_delete_feature() {
    let manifests = manifests();
    assert!(manifests.len() > 5, "found {manifests:?}");
    let mut enabling = Vec::new();
    for m in &manifests {
        let text = std::fs::read_to_string(m).unwrap();
        if enables_delete(&text) {
            enabling.push(m.clone());
        }
    }
    let cleaner = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    assert_eq!(
        enabling.len(),
        1,
        "only scull-cleaner may enable scull-platform's `delete` feature: {enabling:?}"
    );
    assert_eq!(
        std::fs::canonicalize(&enabling[0]).unwrap(),
        std::fs::canonicalize(cleaner).unwrap()
    );
}

/// The shipped binaries do not compile the walker in: `cargo tree -e features` for `scull-cli`
/// and `scull-mcp` (normal + build edges, every target) never shows `scull-platform`'s
/// `delete` feature. Lifted for `scull-cli` only when P2-CLI-01 wires `scull clean` behind the
/// Phase 2 safeguards.
#[test]
fn shipped_binaries_do_not_enable_the_delete_feature() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    for package in ["scull-cli", "scull-mcp", "scull-engine"] {
        let out = std::process::Command::new(&cargo)
            .current_dir(workspace_root())
            .args([
                "tree",
                "--offline",
                "--locked",
                "-p",
                package,
                "-e",
                "features,normal,build",
                "--target",
                "all",
                "--prefix",
                "none",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && text.contains("scull-platform"),
            "cargo tree -p {package} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let bad: Vec<&str> = text
            .lines()
            .filter(|l| {
                l.contains("scull-platform feature \"delete\"") || l.contains("scull-cleaner")
            })
            .collect();
        assert!(bad.is_empty(), "{package} pulls in the walker: {bad:?}");
    }
}

/// No source of the interface crates refers to the Executor (SAFE-10; until P2-CLI-01).
#[test]
fn interface_crates_do_not_use_the_cleaner() {
    let root = workspace_root();
    for krate in ["scull-cli", "scull-mcp", "scull-engine"] {
        let mut stack = vec![root.join("crates").join(krate).join("src")];
        let mut files = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(
                        !text.contains("scull_cleaner"),
                        "{} refers to scull_cleaner",
                        p.display()
                    );
                }
            }
        }
        assert!(files > 0, "no sources found for {krate}");
        let manifest =
            std::fs::read_to_string(root.join("crates").join(krate).join("Cargo.toml")).unwrap();
        let deps = manifest
            .split("\n[")
            .filter(|s| s.starts_with("dependencies") || s.starts_with("target."))
            .collect::<String>();
        assert!(
            !deps
                .lines()
                .any(|l| l.trim_start().starts_with("scull-cleaner")),
            "{krate} depends on scull-cleaner"
        );
    }
}

#[test]
fn detector_sees_every_spelling() {
    for yes in [
        "[dependencies]\nscull-platform = { workspace = true, features = [\"delete\"] }\n",
        "[dev-dependencies.scull-platform]\nworkspace = true\nfeatures = [\"delete\"]\n",
        "[features]\nx = [\"scull-platform/delete\"]\n",
        "[target.'cfg(unix)'.dependencies]\nscull-platform = { path = \"../p\", features = ['delete'] }\n",
    ] {
        assert!(enables_delete(yes), "{yes}");
    }
    for no in [
        "[dependencies]\nscull-platform.workspace = true\n",
        "[features]\ndelete = []\n",
        "[dependencies]\nscull-platform = { workspace = true } # features = [\"delete\"]\n",
    ] {
        assert!(!enables_delete(no), "{no}");
    }
}
