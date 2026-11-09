//! The engine: one read-only pipeline shared by the CLI, the MCP server and the desktop app.

use crate::collect::{
    Collector, EngineProgress, PROJECT_MARKERS, ProgressFn, State, lock, within_any,
};
use crate::git::{GIT_SIGNAL, Tracking, git_tri};
use crate::id::new_scan_id;
use crate::paths::{canon, canon_exact, is_within, overlaps};
use crate::reader::FsConfigReader;
use crate::report::{
    CachesReport, HistoryOutcome, IssueKind, ProjectSummary, RefusedCacheReport, RepoConfigWarning,
    ScanReport, SignalUnavailable, Totals, WorktreeProblemReport, WorktreeReport,
};
use crate::signals::{
    ActivityInUse, COMPLETENESS_SIGNAL, InUseAnswer, InUseProbe, InUseSource, MOUNTS_SIGNAL,
    NativeVolumes, PROTECTION_SIGNAL, VolumeSource, in_use_unavailable,
};
use scull_core::config::{
    Config, ConfigWarning, ConfigWarningCode, REPO_CONFIG_FILE, REPO_PROTECT_MARKER, RepoPolicy,
    RepoPolicyParse,
};
use scull_core::{
    Action, Candidate, Category, ErrorKind, Evidence, FileIdentity, FsPath, ItemId, ReasonCode,
    ScullError, Sizes, Tier, TierBreakdown, TierReason,
};
use scull_detectors::caches::CacheTool;
use scull_detectors::{
    CacheEnv, CallerSignals, ConfigReader, DetectContext, Detectors, MAX_CONFIG_BYTES,
    locate_caches,
};
use scull_git::{
    Activity, AnalyzeOptions, DEFAULT_ACTIVITY_SAMPLE, GitRunner, Observed, RepoWorktrees,
    WorktreeDecision, WorktreeKind, analyze_repo, classify_worktree, discover, sample_activity,
    worktree_candidate,
};
use scull_platform::activity::Limits;
use scull_platform::{
    EntryKind, Platform, ScanPolicy, SyncedRoot, SyncedRoots, VolumeInfo, VolumeKind,
};
use scull_safety::{
    ActivityWindow, GitTracking, MtimeSample, NativeActivityProbe, ProtectedSet, Regen,
    Signals as RubricSignals, Tri, classify, recent_activity,
};
use scull_scanner::{
    BoundaryReason, CancelToken, DirSummary, EstimateContext, ScanOptions, Snapshots, VolumeFs,
    estimate,
};
use scull_storage::{
    ScanFinish, ScanRecord, ScanStart, ScanStatus, ScanTotals, Store, VolumeSample,
    rollups_from_candidates,
};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What the engine needs from its caller. Everything machine-specific comes in here; the
/// engine reads no environment variables.
pub struct EngineConfig {
    /// The effective configuration (CLI flags > environment > user file > defaults), with
    /// `~` already expanded.
    pub config: Config,
    /// Filesystem facts.
    pub platform: Arc<dyn Platform>,
    /// Hardened `git`, if one was found on a trusted `PATH` entry. Without it Git tracking is
    /// unknown (S2) inside repositories and worktrees are not analysed.
    pub git: Option<GitRunner>,
    /// History database. `None` records nothing.
    pub store: Option<Store>,
    /// Cloud-synced folders (`SyncedRoots::detect()` for the real machine; empty finds only
    /// placeholders by their own metadata).
    pub synced_roots: SyncedRoots,
    /// Scan roots on network or virtual volumes (off by default, D-22). Backup volumes are
    /// never scanned.
    pub allow_network_roots: bool,
    /// Shared Cargo target directories supplied by the caller (e.g. `CARGO_TARGET_DIR`), with
    /// a label naming the source.
    pub explicit_target_dirs: Vec<(PathBuf, String)>,
    /// In-use detection. [`InUseSource::Unavailable`] makes every candidate "in use:
    /// unknown" (at least S2).
    pub in_use: InUseSource,
    /// The mounted-volume list ([`NativeVolumes`] by default).
    pub volumes: Arc<dyn VolumeSource>,
    /// How long one directory listing, or one entry's metadata call, may take before the
    /// walk gives up on it (default 60 s; see [`ScanOptions::dir_timeout`]). A hung entry
    /// (a frozen mount point) costs only itself: it is reported as timed out and its siblings
    /// are still scanned. `None` waits forever.
    pub dir_timeout: Option<Duration>,
}

impl EngineConfig {
    /// A configuration with nothing optional: no Git, no history, no in-use probe.
    pub fn new(config: Config, platform: Arc<dyn Platform>) -> Self {
        Self {
            config,
            platform,
            git: None,
            store: None,
            synced_roots: SyncedRoots::default(),
            allow_network_roots: false,
            explicit_target_dirs: Vec::new(),
            in_use: InUseSource::Unavailable,
            volumes: Arc::new(NativeVolumes),
            dir_timeout: ScanOptions::default().dir_timeout,
        }
    }
}

/// Options for one scan.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ScanRequest {
    /// Record the scan in history (needs a store).
    pub record_history: bool,
    /// Scanner threads (0 = automatic).
    pub threads: usize,
    /// Analyse Git worktrees (needs `git`).
    pub worktrees: bool,
    /// "Now" for activity checks and IDs (Unix seconds); `None` uses the clock.
    pub now: Option<i64>,
}

impl ScanRequest {
    /// The defaults for `scull scan`: history recorded, worktrees analysed.
    pub fn full() -> Self {
        Self {
            record_history: true,
            threads: 0,
            worktrees: true,
            now: None,
        }
    }

    /// Set whether history is recorded.
    #[must_use]
    pub fn history(mut self, on: bool) -> Self {
        self.record_history = on;
        self
    }

    /// Set the clock used for activity checks.
    #[must_use]
    pub fn at(mut self, now: i64) -> Self {
        self.now = Some(now);
        self
    }
}

/// The read-only pipeline: scan → detect → Git → signals → classify → candidates.
pub struct Engine {
    config: Config,
    platform: Arc<dyn Platform>,
    git: Option<GitRunner>,
    store: Option<Mutex<Store>>,
    detectors: Arc<Detectors>,
    synced_roots: SyncedRoots,
    allow_network_roots: bool,
    explicit_target_dirs: Vec<(PathBuf, String)>,
    in_use: InUseSource,
    volume_source: Arc<dyn VolumeSource>,
    /// See [`EngineConfig::dir_timeout`].
    dir_timeout: Option<Duration>,
    /// Mounted volumes, re-read at the start of each run.
    volumes: Mutex<Vec<VolumeInfo>>,
    /// Why the last volume list could not be read, if it couldn't.
    volumes_error: Mutex<Option<String>>,
}

/// The in-use probe for one run, if any.
type RunProbe = Option<Arc<dyn InUseProbe>>;

/// Current Unix time in seconds.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

impl Engine {
    /// Build an engine with the bundled detection rules.
    pub fn new(cfg: EngineConfig) -> Result<Self, ScullError> {
        let detectors = Detectors::bundled().map_err(|e| {
            ScullError::new(
                ErrorKind::Internal,
                format!("bundled rules failed to load: {e}"),
            )
        })?;
        // Match synced folders both as found and in canonical form (candidate paths are
        // canonical).
        let mut synced: Vec<SyncedRoot> = Vec::new();
        for r in cfg.synced_roots.roots() {
            synced.push(r.clone());
            let c = canon(&r.path);
            if c != r.path {
                synced.push(SyncedRoot::new(c, r.provider, r.mechanism));
            }
        }
        Ok(Self {
            config: cfg.config,
            platform: cfg.platform,
            git: cfg.git,
            store: cfg.store.map(Mutex::new),
            detectors: Arc::new(detectors),
            synced_roots: SyncedRoots::from_roots(synced),
            allow_network_roots: cfg.allow_network_roots,
            explicit_target_dirs: cfg.explicit_target_dirs,
            in_use: cfg.in_use,
            volume_source: cfg.volumes,
            dir_timeout: cfg.dir_timeout,
            volumes: Mutex::new(Vec::new()),
            volumes_error: Mutex::new(None),
        })
    }

