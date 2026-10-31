//! Config layers (CLI, environment, user file), the merge (CFG-02) and value origins.

use super::{ColorChoice, Config, DurationSpec, FIELDS, ProactiveLayer, SizeUnits};
use crate::error::{ErrorKind, ScullError};
use crate::tier::Tier;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

/// One trusted configuration source with every field optional: the user config file, the
/// environment ([`EnvLayer`]) or CLI flags. `None` means "not set here".
///
/// This is also the **user config file format** (TOML). Unknown keys are rejected:
///
/// ```toml
/// [scan]
/// roots = ["~/Projects", "~/work"]
/// exclude = ["~/Projects/huge-monorepo"]
/// protect = ["~/Projects/thesis"]
/// activity_window = "48h"
/// threads = 0              # 0 = automatic
/// low_resource = false
///
/// [scan.min_tier]
/// "~/Projects/legacy" = "S2"
///
/// [rules]
/// enabled = ["community.unity-library"]
///
/// [mcp]
/// enabled = true
/// destructive = false
/// redact_home = true
/// hash_names = false
/// max_results = 1000
///
/// [forge]
/// enabled = false
///
/// [output]
/// color = "auto"           # auto | always | never
/// units = "binary"         # binary | decimal
///
/// [proactive]              # alert policies; see `ProactiveConfig`
/// reserve_gib = 10
/// alert_days = 14
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigLayer {
    /// What to scan and how.
    #[serde(default, skip_serializing_if = "ScanLayer::is_empty")]
    pub scan: ScanLayer,
    /// Optional rules.
    #[serde(default, skip_serializing_if = "RulesLayer::is_empty")]
    pub rules: RulesLayer,
    /// The MCP server.
    #[serde(default, skip_serializing_if = "McpLayer::is_empty")]
    pub mcp: McpLayer,
    /// Git forge lookups.
    #[serde(default, skip_serializing_if = "ForgeLayer::is_empty")]
    pub forge: ForgeLayer,
    /// Terminal output.
    #[serde(default, skip_serializing_if = "OutputLayer::is_empty")]
    pub output: OutputLayer,
    /// Alert policies (user config file only).
    #[serde(default, skip_serializing_if = "ProactiveLayer::is_empty")]
    pub proactive: ProactiveLayer,
}

/// `[scan]` in a [`ConfigLayer`]. See [`super::ScanConfig`] for the meaning of each field.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanLayer {
    /// Directories to scan (replaces lower layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<PathBuf>>,
    /// Paths never scanned (added to lower layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<PathBuf>>,
    /// Protected paths, always S3 (added to lower layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protect: Option<Vec<PathBuf>>,
    /// Minimum tier per path (added to lower layers; the most protective tier wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_tier: Option<BTreeMap<PathBuf, Tier>>,
    /// Activity window, e.g. `"24h"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_window: Option<DurationSpec>,
    /// Scanner threads; `0` picks automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<u32>,
    /// Low-resource mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_resource: Option<bool>,
}

/// `[rules]` in a [`ConfigLayer`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RulesLayer {
    /// IDs of optional rules to enable (replaces lower layers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<Vec<String>>,
}

/// `[mcp]` in a [`ConfigLayer`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpLayer {
    /// Whether the MCP server may run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Destructive tools. Only honored from the user config file (see [`resolve`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    /// Replace the home prefix with `~` in results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redact_home: Option<bool>,
    /// Hash project and directory names in results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_names: Option<bool>,
    /// Maximum results per tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u32>,
}

/// `[forge]` in a [`ConfigLayer`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgeLayer {
    /// Whether forge lookups are allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `[output]` in a [`ConfigLayer`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputLayer {
    /// When to use color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<ColorChoice>,
    /// How to print sizes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<SizeUnits>,
}

