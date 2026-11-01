//! SAFE-10 as the PRD states it: deletion is only possible through the core `Executor` API, and
//! the desktop has no direct filesystem-deletion code path. Since P3-EXEC-01 the desktop runs
//! cleanups: only the bridge depends on `scull-cleaner`, only `src/cleanup.rs` uses it, and
//! that module reaches deletion only through the Executor's API (an allowlist of calls); no
//! desktop source calls a filesystem-mutation API itself. The MCP server never reaches the
//! cleaner.
//!
//! Runs in the main workspace on every OS without building Tauri: it reads manifests and
//! sources only (no network, no build). `apps/desktop/src-tauri/tests/safe10.rs` adds the
//! `cargo tree` check on the resolved graph of the app crate.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Path dependencies declared in `[workspace.dependencies]` of the root manifest.
fn workspace_paths() -> BTreeMap<String, PathBuf> {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let mut out = BTreeMap::new();
    let mut in_ws = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_ws = line == "[workspace.dependencies]";
            continue;
        }
        if in_ws
            && let Some((name, rest)) = line.split_once('=')
            && let Some(p) = quoted_after(rest, "path")
        {
            out.insert(name.trim().to_owned(), root.join(p));
        }
    }
    out
}

fn quoted_after(s: &str, key: &str) -> Option<String> {
    let i = s.find(key)?;
    let rest = &s[i + key.len()..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].to_owned())
}

/// A non-dev dependency section header: `[dependencies]`, `[build-dependencies]`,
/// `[target.'cfg(..)'.dependencies]`, or a `[dependencies.<name>]` table.
fn dep_section(header: &str) -> Option<Option<String>> {
    let h = header.trim_start_matches('[').trim_end_matches(']');
    if h.contains("dev-dependencies") {
        return None;
    }
    let tail = h.rsplit("dependencies").next().unwrap_or("");
    if !h.contains("dependencies") {
        return None;
    }
    if let Some(name) = tail.strip_prefix('.') {
        return Some(Some(name.trim_matches('"').to_owned()));
    }
    Some(None)
}

#[derive(Debug, Default)]
struct Manifest {
    /// `scull-*` crates this manifest depends on (normal and build edges), with their
    /// directory when it can be resolved.
    deps: Vec<(String, Option<PathBuf>)>,
    /// Whether `scull-platform`'s `delete` feature is enabled anywhere.
    enables_delete: bool,
}

fn read_manifest(dir: &Path, ws: &BTreeMap<String, PathBuf>) -> Manifest {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    let mut m = Manifest::default();
    let mut section: Option<Option<String>> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            section = dep_section(line);
            if let Some(Some(name)) = &section
                && name.starts_with("scull-")
            {
                m.deps.push((name.clone(), ws.get(name).cloned()));
            }
            continue;
        }
        if line.contains("scull-platform/delete") || line.contains("scull-platform?/delete") {
            m.enables_delete = true;
        }
        let Some(sec) = &section else { continue };
        let delete = line.contains("\"delete\"") || line.contains("'delete'");
        match sec {
            Some(table) => {
                if table == "scull-platform" && delete {
                    m.enables_delete = true;
                }
                if table.starts_with("scull-")
                    && let Some(p) = quoted_after(line, "path")
                    && line.trim_start().starts_with("path")
                {
                    let d = m.deps.iter_mut().find(|(n, _)| n == table).unwrap();
                    d.1 = Some(dir.join(p));
                }
            }
            None => {
                let name = line
                    .split(['.', ' ', '='])
                    .next()
                    .unwrap_or("")
                    .trim_matches('"');
                if !name.starts_with("scull-") {
                    continue;
                }
                if name == "scull-platform" && delete {
                    m.enables_delete = true;
                }
                let path = quoted_after(line, "path")
                    .map(|p| dir.join(p))
                    .or_else(|| ws.get(name).cloned());
                m.deps.push((name.to_owned(), path));
            }
        }
    }
    m
}

