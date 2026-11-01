//! A stuck entry costs only that entry (#301, T-53): a frozen network or FUSE mount point
//! directly inside a scanned directory must not take its healthy siblings down with it.
//!
//! * One entry whose metadata call hangs is reported as timed out on its own; the directory
//!   is listed again without it, and every sibling is counted, listed and descended.
//! * Mount points passed in [`ScanOptions::skip_mounts`] (network mounts from the mount table)
//!   are never touched, not even with `lstat`, and are reported as other-volume boundaries.
//! * The fast path makes no extra platform call per entry.
//!
//! The hang is simulated through the platform seam (a metadata call that blocks until
//! released), so these tests are deterministic and need no privileges. Everything runs inside
//! fresh temp directories; nothing is modified.

#![allow(clippy::unwrap_used, clippy::expect_used)] // tests: a panic is a failure

mod common;

use common::*;
use scull_platform::{DirIter, EntryKind, Metadata, Native, Platform};
use scull_scanner::{BoundaryReason, IssueKind, IssueOp, ScanOptions, ScanStatus};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn opts(threads: usize, timeout_ms: u64) -> ScanOptions {
    let mut o = ScanOptions::default();
    o.threads = threads;
    o.emit_listings = true;
    o.dir_timeout = Some(Duration::from_millis(timeout_ms));
    o
}

