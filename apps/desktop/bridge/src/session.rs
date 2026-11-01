//! The desktop session: one [`Session`] per app, shared by every command.
//!
//! It loads the configuration the same way the CLI does (user file, then environment), builds
//! a read-only [`Engine`] per operation, keeps the last scan for the Projects and Worktrees
//! screens, and tracks running operations so the UI can cancel them.

use crate::insights::{
    self, FootprintHistory, FreeSpaceHistory, RegrowthHistory, RollupHistory, Source,
};
use crate::views::{
    CapabilitiesReport, DesktopStatus, GitInfo, HistoryState, LastScan, OpId, ProgressEvent,
    ProjectsView, RECENT_ALERTS_SECS, ReviewPlanView, ScanParams, StatusState, VolumeSampleView,
    WorktreesView, recent_alerts,
};
use scull_core::config::{Config, ConfigLayer, EnvLayer, ResolvedConfig, resolve, validate_roots};
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_detectors::CacheEnv;
use scull_detectors::rules::Platform as RulesOs;
use scull_engine::paths::canon;
use scull_engine::{
    CachesReport, CancelToken, Engine, EngineConfig, EngineProgress, InUseSource, ScanReport,
    ScanRequest, history_db_path, open_history,
};
use scull_git::{GitRunner, RunnerOptions};
use scull_platform::{Native, Platform, SyncedRoots};
use scull_safety::GapPolicy;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// File name of the user configuration in scull's config directory (same as the CLI).
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Minimum time between two `scanning` progress events of one operation.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Environment lookup.
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Where the session reads its configuration and keeps history. [`Setup::native`] is the real
/// machine; tests point everything at temp directories.
#[derive(Clone)]
pub struct Setup {
    /// The user's home folder.
    pub home: Option<PathBuf>,
    /// scull's config file (`None`: defaults only).
    pub config_file: Option<PathBuf>,
    /// scull's data directory (history database). `None`: no history.
    pub data_dir: Option<PathBuf>,
    /// Environment variables (`SCULL_*` config layer, cache locations).
    pub env: EnvFn,
    /// Record scans in the history database.
    pub record_history: bool,
    /// Use the hardened `git` when one is found.
    pub git: bool,
    /// Detect cloud-synced folders on this machine.
    pub detect_synced_roots: bool,
    /// Allow scan roots on network or virtual volumes (D-22). Off in the app.
    pub allow_network_roots: bool,
    /// Native in-use probes (D-30). Without them every candidate is "in use: unknown".
    pub in_use: bool,
}

impl Setup {
    /// The real machine: the platform's config and data directories, the process
    /// environment, history on, Git and in-use probes on.
    pub fn native() -> Self {
        let dirs = scull_platform::app_dirs().ok();
        Self {
            home: home_from(&|k| std::env::var(k).ok()),
            config_file: dirs.as_ref().map(|d| d.config.join(CONFIG_FILE_NAME)),
            data_dir: dirs.map(|d| d.data),
            env: Arc::new(|k| std::env::var(k).ok()),
            record_history: true,
            git: true,
            detect_synced_roots: true,
            allow_network_roots: false,
            in_use: true,
        }
    }
}

fn home_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// How long a cancellation that arrived before its operation started is remembered.
pub const EARLY_CANCEL_TTL: Duration = Duration::from_secs(10);

/// At most this many early cancellations are remembered.
const EARLY_CANCEL_MAX: usize = 64;

/// One desktop session.
pub struct Session {
    setup: Setup,
    ops: Mutex<HashMap<String, CancelToken>>,
    /// Cancellations for operations that hadn't started yet: the UI's `cancel_operation` can
    /// overtake the `scan` call it cancels (both are async commands).
    early_cancels: Mutex<Vec<(String, Instant)>>,
    last_scan: Mutex<Option<Arc<ScanReport>>>,
    /// What a desktop plan needs from the last scan besides its candidates (P3-EXEC-01).
    last_scan_basis: Mutex<Option<ScanBasis>>,
    /// A caller-supplied in-use source instead of [`Setup::in_use`] (tests).
    in_use_source: Option<InUseSource>,
}

