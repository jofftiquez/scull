//! `scull rules list | validate` (P1-CLI-06; PRD §12, CLS-01…05).
//!
//! * `scull rules list`: every loaded rule (bundled, plus user rule files) with ID, version,
//!   ecosystem, base tier, strategy, platforms, enabled state and source. `scull/rules-list@1`.
//! * `scull rules validate [FILE...]`: runs the strict loader (`scull_detectors::rules`) on each
//!   file as an untrusted **user** rule file, alongside the bundled rules (so a user rule can't
//!   shadow a bundled ID), and prints each file's result with the rejection reason. Without
//!   files it validates the bundled rules. Exit 0 when every file is valid, 2 (usage/config)
//!   otherwise. `scull/rules-validate@1`; with `--json` the complete report is written to stdout
//!   and the error envelope to stderr. Nothing is ever enabled.
//!
//! **User rule files** are the `*.toml` files directly in `<config dir>/rules/` (read in name
//! order). They load **disabled**; a rule is enabled only by listing its ID in the user config
//! (`[rules] enabled = [...]`) or `SCULL_RULES` (CLS-04, CFG-01). The loader is all-or-nothing
//! per file and stops at the first problem, so each invalid file reports one rejection.

use super::{doctor, emit_machine, emit_text, json_name, one_line};
use crate::output::Ui;
use crate::output::json::SchemaId;
use crate::output::style::{Style, tier_style, tier_text};
use crate::output::table::{Align, Cell, Table};
use schemars::JsonSchema;
use scull_core::{Category, ErrorKind, FsPath, ScullError, Tier};
use scull_detectors::rules::{
    BUNDLED_RULE_FILES, Rule, RuleError, RuleErrorKind, RuleSet, RuleSource,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Schema of `scull rules list --json`.
pub const RULES_LIST_SCHEMA: SchemaId = SchemaId::new("rules-list", 1);

/// Schema of `scull rules validate --json`.
pub const RULES_VALIDATE_SCHEMA: SchemaId = SchemaId::new("rules-validate", 1);

/// Directory of user rule files inside the config directory.
pub const USER_RULES_DIR: &str = "rules";

/// Where a rule came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Compiled into this binary.
    Bundled,
    /// A user or community rule file (untrusted: capped at S1, disabled until enabled).
    User,
}

impl From<RuleSource> for Source {
    fn from(s: RuleSource) -> Self {
        match s {
            RuleSource::Bundled => Source::Bundled,
            RuleSource::User => Source::User,
        }
    }
}

/// One loaded rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuleInfo {
    /// Stable namespaced ID, e.g. `rust.target`.
    pub id: String,
    /// Rule version.
    pub version: u32,
    /// Ecosystem, e.g. `rust`.
    pub ecosystem: String,
    /// Human-readable artifact type.
    pub artifact: String,
    /// Candidate category.
    pub category: Category,
    /// Tier when every check passes; the classifier only raises it.
    pub base_tier: Tier,
    /// `filesystem`, `tool_native` or `inspect_only`.
    pub strategy: String,
    /// Operating systems it applies to (`macos`, `linux`, `windows`).
    pub platforms: Vec<String>,
    /// Whether it applies to the OS this binary runs on.
    pub applies_here: bool,
    /// Whether it runs (bundled: always; user: only when enabled in config).
    pub enabled: bool,
    /// Where it came from.
    pub source: Source,
    /// The file it was loaded from.
    pub origin: String,
}

/// `scull rules list` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RulesList {
    /// Loaded rules, bundled first, in load order.
    pub rules: Vec<RuleInfo>,
    /// Directory searched for user rule files, if scull's directories could be located.
    pub user_rules_dir: Option<FsPath>,
    /// Problems found (invalid user rule files, enabled IDs that aren't loaded, config errors).
    pub problems: Vec<String>,
}

/// Why a rule file was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rejection {
    /// Stable machine code, e.g. `name_only`, `s0_not_allowed`, `command_field`.
    pub code: String,
    /// The rule concerned, if the problem is about one rule.
    pub rule_id: Option<String>,
    /// Human explanation.
    pub message: String,
}

