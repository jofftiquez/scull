//! IDE and editor caches (P5-DET-05, PRD §7.1): JetBrains caches, editor workspace storage of
//! workspaces that no longer exist, and extension versions the editor marked obsolete. Never
//! settings or extension state that is in use.
//!
//! Every test builds a fake home folder in a `scull-fixtures` temp tree (never the real one),
//! detects read-only through `scull-platform`, and checks the universal invariants (I-2
//! canary, I-3 tree unchanged, I-1 on the would-be plan).

#![allow(clippy::unwrap_used)]

mod common;

use common::{
    age_tree, check, classify_expect, clean_caller, identity_available, plan_inputs, rel, scan,
};
use scull_core::{Action, Evidence, Tier};
use scull_detectors::DetectedArtifact;
use scull_fixtures::{Fixture, days_ago};
use std::path::Path;

fn found(root: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = scan(root, &[])
        .found
        .iter()
        .map(|a| (a.rule.id.clone(), rel(root, &a.path)))
        .collect();
    v.sort();
    v
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = v
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect();
    v.sort();
    v
}

fn notes(a: &DetectedArtifact) -> Vec<&str> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn expected(t: Tier) -> Tier {
    if identity_available() { t } else { Tier::S3 }
}

/// A `file://` URI for a local path, as VS Code writes it (`file:///c%3A/…` on Windows).
fn file_uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    match s.as_bytes() {
        [d, b':', ..] if d.is_ascii_alphabetic() => {
            format!(
                "file:///{}%3A{}",
                (*d as char).to_ascii_lowercase(),
                &s[2..]
            )
        }
        _ => format!("file://{s}"),
    }
}

