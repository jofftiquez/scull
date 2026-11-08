//! Configuration and policy (PRD §11, CFG-01…04).
//!
//! This module is pure: it parses text and merges values, and never touches the filesystem or
//! the process environment. The caller (CLI, desktop app, MCP server) finds and reads the files,
//! resolves platform paths (`scull-platform`), and passes in an environment lookup function.
//!
//! # Sources and precedence (CFG-02)
//!
//! From most to least important: **CLI flags > environment variables > user config file >
//! built-in defaults**. Each trusted source is a [`ConfigLayer`] (every field optional).
//! [`resolve`] merges them into an effective [`Config`] and records the [`Origin`] of every
//! value, for `scull doctor` and `scull config show`.
//!
//! * Single-valued fields and `scan.roots` / `rules.enabled`: the highest layer that sets the
//!   field wins.
//! * Protection lists (`scan.exclude`, `scan.protect`, `scan.min_tier`) are **additive**: every
//!   layer's entries are kept, so a CLI flag or env var can never drop a path the user config
//!   protects (FX-CFG-04). For `scan.min_tier` the most protective tier wins per path.
//! * `mcp.destructive` is read **only** from the user config file. The `--allow-destructive`
//!   launch flag is a separate, second switch checked by the MCP server (PRD §13.2). So are
//!   the agent limits `mcp.max_pending_plans` and `mcp.prepare_per_minute` (#299): an agent
//!   that can set its MCP server's environment or flags can't raise them.
//! * `[auto]` (unattended auto-policies, D-11) is read **only** from the user config file; see
//!   [`AutoConfig`].
//!
//! The user config file format is [`ConfigLayer`] in TOML (unknown keys are rejected with an
//! [`ErrorKind::Config`](crate::ErrorKind::Config) error, exit code 2). Environment variables are
//! listed in [`ENV_VARS`] and parsed by [`EnvLayer::from_lookup`].
//!
//! # Repository files are untrusted (CFG-03)
//!
//! `.scull-protect` and `.scull.toml` inside a scanned directory come from whoever wrote the
//! repository. They are parsed into a [`RepoPolicy`], which can only **add** protection:
//! exclusions, protected paths, higher minimum tiers and a longer activity window, all relative
//! to the file's own directory. Anything else (roots, rules, MCP, forge, output, lower tiers,
//! paths that escape the directory) is ignored and reported as a [`ConfigWarning`]. A repository
//! file never produces an error that stops a scan. Apply a policy with
//! [`Config::with_repo_policy`].
//!
//! # Scan roots (CFG-04)
//!
//! Defaults contain no scan roots. [`validate_roots`] rejects empty or relative root lists and
//! flags filesystem roots, the home directory and its ancestors as needing explicit
//! confirmation.

mod auto;
mod duration;
mod edit;
mod layer;
mod proactive;
mod repo;
mod roots;

pub use auto::{
    AutoConfig, AutoKind, AutoLayer, AutoPolicy, DEFAULT_MAX_GIB_PER_DAY, DEFAULT_MAX_GIB_PER_RUN,
    DEFAULT_MAX_ITEMS_PER_RUN, MAX_GIB_PER_DAY_CEILING, MAX_GIB_PER_RUN_CEILING,
    MAX_ITEMS_PER_RUN_CEILING, MIN_EVERY, MIN_IDLE,
};
pub use duration::DurationSpec;
pub use edit::edit_setting;
pub use layer::{
    ConfigLayer, ENV_VARS, EnvLayer, EnvVar, ForgeLayer, McpLayer, Origin, OutputLayer,
    QuarantineLayer, ResolvedConfig, RulesLayer, ScanLayer, resolve,
};
pub use proactive::{
    Budget, DEFAULT_ALERT_DAYS, DEFAULT_RESERVE_GIB, DEFAULT_SPIKE_GIB, DEFAULT_SPIKE_MIN_GIB,
    DEFAULT_SPIKE_PERCENT, DEFAULT_SPIKE_WINDOW, DEFAULT_STALE_BUILD, DEFAULT_STALE_CACHE,
    DEFAULT_STALE_WORKTREE, ProactiveConfig, ProactiveLayer,
};
pub use repo::{
    ConfigWarning, ConfigWarningCode, REPO_CONFIG_FILE, REPO_PROTECT_MARKER, RepoPolicy,
    RepoPolicyParse, RepoRelPath,
};
pub use roots::{RootConcern, RootConfirmation, expand_home, validate_roots};

use crate::tier::Tier;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Default activity window: 24 hours (decision D-8).
pub const DEFAULT_ACTIVITY_WINDOW: DurationSpec = DurationSpec::from_hours(24);

