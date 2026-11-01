//! Options, events and results of a scan.

use scull_core::{Confidence, ErrorKind, FileIdentity, Reclaimable, ScullError};
use scull_platform::{CloudState, EntryKind, Metadata, PlatformError};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// How a scan runs. Start from [`ScanOptions::default`] and change fields.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScanOptions {
    /// Worker threads. `0` (default) picks the number of available CPUs, capped at
    /// [`ScanOptions::MAX_AUTO_THREADS`].
    pub threads: usize,
    /// Deepest directory level that is listed; the root is depth 0. Directories below it are
    /// counted with their own metadata and reported as a [`BoundaryReason::MaxDepth`]
    /// boundary, but not listed. `None` (default) means unlimited.
    pub max_depth: Option<usize>,
    /// Stay on the root's volume (default `true`): a directory whose identity volume differs
    /// from the root's is reported as a [`BoundaryReason::OtherVolume`] boundary and not
    /// descended. Has no effect where the platform reports no identity (see
    /// [`scull_platform::Capabilities::file_identity`]).
    pub same_volume: bool,
    /// Emit a [`ScanEvent::Listing`] with every entry of every listed directory (default
    /// `false`). Detectors need it; size-only scans don't.
    pub emit_listings: bool,
    /// Minimum time between [`ScanEvent::Progress`] events (default 200 ms).
    pub progress_interval: Duration,
    /// Give up on a single directory listing, or on the metadata of a single entry, that
    /// takes longer than this (default 60 s), e.g. on a stalled network or FUSE mount (T-53).
    ///
    /// * One entry whose metadata call hangs (a frozen mount point inside a healthy
    ///   directory) costs only itself: it is reported as an [`IssueKind::TimedOut`] issue for
    ///   that entry, the directory is listed again without it, and its siblings are counted,
    ///   listed and descended as usual. At most
    ///   [`ScanOptions::MAX_ENTRY_TIMEOUTS_PER_DIR`] entries of one directory are skipped
    ///   this way; one more gives up the whole directory.
    /// * A listing that hangs outside any one entry's metadata (the directory itself is on
    ///   the stalled mount) gives up the directory: it is reported as an
    ///   [`IssueKind::TimedOut`] issue and its subtree is marked incomplete.
    ///
    /// Either way a replacement worker keeps the scan going. `None` disables the watchdog.
    pub dir_timeout: Option<Duration>,
    /// Mount points that are never touched, not even with `lstat` (default: none). Each one
    /// met in a listing is counted as an entry of unknown size and reported as a
    /// [`BoundaryReason::OtherVolume`] boundary straight from this list, without a system
    /// call, so a stale network or FUSE mount can't stall the listing of the directory that
    /// holds it (T-53). Callers pass the network mounts of the OS mount table, in the same
    /// (canonical) form as the scan root. Matching costs one lookup per listed directory, and
    /// a name comparison per entry only in directories that hold one of these mount points.
    pub skip_mounts: Vec<PathBuf>,
}

impl ScanOptions {
    /// Upper bound for the automatic thread count: beyond this the filesystem, not the CPU,
    /// is the bottleneck.
    pub const MAX_AUTO_THREADS: usize = 16;

    /// Entries of one directory whose metadata may time out (each skipped and reported on its
    /// own, see [`ScanOptions::dir_timeout`]) before the whole directory is given up.
    pub const MAX_ENTRY_TIMEOUTS_PER_DIR: usize = 4;

    /// The thread count a scan with these options uses.
    pub fn effective_threads(&self) -> usize {
        if self.threads > 0 {
            self.threads
        } else {
            std::thread::available_parallelism()
                .map_or(4, usize::from)
                .min(Self::MAX_AUTO_THREADS)
        }
    }
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            threads: 0,
            max_depth: None,
            same_volume: true,
            emit_listings: false,
            progress_interval: Duration::from_millis(200),
            dir_timeout: Some(Duration::from_secs(60)),
            skip_mounts: Vec::new(),
        }
    }
}

