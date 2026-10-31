//! Package-manager caches: npm, pnpm, Bun and Yarn (P1-DET-04, PRD §10.8 QC-01…05, T-23).
//!
//! Global package caches hold no unique data, so they form the Quick Clean category
//! ([`Category::PackageCache`]): **S1**, cleaned with the tool's **own** command (QC-02, QC-03),
//! never through quarantine. This module only *locates* them; nothing here runs a tool (that
//! is P2-QC-01) or reads anything but small configuration files as data.
//!
//! # How a location is resolved
//!
//! Everything machine-specific comes in through [`CacheEnv`]: the home folder, an environment
//! lookup and the OS layout to use. Library code never reads `std::env` (the CLI passes it in),
//! which also lets tests check every OS layout on every CI runner.
//!
//! | Tool | Overrides, highest first | Default (Linux / macOS / Windows) |
//! |---|---|---|
//! | npm | `npm_config_cache`; `cache=` in the user npmrc (`~/.npmrc` or `npm_config_userconfig`) | `~/.npm` / `~/.npm` / `%LocalAppData%\npm-cache` (the cache is its `_cacache`) |
//! | pnpm | `npm_config_store_dir`; `store-dir=` in the user npmrc; `$PNPM_HOME/store`; `$XDG_DATA_HOME/pnpm/store` | `~/.local/share/pnpm/store` / `~/Library/pnpm/store` / `%LocalAppData%\pnpm\store`, plus the legacy `~/.pnpm-store` |
//! | Bun | `BUN_INSTALL_CACHE_DIR`; `$BUN_INSTALL/install/cache` | `~/.bun/install/cache` |
//! | Yarn classic | `YARN_CACHE_FOLDER`; `cache-folder` in `~/.yarnrc` | `$XDG_CACHE_HOME/yarn` or `~/.cache/yarn` / `~/Library/Caches/Yarn` / `%LocalAppData%\Yarn\Cache` |
//! | Yarn Berry | global folder from `YARN_GLOBAL_FOLDER` or `globalFolder` in `~/.yarnrc.yml` (the cache is its `cache/`) | `$XDG_DATA_HOME/yarn/berry` or `~/.yarn/berry` / `~/.yarn/berry` / `%LocalAppData%\Yarn\Berry` |
//!
//! Configuration files are read through the [`DetectContext::config_reader`] as data only.
//! **Project** configuration (a project's `.npmrc`, `.yarnrc.yml`) is never consulted: the
//! cache commands run from a neutral directory, so only user-level configuration applies (T-23).
//!
//! # Validation (QC-05, T-23)
//!
//! * A configured location must be absolute, free of `..`, and must not be a filesystem root,
//!   the home folder or one of its ancestors, or a shared folder such as `~/.cache` or
//!   `%LocalAppData%`. Otherwise the tool is **refused** ([`RefusedCache`]): its native command
//!   would act on that location, so no location of that tool is offered at all. The same
//!   happens when a user config file exists but can't be read: the effective location is then
//!   unknown.
//! * A cache reached through a symlink, junction or other reparse point anywhere below its
//!   anchor (the home folder, or the configured location's parent outside home) is reported
//!   with [`CacheProblem::Link`] and classified S3. Links are never followed, so their target
//!   can't be shown to stay inside the expected location.
//! * A cache on a different volume than its anchor is reported with
//!   [`CacheProblem::OtherVolume`] and classified S3 (crosses a volume boundary).
//! * A tool that is running (an install in progress, a held lock) is the caller's
//!   [`CallerSignals::in_use`]; see [`LocatedCache::activity_processes`].
//!
//! # Commands (QC-03, CLS-04)
//!
//! The command is chosen here, in Rust, by rule ID ([`commands_for_rule`]); never by a rule
//! file. The preferred command (a prune where the tool has one) becomes the candidate's
//! [`Action::ToolCommand`]; the full-wipe alternative is recorded in the evidence. pnpm has no
//! wipe: projects hard-link into the store, so removing it frees little and confuses pnpm.
//!
//! The executor must re-resolve the location before running anything and make the tool act on
//! the same path (e.g. through the tool's own cache-location option), since it runs tools with a
//! sanitized environment (SAFE-12).

use crate::cargo::normalize;
use crate::detector::{CallerSignals, DetectContext, Listing, MAX_CONFIG_BYTES, list};
use crate::rules::Platform as Os;
use scull_core::{
    Action, Candidate, Category, Evidence, FileIdentity, Flags, FsPath, ItemId, ReasonCode,
    Regeneration, RegenerationCost, RuleRef, Sizes, Tier, TierReason,
};
use scull_platform::{EntryKind, Platform, native_path_bytes};
use scull_safety::{Regen, Signals, Tri};
use scull_scanner::DirSummary;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Version of the cache locator rules; bumps when locating, validation or tiering changes.
pub const CACHE_RULE_VERSION: u32 = 1;

/// The preview's not-undoable note for every package cache (QC-02).
pub const NOT_UNDOABLE_NOTE: &str =
    "Not undoable. Packages re-download on next install; offline installs may fail until then.";

/// A package manager whose global cache scull locates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CacheTool {
    /// npm's content-addressable cache (`_cacache`).
    Npm,
    /// The pnpm content-addressable store.
    Pnpm,
    /// Bun's install cache.
    Bun,
    /// Yarn 1.x (classic) cache.
    YarnClassic,
    /// Yarn 2+ (Berry) global cache (the "mirror").
    YarnBerry,
}

impl CacheTool {
    /// Every tool, in reporting order.
    pub const ALL: [CacheTool; 5] = [
        CacheTool::Npm,
        CacheTool::Pnpm,
        CacheTool::Bun,
        CacheTool::YarnClassic,
        CacheTool::YarnBerry,
    ];

