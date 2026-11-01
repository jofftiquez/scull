//! SAFE-10: the desktop has no filesystem-mutation path. Deletion happens only in
//! `scull-cleaner`'s Executor, so neither the bridge nor the Tauri app crate may reach it.
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

#[test]
fn bridge_never_reaches_the_cleaner() {
    let (seen, delete) = reachable(&desktop().join("bridge"));
    assert!(seen.iter().any(|c| c == "scull-platform"), "{seen:?}");
    assert!(
        !seen.iter().any(|c| c == "scull-cleaner"),
        "the desktop bridge reaches scull-cleaner: {seen:?}"
    );
    assert!(
        delete.is_empty(),
        "enables scull-platform/delete: {delete:?}"
    );
}

#[test]
fn app_crate_never_reaches_the_cleaner() {
    let app = desktop().join("src-tauri");
    let (seen, delete) = reachable(&app);
    assert!(
        seen.iter().any(|c| c == "scull-desktop-bridge"),
        "src-tauri should go through the bridge: {seen:?}"
    );
    assert!(
        !seen.iter().any(|c| c == "scull-cleaner"),
        "the desktop app reaches scull-cleaner: {seen:?}"
    );
    assert!(
        delete.is_empty(),
        "enables scull-platform/delete: {delete:?}"
    );
}

/// Filesystem-mutation APIs that must not appear in desktop sources. The history database is
/// written by scull-storage (through the engine), never by desktop code.
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
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        // Skip unit tests (they may build temp trees); everything before `#[cfg(test)]`.
        let code = text.split("#[cfg(test)]").next().unwrap_or("");
        for api in MUTATION_APIS {
            assert!(!code.contains(api), "{} uses {api}", f.display());
        }
    }
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