/// Receives [`ScanEvent`]s. Called concurrently from worker threads, so it must be cheap and
/// must not block for long; forward to a channel if the consumer is slow.
///
/// Any `Fn(ScanEvent) + Send + Sync + 'static` closure is a sink, e.g.
/// `move |e| { let _ = tx.send(e); }` for an `std::sync::mpsc::Sender`.
pub trait EventSink: Send + Sync + 'static {
    /// Handle one event.
    fn event(&self, event: ScanEvent);
}

impl<F> EventSink for F
where
    F: Fn(ScanEvent) + Send + Sync + 'static,
{
    fn event(&self, event: ScanEvent) {
        self(event)
    }
}

/// Discards every event; for callers that only need the [`ScanOutcome`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl EventSink for NullSink {
    fn event(&self, _event: ScanEvent) {}
}

/// Something the scan reports while it runs.
///
/// Ordering guarantees: [`ScanEvent::Started`] comes first; a directory's
/// [`ScanEvent::Listing`] comes before the listings of its subdirectories; a directory's
/// [`ScanEvent::DirDone`] comes after the `DirDone` of every subdirectory (bottom-up), so a
/// consumer can build subtree sizes without holding the tree. Events of unrelated directories
/// interleave in any order. No event is delivered after [`crate::scan`] returns (unless a
/// worker was stuck in an OS call for longer than the shutdown grace period; such a worker
/// delivers nothing once it notices).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanEvent {
    /// The scan started.
    Started {
        /// The scan root, as given.
        root: PathBuf,
        /// Worker threads in use.
        threads: usize,
    },
    /// Throttled progress (see [`ScanOptions::progress_interval`]). One final progress event
    /// is sent just before [`crate::scan`] returns.
    Progress(Progress),
    /// The entries of one directory, top-down. Only with [`ScanOptions::emit_listings`].
    Listing(DirListing),
    /// A directory and its whole subtree are done, bottom-up.
    DirDone(DirSummary),
    /// A directory that was deliberately not descended.
    Boundary(Boundary),
    /// Something could not be read. Never fatal; the affected directory and its ancestors are
    /// marked incomplete ([`DirSummary::complete`]).
    Issue(ScanIssue),
}

/// Running totals for a progress display.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Progress {
    /// Totals so far.
    pub totals: Totals,
    /// A directory a worker is listing right now, if any.
    pub current_dir: Option<PathBuf>,
    /// Time since the scan started.
    pub elapsed: Duration,
}

/// Entry and byte counts. Sizes include the root directory itself and every entry's own
/// metadata (links and special files count their own size, never a target's).
///
/// Counts (`entries`, `files`, ...) are per path. Sizes are per *file*: a file with several
/// hard links is counted once, however many of its links were seen (PHY-03, deduplicated by
/// [`FileIdentity`]). Entries whose identity the platform can't report are never deduplicated
/// (see [`DirSummary::reclaim_confidence`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Totals {
    /// Entries seen in listings (the root itself is not counted).
    pub entries: u64,
    /// Regular files.
    pub files: u64,
    /// Directories, including ones not descended (boundaries).
    pub dirs: u64,
    /// Symlinks, junctions and other reparse points (never followed).
    pub links: u64,
    /// FIFOs, sockets, devices and unknown kinds (never opened, SAFE-18).
    pub specials: u64,
    /// Sum of logical sizes, each hard-linked file counted once.
    pub logical: u64,
    /// Sum of allocated sizes where known (see [`Totals::allocated_unknown`]), each
    /// hard-linked file counted once.
    pub allocated: u64,
    /// Entries (paths) whose allocated size the platform could not report.
    pub allocated_unknown: u64,
    /// Entries (paths, not directories) with a link count above 1.
    pub multi_linked: u64,
    /// Paths whose bytes were not counted because another link to the same file was already
    /// counted (so `multi_linked - hardlinks_deduped` distinct multi-linked files were seen).
    pub hardlinks_deduped: u64,
    /// Errors reported as [`ScanEvent::Issue`].
    pub issues: u64,
    /// Directories reported as [`ScanEvent::Boundary`].
    pub boundaries: u64,
    /// Entries (paths of any kind) that are cloud-sync placeholders: content not on this disk
    /// (see [`is_cloud_placeholder`]). Placeholder directories are also counted in
    /// [`Totals::boundaries`], since they are never listed.
    pub cloud_placeholders: u64,
}