/// The last scan's roots, configuration and SAFE-09 watermark, for planning from it.
#[derive(Debug, Clone)]
pub(crate) struct ScanBasis {
    /// The scan's (canonical) roots.
    pub(crate) roots: Vec<PathBuf>,
    /// The configuration it ran with.
    pub(crate) config: Config,
    /// `Store::operations_watermark`, read **before** the scan started (`None` when it could
    /// not be read: a plan made from this scan is then refused).
    pub(crate) watermark: Option<u64>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Unregisters an operation when it ends, however it ends.
struct OpGuard<'a> {
    session: &'a Session,
    id: String,
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        lock(&self.session.ops).remove(&self.id);
    }
}

impl Session {
    /// A session over `setup`.
    pub fn new(setup: Setup) -> Self {
        Self {
            setup,
            ops: Mutex::new(HashMap::new()),
            early_cancels: Mutex::new(Vec::new()),
            last_scan: Mutex::new(None),
            last_scan_basis: Mutex::new(None),
            in_use_source: None,
        }
    }

    /// Use `source` to tell whether candidates are in use, for every scan and revalidation of
    /// this session (tests: a fixed answer instead of the OS's process list).
    #[must_use]
    pub fn with_in_use_source(mut self, source: InUseSource) -> Self {
        self.in_use_source = Some(source);
        self
    }

    /// The setup in use.
    pub fn setup(&self) -> &Setup {
        &self.setup
    }

    /// The effective configuration: user config file > `SCULL_*` environment > defaults, with
    /// `~` expanded. An invalid file or variable is a configuration error.
    pub fn config(&self) -> Result<Config, ScullError> {
        let resolved = self.resolved_config()?;
        Ok(match &self.setup.home {
            Some(h) => resolved.config.with_home_expanded(h),
            None => resolved.config,
        })
    }

