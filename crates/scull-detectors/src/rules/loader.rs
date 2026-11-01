//! Parsing and strict validation of rule files, and the [`RuleSet`] that holds loaded rules.
//!
//! Loading a file is all-or-nothing: one bad rule rejects the whole file.

use super::error::{RuleError, RuleErrorKind};
use super::model::{
    ActivityChecks, EvidenceKind, MAX_UP, Platform, RegenerationGuidance, RelativeEntry, Rule,
    RuleSource, Strategy,
};
use super::pattern::NamePattern;
use scull_core::{Category, Tier};
use serde::Deserialize;
use std::collections::BTreeSet;

/// The rule file schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Bundled rule files, compiled in (CLS-05: no network fetch). Each entry is
/// `(origin, contents)`; the origin is the path under the repository root.
pub const BUNDLED_RULE_FILES: &[(&str, &str)] = &[
    (
        "rules/ecosystems/rust.toml",
        include_str!("../../../../rules/ecosystems/rust.toml"),
    ),
    (
        "rules/ecosystems/node.toml",
        include_str!("../../../../rules/ecosystems/node.toml"),
    ),
    (
        "rules/ecosystems/python.toml",
        include_str!("../../../../rules/ecosystems/python.toml"),
    ),
    (
        "rules/ecosystems/ml.toml",
        include_str!("../../../../rules/ecosystems/ml.toml"),
    ),
    (
        "rules/ecosystems/jvm.toml",
        include_str!("../../../../rules/ecosystems/jvm.toml"),
    ),
    (
        "rules/ecosystems/dotnet.toml",
        include_str!("../../../../rules/ecosystems/dotnet.toml"),
    ),
    (
        "rules/ecosystems/xcode.toml",
        include_str!("../../../../rules/ecosystems/xcode.toml"),
    ),
    (
        "rules/ecosystems/android.toml",
        include_str!("../../../../rules/ecosystems/android.toml"),
    ),
    (
        "rules/ecosystems/unity.toml",
        include_str!("../../../../rules/ecosystems/unity.toml"),
    ),
    (
        "rules/ecosystems/unreal.toml",
        include_str!("../../../../rules/ecosystems/unreal.toml"),
    ),
    (
        "rules/ecosystems/native.toml",
        include_str!("../../../../rules/ecosystems/native.toml"),
    ),
    (
        "rules/ecosystems/apple.toml",
        include_str!("../../../../rules/ecosystems/apple.toml"),
    ),
    (
        "rules/ecosystems/functional.toml",
        include_str!("../../../../rules/ecosystems/functional.toml"),
    ),
    (
        "rules/ecosystems/ruby.toml",
        include_str!("../../../../rules/ecosystems/ruby.toml"),
    ),
    (
        "rules/ecosystems/zig.toml",
        include_str!("../../../../rules/ecosystems/zig.toml"),
    ),
    (
        "rules/ecosystems/infra.toml",
        include_str!("../../../../rules/ecosystems/infra.toml"),
    ),
    (
        "rules/ecosystems/ide.toml",
        include_str!("../../../../rules/ecosystems/ide.toml"),
    ),
];

/// Key fragments and whole key tokens that suggest a command, script or hook. Any of these
/// anywhere in a rule file rejects it (CLS-04, FX-CFG-06), before schema checks, so the error is
/// explicit even if a future schema version grew a field with a similar name.
const COMMAND_KEY_FRAGMENTS: &[&str] = &["command", "cmd", "exec", "shell", "spawn"];
const COMMAND_KEY_TOKENS: &[&str] = &[
    "script",
    "scripts",
    "run",
    "args",
    "argv",
    "program",
    "hook",
    "hooks",
    "invoke",
    "binary",
    "sh",
    "bash",
    "pwsh",
    "powershell",
];

