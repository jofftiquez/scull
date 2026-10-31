//! Portable traversal tests (P1-SCAN-01): totals match a reference walk, bottom-up summaries,
//! listings, depth limits, volume and loop boundaries, errors as events, cancellation and the
//! per-directory watchdog. Everything runs inside fresh temp directories.

mod common;

use common::*;
use scull_platform::{EntryKind, Native, Operation, Platform, PlatformError};
use scull_scanner::{
    BoundaryReason, CancelToken, IssueKind, IssueOp, NullSink, ScanError, ScanEvent, ScanOptions,
    ScanStatus, scan,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn opts(threads: usize) -> ScanOptions {
    let mut o = ScanOptions::default();
    o.threads = threads;
    o
}

#[test]
fn totals_match_reference_walk() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let want = reference(&root);
    let (out, ev) = run(&root, &opts(4))?;
    assert_eq!(out.status, ScanStatus::Completed);
    assert_matches_reference(&out.totals, &want);
    let root_sum = out.root.as_ref().expect("root summary");
    assert!(root_sum.complete);
    assert_matches_reference(&root_sum.totals, &want);
    assert_eq!(ev.summary_of(&root), Some(root_sum));
    assert_eq!(out.totals.issues, 0);
    assert!(ev.issues().is_empty());
    Ok(())
}

#[test]
fn subtree_summaries_are_bottom_up_and_exact() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let (_, ev) = run(&root, &opts(4))?;

    let done = ev.dir_done();
    // One summary per directory: root, a, a/b, a/b/c, empty, d.
    assert_eq!(done.len(), 6);
    let pos: HashMap<&Path, usize> = done
        .iter()
        .enumerate()
        .map(|(i, d)| (d.path.as_path(), i))
        .collect();
    for d in &done {
        if let Some(parent) = d.path.parent().filter(|p| pos.contains_key(p)) {
            assert!(
                pos[d.path.as_path()] < pos[parent],
                "{:?} before parent",
                d.path
            );
        }
    }
    for sub in ["a", "a/b", "a/b/c", "d", "empty"] {
        let p = root.join(sub);
        let got = ev.summary_of(&p).expect("summary");
        assert_matches_reference(&got.totals, &reference(&p));
        assert_eq!(got.depth, sub.split('/').count());
        assert!(got.complete);
    }
    let abc = ev.summary_of(&root.join("a/b/c")).expect("summary");
    assert_eq!((abc.totals.files, abc.totals.dirs), (2, 0));
    assert!(abc.newest_mtime >= abc.mtime);
    Ok(())
}

#[test]
fn totals_are_identical_for_any_thread_count() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    wide_tree(&root, 3000, 12);
    let want = reference(&root);
    let mut seen = Vec::new();
    for threads in [1, 2, 3, 8, 16] {
        let (out, _) = run(&root, &opts(threads))?;
        assert_eq!(out.threads, threads);
        assert_matches_reference(&out.totals, &want);
        seen.push(out.totals);
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:#?}");
    Ok(())
}

#[test]
fn listings_report_every_entry_with_metadata() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let mut o = opts(4);
    o.emit_listings = true;
    let (_, ev) = run(&root, &o)?;
    let listings = ev.listings();
    assert_eq!(listings.len(), 6);
    let d = listings
        .iter()
        .find(|l| l.path == root.join("d"))
        .expect("listing of d");
    assert_eq!(d.entries.len(), 30);
    let f7 = d.entries.iter().find(|e| e.name == "f7.dat").expect("f7");
    assert_eq!((f7.kind, f7.logical), (EntryKind::File, 700));
    let top = listings.iter().find(|l| l.path == root).expect("root");
    let names: HashSet<_> = top.entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names.len(), 4); // top.txt, a, empty, d
    // Top-down: a directory is listed before its subdirectories.
    let pos = |p: &Path| {
        ev.0.iter()
            .position(|e| matches!(e, ScanEvent::Listing(l) if l.path == p))
    };
    assert!(pos(&root) < pos(&root.join("a")));
    assert!(pos(&root.join("a")) < pos(&root.join("a/b")));
    // Listings are opt-in.
    let (_, quiet) = run(&root, &opts(4))?;
    assert!(quiet.listings().is_empty());
    Ok(())
}

