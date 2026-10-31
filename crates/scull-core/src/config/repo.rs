//! Untrusted per-repository policy files (CFG-03, THREAT_MODEL T-20).

use super::DurationSpec;
use crate::tier::Tier;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

/// File name of the protection marker.
pub const REPO_PROTECT_MARKER: &str = ".scull-protect";

/// File name of the repository policy file.
pub const REPO_CONFIG_FILE: &str = ".scull.toml";

/// Why a configuration setting was ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigWarningCode {
    /// The repository file is not valid TOML; its directory was protected instead.
    RepoParseError,
    /// A setting repository files may not change (roots, rules, MCP, forge, output, threads).
    RepoForbiddenSetting,
    /// A setting that would lower protection (an S0 tier, a shorter activity window).
    RepoLowersProtection,
    /// A path that is absolute, uses `..`, a drive letter, a UNC prefix or `~`.
    RepoPathRejected,
    /// A value of the wrong type or format.
    RepoInvalidValue,
    /// A key scull doesn't know.
    RepoUnknownKey,
}

/// A non-fatal configuration problem: the setting was ignored, scanning continues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigWarning {
    /// Machine-readable reason.
    pub code: ConfigWarningCode,
    /// The dotted key involved (e.g. `scan.roots`, `scan.protect[2]`).
    pub key: String,
    /// Human explanation.
    pub message: String,
}

impl ConfigWarning {
    /// A warning.
    pub fn new(
        code: ConfigWarningCode,
        key: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            key: key.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

/// A path from a repository file, validated to stay inside the file's directory.
///
/// Stored as `/`-separated normal components; `"."` is the directory itself. Rejected: empty
/// strings, NUL, absolute paths (`/x`, `\x`), UNC paths (`\\server\share`), anything with `:`
/// (drive letters, NTFS streams), a leading `~`, and any component that is `..` or consists only
/// of dots and spaces (Windows trims those, so `.. ` would mean `..`).
///
/// ```
/// use scull_core::config::RepoRelPath;
///
/// assert_eq!(RepoRelPath::parse("data/./raw/").unwrap().as_str(), "data/raw");
/// assert_eq!(RepoRelPath::parse("./").unwrap().as_str(), ".");
/// for bad in ["../x", "/etc", "C:\\x", "\\\\srv\\share", "~/x", "a/../../b"] {
///     assert!(RepoRelPath::parse(bad).is_err(), "{bad}");
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct RepoRelPath(String);

impl RepoRelPath {
    /// The repository directory itself.
    pub fn root() -> Self {
        Self(".".into())
    }

    /// Validate and normalize a path written in a repository file.
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.trim().is_empty() {
            return Err("empty path".into());
        }
        if s.contains('\0') {
            return Err("path contains a NUL byte".into());
        }
        if s.starts_with('/') || s.starts_with('\\') {
            return Err(format!(
                "`{s}` is absolute; paths must be relative to the repository"
            ));
        }
        if s.contains(':') {
            return Err(format!(
                "`{s}` contains `:` (drive letter or stream); paths must be relative to the repository"
            ));
        }
        if s.starts_with('~') {
            return Err(format!(
                "`{s}` refers to a home directory; paths must be relative to the repository"
            ));
        }
        let mut parts = Vec::new();
        for c in s.split(['/', '\\']) {
            if c.is_empty() || c == "." {
                continue;
            }
            if c.chars().all(|ch| ch == '.' || ch == ' ') {
                return Err(format!(
                    "`{s}` leaves the repository (`{c}` component); paths must stay inside it"
                ));
            }
            parts.push(c);
        }
        if parts.first().is_some_and(|c| c.starts_with('~')) {
            return Err(format!(
                "`{s}` refers to a home directory; paths must be relative to the repository"
            ));
        }
        if parts.is_empty() {
            Ok(Self::root())
        } else {
            Ok(Self(parts.join("/")))
        }
    }

    /// The normalized string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The absolute path under `base` (the directory containing the repository file).
    pub fn under(&self, base: &Path) -> PathBuf {
        let mut p = base.to_path_buf();
        if self.0 != "." {
            for c in self.0.split('/') {
                p.push(c);
            }
        }
        p
    }
}

impl fmt::Display for RepoRelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Protection added by a repository's own files. It has no field that could lower protection:
/// the only way to build one from untrusted text is [`RepoPolicy::parse_toml`] or
/// [`RepoPolicy::protect_marker`], and [`crate::config::Config::with_repo_policy`] only adds.
///
/// # `.scull-protect`
///
/// A marker file. Its presence protects the directory that contains it, and everything inside,
/// as S3 (FX-CLS-11). Its contents are ignored.
///
/// # `.scull.toml`
///
/// Uses the `[scan]` section of the user config format, restricted to settings that add
/// protection. Every path is relative to the directory containing the file and must stay inside
/// it:
///
/// ```toml
/// [scan]
/// protect = ["data", "notebooks/output"]   # S3
/// exclude = ["vendor/huge"]                # never scanned
/// activity_window = "72h"                  # only used if longer than the user's window
/// min_tier = "S2"                          # shorthand for { "." = "S2" }: everything here >= S2
///
/// # or, per path:
/// # [scan.min_tier]
/// # "build" = "S1"
/// ```
///
/// Everything else (`scan.roots`, `scan.threads`, `[rules]`, `[mcp]`, `[forge]`, `[output]`,
/// unknown keys, `S0` tiers, rejected paths) is ignored and reported as a [`ConfigWarning`].
/// A file that is not valid TOML protects its whole directory (S3) and is reported, since its
/// author evidently meant to configure scull there; it never stops the scan.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoPolicy {
    protect: BTreeSet<RepoRelPath>,
    exclude: BTreeSet<RepoRelPath>,
    min_tier: BTreeMap<RepoRelPath, Tier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activity_window: Option<DurationSpec>,
}