// ---------------------------------------------------------------------------------------------
// Raw (serde) shape. Every struct denies unknown fields.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[allow(dead_code)] // checked before deserialization
    schema_version: i64,
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    version: u32,
    artifact: String,
    ecosystem: String,
    category: Category,
    platforms: Vec<Platform>,
    base_tier: Tier,
    strategy: Strategy,
    evidence: Vec<EvidenceKind>,
    #[serde(rename = "match")]
    matching: RawMatch,
    #[serde(default)]
    markers: RawMarkers,
    #[serde(default)]
    lockfiles: RawLockfiles,
    #[serde(default)]
    exclude: RawExclude,
    #[serde(default)]
    activity: RawActivity,
    #[serde(default)]
    regeneration: RawRegeneration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMatch {
    names: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawMarkers {
    #[serde(default)]
    all: Vec<String>,
    #[serde(default)]
    any: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawLockfiles {
    #[serde(default)]
    any: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawExclude {
    #[serde(default)]
    markers: Vec<String>,
    #[serde(default)]
    within: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawActivity {
    #[serde(default)]
    processes: Vec<String>,
    #[serde(default)]
    lock_files: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRegeneration {
    instructions: Option<String>,
    network: Option<bool>,
    long_build: Option<bool>,
    quick: Option<bool>,
}

// ---------------------------------------------------------------------------------------------
// Parsing.

/// Parse and validate one rule file. `origin` is used in errors and recorded on each rule.
///
/// Checks, in order: TOML syntax · `schema_version` present and supported · no command-like
/// keys anywhere (CLS-04) · schema (unknown fields rejected) · per-rule validation (ID, version,
/// patterns, relative paths, marker required (CLS-03), evidence consistency, trust caps for
/// non-bundled rules (CLS-04)) · duplicate IDs within the file.
pub fn parse_rules(text: &str, origin: &str, source: RuleSource) -> Result<Vec<Rule>, RuleError> {
    let value: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        RuleError::file(origin, RuleErrorKind::Syntax(e.to_string()))
    })?;

    match value.get("schema_version") {
        None => return Err(RuleError::file(origin, RuleErrorKind::MissingSchemaVersion)),
        Some(toml::Value::Integer(v)) if *v == i64::from(SCHEMA_VERSION) => {}
        Some(toml::Value::Integer(v)) => {
            return Err(RuleError::file(
                origin,
                RuleErrorKind::UnsupportedSchemaVersion {
                    found: *v,
                    supported: SCHEMA_VERSION,
                },
            ));
        }
        Some(_) => {
            return Err(RuleError::file(
                origin,
                RuleErrorKind::Schema("`schema_version` must be an integer".into()),
            ));
        }
    }

    if let Some(key) = find_command_key(&value, "") {
        return Err(RuleError::file(origin, RuleErrorKind::CommandField { key }));
    }

    let raw: RawFile = toml::Value::Table(value)
        .try_into()
        .map_err(|e: toml::de::Error| {
            RuleError::file(origin, RuleErrorKind::Schema(e.to_string()))
        })?;

    if raw.rule.is_empty() {
        return Err(RuleError::file(origin, RuleErrorKind::NoRules));
    }

    let mut seen = BTreeSet::new();
    let mut rules = Vec::with_capacity(raw.rule.len());
    for r in raw.rule {
        let rule = validate(r, origin, source)?;
        if !seen.insert(rule.id.clone()) {
            return Err(RuleError::rule(
                origin,
                &rule.id,
                RuleErrorKind::DuplicateId,
            ));
        }
        rules.push(rule);
    }
    Ok(rules)
}

fn is_command_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    COMMAND_KEY_FRAGMENTS.iter().any(|f| lower.contains(f))
        || key_tokens(key)
            .iter()
            .any(|t| COMMAND_KEY_TOKENS.contains(&t.as_str()))
}

/// Split a key into lowercase tokens at `_`, `-`, `.`, spaces and camelCase boundaries
/// (`preScript` → `pre`, `script`).
fn key_tokens(key: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in key.chars() {
        if matches!(c, '_' | '-' | '.' | ' ') {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Depth-first search for a command-like key; returns its dotted path.
fn find_command_key(table: &toml::Table, prefix: &str) -> Option<String> {
    for (k, v) in table {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        if is_command_key(k) {
            return Some(path);
        }
        if let Some(found) = find_in_value(v, &path) {
            return Some(found);
        }
    }
    None
}

fn find_in_value(v: &toml::Value, path: &str) -> Option<String> {
    match v {
        toml::Value::Table(t) => find_command_key(t, path),
        toml::Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, item)| find_in_value(item, &format!("{path}[{i}]"))),
        _ => None,
    }
}

fn validate(r: RawRule, origin: &str, source: RuleSource) -> Result<Rule, RuleError> {
    let id = r.id.clone();
    let fail = |kind: RuleErrorKind| RuleError::rule(origin, &id, kind);

    validate_id(&r.id).map_err(|m| fail(RuleErrorKind::InvalidId(m)))?;
    if r.version == 0 {
        return Err(fail(RuleErrorKind::InvalidVersion));
    }
    non_empty_text("artifact", &r.artifact).map_err(&fail)?;
    non_empty_text("ecosystem", &r.ecosystem).map_err(&fail)?;
    if !r
        .ecosystem
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(fail(RuleErrorKind::InvalidField {
            field: "ecosystem",
            reason: "use lowercase ASCII letters, digits, `-` or `_`".into(),
        }));
    }
    if matches!(r.category, Category::ScullData) {
        return Err(fail(RuleErrorKind::ForbiddenCategory("scull_data".into())));
    }

    // Platforms: non-empty, unique.
    if r.platforms.is_empty() {
        return Err(fail(RuleErrorKind::InvalidField {
            field: "platforms",
            reason: "list at least one platform".into(),
        }));
    }
    let mut platforms = r.platforms.clone();
    platforms.sort();
    platforms.dedup();
    if platforms.len() != r.platforms.len() {
        return Err(fail(RuleErrorKind::InvalidField {
            field: "platforms",
            reason: "duplicate platform".into(),
        }));
    }

    // Trust caps for non-bundled rules (CLS-04).
    if source == RuleSource::User {
        if r.base_tier == Tier::S0 {
            return Err(fail(RuleErrorKind::S0NotAllowed));
        }
        if r.strategy == Strategy::ToolNative {
            return Err(fail(RuleErrorKind::ToolNativeNotAllowed));
        }
    }

    // Candidate names.
    if r.matching.names.is_empty() {
        return Err(fail(RuleErrorKind::InvalidField {
            field: "match.names",
            reason: "list at least one directory name pattern".into(),
        }));
    }
    let names = parse_patterns(&r.matching.names).map_err(&fail)?;
    unique("match.names", &r.matching.names).map_err(&fail)?;

    // Markers (CLS-03).
    let markers_all = parse_entries(&r.markers.all).map_err(&fail)?;
    let markers_any = parse_entries(&r.markers.any).map_err(&fail)?;
    unique("markers.all", &r.markers.all).map_err(&fail)?;
    unique("markers.any", &r.markers.any).map_err(&fail)?;
    if markers_all.is_empty() && markers_any.is_empty() {
        return Err(fail(RuleErrorKind::NameOnly));
    }
    if markers_any.len() == 1 {
        return Err(fail(RuleErrorKind::InvalidField {
            field: "markers.any",
            reason: "a single alternative belongs in `markers.all`".into(),
        }));
    }
    for m in markers_all.iter().chain(&markers_any) {
        check_marker_proves_something(m, &names).map_err(&fail)?;
    }

    let lockfiles = parse_entries(&r.lockfiles.any).map_err(&fail)?;
    unique("lockfiles.any", &r.lockfiles.any).map_err(&fail)?;
    let exclude_markers = parse_entries(&r.exclude.markers).map_err(&fail)?;
    unique("exclude.markers", &r.exclude.markers).map_err(&fail)?;
    let exclude_within = parse_patterns(&r.exclude.within).map_err(&fail)?;
    unique("exclude.within", &r.exclude.within).map_err(&fail)?;

    // Evidence must match what the rule can produce.
    let mut evidence = r.evidence.clone();
    evidence.sort();
    evidence.dedup();
    if evidence.len() != r.evidence.len() {
        return Err(fail(RuleErrorKind::InvalidField {
            field: "evidence",
            reason: "duplicate evidence kind".into(),
        }));
    }
    if !evidence.contains(&EvidenceKind::Marker) {
        return Err(fail(RuleErrorKind::EvidenceMismatch(
            "every rule produces `marker` evidence (CLS-03)".into(),
        )));
    }
    match (
        evidence.contains(&EvidenceKind::Lockfile),
        lockfiles.is_empty(),
    ) {
        (true, true) => {
            return Err(fail(RuleErrorKind::EvidenceMismatch(
                "`lockfile` evidence needs `lockfiles.any`".into(),
            )));
        }
        (false, false) => {
            return Err(fail(RuleErrorKind::EvidenceMismatch(
                "`lockfiles.any` is declared, so list `lockfile` evidence".into(),
            )));
        }
        _ => {}
    }

    // Activity checks.
    for p in &r.activity.processes {
        validate_process_name(p).map_err(&fail)?;
    }
    unique("activity.processes", &r.activity.processes).map_err(&fail)?;
    for l in &r.activity.lock_files {
        validate_inner_path(l).map_err(&fail)?;
    }
    unique("activity.lock_files", &r.activity.lock_files).map_err(&fail)?;

    // Regeneration guidance (display text only).
    if let Some(text) = &r.regeneration.instructions {
        non_empty_text("regeneration.instructions", text).map_err(&fail)?;
    }

    Ok(Rule {
        id: r.id,
        version: r.version,
        artifact: r.artifact,
        ecosystem: r.ecosystem,
        category: r.category,
        platforms,
        base_tier: r.base_tier,
        strategy: r.strategy,
        evidence: r.evidence,
        names,
        markers_all,
        markers_any,
        lockfiles,
        exclude_markers,
        exclude_within,
        activity: ActivityChecks {
            processes: r.activity.processes,
            lock_files: r.activity.lock_files,
        },
        regeneration: RegenerationGuidance {
            instructions: r.regeneration.instructions,
            network: r.regeneration.network,
            long_build: r.regeneration.long_build,
            quick: r.regeneration.quick,
        },
        source,
        origin: origin.to_string(),
    })
}

/// `ecosystem.name` segments: lowercase ASCII letters, digits, `-`, `_`; at least two segments.
fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        return Err("must be 1–64 characters".into());
    }
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 2 {
        return Err("must be namespaced, e.g. `rust.target`".into());
    }
    for s in segments {
        let ok = !s.is_empty()
            && s.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !ok {
            return Err(
                "each segment starts with a lowercase letter and uses only a-z, 0-9, `-`, `_`"
                    .into(),
            );
        }
    }
    Ok(())
}

