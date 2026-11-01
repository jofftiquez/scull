//! Executor fixtures on Unix (P2-EXEC-01): the fd-relative, no-follow removal walker driven
//! through [`Executor::delete`]. Every test builds its tree in a `scull-fixtures` temp
//! directory with a canary beside the root and checks the invariants (I-2 canary untouched,
//! I-3 only the plan item changed). Nothing here touches a path outside its own fixture.
//!
//! Fixture IDs (docs/FIXTURES.md): FX-FS-01, FX-FS-05, FX-FS-07, FX-FS-08, FX-FS-12, FX-FS-14,
//! FX-RACE-01, FX-RACE-02; FX-FS-04 (mount inside a candidate) where the host allows an
//! unprivileged mount namespace.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

mod common;

use common::*;
use scull_cleaner::{Anomaly, Executor, RemoveOutcome, SkipReason, WalkEvent};
use scull_core::{ErrorKind, PlanItem};
use scull_fixtures::Fixture;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// A small candidate: `cand/` with files, a nested directory and a sub-directory `sub/`.
fn small_tree(fx: &mut Fixture) {
    fx.file("cand/a.txt", b"a")
        .unwrap()
        .file_of_size("cand/b.bin", 8192)
        .unwrap()
        .file("cand/sub/c.txt", b"c")
        .unwrap()
        .file("cand/sub/inner/d.txt", b"d")
        .unwrap()
        .file("cand/sub/inner/e.txt", b"e")
        .unwrap()
        .file(
            "keep/outside.txt",
            b"sibling of the candidate, never touched",
        )
        .unwrap();
}

// ------------------------------------------------------------------------------------------
// FX-FS: links, hardlinks, special files, permissions, depth, names
// ------------------------------------------------------------------------------------------

/// FX-FS-01: symlinks inside a candidate pointing to the canary (directory and file, absolute
/// and relative) → the links are removed, the canary is intact (I-2).
#[test]
fn fx_fs_01_symlink_to_canary_inside_candidate_link_removed_canary_intact() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    let canary_dir = fx.canary().symlink_target_dir.clone();
    let canary_file = fx.canary().symlink_target_file.clone();
    assert!(fx.symlink("cand/escape", &canary_dir).unwrap().is_created());
    assert!(
        fx.symlink("cand/escape.txt", &canary_file)
            .unwrap()
            .is_created()
    );
    assert!(
        fx.symlink("cand/sub/up", "../../../canary")
            .unwrap()
            .is_created()
    );
    assert!(
        fx.symlink("cand/sub/dangling", "/nonexistent/scull")
            .unwrap()
            .is_created()
    );
    fx.checkpoint().unwrap();

    print_tree(&fx, "FX-FS-01 before");
    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    println!("report: {:?} {:?}", r.report.outcome, r.report.removed);
    print_tree(&fx, "FX-FS-01 after");
    assert_complete(&r);
    assert_eq!(r.report.removed.symlinks, 4, "{:#?}", r.report);
    assert_eq!(r.report.removed.files, 5, "{:#?}", r.report);
    assert_eq!(r.report.removed.dirs, 3, "{:#?}", r.report);
    assert!(!fx.root().join("cand").exists());
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// The item itself is a symlink to the canary: the link is removed, never its target.
#[test]
fn symlink_item_removes_only_the_link() {
    let mut fx = Fixture::new().unwrap();
    let canary_dir = fx.canary().symlink_target_dir.clone();
    assert!(fx.symlink("cand", &canary_dir).unwrap().is_created());
    fx.checkpoint().unwrap();

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.symlinks, 1);
    assert!(fs::symlink_metadata(fx.root().join("cand")).is_err());
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// A single-file item is unlinked; its bytes are reported.
#[test]
fn single_file_item_is_unlinked() {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("big.bin", 100_000)
        .unwrap()
        .checkpoint()
        .unwrap();
    let item = s0_delete(&fx, "big.bin");
    let r = delete(&fx, &item, "big.bin").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.files, 1);
    assert_eq!(r.report.removed.logical_bytes, 100_000);
    check(&fx, &[&item], &["big.bin"]);
}