/// The result for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileResult {
    /// The file (a bundled origin such as `rules/ecosystems/rust.toml`, or the path given).
    pub path: String,
    /// Trust level it was validated with.
    pub source: Source,
    /// Whether it loads.
    pub valid: bool,
    /// IDs of the rules it declares (empty when invalid).
    pub rules: Vec<String>,
    /// Why it was rejected.
    pub rejection: Option<Rejection>,
}

/// `scull rules validate` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RulesValidation {
    /// Whether every file is valid.
    pub valid: bool,
    /// One entry per file, in the order given.
    pub files: Vec<FileResult>,
}

/// `scull rules list`.
pub fn run_list(ui: &Ui) -> Result<(), ScullError> {
    let config_dir = scull_platform::app_dirs().ok().map(|d| d.config);
    let list = list(config_dir.as_deref(), |k| std::env::var(k).ok())?;
    if ui.format.is_machine() {
        emit_machine(ui.format, RULES_LIST_SCHEMA, "rules", &list)
    } else {
        emit_text(&render_list(&list, ui.color_out))
    }
}

/// `scull rules validate [FILE...]`.
pub fn run_validate(ui: &Ui, paths: &[PathBuf]) -> Result<(), ScullError> {
    let report = validate(paths)?;
    if ui.format.is_machine() {
        emit_machine(ui.format, RULES_VALIDATE_SCHEMA, "validation", &report)?;
    } else {
        emit_text(&render_validation(&report, ui.color_out))?;
    }
    if report.valid {
        return Ok(());
    }
    let bad = report.files.iter().filter(|f| !f.valid).count();
    Err(ScullError::new(
        ErrorKind::Config,
        format!(
            "{bad} of {} rule file{} {} invalid",
            report.files.len(),
            if report.files.len() == 1 { "" } else { "s" },
            if bad == 1 { "is" } else { "are" }
        ),
    ))
}

fn bundled() -> Result<RuleSet, ScullError> {
    RuleSet::bundled().map_err(|e| {
        ScullError::new(
            ErrorKind::Internal,
            format!("bundled rules are invalid: {e}"),
        )
    })
}

/// Build the rule list: bundled rules, user rule files from `<config_dir>/rules/`, and the
/// enabled IDs from the user config and environment (read through `env`).
pub fn list(
    config_dir: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<RulesList, ScullError> {
    let mut set = bundled()?;
    let mut problems = Vec::new();
    let user_dir = config_dir.map(|d| d.join(USER_RULES_DIR));
    if let Some(dir) = &user_dir {
        for path in user_rule_files(dir, &mut problems) {
            let origin = path.display().to_string();
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    if let Err(e) = set.add_user_file(&text, &origin) {
                        problems.push(format!("not loaded: {}", one_line(&e.to_string())));
                    }
                }
                Err(e) => problems.push(format!("cannot read {origin}: {e}")),
            }
        }
    }

    let config = doctor::config_report(config_dir, env);
    if let Some(e) = config.error.as_deref().or(config.env_error.as_deref()) {
        problems.push(format!(
            "user rules stay disabled: the config can't be read: {}",
            one_line(e)
        ));
    } else {
        let enabled = config
            .effective
            .iter()
            .find(|v| v.field == "rules.enabled")
            .and_then(|v| v.value.as_array().cloned())
            .unwrap_or_default();
        for id in enabled.iter().filter_map(serde_json::Value::as_str) {
            if set.enable(id).is_err() {
                problems.push(format!("enabled rule `{id}` is not loaded"));
            }
        }
    }

    let here = scull_detectors::rules::Platform::current();
    let rules = set
        .all()
        .map(|r| info(r, set.is_enabled(r.id()), here))
        .collect();
    Ok(RulesList {
        rules,
        user_rules_dir: user_dir.as_deref().map(FsPath::from_path),
        problems,
    })
}

/// `*.toml` files directly in `dir`, by name. A missing directory is not a problem.
fn user_rule_files(dir: &Path, problems: &mut Vec<String>) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            problems.push(format!("cannot list {}: {e}", dir.display()));
            return Vec::new();
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    files
}

