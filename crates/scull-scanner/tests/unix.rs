//! Unix-only traversal tests: symlink loops (FX-FS-02), special files (FX-FS-14, SAFE-18) and
//! permission-denied directories (FX-FS-08). Everything runs inside fresh temp directories.
#![cfg(unix)]

mod common;

use common::*;
use scull_platform::EntryKind;
use scull_scanner::{IssueKind, IssueOp, ScanOptions, ScanStatus};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::time::Duration;

fn opts() -> ScanOptions {
    let mut o = ScanOptions::default();
    o.threads = 4;
    o.emit_listings = true;
    o
}

#[test]
fn fx_fs_02_symlink_loops_are_not_followed() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    symlink(".", root.join("self"))?;
    symlink(&root, root.join("a/b/up"))?;
    symlink("loop2", root.join("loop1"))?;
    symlink("loop1", root.join("loop2"))?;
    symlink("missing", root.join("dangling"))?;
    let want = reference(&root);
    let r = root.clone();
    let (out, ev) = within(Duration::from_secs(20), move || run(&r, &opts()))?;
    assert_eq!(out.status, ScanStatus::Completed);
    assert_matches_reference(&out.totals, &want);
    assert_eq!(out.totals.links, 5);
    // Links appear as entries with their own kind and are never listed through.
    assert!(
        ev.listings()
            .iter()
            .all(|l| !l.path.starts_with(root.join("self")))
    );
    assert!(
        ev.listings()
            .iter()
            .all(|l| !l.path.starts_with(root.join("a/b/up")))
    );
    let top = ev
        .listings()
        .into_iter()
        .find(|l| l.path == root)
        .expect("root");
    let link = top.entries.iter().find(|e| e.name == "self").expect("self");
    assert_eq!(link.kind, EntryKind::Symlink);
    assert!(ev.issues().is_empty());
    assert_eq!(ev.dir_done().len(), 6);
    Ok(())
}

#[test]
fn symlinked_root_is_refused() {
    let (_t, root) = temp_root();
    fs::create_dir(root.join("real")).unwrap();
    symlink(root.join("real"), root.join("link")).unwrap();
    let err = run(&root.join("link"), &opts()).err().expect("refused");
    assert!(matches!(
        err,
        scull_scanner::ScanError::RootNotADirectory {
            found: EntryKind::Symlink,
            ..
        }
    ));
}

#[test]
fn fx_fs_14_special_files_are_never_opened() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let fifo = root.join("a/pipe");
    let status = std::process::Command::new("mkfifo").arg(&fifo).status()?;
    assert!(status.success(), "mkfifo failed");
    let _sock = UnixListener::bind(root.join("d/sock"))?;
    let want = reference(&root);
    let r = root.clone();
    // Opening the FIFO for reading would block forever: a hang here is a SAFE-18 violation.
    let (out, ev) = within(Duration::from_secs(20), move || run(&r, &opts()))?;
    assert_eq!(out.status, ScanStatus::Completed);
    assert_matches_reference(&out.totals, &want);
    assert_eq!(out.totals.specials, 2);
    let kinds: Vec<EntryKind> = ev
        .listings()
        .iter()
        .flat_map(|l| l.entries.iter())
        .filter(|e| e.kind.is_special())
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds.len(), 2);
    assert!(kinds.contains(&EntryKind::Fifo) && kinds.contains(&EntryKind::Socket));
    Ok(())
}

#[test]
fn fx_fs_08_unreadable_directory_is_reported_and_scan_continues() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let locked = root.join("a/b");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
    if fs::read_dir(&locked).is_ok() {
        // Running as root: permissions don't apply. Restore and skip.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;
        eprintln!("skipped: running with privileges that bypass permissions");
        return Ok(());
    }
    let result = run(&root, &opts());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?; // let tempdir clean up
    let (out, ev) = result?;
    assert_eq!(out.status, ScanStatus::Completed);
    let issues = ev.issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].path, locked);
    assert_eq!(
        (issues[0].op, issues[0].kind),
        (IssueOp::ListDir, IssueKind::PermissionDenied)
    );
    assert_eq!(out.totals.files, 32); // everything outside a/b
    assert!(!ev.summary_of(&locked).expect("a/b").complete);
    assert!(!out.root.expect("root").complete);
    assert!(ev.summary_of(&root.join("d")).expect("d").complete);
    Ok(())
}

#[test]
fn hardlinks_are_flagged_for_dedup() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    fs::hard_link(root.join("a/one.bin"), root.join("d/one-again.bin"))?;
    let (out, ev) = run(&root, &opts())?;
    assert_eq!(out.totals.multi_linked, 2);
    assert_eq!(out.totals.hardlinks_deduped, 1);
    let ids: Vec<_> = ev
        .listings()
        .iter()
        .flat_map(|l| l.entries.iter())
        .filter(|e| e.kind == EntryKind::File && e.nlink == Some(2))
        .map(|e| e.identity)
        .collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(
        ids[0], ids[1],
        "same identity: the bytes are attributed once"
    );
    Ok(())
}