/// Every `scull-*` crate reachable from `start` through normal and build dependencies.
fn reachable(start: &Path) -> (Vec<String>, Vec<String>) {
    let ws = workspace_paths();
    let mut seen: Vec<String> = Vec::new();
    let mut delete: Vec<String> = Vec::new();
    let mut todo = vec![(String::from("<start>"), start.to_path_buf())];
    while let Some((name, dir)) = todo.pop() {
        if seen.contains(&name) {
            continue;
        }
        let m = read_manifest(&dir, &ws);
        if m.enables_delete {
            delete.push(name.clone());
        }
        for (dep, path) in m.deps {
            if dep == "scull-cleaner" {
                seen.push(dep);
                continue;
            }
            let path = path.unwrap_or_else(|| panic!("{name}: cannot resolve {dep}"));
            todo.push((dep, path));
        }
        seen.push(name);
    }
    (seen, delete)
}

/// Direct `scull-*` dependencies of the manifest in `dir`.
fn direct_deps(dir: &Path) -> Vec<String> {
    read_manifest(dir, &workspace_paths())
        .deps
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

/// PRD SAFE-10: deletion only through the core Executor. The bridge reaches `scull-cleaner`
/// (P3-EXEC-01) and never enables `scull-platform`'s `delete` feature itself; the Tauri app
/// crate reaches the cleaner only through the bridge.
#[test]
fn only_the_bridge_reaches_the_cleaner() {
    let bridge = direct_deps(&desktop().join("bridge"));
    assert!(bridge.iter().any(|d| d == "scull-cleaner"), "{bridge:?}");
    let ws = workspace_paths();
    assert!(
        !read_manifest(&desktop().join("bridge"), &ws).enables_delete,
        "the bridge enables scull-platform/delete itself"
    );
    let app = direct_deps(&desktop().join("src-tauri"));
    assert!(app.iter().any(|d| d == "scull-desktop-bridge"), "{app:?}");
    assert!(
        !app.iter().any(|d| d == "scull-cleaner"),
        "the app crate depends on scull-cleaner directly: {app:?}"
    );
    assert!(
        !read_manifest(&desktop().join("src-tauri"), &ws).enables_delete,
        "the app crate enables scull-platform/delete"
    );
    // The MCP server never reaches the cleaner (nor the desktop bridge).
    let (seen, delete) = reachable(&repo_root().join("crates/scull-mcp"));
    assert!(
        !seen
            .iter()
            .any(|c| c == "scull-cleaner" || c == "scull-desktop-bridge"),
        "{seen:?}"
    );
    assert!(delete.is_empty(), "{delete:?}");
}

/// The one desktop module allowed to use `scull_cleaner` (P3-EXEC-01).
const CLEANUP_MODULE: &str = "bridge/src/cleanup.rs";

/// Filesystem-mutation APIs that must not appear in desktop sources outside the Executor
/// module. The history database is written by scull-storage, never by desktop code.
const MUTATION_APIS: &[&str] = &[
    "scull_cleaner",
    "remove_file",
    "remove_dir",
    "fs::rename",
    "fs::write",
    "fs::copy",
    "create_dir",
    "set_permissions",
    "File::create",
    "OpenOptions",
    "hard_link",
    "symlink",
    "trash::",
];

/// What even the Executor module may not do itself: every deletion or move goes through the
/// Executor's own API.
const DIRECT_DELETION: &[&str] = &[
    "remove_file",
    "remove_dir",
    "fs::rename",
    "fs::write",
    "fs::copy",
    "set_permissions",
    "File::create",
    "OpenOptions",
    "hard_link",
    "symlink",
    "scull_platform::remove",
    "scull_platform::trash",
    "scull_platform::quarantine",
    ".delete(",
    "delete_observed",
    "remove_orphaned",
    "purge",
];

/// The Executor / Quick Clean calls the module may make.
const EXECUTOR_CALLS: &[&str] = &[
    "acquire",
    "recover",
    "begin_operation",
    "execute_plan",
    "restore",
    "plan",
    "run",
    "with_quarantine",
    "with_trash",
    "with_elevation",
    "finish",
    "finish_measured",
    "audit_plan",
];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The code of `f` before its unit tests (they may build temp trees).
fn code_of(f: &Path) -> String {
    let text = std::fs::read_to_string(f).unwrap();
    text.split("#[cfg(test)]").next().unwrap_or("").to_owned()
}

#[test]
fn desktop_sources_have_no_filesystem_mutation() {
    let mut files = Vec::new();
    for d in ["bridge/src", "src-tauri/src", "src-tauri/build.rs"] {
        let p = desktop().join(d);
        if p.is_file() {
            files.push(p);
        } else {
            rust_sources(&p, &mut files);
        }
    }
    assert!(files.len() >= 3, "found {files:?}");
    let cleanup = desktop().join(CLEANUP_MODULE);
    for f in files.iter().filter(|f| **f != cleanup) {
        let code = code_of(f);
        for api in MUTATION_APIS {
            assert!(!code.contains(api), "{} uses {api}", f.display());
        }
    }
}

#[test]
fn the_cleanup_module_deletes_only_through_the_executor() {
    let code = code_of(&desktop().join(CLEANUP_MODULE));
    for api in DIRECT_DELETION {
        assert!(!code.contains(api), "{CLEANUP_MODULE} uses {api}");
    }
    // Methods called on the Executor and Quick Clean handles.
    for handle in ["executor.", "qc.", "Executor::"] {
        for (i, _) in code.match_indices(handle) {
            let rest = &code[i + handle.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert!(
                EXECUTOR_CALLS.contains(&name.as_str()),
                "{CLEANUP_MODULE} calls {handle}{name}"
            );
        }
    }
    // The only directory it creates is Quick Clean's private sandbox, owner-only, in scull's
    // own data directory.
    assert_eq!(
        code.matches("create_dir").count(),
        1,
        "one create_dir_owner_only"
    );
    assert!(code.contains("scull_platform::create_dir_owner_only(&dir)"));
    // Every operation it starts is recorded as the human's desktop confirmation.
    assert!(code.contains("method: ApprovalMethod::Desktop"));
    assert!(!code.contains("ApprovalMethod::Cli"));
}

#[test]
fn detector_sees_every_spelling() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let ws = workspace_paths();
    for (text, cleaner, del) in [
        (
            "[dependencies]\nscull-cleaner.workspace = true\n",
            true,
            false,
        ),
        (
            "[target.'cfg(unix)'.dependencies]\nscull-cleaner = { path = \"x\" }\n",
            true,
            false,
        ),
        ("[dependencies.scull-cleaner]\npath = \"x\"\n", true, false),
        ("[build-dependencies]\nscull-cleaner = \"0\"\n", true, false),
        (
            "[dependencies]\nscull-platform = { workspace = true, features = [\"delete\"] }\n",
            false,
            true,
        ),
        (
            "[dependencies.scull-platform]\nworkspace = true\nfeatures = ['delete']\n",
            false,
            true,
        ),
        ("[features]\nx = [\"scull-platform/delete\"]\n", false, true),
        (
            "[dev-dependencies]\nscull-cleaner.workspace = true\n",
            false,
            false,
        ),
        (
            "[dependencies]\nscull-platform.workspace = true # features = [\"delete\"]\n",
            false,
            false,
        ),
    ] {
        std::fs::write(tmp.join("Cargo.toml"), text).unwrap();
        let m = read_manifest(tmp, &ws);
        assert_eq!(
            m.deps.iter().any(|(n, _)| n == "scull-cleaner"),
            cleaner,
            "{text}"
        );
        assert_eq!(m.enables_delete, del, "{text}");
    }
}