impl ScanLayer {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
impl RulesLayer {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
impl McpLayer {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
impl ForgeLayer {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}
impl OutputLayer {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl ConfigLayer {
    /// Parse a user config file. Unknown keys and invalid values are an
    /// [`ErrorKind::Config`] error (exit code 2) whose message names the line, the key and the
    /// expected keys. The caller adds the file path with [`ScullError::with_path`].
    ///
    /// ```
    /// use scull_core::config::ConfigLayer;
    /// use scull_core::ErrorKind;
    ///
    /// let err = ConfigLayer::from_toml("[scan]\nrootz = []\n").unwrap_err();
    /// assert_eq!(err.kind, ErrorKind::Config);
    /// assert!(err.message.contains("unknown field `rootz`"));
    /// assert_eq!(err.exit_code().as_i32(), 2);
    /// ```
    pub fn from_toml(text: &str) -> Result<Self, ScullError> {
        toml::from_str(text).map_err(|e| {
            ScullError::new(
                ErrorKind::Config,
                format!("invalid user config: {}", e.to_string().trim_end()),
            )
        })
    }

    /// Whether no field is set.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// An environment variable scull reads, for docs and `scull doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvVar {
    /// Variable name.
    pub name: &'static str,
    /// The config field it sets (a key in [`FIELDS`]).
    pub field: &'static str,
    /// Accepted values.
    pub help: &'static str,
}

/// Every environment variable read by [`EnvLayer::from_lookup`].
///
/// Path lists use the platform's `PATH` separator (`:` on Unix, `;` on Windows). Booleans
/// accept `1/0`, `true/false`, `yes/no`, `on/off`. For `output.color`, `SCULL_COLOR` wins over
/// `SCULL_NO_COLOR`, which wins over the standard `NO_COLOR`. There is deliberately no variable
/// for `mcp.destructive` or `scan.min_tier`.
pub const ENV_VARS: [EnvVar; 16] = [
    EnvVar {
        name: "SCULL_ROOTS",
        field: "scan.roots",
        help: "path list; replaces configured scan roots",
    },
    EnvVar {
        name: "SCULL_EXCLUDE",
        field: "scan.exclude",
        help: "path list; added to configured exclusions",
    },
    EnvVar {
        name: "SCULL_PROTECT",
        field: "scan.protect",
        help: "path list; added to configured protected paths",
    },
    EnvVar {
        name: "SCULL_ACTIVITY_WINDOW",
        field: "scan.activity_window",
        help: "duration, e.g. 24h, 90m, 7d",
    },
    EnvVar {
        name: "SCULL_THREADS",
        field: "scan.threads",
        help: "number of scanner threads; 0 = automatic",
    },
    EnvVar {
        name: "SCULL_LOW_RESOURCE",
        field: "scan.low_resource",
        help: "boolean",
    },
    EnvVar {
        name: "SCULL_RULES",
        field: "rules.enabled",
        help: "comma-separated optional rule IDs to enable",
    },
    EnvVar {
        name: "SCULL_MCP",
        field: "mcp.enabled",
        help: "boolean",
    },
    EnvVar {
        name: "SCULL_MCP_REDACT_HOME",
        field: "mcp.redact_home",
        help: "boolean",
    },
    EnvVar {
        name: "SCULL_MCP_HASH_NAMES",
        field: "mcp.hash_names",
        help: "boolean",
    },
    EnvVar {
        name: "SCULL_MCP_MAX_RESULTS",
        field: "mcp.max_results",
        help: "positive integer",
    },
    EnvVar {
        name: "SCULL_FORGE",
        field: "forge.enabled",
        help: "boolean",
    },
    EnvVar {
        name: "SCULL_COLOR",
        field: "output.color",
        help: "auto, always or never",
    },
    EnvVar {
        name: "SCULL_NO_COLOR",
        field: "output.color",
        help: "any non-empty value means never",
    },
    EnvVar {
        name: "NO_COLOR",
        field: "output.color",
        help: "any non-empty value means never (no-color.org)",
    },
    EnvVar {
        name: "SCULL_UNITS",
        field: "output.units",
        help: "binary or decimal",
    },
];

/// The environment as a config layer, plus which variable set each field (for [`Origin`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvLayer {
    /// The values read.
    pub layer: ConfigLayer,
    /// Field key → the variable that set it.
    pub vars: BTreeMap<&'static str, &'static str>,
}

fn env_err(var: &str, value: &str, why: impl fmt::Display) -> ScullError {
    ScullError::new(
        ErrorKind::Config,
        format!("invalid environment variable {var}={value:?}: {why}"),
    )
}

fn parse_bool(var: &str, v: &str) -> Result<bool, ScullError> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(env_err(
            var,
            v,
            "expected a boolean (true/false, 1/0, yes/no, on/off)",
        )),
    }
}

fn parse_u32(var: &str, v: &str) -> Result<u32, ScullError> {
    v.trim()
        .parse()
        .map_err(|_| env_err(var, v, "expected a non-negative integer"))
}

fn parse_paths(var: &str, v: &str) -> Result<Vec<PathBuf>, ScullError> {
    let paths: Vec<PathBuf> = std::env::split_paths(v)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    if paths.is_empty() {
        return Err(env_err(var, v, "expected at least one path"));
    }
    Ok(paths)
}

fn parse_enum<T: serde::de::DeserializeOwned>(
    var: &str,
    v: &str,
    want: &str,
) -> Result<T, ScullError> {
    T::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(v.trim()))
        .map_err(|_| env_err(var, v, format!("expected {want}")))
}

