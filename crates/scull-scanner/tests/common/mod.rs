//! Shared helpers for scanner tests. Everything happens inside fresh temp directories.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // test helpers: panics are failures

pub mod graph;

use scull_platform::{DirIter, Metadata, Native, Platform};
use scull_scanner::{
    CancelToken, DirSummary, ScanError, ScanEvent, ScanOptions, ScanOutcome, Totals, scan,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Events collected by [`run`].
#[derive(Default)]
pub struct Events(pub Vec<ScanEvent>);

impl Events {
    pub fn dir_done(&self) -> Vec<&DirSummary> {
        self.0
            .iter()
            .filter_map(|e| match e {
                ScanEvent::DirDone(d) => Some(d),
                _ => None,
            })
            .collect()
    }

    pub fn summary_of(&self, path: &Path) -> Option<&DirSummary> {
        self.dir_done().into_iter().find(|d| d.path == path)
    }

    pub fn issues(&self) -> Vec<&scull_scanner::ScanIssue> {
        self.0
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Issue(i) => Some(i),
                _ => None,
            })
            .collect()
    }

    pub fn boundaries(&self) -> Vec<&scull_scanner::Boundary> {
        self.0
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Boundary(b) => Some(b),
                _ => None,
            })
            .collect()
    }

    pub fn listings(&self) -> Vec<&scull_scanner::DirListing> {
        self.0
            .iter()
            .filter_map(|e| match e {
                ScanEvent::Listing(l) => Some(l),
                _ => None,
            })
            .collect()
    }
}

/// Scan `root` with `platform`, collecting every event.
pub fn run_with(
    platform: Arc<dyn Platform>,
    root: &Path,
    opts: &ScanOptions,
) -> Result<(ScanOutcome, Events), ScanError> {
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let outcome = scan(platform, root, opts, &CancelToken::new(), move |e| {
        sink.lock().unwrap().0.push(e);
    })?;
    let events = std::mem::take(&mut *events.lock().unwrap());
    Ok((outcome, events))
}

/// Scan `root` with the native platform, collecting every event.
pub fn run(root: &Path, opts: &ScanOptions) -> Result<(ScanOutcome, Events), ScanError> {
    run_with(Arc::new(Native), root, opts)
}

/// Run `f` on another thread; fail if it takes longer than `limit` (a hang means a FIFO was
/// opened or a loop was followed).
pub fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit)
        .expect("scan did not finish in time (hang)")
}

/// Write a file of `len` bytes.
pub fn file(path: &Path, len: usize) {
    fs::write(path, vec![b'x'; len]).unwrap();
}

/// A mixed tree: nested dirs, files of varied sizes, an empty dir. Returns the root.
pub fn mixed_tree(root: &Path) {
    file(&root.join("top.txt"), 10);
    fs::create_dir_all(root.join("a/b/c")).unwrap();
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::create_dir_all(root.join("d")).unwrap();
    file(&root.join("a/one.bin"), 5000);
    file(&root.join("a/b/two.bin"), 70_000);
    file(&root.join("a/b/c/three.bin"), 1);
    file(&root.join("a/b/c/zero.bin"), 0);
    for i in 0..30 {
        file(&root.join(format!("d/f{i}.dat")), i * 100);
    }
}

/// `count` files spread over directories `fanout` wide, two levels deep. Returns the number
/// of entries created (files + dirs).
pub fn wide_tree(root: &Path, count: usize, fanout: usize) -> usize {
    let mut entries = 0;
    let per_leaf = count.div_ceil(fanout * fanout).max(1);
    let mut made = 0;
    'outer: for i in 0..fanout {
        let d1 = root.join(format!("d{i}"));
        fs::create_dir(&d1).unwrap();
        entries += 1;
        for j in 0..fanout {
            let d2 = d1.join(format!("e{j}"));
            fs::create_dir(&d2).unwrap();
            entries += 1;
            for k in 0..per_leaf {
                if made == count {
                    break 'outer;
                }
                fs::write(d2.join(format!("f{k}")), b"").unwrap();
                made += 1;
                entries += 1;
            }
        }
    }
    entries
}

/// Reference totals from a plain recursive walk with `std::fs::symlink_metadata` (logical
/// sizes and counts; allocated sizes from the platform, which the platform crate tests).
pub fn reference(root: &Path) -> Totals {
    let mut t = Totals::default();
    let md = fs::symlink_metadata(root).unwrap();
    t.logical += md.len();
    t.allocated += Native.metadata(root).unwrap().allocated_size.unwrap_or(0);
    walk(root, &mut t);
    t
}

