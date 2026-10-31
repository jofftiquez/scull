//! Rust detector fixtures (P1-DET-02): FX-CLS-01, -02, -04, -09, plus workspaces and safety
//! edges. Every test builds its tree with `scull-fixtures`, runs detection read-only through
//! `scull-platform`, and checks the universal invariants (I-2 canary, I-3 tree unchanged, I-1
//! on the would-be plan).

#![allow(clippy::unwrap_used)]

mod common;

use common::{
    StdReader, age_tree, check, classify_expect, clean_caller, logical_bytes, plan_inputs, rel,
    scan,
};
use scull_core::{Action, Evidence, ReasonCode, Tier};
use scull_detectors::cargo::{SHARED_TARGET_RULE_ID, SharedTargetDirs};
use scull_detectors::rules::RuleSet;
use scull_detectors::{DetectContext, Detector, RuleDetector};
use scull_fixtures::{CACHEDIR_TAG, Fixture, NodeLock, ago, days_ago};
use scull_platform::Native;
use scull_safety::Regen;
use std::path::Path;
use std::time::Duration;

fn evidence_paths(ev: &[Evidence], root: &Path) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            Evidence::Marker { path } => Some(format!("marker {}", rel(root, &path.to_path_buf()))),
            Evidence::Lockfile { path } => {
                Some(format!("lockfile {}", rel(root, &path.to_path_buf())))
            }
            Evidence::KnownArtifact { artifact } => Some(format!("known {artifact}")),
            _ => None,
        })
        .collect()
}

fn project_names(a: &scull_detectors::DetectedArtifact) -> Vec<&str> {
    a.projects.iter().map(|p| p.name.as_str()).collect()
}

/// A shared target directory at `rel` as Cargo lays it out, with binaries for `bins`.
fn shared_target(fx: &mut Fixture, rel_path: &str, bins: &[&str]) {
    fx.file(format!("{rel_path}/CACHEDIR.TAG"), CACHEDIR_TAG)
        .unwrap()
        .file(
            format!("{rel_path}/.rustc_info.json"),
            r#"{"rustc_fingerprint":0,"outputs":{}}"#,
        )
        .unwrap();
    for b in bins {
        fx.file_of_size(format!("{rel_path}/debug/{b}"), 2048)
            .unwrap()
            .file(format!("{rel_path}/debug/{b}.d"), "deps\n")
            .unwrap();
    }
}