fn non_empty_text(field: &'static str, s: &str) -> Result<(), RuleErrorKind> {
    if s.trim().is_empty() {
        return Err(RuleErrorKind::InvalidField {
            field,
            reason: "must not be empty".into(),
        });
    }
    if s.chars().any(|c| c.is_control() && c != '\n') {
        return Err(RuleErrorKind::InvalidField {
            field,
            reason: "contains a control character".into(),
        });
    }
    Ok(())
}

fn unique(field: &'static str, items: &[String]) -> Result<(), RuleErrorKind> {
    let set: BTreeSet<&String> = items.iter().collect();
    if set.len() != items.len() {
        return Err(RuleErrorKind::InvalidField {
            field,
            reason: "duplicate entry".into(),
        });
    }
    Ok(())
}

fn parse_patterns(raw: &[String]) -> Result<Vec<NamePattern>, RuleErrorKind> {
    raw.iter()
        .map(|p| {
            NamePattern::parse(p).map_err(|e| RuleErrorKind::InvalidPattern {
                pattern: p.clone(),
                reason: e.0,
            })
        })
        .collect()
}

fn parse_entries(raw: &[String]) -> Result<Vec<RelativeEntry>, RuleErrorKind> {
    raw.iter().map(|s| parse_entry(s)).collect()
}