#[test]
fn depth_limit_is_honored() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let mut o = opts(4);
    o.max_depth = Some(1);
    o.emit_listings = true;
    let (out, ev) = run(&root, &o)?;
    assert!(ev.listings().iter().all(|l| l.depth <= 1));
    assert!(ev.dir_done().iter().all(|d| d.depth <= 1));
    let b = ev.boundaries();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].path, root.join("a/b"));
    assert_eq!((b[0].reason, b[0].depth), (BoundaryReason::MaxDepth, 2));
    // a/b is counted as a directory, its contents are not.
    let full = reference(&root);
    assert_eq!(out.totals.dirs, full.dirs - 1); // a/b/c not seen
    assert_eq!(out.totals.files, full.files - 3); // two.bin, three.bin, zero.bin
    assert_eq!(out.totals.boundaries, 1);
    let root_sum = out.root.expect("root");
    assert!(root_sum.complete, "a depth limit is not an error");
    assert_eq!(root_sum.totals.boundaries, 1);

    o.max_depth = Some(0);
    let (out, ev) = run(&root, &o)?;
    assert_eq!(ev.listings().len(), 1);
    assert_eq!(out.totals.entries, 4);
    assert_eq!(ev.boundaries().len(), 3);
    Ok(())
}

#[test]
fn root_must_be_an_existing_real_directory() {
    let (_t, root) = temp_root();
    file(&root.join("f"), 3);
    let missing = scan(
        Arc::new(Native),
        &root.join("nope"),
        &opts(1),
        &CancelToken::new(),
        NullSink,
    );
    assert!(matches!(missing, Err(ScanError::Root(ref e)) if e.is_not_found()));
    let not_dir = scan(
        Arc::new(Native),
        &root.join("f"),
        &opts(1),
        &CancelToken::new(),
        NullSink,
    );
    assert!(matches!(
        not_dir,
        Err(ScanError::RootNotADirectory {
            found: EntryKind::File,
            ..
        })
    ));
    let err = not_dir.unwrap_err();
    let core: scull_core::ScullError = (&err).into();
    assert_eq!(core.kind, scull_core::ErrorKind::Usage);
}

#[test]
fn other_volume_is_a_boundary_unless_allowed() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    if Native.metadata(&root)?.identity.is_none() {
        return Ok(()); // no identity on this platform yet (#32): volume check can't apply
    }
    let mounted = root.join("a");
    let mut p = Tweaked::new();
    let m = mounted.clone();
    p.meta = Box::new(move |path, md| {
        if path.starts_with(&m)
            && let Some(id) = md.identity.as_mut()
        {
            id.volume = id.volume.wrapping_add(1);
        }
    });
    let p: Arc<dyn scull_platform::Platform> = Arc::new(p);
    let (out, ev) = run_with(Arc::clone(&p), &root, &opts(4))?;
    let b = ev.boundaries();
    assert_eq!(b.len(), 1);
    assert_eq!(
        (b[0].path.as_path(), b[0].reason),
        (mounted.as_path(), BoundaryReason::OtherVolume)
    );
    assert!(ev.dir_done().iter().all(|d| !d.path.starts_with(&mounted)));
    assert_eq!(out.totals.files, 31); // top.txt + d/*

    let mut cross = opts(4);
    cross.same_volume = false;
    let (out, ev) = run_with(p, &root, &cross)?;
    assert!(ev.boundaries().is_empty());
    assert_eq!(out.totals.files, 35);
    Ok(())
}

#[test]
fn identity_loop_is_reported_once_and_not_descended() -> TestResult {
    // A directory with the same identity as an ancestor (what a bind mount of a parent looks
    // like) must not be descended (FX-FS-02, T-53).
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let Some(root_id) = Native.metadata(&root)?.identity else {
        return Ok(());
    };
    let looped = root.join("a/b/c");
    let mut p = Tweaked::new();
    let l = looped.clone();
    p.meta = Box::new(move |path, md| {
        if path == l {
            md.identity = Some(root_id);
        }
    });
    let (out, ev) = within(Duration::from_secs(20), move || {
        run_with(Arc::new(p), &root, &opts(4))
    })?;
    let b = ev.boundaries();
    assert_eq!(b.len(), 1);
    assert_eq!(
        (b[0].path.as_path(), b[0].reason),
        (looped.as_path(), BoundaryReason::Loop)
    );
    assert_eq!(out.status, ScanStatus::Completed);
    Ok(())
}