fn info(r: &Rule, enabled: bool, here: Option<scull_detectors::rules::Platform>) -> RuleInfo {
    RuleInfo {
        id: r.id().to_owned(),
        version: r.version(),
        ecosystem: r.ecosystem().to_owned(),
        artifact: r.artifact().to_owned(),
        category: r.category(),
        base_tier: r.base_tier(),
        strategy: json_name(&r.strategy()),
        platforms: r.platforms().iter().map(json_name).collect(),
        applies_here: here.is_some_and(|p| r.applies_to(p)),
        enabled,
        source: r.source().into(),
        origin: r.origin().to_owned(),
    }
}

/// Validate `paths` as user rule files next to the bundled rules, or the bundled rule files
/// themselves when `paths` is empty.
pub fn validate(paths: &[PathBuf]) -> Result<RulesValidation, ScullError> {
    let mut files = Vec::new();
    if paths.is_empty() {
        let mut set = RuleSet::new();
        for (origin, text) in BUNDLED_RULE_FILES {
            let r = set.add_file(text, origin, RuleSource::Bundled);
            files.push(result(origin, Source::Bundled, r));
        }
    } else {
        let mut set = bundled()?;
        for path in paths {
            let origin = path.display().to_string();
            let r = match std::fs::read_to_string(path) {
                Ok(text) => set.add_user_file(&text, &origin),
                Err(e) => {
                    files.push(FileResult {
                        path: origin,
                        source: Source::User,
                        valid: false,
                        rules: Vec::new(),
                        rejection: Some(Rejection {
                            code: "unreadable".to_owned(),
                            rule_id: None,
                            message: format!("cannot read the file: {e}"),
                        }),
                    });
                    continue;
                }
            };
            files.push(result(&origin, Source::User, r));
        }
    }
    Ok(RulesValidation {
        valid: files.iter().all(|f| f.valid),
        files,
    })
}

fn result(origin: &str, source: Source, r: Result<Vec<String>, RuleError>) -> FileResult {
    match r {
        Ok(rules) => FileResult {
            path: origin.to_owned(),
            source,
            valid: true,
            rules,
            rejection: None,
        },
        Err(e) => FileResult {
            path: origin.to_owned(),
            source,
            valid: false,
            rules: Vec::new(),
            rejection: Some(Rejection {
                code: code(&e.kind).to_owned(),
                rule_id: e.rule_id.clone(),
                message: e.kind.to_string(),
            }),
        },
    }
}

/// Stable machine code of a loader rejection.
pub fn code(kind: &RuleErrorKind) -> &'static str {
    match kind {
        RuleErrorKind::Syntax(_) => "syntax",
        RuleErrorKind::MissingSchemaVersion => "missing_schema_version",
        RuleErrorKind::UnsupportedSchemaVersion { .. } => "unsupported_schema_version",
        RuleErrorKind::CommandField { .. } => "command_field",
        RuleErrorKind::Schema(_) => "schema",
        RuleErrorKind::NoRules => "no_rules",
        RuleErrorKind::NameOnly => "name_only",
        RuleErrorKind::MarkerMatchesCandidate { .. } => "marker_matches_candidate",
        RuleErrorKind::WildcardMarker { .. } => "wildcard_marker",
        RuleErrorKind::InvalidId(_) => "invalid_id",
        RuleErrorKind::InvalidVersion => "invalid_version",
        RuleErrorKind::InvalidPattern { .. } => "invalid_pattern",
        RuleErrorKind::InvalidPath { .. } => "invalid_path",
        RuleErrorKind::InvalidField { .. } => "invalid_field",
        RuleErrorKind::EvidenceMismatch(_) => "evidence_mismatch",
        RuleErrorKind::S0NotAllowed => "s0_not_allowed",
        RuleErrorKind::ToolNativeNotAllowed => "tool_native_not_allowed",
        RuleErrorKind::ForbiddenCategory(_) => "forbidden_category",
        RuleErrorKind::DuplicateId => "duplicate_id",
        RuleErrorKind::UnknownRule => "unknown_rule",
        RuleErrorKind::UserWorkFolder { .. } => "user_work_folder",
        RuleErrorKind::WildcardName { .. } => "wildcard_name",
        RuleErrorKind::NoProjectMarker => "no_project_marker",
        _ => "other",
    }
}