    /// The effective configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether a history store is attached.
    pub fn has_store(&self) -> bool {
        self.store.is_some()
    }

    /// Canonicalize and check scan roots: each must be an existing directory (not a link).
    /// Duplicates and roots inside another root are dropped, compared by path and by file
    /// identity (volume + file ID), so two paths to one folder that `canonicalize` keeps apart
    /// (macOS firmlinks: `~/x` and `/System/Volumes/Data/Users/<me>/x`) are scanned once
    /// (FX-MAC-03, #249). The first of two aliases is kept.
    pub fn prepare_roots(&self, roots: &[PathBuf]) -> Result<Vec<PathBuf>, ScullError> {
        let mut out: Vec<PathBuf> = Vec::new();
        for r in roots {
            let c = canon(r);
            match self.platform.metadata(&c) {
                Ok(md) if md.kind == scull_platform::EntryKind::Dir => {}
                Ok(md) => {
                    return Err(ScullError::new(
                        ErrorKind::Usage,
                        format!("scan root is not a directory (found {})", md.kind),
                    )
                    .with_path(FsPath::from_path(r)));
                }
                Err(e) => {
                    let kind = if e.is_not_found() {
                        ErrorKind::NotFound
                    } else if e.is_permission_denied() {
                        ErrorKind::PermissionDenied
                    } else {
                        ErrorKind::Io
                    };
                    return Err(ScullError::new(kind, format!("cannot scan: {e}"))
                        .with_path(FsPath::from_path(r)));
                }
            }
            // D-22: backup volumes never, network and virtual volumes only on request.
            if let Ok(v) = scull_platform::volume_of(&c) {
                let blocked = match v.kind.scan_policy() {
                    ScanPolicy::Never => Some("a backup volume; backup volumes are never scanned"),
                    ScanPolicy::OffByDefault if !self.allow_network_roots => Some(
                        "on a network or virtual volume (network share, FUSE, tmpfs, overlay), \
                         which is not scanned unless you opt in",
                    ),
                    _ => None,
                };
                if let Some(why) = blocked {
                    return Err(ScullError::new(
                        ErrorKind::SafetyBlocked,
                        format!(
                            "scan root is {why} ({} volume at {})",
                            v.kind,
                            v.mount_point.display()
                        ),
                    )
                    .with_path(FsPath::from_path(r)));
                }
            }
            if !out.contains(&c) {
                out.push(c);
            }
        }
        let all = out.clone();
        out.retain(|r| !all.iter().any(|o| o != r && is_within(r, o)));
        Ok(self.dedupe_roots_by_identity(out))
    }

    /// Drop roots that are the same folder as, or lie inside, another root by file identity:
    /// a root whose own identity, or one of its ancestors' identities, is another root's. Of
    /// two roots with the same identity the earlier one is kept. Roots without an identity
    /// (the platform can't tell) are compared by path only.
    fn dedupe_roots_by_identity(&self, roots: Vec<PathBuf>) -> Vec<PathBuf> {
        let identity = |p: &Path| self.platform.metadata(p).ok().and_then(|md| md.identity);
        let own: Vec<_> = roots.iter().map(|r| identity(r)).collect();
        let mut keep = Vec::with_capacity(roots.len());
        for (i, root) in roots.iter().enumerate() {
            // Same folder as an earlier root.
            let alias = own[i].is_some() && own[..i].contains(&own[i]);
            // Inside another root through an ancestor that is that root by identity.
            let nested = || {
                root.ancestors().skip(1).any(|a| {
                    identity(a).is_some_and(|id| {
                        own.iter()
                            .enumerate()
                            .any(|(j, o)| j != i && *o == Some(id))
                    })
                })
            };
            if !alias && !nested() {
                keep.push(root.clone());
            }
        }
        keep
    }

    /// The canonical form of every root in `roots` that is reached through a link pointing
    /// outside the folder holding it (PRD §8.3, SAFE-04, FX-FS-03): some component of the path
    /// as given is a symlink or junction whose real location is not inside the real location of
    /// the folder that contains the link. Everything found below such a root is S3
    /// (`link_escape`). Links that stay inside their own folder, and system aliases such as
    /// macOS's `/tmp` → `/private/tmp` (a link in `/` that points inside `/`), don't count.
    fn link_escaped_roots(&self, roots: &[PathBuf]) -> Vec<PathBuf> {
        let platform = self.platform.as_ref();
        roots
            .iter()
            .filter(|r| {
                let abs = if r.is_absolute() {
                    (*r).clone()
                } else {
                    match std::env::current_dir() {
                        Ok(cwd) => cwd.join(r),
                        Err(_) => return false,
                    }
                };
                abs.ancestors().any(|prefix| {
                    let Some(parent) = prefix.parent() else {
                        return false;
                    };
                    let is_link = platform.metadata(prefix).is_ok_and(|md| {
                        matches!(
                            md.kind,
                            scull_platform::EntryKind::Symlink
                                | scull_platform::EntryKind::Junction
                        )
                    });
                    is_link && !is_within(&canon(prefix), &canon(parent))
                })
            })
            .map(|r| canon(r))
            .collect()
    }

