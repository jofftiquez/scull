//! Shared helpers for the Executor fixtures. Everything runs inside a `scull-fixtures` temp
//! directory with a canary beside the root, and every test checks the invariants.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use scull_cleaner::{DeleteReport, DeleteTarget, Executor, RemoveOutcome, WalkEvent};
use scull_core::{
    Action, Category, FileIdentity, FsPath, ItemId, PlanItem, RuleRef, ScullError, Tier,
};
use scull_fixtures::{Fixture, InvariantInputs, InvariantReport};
use scull_platform::{Native, Platform, native_path_bytes};
use std::path::{Path, PathBuf};

/// No-follow identity of `path`, as the scanner records it.
pub fn identity(path: &Path) -> FileIdentity {
    Native
        .metadata(path)
        .unwrap()
        .identity
        .expect("identity available")
}

/// A plan item for `path` as the planner would build it from a fresh candidate.
pub fn plan_item(path: &Path, tier: Tier, action: Action) -> PlanItem {
    let identity = identity(path);
    PlanItem {
        item_id: ItemId::derive(identity, &native_path_bytes(path)),
        identity,
        path: FsPath::from_path(path),
        category: Category::ProjectArtifact,
        rule: RuleRef {
            id: "test.executor".into(),
            version: 1,
        },
        tier,
        action,
        reclaimable_bytes: 0,
    }
}

/// An S0 delete item for `rel` (relative to the fixture root).
pub fn s0_delete(fx: &Fixture, rel: impl AsRef<Path>) -> PlanItem {
    plan_item(&fx.root().join(rel), Tier::S0, Action::Delete)
}

/// Deletes `item` (found at `rel` under the fixture root) with the fixture root as anchor.
pub fn delete(
    fx: &Fixture,
    item: &PlanItem,
    rel: impl AsRef<Path>,
) -> Result<DeleteReport, ScullError> {
    let path = fx.root().join(rel);
    Executor::new().delete(item, &target(fx, &path))
}

/// [`delete`] with an observer.
pub fn delete_observed(
    fx: &Fixture,
    item: &PlanItem,
    rel: impl AsRef<Path>,
    observer: &mut dyn FnMut(&WalkEvent<'_>),
) -> Result<DeleteReport, ScullError> {
    let path = fx.root().join(rel);
    Executor::new().delete_observed(item, &target(fx, &path), observer)
}

pub fn target<'a>(fx: &'a Fixture, path: &'a Path) -> DeleteTarget<'a> {
    DeleteTarget {
        anchor: fx.root(),
        anchor_identity: Some(identity(fx.root())),
        path,
    }
}

/// Invariants for a run that executed `items`, allowing changes at `allowed` only.
pub fn check(fx: &Fixture, items: &[&PlanItem], allowed: &[&str]) -> InvariantReport {
    check_with(fx, items, allowed.iter().map(PathBuf::from).collect())
}

/// [`check`] with arbitrary (e.g. non-UTF-8) allowed paths.
pub fn check_with(fx: &Fixture, items: &[&PlanItem], allowed: Vec<PathBuf>) -> InvariantReport {
    let mut inputs = InvariantInputs::new().plan(
        items
            .iter()
            .map(|i| (i.item_id.clone(), i.tier, i.action.clone())),
    );
    for a in allowed {
        inputs = inputs.allow(a);
    }
    let report = fx.check_invariants(&inputs);
    println!("{report}");
    report.assert_ok();
    report
}

/// Prints the fixture's root and canary (no-follow snapshots) for before/after evidence.
pub fn print_tree(fx: &Fixture, label: &str) {
    use scull_fixtures::{EntryKind, TreeSnapshot};
    println!("--- {label} ---");
    for (name, dir) in [("root", fx.root()), ("canary", fx.canary().dir.as_path())] {
        let snap = TreeSnapshot::capture(dir).unwrap();
        println!("  {name}/");
        for (rel, e) in snap.entries() {
            if rel.as_os_str().is_empty() {
                continue;
            }
            let shown = match &e.kind {
                EntryKind::Dir => format!("{}/", rel.display()),
                EntryKind::File => format!("{} ({} B)", rel.display(), e.size),
                EntryKind::Symlink { target } => {
                    format!("{} -> {}", rel.display(), target.display())
                }
                EntryKind::Other => format!("{} (special)", rel.display()),
            };
            println!("  {name}/{shown}");
        }
    }
}

/// Asserts the walk removed the whole target.
pub fn assert_complete(r: &DeleteReport) {
    assert_eq!(r.report.outcome, RemoveOutcome::Complete, "{:#?}", r.report);
    assert!(r.report.skipped.is_empty(), "{:#?}", r.report);
    assert!(r.report.aborted.is_none(), "{:#?}", r.report);
}

/// Canary files every symlink / hardlink fixture points at still have their content.
pub fn assert_canary_files(fx: &Fixture) {
    let c = fx.canary();
    assert_eq!(
        std::fs::read(c.symlink_target_dir.join("keep.txt")).unwrap(),
        b"symlink target content\n"
    );
    assert_eq!(
        std::fs::read(&c.symlink_target_file).unwrap(),
        b"file symlink / hardlink target\n"
    );
    assert_eq!(
        std::fs::read(&c.file).unwrap(),
        b"scull canary: must never change\n"
    );
}