impl Totals {
    /// Count an entry (kind counters, unknown allocation, link flag) without its bytes.
    pub(crate) fn add_counts(&mut self, md: &Metadata) {
        self.count_kind(md.kind);
        if md.allocated_size.is_none() {
            self.allocated_unknown += 1;
        }
        if md.nlink.is_some_and(|n| n > 1) && md.kind != EntryKind::Dir {
            self.multi_linked += 1;
        }
        if is_cloud_placeholder(md) {
            self.cloud_placeholders += 1;
        }
    }

    /// Count an entry that was deliberately not `lstat`ed (a mount point from
    /// [`ScanOptions::skip_mounts`]): its kind comes from the listing, its size is unknown.
    pub(crate) fn add_unstatted(&mut self, kind: EntryKind) {
        self.count_kind(kind);
        self.allocated_unknown += 1;
    }

    /// One more entry, in the counter of its kind.
    fn count_kind(&mut self, kind: EntryKind) {
        self.entries += 1;
        match kind {
            EntryKind::File => self.files += 1,
            EntryKind::Dir => self.dirs += 1,
            k if k.is_link() => self.links += 1,
            _ => self.specials += 1,
        }
    }

    /// Add bytes, no counters.
    pub(crate) fn add_bytes(&mut self, logical: u64, allocated: Option<u64>) {
        self.logical = self.logical.saturating_add(logical);
        if let Some(a) = allocated {
            self.allocated = self.allocated.saturating_add(a);
        }
    }

    /// Take back bytes added for a link whose file was already counted.
    pub(crate) fn remove_bytes(&mut self, logical: u64, allocated: Option<u64>) {
        self.logical = self.logical.saturating_sub(logical);
        if let Some(a) = allocated {
            self.allocated = self.allocated.saturating_sub(a);
        }
    }

    /// Add a directory node's own size (it is counted as an entry by its parent).
    pub(crate) fn add_size(&mut self, md: &Metadata) {
        self.add_bytes(md.logical_size, md.allocated_size);
        if md.allocated_size.is_none() {
            self.allocated_unknown += 1;
        }
    }

    pub(crate) fn merge(&mut self, o: &Totals) {
        self.entries += o.entries;
        self.files += o.files;
        self.dirs += o.dirs;
        self.links += o.links;
        self.specials += o.specials;
        self.logical = self.logical.saturating_add(o.logical);
        self.allocated = self.allocated.saturating_add(o.allocated);
        self.allocated_unknown += o.allocated_unknown;
        self.multi_linked += o.multi_linked;
        self.hardlinks_deduped += o.hardlinks_deduped;
        self.issues += o.issues;
        self.boundaries += o.boundaries;
        self.cloud_placeholders += o.cloud_placeholders;
    }
}

/// Whether an entry's own metadata marks it as a cloud-sync placeholder whose content is not
/// on this disk (Windows recall / offline attributes, macOS `SF_DATALESS`; see
/// [`CloudState::from_metadata`]). Reading such a file, or listing such a directory, makes the
/// sync provider download it (PLT-04, T-52), so the scanner never lists one and callers must
/// never read one. Decided from metadata bits alone: no extra system call.
pub fn is_cloud_placeholder(md: &Metadata) -> bool {
    CloudState::from_metadata(md).is_some_and(|c| c.placeholder)
}