    /// Scan `roots` and classify everything found. Read-only: nothing on disk changes except
    /// the history database when a store is attached and `req.record_history` is set.
    ///
    /// When `cancel` fires the walk stops and the report is returned with `cancelled: true`;
    /// history records the scan as cancelled, which never marks items gone.
    pub fn scan(
        &self,
        roots: &[PathBuf],
        req: &ScanRequest,
        cancel: &CancelToken,
        progress: impl Fn(EngineProgress) + Send + Sync + 'static,
    ) -> Result<ScanReport, ScullError> {
        let progress: ProgressFn = Arc::new(progress);
        let started_at = req.now.unwrap_or_else(now_unix);
        let scan_id = new_scan_id(started_at);
        // Before canonicalizing: which roots the caller reached through an escaping link.
        let link_roots = self.link_escaped_roots(roots);
        let roots = self.prepare_roots(roots)?;
        self.refresh_volumes();
        let mut history_err = None;
        let record = req.record_history && self.store.is_some();
        if record && let Some(store) = &self.store {
            let start = ScanStart {
                scan_id: scan_id.clone(),
                started_at,
                roots: roots.iter().map(|r| FsPath::from_path(r)).collect(),
            };
            if let Err(e) = lock(store).record_scan_start(&start) {
                history_err = Some(e.to_string());
            }
        }

        let collector = Collector::new(
            Arc::clone(&self.platform),
            Some(Arc::clone(&self.detectors)),
            self.walk_options(),
            Arc::clone(&progress),
        );
        {
            let mut st = lock(&collector.state);
            for (p, label) in &self.explicit_target_dirs {
                let c = canon(p);
                if st.shared_targets.add_explicit(&c, label.clone()) {
                    st.interesting.insert(c);
                }
            }
        }
        let mut cancelled = false;
        for root in &roots {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            let outcome = collector
                .walk(root, req.threads, cancel)
                .map_err(|e| ScullError::from(&e).with_path(FsPath::from_path(root)))?;
            if outcome.is_cancelled() {
                cancelled = true;
                break;
            }
        }

        let mut st = std::mem::take(&mut *lock(&collector.state));
        let mut found: Vec<_> = std::mem::take(&mut st.artifacts).into_values().collect();
        if !cancelled {
            progress(EngineProgress::Analyzing {
                what: "shared Cargo target directories".into(),
            });
            let reader = FsConfigReader;
            let ctx = DetectContext::new().with_config_reader(&reader);
            std::mem::take(&mut st.shared_targets).finish(self.platform.as_ref(), &ctx, &mut found);
        }
        // One candidate per directory: drop anything inside another artifact (e.g. a project
        // target merged into a shared one), and anything outside the roots.
        found.sort_by(|a, b| a.path.cmp(&b.path));
        let paths: Vec<PathBuf> = found.iter().map(|a| a.path.clone()).collect();
        found.retain(|a| !paths.iter().any(|p| p != &a.path && is_within(&a.path, p)));
        let mut outside = Vec::new();
        found.retain(|a| {
            let keep = within_any(&a.path, &roots);
            if !keep {
                outside.push(a.path.clone());
            }
            keep
        });
        for p in outside {
            st.issue(
                &p,
                IssueKind::OutsideRoots,
                "shared build directory declared by a scanned project lies outside the scan \
                 roots; add it as a root to include it"
                    .into(),
            );
        }
        if !cancelled {
            let need: Vec<PathBuf> = found
                .iter()
                .map(|a| a.path.clone())
                .filter(|p| !st.summaries.contains_key(p))
                .collect();
            self.measure(&mut st, &need, req.threads, cancel);
        }

        progress(EngineProgress::Analyzing {
            what: "classifying".into(),
        });
        let now = started_at.max(req.now.unwrap_or_else(now_unix));
        let policies = self.repo_policies(&st);
        let probe = self.run_probe();
        let mut post = Post::new(self, &st, &policies.0, now).with_link_roots(link_roots);
        let mut candidates = Vec::new();
        let mut unmeasured = Vec::new();
        for a in found {
            let Some(summary) = st.summaries.get(&a.path).cloned() else {
                if !cancelled {
                    unmeasured.push(a.path);
                }
                continue;
            };
            let answer = ask(
                &probe,
                &a.path,
                &a.activity_lock_files,
                &a.activity_processes,
            );
            let sig = post.signals(&a.path, Some(&summary), answer);
            let mut c = a.into_candidate(&sig.caller);
            finish_candidate(&mut c, sig, Some(&summary), Some(self.estimate(&summary)));
            candidates.push(c);
        }

        let mut worktrees = Vec::new();
        let mut worktree_problems = Vec::new();
        if req.worktrees && !cancelled {
            progress(EngineProgress::Analyzing {
                what: "Git worktrees".into(),
            });
            let (reports, problems, wt_candidates) = self.worktrees(&mut post, &roots, &probe);
            worktrees = reports;
            worktree_problems = problems;
            candidates.extend(wt_candidates);
        }

        let candidates = dedupe(candidates);
        let projects = projects(&st, &candidates, &post.tracking);
        let gaps = post.run_gaps();
        drop(post);
        for p in unmeasured {
            st.issue(
                &p,
                IssueKind::ReadError,
                "could not be measured; not offered".into(),
            );
        }
        let totals = Totals::of(&candidates);
        let mut signals_unavailable = signals_unavailable(&candidates);
        for (signal, reason) in &gaps {
            push_signal(&mut signals_unavailable, signal, reason, 0);
        }
        if req.worktrees && self.git.is_none() && !st.git_roots.is_empty() {
            push_signal(
                &mut signals_unavailable,
                GIT_SIGNAL,
                "`git` was not found on a trusted PATH entry: worktrees were not analysed and \
                 Git tracking inside repositories is unknown (at least S2)",
                0,
            );
        }
        let finished_at = now_unix().max(started_at);

        let history = if record {
            match history_err {
                Some(message) => HistoryOutcome::Failed { message },
                None => self.record(&scan_id, finished_at, cancelled, &st, &candidates, &roots),
            }
        } else {
            HistoryOutcome::Disabled
        };
        if let HistoryOutcome::Failed { message } = &history {
            st.issue(Path::new(""), IssueKind::History, message.clone());
        }

        Ok(ScanReport {
            scan_id,
            started_at,
            finished_at,
            roots: roots.iter().map(|r| FsPath::from_path(r)).collect(),
            cancelled,
            candidates,
            projects,
            worktrees,
            worktree_problems,
            totals,
            entries_scanned: st.entries,
            issues: st.issues,
            issues_truncated: st.issues_truncated,
            signals_unavailable,
            config_warnings: policies.1,
            history,
        })
    }

    /// Size directories the walk could not attribute (e.g. a shared target directory
    /// finished before the project declaring it was listed).
    fn measure(&self, st: &mut State, dirs: &[PathBuf], threads: usize, cancel: &CancelToken) {
        for d in dirs {
            if cancel.is_cancelled() {
                return;
            }
            let c = Collector::new(
                Arc::clone(&self.platform),
                None,
                self.walk_options(),
                Arc::new(|_| {}),
            );
            if let Ok(outcome) = c.walk(d, threads, cancel)
                && !outcome.is_cancelled()
                && let Some(s) = outcome.root
            {
                let sub = std::mem::take(&mut *lock(&c.state));
                st.protect_markers.extend(sub.protect_markers);
                st.repo_configs.extend(sub.repo_configs);
                st.reparse_dirs.extend(sub.reparse_dirs);
                st.other_volume.extend(sub.other_volume);
                st.boundaries.extend(sub.boundaries);
                st.git_roots
                    .extend(sub.git_roots.into_iter().filter(|g| g != d));
                st.summaries.insert(d.clone(), s);
            }
        }
    }

    /// Parse every `.scull.toml` seen; `.scull-protect` markers are handled separately.
    fn repo_policies(&self, st: &State) -> (BTreeMap<PathBuf, RepoPolicy>, Vec<RepoConfigWarning>) {
        let mut policies = BTreeMap::new();
        let mut warnings = Vec::new();
        for dir in &st.repo_configs {
            let file = dir.join(REPO_CONFIG_FILE);
            let parsed = match FsConfigReader.read_text(&file, MAX_CONFIG_BYTES) {
                Some(text) => RepoPolicy::parse_toml(&text),
                // Unreadable or too large: protect the whole directory, like invalid TOML.
                None => RepoPolicyParse {
                    policy: RepoPolicy::protect_marker(),
                    warnings: vec![ConfigWarning::new(
                        ConfigWarningCode::RepoParseError,
                        REPO_CONFIG_FILE,
                        "could not be read (or is larger than 1 MiB), so the whole directory \
                         is protected",
                    )],
                },
            };
            for w in parsed.warnings {
                warnings.push(RepoConfigWarning {
                    file: FsPath::from_path(&file),
                    warning: w,
                });
            }
            policies.insert(dir.clone(), parsed.policy);
        }
        (policies, warnings)
    }

    /// The in-use probe for one run: a fresh process snapshot for the native source.
    fn run_probe(&self) -> RunProbe {
        match &self.in_use {
            InUseSource::Unavailable => None,
            InUseSource::Native(policy) => Some(Arc::new(ActivityInUse(
                NativeActivityProbe::capture(Limits::default()).with_policy(*policy),
            ))),
            InUseSource::Probe(p) => Some(Arc::clone(p)),
        }
    }