/// Default cap on the number of results an MCP tool returns (PRD §13.5).
pub const DEFAULT_MCP_MAX_RESULTS: u32 = 1000;

/// Default cap on agent plans awaiting approval (unexpired, unapproved) per machine and user
/// (#299).
pub const DEFAULT_MCP_MAX_PENDING_PLANS: u32 = 5;

/// Default number of `prepare_cleanup` calls an MCP session may make per minute (#299).
pub const DEFAULT_MCP_PREPARE_PER_MINUTE: u32 = 6;

/// Every effective config field, as the dotted key used in TOML, [`ResolvedConfig::origins`]
/// and `scull config show`.
pub const FIELDS: [&str; 35] = [
    "scan.roots",
    "scan.exclude",
    "scan.protect",
    "scan.min_tier",
    "scan.activity_window",
    "scan.threads",
    "scan.low_resource",
    "rules.enabled",
    "mcp.enabled",
    "mcp.destructive",
    "mcp.redact_home",
    "mcp.hash_names",
    "mcp.max_results",
    "mcp.max_pending_plans",
    "mcp.prepare_per_minute",
    "forge.enabled",
    "output.color",
    "output.units",
    "proactive.reserve_gib",
    "proactive.alert_days",
    "proactive.stale_worktree",
    "proactive.stale_build",
    "proactive.stale_cache",
    "proactive.spike_gib",
    "proactive.spike_window",
    "proactive.spike_percent",
    "proactive.spike_min_gib",
    "proactive.budgets",
    "quarantine.base",
    "quarantine.bases",
    "auto.paused",
    "auto.max_items_per_run",
    "auto.max_gib_per_run",
    "auto.max_gib_per_day",
    "auto.policy",
];

/// Environment variable that sets `quarantine.base` (see [`QuarantineConfig`]).
pub const QUARANTINE_BASE_ENV: &str = "SCULL_QUARANTINE_BASE";

/// Set (to a directory) by the test fixtures (`scull-fixtures`) on every scull process a test
/// starts. Under it, the Executor never uses a location outside that directory: quarantine
/// with no configured base is blocked instead of using the automatic location, a configured
/// base, a Trash or a quarantine entry outside it is refused (P2-EXEC-13).
/// It can only add refusals, never remove one, so it is safe to honor in release builds.
pub const TEST_SANDBOX_ENV: &str = "SCULL_TEST_SANDBOX";

/// The fields whose values from every layer are combined instead of overridden.
pub const ADDITIVE_FIELDS: [&str; 3] = ["scan.exclude", "scan.protect", "scan.min_tier"];

/// The effective configuration after merging every source.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// What to scan and how.
    pub scan: ScanConfig,
    /// Optional rules.
    pub rules: RulesConfig,
    /// The MCP server.
    pub mcp: McpConfig,
    /// Git forge lookups.
    pub forge: ForgeConfig,
    /// Terminal output.
    pub output: OutputConfig,
    /// Alert policies for proactive protection (PRD §16).
    pub proactive: ProactiveConfig,
    /// Where quarantined items are kept.
    pub quarantine: QuarantineConfig,
    /// Opt-in unattended auto-policies (D-11). User config file only.
    pub auto: AutoConfig,
}

/// Where the quarantine lives (P2-EXEC-13; PRD §10.6, D-3).
///
/// By default the quarantine of a volume is `<base>/.scull-quarantine/`, where `<base>` is
/// chosen automatically: the shallowest directory above the item, on its volume, that the
/// current user owns (usually the home directory). These settings replace that choice:
///
/// ```toml
/// [quarantine]
/// base = "~/.scull-q"                       # for items on the volume holding this directory
///
/// [quarantine.bases]                        # per volume: longest matching prefix wins
/// "/Volumes/Data" = "/Volumes/Data/me/.q"
/// "D:\\" = "D:\\Users\\me\\q"
/// ```
///
/// * For an item, the entry of `bases` whose key is the longest path prefix of the item's path
///   applies; otherwise `base`; otherwise the automatic choice.
/// * A configured base must be on the **same volume** as the item. If it isn't, that item is
///   not quarantined there: it falls back to the Trash (when enabled), else it is blocked. It
///   is never moved across volumes and never deleted instead.
/// * The base is created owner-only (`0700` / protected DACL) when missing, must be a real
///   directory (not a link) and must not be inside an item being cleaned; the
///   `.scull-quarantine` folder inside it is created and checked owner-only.
/// * Repository files (`.scull.toml`) can't set these (CFG-03).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct QuarantineConfig {
    /// Directory that holds `.scull-quarantine` for items on its volume. `None`: automatic.
    pub base: Option<PathBuf>,
    /// Per volume (or any directory): items under the key use the value as their base.
    pub bases: BTreeMap<PathBuf, PathBuf>,
}

