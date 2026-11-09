//! Shared setup for the read-only commands (`scan`, `projects`, `worktrees`, `caches`): the
//! effective configuration (CFG-02), scan roots (CFG-04), the [`Engine`], Ctrl-C bridging and
//! the progress line on stderr (CLI-02).

use crate::cancel;
use crate::cmd::doctor::CONFIG_FILE_NAME;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::style::Style;
use scull_core::config::{Config, ConfigLayer, EnvLayer, resolve, validate_roots};
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_engine::paths::canon;
use scull_engine::{CancelToken, Engine, EngineConfig, EngineProgress, InUseSource, open_history};
use scull_git::{GitRunner, RunnerOptions};
use scull_platform::{Native, SyncedRoots, app_dirs};
use scull_safety::GapPolicy;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The flag that confirms a broad scan root (CFG-04).
pub const BROAD_ROOTS_FLAG: &str = "--allow-broad-roots";

/// The flag that allows scan roots on network or virtual volumes (D-22).
pub const NETWORK_ROOTS_FLAG: &str = "--allow-network-roots";

/// The effective configuration and the user's home folder.
pub struct Loaded {
    /// CLI flags > environment > user config file > defaults, `~` expanded.
    pub config: Config,
    /// The home folder, if known.
    pub home: Option<PathBuf>,
    /// `--allow-network-roots`: scan roots on network or virtual volumes (D-22).
    pub allow_network_roots: bool,
}

/// Load the configuration. `cli_roots` (from the command line, made absolute against the
/// working directory) override `scan.roots` (CFG-02). An invalid config file or environment
/// variable is a configuration error (exit 2).
pub fn load_config(cli_roots: &[PathBuf]) -> Result<Loaded, ScullError> {
    let mut user = ConfigLayer::default();
    if let Ok(dirs) = app_dirs() {
        let path = dirs.config.join(CONFIG_FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                user = ConfigLayer::from_toml(&text)
                    .map_err(|e| e.with_path(FsPath::from_path(&path)))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ScullError::new(
                    ErrorKind::Config,
                    format!("cannot read the config file: {e}"),
                )
                .with_path(FsPath::from_path(&path)));
            }
        }
    }
    let env = EnvLayer::from_lookup(|k| std::env::var(k).ok())?;
    let mut cli = ConfigLayer::default();
    if !cli_roots.is_empty() {
        let cwd = std::env::current_dir().map_err(|e| ScullError::from_io(&e))?;
        cli.scan.roots = Some(cli_roots.iter().map(|p| cwd.join(p)).collect());
    }
    let resolved = resolve(&cli, &env, &user);
    let home = scull_safety::protect::home_from_env();
    let config = match &home {
        Some(h) => resolved.config.with_home_expanded(h),
        None => resolved.config,
    };
    Ok(Loaded {
        config,
        home,
        allow_network_roots: false,
    })
}

/// The scan roots: `scan.roots` (CLI paths or config), canonical, and checked against CFG-04.
/// A filesystem root, the home folder or a folder containing it needs `allow_broad`.
pub fn roots(loaded: &Loaded, allow_broad: bool) -> Result<Vec<PathBuf>, ScullError> {
    let roots: Vec<PathBuf> = loaded
        .config
        .scan
        .roots
        .iter()
        .map(|p| canon_if_absolute(p))
        .collect();
    let home = loaded
        .home
        .as_deref()
        .map(canon)
        .unwrap_or_else(|| PathBuf::from("/nonexistent-home-for-scull"));
    let concerns = validate_roots(&roots, &home, &[]).map_err(|e| {
        if roots.is_empty() {
            ScullError::new(
                ErrorKind::Usage,
                "no scan roots: run `scull init` to pick your project folders, or pass one \
                 (`scull scan <folder>`, `scull clean --root <folder>`)",
            )
        } else {
            e
        }
    })?;
    if !concerns.is_empty() && !allow_broad {
        let msgs: Vec<String> = concerns.iter().map(|c| c.message.clone()).collect();
        return Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            format!(
                "{}; pass {BROAD_ROOTS_FLAG} to scan it anyway",
                msgs.join("; ")
            ),
        ));
    }
    // Check every root before anything is opened (no history database for a typo).
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    cfg.allow_network_roots = loaded.allow_network_roots;
    let probe = Engine::new(cfg)?;
    probe.prepare_roots(&roots).map_err(|mut e| {
        if e.kind == ErrorKind::SafetyBlocked && e.message.contains("unless you opt in") {
            e.message
                .push_str(&format!("; pass {NETWORK_ROOTS_FLAG} to scan it anyway"));
        }
        e
    })
}