    /// Re-read the mounted volumes (once per run): filesystem types for the reclaim estimate
    /// and mount points for the mount-boundary check. A failure is remembered: without the
    /// list a bind mount inside a candidate can't be ruled out (bind mounts keep the volume
    /// ID, so the walk doesn't see them), which the protection step treats as a crossing.
    fn refresh_volumes(&self) {
        let (v, err) = match self.volume_source.volumes() {
            Ok(v) => (v, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
        *lock(&self.volumes) = v;
        *lock(&self.volumes_error) = err;
    }

    /// Sizes of a scanned directory with the PRD §9 reclaim estimate, using the filesystem
    /// type of its volume. Snapshot state is not known yet, so confidence is at most medium.
    fn estimate(&self, s: &DirSummary) -> Sizes {
        let mut ctx = EstimateContext::default();
        ctx.allocated_size_supported = self.platform.capabilities().allocated_size;
        if let Some(id) = s.identity {
            let vols = lock(&self.volumes);
            if let Some(v) = vols.iter().find(|v| v.volume_id == Some(id.volume)) {
                ctx.fs = VolumeFs::from_name(&v.fs_type);
                ctx.snapshots = match v.supports.snapshots {
                    Some(false) => Snapshots::Absent,
                    Some(true) => Snapshots::Possible,
                    None => Snapshots::Unknown,
                };
            }
        }
        estimate(s, &ctx)
    }

    /// Base options of every walk this run: the configured timeout, and the network mount
    /// points to leave untouched ([`Engine::skip_mounts`]).
    fn walk_options(&self) -> ScanOptions {
        let mut opts = ScanOptions::default();
        opts.dir_timeout = self.dir_timeout;
        opts.skip_mounts = self.skip_mounts();
        opts
    }

    /// Network mount points (NFS, SMB, sshfs, every FUSE filesystem on Linux, ...) from the
    /// mount table read for this run: the walk reports them as other-volume boundaries
    /// without touching them, so a stale or frozen one costs nothing and its siblings are
    /// still scanned (T-53, #301). A later mount at the same point hides an earlier one.
    /// Empty when the table could not be read; the walk's per-entry deadline then still
    /// confines a hung mount point to itself.
    fn skip_mounts(&self) -> Vec<PathBuf> {
        let mut by_point: BTreeMap<PathBuf, VolumeKind> = BTreeMap::new();
        for v in lock(&self.volumes).iter() {
            by_point.insert(v.mount_point.clone(), v.kind);
        }
        by_point
            .into_iter()
            .filter(|(_, kind)| *kind == VolumeKind::Network)
            .map(|(point, _)| point)
            .collect()
    }

    /// Whether a volume is mounted strictly inside `dir` (bind mounts and mounted folders keep
    /// the volume ID, so the walk's boundaries don't show them).
    fn has_mount_inside(&self, dir: &Path) -> bool {
        !scull_platform::mounts_within(dir, &lock(&self.volumes)).is_empty()
    }

    /// Analyse every repository the walk met (GIT-02) and classify its worktrees (GIT-03).
    fn worktrees(
        &self,
        post: &mut Post<'_>,
        roots: &[PathBuf],
        probe: &RunProbe,
    ) -> (
        Vec<WorktreeReport>,
        Vec<WorktreeProblemReport>,
        Vec<Candidate>,
    ) {
        let Some(runner) = &self.git else {
            return Default::default();
        };
        let mut reports = Vec::new();
        let mut problems = Vec::new();
        let mut candidates = Vec::new();
        let mut seen_common: HashSet<PathBuf> = HashSet::new();
        let mut seen_wt: HashSet<PathBuf> = HashSet::new();
        let st = post.st;
        let starts: BTreeSet<&PathBuf> = st
            .git_roots
            .iter()
            .chain(&st.bare_repos)
            .filter(|p| within_any(p, roots))
            .collect();
        for start in starts {
            if seen_wt.contains(start.as_path()) {
                continue;
            }
            let Ok(repo) = discover(start) else {
                continue;
            };
            let common = canon(&repo.common_dir.to_path_buf());
            if !seen_common.insert(common) {
                continue;
            }
            for w in &repo.worktrees {
                seen_wt.insert(canon(&w.path.to_path_buf()));
            }
            problems.extend(WorktreeProblemReport::of(&repo));
            let (r, c) = self.analyze(runner, &repo, post, roots, probe);
            reports.extend(r);
            candidates.extend(c);
        }
        (reports, problems, candidates)
    }

    fn analyze(
        &self,
        runner: &GitRunner,
        repo: &RepoWorktrees,
        post: &mut Post<'_>,
        roots: &[PathBuf],
        probe: &RunProbe,
    ) -> (Vec<WorktreeReport>, Vec<Candidate>) {
        let platform = self.platform.as_ref();
        let st = post.st;
        let states = analyze_repo(
            runner,
            repo,
            |wt| {
                let path = canon(&wt.path.to_path_buf());
                if wt.prunable.is_some() {
                    return Observed::unknown("the worktree folder is missing");
                }
                match st.summaries.get(&path) {
                    Some(s) => Observed::known(Activity {
                        newest_unix: s.newest_mtime,
                        newest_path: None,
                        entries_sampled: s.totals.entries,
                        complete: s.complete,
                    }),
                    None => sample_activity(platform, &path, DEFAULT_ACTIVITY_SAMPLE),
                }
            },
            &AnalyzeOptions::default(),
        );
        let reader = FsConfigReader;
        let detectors = Arc::clone(&self.detectors);
        let matcher = move |wt: &Path, rel: &str| -> Option<String> {
            let rel = rel.strip_suffix('/')?;
            let dir = rel.split('/').fold(wt.to_path_buf(), |p, c| p.join(c));
            let ctx = DetectContext::new().with_config_reader(&reader);
            detectors
                .detect(&dir, platform, &ctx)
                .map(|a| a.rule.id.clone())
        };
        let mut reports = Vec::new();
        let mut candidates = Vec::new();
        for state in states {
            let path = canon(&state.path.to_path_buf());
            let answer = if state.missing {
                InUseAnswer {
                    in_use: Tri::No,
                    evidence: Vec::new(),
                }
            } else {
                ask(probe, &path, &[], &[])
            };
            let in_use = git_tri(answer.in_use);
            let mut decision = classify_worktree(&state, in_use, &matcher);
            decision.evidence.extend(answer.evidence);
            let target = if state.missing {
                canon(&state.git_dir.to_path_buf())
            } else {
                path.clone()
            };
            let inside = within_any(&target, roots);
            let summary = st.summaries.get(&path);
            // The same protection step as every other candidate; the GIT-03 table only knows
            // what Git says. A stale registration (folder missing) has no subtree to observe.
            let present = !state.missing;
            let protect = post.protection(&target, summary.filter(|_| present), present);
            protect_decision(&mut decision, &protect);
            let removable = state.kind == WorktreeKind::Linked;
            let mut candidate_id = None;
            if removable && inside {
                let (identity, missing_identity) =
                    match platform.metadata(&target).ok().and_then(|m| m.identity) {
                        Some(i) => (i, false),
                        None => (FileIdentity::new(0, 0), true),
                    };
                let sizes = match (summary, state.missing) {
                    (Some(s), false) => self.estimate(s),
                    _ => Sizes::default(),
                };
                let mut c = worktree_candidate(&state, &decision, in_use, identity, sizes);
                apply_protection(&mut c, &protect);
                if missing_identity {
                    c.raise(TierReason::new(
                        Tier::S3,
                        ReasonCode::SignalUnavailable,
                        "file identity unavailable; the item can't be revalidated before removal",
                    ));
                }
                candidate_id = Some(c.id.clone());
                candidates.push(c);
            }
            reports.push(WorktreeReport {
                path: FsPath::from_path(&path),
                kind: state.kind,
                common_dir: repo.common_dir.clone(),
                inside_roots: inside,
                allocated: summary.map(|s| s.totals.allocated),
                candidate: candidate_id,
                state,
                decision,
            });
        }
        (reports, candidates)
    }

    /// Analyse the single worktree at `path` (`scull worktrees inspect`).
    pub fn inspect_worktree(&self, path: &Path) -> Result<WorktreeReport, ScullError> {
        let Some(runner) = &self.git else {
            return Err(ScullError::new(
                ErrorKind::UnsupportedPlatform,
                "`git` was not found on a trusted PATH entry; worktrees can't be analysed",
            ));
        };
        let want = canon(path);
        let repo = discover(&want).map_err(|e| {
            ScullError::new(ErrorKind::NotFound, format!("not a Git worktree: {e}"))
                .with_path(FsPath::from_path(path))
        })?;
        // Walk the worktree (read-only) so the protection step sees what a scan would:
        // markers and policies inside and above it, nested repositories, mounts, cloud
        // placeholders and whether every folder could be read. A failed walk leaves it
        // unmeasured, which the protection step treats as unknown (at least S2).
        self.refresh_volumes();
        let collector = Collector::new(
            Arc::clone(&self.platform),
            None,
            self.walk_options(),
            Arc::new(|_| {}),
        );
        if let Err(e) = collector.walk(&want, 0, &CancelToken::new()) {
            lock(&collector.state).issue(&want, IssueKind::ReadError, e.to_string());
        }
        let st = std::mem::take(&mut *lock(&collector.state));
        let policies = self.repo_policies(&st);
        let mut post = Post::new(self, &st, &policies.0, now_unix());
        let parent = want.parent().map(Path::to_path_buf).unwrap_or(want.clone());
        let (reports, _) = self.analyze(runner, &repo, &mut post, &[parent], &self.run_probe());
        reports
            .into_iter()
            .find(|r| canon(&r.path.to_path_buf()) == want)
            .ok_or_else(|| {
                ScullError::new(
                    ErrorKind::NotFound,
                    "the path is inside a repository but is not one of its worktree folders",
                )
                .with_path(FsPath::from_path(path))
            })
    }

    /// Locate and classify package-manager caches (`scull caches`). Read-only.
    pub fn caches(
        &self,
        env: &CacheEnv,
        threads: usize,
        cancel: &CancelToken,
        progress: impl Fn(EngineProgress) + Send + Sync + 'static,
    ) -> CachesReport {
        let progress: ProgressFn = Arc::new(progress);
        let now = now_unix();
        let reader = FsConfigReader;
        let ctx = DetectContext::new().with_config_reader(&reader);
        let located = locate_caches(env, self.platform.as_ref(), &ctx);
        self.refresh_volumes();
        let collector = Collector::new(
            Arc::clone(&self.platform),
            None,
            self.walk_options(),
            progress,
        );
        let mut cancelled = false;
        let mut found = Vec::new();
        for c in located.found {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            // Never canonicalized: a cache reached through a link is not followed (QC-05).
            let path = c.path.clone();
            match collector.walk(&path, threads, cancel) {
                Ok(o) if o.is_cancelled() => {
                    cancelled = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    lock(&collector.state).issue(&path, IssueKind::ReadError, e.to_string());
                }
            }
            found.push((c, path));
        }
        let st = std::mem::take(&mut *lock(&collector.state));
        let policies = self.repo_policies(&st);
        let probe = self.run_probe();
        let mut post = Post::new(self, &st, &policies.0, now);
        let mut candidates = Vec::new();
        if !cancelled {
            for (c, path) in found {
                let summary = st.summaries.get(&path).cloned();
                let processes: Vec<String> = c
                    .activity_processes()
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect();
                let answer = ask(&probe, &path, &[], &processes);
                let mut sig = post.signals(&path, summary.as_ref(), answer);
                let platform = self.platform.as_ref();
                if let Some(owned) = tool_owned_repos(c.tool, &path, platform) {
                    sig.protect.exempt_tool_repos(owned);
                }
                let mut cand = c.into_candidate(&sig.caller, summary.as_ref());
                let estimate = summary.as_ref().map(|s| self.estimate(s));
                finish_candidate(&mut cand, sig, summary.as_ref(), estimate);
                candidates.push(cand);
            }
        }
        let candidates = dedupe(candidates);
        let mut unavailable = signals_unavailable(&candidates);
        for (signal, reason) in post.run_gaps() {
            push_signal(&mut unavailable, signal, &reason, 0);
        }
        CachesReport {
            measured_at: now,
            cancelled,
            totals: Totals::of(&candidates),
            signals_unavailable: unavailable,
            refused: located
                .refused
                .iter()
                .map(|r| RefusedCacheReport {
                    rule: r.tool.rule_id().to_owned(),
                    path: r.path.as_deref().map(FsPath::from_path),
                    source: r.source.to_string(),
                    reason: r.reason.clone(),
                })
                .collect(),
            candidates,
            issues: st.issues,
        }
    }

    fn record(
        &self,
        scan_id: &scull_core::ScanId,
        finished_at: i64,
        cancelled: bool,
        st: &State,
        candidates: &[Candidate],
        roots: &[PathBuf],
    ) -> HistoryOutcome {
        let Some(store) = &self.store else {
            return HistoryOutcome::Disabled;
        };
        let status = if cancelled {
            ScanStatus::Cancelled
        } else if st.read_errors > 0 {
            ScanStatus::Partial
        } else {
            ScanStatus::Completed
        };
        let rollups = match rollups_from_candidates(candidates) {
            Ok(r) => r,
            Err(e) => {
                return HistoryOutcome::Failed {
                    message: e.to_string(),
                };
            }
        };
        let totals = Totals::of(candidates);
        let finish = ScanFinish {
            scan_id: scan_id.clone(),
            finished_at,
            status,
            totals: ScanTotals {
                entries_scanned: st.entries,
                candidates: candidates.len() as u64,
                logical_bytes: totals.logical.total(),
                allocated_bytes: totals.allocated.total(),
                reclaimable_bytes: totals.reclaimable.total(),
                errors: st.read_errors,
            },
            rollups,
        };
        // Free space of every volume the scan covered (INS-01), when the OS reports it.
        let mut volumes: Vec<VolumeSample> = Vec::new();
        for r in roots {
            let Ok(v) = scull_platform::volume_of(r) else {
                continue;
            };
            let free = scull_platform::free_space(r).ok().or(v.free_bytes);
            if let (Some(total), Some(free)) = (v.total_bytes, free)
                && free <= total
            {
                let id = v.mount_point.to_string_lossy().into_owned();
                if !id.is_empty() && !volumes.iter().any(|s| s.volume_id == id) {
                    volumes.push(VolumeSample {
                        volume_id: id,
                        total,
                        free,
                        at: finished_at,
                    });
                }
            }
        }
        let rec = ScanRecord {
            finish,
            candidates,
            volumes: &volumes,
        };
        match lock(store).record_scan(&rec) {
            Ok(r) => HistoryOutcome::Recorded {
                new_items: r.new_items,
                disappeared: r.disappeared,
            },
            Err(e) => HistoryOutcome::Failed {
                message: format!("history not recorded: {e}"),
            },
        }
    }
}

/// Caller signals for one candidate, plus what the engine adds on top of the rubric.
struct Signals {
    caller: CallerSignals,
    /// Facts behind the caller's Git, activity and in-use answers.
    evidence: Vec<Evidence>,
    /// The engine-side protection signals, applied by [`apply_protection`].
    protect: Protect,
}

/// The engine-side protection signals for one candidate path (PRD §8.3 checks the detectors,
/// the Git table and the cache classifier can't observe themselves). Applied to every
/// candidate the engine emits by [`apply_protection`] (and to a worktree's report by
/// [`protect_decision`]): tiers only go up, and the action follows the tier (D-28).
#[derive(Debug, Default)]
struct Protect {
    in_protected_root: bool,
    user_excluded: bool,
    protect_marker: bool,
    /// Crosses a mount, or the mount table is unreadable (the protective answer).
    crosses_mount: bool,
    /// Reached through a link pointing outside the folder holding it (the scan root it lies
    /// under, as the caller gave it).
    link_escape: bool,
    /// Cloud provider (or placeholder description) when synced or holding a placeholder.
    cloud: Option<String>,
    evidence: Vec<Evidence>,
    /// Raises beyond the rubric rows: minimum tiers, nested repositories, completeness,
    /// unresolved protection entries.
    raises: Vec<TierReason>,
    /// Git repositories inside the item (each also raised to S3 in `raises`).
    nested_repos: Vec<PathBuf>,
}

/// The S3 reason for a Git repository inside an item.
fn nested_repo_message(repo: &Path) -> String {
    format!("contains a Git repository at {}", repo.display())
}

impl Protect {
    /// #139: drop the nested-repository raises for the repositories `tool_owned` accepts (a
    /// package tool's own Git folder, e.g. uv's `git-v0`). Only for caches that are cleaned
    /// exclusively with the tool's own command ([`tool_owned_repos`]): the tool manages those
    /// repositories, and they hold no unique work. Every other repository still raises S3, and
    /// every other signal is untouched.
    fn exempt_tool_repos(&mut self, tool_owned: impl Fn(&Path) -> bool) {
        for repo in self.nested_repos.iter().filter(|r| tool_owned(r)) {
            let msg = nested_repo_message(repo);
            self.raises
                .retain(|r| !(r.code == ReasonCode::GitTracked && r.message == msg));
        }
    }

    /// Every reason this protection implies: the PRD §8.3 rubric rows for the boolean signals
    /// (the very reasons the rubric gives a detector-built candidate, so re-applying them to
    /// one is a no-op) plus the engine's own raises. Git tracking, in-use and recent activity
    /// are not here: each candidate's own classifier owns them (the rubric for artifacts, the
    /// QC rule for caches, the GIT-03 table for worktrees).
    fn reasons(&self) -> Vec<TierReason> {
        let rows = RubricSignals {
            git: GitTracking::NotTracked,
            in_protected_root: self.in_protected_root,
            user_excluded: self.user_excluded,
            protect_marker: self.protect_marker,
            in_use: Tri::No,
            crosses_mount: self.crosses_mount,
            link_escape: self.link_escape,
            cloud_sync: self.cloud.is_some(),
            marker_present: true,
            lockfile_present: true,
            artifact_known: true,
            regen: Regen::Local,
            recent_activity: Tri::No,
        };
        classify(&rows)
            .reasons()
            .iter()
            // The positive row only fires when nothing else did; it never protects anything.
            .filter(|r| r.code != ReasonCode::Regenerable)
            .chain(&self.raises)
            .cloned()
            .collect()
    }
}

/// An unresolvable protection entry, with the paths it may be spelled as.
struct UnresolvedEntry {
    path: PathBuf,
    reason: String,
    /// The entry as configured, its canonical prefix with the rest kept lexically, and its
    /// real path when that could be resolved after all.
    forms: Vec<PathBuf>,
    /// Whether its real location is known (then only overlapping candidates are affected).
    decided: bool,
}

impl UnresolvedEntry {
    fn why(&self) -> String {
        format!(
            "`{}` could not be resolved ({})",
            self.path.display(),
            self.reason
        )
    }
}

/// Where an unresolvable protection entry really is, if that can be known for sure: its real
/// path, or else its parent's real path plus its name when the parent's listing shows it as a
/// plain folder or file. Not a link, it can't stand for any other location.
///
/// The second form is for entries that refuse even an attribute-only open, which Windows'
/// own `C:\System Volume Information` does: neither its metadata nor its real path can be
/// read, but the listing of `C:\` (no open of the entry) says it is an ordinary folder. A
/// link, an unknown kind, or a name the listing doesn't show as spelled (e.g. a short 8.3
/// name) leaves the location undecided, which keeps every candidate at least S2.
fn real_location(platform: &dyn Platform, path: &Path) -> Option<PathBuf> {
    if let Some(real) = canon_exact(path) {
        return Some(real);
    }
    let name = path.file_name()?;
    let parent = canon_exact(path.parent()?)?;
    let listed = platform
        .read_dir(&parent)
        .ok()?
        .filter_map(Result::ok)
        .find(|e| e.file_name == name)?;
    matches!(
        listed.kind,
        scull_platform::EntryKind::Dir | scull_platform::EntryKind::File
    )
    .then(|| parent.join(name))
}

/// Classification context after the walk.
struct Post<'a> {
    engine: &'a Engine,
    /// Built-in protected roots, user `scan.protect` / `scan.exclude` and repository
    /// policies, by file identity (SAFE-03, SAFE-13).
    protected: ProtectedSet,
    /// Entries of `protected` that could not be resolved (never silently dropped).
    unresolved: Vec<UnresolvedEntry>,
    /// Why the mounted-volume list could not be read this run, if it couldn't.
    mounts_unknown: Option<String>,
    /// `scan.min_tier` as given and in canonical form.
    min_tier: Vec<(PathBuf, PathBuf, Tier)>,
    st: &'a State,
    policies: &'a BTreeMap<PathBuf, RepoPolicy>,
    tracking: Tracking<'a>,
    now: i64,
    /// Canonical scan roots the caller reached through an escaping link
    /// ([`Engine::link_escaped_roots`]): everything below them is `link_escape` (S3).
    link_roots: Vec<PathBuf>,
}

impl<'a> Post<'a> {
    /// Mark everything below `roots` as reached through an escaping link.
    fn with_link_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.link_roots = roots;
        self
    }