/// A Cargo config redirecting the target dir. `value` is written as a TOML literal string, so
/// Windows paths need no escaping.
fn target_dir_config(fx: &mut Fixture, project: &str, value: &str) {
    fx.file(
        format!("{project}/.cargo/config.toml"),
        format!("[build]\ntarget-dir = '{value}'\n"),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-01

#[test]
fn fx_cls_01_cargo_target_with_lockfile_idle_is_s0() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 4096)
        .unwrap();
    age_tree(&mut fx, "app/target", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["app/target"]);
    let a = s.at(&root, "app/target");
    assert_eq!(a.rule.id, "rust.target");
    assert_eq!(project_names(a), ["app"]);
    assert_eq!(
        evidence_paths(&a.evidence, &root),
        [
            "marker app/Cargo.toml",
            "marker app/target/CACHEDIR.TAG",
            "lockfile app/Cargo.lock",
            "known Cargo build output",
        ]
    );
    assert!(a.signals.marker_present && a.signals.lockfile_present);
    assert_eq!(a.signals.regen, Regen::Local);
    assert!(
        a.activity_lock_files
            .contains(&root.join("app/target/debug/.cargo-lock"))
    );
    assert!(
        a.regeneration
            .instructions
            .as_deref()
            .unwrap()
            .contains("cargo build")
    );

    let caller = clean_caller(a);
    assert_eq!(caller.recent_activity, scull_safety::Tri::No, "idle 3 days");
    let c = classify_expect(a, &caller, Tier::S0);
    if c.tier == Tier::S0 {
        assert_eq!(c.action, Action::Delete);
        assert!(c.reasons.iter().any(|r| r.code == ReasonCode::Regenerable));
    }
    assert_eq!(c.project.as_ref().unwrap().name, "app");
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn fx_cls_01_cargo_target_without_lockfile_is_s2() {
    let mut fx = Fixture::new().unwrap();
    fx.file(
        "app/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap()
    .cargo_target("app", "app", 1024)
    .unwrap();
    age_tree(&mut fx, "app/target", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    let a = s.at(&root, "app/target");
    assert!(!a.signals.lockfile_present);
    let c = classify_expect(a, &clean_caller(a), Tier::S2);
    assert!(c.reasons.iter().any(|r| r.code == ReasonCode::NoLockfile));
    assert_ne!(c.action, Action::Delete);
    check(&fx, &plan_inputs(&[c]));
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-02

#[test]
fn fx_cls_02_target_with_photos_and_no_cargo_toml_is_not_a_candidate() {
    let mut fx = Fixture::new().unwrap();
    // A folder named target/ full of photos; no Cargo.toml anywhere above.
    fx.file_of_size("Pictures/target/IMG_0001.jpg", 3000)
        .unwrap()
        .file_of_size("Pictures/target/IMG_0002.jpg", 3000)
        .unwrap()
        .file("Pictures/notes.txt", "holiday")
        .unwrap();
    // Cargo.toml exists, but not beside target/ (it's two levels up): wrong location.
    fx.cargo_project("work", "work")
        .unwrap()
        .file_of_size("work/assets/target/IMG_0003.jpg", 3000)
        .unwrap();
    // Cargo.toml beside it, but Cargo didn't create it (no CACHEDIR.TAG): user data.
    fx.cargo_project("shop", "shop")
        .unwrap()
        .file_of_size("shop/target/IMG_0004.jpg", 3000)
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert!(s.found.is_empty(), "found {:?}", s.rel_paths(&root));
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn fx_cls_02_symlink_named_target_is_never_a_candidate() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app").unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    let made = fx.symlink("app/target", &canary).unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert!(s.found.is_empty());
    if made.is_created() {
        // Asked directly, the detector refuses the link too (lstat: not a directory).
        let rules = RuleSet::bundled().unwrap();
        let d = RuleDetector::new(rules.get("rust.target").unwrap().clone());
        let p = Native::new();
        assert!(
            d.detect(&root.join("app/target"), &p, &DetectContext::new())
                .is_none()
        );
        assert!(
            !s.visited
                .iter()
                .any(|v| v.starts_with(root.join("app/target")))
        );
    }
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-04

#[test]
fn fx_cls_04_shared_cargo_target_dir_is_one_candidate_for_both_projects() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("a", "a")
        .unwrap()
        .cargo_project("b", "b")
        .unwrap();
    shared_target(&mut fx, "build-cache", &["a", "b"]);
    // a: relative target-dir; b: absolute.
    target_dir_config(&mut fx, "a", "../build-cache");
    let abs = fx.root().join("build-cache");
    target_dir_config(&mut fx, "b", abs.to_str().unwrap());
    age_tree(&mut fx, "build-cache", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["build-cache"], "one candidate");
    let a = s.at(&root, "build-cache");
    assert_eq!(a.rule.id, SHARED_TARGET_RULE_ID);
    assert_eq!(project_names(a), ["a", "b"], "attributed to both projects");
    let ev = evidence_paths(&a.evidence, &root);
    for e in [
        "marker build-cache/CACHEDIR.TAG",
        "marker a/Cargo.toml",
        "lockfile a/Cargo.lock",
        "marker b/Cargo.toml",
        "lockfile b/Cargo.lock",
    ] {
        assert!(ev.contains(&e.to_string()), "missing {e} in {ev:?}");
    }
    assert!(a.evidence.iter().any(
        |e| matches!(e, Evidence::Note { text } if text.contains("shared by 2 project(s): a, b"))
    ));

    // Bytes counted once: the artifacts' bytes equal the directory's own, measured
    // independently, and nothing else was attributed.
    let total: u64 = s.found.iter().map(|a| logical_bytes(&a.path)).sum();
    assert_eq!(total, logical_bytes(&root.join("build-cache")));
    assert!(total >= 2 * 2048);

    // Two projects must rebuild: costly, so S1 (never S0) even when idle with lockfiles.
    assert_eq!(a.signals.regen, Regen::NetworkOrLong);
    let c = classify_expect(a, &clean_caller(a), Tier::S1);
    assert_eq!(c.project.as_ref().unwrap().name, "a");
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn fx_cls_04_cargo_target_dir_from_caller_without_projects_is_s2() {
    let mut fx = Fixture::new().unwrap();
    shared_target(&mut fx, "cargo-target", &["tool"]);
    age_tree(&mut fx, "cargo-target", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[(root.join("cargo-target"), "CARGO_TARGET_DIR")]);
    assert_eq!(s.rel_paths(&root), ["cargo-target"]);
    let a = s.at(&root, "cargo-target");
    assert!(a.projects.is_empty());
    assert!(
        a.evidence
            .iter()
            .any(|e| matches!(e, Evidence::Note { text } if text.contains("CARGO_TARGET_DIR")))
    );
    // No project known to use it: nothing to reproduce from.
    let c = classify_expect(a, &clean_caller(a), Tier::S2);
    assert!(c.reasons.iter().any(|r| r.code == ReasonCode::NoLockfile));
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn fx_cls_04_project_target_used_by_another_project_is_merged_not_duplicated() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("a", "a")
        .unwrap()
        .cargo_target("a", "a", 1024)
        .unwrap()
        .cargo_project("b", "b")
        .unwrap();
    target_dir_config(&mut fx, "b", "../a/target");
    // Same directory named twice (relative and via CARGO_TARGET_DIR) still yields one item.
    age_tree(&mut fx, "a/target", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[(root.join("a/./target"), "CARGO_TARGET_DIR")]);
    assert_eq!(s.rel_paths(&root), ["a/target"]);
    let a = s.at(&root, "a/target");
    assert_eq!(a.rule.id, SHARED_TARGET_RULE_ID);
    assert_eq!(project_names(a), ["a", "b"]);
    let c = classify_expect(a, &clean_caller(a), Tier::S1);
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn fx_cls_04_target_dir_naming_the_project_itself_keeps_the_local_item() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("a", "a")
        .unwrap()
        .cargo_target("a", "a", 1024)
        .unwrap();
    target_dir_config(&mut fx, "a", "target"); // same as the default
    age_tree(&mut fx, "a/target", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["a/target"]);
    let a = s.at(&root, "a/target");
    assert_eq!(a.rule.id, "rust.target");
    let c = classify_expect(a, &clean_caller(a), Tier::S0);
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn fx_cls_04_declared_dirs_that_hold_projects_or_data_are_never_reported() {
    let mut fx = Fixture::new().unwrap();
    // target-dir pointing at the project itself, and at a parent that contains the project.
    fx.cargo_project("ws/app", "app").unwrap();
    fx.file("ws/CACHEDIR.TAG", CACHEDIR_TAG).unwrap();
    target_dir_config(&mut fx, "ws/app", "..");
    fx.cargo_project("solo", "solo").unwrap();
    fx.file("solo/CACHEDIR.TAG", CACHEDIR_TAG).unwrap();
    target_dir_config(&mut fx, "solo", ".");
    // An explicit path to a repository that happens to carry a CACHEDIR.TAG.
    shared_target(&mut fx, "repo", &["x"]);
    fx.dir("repo/.git").unwrap();
    // An explicit path to a folder Cargo never touched (no CACHEDIR.TAG).
    fx.file_of_size("photos/IMG_0001.jpg", 100).unwrap();
    // An explicit path that is a symlink to a real target dir: links are never followed.
    shared_target(&mut fx, "real-cache", &["y"]);
    let link = fx.symlink("link-cache", "real-cache").unwrap();
    // A config that isn't valid TOML: ignored, not guessed.
    fx.cargo_project("broken", "broken").unwrap();
    fx.file(
        "broken/.cargo/config.toml",
        "[build\ntarget-dir = '../real-cache'\n",
    )
    .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let mut shared = SharedTargetDirs::new();
    assert!(!shared.add_explicit("relative/dir", "CARGO_TARGET_DIR"));

    let s = scan(
        &root,
        &[
            (root.join("repo"), "CARGO_TARGET_DIR"),
            (root.join("photos"), "CARGO_TARGET_DIR"),
            (root.join("link-cache"), "CARGO_TARGET_DIR"),
        ],
    );
    assert!(s.found.is_empty(), "found {:?}", s.rel_paths(&root));
    if link.is_created() {
        assert!(
            !s.visited
                .iter()
                .any(|v| v.starts_with(root.join("link-cache")))
        );
    }
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn fx_cls_04_without_a_config_reader_nothing_is_read() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("a", "a").unwrap();
    shared_target(&mut fx, "build-cache", &["a"]);
    target_dir_config(&mut fx, "a", "../build-cache");
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let p = Native::new();
    let mut shared = SharedTargetDirs::new();
    assert_eq!(
        shared.add_project(&root.join("a"), &p, &DetectContext::new()),
        None
    );
    let reader = StdReader;
    let ctx = DetectContext::new().with_config_reader(&reader);
    assert_eq!(
        shared.add_project(&root.join("a"), &p, &ctx),
        Some(root.join("build-cache"))
    );
    assert!(shared.contains(&root.join("a/../build-cache")));
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// Workspaces, nesting, links.

#[test]
fn workspace_members_share_the_root_target_attributed_once() {
    let mut fx = Fixture::new().unwrap();
    fx.file(
        "ws/Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    )
    .unwrap()
    .file("ws/Cargo.lock", "version = 4\n")
    .unwrap();
    for m in ["core", "cli", "util"] {
        fx.file(
            format!("ws/crates/{m}/Cargo.toml"),
            format!("[package]\nname = \"{m}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap()
        .file(format!("ws/crates/{m}/src/lib.rs"), "")
        .unwrap();
    }
    fx.cargo_target("ws", "cli", 2048).unwrap();
    age_tree(&mut fx, "ws/target", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["ws/target"]);
    let a = s.at(&root, "ws/target");
    assert_eq!(
        project_names(a),
        ["ws"],
        "attributed once, to the workspace root"
    );
    let c = classify_expect(a, &clean_caller(a), Tier::S0);
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn crate_inside_node_modules_is_part_of_that_candidate() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["napi-thing"])
        .unwrap()
        .cargo_project("web/node_modules/napi-thing", "napi-thing")
        .unwrap()
        .cargo_target("web/node_modules/napi-thing", "napi-thing", 512)
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let rules = RuleSet::bundled().unwrap();
    let d = RuleDetector::new(rules.get("rust.target").unwrap().clone());
    let dir = root.join("web/node_modules/napi-thing/target");
    assert!(
        d.detect(&dir, &Native::new(), &DetectContext::new())
            .is_none()
    );
    let s = scan(&root, &[]);
    assert!(!s.rel_paths(&root).iter().any(|p| p.ends_with("/target")));
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn symlink_inside_target_is_flagged_and_not_followed() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 1024)
        .unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    let made = fx.symlink("app/target/escape", &canary).unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    let a = s.at(&root, "app/target");
    if made.is_created() {
        assert_eq!(a.contains_links, Some(true));
        let c = a.clone().into_candidate(&clean_caller(a));
        assert_eq!(c.flags.symlink, Some(true));
    }
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-09

#[test]
fn fx_cls_09_target_with_file_modified_an_hour_ago_is_bumped_to_s1() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 4096)
        .unwrap();
    age_tree(&mut fx, "app/target", days_ago(3));
    fx.set_mtime("app/target/debug/app", ago(Duration::from_secs(3600)))
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    let a = s.at(&root, "app/target");
    let caller = clean_caller(a);
    assert_eq!(caller.recent_activity, scull_safety::Tri::Yes);
    let c = classify_expect(a, &caller, Tier::S1);
    assert!(
        c.reasons
            .iter()
            .any(|r| r.code == ReasonCode::RecentActivity)
    );
    check(&fx, &plan_inputs(&[c]));
}
