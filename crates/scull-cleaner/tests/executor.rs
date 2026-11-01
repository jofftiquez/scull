//! Executor refusals on every OS (P2-EXEC-01): what the Executor checks before the disk is
//! touched. Each test builds a fixture with a canary and checks the invariants; nothing may
//! change (I-3 with no allowed paths).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use scull_cleaner::{DeleteTarget, Executor};
use scull_core::{Action, ErrorKind, Tier};
use scull_fixtures::Fixture;

fn tree(fx: &mut Fixture) {
    fx.file("cand/a.txt", b"a")
        .unwrap()
        .file("cand/sub/b.txt", b"b")
        .unwrap()
        .file("other/c.txt", b"c")
        .unwrap()
        .checkpoint()
        .unwrap();
}

/// D-3 / D-28 at execution: Delete is S0-only; S1/S2 deletes, S3 items and actions this build
/// does not execute (quarantine, Trash, `None`) are refused with `SafetyBlocked`.
#[test]
fn delete_is_refused_unless_s0() {
    let mut fx = Fixture::new().unwrap();
    tree(&mut fx);
    let path = fx.root().join("cand");
    for (tier, action) in [
        (Tier::S1, Action::Delete),
        (Tier::S2, Action::Delete),
        (Tier::S3, Action::Delete),
        (Tier::S3, Action::None),
        (Tier::S0, Action::None),
        (Tier::S1, Action::Quarantine),
        (Tier::S2, Action::Trash),
        (Tier::S0, Action::GitWorktreeRemove),
    ] {
        let item = plan_item(&path, tier, action.clone());
        let e = delete(&fx, &item, "cand").unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{tier} {action:?}: {e:?}");
        assert_eq!(e.item_id.as_ref(), Some(&item.item_id));
    }
    assert!(path.join("sub/b.txt").exists());
    check(&fx, &[], &[]);
}

/// The native path must be the planned item's: a different path (here a sibling) with the
/// item's identity does not hash to its `ItemId` → `PlanInvalidated`, nothing removed.
#[test]
fn a_path_that_is_not_the_items_is_refused() {
    let mut fx = Fixture::new().unwrap();
    tree(&mut fx);
    let item = s0_delete(&fx, "cand");
    let e = delete(&fx, &item, "other").unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    check(&fx, &[], &[]);
}

/// The item must be strictly below the anchor; the anchor (scan root) itself is never deleted;
/// relative paths are refused.
#[test]
fn item_outside_the_anchor_or_the_anchor_itself_is_refused() {
    let mut fx = Fixture::new().unwrap();
    tree(&mut fx);
    let item = s0_delete(&fx, "cand");
    let exec = Executor::new();
    let cand = fx.root().join("cand");
    let sub = cand.join("sub");
    let root_id = identity(fx.root());

    // Anchor below the item: the item is not inside it.
    let e = exec
        .delete(
            &item,
            &DeleteTarget {
                anchor: &sub,
                anchor_identity: None,
                path: &cand,
            },
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");

    // The anchor itself.
    let root_item = plan_item(fx.root(), Tier::S0, Action::Delete);
    let e = exec
        .delete(
            &root_item,
            &DeleteTarget {
                anchor: fx.root(),
                anchor_identity: Some(root_id),
                path: fx.root(),
            },
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");

    // Relative paths.
    let e = exec
        .delete(
            &item,
            &DeleteTarget {
                anchor: std::path::Path::new("."),
                anchor_identity: None,
                path: std::path::Path::new("./cand"),
            },
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");

    // An anchor whose identity is not the expected one.
    let e = exec
        .delete(
            &item,
            &DeleteTarget {
                anchor: fx.root(),
                anchor_identity: Some(identity(&cand)),
                path: &cand,
            },
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    assert!(cand.join("sub/b.txt").exists());
    check(&fx, &[], &[]);
}