/// FX-FS-05: a file hardlinked between the candidate and the canary → the candidate's link is
/// removed; the canary file keeps its content and its link.
#[test]
fn fx_fs_05_hardlink_to_canary_link_removed_canary_intact() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    let canary_file = fx.canary().symlink_target_file.clone();
    fx.hardlink("cand/sub/hl.txt", &canary_file).unwrap();
    fx.checkpoint().unwrap();
    let canary_id = identity(&canary_file);

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.files, 6);
    assert_eq!(
        identity(&canary_file),
        canary_id,
        "same inode, still linked"
    );
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// Runs `f` on another thread and fails if it takes longer than `secs` (a FIFO opened for
/// reading without a writer blocks forever: a hang means SAFE-18 was violated).
fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("timed out: a special file was opened (SAFE-18)")
}

/// FX-FS-14: a FIFO (no writer) and a Unix socket inside a candidate → unlinked, never opened,
/// no hang.
#[test]
fn fx_fs_14_fifo_and_socket_inside_candidate_unlinked_never_opened() {
    use std::os::unix::net::UnixListener;
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    let fifo = fx.root().join("cand/sub/pipe");
    let ok = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "mkfifo");
    let sock_path = fx.root().join("cand/s.sock");
    // sun_path is ~104 bytes; skip the socket (not the test) on very long temp paths.
    let _listener =
        (sock_path.as_os_str().len() < 100).then(|| UnixListener::bind(&sock_path).unwrap());
    let specials = 1 + u64::from(_listener.is_some());
    fx.checkpoint().unwrap();

    let item = s0_delete(&fx, "cand");
    let anchor = fx.root().to_path_buf();
    let anchor_id = identity(&anchor);
    let path = anchor.join("cand");
    let item2 = item.clone();
    let r = within(30, move || {
        Executor::new().delete(
            &item2,
            &scull_cleaner::DeleteTarget {
                anchor: &anchor,
                anchor_identity: Some(anchor_id),
                path: &path,
            },
        )
    })
    .unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.special, specials, "{:#?}", r.report);
    check(&fx, &[&item], &["cand"]);
}

/// FX-FS-07: a read-only file is deleted (permitted: its directory is writable); a
/// read-only directory's contents cannot be, so they are reported with a reason and kept, with
/// their ancestors; permissions are never changed (no escalation).
#[test]
fn fx_fs_07_read_only_entries_deleted_if_permitted_or_reported() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.file("cand/ro-file.txt", b"read-only file").unwrap();
    fx.file("cand/ro-dir/locked.txt", b"in a read-only dir")
        .unwrap();
    let ro_file = fx.root().join("cand/ro-file.txt");
    let ro_dir = fx.root().join("cand/ro-dir");
    fs::set_permissions(&ro_file, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o555)).unwrap();
    fx.checkpoint().unwrap();

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    let root_bypasses = scull_platform::is_elevated() == scull_platform::Elevation::Elevated;
    if root_bypasses {
        // root ignores permission bits: everything goes.
        assert_complete(&r);
    } else {
        assert_eq!(
            r.report.outcome,
            RemoveOutcome::Incomplete,
            "{:#?}",
            r.report
        );
        assert!(
            !ro_file.exists(),
            "a read-only file in a writable dir is deleted"
        );
        let kept: Vec<_> = r
            .report
            .skipped
            .iter()
            .map(|s| (s.relative.clone(), s.reason))
            .collect();
        assert_eq!(
            kept,
            vec![(
                PathBuf::from("ro-dir/locked.txt"),
                SkipReason::PermissionDenied
            )],
            "{:#?}",
            r.report
        );
        assert!(ro_dir.join("locked.txt").exists());
        let mode = fs::symlink_metadata(&ro_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o555, "the walker never changes permissions");
        // Everything else in the candidate is gone; the kept entry's ancestors remain.
        assert!(!fx.root().join("cand/sub").exists());
        assert!(!fx.root().join("cand/a.txt").exists());
        // The test (not the walker) restores the mode so the temp dir can be cleaned up.
        fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    check(&fx, &[&item], &["cand"]);
}