fn canon_if_absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        canon(p)
    } else {
        p.to_path_buf()
    }
}

/// Build the engine: hardened `git` (never resolved from inside a scan root), native in-use
/// probes (D-30 gap policy), and the history database when `history` is set.
pub fn engine(
    loaded: &Loaded,
    roots: &[PathBuf],
    history: bool,
    ui: &Ui,
) -> Result<Engine, ScullError> {
    let opts = RunnerOptions {
        excluded_roots: roots.to_vec(),
        ..RunnerOptions::default()
    };
    let git = match GitRunner::with_options(opts) {
        Ok(g) => Some(g),
        Err(e) => {
            ui.debug(&format!("git unavailable: {e}"));
            None
        }
    };
    let store = if history {
        match app_dirs() {
            Ok(dirs) => match open_history(&dirs.data) {
                Ok(s) => Some(s),
                Err(e) => {
                    warn(ui, &format!("history is not recorded this time: {e}"));
                    None
                }
            },
            Err(e) => {
                warn(ui, &format!("history is not recorded this time: {e}"));
                None
            }
        }
    } else {
        None
    };
    let mut cfg = EngineConfig::new(loaded.config.clone(), Arc::new(Native));
    cfg.git = git;
    cfg.store = store;
    cfg.in_use = InUseSource::Native(GapPolicy::default());
    cfg.synced_roots = SyncedRoots::detect();
    cfg.allow_network_roots = loaded.allow_network_roots;
    if let Some(t) = std::env::var_os("CARGO_TARGET_DIR").filter(|v| !v.is_empty()) {
        let cwd = std::env::current_dir().map_err(|e| ScullError::from_io(&e))?;
        cfg.explicit_target_dirs
            .push((cwd.join(PathBuf::from(t)), "CARGO_TARGET_DIR".into()));
    }
    Engine::new(cfg)
}

/// Forwards Ctrl-C (the process-wide flag in [`crate::cancel`]) to a scanner [`CancelToken`]
/// while it lives.
pub struct CancelBridge {
    token: CancelToken,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CancelBridge {
    /// Start watching.
    pub fn start() -> Self {
        let token = CancelToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let (t, d) = (token.clone(), Arc::clone(&done));
        let handle = std::thread::Builder::new()
            .name("scull-cancel".into())
            .spawn(move || {
                while !d.load(Ordering::Relaxed) {
                    if cancel::is_cancelled() {
                        t.cancel();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            })
            .ok();
        if cancel::is_cancelled() {
            token.cancel();
        }
        Self {
            token,
            done,
            handle,
        }
    }

    /// The token to pass to the engine.
    pub fn token(&self) -> &CancelToken {
        &self.token
    }
}

impl Drop for CancelBridge {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A throttled progress callback printing to stderr (at most once a second while walking).
pub fn progress(ui: &Ui) -> impl Fn(EngineProgress) + Send + Sync + 'static {
    let ui = ui.clone();
    let last = Mutex::new(None::<Instant>);
    move |p| {
        if !ui.progress {
            return;
        }
        match p {
            EngineProgress::Scanning {
                root,
                entries,
                allocated,
                ..
            } => {
                let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
                    return;
                }
                *last = Some(Instant::now());
                ui.progress(&format!(
                    "scanning {}: {entries} entries, {} allocated",
                    root.display(),
                    format_bytes(allocated)
                ));
            }
            EngineProgress::Analyzing { what } => ui.progress(&format!("analysing {what}...")),
            _ => {}
        }
    }
}

/// A warning on stderr (suppressed by `--quiet`), escaped for the terminal.
pub fn warn(ui: &Ui, msg: &str) {
    if !ui.quiet {
        let line = format!(
            "scull: {} {}",
            Style::Warn.paint("warning:", ui.color_err),
            escape_for_terminal(msg)
        );
        let _ = writeln!(std::io::stderr(), "{line}");
    }
}