#[test]
fn errors_are_reported_and_never_fatal() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let denied = root.join("a/b");
    let mut p = Tweaked::new();
    let d = denied.clone();
    p.list = Box::new(move |path| {
        (path == d).then(|| {
            PlatformError::from_io(
                Operation::ReadDir,
                path,
                io::Error::from(io::ErrorKind::PermissionDenied),
            )
        })
    });
    let (out, ev) = run_with(Arc::new(p), &root, &opts(4))?;
    assert_eq!(out.status, ScanStatus::Completed);
    let issues = ev.issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, denied);
    assert_eq!(
        (issues[0].op, issues[0].kind),
        (IssueOp::ListDir, IssueKind::PermissionDenied)
    );
    assert_eq!(out.totals.issues, 1);
    // Siblings are still scanned; a/b itself is counted, its contents are not.
    assert_eq!(out.totals.files, 32);
    // The failure marks the directory and its ancestors incomplete, not its siblings.
    assert!(!ev.summary_of(&denied).expect("a/b").complete);
    assert!(!ev.summary_of(&root.join("a")).expect("a").complete);
    assert!(ev.summary_of(&root.join("d")).expect("d").complete);
    let r = out.root.expect("root");
    assert!(!r.complete);
    assert_eq!(r.totals.issues, 1);
    Ok(())
}

#[test]
fn metadata_errors_are_reported() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let gone = root.join("d/f3.dat");
    let g = gone.clone();
    let p = TweakedMeta(move |path: &Path| path == g);
    let (out, ev) = run_with(Arc::new(p), &root, &opts(2))?;
    let issues = ev.issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        (issues[0].path.as_path(), issues[0].op),
        (gone.as_path(), IssueOp::Metadata)
    );
    assert_eq!(issues[0].kind, IssueKind::NotFound);
    assert_eq!(out.totals.files, 34);
    assert!(!ev.summary_of(&root.join("d")).expect("d").complete);
    Ok(())
}

/// Native, but `metadata` reports "not found" for chosen paths (a file deleted mid-scan).
struct TweakedMeta<F>(F);

impl<F: Fn(&Path) -> bool + Send + Sync> scull_platform::Platform for TweakedMeta<F> {
    fn capabilities(&self) -> scull_platform::Capabilities {
        Native.capabilities()
    }
    fn metadata(&self, path: &Path) -> scull_platform::Result<scull_platform::Metadata> {
        if (self.0)(path) {
            return Err(PlatformError::from_io(
                Operation::Metadata,
                path,
                io::Error::from(io::ErrorKind::NotFound),
            ));
        }
        Native.metadata(path)
    }
    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<scull_platform::DirIter<'a>> {
        Native.read_dir(path)
    }
}

#[test]
fn progress_is_throttled_and_final() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let (out, ev) = run(&root, &opts(2))?;
    assert!(matches!(
        ev.0.first(),
        Some(ScanEvent::Started { threads: 2, .. })
    ));
    let Some(ScanEvent::Progress(last)) = ev.0.last() else {
        panic!("last event must be the final progress");
    };
    assert_eq!(last.totals, out.totals);
    Ok(())
}

/// Builds the 50k-entry tree used by the cancellation tests once per test.
fn big_tree() -> (tempfile::TempDir, PathBuf, usize) {
    let (t, root) = temp_root();
    let n = wide_tree(&root, 50_000, 40);
    (t, root, n)
}

#[test]
fn panicking_sink_fails_the_scan_instead_of_hanging() {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let result = within(Duration::from_secs(20), move || {
        std::panic::catch_unwind(|| {
            scan(
                Arc::new(Native),
                &root,
                &opts(4),
                &CancelToken::new(),
                |e: ScanEvent| {
                    if matches!(e, ScanEvent::DirDone(_)) {
                        panic!("sink bug");
                    }
                },
            )
        })
        .is_err()
    });
    assert!(result, "the scan must propagate the failure");
}

#[test]
fn cancellation_returns_within_500ms() -> TestResult {
    let (_t, root, entries) = big_tree();
    assert!(entries >= 50_000);
    let cancel = CancelToken::new();
    let cancelled_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    // A slow consumer stretches the scan so cancellation lands mid-walk.
    let (c, at) = (cancel.clone(), Arc::clone(&cancelled_at));
    let started = Instant::now();
    let sink = move |e: ScanEvent| {
        if matches!(e, ScanEvent::DirDone(_)) {
            std::thread::sleep(Duration::from_millis(2));
        }
        if started.elapsed() > Duration::from_millis(100) && !c.is_cancelled() {
            *at.lock().unwrap() = Some(Instant::now());
            c.cancel();
        }
    };
    let mut o = opts(4);
    o.progress_interval = Duration::from_millis(10);
    let out = scan(Arc::new(Native), &root, &o, &cancel, sink)?;
    let returned = Instant::now();
    let at = cancelled_at.lock().unwrap().expect("cancel was triggered");
    let latency = returned - at;
    assert_eq!(out.status, ScanStatus::Cancelled);
    assert!(out.is_cancelled());
    assert!(out.root.is_none(), "a cancelled scan has no root summary");
    assert!((out.totals.entries as usize) < entries, "partial totals");
    assert!(
        latency < Duration::from_millis(500),
        "cancel took {latency:?}"
    );
    eprintln!(
        "cancel latency {latency:?}, {} entries seen",
        out.totals.entries
    );
    Ok(())
}