    /// Rule ID recorded on its candidates.
    pub fn rule_id(self) -> &'static str {
        match self {
            CacheTool::Npm => "npm.cache",
            CacheTool::Pnpm => "pnpm.store",
            CacheTool::Bun => "bun.cache",
            CacheTool::YarnClassic => "yarn.classic-cache",
            CacheTool::YarnBerry => "yarn.berry-cache",
        }
    }

    /// The tool for a rule ID.
    pub fn from_rule_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.rule_id() == id)
    }

    /// Name of the tool's executable (resolved from trusted locations by the executor, SAFE-12).
    pub fn binary(self) -> &'static str {
        match self {
            CacheTool::Npm => "npm",
            CacheTool::Pnpm => "pnpm",
            CacheTool::Bun => "bun",
            CacheTool::YarnClassic | CacheTool::YarnBerry => "yarn",
        }
    }

    /// Human-readable artifact type.
    pub fn artifact(self) -> &'static str {
        match self {
            CacheTool::Npm => "npm cache",
            CacheTool::Pnpm => "pnpm store",
            CacheTool::Bun => "Bun install cache",
            CacheTool::YarnClassic => "Yarn classic cache",
            CacheTool::YarnBerry => "Yarn Berry global cache",
        }
    }

    /// Process names whose presence means the tool may be using its cache right now (QC-05).
    pub fn processes(self) -> &'static [&'static str] {
        match self {
            CacheTool::Npm => &["npm", "npx"],
            CacheTool::Pnpm => &["pnpm", "pnpx"],
            CacheTool::Bun => &["bun", "bunx"],
            CacheTool::YarnClassic | CacheTool::YarnBerry => &["yarn"],
        }
    }
}

impl fmt::Display for CacheTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.artifact())
    }
}

/// A native command: executable name plus structured arguments (never a shell string).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeCommand {
    /// Executable name.
    pub tool: &'static str,
    /// Arguments.
    pub args: &'static [&'static str],
}

impl NativeCommand {
    /// The command as an [`Action::ToolCommand`].
    pub fn to_action(self) -> Action {
        Action::ToolCommand {
            tool: self.tool.to_owned(),
            args: self.args.iter().map(|a| (*a).to_owned()).collect(),
        }
    }
}

impl fmt::Display for NativeCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tool)?;
        for a in self.args {
            write!(f, " {a}")?;
        }
        Ok(())
    }
}

/// The native cleanup commands for one cache (QC-03 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheCommands {
    /// The default command: a prune where the tool has one, otherwise its only cleanup.
    pub preferred: NativeCommand,
    /// Whether [`CacheCommands::preferred`] only removes unused entries (a prune) rather than
    /// everything.
    pub preferred_prunes: bool,
    /// The full-wipe alternative, offered second; `None` when there is none (pnpm: never wipe
    /// the store) or the preferred command already is the wipe.
    pub wipe: Option<NativeCommand>,
}

/// The native commands for a cache rule ID (CLS-04: chosen in Rust, by rule ID).
pub fn commands_for_rule(rule_id: &str) -> Option<CacheCommands> {
    CacheTool::from_rule_id(rule_id).map(commands_for_tool)
}

fn commands_for_tool(tool: CacheTool) -> CacheCommands {
    let c = |tool, args| NativeCommand { tool, args };
    match tool {
        CacheTool::Npm => CacheCommands {
            preferred: c("npm", &["cache", "verify"]),
            preferred_prunes: true,
            wipe: Some(c("npm", &["cache", "clean", "--force"])),
        },
        CacheTool::Pnpm => CacheCommands {
            preferred: c("pnpm", &["store", "prune"]),
            preferred_prunes: true,
            wipe: None,
        },
        CacheTool::Bun => CacheCommands {
            preferred: c("bun", &["pm", "cache", "rm"]),
            preferred_prunes: false,
            wipe: None,
        },
        CacheTool::YarnClassic => CacheCommands {
            preferred: c("yarn", &["cache", "clean"]),
            preferred_prunes: false,
            wipe: None,
        },
        // Berry: `--mirror` targets the global cache; plain `cache clean` only a project's.
        CacheTool::YarnBerry => CacheCommands {
            preferred: c("yarn", &["cache", "clean", "--mirror"]),
            preferred_prunes: false,
            wipe: None,
        },
    }
}

type EnvLookup = dyn Fn(&str) -> Option<String> + Send + Sync;

/// The machine facts cache locations depend on, supplied by the caller (the CLI reads the real
/// environment; tests pass fakes).
pub struct CacheEnv {
    /// The user's home folder (absolute).
    pub home: PathBuf,
    /// Which OS layout to resolve defaults for.
    pub os: Os,
    env: Box<EnvLookup>,
}

impl fmt::Debug for CacheEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheEnv")
            .field("home", &self.home)
            .field("os", &self.os)
            .finish_non_exhaustive()
    }
}