/// One entry of a directory listing, with its own no-follow metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct EntryInfo {
    /// Name within the directory.
    pub name: OsString,
    /// No-follow kind (from `lstat`, which wins over the listing's type hint).
    pub kind: EntryKind,
    /// Path-independent identity, if the platform provides it.
    pub identity: Option<FileIdentity>,
    /// Logical size of the entry itself (for a directory: the directory node, not its
    /// contents; see [`ScanEvent::DirDone`] for subtree sizes).
    pub logical: u64,
    /// Allocated size of the entry itself, if known.
    pub allocated: Option<u64>,
    /// Hard-link count, if known (used with `identity` to attribute bytes once).
    pub nlink: Option<u64>,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: Option<i64>,
    /// A cloud-sync placeholder: its content is not on this disk (see
    /// [`is_cloud_placeholder`]). Never read it; a placeholder directory was not listed and is
    /// reported as a [`BoundaryReason::CloudPlaceholder`] boundary. Such entries are at least
    /// S2 (PRD §8.3).
    pub cloud_placeholder: bool,
}

impl EntryInfo {
    /// An entry that was deliberately not `lstat`ed (see [`ScanOptions::skip_mounts`]): kind
    /// from the listing, everything else unknown.
    pub(crate) fn unstatted(name: OsString, kind: EntryKind) -> Self {
        Self {
            name,
            kind,
            identity: None,
            logical: 0,
            allocated: None,
            nlink: None,
            mtime: None,
            cloud_placeholder: false,
        }
    }

    pub(crate) fn new(name: OsString, md: &Metadata) -> Self {
        Self {
            name,
            kind: md.kind,
            identity: md.identity,
            logical: md.logical_size,
            allocated: md.allocated_size,
            nlink: md.nlink,
            mtime: md.mtime_unix,
            cloud_placeholder: is_cloud_placeholder(md),
        }
    }
}

/// All readable entries of one directory, in OS order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DirListing {
    /// The directory.
    pub path: PathBuf,
    /// Depth below the root (root = 0).
    pub depth: usize,
    /// The directory's identity, if known.
    pub identity: Option<FileIdentity>,
    /// Its entries. Entries whose metadata could not be read are missing here and reported as
    /// [`ScanEvent::Issue`].
    pub entries: Vec<EntryInfo>,
}

/// A finished directory with the totals of its whole subtree (itself included).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DirSummary {
    /// The directory.
    pub path: PathBuf,
    /// Depth below the root (root = 0).
    pub depth: usize,
    /// The directory's identity, if known.
    pub identity: Option<FileIdentity>,
    /// The directory's own modification time.
    pub mtime: Option<i64>,
    /// Newest modification time of anything in the subtree, the directory itself included.
    pub newest_mtime: Option<i64>,
    /// Subtree totals. `entries` counts everything below this directory; sizes include the
    /// directory itself. A file hard-linked several times inside the subtree is counted once.
    pub totals: Totals,
    /// `false` if any part of the subtree could not be read (errors or timeouts). Boundaries
    /// (depth limit, other volume) don't make a subtree incomplete; they are counted in
    /// [`Totals::boundaries`].
    pub complete: bool,
    /// Allocated bytes that removing this subtree would free: [`Totals::allocated`] minus
    /// [`DirSummary::shared_allocated`]. A hard-linked file counts here only if *all* of its
    /// links are inside the subtree.
    pub unique_allocated: u64,
    /// Allocated bytes of files that also have links outside this subtree (e.g. a pnpm
    /// `node_modules` hard-linked into a store elsewhere): removing the subtree frees ~0 for
    /// them while the other links remain. Also includes multi-linked files whose identity the
    /// platform could not report, since their other links can't be located.
    /// `unique_allocated + shared_allocated == totals.allocated`.
    pub shared_allocated: u64,
    /// How far to trust [`DirSummary::unique_allocated`] as a reclaim estimate (hard links
    /// only): [`Confidence::High`] when every byte could be attributed (no hard links, or
    /// every link's identity known); [`Confidence::Low`] when some multi-linked file had no
    /// identity (nothing is deduplicated and its bytes are reported as shared), a link count
    /// or allocated size was unknown, part of the subtree could not be read
    /// ([`DirSummary::complete`] is `false`), or the subtree holds a cloud placeholder
    /// directory ([`BoundaryReason::CloudPlaceholder`]) whose contents are unknown.
    pub reclaim_confidence: Confidence,
}

