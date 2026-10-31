//! A read-only classification pipeline for end-to-end fixtures, composed from the real pieces:
//! `scull-scanner` (walk + sizes), `scull-detectors` (rules), `scull-git` (tracking),
//! `scull-safety` (protected roots, in-use, activity, rubric) and `scull-core` (repo policy).
//!
//! The shared engine crate (P1-CLI-02, #56) is not merged yet; until then this is the glue a
//! fixture runs through. When it lands, [`Pipeline::run`] should call it instead.
//!
//! Signals the pipeline cannot observe are set to their protective value, except two facts that
//! are ground truth for a fresh temporary fixture tree and say so: it is not inside a
//! cloud-synced folder, and it is not reached through a link (the scanner never follows one).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

pub mod worktrees;

use scull_core::config::{REPO_CONFIG_FILE, RepoPolicy};
use scull_core::{Candidate, ItemId, Tier};
use scull_detectors::rules::RuleSet;
use scull_detectors::{
    CallerSignals, ConfigReader, DetectContext, DetectedArtifact, Detectors, SharedTargetDirs,
};
use scull_fixtures::{Fixture, InvariantId, InvariantInputs, InvariantReport, PlanEntry};
use scull_git::{GitRunner, IgnoredMode, StatusOptions, UntrackedMode};
use scull_platform::{EntryKind, Native};
use scull_safety::{
    ActivityProbe, ActivityWindow, GitTracking, NativeActivityProbe, ProbeResult, Tri, check_in_use,
};
use scull_safety::{ProtectedSet, Scope, Source, TargetOs, default_protected};
use scull_scanner::{
    Boundary, BoundaryReason, CancelToken, DirListing, DirSummary, EstimateContext, ScanEvent,
    ScanIssue, ScanOptions, ScanOutcome, Snapshots, VolumeFs, estimate,
};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Reads small regular files with `std::fs` (test-only [`ConfigReader`]).
pub struct StdReader;

impl ConfigReader for StdReader {
    fn read_text(&self, path: &Path, max_bytes: u64) -> Option<String> {
        let md = fs::symlink_metadata(path).ok()?;
        if !md.is_file() || md.len() > max_bytes {
            return None;
        }
        let mut s = String::new();
        fs::File::open(path)
            .ok()?
            .take(max_bytes)
            .read_to_string(&mut s)
            .ok()?;
        Some(s)
    }
}

/// Where the in-use signal comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InUseSource {
    /// The real OS probe (`NativeActivityProbe`): process cwds, open files, tool locks.
    Native,
    /// Fixture ground truth: nothing has a handle or cwd inside a tree this test just built.
    /// Used where the native probe is not what the fixture is about, so that a permission-
    /// limited runner (e.g. macOS, other users' processes) doesn't turn every item into S3.
    IdleFixture,
}

struct IdleProbe;

impl ActivityProbe for IdleProbe {
    fn process_cwds_under(&self, _dir: &Path) -> ProbeResult {
        ProbeResult::not_in_use()
    }
    fn open_files_under(&self, _dir: &Path) -> ProbeResult {
        ProbeResult::not_in_use()
    }
    fn files_held_open(&self, _files: &[PathBuf]) -> ProbeResult {
        ProbeResult::not_in_use()
    }
}

/// One classified item.
#[derive(Debug, Clone)]
pub struct Item {
    /// The candidate, with sizes from the scan.
    pub candidate: Candidate,
    /// The detector's artifact it came from.
    pub artifact: DetectedArtifact,
    /// The caller signals it was classified with.
    pub caller: CallerSignals,
    /// The scanner's summary of the artifact directory (`None` if outside the scan root).
    pub summary: Option<DirSummary>,
}

/// Everything one pipeline run produced.
#[derive(Debug)]
pub struct Run {
    /// The scan root.
    pub root: PathBuf,
    /// Classified items, sorted by path.
    pub items: Vec<Item>,
    /// The scanner's outcome.
    pub outcome: ScanOutcome,
    /// Non-fatal scan issues.
    pub issues: Vec<ScanIssue>,
    /// Directories counted but not descended.
    pub boundaries: Vec<Boundary>,
    /// Directories the detection walk visited (it never enters a link or an artifact).
    pub visited: Vec<PathBuf>,
}