/// FX-FS-08: a permission-denied sub-folder (mode 000) inside a candidate → the item is
/// reported incomplete with the reason, the folder is kept untouched, no escalation; the rest
/// of the candidate is removed.
#[test]
fn fx_fs_08_permission_denied_subfolder_reported_not_escalated() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.file("cand/denied/secret.txt", b"unreadable").unwrap();
    let denied = fx.root().join("cand/denied");
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
    fx.checkpoint().unwrap();

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    if scull_platform::is_elevated() == scull_platform::Elevation::Elevated {
        assert_complete(&r);
    } else {
        assert_eq!(
            r.report.outcome,
            RemoveOutcome::Incomplete,
            "{:#?}",
            r.report
        );
        assert_eq!(r.report.skipped.len(), 1, "{:#?}", r.report);
        let kept = &r.report.skipped[0];
        assert_eq!(kept.relative, Path::new("denied"));
        assert_eq!(kept.reason, SkipReason::PermissionDenied);
        assert!(!kept.detail.is_empty());
        let mode = fs::symlink_metadata(&denied).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o000, "the walker never changes permissions");
        assert!(!fx.root().join("cand/sub").exists());
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(denied.join("secret.txt").exists());
    }
    check(&fx, &[&item], &["cand"]);
}

/// FX-FS-12 / deep trees: a 3000-level chain (path far beyond PATH_MAX) is removed
/// iteratively, fd-relative, within the descriptor budget (no recursion, no stack overflow).
#[test]
fn fx_fs_12_very_deep_tree_beyond_path_max_is_removed() {
    const DEPTH: usize = 3000;
    let mut fx = Fixture::new().unwrap();
    fx.dir("cand").unwrap().checkpoint().unwrap();
    // Built after the checkpoint (the snapshot walks by path and cannot reach that deep), and
    // with short paths only: grow the chain at the top by renames.
    let cand = fx.root().join("cand");
    let chain = cand.join("chain");
    let tmp = cand.join("p");
    fs::create_dir(&chain).unwrap();
    fs::write(chain.join("leaf.txt"), b"bottom").unwrap();
    for _ in 0..DEPTH {
        fs::create_dir(&tmp).unwrap();
        fs::rename(&chain, tmp.join("d")).unwrap();
        fs::rename(&tmp, &chain).unwrap();
    }

    let item = s0_delete(&fx, "cand");
    let r = within(120, {
        let anchor = fx.root().to_path_buf();
        let anchor_id = identity(&anchor);
        let item = item.clone();
        move || {
            let path = anchor.join("cand");
            Executor::new().delete(
                &item,
                &scull_cleaner::DeleteTarget {
                    anchor: &anchor,
                    anchor_identity: Some(anchor_id),
                    path: &path,
                },
            )
        }
    })
    .unwrap();
    assert_complete(&r);
    assert_eq!(
        r.report.removed.dirs,
        DEPTH as u64 + 2,
        "cand + chain + d x DEPTH"
    );
    assert_eq!(r.report.removed.files, 1);
    check(&fx, &[&item], &["cand"]);
}

/// Names that are not valid UTF-8 (Linux; macOS APFS refuses them, so the test notes the
/// skip): the candidate and its entries are removed by their exact bytes, and the item is
/// matched by native path bytes, never by its lossy display path.
#[test]
fn non_utf8_names_are_removed_by_their_bytes() {
    let mut fx = Fixture::new().unwrap();
    let cand = OsStr::from_bytes(b"cand-\xff\xfe");
    let inner = OsStr::from_bytes(b"inner-\x80.bin");
    if let Err(e) = fs::create_dir(fx.root().join(cand)) {
        eprintln!("SKIP non-UTF-8 names: this filesystem refuses them ({e})");
        fx.checkpoint().unwrap();
        check(&fx, &[], &[]);
        return;
    }
    fs::write(fx.root().join(cand).join(inner), b"x").unwrap();
    fs::create_dir(fx.root().join(cand).join(inner).with_extension("d")).unwrap();
    fx.file("keep.txt", b"keep").unwrap().checkpoint().unwrap();

    let item = s0_delete(&fx, Path::new(cand));
    assert!(item.path.is_lossy());
    let r = delete(&fx, &item, Path::new(cand)).unwrap();
    assert_complete(&r);
    assert!(fs::symlink_metadata(fx.root().join(cand)).is_err());
    check_with(&fx, &[&item], vec![PathBuf::from(cand)]);
}