    fn new(
        engine: &'a Engine,
        st: &'a State,
        policies: &'a BTreeMap<PathBuf, RepoPolicy>,
        now: i64,
    ) -> Self {
        let scan = &engine.config.scan;
        let platform = engine.platform.as_ref();
        let mut protected = ProtectedSet::with_defaults(platform);
        protected.add_config(platform, scan);
        for (dir, policy) in policies {
            protected.add_repo_policy(platform, dir, policy);
        }
        let unresolved = protected
            .unresolved()
            .iter()
            .map(|u| {
                let real = real_location(platform, &u.path);
                let mut forms = vec![u.path.clone(), canon(&u.path)];
                forms.extend(real.clone());
                UnresolvedEntry {
                    path: u.path.clone(),
                    reason: u.reason.clone(),
                    forms,
                    decided: real.is_some(),
                }
            })
            .collect();
        Self {
            engine,
            protected,
            unresolved,
            mounts_unknown: lock(&engine.volumes_error).clone(),
            min_tier: scan
                .min_tier
                .iter()
                .map(|(p, t)| (p.clone(), canon(p), *t))
                .collect(),
            st,
            policies,
            tracking: Tracking::new(engine.git.as_ref(), st.git_roots.iter().cloned()),
            now,
            link_roots: Vec::new(),
        }
    }