impl Run {
    /// Item paths relative to the root, `/`-separated, sorted.
    pub fn rel_paths(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|i| rel(&self.root, &i.artifact.path))
            .collect()
    }

    /// The one item at `root/rel`.
    pub fn at(&self, rel_path: &str) -> &Item {
        let hits: Vec<&Item> = self
            .items
            .iter()
            .filter(|i| rel(&self.root, &i.artifact.path) == rel_path)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "expected one item at {rel_path}, found {:?}",
            self.rel_paths()
        );
        hits[0]
    }

    /// The executable plan: every candidate with a mutating action.
    pub fn plan(&self) -> Vec<&Candidate> {
        self.items
            .iter()
            .map(|i| &i.candidate)
            .filter(|c| c.action.is_mutating())
            .collect()
    }

    /// Tier of every item by ID.
    pub fn tiers(&self) -> BTreeMap<ItemId, Tier> {
        self.items
            .iter()
            .map(|i| (i.candidate.id.clone(), i.candidate.tier))
            .collect()
    }
}

/// The pipeline configuration.
pub struct Pipeline {
    root: PathBuf,
    rules: Option<RuleSet>,
    explicit_targets: Vec<(PathBuf, String)>,
    user_protect: Vec<PathBuf>,
    home: Option<PathBuf>,
    in_use: InUseSource,
    window: ActivityWindow,
}