// ------------------------------------------------------------------------------------------
// Refusals: nothing removed
// ------------------------------------------------------------------------------------------

/// FX-RACE-01: the candidate is replaced by a symlink to the canary between plan and execute
/// → refused (identity mismatch), nothing removed, canary intact.
#[test]
fn fx_race_01_candidate_replaced_by_symlink_is_refused() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    // Between plan and execute: move the real candidate away, put a link in its place.
    fs::rename(fx.root().join("cand"), fx.root().join("cand.real")).unwrap();
    symlink(&fx.canary().symlink_target_dir, fx.root().join("cand")).unwrap();

    let e = delete(&fx, &item, "cand").unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    assert!(
        fs::symlink_metadata(fx.root().join("cand"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fx.root().join("cand.real/sub/inner/d.txt").exists());
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand", "cand.real"]);
}

/// SAFE-05: the candidate directory is replaced by another directory (new inode) → refused,
/// nothing removed from either.
#[test]
fn identity_mismatch_at_the_root_is_refused_and_nothing_removed() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    fs::rename(fx.root().join("cand"), fx.root().join("cand.old")).unwrap();
    fs::create_dir(fx.root().join("cand")).unwrap();
    fs::write(fx.root().join("cand/new.txt"), b"not planned").unwrap();

    let e = delete(&fx, &item, "cand").unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    assert!(e.message.contains("different object"), "{e}");
    assert!(fx.root().join("cand/new.txt").exists());
    assert!(fx.root().join("cand.old/a.txt").exists());
    check(&fx, &[&item], &["cand", "cand.old"]);
}

/// SAFE-04: a component between the anchor and the item is a symlink → refused, never
/// followed, even though it points inside the root.
#[test]
fn link_in_the_path_below_the_anchor_is_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("real/cand/x.txt", b"x").unwrap();
    assert!(fx.symlink("via", "real").unwrap().is_created());
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "via/cand");
    let e = delete(&fx, &item, "via/cand").unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");
    assert!(fx.root().join("real/cand/x.txt").exists());
    check(&fx, &[&item], &[]);
}

// ------------------------------------------------------------------------------------------
// FX-RACE-02: swaps during the walk (deterministic hooks + a real race)
// ------------------------------------------------------------------------------------------