    /// Run-wide gaps for `signals_unavailable`, disclosed even when no candidate was hit.
    fn run_gaps(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(e) = &self.mounts_unknown {
            out.push((MOUNTS_SIGNAL, mounts_reason(e)));
        }
        for u in &self.unresolved {
            out.push((
                PROTECTION_SIGNAL,
                format!(
                    "protection entry {}: candidates it might cover are protected",
                    u.why()
                ),
            ));
        }
        out
    }

    /// The engine-side protection signals for `path` (see [`Protect`]). `summary` is the
    /// scanner's result for `path`; with `expect_summary` a missing or incomplete one makes the
    /// candidate's contents unknown (at least S2).
    fn protection(
        &self,
        path: &Path,
        summary: Option<&DirSummary>,
        expect_summary: bool,
    ) -> Protect {
        let platform = self.engine.platform.as_ref();
        let mut p = Protect::default();

        let prot = self.protected.check(path, platform);
        if let Some(root) = self.link_roots.iter().find(|r| is_within(path, r)) {
            p.link_escape = true;
            p.evidence.push(Evidence::LinkEscape {
                target: FsPath::from_path(root),
            });
        }
        p.in_protected_root = prot.in_protected_root;
        p.user_excluded = prot.user_excluded;
        p.protect_marker = prot.protect_marker;
        p.evidence.extend(prot.evidence);

        // A protection entry that could not be resolved is never dropped: it protects by its
        // path, and where its real location is unknown (it may be a link into this folder)
        // every candidate is treated as possibly covered.
        for u in &self.unresolved {
            let why = u.why();
            if u.forms.iter().any(|f| overlaps(path, f)) {
                p.raises.push(TierReason::new(
                    Tier::S3,
                    ReasonCode::ProtectedRoot,
                    format!("matches the protection entry {why}; protected by its path"),
                ));
            } else if !u.decided {
                p.raises.push(TierReason::new(
                    Tier::S2,
                    ReasonCode::SignalUnavailable,
                    format!(
                        "protection entry {why}: it might cover this folder through a link, so \
                         it is treated as possibly protected"
                    ),
                ));
            } else {
                continue;
            }
            p.evidence.push(Evidence::SignalUnavailable {
                signal: PROTECTION_SIGNAL.into(),
                reason: why,
            });
        }

        for (given, c, t) in &self.min_tier {
            if *t > Tier::S0 && overlaps(path, c) {
                p.raises.push(TierReason::new(
                    *t,
                    ReasonCode::UserExclusion,
                    format!("user config sets minimum tier {t} for {}", given.display()),
                ));
            }
        }

        // Markers above the candidate are found by the protected set; this also catches one
        // anywhere *inside* it (deleting the candidate would delete the protected folder).
        for dir in &self.st.protect_markers {
            if overlaps(path, dir) && !p.protect_marker {
                p.protect_marker = true;
                p.evidence.push(Evidence::ProtectMarker {
                    path: FsPath::from_path(&dir.join(REPO_PROTECT_MARKER)),
                });
            }
        }
        for (dir, policy) in self.policies {
            let file = dir.join(REPO_CONFIG_FILE);
            for (rel, t) in policy.min_tier() {
                if overlaps(path, &rel.under(dir)) {
                    p.raises.push(TierReason::new(
                        *t,
                        ReasonCode::ProtectMarker,
                        format!(
                            "{} sets minimum tier {t} for `{}`",
                            file.display(),
                            rel.as_str()
                        ),
                    ));
                }
            }
        }

        let crossing = self.st.other_volume.iter().any(|b| is_within(b, path))
            || self.engine.has_mount_inside(path);
        if crossing {
            p.evidence.push(Evidence::CrossesMount);
        }
        if let Some(e) = &self.mounts_unknown {
            p.evidence.push(Evidence::SignalUnavailable {
                signal: MOUNTS_SIGNAL.into(),
                reason: mounts_reason(e),
            });
        }
        p.crosses_mount = crossing || self.mounts_unknown.is_some();

        // Metadata and paths only: a placeholder is never read (PLT-04).
        let placeholder = self
            .st
            .boundaries
            .iter()
            .find(|(b, r)| *r == BoundaryReason::CloudPlaceholder && is_within(b, path));
        p.cloud = platform
            .metadata(path)
            .ok()
            .and_then(|md| self.engine.synced_roots.cloud_state(path, &md))
            .map(|c| c.provider.as_str().to_owned())
            .or_else(|| {
                self.st
                    .reparse_dirs
                    .iter()
                    .any(|d| is_within(d, path))
                    .then(|| "reparse point (possible cloud placeholder)".to_owned())
            })
            .or_else(|| placeholder.map(|_| "online-only placeholder inside".to_owned()));
        if let Some(provider) = &p.cloud {
            p.evidence.push(Evidence::CloudSync {
                provider: provider.clone(),
            });
        }
        if let Some((b, _)) = placeholder {
            p.evidence.push(Evidence::Note {
                text: format!(
                    "{} is an online-only cloud placeholder: not listed (nothing downloaded), \
                     its contents are unknown",
                    b.display()
                ),
            });
        }

        // Completeness: whatever the walk could not see (unreadable or timed-out folders,
        // folders not descended) may hold tracked files, a nested repository or a
        // `.scull-protect` marker. Unknown, never "nothing there" (PRD §8.4).
        let mut gaps: Vec<String> = Vec::new();
        if expect_summary {
            match summary {
                None => gaps.push("it could not be measured".into()),
                Some(s) if !s.complete => gaps.push(
                    "part of it could not be read (permission denied, I/O error or time-out)"
                        .into(),
                ),
                Some(_) => {}
            }
        }
        let inside = |b: &Path| b != path && is_within(b, path);
        if let Some((b, r)) = self
            .st
            .boundaries
            .iter()
            .find(|(b, r)| *r != BoundaryReason::CloudPlaceholder && inside(b))
        {
            gaps.push(format!(
                "{} inside was not descended ({})",
                b.display(),
                boundary_text(*r)
            ));
        }
        if expect_summary && let Some(s) = summary {
            // Backstop: the summary counts every boundary below it; any the walk didn't
            // report to us is just as unknown.
            let seen = self.st.other_volume.iter().filter(|b| inside(b)).count()
                + self.st.boundaries.iter().filter(|(b, _)| inside(b)).count();
            if s.totals.boundaries > seen as u64 {
                gaps.push("some folders inside were not descended".into());
            }
        }
        if !gaps.is_empty() {
            let reason = format!(
                "{}: what it holds (tracked files, nested repositories, protection markers) is \
                 unknown",
                gaps.join("; ")
            );
            p.raises.push(TierReason::new(
                Tier::S2,
                ReasonCode::SignalUnavailable,
                format!("not fully observed: {reason}"),
            ));
            p.evidence.push(Evidence::SignalUnavailable {
                signal: COMPLETENESS_SIGNAL.into(),
                reason,
            });
        }

        // A Git repository inside may hold unique, unpushed work.
        for g in &self.st.git_roots {
            if g != path && is_within(g, path) {
                p.raises.push(TierReason::new(
                    Tier::S3,
                    ReasonCode::GitTracked,
                    nested_repo_message(g),
                ));
                p.nested_repos.push(g.clone());
            }
        }
        p
    }