impl DirSummary {
    /// The subtree's physically reclaimable bytes as a core [`Reclaimable`] (PHY-01): bytes
    /// freed if it were removed now, bytes pinned by links elsewhere, and the confidence.
    /// Accounts for hard links only; clones and snapshots are judged elsewhere (PRD §9).
    pub fn reclaimable(&self) -> Reclaimable {
        Reclaimable {
            bytes: self.unique_allocated,
            confidence: self.reclaim_confidence,
            shared_bytes: self.shared_allocated,
        }
    }
}

/// Why a directory was not descended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BoundaryReason {
    /// Below [`ScanOptions::max_depth`].
    MaxDepth,
    /// On a different volume than the root (a mount point; full mount detection is #33), or
    /// one of [`ScanOptions::skip_mounts`] (then never `lstat`ed and without identity).
    OtherVolume,
    /// Same identity as one of its ancestors (e.g. a bind mount of a parent): descending
    /// would loop forever (T-53, FX-FS-02).
    Loop,
    /// A cloud-sync placeholder directory (macOS dataless iCloud / File Provider folder,
    /// Windows recall-on-open or offline directory; see [`is_cloud_placeholder`]). Listing it
    /// would make the sync provider download it (PLT-04), so it is never listed: only its own
    /// metadata is counted, and the summaries containing it get [`Confidence::Low`]
    /// ([`DirSummary::reclaim_confidence`]).
    CloudPlaceholder,
}

/// A directory that was counted but not descended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Boundary {
    /// The directory.
    pub path: PathBuf,
    /// Its depth below the root.
    pub depth: usize,
    /// Its identity, if known.
    pub identity: Option<FileIdentity>,
    /// Why it was not descended.
    pub reason: BoundaryReason,
}

/// What a [`ScanIssue`] was doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IssueOp {
    /// Opening or listing a directory.
    ListDir,
    /// Reading one entry of a listing.
    ReadEntry,
    /// Reading an entry's metadata.
    Metadata,
}

/// Classification of a [`ScanIssue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IssueKind {
    /// Access denied (permissions, macOS TCC / Full Disk Access).
    PermissionDenied,
    /// The entry vanished during the scan.
    NotFound,
    /// A listing, or one entry's metadata, exceeded [`ScanOptions::dir_timeout`]
    /// ([`ScanIssue::op`] says which).
    TimedOut,
    /// Any other error.
    Other,
}

/// A non-fatal error. Reported, never hidden (PRD §18.2, PLT-01).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScanIssue {
    /// The path involved.
    pub path: PathBuf,
    /// What was being done.
    pub op: IssueOp,
    /// Classification.
    pub kind: IssueKind,
    /// Human-readable description.
    pub message: String,
}

impl ScanIssue {
    pub(crate) fn from_platform(op: IssueOp, err: &PlatformError) -> Self {
        let kind = if err.is_permission_denied() {
            IssueKind::PermissionDenied
        } else if err.is_not_found() {
            IssueKind::NotFound
        } else {
            IssueKind::Other
        };
        Self {
            path: err.path().to_path_buf(),
            op,
            kind,
            message: err.to_string(),
        }
    }

    pub(crate) fn timed_out(path: PathBuf, after: Duration) -> Self {
        let message = format!(
            "listing {} took longer than {} ms; skipped",
            path.display(),
            after.as_millis()
        );
        Self {
            path,
            op: IssueOp::ListDir,
            kind: IssueKind::TimedOut,
            message,
        }
    }