impl QuarantineConfig {
    /// The configured base for an item at `item` (absolute): the `bases` entry with the longest
    /// key that is a path prefix of `item`, else `base`. `None`: nothing configured (the
    /// automatic choice applies).
    pub fn base_for(&self, item: &Path) -> Option<&Path> {
        quarantine_base_for(&self.bases, self.base.as_deref(), item)
    }

    /// Whether any base is configured.
    pub fn is_configured(&self) -> bool {
        self.base.is_some() || !self.bases.is_empty()
    }
}

/// The quarantine base for an item at `item`: the entry of `bases` with the longest key that is
/// a (component-wise) prefix of `item`, else `base`. Shared by [`QuarantineConfig::base_for`]
/// and the Executor's settings.
pub fn quarantine_base_for<'a>(
    bases: &'a BTreeMap<PathBuf, PathBuf>,
    base: Option<&'a Path>,
    item: &Path,
) -> Option<&'a Path> {
    bases
        .iter()
        .filter(|(k, _)| item.starts_with(k))
        .max_by_key(|(k, _)| k.components().count())
        .map(|(_, v)| v.as_path())
        .or(base)
}

/// Scan settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ScanConfig {
    /// Directories to scan. Empty by default: roots must be explicit (CFG-04).
    pub roots: Vec<PathBuf>,
    /// Paths never scanned (and so never candidates).
    pub exclude: BTreeSet<PathBuf>,
    /// Protected paths: everything inside is S3 (PRD §8.3).
    pub protect: BTreeSet<PathBuf>,
    /// Minimum tier for everything inside a path (e.g. `"~/Projects/legacy" = "S2"`).
    pub min_tier: BTreeMap<PathBuf, Tier>,
    /// Items modified within this window are bumped one tier (default 24 h, D-8).
    pub activity_window: DurationSpec,
    /// Scanner threads; `0` picks automatically.
    pub threads: u32,
    /// Low-resource mode: fewer threads and lower I/O priority.
    pub low_resource: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            exclude: BTreeSet::new(),
            protect: BTreeSet::new(),
            min_tier: BTreeMap::new(),
            activity_window: DEFAULT_ACTIVITY_WINDOW,
            threads: 0,
            low_resource: false,
        }
    }
}

/// Optional (user or community) rules. Disabled until enabled here (CLS-04).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct RulesConfig {
    /// IDs of optional rules the user turned on.
    pub enabled: BTreeSet<String>,
}

/// MCP server settings (PRD §13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct McpConfig {
    /// Whether `scull mcp serve` may run at all.
    pub enabled: bool,
    /// Destructive tools; also needs the `--allow-destructive` launch flag. File-only setting.
    pub destructive: bool,
    /// Replace the home directory prefix with `~` in results (on by default, PRD §13.5).
    pub redact_home: bool,
    /// Hash project and directory names in results.
    pub hash_names: bool,
    /// Maximum number of results per tool call.
    pub max_results: u32,
    /// Most agent plans awaiting approval (unexpired, unapproved) per machine and user; a
    /// `prepare_cleanup` beyond it is refused (`too_many_pending_plans`). File-only setting;
    /// `0` counts as `1`.
    pub max_pending_plans: u32,
    /// Most `prepare_cleanup` calls per minute in one MCP session; more are refused
    /// (`rate_limited`). File-only setting; `0` counts as `1`.
    pub prepare_per_minute: u32,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destructive: false,
            redact_home: true,
            hash_names: false,
            max_results: DEFAULT_MCP_MAX_RESULTS,
            max_pending_plans: DEFAULT_MCP_MAX_PENDING_PLANS,
            prepare_per_minute: DEFAULT_MCP_PREPARE_PER_MINUTE,
        }
    }
}

/// Git forge lookups, e.g. for squash-merge detection. Off by default (D-9).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ForgeConfig {
    /// Whether scull may query a forge API over the network.
    pub enabled: bool,
}

/// Terminal output preferences.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct OutputConfig {
    /// When to use color.
    pub color: ColorChoice,
    /// How to print sizes.
    pub units: SizeUnits,
}

/// When to use color in terminal output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColorChoice {
    /// Color when writing to a terminal and `NO_COLOR` is unset.
    #[default]
    Auto,
    /// Always color.
    Always,
    /// Never color.
    Never,
}