impl CacheEnv {
    /// A context for `home` and `os`, with `env` looking up environment variables.
    pub fn new(
        home: impl Into<PathBuf>,
        os: Os,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            home: home.into(),
            os,
            env: Box::new(env),
        }
    }

    /// A context whose environment is the given `(name, value)` pairs.
    pub fn with_vars<K, V>(
        home: impl Into<PathBuf>,
        os: Os,
        vars: impl IntoIterator<Item = (K, V)>,
    ) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        let map: BTreeMap<String, String> = vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self::new(home, os, move |k| map.get(k).cloned())
    }

    /// A non-empty environment variable.
    pub fn var(&self, name: &str) -> Option<String> {
        (self.env)(name).filter(|v| !v.is_empty())
    }

    /// The first of `names` that is set, with its name.
    fn first_var(&self, names: &[&str]) -> Option<(String, String)> {
        names
            .iter()
            .find_map(|n| self.var(n).map(|v| ((*n).to_owned(), v)))
    }

    /// An environment variable holding an absolute directory (relative values are ignored, as
    /// the XDG spec requires).
    fn abs_var(&self, name: &str) -> Option<PathBuf> {
        self.var(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    }

    /// `%LocalAppData%`, or `~\AppData\Local` when unset.
    fn local_app_data(&self) -> PathBuf {
        self.abs_var("LOCALAPPDATA")
            .unwrap_or_else(|| self.home.join("AppData").join("Local"))
    }

    /// Folders that hold more than one tool's data: a cache location must never be one.
    fn shared_folders(&self) -> Vec<PathBuf> {
        let h = &self.home;
        let mut v: Vec<PathBuf> = [
            ".cache",
            ".local",
            ".config",
            "Library",
            "AppData",
            "Documents",
            "Desktop",
            "Downloads",
        ]
        .iter()
        .map(|d| h.join(d))
        .collect();
        v.push(h.join(".local").join("share"));
        v.push(h.join("Library").join("Caches"));
        v.push(h.join("AppData").join("Local"));
        v.push(h.join("AppData").join("Roaming"));
        v.push(self.local_app_data());
        for x in [
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "APPDATA",
        ] {
            v.extend(self.abs_var(x));
        }
        v.into_iter().map(|p| normalize(&p)).collect()
    }
}

/// Where a cache location came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheSource {
    /// The tool's default for the OS.
    Default,
    /// An environment variable.
    Env {
        /// Its name.
        var: String,
    },
    /// A key in a user configuration file (read as data).
    Config {
        /// The file.
        file: PathBuf,
        /// The key.
        key: String,
    },
}

impl fmt::Display for CacheSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheSource::Default => f.write_str("the tool's default location"),
            CacheSource::Env { var } => write!(f, "the {var} environment variable"),
            CacheSource::Config { file, key } => write!(f, "`{key}` in {}", file.display()),
        }
    }
}

/// A QC-05 check that failed for a located cache. Any problem makes the candidate S3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheProblem {
    /// `path` (the cache or a folder between its anchor and it) is a symlink, junction or
    /// other reparse point; its target is never followed, so it can't be verified to stay in
    /// the expected location.
    Link {
        /// The link.
        path: PathBuf,
    },
    /// The cache is on a different volume than its anchor.
    OtherVolume {
        /// The anchor whose volume was expected.
        anchor: PathBuf,
    },
}

/// A cache folder that exists at a validated location.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocatedCache {
    /// The tool.
    pub tool: CacheTool,
    /// The cache folder (what sizes and the candidate refer to).
    pub path: PathBuf,
    /// Its no-follow identity, if the platform provides one.
    pub identity: Option<FileIdentity>,
    /// Where the location came from.
    pub source: CacheSource,
    /// The folder the location was checked from (home, or the configured location's parent).
    pub anchor: PathBuf,
    /// Whether the folder's contents look like this tool's cache layout.
    pub recognized: bool,
    /// Entries that identified the layout (e.g. `_cacache/index-v5`, pnpm `v10`).
    pub markers: Vec<PathBuf>,
    /// Extra facts for the evidence (store versions, disabled global cache, ...).
    pub notes: Vec<String>,
    /// Failed QC-05 checks; any one makes the candidate S3.
    pub problems: Vec<CacheProblem>,
    /// Whether the volume check could run (both identities known).
    pub volume_checked: bool,
}

/// A tool whose cache location was refused (T-23, QC-05): nothing of it is offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedCache {
    /// The tool.
    pub tool: CacheTool,
    /// The refused location, if one could be determined.
    pub path: Option<PathBuf>,
    /// Where it came from.
    pub source: CacheSource,
    /// Why it was refused.
    pub reason: String,
}

/// Everything [`locate_caches`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CacheScan {
    /// Caches that exist, in [`CacheTool::ALL`] order.
    pub found: Vec<LocatedCache>,
    /// Tools skipped because their location was refused; report them (QC-05).
    pub refused: Vec<RefusedCache>,
}