    /// One entry's metadata call hung: the entry is skipped, its siblings are not.
    pub(crate) fn entry_timed_out(path: PathBuf, after: Duration) -> Self {
        let message = format!(
            "reading the metadata of {} took longer than {} ms (a stalled mount?); skipped, \
             the rest of its directory was scanned",
            path.display(),
            after.as_millis()
        );
        Self {
            path,
            op: IssueOp::Metadata,
            kind: IssueKind::TimedOut,
            message,
        }
    }
}

/// How a scan ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ScanStatus {
    /// Every reachable directory was processed (some may have reported issues).
    Completed,
    /// Stopped by a [`crate::CancelToken`]. Totals are partial; nothing was persisted.
    Cancelled,
}

/// The result of [`crate::scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScanOutcome {
    /// Completed or cancelled.
    pub status: ScanStatus,
    /// Totals of everything processed. For a cancelled scan this is partial.
    pub totals: Totals,
    /// The root's summary; `None` if the scan was cancelled before the root finished.
    pub root: Option<DirSummary>,
    /// Wall-clock duration.
    pub elapsed: Duration,
    /// Worker threads used.
    pub threads: usize,
    /// Most file identities held at once for scan-wide hard-link deduplication. Only files
    /// with a link count above 1 are tracked, and each is dropped as soon as all its links
    /// were seen, so this stays far below the entry count (0 without hard links).
    pub peak_link_identities: usize,
}

impl ScanOutcome {
    /// Whether the scan was cancelled (its totals are partial).
    pub fn is_cancelled(&self) -> bool {
        self.status == ScanStatus::Cancelled
    }
}

/// A scan that could not start. Errors below the root are [`ScanEvent::Issue`]s instead.
#[derive(Debug)]
#[non_exhaustive]
pub enum ScanError {
    /// The root's metadata could not be read.
    Root(PlatformError),
    /// The root is not a real directory. Symlinks and junctions are never followed, even as
    /// the root (SAFE-04); resolve them explicitly before scanning if that is intended.
    RootNotADirectory {
        /// The root.
        path: PathBuf,
        /// What it is instead.
        found: EntryKind,
    },
    /// The root is a cloud-sync placeholder directory: listing it would download it
    /// (PLT-04), so it is refused rather than scanned (see [`is_cloud_placeholder`]).
    RootCloudPlaceholder {
        /// The root.
        path: PathBuf,
    },
    /// A worker thread could not be started.
    Spawn(std::io::Error),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanError::Root(e) => write!(f, "cannot scan: {e}"),
            ScanError::RootNotADirectory { path, found } => write!(
                f,
                "cannot scan {}: not a directory (found {found}; links are never followed)",
                path.display()
            ),
            ScanError::RootCloudPlaceholder { path } => write!(
                f,
                "cannot scan {}: it is a cloud-sync placeholder (online-only); listing it would \
                 download it. Make it available offline in the sync client first, or scan a \
                 different folder",
                path.display()
            ),
            ScanError::Spawn(e) => write!(f, "cannot start scan worker: {e}"),
        }
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ScanError::Root(e) => Some(e),
            ScanError::Spawn(e) => Some(e),
            ScanError::RootNotADirectory { .. } | ScanError::RootCloudPlaceholder { .. } => None,
        }
    }
}

impl From<&ScanError> for ScullError {
    fn from(e: &ScanError) -> Self {
        let kind = match e {
            ScanError::Root(p) if p.is_not_found() => ErrorKind::NotFound,
            ScanError::Root(p) if p.is_permission_denied() => ErrorKind::PermissionDenied,
            ScanError::Root(_) => ErrorKind::Io,
            ScanError::RootNotADirectory { .. } => ErrorKind::Usage,
            ScanError::RootCloudPlaceholder { .. } => ErrorKind::SafetyBlocked,
            ScanError::Spawn(_) => ErrorKind::Internal,
        };
        ScullError::new(kind, e.to_string())
    }
}