impl EnvLayer {
    /// Build the environment layer from a lookup function (normally `|k| std::env::var(k).ok()`,
    /// injected so this stays pure and testable). Variables set to an empty string are treated
    /// as unset. An invalid value is an [`ErrorKind::Config`] error naming the variable.
    ///
    /// ```
    /// use scull_core::config::EnvLayer;
    ///
    /// let env = EnvLayer::from_lookup(|k| (k == "SCULL_THREADS").then(|| "4".to_string())).unwrap();
    /// assert_eq!(env.layer.scan.threads, Some(4));
    /// assert_eq!(env.vars.get("scan.threads"), Some(&"SCULL_THREADS"));
    /// ```
    pub fn from_lookup<F: Fn(&str) -> Option<String>>(get: F) -> Result<Self, ScullError> {
        let mut out = EnvLayer::default();
        let get = |k: &str| get(k).filter(|v| !v.is_empty());
        let l = &mut out.layer;
        let vars = &mut out.vars;
        let mut set = |field: &'static str, var: &'static str| {
            vars.insert(field, var);
        };
        if let Some(v) = get("SCULL_ROOTS") {
            l.scan.roots = Some(parse_paths("SCULL_ROOTS", &v)?);
            set("scan.roots", "SCULL_ROOTS");
        }
        if let Some(v) = get("SCULL_EXCLUDE") {
            l.scan.exclude = Some(parse_paths("SCULL_EXCLUDE", &v)?);
            set("scan.exclude", "SCULL_EXCLUDE");
        }
        if let Some(v) = get("SCULL_PROTECT") {
            l.scan.protect = Some(parse_paths("SCULL_PROTECT", &v)?);
            set("scan.protect", "SCULL_PROTECT");
        }
        if let Some(v) = get("SCULL_ACTIVITY_WINDOW") {
            let d = v
                .parse::<DurationSpec>()
                .map_err(|e| env_err("SCULL_ACTIVITY_WINDOW", &v, e))?;
            l.scan.activity_window = Some(d);
            set("scan.activity_window", "SCULL_ACTIVITY_WINDOW");
        }
        if let Some(v) = get("SCULL_THREADS") {
            l.scan.threads = Some(parse_u32("SCULL_THREADS", &v)?);
            set("scan.threads", "SCULL_THREADS");
        }
        if let Some(v) = get("SCULL_LOW_RESOURCE") {
            l.scan.low_resource = Some(parse_bool("SCULL_LOW_RESOURCE", &v)?);
            set("scan.low_resource", "SCULL_LOW_RESOURCE");
        }
        if let Some(v) = get("SCULL_RULES") {
            let ids: Vec<String> = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            l.rules.enabled = Some(ids);
            set("rules.enabled", "SCULL_RULES");
        }
        if let Some(v) = get("SCULL_MCP") {
            l.mcp.enabled = Some(parse_bool("SCULL_MCP", &v)?);
            set("mcp.enabled", "SCULL_MCP");
        }
        if let Some(v) = get("SCULL_MCP_REDACT_HOME") {
            l.mcp.redact_home = Some(parse_bool("SCULL_MCP_REDACT_HOME", &v)?);
            set("mcp.redact_home", "SCULL_MCP_REDACT_HOME");
        }
        if let Some(v) = get("SCULL_MCP_HASH_NAMES") {
            l.mcp.hash_names = Some(parse_bool("SCULL_MCP_HASH_NAMES", &v)?);
            set("mcp.hash_names", "SCULL_MCP_HASH_NAMES");
        }
        if let Some(v) = get("SCULL_MCP_MAX_RESULTS") {
            let n = parse_u32("SCULL_MCP_MAX_RESULTS", &v)?;
            if n == 0 {
                return Err(env_err("SCULL_MCP_MAX_RESULTS", &v, "must be at least 1"));
            }
            l.mcp.max_results = Some(n);
            set("mcp.max_results", "SCULL_MCP_MAX_RESULTS");
        }
        if let Some(v) = get("SCULL_FORGE") {
            l.forge.enabled = Some(parse_bool("SCULL_FORGE", &v)?);
            set("forge.enabled", "SCULL_FORGE");
        }
        if let Some(v) = get("SCULL_COLOR") {
            l.output.color = Some(parse_enum("SCULL_COLOR", &v, "auto, always or never")?);
            set("output.color", "SCULL_COLOR");
        } else if get("SCULL_NO_COLOR").is_some() {
            l.output.color = Some(ColorChoice::Never);
            set("output.color", "SCULL_NO_COLOR");
        } else if get("NO_COLOR").is_some() {
            l.output.color = Some(ColorChoice::Never);
            set("output.color", "NO_COLOR");
        }
        if let Some(v) = get("SCULL_UNITS") {
            l.output.units = Some(parse_enum("SCULL_UNITS", &v, "binary or decimal")?);
            set("output.units", "SCULL_UNITS");
        }
        Ok(out)
    }
}

