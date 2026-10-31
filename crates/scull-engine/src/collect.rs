//! The scanner event sink: detection during the walk, plus everything the classification step
//! needs afterwards (markers, Git roots, mount boundaries, subtree sizes of interesting
//! directories). Memory stays bounded: only directories that matter are remembered.

use crate::paths::{inside_git_dir, is_within};
use crate::reader::FsConfigReader;
use crate::report::{Issue, IssueKind};
use scull_core::FsPath;
use scull_core::config::{REPO_CONFIG_FILE, REPO_PROTECT_MARKER};
use scull_detectors::{DetectContext, DetectedArtifact, Detectors, SharedTargetDirs};
use scull_platform::{EntryKind, Platform};
use scull_scanner::{
    BoundaryReason, CancelToken, DirListing, DirSummary, IssueKind as ScanIssueKind, ScanEvent,
    ScanOptions, ScanOutcome,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Issues listed in a report; more are only counted.
pub const MAX_ISSUES: usize = 200;

/// Project marker files and the ecosystem each implies.
pub const PROJECT_MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "rust"),
    ("package.json", "node"),
    ("pyproject.toml", "python"),
    ("go.mod", "go"),
    ("pom.xml", "java"),
    ("build.gradle", "java"),
    ("build.gradle.kts", "java"),
    ("Gemfile", "ruby"),
    ("composer.json", "php"),
    ("Package.swift", "swift"),
    ("CMakeLists.txt", "cmake"),
    ("pubspec.yaml", "dart"),
    ("mix.exs", "elixir"),
    ("deno.json", "deno"),
];

/// Progress of a running operation, for a progress line on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineProgress {
    /// Walking a root.
    Scanning {
        /// The root being walked.
        root: PathBuf,
        /// Entries counted so far in this root.
        entries: u64,
        /// Allocated bytes counted so far in this root.
        allocated: u64,
        /// Directory being listed.
        current: Option<PathBuf>,
    },
    /// Post-walk analysis (Git, classification).
    Analyzing {
        /// What is being analysed.
        what: String,
    },
}

/// Callback receiving [`EngineProgress`].
pub type ProgressFn = Arc<dyn Fn(EngineProgress) + Send + Sync>;

/// Everything remembered from the walk.
#[derive(Default)]
pub(crate) struct State {
    /// Detected artifacts by path.
    pub artifacts: BTreeMap<PathBuf, DetectedArtifact>,
    /// Shared Cargo target directories declared by projects seen.
    pub shared_targets: SharedTargetDirs,
    /// Subtree summaries of interesting directories.
    pub summaries: HashMap<PathBuf, DirSummary>,
    /// Directories whose summary we want (projects, Git roots, declared target dirs).
    pub interesting: HashSet<PathBuf>,
    /// Directories holding a `.scull-protect` marker.
    pub protect_markers: BTreeSet<PathBuf>,
    /// Directories holding a `.scull.toml`.
    pub repo_configs: BTreeSet<PathBuf>,
    /// Directories holding a `.git` entry (work trees, main or linked, and submodules).
    pub git_roots: BTreeSet<PathBuf>,
    /// Bare repositories (`HEAD`, `objects/`, `refs/` side by side).
    pub bare_repos: BTreeSet<PathBuf>,
    /// Project directories and the marker files found in each.
    pub projects: BTreeMap<PathBuf, Vec<String>>,
    /// Directories containing a reparse point (Windows cloud placeholders and similar).
    pub reparse_dirs: BTreeSet<PathBuf>,
    /// Directories on another volume (counted, not descended).
    pub other_volume: Vec<PathBuf>,
    /// Problems worth reporting.
    pub issues: Vec<Issue>,
    /// Issues not listed.
    pub issues_truncated: u64,
    /// Read errors (not boundaries): any makes a scan partial.
    pub read_errors: u64,
    /// Entries counted over all roots.
    pub entries: u64,
}

impl State {
    pub fn issue(&mut self, path: &Path, kind: IssueKind, message: String) {
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(Issue {
                path: FsPath::from_path(path),
                kind,
                message,
            });
        } else {
            self.issues_truncated += 1;
        }
    }

    /// The artifact `dir` lies strictly inside, if any.
    fn inside_artifact(&self, dir: &Path) -> bool {
        dir.ancestors()
            .skip(1)
            .any(|a| self.artifacts.contains_key(a))
    }
}

/// Shared between scanner worker threads.
pub(crate) struct Collector {
    pub platform: Arc<dyn Platform>,
    pub detectors: Option<Arc<Detectors>>,
    pub state: Mutex<State>,
    pub progress: ProgressFn,
}

