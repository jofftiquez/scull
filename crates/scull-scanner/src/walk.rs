//! The traversal engine.
//!
//! # Design
//!
//! * **Work queue.** A shared LIFO stack of directories still to list, guarded by one mutex
//!   and a condvar. LIFO keeps the walk roughly depth-first, so the set of pending
//!   directories stays proportional to depth × fan-out instead of the whole tree. Workers take
//!   the lock once per directory (to pop) and once per directory with subdirectories (to push
//!   them all).
//! * **No tree in memory.** Each pending directory is a [`DirNode`] holding its path, a link
//!   to its parent, a running [`Agg`] and a counter of unfinished parts (its own listing plus
//!   one per subdirectory). When the counter reaches zero the directory emits
//!   [`ScanEvent::DirDone`], folds its totals into its parent and is dropped, so finished
//!   subtrees cost nothing.
//! * **Claiming.** A worker gathers a directory's entries into local buffers and only then
//!   *claims* the node to commit them. The watchdog can claim a node first when its listing
//!   overruns [`ScanOptions::dir_timeout`]; the worker then discards its work. Either way a
//!   node's results are committed exactly once.
//! * **Detached workers.** Workers are ordinary (not scoped) threads, so a worker stuck in an
//!   OS call (a stalled network mount) can neither stop cancellation from returning on time
//!   nor stall the scan: the watchdog replaces it and the stuck thread exits when the call
//!   returns, delivering nothing.

use crate::CancelToken;
use crate::types::{
    Boundary, BoundaryReason, DirListing, DirSummary, EntryInfo, EventSink, IssueOp, Progress,
    ScanError, ScanEvent, ScanIssue, ScanOptions, ScanOutcome, ScanStatus, Totals,
};
use scull_core::FileIdentity;
use scull_platform::{EntryKind, Metadata, Platform};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How often idle workers and the coordinator wake up to check for cancellation, progress and
/// timeouts. Bounds cancellation latency together with one OS call.
const TICK: Duration = Duration::from_millis(20);

/// How long the coordinator waits for workers to exit after the scan ends before returning
/// anyway (a worker can be stuck in an OS call).
const SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// Lock a mutex, ignoring poisoning: the data under our locks stays consistent even if a
/// sink panicked while a lock was held elsewhere.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Totals of a directory's subtree, accumulated as parts of it finish.
#[derive(Debug, Clone, Copy)]
struct Agg {
    totals: Totals,
    newest_mtime: Option<i64>,
    complete: bool,
}

impl Agg {
    fn new(own: &Metadata) -> Self {
        let mut totals = Totals::default();
        totals.add_size(own);
        Self {
            totals,
            newest_mtime: own.mtime_unix,
            complete: true,
        }
    }

    fn see_mtime(&mut self, m: Option<i64>) {
        self.newest_mtime = self.newest_mtime.max(m);
    }

    /// Fold in a finished subdirectory (the subdirectory itself counts as one entry).
    fn merge_child(&mut self, child: &Agg) {
        self.totals.merge(&child.totals);
        self.totals.entries += 1;
        self.totals.dirs += 1;
        self.see_mtime(child.newest_mtime);
        self.complete &= child.complete;
    }
}

/// A directory that has been discovered but whose subtree is not finished yet.
struct DirNode {
    path: PathBuf,
    depth: usize,
    identity: Option<FileIdentity>,
    mtime: Option<i64>,
    parent: Option<Arc<DirNode>>,
    /// Set once by whoever commits this node's listing: its worker or the watchdog.
    claimed: AtomicBool,
    /// Unfinished parts: 1 for the listing itself plus one per queued subdirectory.
    pending: AtomicUsize,
    agg: Mutex<Agg>,
}

impl DirNode {
    fn new(path: PathBuf, depth: usize, md: &Metadata, parent: Option<Arc<DirNode>>) -> Self {
        Self {
            path,
            depth,
            identity: md.identity,
            mtime: md.mtime_unix,
            parent,
            claimed: AtomicBool::new(false),
            pending: AtomicUsize::new(1),
            agg: Mutex::new(Agg::new(md)),
        }
    }