/// The human rule table.
pub fn render_list(l: &RulesList, color: bool) -> String {
    let mut t = Table::new([
        ("ID", Align::Left),
        ("Ver", Align::Right),
        ("Ecosystem", Align::Left),
        ("Base tier", Align::Left),
        ("Strategy", Align::Left),
        ("Platforms", Align::Left),
        ("Enabled", Align::Left),
        ("Source", Align::Left),
    ]);
    for r in &l.rules {
        t.push(vec![
            Cell::text(&r.id),
            Cell::text(&r.version.to_string()),
            Cell::text(&r.ecosystem),
            Cell::text(&tier_text(r.base_tier)).styled(tier_style(r.base_tier)),
            Cell::text(&r.strategy),
            Cell::text(&r.platforms.join(", ")),
            if r.enabled {
                Cell::text("yes")
            } else {
                Cell::text("no").styled(Style::Dim)
            },
            Cell::text(&json_name(&r.source)),
        ]);
    }
    let mut out = t.render(color);
    let user = l.rules.iter().filter(|r| r.source == Source::User).count();
    out.push_str(&format!(
        "\n{} rules ({} bundled, {user} user)",
        l.rules.len(),
        l.rules.len() - user
    ));
    if let Some(dir) = &l.user_rules_dir {
        out.push_str("; user rule files: ");
        out.push_str(Cell::path(dir.as_str()).as_str());
    }
    out.push('\n');
    for p in &l.problems {
        out.push_str(&Style::Warn.paint(&format!("warning: {}", Cell::text(p).as_str()), color));
        out.push('\n');
    }
    out
}

/// The human validation report: one line per file, the rejection under invalid ones.
pub fn render_validation(v: &RulesValidation, color: bool) -> String {
    let mut out = String::new();
    for f in &v.files {
        let path = Cell::path(&f.path);
        if f.valid {
            out.push_str(&format!(
                "{} {} ({} rule{}: {})\n",
                Style::Bold.paint("ok     ", color),
                path.as_str(),
                f.rules.len(),
                if f.rules.len() == 1 { "" } else { "s" },
                Cell::text(&f.rules.join(", ")).as_str()
            ));
        } else {
            out.push_str(&format!(
                "{} {}\n",
                Style::Error.paint("invalid", color),
                path.as_str()
            ));
            if let Some(r) = &f.rejection {
                let rule = r
                    .rule_id
                    .as_deref()
                    .map(|id| format!("rule `{id}`: "))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "        [{}] {}\n",
                    r.code,
                    Cell::text(&one_line(&format!("{rule}{}", r.message))).as_str()
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_rules_list_and_validate() {
        let set = RuleSet::bundled().expect("bundled");
        let l = list(None, |_| None).expect("list");
        assert_eq!(l.rules.len(), set.len());
        assert!(
            l.rules
                .iter()
                .all(|r| r.enabled && r.source == Source::Bundled)
        );
        assert!(l.problems.is_empty(), "{:?}", l.problems);
        let v = validate(&[]).expect("validate");
        assert!(v.valid);
        assert_eq!(v.files.len(), BUNDLED_RULE_FILES.len());
        let ids: usize = v.files.iter().map(|f| f.rules.len()).sum();
        assert_eq!(ids, set.len());
    }

    #[test]
    fn render_marks_invalid_files_and_escapes() {
        let v = RulesValidation {
            valid: false,
            files: vec![FileResult {
                path: "/r/evil\x1b[2J.toml".into(),
                source: Source::User,
                valid: false,
                rules: Vec::new(),
                rejection: Some(Rejection {
                    code: "name_only".into(),
                    rule_id: Some("x.y".into()),
                    message: "no marker".into(),
                }),
            }],
        };
        let out = render_validation(&v, false);
        assert!(!out.contains('\x1b'));
        assert!(out.contains("invalid /r/evil\\x1b[2J.toml"));
        assert!(out.contains("[name_only] rule `x.y`: no marker"));
    }
}