/// Releases the hung calls when dropped, so a failing assertion can't leave threads blocked.
struct Release(Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A platform whose metadata call on any of `hung` blocks until `release` is set, like
/// `lstat` on the mount point of a frozen FUSE daemon. Counts the calls it blocked.
fn hanging(hung: Vec<PathBuf>, release: Arc<AtomicBool>, blocked: Arc<AtomicUsize>) -> Tweaked {
    let mut p = Tweaked::new();
    p.meta = Box::new(move |path, _md| {
        if hung.iter().any(|h| h == path) {
            blocked.fetch_add(1, Ordering::AcqRel);
            while !release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    });
    p
}

/// `proj/target` (a build directory with files), a few healthy siblings, and the given
/// directories that will "hang" (each holding a file that must never be counted).
fn tree_with(root: &Path, hung: &[&str]) {
    fs::create_dir_all(root.join("proj/target/debug")).unwrap();
    file(&root.join("proj/Cargo.toml"), 40);
    file(&root.join("proj/target/debug/app"), 3000);
    file(&root.join("proj/target/debug/app.d"), 100);
    fs::create_dir_all(root.join("docs")).unwrap();
    file(&root.join("docs/readme.md"), 12);
    file(&root.join("top.txt"), 7);
    for h in hung {
        fs::create_dir_all(root.join(h)).unwrap();
        file(&root.join(h).join("behind-the-mount.bin"), 999);
    }
}

fn names_listed(ev: &Events, dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = ev
        .listings()
        .into_iter()
        .find(|l| l.path == dir)
        .unwrap_or_else(|| panic!("no listing of {}", dir.display()))
        .entries
        .iter()
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn one_hung_entry_is_timed_out_alone_and_its_siblings_are_scanned() -> TestResult {
    let (_t, root) = temp_root();
    tree_with(&root, &["hang"]);
    let hang = root.join("hang");
    let release = Arc::new(AtomicBool::new(false));
    let _release = Release(Arc::clone(&release));
    let blocked = Arc::new(AtomicUsize::new(0));
    let p = hanging(
        vec![hang.clone()],
        Arc::clone(&release),
        Arc::clone(&blocked),
    );

    let t = Instant::now();
    let r = root.clone();
    let (out, ev) = within(Duration::from_secs(20), move || {
        run_with(Arc::new(p), &r, &opts(2, 300))
    })?;
    let took = t.elapsed();

    assert_eq!(out.status, ScanStatus::Completed);
    assert!(took < Duration::from_secs(10), "took {took:?}");
    // Exactly one issue: the hung entry, as a metadata timeout. Not the directory.
    let issues = ev.issues();
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(issues[0].path, hang);
    assert_eq!(issues[0].kind, IssueKind::TimedOut);
    assert_eq!(issues[0].op, IssueOp::Metadata);
    assert!(
        issues[0].message.contains("skipped"),
        "{}",
        issues[0].message
    );
    // The hung entry was asked once: the next attempt skipped it.
    assert_eq!(blocked.load(Ordering::Acquire), 1);

    // Siblings are listed, descended and summed; nothing behind the hung entry is.
    assert_eq!(names_listed(&ev, &root), ["docs", "proj", "top.txt"]);
    let target = ev
        .summary_of(&root.join("proj/target"))
        .expect("proj/target summarized");
    assert!(target.complete);
    assert_eq!(target.totals.files, 2);
    assert!(ev.summary_of(&root.join("docs")).is_some());
    assert!(ev.listings().iter().all(|l| !l.path.starts_with(&hang)));
    assert_eq!(out.totals.files, 5, "{:?}", out.totals);
    assert_eq!(out.totals.issues, 1);
    // The root holds an unread entry: incomplete, honestly.
    let root_sum = out.root.expect("root summary");
    assert!(!root_sum.complete);
    assert_eq!(root_sum.totals.files, 5);
    Ok(())
}

#[test]
fn hung_entries_beyond_the_limit_give_up_the_directory() -> TestResult {
    let max = ScanOptions::MAX_ENTRY_TIMEOUTS_PER_DIR;
    let (_t, root) = temp_root();
    let names: Vec<String> = (0..=max).map(|i| format!("hang{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    tree_with(&root, &refs);
    let hung: Vec<PathBuf> = names.iter().map(|n| root.join(n)).collect();
    let release = Arc::new(AtomicBool::new(false));
    let _release = Release(Arc::clone(&release));
    let p = hanging(hung, Arc::clone(&release), Arc::new(AtomicUsize::new(0)));

    let r = root.clone();
    let (out, ev) = within(Duration::from_secs(30), move || {
        run_with(Arc::new(p), &r, &opts(max + 2, 200))
    })?;
    assert_eq!(out.status, ScanStatus::Completed);
    let issues = ev.issues();
    let entries = issues
        .iter()
        .filter(|i| i.kind == IssueKind::TimedOut && i.op == IssueOp::Metadata)
        .count();
    let dirs: Vec<_> = issues
        .iter()
        .filter(|i| i.kind == IssueKind::TimedOut && i.op == IssueOp::ListDir)
        .collect();
    // `max` entries skipped one by one, then the whole directory given up.
    assert_eq!(entries, max, "{issues:#?}");
    assert_eq!(dirs.len(), 1, "{issues:#?}");
    assert_eq!(dirs[0].path, root);
    assert!(!out.root.expect("root").complete);
    Ok(())
}

#[test]
fn without_a_spare_worker_a_hung_entry_gives_up_the_directory() -> TestResult {
    // One thread: the first hung entry uses the only replacement; the second can't be
    // retried (the queued directory would wait behind stuck workers), so the directory is
    // given up as before, and the scan still completes.
    let (_t, root) = temp_root();
    tree_with(&root, &["hang0", "hang1"]);
    let hung = vec![root.join("hang0"), root.join("hang1")];
    let release = Arc::new(AtomicBool::new(false));
    let _release = Release(Arc::clone(&release));
    let p = hanging(hung, Arc::clone(&release), Arc::new(AtomicUsize::new(0)));
    let r = root.clone();
    let (out, ev) = within(Duration::from_secs(20), move || {
        run_with(Arc::new(p), &r, &opts(1, 200))
    })?;
    assert_eq!(out.status, ScanStatus::Completed);
    let kinds: Vec<(IssueKind, IssueOp)> = ev.issues().iter().map(|i| (i.kind, i.op)).collect();
    assert_eq!(
        kinds,
        [
            (IssueKind::TimedOut, IssueOp::Metadata),
            (IssueKind::TimedOut, IssueOp::ListDir)
        ]
    );
    Ok(())
}

#[test]
fn a_hung_entry_deep_in_the_tree_only_costs_itself() -> TestResult {
    // The frozen mount sits inside a candidate (`proj/target/net`): the candidate is still
    // listed and summarized (incomplete, so never offered as safe), its other files counted.
    let (_t, root) = temp_root();
    tree_with(&root, &["proj/target/net"]);
    let net = root.join("proj/target/net");
    let release = Arc::new(AtomicBool::new(false));
    let _release = Release(Arc::clone(&release));
    let p = hanging(
        vec![net.clone()],
        Arc::clone(&release),
        Arc::new(AtomicUsize::new(0)),
    );
    let r = root.clone();
    let (out, ev) = within(Duration::from_secs(20), move || {
        run_with(Arc::new(p), &r, &opts(4, 300))
    })?;
    assert_eq!(out.status, ScanStatus::Completed);
    let issues = ev.issues();
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(
        (issues[0].path.as_path(), issues[0].op),
        (net.as_path(), IssueOp::Metadata)
    );
    let target = ev.summary_of(&root.join("proj/target")).expect("summary");
    assert!(
        !target.complete,
        "a candidate with an unread entry is incomplete"
    );
    assert_eq!(target.totals.files, 2);
    assert_eq!(names_listed(&ev, &root.join("proj/target")), ["debug"]);
    assert_eq!(out.totals.files, 5);
    Ok(())
}

/// Wraps a platform, recording every path whose metadata is read or which is listed.
struct Counting {
    meta: Mutex<Vec<PathBuf>>,
    lists: Mutex<Vec<PathBuf>>,
}

impl Counting {
    fn new() -> Self {
        Self {
            meta: Mutex::new(Vec::new()),
            lists: Mutex::new(Vec::new()),
        }
    }
}

impl Platform for Counting {
    fn capabilities(&self) -> scull_platform::Capabilities {
        Native.capabilities()
    }

    fn metadata(&self, path: &Path) -> scull_platform::Result<Metadata> {
        self.meta.lock().unwrap().push(path.to_path_buf());
        Native.metadata(path)
    }

    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<DirIter<'a>> {
        self.lists.lock().unwrap().push(path.to_path_buf());
        Native.read_dir(path)
    }
}

#[test]
fn skip_mounts_are_never_touched_and_reported_as_other_volume() -> TestResult {
    let (_t, root) = temp_root();
    tree_with(&root, &["share", "proj/target/net"]);
    let share = root.join("share");
    let net = root.join("proj/target/net");
    let platform = Arc::new(Counting::new());
    let mut o = opts(4, 60_000);
    o.skip_mounts = vec![share.clone(), net.clone(), root.join("absent/elsewhere")];
    let (out, ev) = run_with(Arc::clone(&platform) as Arc<dyn Platform>, &root, &o)?;
    assert_eq!(out.status, ScanStatus::Completed);
    assert!(ev.issues().is_empty(), "{:#?}", ev.issues());

    // Not a single platform call on a skipped mount point or below it.
    let touched: Vec<PathBuf> = platform
        .meta
        .lock()
        .unwrap()
        .iter()
        .chain(platform.lists.lock().unwrap().iter())
        .filter(|p| p.starts_with(&share) || p.starts_with(&net))
        .cloned()
        .collect();
    assert!(touched.is_empty(), "touched: {touched:?}");

    // Reported as other-volume boundaries without identity, listed with unknown size.
    let mut b: Vec<(PathBuf, BoundaryReason, bool)> = ev
        .boundaries()
        .iter()
        .map(|b| (b.path.clone(), b.reason, b.identity.is_none()))
        .collect();
    b.sort_by(|x, y| x.0.cmp(&y.0));
    let mut want = vec![
        (net.clone(), BoundaryReason::OtherVolume, true),
        (share.clone(), BoundaryReason::OtherVolume, true),
    ];
    want.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(b, want);
    let listing = ev
        .listings()
        .into_iter()
        .find(|l| l.path == root)
        .expect("root listing");
    let e = listing
        .entries
        .iter()
        .find(|e| e.name == "share")
        .expect("share listed");
    assert_eq!(e.kind, EntryKind::Dir);
    assert_eq!((e.allocated, e.identity, e.mtime), (None, None, None));

    // Counted as directories (not descended) with unknown size; siblings fully scanned.
    let target = ev.summary_of(&root.join("proj/target")).expect("summary");
    assert!(target.complete, "a boundary is not a read error");
    assert_eq!(target.totals.boundaries, 1);
    assert_eq!(target.totals.files, 2);
    assert_eq!(out.totals.files, 5);
    assert_eq!(out.totals.boundaries, 2);
    assert!(out.totals.allocated_unknown >= 2);
    Ok(())
}

#[test]
fn the_fast_path_makes_one_metadata_call_per_entry() -> TestResult {
    // No extra system call per entry (#301): with or without unrelated skip mounts, the scan
    // reads each entry's metadata exactly once (plus the root's) and lists each directory once.
    let (_t, root) = temp_root();
    mixed_tree(&root);
    for skip in [
        Vec::new(),
        vec![root.join("a/elsewhere"), PathBuf::from("/nonexistent/m")],
    ] {
        let platform = Arc::new(Counting::new());
        let mut o = opts(4, 60_000);
        o.skip_mounts = skip;
        let (out, ev) = run_with(Arc::clone(&platform) as Arc<dyn Platform>, &root, &o)?;
        assert!(ev.issues().is_empty());
        let meta = platform.meta.lock().unwrap().len() as u64;
        let lists = platform.lists.lock().unwrap().len() as u64;
        assert_eq!(meta, out.totals.entries + 1, "metadata calls");
        assert_eq!(lists, out.totals.dirs + 1, "listings");
    }
    Ok(())
}

/// The real thing, for a manual or containerized run: `SCULL_FROZEN_ROOT` is a directory
/// holding a FUSE mount whose daemon was stopped (`bindfs` + `SIGSTOP`), named by
/// `SCULL_FROZEN_ENTRY`. Scanned with the native platform and no mount table (so the
/// per-entry deadline is what saves the siblings): the frozen mount point alone is timed out.
/// Needs a FUSE mount, so it is ignored and skips itself without the variables.
#[test]
#[ignore = "manual: needs a frozen FUSE mount (SCULL_FROZEN_ROOT / SCULL_FROZEN_ENTRY)"]
fn real_frozen_fuse_mount_point_is_timed_out_alone() -> TestResult {
    let (Ok(root), Ok(entry)) = (
        std::env::var("SCULL_FROZEN_ROOT"),
        std::env::var("SCULL_FROZEN_ENTRY"),
    ) else {
        eprintln!("skipped: SCULL_FROZEN_ROOT / SCULL_FROZEN_ENTRY not set");
        return Ok(());
    };
    let root = PathBuf::from(root);
    let frozen = root.join(entry);
    let r = root.clone();
    let t = Instant::now();
    let (out, ev) = within(Duration::from_secs(60), move || {
        run_with(Arc::new(Native), &r, &opts(4, 3000))
    })?;
    let took = t.elapsed();
    let issues = ev.issues();
    for i in &issues {
        println!(
            "issue: {:?} ({:?}) at {}: {}",
            i.kind,
            i.op,
            i.path.display(),
            i.message
        );
    }
    let listed = names_listed(&ev, &root);
    println!("listed in {}: {listed:?}", root.display());
    println!(
        "scan took {took:.1?}; {} files, {} dirs",
        out.totals.files, out.totals.dirs
    );
    assert_eq!(out.status, ScanStatus::Completed);
    assert_eq!(issues.len(), 1, "{issues:#?}");
    assert_eq!(
        (issues[0].path.as_path(), issues[0].kind, issues[0].op),
        (frozen.as_path(), IssueKind::TimedOut, IssueOp::Metadata)
    );
    assert!(!listed.is_empty(), "siblings listed");
    assert!(!listed.iter().any(|n| frozen.ends_with(n)), "{listed:?}");
    Ok(())
}