    /// Try to become the one who commits this node's listing.
    fn claim(&self) -> bool {
        !self.claimed.swap(true, Ordering::AcqRel)
    }

    fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::Acquire)
    }

    /// Whether `id` is the identity of this directory or one of its ancestors.
    fn has_ancestor(&self, id: FileIdentity) -> bool {
        let mut cur = Some(self);
        while let Some(n) = cur {
            if n.identity == Some(id) {
                return true;
            }
            cur = n.parent.as_deref();
        }
        false
    }
}

/// What one worker is doing, for the watchdog and progress display.
#[derive(Default)]
struct Slot {
    current: Mutex<Option<(Arc<DirNode>, Instant)>>,
    /// Set by the watchdog when it gave up on this worker's directory and started a
    /// replacement; the worker exits after its current call returns.
    retired: AtomicBool,
}

struct Shared {
    platform: Arc<dyn Platform>,
    sink: Box<dyn EventSink>,
    cancel: CancelToken,
    opts: ScanOptions,
    root_volume: Option<u64>,
    queue: Mutex<Vec<Arc<DirNode>>>,
    queue_cv: Condvar,
    /// The scan is over (finished, cancelled or failed); workers exit.
    stop: AtomicBool,
    /// No more events may be delivered.
    closed: AtomicBool,
    /// Set when the root's subtree is finished.
    done: Mutex<Option<DirSummary>>,
    done_cv: Condvar,
    live_workers: AtomicUsize,
    /// A worker died from a panic (in the sink or a platform implementation).
    worker_panicked: AtomicBool,
    totals: Mutex<Totals>,
    slots: Mutex<Vec<Arc<Slot>>>,
}

impl Shared {
    fn emit(&self, event: ScanEvent) {
        if !self.closed.load(Ordering::Acquire) {
            self.sink.event(event);
        }
    }

    fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.cancel.is_cancelled()
    }

    /// Next directory to list, or `None` when the scan is over.
    fn next_job(&self) -> Option<Arc<DirNode>> {
        let mut q = lock(&self.queue);
        loop {
            if self.should_stop() {
                return None;
            }
            if let Some(n) = q.pop() {
                return Some(n);
            }
            q = self
                .queue_cv
                .wait_timeout(q, TICK)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn push_jobs(&self, nodes: Vec<Arc<DirNode>>) {
        let n = nodes.len();
        if n == 0 {
            return;
        }
        lock(&self.queue).extend(nodes);
        if n == 1 {
            self.queue_cv.notify_one();
        } else {
            self.queue_cv.notify_all();
        }
    }

    /// Mark one part of `node` finished; finish the node and its ancestors as they complete.
    fn finish_part(&self, node: &Arc<DirNode>) {
        let mut cur = Arc::clone(node);
        loop {
            if cur.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
                return;
            }
            let agg = *lock(&cur.agg);
            let summary = DirSummary {
                path: cur.path.clone(),
                depth: cur.depth,
                identity: cur.identity,
                mtime: cur.mtime,
                newest_mtime: agg.newest_mtime,
                totals: agg.totals,
                complete: agg.complete,
            };
            match &cur.parent {
                Some(parent) => {
                    self.emit(ScanEvent::DirDone(summary));
                    lock(&parent.agg).merge_child(&agg);
                    cur = Arc::clone(parent);
                }
                None => {
                    self.emit(ScanEvent::DirDone(summary.clone()));
                    *lock(&self.done) = Some(summary);
                    self.done_cv.notify_all();
                    return;
                }
            }
        }
    }

    /// Why a subdirectory at `depth` with metadata `md` must not be descended, if it mustn't.
    fn boundary(&self, parent: &DirNode, depth: usize, md: &Metadata) -> Option<BoundaryReason> {
        if self.opts.max_depth.is_some_and(|max| depth > max) {
            return Some(BoundaryReason::MaxDepth);
        }
        let id = md.identity?;
        if self.opts.same_volume && self.root_volume.is_some_and(|v| v != id.volume) {
            return Some(BoundaryReason::OtherVolume);
        }
        parent.has_ancestor(id).then_some(BoundaryReason::Loop)
    }

    /// List one directory and commit the results.
    fn process(&self, node: &Arc<DirNode>) {
        let entries = match self.platform.read_dir(&node.path) {
            Ok(it) => it,
            Err(e) => {
                if node.claim() {
                    let issue = ScanIssue::from_platform(IssueOp::ListDir, &e);
                    {
                        let mut agg = lock(&node.agg);
                        agg.totals.issues += 1;
                        agg.complete = false;
                    }
                    lock(&self.totals).issues += 1;
                    self.emit(ScanEvent::Issue(issue));
                    self.finish_part(node);
                }
                return;
            }
        };

        let child_depth = node.depth + 1;
        let mut local = Agg {
            totals: Totals::default(),
            newest_mtime: None,
            complete: true,
        };
        let mut counted = Totals::default();
        let mut issues = Vec::new();
        let mut boundaries = Vec::new();
        let mut subdirs: Vec<(PathBuf, Metadata)> = Vec::new();
        let mut listing = self.opts.emit_listings.then(Vec::new);

        for item in entries {
            // Checked per entry: bounds cancellation latency in huge directories, and stops
            // work the watchdog already gave up on.
            if self.should_stop() || node.is_claimed() {
                return;
            }
            let de = match item {
                Ok(de) => de,
                Err(e) => {
                    issues.push(ScanIssue::from_platform(IssueOp::ReadEntry, &e));
                    continue;
                }
            };
            // lstat: authoritative kind (the listing's hint may be stale) and sizes. Never
            // follows a link and never opens the entry, so FIFOs and devices are safe.
            let md = match self.platform.metadata(&de.path) {
                Ok(md) => md,
                Err(e) => {
                    issues.push(ScanIssue::from_platform(IssueOp::Metadata, &e));
                    continue;
                }
            };
            counted.add_entry(&md);
            if let Some(l) = listing.as_mut() {
                l.push(EntryInfo::new(de.file_name, &md));
            }
            if md.kind == EntryKind::Dir {
                if let Some(reason) = self.boundary(node, child_depth, &md) {
                    boundaries.push(Boundary {
                        path: de.path,
                        depth: child_depth,
                        identity: md.identity,
                        reason,
                    });
                } else {
                    // Its own size and entry count are added when it finishes.
                    subdirs.push((de.path, md));
                    continue;
                }
            }
            local.totals.add_entry(&md);
            local.see_mtime(md.mtime_unix);
        }

        if !node.claim() {
            return; // the watchdog gave up on this directory and reported it
        }

        local.totals.issues += issues.len() as u64;
        local.totals.boundaries += boundaries.len() as u64;
        local.complete = issues.is_empty();
        counted.issues = local.totals.issues;
        counted.boundaries = local.totals.boundaries;
        lock(&self.totals).merge(&counted);

        for issue in issues {
            self.emit(ScanEvent::Issue(issue));
        }
        for b in boundaries {
            self.emit(ScanEvent::Boundary(b));
        }
        if let Some(entries) = listing {
            self.emit(ScanEvent::Listing(DirListing {
                path: node.path.clone(),
                depth: node.depth,
                identity: node.identity,
                entries,
            }));
        }

        // Register the subdirectories as unfinished parts *before* queueing them, so the node
        // can't finish early; then release this listing's own part.
        node.pending.fetch_add(subdirs.len(), Ordering::AcqRel);
        {
            let mut agg = lock(&node.agg);
            agg.totals.merge(&local.totals);
            agg.see_mtime(local.newest_mtime);
            agg.complete &= local.complete;
        }
        let jobs = subdirs
            .into_iter()
            .map(|(path, md)| Arc::new(DirNode::new(path, child_depth, &md, Some(node.clone()))))
            .collect();
        self.push_jobs(jobs);
        self.finish_part(node);
    }

    /// Give up on directories whose listing overran the timeout.
    fn watchdog(self: &Arc<Self>, timeout: Duration, replacements: &mut usize) {
        let slots: Vec<Arc<Slot>> = lock(&self.slots).clone();
        for slot in slots {
            let overdue = match &*lock(&slot.current) {
                Some((node, started)) if started.elapsed() > timeout => Some(Arc::clone(node)),
                _ => None,
            };
            let Some(node) = overdue else { continue };
            if !node.claim() {
                continue;
            }
            slot.retired.store(true, Ordering::Release);
            {
                let mut agg = lock(&node.agg);
                agg.totals.issues += 1;
                agg.complete = false;
            }
            lock(&self.totals).issues += 1;
            self.emit(ScanEvent::Issue(ScanIssue::timed_out(
                node.path.clone(),
                timeout,
            )));
            // Keep the pool at strength, but don't pile up threads behind a mount that hangs
            // every call.
            if *replacements < self.opts.effective_threads() {
                *replacements += 1;
                // A failed spawn only means one fewer worker.
                let _ = spawn_worker(self);
            }
            self.finish_part(&node);
        }
    }

    fn current_dir(&self) -> Option<PathBuf> {
        let slots = lock(&self.slots);
        slots
            .iter()
            .filter_map(|s| lock(&s.current).as_ref().map(|(n, t)| (*t, n.path.clone())))
            .max_by_key(|(t, _)| *t)
            .map(|(_, p)| p)
    }
}