impl Pipeline {
    /// Scan `root` with the bundled rules, the default 24 h activity window and
    /// [`InUseSource::IdleFixture`] (fixtures about in-use switch to [`InUseSource::Native`]).
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            rules: None,
            explicit_targets: Vec::new(),
            user_protect: Vec::new(),
            home: None,
            in_use: InUseSource::IdleFixture,
            window: ActivityWindow::default(),
        }
    }

    /// Use `rules` instead of the bundled set.
    pub fn rules(mut self, rules: RuleSet) -> Self {
        self.rules = Some(rules);
        self
    }

    /// A caller-supplied Cargo target directory (e.g. `CARGO_TARGET_DIR`).
    pub fn cargo_target_dir(mut self, path: &Path, label: &str) -> Self {
        self.explicit_targets
            .push((path.to_path_buf(), label.to_owned()));
        self
    }

    /// A user-protected path (`scan.protect`).
    pub fn protect(mut self, path: &Path) -> Self {
        self.user_protect.push(path.to_path_buf());
        self
    }

    /// The home directory for the built-in protected roots (`~/.ssh`, `~/.gnupg`, ...): a
    /// stand-in inside the fixture. Without one, only the OS-wide built-in roots apply (tests
    /// never look at the real home).
    pub fn home(mut self, home: &Path) -> Self {
        self.home = Some(home.to_path_buf());
        self
    }

    /// Where the in-use signal comes from.
    pub fn in_use(mut self, source: InUseSource) -> Self {
        self.in_use = source;
        self
    }

    /// Run scan → detect → caller signals → classify. Read-only.
    pub fn run(&self) -> Run {
        let platform = Native::new();
        let (outcome, events) = scan(&self.root);
        let mut listings: HashMap<PathBuf, DirListing> = HashMap::new();
        let mut summaries: HashMap<PathBuf, DirSummary> = HashMap::new();
        let mut boundaries = Vec::new();
        let mut issues = Vec::new();
        for e in events {
            match e {
                ScanEvent::Listing(l) => {
                    listings.insert(l.path.clone(), l);
                }
                ScanEvent::DirDone(s) => {
                    summaries.insert(s.path.clone(), s);
                }
                ScanEvent::Boundary(b) => boundaries.push(b),
                ScanEvent::Issue(i) => issues.push(i),
                _ => {}
            }
        }

        // Detection: top-down over what the scanner listed, never into an artifact.
        let detectors = match &self.rules {
            Some(r) => Detectors::from_rules(r),
            None => Detectors::bundled().expect("bundled rules"),
        };
        let reader = StdReader;
        let ctx = DetectContext::new().with_config_reader(&reader);
        let mut shared = SharedTargetDirs::new();
        for (p, label) in &self.explicit_targets {
            assert!(shared.add_explicit(p, label.clone()), "explicit target dir");
        }
        let mut found = Vec::new();
        let mut visited = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            visited.push(dir.clone());
            if let Some(a) = detectors.detect(&dir, &platform, &ctx) {
                found.push(a);
                continue;
            }
            let Some(listing) = listings.get(&dir) else {
                continue; // a boundary or an unreadable directory: nothing to look into
            };
            let mut has_manifest = false;
            let mut children = Vec::new();
            for e in &listing.entries {
                match e.kind {
                    EntryKind::Dir => children.push(dir.join(&e.name)),
                    EntryKind::File if e.name == "Cargo.toml" => has_manifest = true,
                    _ => {}
                }
            }
            children.sort();
            stack.extend(children.into_iter().rev());
            if has_manifest {
                shared.add_project(&dir, &platform, &ctx);
            }
        }
        shared.finish(&platform, &ctx, &mut found);

        // Caller signals and classification.
        let git = GitRunner::new().ok();
        let probe: Box<dyn ActivityProbe> = match self.in_use {
            InUseSource::Native => Box::new(NativeActivityProbe::capture(Default::default())),
            InUseSource::IdleFixture => Box::new(IdleProbe),
        };
        let protected = self.protected_set(&platform, &found);
        let now = now_unix();
        let mut items: Vec<Item> = found
            .into_iter()
            .map(|a| {
                let summary = summaries.get(&a.path).cloned();
                let in_use = check_in_use(&a.path, &a.activity_lock_files, probe.as_ref());
                let protection = protected.check(&a.path, &platform);
                let caller = CallerSignals {
                    git: git_tracking(git.as_ref(), &self.root, &a.path),
                    in_protected_root: protection.in_protected_root,
                    user_excluded: protection.user_excluded,
                    protect_marker: protection.protect_marker,
                    in_use: in_use.in_use,
                    crosses_mount: boundaries.iter().any(|b| {
                        b.reason == BoundaryReason::OtherVolume && b.path.starts_with(&a.path)
                    }),
                    // Ground truth: the scanner never follows a link, so everything it reached
                    // is a real directory below the root.
                    link_escape: false,
                    // Ground truth: a fresh temporary directory is not cloud-synced.
                    cloud_sync: false,
                    recent_activity: match &summary {
                        Some(s) => self.window.observe(s.newest_mtime, !s.complete, now),
                        None => Tri::Unknown,
                    },
                };
                let mut candidate = a.clone().into_candidate(&caller);
                candidate.evidence.extend(protection.evidence);
                candidate.evidence.extend(in_use.evidence);
                if let Some(s) = &summary {
                    candidate.sizes = estimate(
                        s,
                        &EstimateContext::new(VolumeFs::Unknown, Snapshots::Unknown),
                    );
                }
                assert!(candidate.is_consistent(), "{candidate:?}");
                Item {
                    candidate,
                    artifact: a,
                    caller,
                    summary,
                }
            })
            .collect();
        items.sort_by(|x, y| x.artifact.path.cmp(&y.artifact.path));
        Run {
            root: self.root.clone(),
            items,
            outcome,
            issues,
            boundaries,
            visited,
        }
    }
}

impl Pipeline {
    /// The protected-roots set (P1-SAFE-03) the engine would build: built-in roots for this OS
    /// (with the stand-in home, if any), the user's protected paths, and every `.scull.toml`
    /// between an artifact and the scan root (CFG-03: repo files only add protection).
    /// `.scull-protect` markers are found by [`ProtectedSet::check`] itself.
    fn protected_set(&self, platform: &Native, found: &[DetectedArtifact]) -> ProtectedSet {
        let env = |k: &str| std::env::var_os(k);
        let mut set = ProtectedSet::new();
        set.add_defaults(
            platform,
            &default_protected(TargetOs::current(), self.home.as_deref(), &env),
        );
        for p in &self.user_protect {
            set.protect(platform, p, Scope::Subtree, Source::User);
        }
        let mut repo_dirs: Vec<&Path> = found
            .iter()
            .flat_map(|a| a.path.ancestors().take_while(|p| p.starts_with(&self.root)))
            .filter(|d| d.join(REPO_CONFIG_FILE).is_file())
            .collect();
        repo_dirs.sort();
        repo_dirs.dedup();
        for dir in repo_dirs {
            let text = fs::read_to_string(dir.join(REPO_CONFIG_FILE)).unwrap_or_default();
            set.add_repo_policy(platform, dir, &RepoPolicy::parse_toml(&text).policy);
        }
        set
    }
}