    /// Every signal for a detector-built candidate (artifact or cache): the protection step's,
    /// plus Git tracking, recent activity and in-use.
    fn signals(
        &mut self,
        path: &Path,
        summary: Option<&DirSummary>,
        in_use: InUseAnswer,
    ) -> Signals {
        let protect = self.protection(path, summary, true);
        let mut evidence = Vec::new();

        let (git, ev) = self.tracking.of(path, summary);
        evidence.extend(ev);

        let mut window = self.engine.config.scan.activity_window.as_secs();
        for (dir, policy) in self.policies {
            if is_within(path, dir)
                && let Some(w) = policy.activity_window()
            {
                window = window.max(w.as_secs());
            }
        }
        // The scanner's newest mtime and completeness; an incomplete subtree only ever says
        // "recent" or "unknown", never "idle" (PRD §8.4).
        let sample = match summary {
            Some(s) => MtimeSample::from_summary(s.newest_mtime, s.complete),
            None => MtimeSample::from_summary(None, false),
        };
        let activity = recent_activity(ActivityWindow::from_secs(window), sample, self.now);
        evidence.push(activity.evidence);
        evidence.extend(in_use.evidence);

        Signals {
            caller: CallerSignals {
                git,
                in_protected_root: protect.in_protected_root,
                user_excluded: protect.user_excluded,
                protect_marker: protect.protect_marker,
                in_use: in_use.in_use,
                crosses_mount: protect.crosses_mount,
                link_escape: protect.link_escape,
                cloud_sync: protect.cloud.is_some(),
                recent_activity: activity.recent_activity,
            },
            evidence,
            protect,
        }
    }
}