/// Where an effective config value came from.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum Origin {
    /// The built-in default.
    Default,
    /// The user config file.
    UserConfig,
    /// An environment variable.
    Env {
        /// The variable name.
        var: String,
    },
    /// A command-line flag.
    Cli,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Default => f.write_str("default"),
            Origin::UserConfig => f.write_str("user config"),
            Origin::Env { var } => write!(f, "env {var}"),
            Origin::Cli => f.write_str("command line"),
        }
    }
}

/// The effective config and where each value came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedConfig {
    /// The merged configuration.
    pub config: Config,
    /// For every key in [`FIELDS`]: the source of the value. Override fields have exactly one
    /// origin; additive fields list every layer that contributed (most important first), or
    /// `[default]` if none did.
    pub origins: BTreeMap<String, Vec<Origin>>,
}

impl ResolvedConfig {
    /// The origins of `field` (a key in [`FIELDS`]); empty for an unknown key.
    pub fn origin(&self, field: &str) -> &[Origin] {
        self.origins.get(field).map_or(&[], Vec::as_slice)
    }
}

/// Merge the trusted layers over the built-in defaults (CFG-02): `cli` > `env` > `user` >
/// [`Config::default`]. See the [module docs](super) for which fields are additive.
///
/// `mcp.destructive` is taken from `user` only; values in `cli` or `env` are ignored, so
/// destructive MCP tools always need an explicit line in the config file.
pub fn resolve(cli: &ConfigLayer, env: &EnvLayer, user: &ConfigLayer) -> ResolvedConfig {
    let env_origin = |field: &str| Origin::Env {
        var: env
            .vars
            .get(field)
            .map_or_else(|| "environment".to_string(), |v| (*v).to_string()),
    };
    // Most important first.
    let layers: [&ConfigLayer; 3] = [cli, &env.layer, user];
    let origin_of = |i: usize, field: &str| match i {
        0 => Origin::Cli,
        1 => env_origin(field),
        _ => Origin::UserConfig,
    };

    let mut config = Config::default();
    let mut origins: BTreeMap<String, Vec<Origin>> = FIELDS
        .iter()
        .map(|f| ((*f).to_string(), vec![Origin::Default]))
        .collect();

    macro_rules! pick {
        ($sec:ident . $field:ident) => {{
            let key = concat!(stringify!($sec), ".", stringify!($field));
            if let Some((i, v)) = layers
                .iter()
                .enumerate()
                .find_map(|(i, l)| l.$sec.$field.as_ref().map(|v| (i, v)))
            {
                config.$sec.$field = v.clone().into_iter().collect();
                origins.insert(key.to_string(), vec![origin_of(i, key)]);
            }
        }};
        ($sec:ident . $field:ident, scalar) => {{
            let key = concat!(stringify!($sec), ".", stringify!($field));
            if let Some((i, v)) = layers
                .iter()
                .enumerate()
                .find_map(|(i, l)| l.$sec.$field.as_ref().map(|v| (i, v)))
            {
                config.$sec.$field = v.clone();
                origins.insert(key.to_string(), vec![origin_of(i, key)]);
            }
        }};
    }

    pick!(scan.roots);
    pick!(scan.activity_window, scalar);
    pick!(scan.threads, scalar);
    pick!(scan.low_resource, scalar);
    pick!(rules.enabled);
    pick!(mcp.enabled, scalar);
    pick!(mcp.redact_home, scalar);
    pick!(mcp.hash_names, scalar);
    pick!(mcp.max_results, scalar);
    pick!(forge.enabled, scalar);
    pick!(output.color, scalar);
    pick!(output.units, scalar);
    pick!(proactive.reserve_gib, scalar);
    pick!(proactive.alert_days, scalar);
    pick!(proactive.stale_worktree, scalar);
    pick!(proactive.stale_build, scalar);
    pick!(proactive.stale_cache, scalar);
    pick!(proactive.spike_gib, scalar);
    pick!(proactive.spike_window, scalar);
    pick!(proactive.spike_percent, scalar);
    pick!(proactive.spike_min_gib, scalar);
    pick!(proactive.budgets, scalar);

    // File-only: destructive MCP tools.
    if let Some(v) = user.mcp.destructive {
        config.mcp.destructive = v;
        origins.insert("mcp.destructive".into(), vec![Origin::UserConfig]);
    }

    // Additive protection fields.
    let mut add_origin = |key: &str, i: usize| {
        let o = origin_of(i, key);
        let e = origins.entry(key.to_string()).or_default();
        if e.as_slice() == [Origin::Default] {
            e.clear();
        }
        e.push(o);
    };
    for (i, l) in layers.iter().enumerate() {
        if let Some(v) = &l.scan.exclude {
            config.scan.exclude.extend(v.iter().cloned());
            add_origin("scan.exclude", i);
        }
        if let Some(v) = &l.scan.protect {
            config.scan.protect.extend(v.iter().cloned());
            add_origin("scan.protect", i);
        }
        if let Some(v) = &l.scan.min_tier {
            for (p, t) in v {
                let e = config.scan.min_tier.entry(p.clone()).or_insert(*t);
                *e = (*e).max(*t);
            }
            add_origin("scan.min_tier", i);
        }
    }

    ResolvedConfig { config, origins }
}