/// Locate every supported package cache for `env`, without running any tool.
///
/// Missing caches are simply absent from the result. See the module docs for the resolution
/// order and the checks.
pub fn locate_caches(
    env: &CacheEnv,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> CacheScan {
    let mut out = CacheScan::default();
    let npmrc = UserRc::npmrc(env, platform, ctx);
    for tool in CacheTool::ALL {
        let spots = match tool {
            CacheTool::Npm => npm_spots(env, &npmrc),
            CacheTool::Pnpm => pnpm_spots(env, &npmrc),
            CacheTool::Bun => Ok(bun_spots(env)),
            CacheTool::YarnClassic => yarn_classic_spots(env, platform, ctx),
            CacheTool::YarnBerry => yarn_berry_spots(env, platform, ctx),
        };
        let spots = match spots {
            Ok(s) => s,
            Err(r) => {
                out.refused.push(*r);
                continue;
            }
        };
        // Validate every location first: one bad location refuses the whole tool.
        if let Some(r) = spots.iter().find_map(|s| validate(env, s).err()) {
            out.refused.push(r);
            continue;
        }
        for spot in spots {
            let Some(c) = inspect(env, platform, spot) else {
                continue;
            };
            if out
                .found
                .iter()
                .any(|f| f.path == c.path || (f.identity.is_some() && f.identity == c.identity))
            {
                continue; // the same folder reached twice (e.g. `YARN_CACHE_FOLDER`)
            }
            out.found.push(c);
        }
    }
    out
}

/// One place a tool's cache may be.
#[derive(Debug, Clone)]
struct Spot {
    tool: CacheTool,
    /// The configured location (validated).
    root: PathBuf,
    /// The cache folder inside it (often `root` itself).
    path: PathBuf,
    source: CacheSource,
    notes: Vec<String>,
}

impl Spot {
    fn new(tool: CacheTool, root: PathBuf, source: CacheSource) -> Self {
        Self {
            tool,
            path: root.clone(),
            root,
            source,
            notes: Vec::new(),
        }
    }

    fn sub(mut self, rel: &str) -> Self {
        self.path = self.root.join(rel);
        self
    }
}

type Spots = Result<Vec<Spot>, Box<RefusedCache>>;

fn refused(
    tool: CacheTool,
    path: Option<PathBuf>,
    source: CacheSource,
    reason: String,
) -> Box<RefusedCache> {
    Box::new(RefusedCache {
        tool,
        path,
        source,
        reason,
    })
}

/// A user-level configuration file.
enum UserRc {
    /// No such file: nothing configured there.
    Absent,
    /// Its parsed text.
    Text { file: PathBuf, text: String },
    /// It exists but can't be read as a small regular file (or there's no reader).
    Unreadable { file: PathBuf },
}

impl UserRc {
    fn read(file: PathBuf, platform: &dyn Platform, ctx: &DetectContext<'_>) -> Self {
        let md = match platform.metadata(&file) {
            Ok(md) => md,
            Err(e) if e.is_not_found() => return UserRc::Absent,
            Err(_) => return UserRc::Unreadable { file },
        };
        if md.kind != EntryKind::File || md.logical_size > MAX_CONFIG_BYTES {
            return UserRc::Unreadable { file };
        }
        match ctx
            .config_reader
            .and_then(|r| r.read_text(&file, MAX_CONFIG_BYTES))
        {
            Some(text) => UserRc::Text { file, text },
            None => UserRc::Unreadable { file },
        }
    }

    /// The user npmrc: `npm_config_userconfig` or `~/.npmrc`. A relative override is read as
    /// unreadable (the location is unknown).
    fn npmrc(env: &CacheEnv, platform: &dyn Platform, ctx: &DetectContext<'_>) -> Self {
        match env.first_var(&["npm_config_userconfig", "NPM_CONFIG_USERCONFIG"]) {
            Some((_, v)) => {
                let p = PathBuf::from(v);
                if p.is_absolute() {
                    Self::read(p, platform, ctx)
                } else {
                    UserRc::Unreadable { file: p }
                }
            }
            None => Self::read(env.home.join(".npmrc"), platform, ctx),
        }
    }

    /// The value of `key` (parsed with `parse`), or why the tool must be refused.
    fn lookup(
        &self,
        tool: CacheTool,
        key: &str,
        parse: fn(&str) -> BTreeMap<String, String>,
    ) -> Result<Option<(PathBuf, String)>, Box<RefusedCache>> {
        match self {
            UserRc::Absent => Ok(None),
            UserRc::Text { file, text } => Ok(parse(text).remove(key).map(|v| (file.clone(), v))),
            UserRc::Unreadable { file } => Err(refused(
                tool,
                None,
                CacheSource::Config {
                    file: file.clone(),
                    key: key.to_owned(),
                },
                format!(
                    "{} exists but can't be read as a small regular file, so the effective \
                     cache location is unknown",
                    file.display()
                ),
            )),
        }
    }
}

/// A configured path from `source`: `~` and `${VAR}` expanded, must be absolute.
fn configured(
    env: &CacheEnv,
    tool: CacheTool,
    raw: &str,
    source: CacheSource,
) -> Result<(PathBuf, CacheSource), Box<RefusedCache>> {
    match expand(raw, env) {
        Ok(p) => Ok((p, source)),
        Err(why) => Err(refused(tool, None, source, format!("`{raw}`: {why}"))),
    }
}

/// An override from the first set variable of `vars`, else from `key` in `rc`.
fn override_from(
    env: &CacheEnv,
    tool: CacheTool,
    vars: &[&str],
    rc: &UserRc,
    key: &str,
    parse: fn(&str) -> BTreeMap<String, String>,
) -> Result<Option<(PathBuf, CacheSource)>, Box<RefusedCache>> {
    if let Some((var, v)) = env.first_var(vars) {
        return configured(env, tool, &v, CacheSource::Env { var }).map(Some);
    }
    match rc.lookup(tool, key, parse)? {
        Some((file, v)) => configured(
            env,
            tool,
            &v,
            CacheSource::Config {
                file,
                key: key.to_owned(),
            },
        )
        .map(Some),
        None => Ok(None),
    }
}

fn npm_spots(env: &CacheEnv, npmrc: &UserRc) -> Spots {
    let tool = CacheTool::Npm;
    let (root, source) = match override_from(
        env,
        tool,
        &["npm_config_cache", "NPM_CONFIG_CACHE"],
        npmrc,
        "cache",
        parse_npmrc,
    )? {
        Some(o) => o,
        None => {
            let root = match env.os {
                Os::Windows => env.local_app_data().join("npm-cache"),
                Os::Linux | Os::Macos => env.home.join(".npm"),
            };
            (root, CacheSource::Default)
        }
    };
    Ok(vec![Spot::new(tool, root, source).sub("_cacache")])
}

fn pnpm_spots(env: &CacheEnv, npmrc: &UserRc) -> Spots {
    let tool = CacheTool::Pnpm;
    if let Some((root, source)) = override_from(
        env,
        tool,
        &["npm_config_store_dir", "NPM_CONFIG_STORE_DIR"],
        npmrc,
        "store-dir",
        parse_npmrc,
    )? {
        return Ok(vec![Spot::new(tool, root, source)]);
    }
    let derived = |var: &str, rel: &[&str]| {
        env.abs_var(var).map(|base| {
            let root = rel.iter().fold(base, |p, s| p.join(s));
            Spot::new(tool, root, CacheSource::Env { var: var.into() })
        })
    };
    let primary = derived("PNPM_HOME", &["store"])
        .or_else(|| derived("XDG_DATA_HOME", &["pnpm", "store"]))
        .unwrap_or_else(|| {
            let root = match env.os {
                Os::Linux => env
                    .home
                    .join(".local")
                    .join("share")
                    .join("pnpm")
                    .join("store"),
                Os::Macos => env.home.join("Library").join("pnpm").join("store"),
                Os::Windows => env.local_app_data().join("pnpm").join("store"),
            };
            Spot::new(tool, root, CacheSource::Default)
        });
    let mut legacy = Spot::new(tool, env.home.join(".pnpm-store"), CacheSource::Default);
    legacy
        .notes
        .push("legacy pnpm store location (`~/.pnpm-store`, used by older pnpm versions)".into());
    Ok(vec![primary, legacy])
}

fn bun_spots(env: &CacheEnv) -> Vec<Spot> {
    let tool = CacheTool::Bun;
    if let Some((var, v)) = env.first_var(&["BUN_INSTALL_CACHE_DIR"]) {
        // Bun takes the value literally; relative values are refused by validation.
        return vec![Spot::new(tool, PathBuf::from(v), CacheSource::Env { var })];
    }
    match env.abs_var("BUN_INSTALL") {
        Some(base) => vec![Spot::new(
            tool,
            base.join("install").join("cache"),
            CacheSource::Env {
                var: "BUN_INSTALL".into(),
            },
        )],
        None => vec![Spot::new(
            tool,
            env.home.join(".bun").join("install").join("cache"),
            CacheSource::Default,
        )],
    }
}

fn yarn_classic_spots(env: &CacheEnv, platform: &dyn Platform, ctx: &DetectContext<'_>) -> Spots {
    let tool = CacheTool::YarnClassic;
    let rc = UserRc::read(env.home.join(".yarnrc"), platform, ctx);
    if let Some((root, source)) = override_from(
        env,
        tool,
        &["YARN_CACHE_FOLDER"],
        &rc,
        "cache-folder",
        parse_yarnrc,
    )? {
        return Ok(vec![Spot::new(tool, root, source)]);
    }
    let root = match env.os {
        Os::Linux => env
            .abs_var("XDG_CACHE_HOME")
            .unwrap_or_else(|| env.home.join(".cache"))
            .join("yarn"),
        Os::Macos => env.home.join("Library").join("Caches").join("Yarn"),
        Os::Windows => env.local_app_data().join("Yarn").join("Cache"),
    };
    Ok(vec![Spot::new(tool, root, CacheSource::Default)])
}

fn yarn_berry_spots(env: &CacheEnv, platform: &dyn Platform, ctx: &DetectContext<'_>) -> Spots {
    let tool = CacheTool::YarnBerry;
    let rc = UserRc::read(env.home.join(".yarnrc.yml"), platform, ctx);
    let (root, source) = match override_from(
        env,
        tool,
        &["YARN_GLOBAL_FOLDER"],
        &rc,
        "globalFolder",
        parse_yarnrc_yml,
    )? {
        Some(o) => o,
        None => {
            let root = match env.os {
                Os::Windows => env.local_app_data().join("Yarn").join("Berry"),
                Os::Linux | Os::Macos => env
                    .abs_var("XDG_DATA_HOME")
                    .map(|x| x.join("yarn").join("berry"))
                    .unwrap_or_else(|| env.home.join(".yarn").join("berry")),
            };
            (root, CacheSource::Default)
        }
    };
    let mut spot = Spot::new(tool, root, source).sub("cache");
    let global = match env.var("YARN_ENABLE_GLOBAL_CACHE") {
        Some(v) => Some(v),
        None => match &rc {
            UserRc::Text { text, .. } => parse_yarnrc_yml(text).remove("enableGlobalCache"),
            _ => None,
        },
    };
    if global.is_some_and(|v| matches!(v.trim(), "false" | "0")) {
        spot.notes.push(
            "`enableGlobalCache` is off: Yarn keeps caches per project, so this global cache \
             only holds leftovers"
                .into(),
        );
    }
    Ok(vec![spot])
}

/// T-23 / QC-05 location checks on a spot's configured root.
fn validate(env: &CacheEnv, s: &Spot) -> Result<(), RefusedCache> {
    let refuse = |reason: &str| RefusedCache {
        tool: s.tool,
        path: Some(s.root.clone()),
        source: s.source.clone(),
        reason: format!("{} {reason}", s.root.display()),
    };
    if !s.root.is_absolute() {
        return Err(refuse("is not an absolute path"));
    }
    if s.root
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(refuse("contains `.` or `..` components"));
    }
    let root = normalize(&s.root);
    if root.parent().is_none() {
        return Err(refuse("is a filesystem root"));
    }
    if normalize(&env.home).starts_with(&root) {
        return Err(refuse("is the home folder or one of its ancestors"));
    }
    if env.shared_folders().contains(&root) {
        return Err(refuse(
            "is a shared folder holding other programs' data, not a cache of its own",
        ));
    }
    Ok(())
}