fn walk(dir: &Path, t: &mut Totals) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let md = fs::symlink_metadata(e.path()).unwrap();
        t.entries += 1;
        t.logical += md.len();
        t.allocated += Native
            .metadata(&e.path())
            .unwrap()
            .allocated_size
            .unwrap_or(0);
        let ft = md.file_type();
        if ft.is_symlink() {
            t.links += 1;
        } else if ft.is_dir() {
            t.dirs += 1;
            walk(&e.path(), t);
        } else if ft.is_file() {
            t.files += 1;
        } else {
            t.specials += 1;
        }
    }
}

/// Compare the fields [`reference`] computes.
pub fn assert_matches_reference(got: &Totals, want: &Totals) {
    assert_eq!(got.entries, want.entries, "entries");
    assert_eq!(got.files, want.files, "files");
    assert_eq!(got.dirs, want.dirs, "dirs");
    assert_eq!(got.links, want.links, "links");
    assert_eq!(got.specials, want.specials, "specials");
    assert_eq!(got.logical, want.logical, "logical bytes");
    assert_eq!(got.allocated, want.allocated, "allocated bytes");
}

type MetaHook = dyn Fn(&Path, &mut Metadata) + Send + Sync;
type ListHook = dyn Fn(&Path) -> Option<scull_platform::PlatformError> + Send + Sync;

/// The native platform with hooks to alter metadata or fail / stall listings, for conditions
/// a temp dir can't produce portably (other volumes, identity loops, stalled mounts).
pub struct Tweaked {
    pub meta: Box<MetaHook>,
    pub list: Box<ListHook>,
}

impl Tweaked {
    pub fn new() -> Self {
        Self {
            meta: Box::new(|_, _| {}),
            list: Box::new(|_| None),
        }
    }
}

impl Platform for Tweaked {
    fn capabilities(&self) -> scull_platform::Capabilities {
        Native.capabilities()
    }

    fn metadata(&self, path: &Path) -> scull_platform::Result<Metadata> {
        let mut md = Native.metadata(path)?;
        (self.meta)(path, &mut md);
        Ok(md)
    }

    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<DirIter<'a>> {
        if let Some(e) = (self.list)(path) {
            return Err(e);
        }
        Native.read_dir(path)
    }
}

/// Canonical temp dir path, so paths reported by the scan compare equal to ours.
pub fn temp_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    (dir, root)
}

/// Wraps a platform and records every directory the scanner lists. Before delegating, it
/// checks the path's no-follow kind: listing anything but a real directory (a link, FIFO,
/// socket or device: "opening" it, SAFE-18) is refused and recorded as a violation.
pub struct Recording<P> {
    pub inner: P,
    reads: Mutex<Vec<PathBuf>>,
    violations: Mutex<Vec<String>>,
}

impl<P: Platform> Recording<P> {
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            reads: Mutex::default(),
            violations: Mutex::default(),
        }
    }

    /// Paths listed, in call order.
    pub fn reads(&self) -> Vec<PathBuf> {
        self.reads.lock().unwrap().clone()
    }

    /// Listings of anything that is not a real directory.
    pub fn violations(&self) -> Vec<String> {
        self.violations.lock().unwrap().clone()
    }
}

impl<P: Platform> Platform for Recording<P> {
    fn capabilities(&self) -> scull_platform::Capabilities {
        self.inner.capabilities()
    }

    fn metadata(&self, path: &Path) -> scull_platform::Result<Metadata> {
        self.inner.metadata(path)
    }

    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<DirIter<'a>> {
        self.reads.lock().unwrap().push(path.to_path_buf());
        let kind = self.inner.metadata(path)?.kind;
        if kind != scull_platform::EntryKind::Dir {
            self.violations
                .lock()
                .unwrap()
                .push(format!("listed a {kind}: {}", path.display()));
            return Err(scull_platform::PlatformError::not_a_directory(path, kind));
        }
        self.inner.read_dir(path)
    }
}

/// Every path an event reports (listings and their entries, summaries, boundaries, issues).
pub fn reported_paths(ev: &Events) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in &ev.0 {
        match e {
            ScanEvent::Listing(l) => {
                out.push(l.path.clone());
                out.extend(l.entries.iter().map(|en| l.path.join(&en.name)));
            }
            ScanEvent::DirDone(d) => out.push(d.path.clone()),
            ScanEvent::Boundary(b) => out.push(b.path.clone()),
            ScanEvent::Issue(i) => out.push(i.path.clone()),
            _ => {}
        }
    }
    out
}

/// Create a FIFO with the `mkfifo` tool. `false` where there is none (Windows).
pub fn make_fifo(path: &Path) -> bool {
    std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .is_ok_and(|s| s.success())
}