fn worker(shared: &Arc<Shared>, slot: &Slot) {
    while let Some(node) = shared.next_job() {
        *lock(&slot.current) = Some((Arc::clone(&node), Instant::now()));
        shared.process(&node);
        *lock(&slot.current) = None;
        if slot.retired.load(Ordering::Acquire) {
            break;
        }
    }
}

/// Accounts for a worker's exit, including by panic: a dead worker would otherwise leave its
/// directory unfinished forever and the scan would never complete.
struct WorkerExit(Arc<Shared>);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.worker_panicked.store(true, Ordering::Release);
        }
        self.0.live_workers.fetch_sub(1, Ordering::AcqRel);
        self.0.done_cv.notify_all();
    }
}

/// Stops the workers and closes the sink if the coordinator unwinds (e.g. a panicking sink
/// on the progress path), so no thread outlives the scan spinning forever.
struct StopOnDrop<'a>(&'a Shared);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Release);
        self.0.closed.store(true, Ordering::Release);
        self.0.queue_cv.notify_all();
    }
}

fn spawn_worker(shared: &Arc<Shared>) -> std::io::Result<()> {
    let slot = Arc::new(Slot::default());
    lock(&shared.slots).push(Arc::clone(&slot));
    shared.live_workers.fetch_add(1, Ordering::AcqRel);
    let s = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("scull-scan".into())
        .spawn(move || {
            let exit = WorkerExit(s);
            worker(&exit.0, &slot);
        });
    // On failure the closure, and with it the `WorkerExit` that would undo the increment, was
    // never constructed.
    spawned.map(drop).inspect_err(|_| {
        shared.live_workers.fetch_sub(1, Ordering::AcqRel);
    })
}