/// The folder a location is checked from: the home folder when inside it, else the configured
/// root's parent.
fn anchor_of(env: &CacheEnv, root: &Path) -> PathBuf {
    let home = normalize(&env.home);
    if root != home && root.starts_with(&home) {
        home
    } else {
        root.parent()
            .map_or_else(|| root.to_path_buf(), Path::to_path_buf)
    }
}

/// Look at a validated spot. `None` when nothing (or not a folder) is there.
fn inspect(env: &CacheEnv, platform: &dyn Platform, s: Spot) -> Option<LocatedCache> {
    let path = normalize(&s.path);
    let anchor = anchor_of(env, &normalize(&s.root));
    let md = platform.metadata(&path).ok()?;
    let mut problems = Vec::new();
    // Every component strictly below the anchor, down to the cache itself (no-follow each).
    if let Ok(rest) = path.strip_prefix(&anchor) {
        let mut at = anchor.clone();
        for c in rest.components() {
            at.push(c.as_os_str());
            match platform.metadata(&at) {
                Ok(m) if m.kind.is_link() => {
                    problems.push(CacheProblem::Link { path: at.clone() });
                    break;
                }
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
    let is_link = md.kind.is_link();
    if !is_link && md.kind != EntryKind::Dir {
        return None;
    }
    let mut c = LocatedCache {
        tool: s.tool,
        path: path.clone(),
        identity: md.identity,
        source: s.source,
        anchor: anchor.clone(),
        recognized: false,
        markers: Vec::new(),
        notes: s.notes,
        problems,
        volume_checked: false,
    };
    if is_link {
        return Some(c); // never listed through
    }
    if let Some(inside) = list(platform, &path) {
        recognize(&mut c, &inside);
    }
    if c.problems.is_empty()
        && let (Some(id), Ok(Some(aid))) =
            (md.identity, platform.metadata(&anchor).map(|m| m.identity))
    {
        c.volume_checked = true;
        if id.volume != aid.volume {
            c.problems.push(CacheProblem::OtherVolume { anchor });
        }
    }
    Some(c)
}

/// Whether `name` is a store/cache version folder such as `v3`, `v10` or `v6`.
fn is_version_dir(name: &str) -> bool {
    name.len() > 1 && name.starts_with('v') && name[1..].bytes().all(|b| b.is_ascii_digit())
}

/// Check the folder's entries against the tool's layout.
fn recognize(c: &mut LocatedCache, inside: &Listing) {
    let dirs = || {
        inside
            .iter()
            .filter(|(_, k)| **k == EntryKind::Dir)
            .map(|(n, _)| n.as_str())
    };
    let markers: Vec<&str> = match c.tool {
        CacheTool::Npm => dirs()
            .filter(|n| matches!(*n, "index-v5" | "content-v2"))
            .collect(),
        CacheTool::Pnpm | CacheTool::YarnClassic => dirs().filter(|n| is_version_dir(n)).collect(),
        CacheTool::Bun => inside
            .keys()
            .map(String::as_str)
            .filter(|n| n.contains("@@@") || n.ends_with(".npm"))
            .take(1)
            .collect(),
        CacheTool::YarnBerry => inside
            .iter()
            .filter(|(n, k)| **k == EntryKind::File && n.ends_with(".zip"))
            .map(|(n, _)| n.as_str())
            .take(1)
            .collect(),
    };
    c.recognized = !markers.is_empty();
    if c.tool == CacheTool::Pnpm && c.recognized {
        c.notes.push(format!(
            "store layout versions present: {}",
            markers.join(", ")
        ));
    }
    c.markers = markers.into_iter().map(|m| c.path.join(m)).collect();
}

impl LocatedCache {
    /// Rule reference recorded on the candidate.
    pub fn rule(&self) -> RuleRef {
        RuleRef {
            id: self.tool.rule_id().into(),
            version: CACHE_RULE_VERSION,
        }
    }

    /// The native commands for this cache.
    pub fn commands(&self) -> CacheCommands {
        commands_for_tool(self.tool)
    }

    /// Process names the caller checks when filling [`CallerSignals::in_use`] (QC-05: skip a
    /// tool that is running).
    pub fn activity_processes(&self) -> &'static [&'static str] {
        self.tool.processes()
    }

    /// Classify the cache and build its [`Candidate`] (QC-02).
    ///
    /// * The tier starts from the PRD §8.3 rubric with the caller's signals (Git, protected
    ///   roots, exclusions, in use, mounts, links, cloud sync) and the cache's own facts
    ///   (re-downloadable: needs the network), plus a [`ReasonCode::PackageCache`] reason: **S1**
    ///   when every check passes.
    /// * Recent activity is **not** a risk signal for a package cache (every install touches
    ///   it, and it holds no unique data), so [`CallerSignals::recent_activity`] is ignored; the
    ///   last use is recorded as evidence instead.
    /// * An unrecognized layout raises it to S2; any [`CacheProblem`] or a missing file
    ///   identity raises it to S3 (no action).
    /// * The action is the preferred native command ([`Action::ToolCommand`]); the wipe
    ///   alternative and the not-undoable note go into evidence and regeneration.
    /// * `summary` is the scanner's result for [`LocatedCache::path`] (ignored if it is for
    ///   another path): sizes, shared (hard-linked) bytes and the newest mtime (last use).
    pub fn into_candidate(self, caller: &CallerSignals, summary: Option<&DirSummary>) -> Candidate {
        let commands = self.commands();
        let rule = self.rule();
        let summary = summary.filter(|s| s.path == self.path);
        let (identity, identity_missing) = match self.identity {
            Some(i) => (i, false),
            None => (FileIdentity::new(0, 0), true),
        };
        let mut evidence = vec![
            Evidence::KnownArtifact {
                artifact: self.tool.artifact().into(),
            },
            Evidence::Note {
                text: format!("location from {}", self.source),
            },
        ];
        evidence.extend(self.markers.iter().map(|m| Evidence::Marker {
            path: FsPath::from_path(m),
        }));
        evidence.extend(
            self.notes
                .iter()
                .map(|t| Evidence::Note { text: t.clone() }),
        );
        evidence.push(Evidence::Note {
            text: match (commands.wipe, commands.preferred_prunes) {
                (Some(w), _) => format!(
                    "cleaned with `{}` (removes unused entries); full wipe alternative: `{w}`",
                    commands.preferred
                ),
                (None, true) => format!(
                    "cleaned with `{}` (removes unused entries); no full wipe is offered",
                    commands.preferred
                ),
                (None, false) => {
                    format!("cleaned with `{}` (removes everything)", commands.preferred)
                }
            },
        });
        if self.tool == CacheTool::Pnpm {
            evidence.push(Evidence::Note {
                text: "projects' node_modules hard-link into this store: it is never removed \
                       directly, and `pnpm store prune` frees only packages no project \
                       references (shared bytes show how much is still referenced)"
                    .into(),
            });
        }
        let mut sizes = Sizes::default();
        let mut hardlinked = None;
        if let Some(s) = summary {
            sizes.logical = s.totals.logical;
            sizes.allocated = s.totals.allocated;
            sizes.reclaimable = s.reclaimable();
            hardlinked = Some(s.shared_allocated > 0);
            if s.shared_allocated > 0 {
                evidence.push(Evidence::Hardlinked {
                    shared_bytes: s.shared_allocated,
                });
            }
            if let Some(newest) = s.newest_mtime {
                evidence.push(Evidence::LastModified {
                    newest_unix: newest,
                    sampled: !s.complete,
                });
            }
        }

        let mut candidate = Candidate {
            id: ItemId::derive(identity, &native_path_bytes(&self.path)),
            path: FsPath::from_path(&self.path),
            identity,
            category: Category::PackageCache,
            artifact: self.tool.artifact().into(),
            rule,
            project: None,
            tier: Tier::S0,
            reasons: Vec::new(),
            evidence,
            sizes,
            flags: Flags {
                hardlinked,
                ..Flags::default()
            },
            regeneration: Regeneration {
                instructions: Some(NOT_UNDOABLE_NOTE.into()),
                cost: RegenerationCost::Unknown,
            },
            action: commands.preferred.to_action(),
        };
        let signals = Signals {
            git: caller.git,
            in_protected_root: caller.in_protected_root,
            user_excluded: caller.user_excluded,
            protect_marker: caller.protect_marker,
            in_use: caller.in_use,
            crosses_mount: caller.crosses_mount,
            link_escape: caller.link_escape,
            cloud_sync: caller.cloud_sync,
            marker_present: true,
            lockfile_present: true,
            artifact_known: true,
            regen: Regen::NetworkOrLong,
            recent_activity: Tri::No,
        };
        scull_safety::apply(&mut candidate, &signals);
        candidate.raise(TierReason::new(
            Tier::S1,
            ReasonCode::PackageCache,
            format!(
                "package-manager cache: re-downloadable, cleaned with `{}`. {NOT_UNDOABLE_NOTE}",
                commands.preferred
            ),
        ));
        if !self.recognized && self.problems.is_empty() {
            candidate.raise(TierReason::new(
                Tier::S2,
                ReasonCode::SignalUnavailable,
                format!(
                    "contents don't match the {} layout; it may not be (only) a cache",
                    self.tool.artifact()
                ),
            ));
        }
        for p in &self.problems {
            match p {
                CacheProblem::Link { path } => {
                    candidate.flags.symlink = Some(true);
                    candidate.evidence.push(Evidence::Note {
                        text: format!(
                            "{} is a link; links are never followed, so the cache can't be \
                             verified to be inside its expected location",
                            path.display()
                        ),
                    });
                    candidate.raise(TierReason::new(
                        Tier::S3,
                        ReasonCode::LinkEscape,
                        format!(
                            "skipped: reached through the link {} (QC-05)",
                            path.display()
                        ),
                    ));
                }
                CacheProblem::OtherVolume { anchor } => {
                    candidate.flags.cross_mount = Some(true);
                    candidate.evidence.push(Evidence::CrossesMount);
                    candidate.raise(TierReason::new(
                        Tier::S3,
                        ReasonCode::CrossesMount,
                        format!(
                            "skipped: on a different volume than {} (QC-05)",
                            anchor.display()
                        ),
                    ));
                }
            }
        }
        if !self.volume_checked && self.problems.is_empty() {
            candidate.evidence.push(Evidence::SignalUnavailable {
                signal: "volume".into(),
                reason: "file identity unavailable; the cache's volume could not be compared"
                    .into(),
            });
        }
        if identity_missing {
            candidate.raise(TierReason::new(
                Tier::S3,
                ReasonCode::SignalUnavailable,
                "file identity unavailable on this platform; the item can't be revalidated \
                 before cleanup",
            ));
            candidate.evidence.push(Evidence::SignalUnavailable {
                signal: "file_identity".into(),
                reason: "the platform reported no file identity for this directory".into(),
            });
        }
        candidate
    }
}

/// Expand a configured path: `${VAR}` from the environment (an unset variable is an error, as
/// in npm), a leading `~` to the home folder. The result must be absolute.
pub(crate) fn expand(raw: &str, env: &CacheEnv) -> Result<PathBuf, String> {
    let mut s = String::new();
    let mut rest = raw.trim();
    while let Some(i) = rest.find("${") {
        s.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| "unterminated `${`".to_owned())?;
        let name = &after[..end];
        let v = env
            .var(name)
            .ok_or_else(|| format!("references the unset variable `{name}`"))?;
        s.push_str(&v);
        rest = &after[end + 1..];
    }
    s.push_str(rest);
    let p = if s == "~" {
        env.home.clone()
    } else if let Some(r) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        env.home.join(r)
    } else {
        PathBuf::from(&s)
    };
    if p.as_os_str().is_empty() || !p.is_absolute() {
        return Err("not an absolute path (relative paths depend on the working directory)".into());
    }
    Ok(p)
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return &v[1..v.len() - 1];
        }
    }
    v
}