fn mounts_reason(e: &str) -> String {
    format!(
        "the mounted-volume list could not be read ({e}); a bind mount inside a candidate can't \
         be ruled out, so every candidate is treated as crossing a mount (S3)"
    )
}

fn boundary_text(r: BoundaryReason) -> &'static str {
    match r {
        BoundaryReason::MaxDepth => "depth limit",
        BoundaryReason::Loop => "directory loop",
        BoundaryReason::OtherVolume => "another volume",
        BoundaryReason::CloudPlaceholder => "cloud placeholder",
        _ => "not descended",
    }
}

fn merge_evidence(into: &mut Vec<Evidence>, from: &[Evidence]) {
    for e in from {
        if !into.contains(e) {
            into.push(e.clone());
        }
    }
}

/// **The** protection step: fold the engine-side protection signals into a candidate with
/// [`Candidate::raise`] (tier only goes up, action constrained to it). Every candidate the
/// engine emits goes through here: artifacts, shared target directories, caches, worktrees.
/// Which "repositories" inside a cache belong to the tool itself (#139), for the tools whose
/// cache is cleaned only with their own command (`uv cache prune` / `uv cache clean`; the
/// detector never gives a uv cache a delete or quarantine action):
///
/// * uv's Git cache, `git-v0` (clones in `db`, checkouts in `checkouts`);
/// * uv's marker files: an **empty** regular `.git` file (uv writes one into buckets such as
///   `sdists-v9` so Git doesn't treat the cache as part of an enclosing repository). A real
///   `.git` file points to a Git directory (`gitdir: …`), so an empty one holds no history.
fn tool_owned_repos<'a>(
    tool: CacheTool,
    cache: &Path,
    platform: &'a dyn Platform,
) -> Option<impl Fn(&Path) -> bool + 'a> {
    match tool {
        CacheTool::Uv => {
            let git = cache.join("git-v0");
            Some(move |repo: &Path| {
                is_within(repo, &git)
                    || platform
                        .metadata(&repo.join(".git"))
                        .is_ok_and(|m| m.kind == EntryKind::File && m.logical_size == 0)
            })
        }
        _ => None,
    }
}

fn apply_protection(c: &mut Candidate, p: &Protect) {
    merge_evidence(&mut c.evidence, &p.evidence);
    for r in p.reasons() {
        if !c.reasons.contains(&r) {
            c.raise(r);
        }
    }
    c.flags.cross_mount = Some(p.crosses_mount || c.flags.cross_mount == Some(true));
    c.flags.cloud_placeholder = Some(p.cloud.is_some() || c.flags.cloud_placeholder == Some(true));
}

/// The same protection applied to a worktree's GIT-03 decision, so the report (and
/// `scull worktrees inspect`) agrees with the candidate built from it.
fn protect_decision(d: &mut WorktreeDecision, p: &Protect) {
    merge_evidence(&mut d.evidence, &p.evidence);
    for r in p.reasons() {
        if !d.reasons.contains(&r) {
            d.tier = d.tier.max_protective(r.tier);
            d.reasons.push(r);
        }
    }
    let action = std::mem::replace(&mut d.action, Action::None);
    d.action = action.constrained_to(d.tier);
}

/// Apply the engine's extra evidence, protection and sizes to a detector-built candidate.
fn finish_candidate(
    c: &mut Candidate,
    sig: Signals,
    summary: Option<&DirSummary>,
    sizes: Option<Sizes>,
) {
    merge_evidence(&mut c.evidence, &sig.evidence);
    apply_protection(c, &sig.protect);
    if let Some(sizes) = sizes {
        c.sizes = sizes;
    }
    if let Some(s) = summary {
        c.flags.hardlinked = Some(s.shared_allocated > 0);
        if s.shared_allocated > 0 {
            let e = Evidence::Hardlinked {
                shared_bytes: s.shared_allocated,
            };
            if !c.evidence.contains(&e) {
                c.evidence.push(e);
            }
        }
    }
}

/// Ask the run's probe about `dir`, or report in-use as unavailable.
fn ask(probe: &RunProbe, dir: &Path, lock_files: &[PathBuf], processes: &[String]) -> InUseAnswer {
    match probe {
        Some(p) => p.check(dir, lock_files, processes),
        None => in_use_unavailable(),
    }
}

/// One candidate per item ID (the first wins), most reclaimable first, then by path.
fn dedupe(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen: HashSet<ItemId> = HashSet::new();
    let mut out: Vec<Candidate> = candidates
        .into_iter()
        .filter(|c| seen.insert(c.id.clone()))
        .collect();
    out.sort_by(|a, b| {
        b.sizes
            .reclaimable
            .bytes
            .cmp(&a.sizes.reclaimable.bytes)
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// Every unavailable signal named in candidate evidence, with how many candidates it hit.
fn signals_unavailable(candidates: &[Candidate]) -> Vec<SignalUnavailable> {
    let mut out: Vec<SignalUnavailable> = Vec::new();
    for c in candidates {
        let mut seen = BTreeSet::new();
        for e in &c.evidence {
            if let Evidence::SignalUnavailable { signal, reason } = e
                && seen.insert(signal.clone())
            {
                push_signal(&mut out, signal, reason, 1);
            }
        }
    }
    out
}

fn push_signal(out: &mut Vec<SignalUnavailable>, signal: &str, reason: &str, n: u64) {
    match out.iter_mut().find(|s| s.signal == signal) {
        Some(s) => s.affected += n,
        None => out.push(SignalUnavailable {
            signal: signal.to_owned(),
            reason: reason.to_owned(),
            affected: n,
        }),
    }
}

/// Projects: directories with markers, plus the owners of candidates.
fn projects(st: &State, candidates: &[Candidate], tracking: &Tracking<'_>) -> Vec<ProjectSummary> {
    let mut map: BTreeMap<PathBuf, ProjectSummary> = BTreeMap::new();
    let blank = |root: &Path| ProjectSummary {
        root: FsPath::from_path(root),
        name: root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned()),
        ecosystems: Vec::new(),
        markers: Vec::new(),
        git_worktree: tracking.worktree_of(root).map(FsPath::from_path),
        candidates: 0,
        allocated: TierBreakdown::default(),
        reclaimable: TierBreakdown::default(),
        badge: None,
        total_allocated: st.summaries.get(root).map(|s| s.totals.allocated),
        newest_mtime: st.summaries.get(root).and_then(|s| s.newest_mtime),
    };
    for (dir, markers) in &st.projects {
        let p = map.entry(dir.clone()).or_insert_with(|| blank(dir));
        for m in markers {
            p.markers.push(m.clone());
            if let Some((_, eco)) = PROJECT_MARKERS.iter().find(|(n, _)| n == m) {
                p.ecosystems.push((*eco).to_owned());
            }
        }
    }
    for c in candidates {
        if c.category == Category::Worktree {
            continue;
        }
        let Some(pr) = &c.project else { continue };
        let root = pr.root.to_path_buf();
        let p = map.entry(root.clone()).or_insert_with(|| blank(&root));
        p.ecosystems.push(pr.ecosystem.clone());
        p.candidates += 1;
        p.allocated.add(c.tier, c.sizes.allocated);
        p.reclaimable.add(c.tier, c.sizes.reclaimable.bytes);
        p.badge = Some(p.badge.map_or(c.tier, |b| b.max_protective(c.tier)));
    }
    map.into_values()
        .map(|mut p| {
            p.ecosystems.sort();
            p.ecosystems.dedup();
            p
        })
        .collect()
}