/// The result of parsing a repository file: the policy to apply and what was ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoPolicyParse {
    /// What the file adds.
    pub policy: RepoPolicy,
    /// Settings that were ignored, and why.
    pub warnings: Vec<ConfigWarning>,
}

fn forbidden_reason(key: &str) -> Option<&'static str> {
    let section = key.split('.').next().unwrap_or(key);
    Some(match (section, key) {
        (_, "scan.roots") => "repository files cannot add scan roots",
        (_, "scan.threads" | "scan.low_resource") => {
            "repository files cannot change performance settings"
        }
        ("rules", _) => "repository files cannot enable or change rules",
        ("mcp", _) => "repository files cannot change MCP settings",
        ("forge", _) => "repository files cannot enable forge lookups",
        ("output", _) => "repository files cannot change output preferences",
        ("proactive", _) => "repository files cannot change alert policies",
        _ => return None,
    })
}

fn type_name(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

/// Every leaf key under `prefix` (tables are expanded, arrays are leaves).
fn leaf_keys(prefix: &str, v: &toml::Value, out: &mut Vec<String>) {
    match v {
        toml::Value::Table(t) if !t.is_empty() => {
            for (k, v) in t {
                leaf_keys(&format!("{prefix}.{k}"), v, out);
            }
        }
        _ => out.push(prefix.to_string()),
    }
}

impl RepoPolicy {
    /// The policy implied by a `.scull-protect` marker: the whole directory is S3.
    pub fn protect_marker() -> Self {
        let mut p = Self::default();
        p.protect.insert(RepoRelPath::root());
        p
    }

    /// Paths protected as S3.
    pub fn protect(&self) -> &BTreeSet<RepoRelPath> {
        &self.protect
    }

    /// Paths never scanned.
    pub fn exclude(&self) -> &BTreeSet<RepoRelPath> {
        &self.exclude
    }

    /// Minimum tier per path (always S1 or higher).
    pub fn min_tier(&self) -> &BTreeMap<RepoRelPath, Tier> {
        &self.min_tier
    }

    /// Requested activity window (applied only if longer than the effective one).
    pub fn activity_window(&self) -> Option<DurationSpec> {
        self.activity_window
    }

    /// Whether the policy adds nothing.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Combine with another policy for the same directory (e.g. `.scull-protect` and
    /// `.scull.toml`). The result is at least as protective as both.
    pub fn merge(&mut self, other: &RepoPolicy) {
        self.protect.extend(other.protect.iter().cloned());
        self.exclude.extend(other.exclude.iter().cloned());
        for (p, t) in &other.min_tier {
            let e = self.min_tier.entry(p.clone()).or_insert(*t);
            *e = (*e).max(*t);
        }
        self.activity_window = self.activity_window.max(other.activity_window);
    }

    /// Parse a `.scull.toml`. Never fails: ignored settings become warnings (see the
    /// [type docs](RepoPolicy)).
    pub fn parse_toml(text: &str) -> RepoPolicyParse {
        let mut out = RepoPolicyParse::default();
        let table = match toml::from_str::<toml::Table>(text) {
            Ok(t) => t,
            Err(e) => {
                out.policy.protect.insert(RepoRelPath::root());
                out.warnings.push(ConfigWarning::new(
                    ConfigWarningCode::RepoParseError,
                    REPO_CONFIG_FILE,
                    format!(
                        "not valid TOML, so the whole directory is protected: {}",
                        e.message()
                    ),
                ));
                return out;
            }
        };
        for (k, v) in &table {
            if k == "scan" {
                match v.as_table() {
                    Some(t) => out.parse_scan(t),
                    None => {
                        out.invalid("scan", format!("expected a table, found {}", type_name(v)))
                    }
                }
            } else {
                let mut keys = Vec::new();
                leaf_keys(k, v, &mut keys);
                for key in keys {
                    out.forbidden_or_unknown(&key);
                }
            }
        }
        out
    }
}

impl RepoPolicyParse {
    fn warn(&mut self, code: ConfigWarningCode, key: &str, msg: impl Into<String>) {
        self.warnings.push(ConfigWarning::new(code, key, msg));
    }

    fn invalid(&mut self, key: &str, why: String) {
        self.warn(
            ConfigWarningCode::RepoInvalidValue,
            key,
            format!("{why}; ignored"),
        );
    }

    fn forbidden_or_unknown(&mut self, key: &str) {
        match forbidden_reason(key) {
            Some(why) => self.warn(
                ConfigWarningCode::RepoForbiddenSetting,
                key,
                format!("{why}; ignored (repository files can only add protection)"),
            ),
            None => self.warn(
                ConfigWarningCode::RepoUnknownKey,
                key,
                "unknown key; ignored (repository files can only add protection)",
            ),
        }
    }

    fn path(&mut self, key: &str, v: &toml::Value) -> Option<RepoRelPath> {
        let Some(s) = v.as_str() else {
            self.invalid(
                key,
                format!("expected a path string, found {}", type_name(v)),
            );
            return None;
        };
        self.path_str(key, s)
    }

    fn path_str(&mut self, key: &str, s: &str) -> Option<RepoRelPath> {
        match RepoRelPath::parse(s) {
            Ok(p) => Some(p),
            Err(why) => {
                self.warn(
                    ConfigWarningCode::RepoPathRejected,
                    key,
                    format!("{why}; ignored"),
                );
                None
            }
        }
    }

    fn paths(&mut self, key: &str, v: &toml::Value) -> Vec<RepoRelPath> {
        match v {
            toml::Value::Array(items) => items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| self.path(&format!("{key}[{i}]"), item))
                .collect(),
            toml::Value::String(_) => self.path(key, v).into_iter().collect(),
            _ => {
                self.invalid(
                    key,
                    format!("expected a list of paths, found {}", type_name(v)),
                );
                Vec::new()
            }
        }
    }

    fn tier(&mut self, key: &str, path: RepoRelPath, v: &toml::Value) {
        let parsed = match v.as_str() {
            Some("S0") => {
                self.warn(
                    ConfigWarningCode::RepoLowersProtection,
                    key,
                    "repository files cannot lower a tier to S0; ignored",
                );
                return;
            }
            Some("S1") => Tier::S1,
            Some("S2") => Tier::S2,
            Some("S3") => Tier::S3,
            _ => {
                self.invalid(key, format!("expected \"S1\", \"S2\" or \"S3\", found {v}"));
                return;
            }
        };
        let e = self.policy.min_tier.entry(path).or_insert(parsed);
        *e = (*e).max(parsed);
    }

    fn parse_scan(&mut self, t: &toml::Table) {
        for (k, v) in t {
            let key = format!("scan.{k}");
            match k.as_str() {
                "protect" => {
                    let ps = self.paths(&key, v);
                    self.policy.protect.extend(ps);
                }
                "exclude" => {
                    let ps = self.paths(&key, v);
                    self.policy.exclude.extend(ps);
                }
                "activity_window" => match v.as_str().map(str::parse::<DurationSpec>) {
                    Some(Ok(d)) => {
                        self.policy.activity_window = self.policy.activity_window.max(Some(d));
                    }
                    Some(Err(why)) => self.invalid(&key, why),
                    None => self.invalid(
                        &key,
                        format!(
                            "expected a duration string like \"72h\", found {}",
                            type_name(v)
                        ),
                    ),
                },
                "min_tier" => match v {
                    toml::Value::Table(m) => {
                        for (p, tv) in m {
                            let k2 = format!("scan.min_tier.\"{p}\"");
                            if let Some(path) = self.path_str(&k2, p) {
                                self.tier(&k2, path, tv);
                            }
                        }
                    }
                    _ => self.tier(&key, RepoRelPath::root(), v),
                },
                _ => {
                    let mut keys = Vec::new();
                    leaf_keys(&key, v, &mut keys);
                    for key in keys {
                        self.forbidden_or_unknown(&key);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_normalizes() {
        for (inp, want) in [
            (".", "."),
            ("./", "."),
            ("data", "data"),
            ("data/", "data"),
            ("a//b/./c", "a/b/c"),
            ("a\\b", "a/b"),
            (".hidden", ".hidden"),
            ("a..b", "a..b"),
        ] {
            assert_eq!(RepoRelPath::parse(inp).unwrap().as_str(), want, "{inp}");
        }
    }

    #[test]
    fn rel_path_rejects_escapes() {
        for bad in [
            "",
            "  ",
            "..",
            "../x",
            "a/../../b",
            "a/..",
            "a\\..\\..\\b",
            "/etc/passwd",
            "\\Windows",
            "\\\\server\\share",
            "//server/share",
            "C:\\Users",
            "c:relative",
            "file.txt:stream",
            "~",
            "~/x",
            "~root/x",
            "./~",
            ".\\~/x",
            "a/.../b",
            "a/.. /b",
            "a\0b",
        ] {
            assert!(
                RepoRelPath::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn under_joins_components() {
        let base = Path::new("/r");
        assert_eq!(RepoRelPath::root().under(base), PathBuf::from("/r"));
        assert_eq!(
            RepoRelPath::parse("a/b").unwrap().under(base),
            Path::new("/r").join("a").join("b")
        );
    }

    #[test]
    fn marker_protects_whole_directory() {
        let p = RepoPolicy::protect_marker();
        assert!(p.protect().contains(&RepoRelPath::root()));
    }

    #[test]
    fn invalid_toml_protects_directory_and_warns() {
        let r = RepoPolicy::parse_toml("[scan\nprotect = ");
        assert!(r.policy.protect().contains(&RepoRelPath::root()));
        assert_eq!(r.warnings.len(), 1);
        assert_eq!(r.warnings[0].code, ConfigWarningCode::RepoParseError);
    }

    #[test]
    fn wrong_types_are_warnings() {
        let r = RepoPolicy::parse_toml("scan = 3\n");
        assert!(r.policy.is_empty());
        assert_eq!(r.warnings[0].code, ConfigWarningCode::RepoInvalidValue);
        let r = RepoPolicy::parse_toml(
            "[scan]\nprotect = 5\nexclude = [1, \"ok\"]\nactivity_window = 3\nmin_tier = \"S9\"\n",
        );
        assert_eq!(r.policy.exclude().len(), 1);
        assert_eq!(r.warnings.len(), 4, "{:?}", r.warnings);
        assert!(
            r.warnings
                .iter()
                .all(|w| w.code == ConfigWarningCode::RepoInvalidValue)
        );
    }

    #[test]
    fn merge_is_union() {
        let mut a =
            RepoPolicy::parse_toml("[scan]\nmin_tier = \"S1\"\nactivity_window = \"2d\"\n").policy;
        let b = RepoPolicy::parse_toml("[scan]\nmin_tier = \"S2\"\nexclude = [\"x\"]\n").policy;
        a.merge(&b);
        a.merge(&RepoPolicy::protect_marker());
        assert_eq!(a.min_tier().get(&RepoRelPath::root()), Some(&Tier::S2));
        assert!(a.exclude().contains(&RepoRelPath::parse("x").unwrap()));
        assert!(a.protect().contains(&RepoRelPath::root()));
        assert_eq!(a.activity_window().unwrap().as_secs(), 2 * 86_400);
    }
}