    /// The effective configuration as written (`~` not expanded) and where each value came
    /// from (`scull config show`). Reads the config file; never writes it.
    pub fn resolved_config(&self) -> Result<ResolvedConfig, ScullError> {
        let mut user = ConfigLayer::default();
        if let Some(path) = &self.setup.config_file {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    user = ConfigLayer::from_toml(&text)
                        .map_err(|e| e.with_path(FsPath::from_path(path)))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(ScullError::new(
                        ErrorKind::Config,
                        format!("cannot read the config file: {e}"),
                    )
                    .with_path(FsPath::from_path(path)));
                }
            }
        }
        let env = Arc::clone(&self.setup.env);
        let env = EnvLayer::from_lookup(move |k| env(k))?;
        Ok(resolve(&ConfigLayer::default(), &env, &user))
    }

    /// Settings screen: effective configuration with origins, built-in protection and scull's
    /// own data (read-only; [`crate::settings`]).
    pub fn settings(&self) -> crate::settings::SettingsView {
        crate::settings::settings_view(self)
    }

    fn register(&self, op: &OpId) -> Result<(CancelToken, OpGuard<'_>), ScullError> {
        let mut ops = lock(&self.ops);
        if ops.contains_key(op.as_str()) {
            return Err(ScullError::new(
                ErrorKind::Usage,
                format!("operation {} is already running", op.as_str()),
            ));
        }
        let token = CancelToken::new();
        let mut early = lock(&self.early_cancels);
        early.retain(|(_, at)| at.elapsed() < EARLY_CANCEL_TTL);
        if let Some(i) = early.iter().position(|(id, _)| id == op.as_str()) {
            early.remove(i);
            token.cancel();
        }
        drop(early);
        ops.insert(op.as_str().to_owned(), token.clone());
        Ok((
            token,
            OpGuard {
                session: self,
                id: op.as_str().to_owned(),
            },
        ))
    }

    /// Cancel an operation. `true` when it was running. Otherwise the request is remembered
    /// for [`EARLY_CANCEL_TTL`], so an operation that starts with that ID shortly after (the
    /// cancel overtook its start) is cancelled at once; IDs are meant to be unique (UUIDs).
    pub fn cancel(&self, op: &OpId) -> bool {
        if let Some(t) = lock(&self.ops).get(op.as_str()) {
            t.cancel();
            return true;
        }
        let mut early = lock(&self.early_cancels);
        early.retain(|(_, at)| at.elapsed() < EARLY_CANCEL_TTL);
        if early.len() >= EARLY_CANCEL_MAX {
            early.remove(0);
        }
        early.push((op.as_str().to_owned(), Instant::now()));
        false
    }

    /// IDs of the operations running now.
    pub fn running(&self) -> Vec<String> {
        let mut v: Vec<String> = lock(&self.ops).keys().cloned().collect();
        v.sort();
        v
    }

    fn engine(
        &self,
        config: Config,
        roots: &[PathBuf],
        history: bool,
    ) -> Result<Engine, ScullError> {
        let mut cfg = EngineConfig::new(config, Arc::new(Native));
        if self.setup.git {
            let opts = RunnerOptions {
                excluded_roots: roots.to_vec(),
                ..RunnerOptions::default()
            };
            cfg.git = GitRunner::with_options(opts).ok();
        }
        if history && let Some(data) = &self.setup.data_dir {
            // History is best effort: a scan still runs when the database can't be opened
            // (the report's `history` field says what happened).
            cfg.store = open_history(data).ok();
        }
        if let Some(src) = &self.in_use_source {
            cfg.in_use = src.clone();
        } else if self.setup.in_use {
            cfg.in_use = InUseSource::Native(GapPolicy::default());
        }
        if self.setup.detect_synced_roots {
            cfg.synced_roots = SyncedRoots::detect();
        }
        cfg.allow_network_roots = self.setup.allow_network_roots;
        Engine::new(cfg)
    }

    /// The roots to scan: the request's, or `scan.roots` from the configuration; canonical and
    /// checked against CFG-04 (a filesystem root, the home folder or a folder containing it
    /// needs `allow_broad_roots`).
    pub fn roots(&self, config: &Config, params: &ScanParams) -> Result<Vec<PathBuf>, ScullError> {
        let roots: Vec<PathBuf> = if params.roots.is_empty() {
            config.scan.roots.clone()
        } else {
            params.roots.iter().map(PathBuf::from).collect()
        };
        if let Some(r) = roots.iter().find(|r| !r.is_absolute()) {
            return Err(
                ScullError::new(ErrorKind::Usage, "scan roots must be absolute paths")
                    .with_path(FsPath::from_path(r)),
            );
        }
        let roots: Vec<PathBuf> = roots.iter().map(|r| canon(r)).collect();
        let home = self
            .setup
            .home
            .as_deref()
            .map(canon)
            .unwrap_or_else(|| PathBuf::from("/nonexistent-home-for-scull"));
        let concerns = validate_roots(&roots, &home, &[]).map_err(|e| {
            if roots.is_empty() {
                ScullError::new(
                    ErrorKind::Usage,
                    "no scan roots: choose a folder to scan, or set scan.roots in the config file",
                )
            } else {
                e
            }
        })?;
        if !concerns.is_empty() && !params.allow_broad_roots {
            let msgs: Vec<String> = concerns.iter().map(|c| c.message.clone()).collect();
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!(
                    "{}; confirm the broad scan to run it anyway",
                    msgs.join("; ")
                ),
            ));
        }
        Ok(roots)
    }

    /// Scan and classify (read-only). Progress goes to `emit`, tagged with `op`; [`Self::cancel`]
    /// stops the walk and the report comes back with `cancelled: true`.
    pub fn scan(
        &self,
        op: &OpId,
        params: &ScanParams,
        emit: impl Fn(ProgressEvent) + Send + Sync + 'static,
    ) -> Result<ScanReport, ScullError> {
        let config = self.config()?;
        let roots = self.roots(&config, params)?;
        let (token, _guard) = self.register(op)?;
        let history = self.setup.record_history && params.record_history.unwrap_or(true);
        // SAFE-09: the last recorded operation before the scan starts; any operation recorded
        // after it may have touched what the scan observes and invalidates a plan made from it.
        let watermark = self.operations_watermark();
        let basis = ScanBasis {
            roots: roots.clone(),
            config: config.clone(),
            watermark,
        };
        let engine = self.engine(config, &roots, history)?;
        let req = ScanRequest::full().history(history);
        let report = engine.scan(&roots, &req, &token, throttled(op.clone(), emit))?;
        // A cancelled scan is partial: it never replaces a complete one.
        let mut last = lock(&self.last_scan);
        if !report.cancelled || last.is_none() {
            *last = Some(Arc::new(report.clone()));
            *lock(&self.last_scan_basis) = Some(basis);
        }
        Ok(report)
    }

    /// The SAFE-09 watermark now: 0 without a database (no operation was ever recorded);
    /// `None` when the database exists but can't be read.
    pub(crate) fn operations_watermark(&self) -> Option<u64> {
        let Some(data) = &self.setup.data_dir else {
            return Some(0);
        };
        let db = history_db_path(data);
        if !db.is_file() {
            return Some(0);
        }
        scull_storage::Store::open_read_only(&db, &scull_storage::StoreOptions::default())
            .and_then(|s| s.operations_watermark())
            .ok()
    }

    /// The last complete scan with its roots, configuration and watermark.
    pub(crate) fn last_scan_basis(&self) -> Option<(Arc<ScanReport>, ScanBasis)> {
        let scan = self.last_scan()?;
        let basis = lock(&self.last_scan_basis).clone()?;
        Some((scan, basis))
    }

    /// An engine for executing or revalidating under `roots` (no history recording).
    pub(crate) fn engine_for(
        &self,
        config: Config,
        roots: &[PathBuf],
    ) -> Result<Engine, ScullError> {
        self.engine(config, roots, false)
    }

    /// The last complete scan of this session (or the last cancelled one, before any complete
    /// scan), if any.
    pub fn last_scan(&self) -> Option<Arc<ScanReport>> {
        lock(&self.last_scan).clone()
    }

    /// Projects from the last scan (`scan_id: null` and an empty list before the first one).
    pub fn projects(&self) -> ProjectsView {
        ProjectsView::of(self.last_scan().as_deref())
    }

    /// Git worktrees from the last scan.
    pub fn worktrees(&self) -> WorktreesView {
        WorktreesView::of(self.last_scan().as_deref())
    }

    /// Locate and classify package-manager caches (Quick Clean candidates; read-only).
    pub fn caches(
        &self,
        op: &OpId,
        emit: impl Fn(ProgressEvent) + Send + Sync + 'static,
    ) -> Result<CachesReport, ScullError> {
        let config = self.config()?;
        let home = self.setup.home.clone().ok_or_else(|| {
            ScullError::new(
                ErrorKind::Config,
                "the home folder is not known (HOME / USERPROFILE)",
            )
        })?;
        let os = RulesOs::current().ok_or_else(|| {
            ScullError::new(ErrorKind::UnsupportedPlatform, "no cache rules for this OS")
        })?;
        let env = Arc::clone(&self.setup.env);
        let cache_env = CacheEnv::new(home, os, move |k| env(k));
        let (token, _guard) = self.register(op)?;
        let threads = config.scan.threads as usize;
        let engine = self.engine(config, &[], false)?;
        Ok(engine.caches(&cache_env, threads, &token, throttled(op.clone(), emit)))
    }

    /// Last recorded scan and free-space samples from the history database. Never creates or
    /// migrates the database: a missing one reads `no_database`.
    pub fn status(&self, now: i64) -> DesktopStatus {
        let session_scan = self.last_scan().map(|r| LastScan::of_report(&r, now));
        let Some(data) = &self.setup.data_dir else {
            return DesktopStatus::new(now, StatusState::NoDatabase, None, session_scan);
        };
        if !history_db_path(data).is_file() {
            return DesktopStatus::new(now, StatusState::NoDatabase, None, session_scan);
        }
        let store = match open_history(data) {
            Ok(store) => store,
            Err(e) => {
                return DesktopStatus::new(
                    now,
                    StatusState::Unavailable,
                    Some(e.to_string()),
                    session_scan,
                );
            }
        };
        match store.status_summary() {
            Ok(summary) => {
                let mut st = DesktopStatus::new(
                    now,
                    StatusState::NoScan,
                    None,
                    summary
                        .last_scan
                        .as_ref()
                        .map(|l| LastScan::of_history(l, now)),
                );
                match (&mut st.last_scan, session_scan) {
                    // History doesn't keep the root count; this session's scan does.
                    (Some(h), Some(s)) if h.scan_id == s.scan_id => h.roots = s.roots,
                    (None, s) => st.last_scan = s,
                    _ => {}
                }
                if summary.last_scan.is_some() {
                    st.state = StatusState::Ok;
                }
                st.volumes = summary.volumes.iter().map(VolumeSampleView::from).collect();
                // Announcements never break the status: unreadable ones are left out.
                st.review_plan = scull_proactive::review::latest_review_plan(&store, now)
                    .ok()
                    .flatten()
                    .map(|p| ReviewPlanView::from(&p));
                if let Ok(recorded) = store.alerts(now - RECENT_ALERTS_SECS) {
                    let alerts: Vec<serde_json::Value> =
                        recorded.into_iter().map(|a| a.alert).collect();
                    st.alerts = recent_alerts(&alerts, now);
                }
                st
            }
            Err(e) => DesktopStatus::new(
                now,
                StatusState::Unavailable,
                Some(e.to_string()),
                session_scan,
            ),
        }
    }

    /// Recorded cleanup operations, newest first, with their audit logs (History screen).
    /// Read-only: never creates, migrates or writes the database ([`crate::history`]).
    pub fn history(&self, now: i64) -> crate::history::HistoryView {
        crate::history::read_history(self.setup.data_dir.as_deref(), now)
    }

    /// The history database for a read, never created or migrated here: a missing one is
    /// [`Source::Missing`].
    pub(crate) fn history_source(&self) -> Source {
        let Some(data) = &self.setup.data_dir else {
            return Source::Missing;
        };
        if !history_db_path(data).is_file() {
            return Source::Missing;
        }
        match open_history(data) {
            Ok(s) => Source::Store(s),
            Err(e) => Source::Failed(e.to_string()),
        }
    }

    /// Free-space samples of one volume since `since` (Insights forecast). `volume_id`
    /// defaults to the home folder's volume, then `/`, then the most recently sampled one.
    pub fn free_space_history(&self, since: i64, volume_id: Option<&str>) -> FreeSpaceHistory {
        let home_volume = self
            .setup
            .home
            .as_deref()
            .and_then(|h| scull_platform::volume_of(h).ok())
            .map(|v| v.mount_point.to_string_lossy().into_owned());
        insights::free_space_history(
            &self.history_source(),
            since,
            volume_id,
            home_volume.as_deref(),
        )
    }

    /// Bytes per category per usable scan since `since` (Insights growth and mix).
    pub fn rollup_history(&self, since: i64) -> RollupHistory {
        insights::rollup_history(&self.history_source(), since)
    }

    /// Artifacts that came back since `since` (Insights regrowth).
    pub fn regrowth(&self, since: i64) -> RegrowthHistory {
        insights::regrowth(&self.history_source(), since)
    }

    /// scull's own footprint samples since `since` ("What scull knows").
    pub fn footprint_history(&self, since: i64) -> FootprintHistory {
        insights::footprint_history(&self.history_source(), since)
    }

    /// What this machine and build can observe (a doctor-like summary for Settings and the
    /// "signals unavailable" disclosure).
    pub fn capabilities(&self) -> CapabilitiesReport {
        let caps = Native.capabilities();
        let git = if self.setup.git {
            match GitRunner::new() {
                Ok(g) => GitInfo {
                    available: true,
                    version: Some(g.version().to_string()),
                    detail: None,
                },
                Err(e) => GitInfo {
                    available: false,
                    version: None,
                    detail: Some(e.to_string()),
                },
            }
        } else {
            GitInfo {
                available: false,
                version: None,
                detail: Some("git is turned off for this session".into()),
            }
        };
        let history = match &self.setup.data_dir {
            None => HistoryState::Off,
            Some(d) if history_db_path(d).is_file() => HistoryState::Present,
            Some(_) => HistoryState::NotCreated,
        };
        CapabilitiesReport::new(
            &caps,
            scull_platform::is_elevated(),
            git,
            self.setup.in_use,
            history,
            self.setup.config_file.as_deref().map(path_string),
            self.setup.data_dir.as_deref().map(path_string),
        )
    }
}

fn path_string(p: &Path) -> String {
    p.display().to_string()
}

/// Wrap `emit` into an engine progress callback: `scanning` events at most every
/// [`PROGRESS_INTERVAL`], `analyzing` events always.
fn throttled(
    op: OpId,
    emit: impl Fn(ProgressEvent) + Send + Sync + 'static,
) -> impl Fn(EngineProgress) + Send + Sync + 'static {
    let last = Mutex::new(None::<Instant>);
    move |p| {
        if let EngineProgress::Scanning { .. } = &p {
            let mut last = lock(&last);
            if last.is_some_and(|t| t.elapsed() < PROGRESS_INTERVAL) {
                return;
            }
            *last = Some(Instant::now());
        }
        if let Some(ev) = ProgressEvent::of(&op, &p) {
            emit(ev);
        }
    }
}