/// How sizes are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SizeUnits {
    /// Powers of 1024 (KiB, MiB, GiB).
    #[default]
    Binary,
    /// Powers of 1000 (kB, MB, GB).
    Decimal,
}

impl Config {
    /// A copy with a leading `~` in every configured path replaced by `home`
    /// (see [`expand_home`]).
    #[must_use]
    pub fn with_home_expanded(&self, home: &Path) -> Config {
        let mut c = self.clone();
        let s = &mut c.scan;
        s.roots = s.roots.iter().map(|p| expand_home(p, home)).collect();
        s.exclude = s.exclude.iter().map(|p| expand_home(p, home)).collect();
        s.protect = s.protect.iter().map(|p| expand_home(p, home)).collect();
        let mut min = BTreeMap::new();
        for (p, t) in &s.min_tier {
            let e = min.entry(expand_home(p, home)).or_insert(*t);
            *e = (*e).max(*t);
        }
        s.min_tier = min;
        for b in &mut c.proactive.budgets {
            if let Some(p) = &b.path {
                b.path = Some(expand_home(p, home));
            }
        }
        let q = &mut c.quarantine;
        q.base = q.base.as_ref().map(|p| expand_home(p, home));
        q.bases = q
            .bases
            .iter()
            .map(|(k, v)| (expand_home(k, home), expand_home(v, home)))
            .collect();
        for p in &mut c.auto.policy {
            p.under = p.under.iter().map(|u| expand_home(u, home)).collect();
        }
        c
    }

    /// The configuration that applies inside `repo_dir`, after adding the repository's
    /// untrusted [`RepoPolicy`].
    ///
    /// Only protection can change: paths are added to `scan.exclude` / `scan.protect`, entries
    /// in `scan.min_tier` are added or raised, and the activity window may grow. Every other
    /// field is returned unchanged. A repository activity window shorter than the effective one
    /// is ignored and reported as a warning.
    pub fn with_repo_policy(
        &self,
        repo_dir: &Path,
        policy: &RepoPolicy,
    ) -> (Config, Vec<ConfigWarning>) {
        let mut c = self.clone();
        let mut warnings = Vec::new();
        for p in policy.protect() {
            c.scan.protect.insert(p.under(repo_dir));
        }
        for p in policy.exclude() {
            c.scan.exclude.insert(p.under(repo_dir));
        }
        for (p, t) in policy.min_tier() {
            let e = c.scan.min_tier.entry(p.under(repo_dir)).or_insert(*t);
            *e = (*e).max(*t);
        }
        if let Some(w) = policy.activity_window() {
            if w > c.scan.activity_window {
                c.scan.activity_window = w;
            } else if w < c.scan.activity_window {
                warnings.push(ConfigWarning::new(
                    ConfigWarningCode::RepoLowersProtection,
                    "scan.activity_window",
                    format!(
                        "repository activity window {w} is shorter than the configured {}; ignored (repository files can only add protection)",
                        c.scan.activity_window
                    ),
                ));
            }
        }
        (c, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let c = Config::default();
        assert!(c.scan.roots.is_empty(), "CFG-04: no default roots");
        assert_eq!(c.scan.activity_window.as_secs(), 24 * 3600, "D-8");
        assert!(!c.mcp.enabled && !c.mcp.destructive);
        assert!(c.mcp.redact_home, "PRD §13.5: home redaction on by default");
        assert!(!c.forge.enabled, "D-9: forge lookups opt-in");
        assert!(c.rules.enabled.is_empty(), "CLS-04: optional rules off");
    }

    #[test]
    fn fields_list_matches_serialized_config() {
        let v = serde_json::to_value(Config::default()).unwrap();
        let mut keys = Vec::new();
        for (sec, inner) in v.as_object().unwrap() {
            for k in inner.as_object().unwrap().keys() {
                keys.push(format!("{sec}.{k}"));
            }
        }
        keys.sort();
        let mut want: Vec<String> = FIELDS.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(keys, want);
    }

    #[test]
    fn home_expansion_applies_to_all_paths() {
        let mut c = Config::default();
        c.scan.roots.push("~/Projects".into());
        c.scan.exclude.insert("~/big".into());
        c.scan.protect.insert("~".into());
        c.scan.min_tier.insert("~/old".into(), Tier::S2);
        let home = Path::new("/home/me");
        let e = c.with_home_expanded(home);
        assert_eq!(e.scan.roots, vec![home.join("Projects")]);
        assert!(e.scan.exclude.contains(&home.join("big")));
        assert!(e.scan.protect.contains(home));
        assert_eq!(e.scan.min_tier.get(&home.join("old")), Some(&Tier::S2));
    }
}