#[test]
fn cancellation_from_another_thread() -> TestResult {
    let (_t, root) = temp_root();
    wide_tree(&root, 20_000, 30);
    let cancel = CancelToken::new();
    let c = cancel.clone();
    // A sink that is slow on every event keeps the scan busy.
    let sink = |_e: ScanEvent| std::thread::sleep(Duration::from_millis(1));
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let at = Instant::now();
        c.cancel();
        at
    });
    let mut o = opts(8);
    o.emit_listings = true;
    let out = scan(Arc::new(Native), &root, &o, &cancel, sink)?;
    let at = canceller.join().unwrap();
    let latency = at.elapsed();
    assert_eq!(out.status, ScanStatus::Cancelled);
    assert!(
        latency < Duration::from_millis(500),
        "cancel took {latency:?}"
    );
    Ok(())
}

#[test]
fn already_cancelled_scan_returns_immediately() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let cancel = CancelToken::new();
    cancel.cancel();
    let t = Instant::now();
    let out = scan(Arc::new(Native), &root, &opts(4), &cancel, NullSink)?;
    assert_eq!(out.status, ScanStatus::Cancelled);
    assert!(t.elapsed() < Duration::from_millis(500));
    Ok(())
}

/// A listing that never returns (a stalled network mount) until `release` is set.
fn stalling(stall: PathBuf, release: Arc<AtomicBool>) -> Tweaked {
    let mut p = Tweaked::new();
    p.list = Box::new(move |path| {
        if path == stall {
            while !release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        None
    });
    p
}

#[test]
fn stalled_directory_times_out_and_scan_completes() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let stall = root.join("a/b");
    let release = Arc::new(AtomicBool::new(false));
    let p = stalling(stall.clone(), Arc::clone(&release));
    let mut o = opts(2);
    o.dir_timeout = Some(Duration::from_millis(200));
    let t = Instant::now();
    let (out, ev) = run_with(Arc::new(p), &root, &o)?;
    let took = t.elapsed();
    release.store(true, Ordering::Release);
    assert_eq!(out.status, ScanStatus::Completed);
    assert!(took < Duration::from_secs(5), "took {took:?}");
    let issues = ev.issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        (issues[0].path.as_path(), issues[0].kind),
        (stall.as_path(), IssueKind::TimedOut)
    );
    assert_eq!(out.totals.files, 32); // everything except a/b's subtree
    assert!(!out.root.expect("root").complete);
    Ok(())
}

#[test]
fn cancellation_is_prompt_even_with_a_stuck_worker() -> TestResult {
    // PRD §21.2: cancellation stops a scan in < 500 ms. Shared CI runners occasionally
    // deschedule threads for tens of milliseconds, so take the best of up to three attempts:
    // a real regression fails every attempt, a single scheduling hiccup doesn't.
    let mut latencies = Vec::new();
    for _ in 0..3 {
        let (_t, root) = temp_root();
        mixed_tree(&root);
        let release = Arc::new(AtomicBool::new(false));
        let p = stalling(root.join("a"), Arc::clone(&release));
        let cancel = CancelToken::new();
        let c = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let at = Instant::now();
            c.cancel();
            at
        });
        let mut o = opts(2);
        o.dir_timeout = None;
        let out = scan(Arc::new(p), &root, &o, &cancel, NullSink)?;
        let latency = canceller.join().unwrap().elapsed();
        release.store(true, Ordering::Release);
        assert_eq!(out.status, ScanStatus::Cancelled);
        latencies.push(latency);
        if latency < Duration::from_millis(500) {
            return Ok(());
        }
    }
    panic!("cancel took >= 500 ms in every attempt: {latencies:?}");
}

#[test]
fn no_events_after_scan_returns() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let count = Arc::new(Mutex::new(0usize));
    let c = Arc::clone(&count);
    scan(
        Arc::new(Native),
        &root,
        &opts(4),
        &CancelToken::new(),
        move |_e: ScanEvent| {
            *c.lock().unwrap() += 1;
        },
    )?;
    let at_return = *count.lock().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(*count.lock().unwrap(), at_return);
    let _ = fs::metadata(&root)?; // tree untouched
    Ok(())
}