/// FX-RACE-02 (a): `cand/sub` is swapped for a symlink to the canary right after the
/// candidate is opened and before it is listed → the walk sees the link and removes the link
/// only; the moved directory (still inside the candidate) is removed; canary intact.
#[test]
fn fx_race_02_subfolder_swapped_for_symlink_before_listing_only_link_removed() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let canary = fx.canary().symlink_target_dir.clone();
    let mut swapped = false;
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirOpened { depth: 0, .. } = ev {
            fs::rename(root.join("cand/sub"), root.join("cand/sub.moved")).unwrap();
            symlink(&canary, root.join("cand/sub")).unwrap();
            swapped = true;
        }
    })
    .unwrap();
    assert!(swapped);
    assert_complete(&r);
    assert_eq!(r.report.removed.symlinks, 1);
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (b): `cand/sub` is swapped for a symlink to the canary between its no-follow
/// `stat` (a directory) and its `openat` → `O_NOFOLLOW` refuses the link, the walk aborts with
/// `KindChanged` and touches nothing more; canary intact.
#[test]
fn fx_race_02_subfolder_swapped_for_symlink_between_stat_and_open_aborts() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let canary = fx.canary().symlink_target_dir.clone();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::EntryChecked { relative, .. } = ev
            && *relative == Path::new("sub")
        {
            fs::rename(root.join("cand/sub"), root.join("cand/sub.moved")).unwrap();
            symlink(&canary, root.join("cand/sub")).unwrap();
        }
    })
    .unwrap();
    println!(
        "report: {:?}, aborted at {:?}",
        r.report.outcome, r.report.aborted
    );
    print_tree(&fx, "FX-RACE-02 (b) after the swap and the aborted walk");
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    let aborted = r.report.aborted.as_ref().unwrap();
    assert_eq!(aborted.relative, Path::new("sub"));
    assert!(
        matches!(aborted.anomaly, Anomaly::KindChanged { .. }),
        "{aborted:?}"
    );
    assert!(
        fs::symlink_metadata(root.join("cand/sub"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(root.join("cand/sub.moved/inner/d.txt").exists());
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (c): `cand/sub` is replaced by a *different real directory* between `stat` and
/// `openat` → the opened handle's inode differs, the walk aborts with `IdentityChanged`.
#[test]
fn fx_race_02_subfolder_replaced_by_other_directory_aborts_on_identity() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::EntryChecked { relative, .. } = ev
            && *relative == Path::new("sub")
        {
            fs::rename(root.join("cand/sub"), root.join("cand/sub.moved")).unwrap();
            fs::create_dir(root.join("cand/sub")).unwrap();
            fs::write(root.join("cand/sub/planted.txt"), b"not examined").unwrap();
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    let aborted = r.report.aborted.as_ref().unwrap();
    assert!(
        matches!(aborted.anomaly, Anomaly::IdentityChanged { .. }),
        "{aborted:?}"
    );
    assert!(
        root.join("cand/sub/planted.txt").exists(),
        "nothing touched after the anomaly"
    );
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (d): a nested directory (`cand/sub/inner`) is swapped for a symlink to the
/// canary between `stat` and `openat`, deep in the walk → abort; canary intact.
#[test]
fn fx_race_02_nested_folder_swapped_mid_walk_aborts() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let canary = fx.canary().dir.clone();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::EntryChecked { relative, .. } = ev
            && *relative == Path::new("sub/inner")
        {
            fs::rename(root.join("cand/sub/inner"), root.join("cand/inner.moved")).unwrap();
            symlink(&canary, root.join("cand/sub/inner")).unwrap();
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    assert!(matches!(
        r.report.aborted.as_ref().unwrap().anomaly,
        Anomaly::KindChanged { .. }
    ));
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (e): a directory being walked is moved out of the candidate → its contents
/// (candidate data when it was opened) are emptied, then the walker notices the directory is
/// no longer at its name and aborts (`DirectoryMoved`) instead of continuing.
#[test]
fn fx_race_02_directory_moved_out_during_walk_aborts() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let outside = fx.base().join("moved-out");
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirOpened { relative, .. } = ev
            && *relative == Path::new("sub/inner")
        {
            fs::rename(root.join("cand/sub/inner"), &outside).unwrap();
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    assert_eq!(
        r.report.aborted.as_ref().unwrap().anomaly,
        Anomaly::DirectoryMoved
    );
    assert!(
        outside.exists(),
        "the moved directory itself is never removed by name"
    );
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (real race): a thread keeps swapping `cand/a/b` between its real directory and a
/// symlink to the canary while the Executor deletes the candidate, over many rounds. Whatever
/// the interleaving (link unlinked, open refused, identity mismatch, directory moved), the
/// canary is never touched.
#[test]
fn fx_race_02_concurrent_symlink_swaps_never_reach_the_canary() {
    const ROUNDS: usize = 300;
    let mut fx = Fixture::new().unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let canary = fx.canary().symlink_target_dir.clone();
    let cand = root.join("cand");
    let mut tally = std::collections::BTreeMap::<String, usize>::new();
    let mut executed = Vec::new();

    for round in 0..ROUNDS {
        for d in ["a/b/x", "a/b/y", "a/c"] {
            fs::create_dir_all(cand.join(d)).unwrap();
        }
        for i in 0..8 {
            fs::write(cand.join(format!("a/b/x/f{i}")), b"x").unwrap();
            fs::write(cand.join(format!("a/b/f{i}")), b"b").unwrap();
            fs::write(cand.join(format!("a/c/f{i}")), b"c").unwrap();
        }
        let item = s0_delete(&fx, "cand");

        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let (stop, b, real, canary) = (
                stop.clone(),
                cand.join("a/b"),
                cand.join("a/b.real"),
                canary.clone(),
            );
            std::thread::spawn(move || {
                let mut swaps = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    if fs::rename(&b, &real).is_ok() {
                        if symlink(&canary, &b).is_ok() {
                            swaps += 1;
                            std::thread::yield_now();
                            // Remove our link (never its target) and put the directory back.
                            if fs::symlink_metadata(&b).is_ok_and(|m| m.file_type().is_symlink()) {
                                let _ = fs::remove_file(&b);
                            }
                        }
                        let _ = fs::rename(&real, &b);
                    }
                    std::thread::yield_now();
                }
                swaps
            })
        };
        let r = delete(&fx, &item, "cand");
        stop.store(true, Ordering::Relaxed);
        let _swaps = swapper.join().unwrap();
        let label = match &r {
            Ok(r) => match &r.report.aborted {
                Some(a) => format!("Aborted ({})", anomaly_name(&a.anomaly)),
                None => format!("{:?}", r.report.outcome),
            },
            Err(e) => format!("Refused ({:?})", e.kind),
        };
        *tally.entry(label).or_default() += 1;
        executed.push(item);

        // The canary's symlink target must still be whole after every round.
        assert_canary_files(&fx);

        // Clean up what the race left (the swapper is stopped): the same walker, no races.
        if fs::symlink_metadata(&cand).is_ok() {
            let again = s0_delete(&fx, "cand");
            let r = delete(&fx, &again, "cand").unwrap();
            assert_complete(&r);
        }
        assert!(fs::symlink_metadata(&cand).is_err(), "round {round}");
    }
    println!("FX-RACE-02 real race, {ROUNDS} rounds: {tally:?}");
    let executed: Vec<&PlanItem> = executed.iter().collect();
    check(&fx, &executed, &["cand"]);
}

fn anomaly_name(a: &Anomaly) -> &'static str {
    match a {
        Anomaly::IdentityChanged { .. } => "identity changed",
        Anomaly::KindChanged { .. } => "kind changed",
        Anomaly::MountCrossing { .. } => "mount crossing",
        Anomaly::DirectoryMoved => "directory moved",
        _ => "other",
    }
}

// ------------------------------------------------------------------------------------------
// FX-FS-04: a real mount inside the candidate (Linux, unprivileged user + mount namespace)
// ------------------------------------------------------------------------------------------

const IN_MOUNT_NS: &str = "SCULL_TEST_IN_MOUNT_NS";

/// FX-FS-04: a tmpfs mounted inside the candidate → the walk aborts at the mount point
/// (`MountCrossing`), the mounted content is untouched. Needs an unprivileged user + mount
/// namespace (`unshare -rm`); where the host forbids it (e.g. Ubuntu's AppArmor userns
/// restriction, macOS) the test prints why and passes without running. The same abort path
/// is covered deterministically on every OS by the walker's unit tests (simulated device).
#[test]
fn fx_fs_04_mount_inside_candidate_is_never_crossed() {
    if std::env::var_os(IN_MOUNT_NS).is_none() {
        if !cfg!(target_os = "linux") {
            eprintln!("SKIP FX-FS-04: needs a mount namespace (Linux) or a disk image (nightly)");
            return;
        }
        let probe = std::process::Command::new("unshare")
            .args(["--user", "--map-current-user", "--mount", "true"])
            .output();
        match probe {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                eprintln!(
                    "SKIP FX-FS-04: no unprivileged mount namespace here: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                return;
            }
            Err(e) => {
                eprintln!("SKIP FX-FS-04: `unshare` unavailable: {e}");
                return;
            }
        }
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("unshare")
            .args(["--user", "--map-current-user", "--mount", "--"])
            .arg(exe)
            .args([
                "--exact",
                "fx_fs_04_mount_inside_candidate_is_never_crossed",
                "--nocapture",
            ])
            .env(IN_MOUNT_NS, "1")
            .output()
            .unwrap();
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "child in the mount namespace failed");
        return;
    }

    // Inside the namespace: mounts here are private and vanish with the process.
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.dir("cand/mnt").unwrap();
    let mnt = fx.root().join("cand/mnt");
    let mounted = std::process::Command::new("mount")
        .args(["-t", "tmpfs", "scull-test"])
        .arg(&mnt)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !mounted {
        eprintln!("SKIP FX-FS-04: mounting tmpfs failed inside the namespace");
        fx.checkpoint().unwrap();
        check(&fx, &[], &[]);
        return;
    }
    fs::write(mnt.join("on-other-device.txt"), b"must survive").unwrap();
    fx.checkpoint().unwrap();

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    assert!(matches!(
        r.report.aborted.as_ref().unwrap().anomaly,
        Anomaly::MountCrossing { .. }
    ));
    assert_eq!(
        fs::read(mnt.join("on-other-device.txt")).unwrap(),
        b"must survive"
    );
    // The mount point itself as the item: refused before anything is removed.
    let at_mount = s0_delete(&fx, "cand/mnt");
    let e = delete(&fx, &at_mount, "cand/mnt").unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");
    assert_eq!(
        fs::read(mnt.join("on-other-device.txt")).unwrap(),
        b"must survive"
    );
    let _ = std::process::Command::new("umount").arg(&mnt).status();

    // A bind mount of the same filesystem (same st_dev): caught by the statx mount ID.
    fs::create_dir_all(fx.root().join("cand2/bound")).unwrap();
    fs::create_dir_all(fx.root().join("bind-src")).unwrap();
    fs::write(fx.root().join("bind-src/kept.txt"), b"bind source").unwrap();
    let bound = fx.root().join("cand2/bound");
    let bind_ok = std::process::Command::new("mount")
        .arg("--bind")
        .arg(fx.root().join("bind-src"))
        .arg(&bound)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let item2 = s0_delete(&fx, "cand2");
    if bind_ok {
        let r = delete(&fx, &item2, "cand2").unwrap();
        assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
        assert!(matches!(
            r.report.aborted.as_ref().unwrap().anomaly,
            Anomaly::MountIdCrossing { .. } | Anomaly::MountCrossing { .. }
        ));
        assert_eq!(
            fs::read(fx.root().join("bind-src/kept.txt")).unwrap(),
            b"bind source"
        );
        let _ = std::process::Command::new("umount").arg(&bound).status();
        println!("FX-FS-04 bind mount on the same filesystem: never crossed");
    } else {
        eprintln!("SKIP FX-FS-04 bind-mount half: `mount --bind` failed in the namespace");
    }
    println!("FX-FS-04 ran in a mount namespace: mount point never crossed");
    check(&fx, &[&item, &item2], &["cand", "cand2", "bind-src"]);
}

// ------------------------------------------------------------------------------------------
// Hardening from the safety review of #207: late moves, swapped entries, single-file targets
// ------------------------------------------------------------------------------------------

/// FX-RACE-02 (f): `cand/sub` is moved out of the candidate right after it was listed, and a
/// user directory appears at the moved location under one of its pending names → the walker
/// re-verifies the directory's place before removing anything in it and aborts
/// (`DirectoryMoved`): nothing outside the candidate is deleted.
#[test]
fn fx_race_02_directory_moved_after_listing_nothing_outside_deleted() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let moved = fx.base().join("user-area");
    let parked = fx.base().join("parked-inner");
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirListed { relative, .. } = ev
            && *relative == Path::new("sub")
        {
            fs::rename(root.join("cand/sub"), &moved).unwrap();
            // The user's own data now sits under the pending name `inner`.
            fs::rename(moved.join("inner"), &parked).unwrap();
            fs::create_dir(moved.join("inner")).unwrap();
            fs::write(moved.join("inner/precious.txt"), b"user data").unwrap();
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    assert_eq!(
        r.report.aborted.as_ref().unwrap().anomaly,
        Anomaly::DirectoryMoved
    );
    assert_eq!(
        fs::read(moved.join("inner/precious.txt")).unwrap(),
        b"user data"
    );
    assert!(
        moved.join("c.txt").exists(),
        "nothing in the moved directory is removed"
    );
    assert!(parked.join("d.txt").exists());
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// The periodic `..` check: a directory with many entries moved out mid-batch → the walker
/// notices within `VERIFY_EVERY` entries and aborts; the rest of the moved directory is kept.
#[test]
fn directory_moved_mid_batch_is_caught_within_the_verify_interval() {
    const FILES: usize = 300;
    let mut fx = Fixture::new().unwrap();
    for i in 0..FILES {
        fx.file(format!("cand/big/f{i:03}"), b"x").unwrap();
    }
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let moved = fx.base().join("moved-big");
    let mut seen = 0usize;
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::EntryChecked { relative, .. } = ev
            && relative.starts_with("big")
            && *relative != Path::new("big")
        {
            seen += 1;
            if seen == 3 {
                fs::rename(root.join("cand/big"), &moved).unwrap();
            }
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    assert_eq!(
        r.report.aborted.as_ref().unwrap().anomaly,
        Anomaly::DirectoryMoved
    );
    let left = fs::read_dir(&moved).unwrap().count();
    let removed_after_move = FILES - 2 - left;
    println!("removed after the move: {removed_after_move}");
    assert!(
        removed_after_move <= scull_cleaner::VERIFY_EVERY,
        "{removed_after_move} entries removed after the move"
    );
    check(&fx, &[&item], &["cand"]);
}

/// An entry listed as a file but replaced by a directory before it is examined → the kind
/// recorded at listing time disagrees, the walk aborts (`KindChanged`).
#[test]
fn entry_kind_changed_since_listing_aborts() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirListed { relative, .. } = ev
            && relative.as_os_str().is_empty()
        {
            fs::remove_file(root.join("cand/a.txt")).unwrap();
            fs::create_dir(root.join("cand/a.txt")).unwrap();
            fs::write(root.join("cand/a.txt/planted"), b"p").unwrap();
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    assert!(matches!(
        r.report.aborted.as_ref().unwrap().anomaly,
        Anomaly::KindChanged { .. }
    ));
    check(&fx, &[&item], &["cand"]);
}

/// A single-file item replaced by another file between its check and its removal → refused,
/// the replacement is kept.
#[test]
fn single_file_replaced_before_unlink_is_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("big.bin", b"planned")
        .unwrap()
        .file("other.bin", b"not planned")
        .unwrap()
        .checkpoint()
        .unwrap();
    let item = s0_delete(&fx, "big.bin");
    let root = fx.root().to_path_buf();
    let e = delete_observed(&fx, &item, "big.bin", &mut |ev| {
        if let WalkEvent::EntryChecked { relative, .. } = ev
            && relative.as_os_str().is_empty()
        {
            fs::rename(root.join("other.bin"), root.join("big.bin")).unwrap();
        }
    })
    .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    assert_eq!(fs::read(root.join("big.bin")).unwrap(), b"not planned");
    check(&fx, &[&item], &["big.bin", "other.bin"]);
}

/// A single-file item with other hard links is refused: removing one name frees nothing (the
/// other link keeps the data) and the other name may be the user's.
#[test]
fn hardlinked_single_file_item_is_refused() {
    let mut fx = Fixture::new().unwrap();
    let canary_file = fx.canary().symlink_target_file.clone();
    fx.hardlink("linked.txt", &canary_file).unwrap();
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "linked.txt");
    let e = delete(&fx, &item, "linked.txt").unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");
    assert!(fx.root().join("linked.txt").exists());
    assert_canary_files(&fx);
    check(&fx, &[&item], &[]);
}