/// The real scanner over `root`, collecting every event.
pub fn scan(root: &Path) -> (ScanOutcome, Vec<ScanEvent>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let mut opts = ScanOptions::default();
    opts.emit_listings = true;
    let outcome = scull_scanner::scan(
        Arc::new(Native::new()),
        root,
        &opts,
        &CancelToken::new(),
        move |e| sink.lock().unwrap().push(e),
    )
    .expect("scan starts");
    let events = std::mem::take(&mut *events.lock().unwrap());
    (outcome, events)
}

/// Git tracking of the files inside `artifact`: through `git status` (untracked files listed
/// one by one, ignored paths as matched) of the work tree containing it. A regular file or link
/// inside the artifact that is neither untracked nor ignored is tracked. Any Git failure gives
/// `Unknown`, never "not tracked".
pub fn git_tracking(git: Option<&GitRunner>, root: &Path, artifact: &Path) -> GitTracking {
    let Some(worktree) = artifact
        .ancestors()
        .skip(1)
        .take_while(|p| p.starts_with(root.parent().unwrap_or(root)))
        .find(|p| fs::symlink_metadata(p.join(".git")).is_ok())
    else {
        return GitTracking::NotTracked; // not inside a repository
    };
    let Some(git) = git else {
        return GitTracking::Unknown;
    };
    let opts = StatusOptions {
        untracked: UntrackedMode::All,
        ignored: IgnoredMode::Matching,
    };
    let Ok(status) = git.status(worktree, &opts) else {
        return GitTracking::Unknown;
    };
    let mut excluded: Vec<(PathBuf, bool)> = Vec::new();
    for e in status.untracked().chain(status.ignored()) {
        match e.path.to_path(worktree) {
            Some(p) => excluded.push((p, e.path.is_dir())),
            None => return GitTracking::Unknown,
        }
    }
    let mut files = 0u64;
    let mut stack = vec![artifact.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(md) = fs::symlink_metadata(&p) else {
            return GitTracking::Unknown;
        };
        if excluded
            .iter()
            .any(|(x, is_dir)| p == *x || (*is_dir && p.starts_with(x)))
        {
            continue;
        }
        if md.is_dir() {
            let Ok(rd) = fs::read_dir(&p) else {
                return GitTracking::Unknown;
            };
            stack.extend(rd.filter_map(|e| e.ok().map(|e| e.path())));
        } else {
            files += 1;
        }
    }
    if files > 0 {
        GitTracking::Tracked { files }
    } else {
        GitTracking::NotTracked
    }
}

/// `path` relative to `root`, `/`-separated.
pub fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Run the universal invariants for a fixture whose pipeline produced `scan` (scan time) and
/// `revalidated` (a second run standing in for execute-time revalidation): I-1 on the plan,
/// I-2 canary, I-3 tree (only `allowed` may differ), I-8 tiers. Requires each of them to have
/// run and passed, and returns the report (I-4…I-7 are reported as unimplemented by the
/// harness, never as passes).
pub fn check_all(fx: &Fixture, scan: &Run, revalidated: &Run, allowed: &[&str]) -> InvariantReport {
    let mut inputs = InvariantInputs::new()
        .plan(scan.plan().into_iter().map(PlanEntry::from))
        .tiers(scan.tiers(), revalidated.tiers());
    for a in allowed {
        inputs = inputs.allow(*a);
    }
    let report = fx.check_invariants(&inputs);
    report.assert_ok();
    let ran = report.ran();
    for id in [
        InvariantId::I1,
        InvariantId::I2,
        InvariantId::I3,
        InvariantId::I8,
    ] {
        assert!(ran.contains(&id), "{id} ran");
    }
    report
}

/// Run `pipeline` twice (scan, then revalidation) and check the invariants with nothing allowed
/// to change. Returns the scan-time run.
pub fn run_checked(fx: &Fixture, pipeline: &Pipeline) -> Run {
    let scan = pipeline.run();
    let revalidated = pipeline.run();
    check_all(fx, &scan, &revalidated, &[]);
    scan
}