/// Parse `../../name`, `../name` or `name`.
pub(crate) fn parse_entry(raw: &str) -> Result<RelativeEntry, RuleErrorKind> {
    let bad = |reason: &str| RuleErrorKind::InvalidPath {
        path: raw.to_string(),
        reason: reason.to_string(),
    };
    check_path_chars(raw).map_err(|r| bad(&r))?;
    let segments: Vec<&str> = raw.split('/').collect();
    let (name, ups) = segments.split_last().ok_or_else(|| bad("empty path"))?;
    if ups.iter().any(|s| *s != "..") {
        return Err(bad(
            "only `..` may precede the name (e.g. `../Cargo.toml`, `CMakeCache.txt`)",
        ));
    }
    if ups.len() > MAX_UP {
        return Err(bad("goes up more than 4 levels"));
    }
    let pattern = NamePattern::parse(name).map_err(|e| bad(&e.0))?;
    Ok(RelativeEntry::new(raw.to_string(), ups.len(), pattern))
}

fn check_path_chars(raw: &str) -> Result<(), String> {
    if raw.is_empty() {
        return Err("empty path".into());
    }
    if raw.starts_with('/') || raw.starts_with('~') {
        return Err("must be relative to the candidate".into());
    }
    if raw.contains('\\') {
        return Err("use `/` as the separator".into());
    }
    if raw.contains(':') {
        return Err("`:` is not allowed (drive letters, streams)".into());
    }
    if raw.chars().any(char::is_control) {
        return Err("contains a control character".into());
    }
    if raw.split('/').any(str::is_empty) {
        return Err("empty path segment".into());
    }
    Ok(())
}

/// A path strictly inside the candidate: literal names, no `..`, no wildcards.
fn validate_inner_path(raw: &str) -> Result<(), RuleErrorKind> {
    let bad = |reason: &str| RuleErrorKind::InvalidPath {
        path: raw.to_string(),
        reason: reason.to_string(),
    };
    check_path_chars(raw).map_err(|r| bad(&r))?;
    let segments: Vec<&str> = raw.split('/').collect();
    if segments.len() > 4 {
        return Err(bad("more than 4 segments"));
    }
    for s in segments {
        if s == ".." || s == "." {
            return Err(bad("must stay inside the candidate"));
        }
        if s.contains(['*', '?', '{', '}', '[', ']']) {
            return Err(bad("lock files are literal paths"));
        }
    }
    Ok(())
}

