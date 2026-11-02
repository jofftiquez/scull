//! Walker unit tests that need its private tuning: a tiny descriptor budget (re-open through
//! `..`), a tiny depth limit, and a simulated mount point (no privileges needed). All in
//! `scull-fixtures` trees with a canary, invariants checked.

use super::{Tuning, remove_tree};
use crate::remove::{Anomaly, RemoveOutcome, RemoveReport, RemoveRequest, SkipReason, WalkEvent};
use crate::{Native, Platform};
use scull_core::FileIdentity;
use scull_fixtures::{Fixture, InvariantInputs};
use std::ffi::CString;
use std::fs;
use std::path::Path;

fn id(path: &Path) -> FileIdentity {
    Native.metadata(path).unwrap().identity.unwrap()
}

fn walk(
    fx: &Fixture,
    rel: &str,
    tuning: &Tuning,
    observer: Option<&mut dyn FnMut(&WalkEvent<'_>)>,
) -> RemoveReport {
    let req = RemoveRequest {
        anchor: fx.root(),
        anchor_identity: Some(id(fx.root())),
        relative: Path::new(rel),
        expected: id(&fx.root().join(rel)),
        expected_kind: None,
    };
    remove_tree(&req, observer, tuning).unwrap()
}

fn check(fx: &Fixture, allowed: &[&str]) {
    let mut inputs = InvariantInputs::new();
    for a in allowed {
        inputs = inputs.allow(*a);
    }
    let report = fx.check_invariants(&inputs);
    println!("{report}");
    report.assert_ok();
}

/// A tree 40 levels deep with files at every level and a canary symlink near the bottom.
fn deep_tree(fx: &mut Fixture) -> usize {
    let mut rel = String::from("cand");
    for i in 0..40 {
        fx.file(format!("{rel}/f{i}.txt"), b"x").unwrap();
        fx.file(format!("{rel}/side{i}/g.txt"), b"y").unwrap();
        rel.push_str(&format!("/d{i}"));
    }
    fx.file(format!("{rel}/leaf.txt"), b"leaf").unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    assert!(
        fx.symlink(format!("{rel}/escape"), &canary)
            .unwrap()
            .is_created()
    );
    40
}

#[test]
fn tiny_fd_budget_reopens_through_dotdot_and_completes() {
    for budget in [1, 2, 5] {
        let mut fx = Fixture::new().unwrap();
        let depth = deep_tree(&mut fx);
        fx.checkpoint().unwrap();
        let tuning = Tuning {
            fd_budget: budget,
            ..Tuning::default()
        };
        let r = walk(&fx, "cand", &tuning, None);
        assert_eq!(
            r.outcome,
            RemoveOutcome::Complete,
            "budget {budget}: {r:#?}"
        );
        assert_eq!(r.removed.dirs as usize, 1 + depth * 2, "budget {budget}");
        assert_eq!(r.removed.symlinks, 1);
        assert!(fs::read(fx.canary().symlink_target_dir.join("keep.txt")).is_ok());
        check(&fx, &["cand"]);
    }
}

/// With handles closed for the budget, a directory moved while the walk is below it is caught
/// when climbing back through `..`: the re-opened parent is not the recorded one.
#[test]
fn moved_ancestor_is_caught_when_reopening_through_dotdot() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/a/b/c/file.txt", b"x").unwrap();
    fx.checkpoint().unwrap();
    let tuning = Tuning {
        fd_budget: 1,
        ..Tuning::default()
    };
    let root = fx.root().to_path_buf();
    let outside = fx.base().join("moved-a");
    let mut obs = |ev: &WalkEvent<'_>| {
        if let WalkEvent::DirOpened { relative, .. } = ev
            && *relative == Path::new("a/b/c")
        {
            fs::rename(root.join("cand/a"), &outside).unwrap();
        }
    };
    let r = walk(&fx, "cand", &tuning, Some(&mut obs));
    assert_eq!(r.outcome, RemoveOutcome::Aborted, "{r:#?}");
    assert_eq!(r.aborted.as_ref().unwrap().anomaly, Anomaly::DirectoryMoved);
    check(&fx, &["cand"]);
}

#[test]
fn simulated_mount_point_is_never_crossed() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/a.txt", b"a")
        .unwrap()
        .file("cand/mnt/mounted.txt", b"other device")
        .unwrap()
        .file("cand/zz/later.txt", b"z")
        .unwrap();
    fx.checkpoint().unwrap();
    let tuning = Tuning {
        foreign_device_name: Some(CString::new("mnt").unwrap()),
        ..Tuning::default()
    };
    let r = walk(&fx, "cand", &tuning, None);
    assert_eq!(r.outcome, RemoveOutcome::Aborted, "{r:#?}");
    let aborted = r.aborted.as_ref().unwrap();
    assert_eq!(aborted.relative, Path::new("mnt"));
    assert!(matches!(aborted.anomaly, Anomaly::MountCrossing { .. }));
    assert_eq!(
        fs::read(fx.root().join("cand/mnt/mounted.txt")).unwrap(),
        b"other device",
        "nothing below the mount point is touched"
    );
    assert!(fx.root().join("cand").exists(), "the candidate is kept");
    check(&fx, &["cand"]);
}