/// A JetBrains product cache folder as the IDE writes it.
fn jetbrains_cache(fx: &mut Fixture, dir: &str) {
    fx.file_of_size(format!("{dir}/caches/content.dat"), 4096)
        .unwrap()
        .file_of_size(format!("{dir}/index/stubs/stubs.dat"), 4096)
        .unwrap()
        .file(format!("{dir}/log/idea.log"), "started\n")
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// JetBrains

#[test]
fn jetbrains_caches_in_os_cache_folders_only() {
    let mut fx = Fixture::new().unwrap();
    // Linux, macOS and Windows cache locations.
    jetbrains_cache(&mut fx, "home/.cache/JetBrains/IntelliJIdea2023.3");
    jetbrains_cache(&mut fx, "home/.cache/JetBrains/IntelliJIdea2024.2");
    jetbrains_cache(&mut fx, "home/Library/Caches/JetBrains/PyCharm2024.1");
    jetbrains_cache(&mut fx, "home/AppData/Local/JetBrains/Rider2024.2");
    // Settings folders with the same shape are never candidates.
    jetbrains_cache(&mut fx, "home/.config/JetBrains/IntelliJIdea2024.2");
    jetbrains_cache(
        &mut fx,
        "home/Library/Application Support/JetBrains/PyCharm2024.1",
    );
    jetbrains_cache(&mut fx, "home/AppData/Roaming/JetBrains/Rider2024.2");
    // A cache-located folder that holds settings (`options/`) is excluded.
    jetbrains_cache(&mut fx, "home/.cache/JetBrains/GoLand2024.2");
    fx.file(
        "home/.cache/JetBrains/GoLand2024.2/options/editor.xml",
        "<application/>",
    )
    .unwrap();
    // Not a product folder name.
    jetbrains_cache(&mut fx, "home/.cache/JetBrains/Toolbox");
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    assert_eq!(
        found(&root),
        pairs(&[
            (
                "jetbrains.caches",
                "home/.cache/JetBrains/IntelliJIdea2023.3"
            ),
            (
                "jetbrains.caches",
                "home/.cache/JetBrains/IntelliJIdea2024.2"
            ),
            (
                "jetbrains.caches",
                "home/AppData/Local/JetBrains/Rider2024.2"
            ),
            (
                "jetbrains.caches",
                "home/Library/Caches/JetBrains/PyCharm2024.1"
            ),
        ])
    );
    let s = scan(&root, &[]);
    let old = s.at(&root, "home/.cache/JetBrains/IntelliJIdea2023.3");
    assert!(
        notes(old)
            .iter()
            .any(|n| n.contains("newer version") && n.contains("IntelliJIdea2024.2")),
        "{:?}",
        notes(old)
    );
    let new = s.at(&root, "home/.cache/JetBrains/IntelliJIdea2024.2");
    assert!(notes(new).iter().all(|n| !n.contains("newer version")));
    let mut cands = Vec::new();
    for a in &s.found {
        assert!(a.projects.is_empty());
        let c = classify_expect(a, &clean_caller(a), Tier::S1);
        if c.tier == Tier::S1 {
            assert_eq!(c.action, Action::Quarantine);
        }
        cands.push(c);
    }
    check(&fx, &plan_inputs(&cands));
}

// ---------------------------------------------------------------------------------------------
// Editor workspace storage

fn workspace_storage(fx: &mut Fixture, dir: &str, workspace_json: &str) {
    fx.file(format!("{dir}/workspace.json"), workspace_json)
        .unwrap()
        .file_of_size(format!("{dir}/state.vscdb"), 4096)
        .unwrap();
}

#[test]
fn workspace_storage_only_for_workspaces_that_no_longer_exist() {
    let mut fx = Fixture::new().unwrap();
    fx.file("home/projects/live/README.md", "live\n").unwrap();
    let live = fx.root().join("home/projects/live");
    let gone = fx.root().join("home/projects/deleted");
    let gone_ws = fx.root().join("home/projects/old.code-workspace");
    let ws = "home/.config/Code/User/workspaceStorage";
    let h = |n: u8| format!("{ws}/{}", format!("{n:x}").repeat(32));
    workspace_storage(
        &mut fx,
        &h(1),
        &format!("{{\"folder\":\"{}\"}}", file_uri(&gone)),
    );
    workspace_storage(
        &mut fx,
        &h(2),
        &format!("{{\"workspace\":\"{}\"}}", file_uri(&gone_ws)),
    );
    // In use (exists), remote, malformed, missing key: left alone.
    workspace_storage(
        &mut fx,
        &h(3),
        &format!("{{\"folder\":\"{}\"}}", file_uri(&live)),
    );
    workspace_storage(
        &mut fx,
        &h(4),
        "{\"folder\":\"vscode-remote://ssh-remote%2Bbox/home/me/app\"}",
    );
    workspace_storage(&mut fx, &h(5), "{not json");
    workspace_storage(&mut fx, &h(6), "{}");
    // Cursor's data folder too; an unknown editor folder never.
    let cursor = format!(
        "home/.config/Cursor/User/workspaceStorage/{}",
        "a".repeat(32)
    );
    workspace_storage(
        &mut fx,
        &cursor,
        &format!("{{\"folder\":\"{}\"}}", file_uri(&gone)),
    );
    workspace_storage(
        &mut fx,
        &format!(
            "home/.config/SomeApp/User/workspaceStorage/{}",
            "b".repeat(32)
        ),
        &format!("{{\"folder\":\"{}\"}}", file_uri(&gone)),
    );
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    assert_eq!(
        found(&root),
        pairs(&[
            ("vscode.workspace-storage", &cursor),
            ("vscode.workspace-storage", &h(1)),
            ("vscode.workspace-storage", &h(2)),
        ])
    );
    let s = scan(&root, &[]);
    let mut cands = Vec::new();
    for a in &s.found {
        assert!(
            notes(a).iter().any(|n| n.contains("no longer exists")),
            "{:?}",
            notes(a)
        );
        let c = a.clone().into_candidate(&clean_caller(a));
        assert!(c.is_consistent());
        assert_eq!(c.tier, expected(Tier::S2), "S2: may hold extension data");
        if c.tier == Tier::S2 {
            assert_eq!(c.action, Action::Quarantine);
        }
        cands.push(c);
    }
    check(&fx, &plan_inputs(&cands));
}

// ---------------------------------------------------------------------------------------------
// Obsolete extension versions

fn extension(fx: &mut Fixture, dir: &str) {
    fx.file(format!("{dir}/package.json"), "{\"name\":\"x\"}")
        .unwrap()
        .file_of_size(format!("{dir}/dist/extension.js"), 4096)
        .unwrap();
}

#[test]
fn obsolete_extensions_need_the_editors_own_list_and_no_reference() {
    let mut fx = Fixture::new().unwrap();
    let ext = "home/.vscode/extensions";
    for v in [
        "rust-lang.rust-analyzer-0.3.2000-linux-x64",
        "rust-lang.rust-analyzer-0.3.2100-linux-x64",
        "ms-python.python-2024.1.0",
        "esbenp.prettier-vscode-10.0.0",
    ] {
        extension(&mut fx, &format!("{ext}/{v}"));
    }
    // The editor marked two versions obsolete; one of them is still referenced (e.g. by
    // another profile's install record) and must stay.
    fx.file(
        format!("{ext}/.obsolete"),
        "{\"rust-lang.rust-analyzer-0.3.2000-linux-x64\":true,\"ms-python.python-2024.1.0\":true}",
    )
    .unwrap()
    .file(
        format!("{ext}/extensions.json"),
        "[{\"identifier\":{\"id\":\"rust-lang.rust-analyzer\"},\"version\":\"0.3.2100\",\"relativeLocation\":\"rust-lang.rust-analyzer-0.3.2100-linux-x64\"},\
          {\"identifier\":{\"id\":\"ms-python.python\"},\"version\":\"2024.1.0\",\"location\":{\"path\":\"/home/me/.vscode/extensions/ms-python.python-2024.1.0\"}},\
          {\"identifier\":{\"id\":\"esbenp.prettier-vscode\"},\"version\":\"10.0.0\",\"relativeLocation\":\"esbenp.prettier-vscode-10.0.0\"}]",
    )
    .unwrap();
    // No `extensions.json`: the install records are unknown, so nothing is a candidate.
    let cur = "home/.cursor/extensions";
    extension(&mut fx, &format!("{cur}/a.b-1.0.0"));
    fx.file(format!("{cur}/.obsolete"), "{\"a.b-1.0.0\":true}")
        .unwrap();
    // A folder in an unknown editor home with the same files.
    let other = "home/.other/extensions";
    extension(&mut fx, &format!("{other}/a.b-1.0.0"));
    fx.file(format!("{other}/.obsolete"), "{\"a.b-1.0.0\":true}")
        .unwrap()
        .file(format!("{other}/extensions.json"), "[]")
        .unwrap();
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let old = format!("{ext}/rust-lang.rust-analyzer-0.3.2000-linux-x64");
    assert_eq!(found(&root), pairs(&[("vscode.obsolete-extension", &old)]));
    let s = scan(&root, &[]);
    let a = s.at(&root, &old);
    assert!(notes(a).iter().any(|n| n.contains("obsolete")));
    let c = classify_expect(a, &clean_caller(a), Tier::S1);
    check(&fx, &plan_inputs(&[c]));
}