fn validate_process_name(p: &str) -> Result<(), RuleErrorKind> {
    let ok = !p.is_empty()
        && p.len() <= 64
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(RuleErrorKind::InvalidField {
            field: "activity.processes",
            reason: format!("`{p}` is not a plain process name (letters, digits, `-_.`)"),
        })
    }
}

/// CLS-03: a marker must say something the candidate's own name doesn't.
fn check_marker_proves_something(
    m: &RelativeEntry,
    names: &[NamePattern],
) -> Result<(), RuleErrorKind> {
    if m.name().is_wildcard_only() {
        return Err(RuleErrorKind::WildcardMarker {
            marker: m.as_str().to_string(),
        });
    }
    // Only markers in the candidate's parent can be the candidate itself.
    if m.up() != 1 {
        return Ok(());
    }
    let self_satisfied = names.iter().any(|n| {
        match (n.literals(), m.name().literals()) {
            (Some(ns), _) => ns.iter().any(|l| m.name().matches_ignore_case(l)),
            (None, Some(ms)) => ms.iter().any(|l| n.matches_ignore_case(l)),
            // Both have wildcards: we can't prove they're disjoint, so refuse.
            (None, None) => true,
        }
    });
    if self_satisfied {
        return Err(RuleErrorKind::MarkerMatchesCandidate {
            marker: m.as_str().to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Rule set.

/// All loaded rules plus which ones are enabled.
///
/// Bundled rules are enabled on load; user rules are loaded **disabled** and only run after
/// [`RuleSet::enable`] (CLS-04). Enabling is a user-config decision; a rule file can't enable
/// itself.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
    enabled: BTreeSet<String>,
}

impl RuleSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bundled rules compiled into this binary, all enabled.
    pub fn bundled() -> Result<Self, RuleError> {
        let mut set = Self::new();
        for (origin, text) in BUNDLED_RULE_FILES {
            set.add_file(text, origin, RuleSource::Bundled)?;
        }
        Ok(set)
    }

    /// Load a rule file into the set. All-or-nothing: on error nothing is added. Bundled rules
    /// are enabled; user rules are added disabled. Returns the IDs added.
    pub fn add_file(
        &mut self,
        text: &str,
        origin: &str,
        source: RuleSource,
    ) -> Result<Vec<String>, RuleError> {
        let rules = parse_rules(text, origin, source)?;
        if let Some(dup) = rules.iter().find(|r| self.get(&r.id).is_some()) {
            return Err(RuleError::rule(origin, &dup.id, RuleErrorKind::DuplicateId));
        }
        let ids: Vec<String> = rules.iter().map(|r| r.id.clone()).collect();
        for r in rules {
            if r.source == RuleSource::Bundled {
                self.enabled.insert(r.id.clone());
            }
            self.rules.push(r);
        }
        Ok(ids)
    }

    /// Load a user or community rule file (disabled until enabled).
    pub fn add_user_file(&mut self, text: &str, origin: &str) -> Result<Vec<String>, RuleError> {
        self.add_file(text, origin, RuleSource::User)
    }

    /// Enable a loaded rule.
    pub fn enable(&mut self, id: &str) -> Result<(), RuleError> {
        if self.get(id).is_none() {
            return Err(RuleError::rule("rule set", id, RuleErrorKind::UnknownRule));
        }
        self.enabled.insert(id.to_string());
        Ok(())
    }

    /// Disable a rule. Returns whether it was enabled.
    pub fn disable(&mut self, id: &str) -> bool {
        self.enabled.remove(id)
    }

    /// Whether a rule is loaded and enabled.
    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled.contains(id)
    }

    /// A loaded rule by ID.
    pub fn get(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// All loaded rules, enabled or not, in load order.
    pub fn all(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter()
    }

    /// Enabled rules in load order.
    pub fn enabled(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().filter(|r| self.enabled.contains(&r.id))
    }

    /// Enabled rules that apply on `platform`.
    pub fn enabled_for(&self, platform: Platform) -> impl Iterator<Item = &Rule> {
        self.enabled().filter(move |r| r.applies_to(platform))
    }

    /// Number of loaded rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether no rules are loaded.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}