/// Scan the directory tree at `root`.
///
/// Lists every directory below `root` in parallel through `platform`, reading each entry's
/// no-follow metadata, and reports results to `sink` as [`ScanEvent`]s while it runs. Returns
/// when the tree is done or `cancel` is triggered (then within a few tens of milliseconds,
/// with [`ScanStatus::Cancelled`] and partial totals).
///
/// Safety properties (SAFE-04, SAFE-18, PRD §18.2):
/// * symlinks, junctions and reparse points are recorded with their own metadata and never
///   followed, even as the root;
/// * FIFOs, sockets and devices are identified from metadata and never opened;
/// * directories on another volume, below the depth limit, or identical to an ancestor are
///   counted and reported as [`ScanEvent::Boundary`], not descended;
/// * errors below the root are reported as [`ScanEvent::Issue`] and never stop the scan;
/// * read-only: nothing is created, modified or persisted.
///
/// # Errors
///
/// Only if the scan cannot start: the root's metadata is unreadable, the root is not a real
/// directory, or no worker thread can be spawned.
pub fn scan(
    platform: Arc<dyn Platform>,
    root: &Path,
    options: &ScanOptions,
    cancel: &CancelToken,
    sink: impl EventSink,
) -> Result<ScanOutcome, ScanError> {
    let started = Instant::now();
    let root_md = platform.metadata(root).map_err(ScanError::Root)?;
    if root_md.kind != EntryKind::Dir {
        return Err(ScanError::RootNotADirectory {
            path: root.to_path_buf(),
            found: root_md.kind,
        });
    }
    let threads = options.effective_threads();
    let mut root_totals = Totals::default();
    root_totals.add_size(&root_md);

    let shared = Arc::new(Shared {
        platform,
        sink: Box::new(sink),
        cancel: cancel.clone(),
        opts: options.clone(),
        root_volume: root_md.identity.map(|i| i.volume),
        queue: Mutex::new(vec![Arc::new(DirNode::new(
            root.to_path_buf(),
            0,
            &root_md,
            None,
        ))]),
        queue_cv: Condvar::new(),
        stop: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        done: Mutex::new(None),
        done_cv: Condvar::new(),
        live_workers: AtomicUsize::new(0),
        worker_panicked: AtomicBool::new(false),
        totals: Mutex::new(root_totals),
        slots: Mutex::new(Vec::new()),
    });

    let _stop_on_unwind = StopOnDrop(&shared);
    shared.emit(ScanEvent::Started {
        root: root.to_path_buf(),
        threads,
    });
    for _ in 0..threads {
        if let Err(e) = spawn_worker(&shared) {
            if shared.live_workers.load(Ordering::Acquire) == 0 {
                shutdown(&shared);
                return Err(ScanError::Spawn(e));
            }
            break; // run with fewer workers
        }
    }

    let mut last_progress = Instant::now();
    let mut replacements = 0;
    let (status, root_summary) = loop {
        {
            let done = lock(&shared.done);
            let done = if done.is_none() {
                shared
                    .done_cv
                    .wait_timeout(done, TICK)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            } else {
                done
            };
            if let Some(summary) = done.as_ref() {
                break (ScanStatus::Completed, Some(summary.clone()));
            }
        }
        if cancel.is_cancelled() {
            break (ScanStatus::Cancelled, None);
        }
        if shared.worker_panicked.load(Ordering::Acquire) {
            shutdown(&shared);
            panic!("a scan worker panicked (in the event sink or the platform)");
        }
        if let Some(timeout) = options.dir_timeout {
            shared.watchdog(timeout, &mut replacements);
        }
        if last_progress.elapsed() >= options.progress_interval {
            last_progress = Instant::now();
            shared.emit(ScanEvent::Progress(Progress {
                totals: *lock(&shared.totals),
                current_dir: shared.current_dir(),
                elapsed: started.elapsed(),
            }));
        }
    };

    shutdown(&shared);
    let totals = *lock(&shared.totals);
    shared.emit(ScanEvent::Progress(Progress {
        totals,
        current_dir: None,
        elapsed: started.elapsed(),
    }));
    shared.closed.store(true, Ordering::Release);
    Ok(ScanOutcome {
        status,
        totals,
        root: root_summary,
        elapsed: started.elapsed(),
        threads,
    })
}

/// Stop the workers and wait (briefly) for them to exit.
fn shutdown(shared: &Shared) {
    shared.stop.store(true, Ordering::Release);
    shared.queue_cv.notify_all();
    lock(&shared.queue).clear();
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    let mut guard = lock(&shared.done);
    while shared.live_workers.load(Ordering::Acquire) > 0 {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        guard = shared
            .done_cv
            .wait_timeout(guard, (deadline - now).min(TICK))
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}