/// `key=value` pairs of an npmrc (ini) file, last assignment wins. Comments (`;`, `#`) and
/// section headers are skipped. Pure: the file is data, never executed.
pub(crate) fn parse_npmrc(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with([';', '#', '[']) {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_owned(), unquote(v).to_owned());
        }
    }
    out
}

/// `key value` pairs of a Yarn classic `.yarnrc` (`cache-folder "/x"`, also `--cache-folder`).
pub(crate) fn parse_yarnrc(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = match line.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v),
            None => continue,
        };
        let k = unquote(k).trim_start_matches("--");
        out.insert(k.to_owned(), unquote(v).to_owned());
    }
    out
}

/// Top-level `key: value` scalars of a `.yarnrc.yml` (nested keys and lists are skipped).
pub(crate) fn parse_yarnrc_yml(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t', '#', '-']) {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = match v.find(" #") {
            Some(i) => &v[..i],
            None => v,
        };
        let v = unquote(v);
        if !v.is_empty() {
            out.insert(unquote(k).to_owned(), v.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> CacheEnv {
        let home = if cfg!(windows) {
            r"C:\Users\me"
        } else {
            "/home/me"
        };
        CacheEnv::with_vars(
            home,
            Os::Linux,
            vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        )
    }

    #[test]
    fn npmrc_is_parsed_as_data() {
        let m = parse_npmrc(
            "; comment\n# another\ncache = \"/x/npm\"\nregistry=https://r\n[section]\n\
             store-dir='/s'\ncache=/y\n//reg/:_authToken=abc\n",
        );
        assert_eq!(m.get("cache").map(String::as_str), Some("/y"), "last wins");
        assert_eq!(m.get("store-dir").map(String::as_str), Some("/s"));
        assert_eq!(m.get("registry").map(String::as_str), Some("https://r"));
    }

    #[test]
    fn yarnrc_formats() {
        let m = parse_yarnrc("# c\ncache-folder \"/c/y\"\n--mutex file\nlastUpdateCheck 1\n");
        assert_eq!(m.get("cache-folder").map(String::as_str), Some("/c/y"));
        assert_eq!(m.get("mutex").map(String::as_str), Some("file"));
        let y = parse_yarnrc_yml(
            "globalFolder: \"/g/berry\" # comment\nenableGlobalCache: false\npackageExtensions:\n  a: b\n",
        );
        assert_eq!(y.get("globalFolder").map(String::as_str), Some("/g/berry"));
        assert_eq!(
            y.get("enableGlobalCache").map(String::as_str),
            Some("false")
        );
        assert!(!y.contains_key("a") && !y.contains_key("packageExtensions"));
    }

    #[test]
    fn expansion() {
        let e = env(&[("BASE", if cfg!(windows) { r"D:\c" } else { "/c" })]);
        assert_eq!(expand("~", &e).unwrap(), e.home);
        assert_eq!(expand("~/.npm", &e).unwrap(), e.home.join(".npm"));
        let base = PathBuf::from(e.var("BASE").unwrap());
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            expand(&format!("${{BASE}}{sep}npm"), &e).unwrap(),
            base.join("npm")
        );
        assert!(expand("${NOPE}/x", &e).unwrap_err().contains("NOPE"));
        assert!(expand("relative/dir", &e).is_err());
        assert!(expand("", &e).is_err());
        assert!(expand("${BASE", &e).is_err());
    }

    #[test]
    fn commands_follow_qc03() {
        let s = |id: &str| commands_for_rule(id).unwrap();
        assert_eq!(s("npm.cache").preferred.to_string(), "npm cache verify");
        assert_eq!(
            s("npm.cache").wipe.unwrap().to_string(),
            "npm cache clean --force"
        );
        assert_eq!(s("pnpm.store").preferred.to_string(), "pnpm store prune");
        assert!(s("pnpm.store").wipe.is_none(), "never wipe the pnpm store");
        assert_eq!(s("bun.cache").preferred.to_string(), "bun pm cache rm");
        assert_eq!(
            s("yarn.classic-cache").preferred.to_string(),
            "yarn cache clean"
        );
        assert_eq!(
            s("yarn.berry-cache").preferred.to_string(),
            "yarn cache clean --mirror"
        );
        assert!(commands_for_rule("rust.target").is_none());
        for t in CacheTool::ALL {
            assert_eq!(CacheTool::from_rule_id(t.rule_id()), Some(t));
            assert_eq!(
                commands_for_rule(t.rule_id()).unwrap().preferred.tool,
                t.binary()
            );
        }
    }

    #[test]
    fn version_dirs() {
        assert!(is_version_dir("v3") && is_version_dir("v10"));
        assert!(!is_version_dir("v") && !is_version_dir("vx") && !is_version_dir("files"));
    }

    #[test]
    fn validation_refuses_unexpected_locations() {
        let e = env(&[]);
        let spot = |p: PathBuf| Spot::new(CacheTool::Npm, p, CacheSource::Default);
        let home = e.home.clone();
        assert!(validate(&e, &spot(home.clone())).is_err(), "home");
        assert!(
            validate(&e, &spot(home.parent().unwrap().into())).is_err(),
            "ancestor"
        );
        assert!(validate(&e, &spot(home.join(".cache"))).is_err(), "shared");
        assert!(
            validate(&e, &spot(home.join("x").join("..").join(".npm"))).is_err(),
            ".."
        );
        assert!(
            validate(&e, &spot(PathBuf::from("rel"))).is_err(),
            "relative"
        );
        let root: PathBuf = home.ancestors().last().unwrap().into();
        assert!(validate(&e, &spot(root)).is_err(), "filesystem root");
        assert!(validate(&e, &spot(home.join(".npm"))).is_ok());
        assert!(validate(&e, &spot(home.join(".cache").join("yarn"))).is_ok());
    }
}
