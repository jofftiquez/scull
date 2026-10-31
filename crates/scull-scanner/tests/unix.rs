//! Unix-only traversal tests: symlink loops (FX-FS-02), special files (FX-FS-14, SAFE-18) and
//! permission-denied directories (FX-FS-08). Everything runs inside fresh temp directories,
//! except that the device-node test reads the metadata of the real `/dev` (device nodes
//! can't be created without root); nothing there is opened or changed.
#![cfg(unix)]

mod common;

use common::*;
use scull_platform::{EntryKind, Native, Platform};
use scull_scanner::{IssueKind, IssueOp, ScanError, ScanOptions, ScanStatus};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

#[test]
fn fx_fs_14_fifo_without_writer_is_never_opened_and_scan_is_fast() -> TestResult {
    // Opening a FIFO that has no writer blocks forever, so a scan that finishes at all never
    // opened it; the recording platform also proves no special file was even listed.
    let (_t, root) = temp_root();
    mixed_tree(&root);
    for fifo in ["pipe", "a/pipe", "a/b/c/pipe"] {
        assert!(make_fifo(&root.join(fifo)), "mkfifo {fifo}");
    }
    let _s1 = UnixListener::bind(root.join("sock"))?;
    let _s2 = UnixListener::bind(root.join("a/b/sock"))?;
    let want = reference(&root);
    let rec = Arc::new(Recording::new(Native));
    let (p, r) = (Arc::clone(&rec), root.clone());
    let started = Instant::now();
    let (out, ev) = within(Duration::from_secs(20), move || run_with(p, &r, &opts()))?;
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "took {took:?}");
    assert_eq!(out.status, ScanStatus::Completed);
    assert!(ev.issues().is_empty(), "{:?}", ev.issues());
    assert_matches_reference(&out.totals, &want);
    assert_eq!(out.totals.specials, 5);
    assert_eq!(rec.violations(), Vec::<String>::new());
    let mut reads = rec.reads();
    reads.sort();
    let mut dirs: Vec<PathBuf> = ["a", "a/b", "a/b/c", "d", "empty"]
        .iter()
        .map(|p| root.join(p))
        .chain([root.clone()])
        .collect();
    dirs.sort();
    assert_eq!(reads, dirs, "only real directories are listed");
    // The FIFOs are still FIFOs.
    for fifo in ["pipe", "a/pipe", "a/b/c/pipe"] {
        assert_eq!(Native.metadata(&root.join(fifo))?.kind, EntryKind::Fifo);
    }
    Ok(())
}

#[test]
fn fx_fs_14_special_file_roots_are_refused_without_opening() -> TestResult {
    let (_t, root) = temp_root();
    let fifo = root.join("pipe");
    assert!(make_fifo(&fifo), "mkfifo");
    let _sock = UnixListener::bind(root.join("sock"))?;
    let mut cases = vec![
        (fifo, EntryKind::Fifo),
        (root.join("sock"), EntryKind::Socket),
    ];
    if Path::new("/dev/null").exists() {
        cases.push((PathBuf::from("/dev/null"), EntryKind::CharDevice));
    }
    for (path, want) in cases {
        let rec = Arc::new(Recording::new(Native));
        let (p, r) = (Arc::clone(&rec), path.clone());
        let res = within(Duration::from_secs(20), move || run_with(p, &r, &opts()));
        assert!(
            matches!(res, Err(ScanError::RootNotADirectory { found, .. }) if found == want),
            "{}",
            path.display()
        );
        assert!(rec.reads().is_empty(), "{} was listed", path.display());
    }
    Ok(())
}

#[test]
fn fx_fs_14_device_nodes_are_classified_from_metadata_only() -> TestResult {
    // Device nodes can't be created without root, so look at the real ones: `lstat` only,
    // never `open` (opening some devices has side effects). Depth 0: just `/dev` itself.
    assert_eq!(
        Native.metadata(Path::new("/dev/null"))?.kind,
        EntryKind::CharDevice
    );
    let rec = Arc::new(Recording::new(Native));
    let p = Arc::clone(&rec);
    let mut o = opts();
    o.max_depth = Some(0);
    o.dir_timeout = Some(Duration::from_secs(10));
    let (out, ev) = within(Duration::from_secs(30), move || {
        run_with(p, Path::new("/dev"), &o)
    })?;
    assert_eq!(out.status, ScanStatus::Completed);
    assert_eq!(rec.reads(), vec![PathBuf::from("/dev")], "only /dev itself");
    assert_eq!(rec.violations(), Vec::<String>::new());
    let listing = ev.listings().into_iter().next().expect("listing of /dev");
    let null = listing
        .entries
        .iter()
        .find(|e| e.name == "null")
        .expect("/dev/null listed");
    assert_eq!(null.kind, EntryKind::CharDevice);
    let devices = listing
        .entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::CharDevice | EntryKind::BlockDevice))
        .count();
    assert!(devices >= 1);
    assert!(out.totals.specials as usize >= devices);
    // Subdirectories of /dev are counted and reported as depth boundaries, never listed.
    let subdirs = listing
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Dir)
        .count();
    assert_eq!(ev.boundaries().len(), subdirs);
    eprintln!(
        "/dev: {} entries, {devices} device nodes, {} specials, {} links",
        listing.entries.len(),
        out.totals.specials,
        out.totals.links
    );
    Ok(())
}