/// Lock, recovering from a poisoned mutex (a panicking detector must not lose the scan).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Collector {
    pub fn new(
        platform: Arc<dyn Platform>,
        detectors: Option<Arc<Detectors>>,
        progress: ProgressFn,
    ) -> Arc<Self> {
        Arc::new(Self {
            platform,
            detectors,
            state: Mutex::new(State::default()),
            progress,
        })
    }

    /// Walk `root` (already canonical), feeding every event through [`Collector::event`].
    pub fn walk(
        self: &Arc<Self>,
        root: &Path,
        threads: usize,
        cancel: &CancelToken,
    ) -> Result<ScanOutcome, scull_scanner::ScanError> {
        self.note_ancestors(root);
        let mut opts = ScanOptions::default();
        opts.emit_listings = true;
        opts.threads = threads;
        let me = Arc::clone(self);
        let root_buf = root.to_path_buf();
        let outcome =
            scull_scanner::scan(Arc::clone(&self.platform), root, &opts, cancel, move |e| {
                me.event(&root_buf, e)
            })?;
        let mut st = lock(&self.state);
        st.entries = st.entries.saturating_add(outcome.totals.entries);
        if let Some(s) = &outcome.root {
            st.summaries.insert(s.path.clone(), s.clone());
        }
        Ok(outcome)
    }

    /// Markers in the directories above a root: they are not listed by the walk, but a
    /// `.scull-protect` or a repository above the root still applies inside it.
    fn note_ancestors(&self, root: &Path) {
        let exists = |p: PathBuf| self.platform.metadata(&p).is_ok();
        let mut st = lock(&self.state);
        for a in root.ancestors().skip(1) {
            if exists(a.join(REPO_PROTECT_MARKER)) {
                st.protect_markers.insert(a.to_path_buf());
            }
            if exists(a.join(REPO_CONFIG_FILE)) {
                st.repo_configs.insert(a.to_path_buf());
            }
            if exists(a.join(".git")) {
                st.git_roots.insert(a.to_path_buf());
            }
        }
    }

    fn event(&self, root: &Path, event: ScanEvent) {
        match event {
            ScanEvent::Progress(p) => (self.progress)(EngineProgress::Scanning {
                root: root.to_path_buf(),
                entries: p.totals.entries,
                allocated: p.totals.allocated,
                current: p.current_dir,
            }),
            ScanEvent::Listing(l) => self.listing(root, l),
            ScanEvent::DirDone(s) => {
                let mut st = lock(&self.state);
                if st.artifacts.contains_key(&s.path) || st.interesting.contains(&s.path) {
                    st.summaries.insert(s.path.clone(), s);
                }
            }
            ScanEvent::Boundary(b) => {
                let mut st = lock(&self.state);
                let (kind, what) = match b.reason {
                    BoundaryReason::OtherVolume => {
                        st.other_volume.push(b.path.clone());
                        (
                            IssueKind::OtherVolume,
                            "on another volume; counted, not descended",
                        )
                    }
                    BoundaryReason::Loop => (IssueKind::Loop, "directory loop; not descended"),
                    _ => (IssueKind::MaxDepth, "depth limit reached; not descended"),
                };
                st.issue(&b.path, kind, what.to_owned());
            }
            ScanEvent::Issue(i) => {
                let kind = match i.kind {
                    ScanIssueKind::PermissionDenied => IssueKind::PermissionDenied,
                    ScanIssueKind::NotFound => IssueKind::NotFound,
                    ScanIssueKind::TimedOut => IssueKind::TimedOut,
                    _ => IssueKind::ReadError,
                };
                let mut st = lock(&self.state);
                st.read_errors += 1;
                st.issue(&i.path, kind, i.message);
            }
            _ => {}
        }
    }

    fn listing(&self, root: &Path, l: DirListing) {
        let dir = l.path;
        if inside_git_dir(&dir, root) {
            return;
        }
        let has = |name: &str| l.entries.iter().find(|e| e.name == name).map(|e| e.kind);
        let in_artifact = lock(&self.state).inside_artifact(&dir);

        // Detection happens here, on the walker thread, before the walker queues this
        // directory's children: so a child's listing always sees its parent's artifact and
        // nothing inside a detected artifact is detected again.
        let mut found = None;
        if !in_artifact && let Some(d) = &self.detectors {
            let reader = FsConfigReader;
            let ctx = DetectContext::new().with_config_reader(&reader);
            found = d.detect(&dir, self.platform.as_ref(), &ctx);
        }
        let mut declared = None;
        if !in_artifact && found.is_none() && has("Cargo.toml") == Some(EntryKind::File) {
            let reader = FsConfigReader;
            let ctx = DetectContext::new().with_config_reader(&reader);
            // Reads at most a few small Cargo config files; done under the lock because
            // `SharedTargetDirs` collects into one value.
            declared =
                lock(&self.state)
                    .shared_targets
                    .add_project(&dir, self.platform.as_ref(), &ctx);
        }

        let mut st = lock(&self.state);
        if let Some(a) = found {
            st.artifacts.insert(dir.clone(), a);
        }
        if let Some(t) = declared {
            st.interesting.insert(t);
        }
        if has(REPO_PROTECT_MARKER).is_some() {
            st.protect_markers.insert(dir.clone());
        }
        if has(REPO_CONFIG_FILE).is_some() {
            st.repo_configs.insert(dir.clone());
        }
        if l.entries
            .iter()
            .any(|e| e.kind.is_link() && e.kind != EntryKind::Symlink)
        {
            st.reparse_dirs.insert(dir.clone());
        }
        if has(".git").is_some() {
            st.git_roots.insert(dir.clone());
            st.interesting.insert(dir.clone());
        }
        if has("HEAD") == Some(EntryKind::File)
            && has("objects") == Some(EntryKind::Dir)
            && has("refs") == Some(EntryKind::Dir)
        {
            st.bare_repos.insert(dir.clone());
        }
        if !in_artifact && !st.artifacts.contains_key(&dir) {
            let markers: Vec<String> = PROJECT_MARKERS
                .iter()
                .filter(|(m, _)| has(m) == Some(EntryKind::File))
                .map(|(m, _)| (*m).to_owned())
                .collect();
            if !markers.is_empty() {
                st.projects.insert(dir.clone(), markers);
                st.interesting.insert(dir);
            }
        }
    }
}

/// Whether `path` lies inside one of `roots`.
pub(crate) fn within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| is_within(path, r))
}