/// A bind mount of the same filesystem has the same `st_dev`; on Linux the walker compares
/// `statx` mount IDs too (simulated here: a different mount ID for the entry named `mnt`).
#[cfg(target_os = "linux")]
#[test]
fn simulated_bind_mount_on_the_same_device_is_never_crossed() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/a.txt", b"a")
        .unwrap()
        .file("cand/mnt/bound.txt", b"same device, other mount")
        .unwrap();
    fx.checkpoint().unwrap();
    let tuning = Tuning {
        foreign_mount_name: Some(CString::new("mnt").unwrap()),
        ..Tuning::default()
    };
    let r = walk(&fx, "cand", &tuning, None);
    if !r.mount_ids_checked {
        println!("SKIP: statx mount IDs unavailable on this kernel; st_dev only");
        check(&fx, &["cand"]);
        return;
    }
    assert_eq!(r.outcome, RemoveOutcome::Aborted, "{r:#?}");
    let aborted = r.aborted.as_ref().unwrap();
    assert_eq!(aborted.relative, Path::new("mnt"));
    assert!(
        matches!(aborted.anomaly, Anomaly::MountIdCrossing { .. }),
        "{aborted:?}"
    );
    assert!(fx.root().join("cand/mnt/bound.txt").exists());
    check(&fx, &["cand"]);
}

/// Mount IDs are compared for the target against its parent too.
#[cfg(target_os = "linux")]
#[test]
fn simulated_bind_mount_as_the_target_is_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("mnt/bound.txt", b"x")
        .unwrap()
        .checkpoint()
        .unwrap();
    let tuning = Tuning {
        foreign_mount_name: Some(CString::new("mnt").unwrap()),
        ..Tuning::default()
    };
    let req = RemoveRequest {
        anchor: fx.root(),
        anchor_identity: Some(id(fx.root())),
        relative: Path::new("mnt"),
        expected: id(&fx.root().join("mnt")),
        expected_kind: None,
    };
    if !super::mount_ids_supported() {
        println!("SKIP: statx mount IDs unavailable on this kernel; st_dev only");
        check(&fx, &[]);
        return;
    }
    let e = remove_tree(&req, None, &tuning).unwrap_err();
    assert!(
        matches!(e, crate::remove::RemoveError::TargetIsMountPoint { .. }),
        "{e}"
    );
    assert!(fx.root().join("mnt/bound.txt").exists());
    check(&fx, &[]);
}

#[test]
fn simulated_mount_point_as_the_target_is_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("mnt/mounted.txt", b"other device")
        .unwrap()
        .checkpoint()
        .unwrap();
    let tuning = Tuning {
        foreign_device_name: Some(CString::new("mnt").unwrap()),
        ..Tuning::default()
    };
    let req = RemoveRequest {
        anchor: fx.root(),
        anchor_identity: Some(id(fx.root())),
        relative: Path::new("mnt"),
        expected: id(&fx.root().join("mnt")),
        expected_kind: None,
    };
    let e = remove_tree(&req, None, &tuning).unwrap_err();
    assert!(
        matches!(e, crate::remove::RemoveError::TargetIsMountPoint { .. }),
        "{e}"
    );
    assert!(fx.root().join("mnt/mounted.txt").exists());
    check(&fx, &[]);
}

#[test]
fn deeper_than_the_limit_is_kept_and_reported() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/l1/l2/l3/deep.txt", b"x")
        .unwrap()
        .file("cand/top.txt", b"t")
        .unwrap();
    fx.checkpoint().unwrap();
    let tuning = Tuning {
        max_depth: 2,
        ..Tuning::default()
    };
    let r = walk(&fx, "cand", &tuning, None);
    assert_eq!(r.outcome, RemoveOutcome::Incomplete, "{r:#?}");
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.skipped[0].reason, SkipReason::TooDeep);
    assert_eq!(r.skipped[0].relative, Path::new("l1/l2/l3"));
    assert!(fx.root().join("cand/l1/l2/l3/deep.txt").exists());
    assert!(!fx.root().join("cand/top.txt").exists());
    check(&fx, &["cand"]);
}

#[test]
fn kind_mismatch_and_bad_anchor_identity_are_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/x.txt", b"x").unwrap().checkpoint().unwrap();
    let cand_id = id(&fx.root().join("cand"));
    let base = RemoveRequest {
        anchor: fx.root(),
        anchor_identity: Some(id(fx.root())),
        relative: Path::new("cand"),
        expected: cand_id,
        expected_kind: Some(crate::EntryKind::File),
    };
    let e = remove_tree(&base, None, &Tuning::default()).unwrap_err();
    assert!(
        matches!(e, crate::remove::RemoveError::KindMismatch { .. }),
        "{e}"
    );
    let wrong_anchor = RemoveRequest {
        anchor_identity: Some(cand_id),
        expected_kind: None,
        ..base
    };
    let e = remove_tree(&wrong_anchor, None, &Tuning::default()).unwrap_err();
    assert!(
        matches!(e, crate::remove::RemoveError::AnchorChanged { .. }),
        "{e}"
    );
    let missing = RemoveRequest {
        relative: Path::new("nope"),
        expected_kind: None,
        ..base
    };
    let e = remove_tree(&missing, None, &Tuning::default()).unwrap_err();
    assert!(
        matches!(e, crate::remove::RemoveError::TargetMissing),
        "{e}"
    );
    assert!(fx.root().join("cand/x.txt").exists());
    check(&fx, &[]);
}
