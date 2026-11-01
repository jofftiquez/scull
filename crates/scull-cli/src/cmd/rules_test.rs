//! `scull rules test FILE... [--init]` (P6-COM-01): the test harness for rule authors.
//!
//! A rule file `foo.toml` ships with a cases file `foo.cases.toml` beside it: small trees of
//! file and folder names and, for each, the folder a rule should (or must not) claim. The
//! harness loads `foo.toml` exactly as a user rule (the same strict loader and community
//! guardrails as `scull rules validate`), evaluates every case with the rule's own marker
//! logic (`Rule::check_markers`) on the tree **described in the file**, and reports each
//! case. **Nothing on disk is read or written** besides the two files (and, with `--init`, a
//! new cases file that is never overwritten): trees are names, not files.
//!
//! Every rule must have at least one case that expects a match and one that expects no
//! match (CLS-04: rules ship with fixtures). Exit 0 when every file loads and every case
//! passes; exit 2 otherwise. Machine output: `scull/rules-test@1`.
//!
//! Cases file format (schema version 1):
//!
//! ```toml
//! schema_version = 1
//!
//! [[case]]
//! name = "import cache next to project.godot"
//! rule = "community.godot-import"
//! tree = ["game/project.godot", "game/.godot/imported/a.ctex"]
//! candidate = "game/.godot"
//! expect = "match"     # match | lockfile_missing | no_match | excluded | missing_marker
//! ```
//!
//! `tree` lists paths relative to an imaginary scan root, `/`-separated; a trailing `/` marks
//! an empty folder, and every parent of a listed path is a folder. `candidate` is the folder
//! the rule is asked about (it is part of the tree). Levels above the scan root are unknown,
//! as they are to the scanner, so a case needing them reports `incomplete`.

use super::rules::{Rejection, code};
use super::{emit_machine, emit_text};
use crate::output::Ui;
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use crate::output::style::Style;
use schemars::JsonSchema;
use scull_core::{ErrorKind, ScullError};
use scull_detectors::rules::{
    BUNDLED_RULE_FILES, MarkerOutcome, Rule, RuleSet, RuleSource, Surroundings,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Schema of `scull rules test --json`.
pub const RULES_TEST_SCHEMA: SchemaId = SchemaId::new("rules-test", 1);

/// The cases file schema version this build understands.
pub const CASES_SCHEMA_VERSION: u32 = 1;

/// Most cases one file may hold, and most paths one tree may list.
pub const MAX_CASES: usize = 500;
/// See [`MAX_CASES`].
pub const MAX_TREE_PATHS: usize = 2_000;

/// What a case expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Expect {
    /// The rule claims the candidate (with its lockfile, if it declares one).
    Match,
    /// The rule claims it, but the declared lockfile is missing (reported at least S2).
    LockfileMissing,
    /// The rule does not claim it, for any reason.
    NoMatch,
    /// An exclusion applies.
    Excluded,
    /// A required marker is absent.
    MissingMarker,
}

impl Expect {
    fn positive(self) -> bool {
        matches!(self, Expect::Match | Expect::LockfileMissing)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CasesFile {
    schema_version: u32,
    #[serde(default, rename = "case")]
    cases: Vec<RawCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCase {
    name: String,
    rule: String,
    tree: Vec<String>,
    candidate: String,
    expect: Expect,
}

/// One evaluated case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaseResult {
    /// The case's name.
    pub name: String,
    /// The rule it exercises.
    pub rule: String,
    /// The candidate folder (relative to the case's tree).
    pub candidate: String,
    /// What it expects.
    pub expect: Expect,
    /// What the rule decided: `match`, `lockfile_missing`, `name_mismatch`, `excluded`,
    /// `missing_marker`, `incomplete`, or `invalid_case`.
    pub outcome: String,
    /// Whether the outcome is what the case expects.
    pub passed: bool,
    /// The marker, exclusion or problem behind the outcome.
    pub detail: Option<String>,
}

/// The results for one rule file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TestFile {
    /// The rule file.
    pub path: String,
    /// Its cases file.
    pub cases_path: String,
    /// The rules it declares (empty when it doesn't load).
    pub rules: Vec<String>,
    /// Why the rule file (or its cases file) was rejected.
    pub rejection: Option<Rejection>,
    /// Every case, in file order.
    pub cases: Vec<CaseResult>,
    /// Rules without a case expecting a match, or without one expecting no match.
    pub missing: Vec<String>,
    /// Whether the file loads, every case passes and nothing is missing.
    pub passed: bool,
}

/// `scull rules test` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RulesTest {
    /// Whether every file passed.
    pub passed: bool,
    /// One entry per rule file, in the order given.
    pub files: Vec<TestFile>,
}

/// The cases file of rule file `path`: `foo.toml` → `foo.cases.toml`.
pub fn cases_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{stem}.cases.toml"))
}

/// `scull rules test FILE... [--init]`.
pub fn run(ui: &Ui, paths: &[PathBuf], init: bool) -> Result<(), ScullError> {
    if paths.is_empty() {
        return Err(ScullError::new(
            ErrorKind::Usage,
            "give the rule file(s) to test, e.g. `scull rules test my-rules.toml`",
        ));
    }
    if init {
        let mut out = String::new();
        for p in paths {
            out.push_str(&scaffold(p)?);
        }
        return emit_text(&out);
    }
    let report = test(paths);
    if ui.format.is_machine() {
        emit_machine(ui.format, RULES_TEST_SCHEMA, "rules_test", &report)?;
    } else {
        emit_text(&render(&report, ui.color_out))?;
    }
    if report.passed {
        return Ok(());
    }
    let failed = report.files.iter().filter(|f| !f.passed).count();
    Err(ScullError::new(
        ErrorKind::Config,
        format!(
            "{failed} of {} rule file{} failed its tests",
            report.files.len(),
            if report.files.len() == 1 { "" } else { "s" }
        ),
    ))
}

/// Test every rule file in `paths` against its cases file.
pub fn test(paths: &[PathBuf]) -> RulesTest {
    let files: Vec<TestFile> = paths.iter().map(|p| test_file(p)).collect();
    RulesTest {
        passed: files.iter().all(|f| f.passed),
        files,
    }
}

fn reject(code: &str, rule_id: Option<String>, message: String) -> Option<Rejection> {
    Some(Rejection {
        code: code.to_owned(),
        rule_id,
        message,
    })
}

fn test_file(path: &Path) -> TestFile {
    let cases_file = cases_path(path);
    let mut out = TestFile {
        path: path.display().to_string(),
        cases_path: cases_file.display().to_string(),
        rules: Vec::new(),
        rejection: None,
        cases: Vec::new(),
        missing: Vec::new(),
        passed: false,
    };
    // The rule file, loaded exactly as `scull rules validate` loads a user rule file.
    let mut set = RuleSet::new();
    for (origin, text) in BUNDLED_RULE_FILES {
        if let Err(e) = set.add_file(text, origin, RuleSource::Bundled) {
            out.rejection = reject("bundled", None, e.to_string());
            return out;
        }
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            out.rejection = reject("unreadable", None, format!("cannot read the file: {e}"));
            return out;
        }
    };
    let origin = path.display().to_string();
    let ids = match set.add_user_file(&text, &origin) {
        Ok(ids) => ids,
        Err(e) => {
            out.rejection = reject(code(&e.kind), e.rule_id.clone(), e.kind.to_string());
            return out;
        }
    };
    out.rules.clone_from(&ids);
    let rules: BTreeMap<&str, &Rule> = ids
        .iter()
        .filter_map(|i| set.get(i).map(|r| (i.as_str(), r)))
        .collect();

    // The cases file.
    let raw = match std::fs::read_to_string(&cases_file) {
        Ok(t) => t,
        Err(e) => {
            out.rejection = reject(
                "no_cases",
                None,
                format!(
                    "cannot read {} ({e}): every rule ships with test cases; create one with \
                     `scull rules test --init {}`",
                    cases_file.display(),
                    path.display()
                ),
            );
            return out;
        }
    };
    let file: CasesFile = match toml::from_str(&raw) {
        Ok(f) => f,
        Err(e) => {
            out.rejection = reject("cases_syntax", None, format!("invalid cases file: {e}"));
            return out;
        }
    };
    if file.schema_version != CASES_SCHEMA_VERSION {
        out.rejection = reject(
            "cases_schema_version",
            None,
            format!(
                "cases schema_version {} is not supported (this build reads {CASES_SCHEMA_VERSION})",
                file.schema_version
            ),
        );
        return out;
    }
    if file.cases.len() > MAX_CASES {
        out.rejection = reject(
            "too_many_cases",
            None,
            format!("at most {MAX_CASES} cases per file"),
        );
        return out;
    }
    for c in &file.cases {
        out.cases
            .push(evaluate(c, rules.get(c.rule.as_str()).copied()));
    }
    for id in &ids {
        let of = |pos: bool| {
            file.cases
                .iter()
                .any(|c| c.rule == *id && c.expect.positive() == pos)
        };
        if !of(true) {
            out.missing.push(format!("{id}: no case expects a match"));
        }
        if !of(false) {
            out.missing.push(format!("{id}: no case expects no match"));
        }
    }
    out.passed = out.cases.iter().all(|c| c.passed) && out.missing.is_empty();
    out
}

/// A tree path's components, or why it is not a valid tree path.
fn components(raw: &str) -> Result<Vec<String>, String> {
    let trimmed = raw.strip_suffix('/').unwrap_or(raw);
    if trimmed.is_empty() {
        return Err("empty path".into());
    }
    let parts: Vec<String> = trimmed.split('/').map(str::to_owned).collect();
    for p in &parts {
        if p.is_empty()
            || p == "."
            || p == ".."
            || p.contains('\\')
            || p.chars().any(char::is_control)
        {
            return Err(format!(
                "`{raw}`: use `/`-separated names, no empty, `.` or `..` parts"
            ));
        }
    }
    Ok(parts)
}

/// The rule's verdict on case `c`.
fn evaluate(c: &RawCase, rule: Option<&Rule>) -> CaseResult {
    let mut r = CaseResult {
        name: c.name.clone(),
        rule: c.rule.clone(),
        candidate: c.candidate.clone(),
        expect: c.expect,
        outcome: "invalid_case".into(),
        passed: false,
        detail: None,
    };
    let Some(rule) = rule else {
        r.detail = Some(format!("the rule file declares no rule `{}`", c.rule));
        return r;
    };
    if c.tree.len() > MAX_TREE_PATHS {
        r.detail = Some(format!("at most {MAX_TREE_PATHS} paths per tree"));
        return r;
    }
    // Every folder of the tree, with its entries.
    let mut dirs: BTreeMap<Vec<String>, BTreeSet<String>> = BTreeMap::new();
    dirs.insert(Vec::new(), BTreeSet::new());
    let mut add = |parts: &[String], is_dir: bool| {
        for i in 0..parts.len() {
            let parent = parts[..i].to_vec();
            dirs.entry(parent).or_default().insert(parts[i].clone());
            if i + 1 < parts.len() || is_dir {
                dirs.entry(parts[..=i].to_vec()).or_default();
            }
        }
    };
    for p in &c.tree {
        match components(p) {
            Ok(parts) => add(&parts, p.ends_with('/')),
            Err(e) => {
                r.detail = Some(e);
                return r;
            }
        }
    }
    let cand = match components(&c.candidate) {
        Ok(p) => p,
        Err(e) => {
            r.detail = Some(e);
            return r;
        }
    };
    add(&cand, true);
    let Some(name) = cand.last().cloned() else {
        r.detail = Some("empty candidate".into());
        return r;
    };
    let mut s = Surroundings::new(name);
    for level in 0..rule.required_levels() {
        // Level 0 is the candidate's own folder, 1 its parent, ...; above the root: unknown.
        if level > cand.len() {
            break;
        }
        let dir = cand[..cand.len() - level].to_vec();
        if let Some(entries) = dirs.get(&dir) {
            s = s.with_level(level, entries.iter().cloned());
        }
    }
    if rule.needs_ancestor_names() {
        let ancestors: Vec<String> = cand[..cand.len() - 1].iter().rev().cloned().collect();
        s = s.with_ancestor_names(ancestors);
    }
    let (outcome, detail) = match rule.check_markers(&s) {
        MarkerOutcome::NameMismatch => ("name_mismatch", None),
        MarkerOutcome::Incomplete { missing_level } => (
            "incomplete",
            Some(match missing_level {
                Some(l) => format!(
                    "the rule looks {l} level(s) up from the candidate, above the tree's root: \
                     put the candidate deeper in the tree"
                ),
                None => "the rule needs the ancestor folders' names".to_owned(),
            }),
        ),
        MarkerOutcome::Excluded { by } => ("excluded", Some(format!("excluded by `{by}`"))),
        MarkerOutcome::MissingMarker { marker } => {
            ("missing_marker", Some(format!("no `{marker}`")))
        }
        MarkerOutcome::Matched(m) if m.lockfile_missing => (
            "lockfile_missing",
            Some("markers found, no lockfile".to_owned()),
        ),
        MarkerOutcome::Matched(m) => (
            "match",
            Some(
                m.markers
                    .iter()
                    .map(|f| f.relative_path())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ),
    };
    r.passed = match c.expect {
        Expect::Match => outcome == "match",
        Expect::LockfileMissing => outcome == "lockfile_missing",
        Expect::NoMatch => matches!(outcome, "name_mismatch" | "excluded" | "missing_marker"),
        Expect::Excluded => outcome == "excluded",
        Expect::MissingMarker => outcome == "missing_marker",
    };
    r.outcome = outcome.to_owned();
    r.detail = detail;
    r
}

/// `--init`: write a starter cases file for `path` (never overwrites); the message to show.
fn scaffold(path: &Path) -> Result<String, ScullError> {
    let text = std::fs::read_to_string(path).map_err(|e| ScullError::from_io(&e))?;
    let ids = RuleSet::new()
        .add_user_file(&text, &path.display().to_string())
        .map_err(|e| ScullError::new(ErrorKind::Config, e.to_string()))?;
    let mut set = RuleSet::new();
    let _ = set.add_user_file(&text, "scaffold");
    let mut body = format!("schema_version = {CASES_SCHEMA_VERSION}\n");
    for id in &ids {
        let Some(rule) = set.get(id) else {
            continue;
        };
        let name = rule
            .names()
            .first()
            .and_then(|n| n.literals())
            .and_then(|l| l.into_iter().next())
            .unwrap_or_else(|| "CANDIDATE".to_owned());
        let markers: Vec<String> = rule
            .markers_all()
            .iter()
            .map(|m| m.as_str().to_owned())
            .collect();
        body.push_str(&format!(
            "\n# {id}: replace these with real trees from the tool's projects.\n\
             [[case]]\nname = \"{name} with its project files\"\nrule = \"{id}\"\n\
             tree = [{}]\ncandidate = \"project/{name}\"\nexpect = \"match\"\n\n\
             [[case]]\nname = \"{name} without the project files\"\nrule = \"{id}\"\n\
             tree = [\"project/{name}/\"]\ncandidate = \"project/{name}\"\nexpect = \"no_match\"\n",
            markers
                .iter()
                .filter(|m| m.starts_with("../") && !m.contains(['*', '?', '{']))
                .map(|m| format!("\"project/{}\"", &m[3..]))
                .chain(std::iter::once(format!("\"project/{name}/\"")))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let target = cases_path(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|e| {
            ScullError::new(
                ErrorKind::Usage,
                format!(
                    "cannot create {} ({e}); an existing cases file is never overwritten",
                    target.display()
                ),
            )
        })?;
    f.write_all(body.as_bytes())
        .map_err(|e| ScullError::from_io(&e))?;
    Ok(format!(
        "Wrote {} with a positive and a negative case per rule; edit the trees, then run \
         `scull rules test {}`.\n",
        escape_for_terminal(&target.display().to_string()),
        escape_for_terminal(&path.display().to_string())
    ))
}

/// The human report.
pub fn render(t: &RulesTest, color: bool) -> String {
    let esc = |s: &str| escape_for_terminal(s).into_owned();
    let mut out = String::new();
    for f in &t.files {
        let head = if f.passed {
            Style::Bold.paint("pass", color)
        } else {
            Style::Warn.paint("FAIL", color)
        };
        out.push_str(&format!(
            "{head} {} ({})\n",
            esc(&f.path),
            esc(&f.cases_path)
        ));
        if let Some(r) = &f.rejection {
            out.push_str(&format!("  [{}] {}\n", r.code, esc(&r.message)));
        }
        for c in &f.cases {
            out.push_str(&format!(
                "  {} {}: expected {}, got {}{}\n",
                if c.passed { "ok  " } else { "FAIL" },
                esc(&c.name),
                super::json_name(&c.expect),
                c.outcome,
                c.detail
                    .as_ref()
                    .map(|d| format!(" ({})", esc(d)))
                    .unwrap_or_default()
            ));
        }
        for m in &f.missing {
            out.push_str(&format!("  missing: {}\n", esc(m)));
        }
    }
    let total: usize = t.files.iter().map(|f| f.cases.len()).sum();
    let passed: usize = t
        .files
        .iter()
        .map(|f| f.cases.iter().filter(|c| c.passed).count())
        .sum();
    out.push_str(&format!("{passed} of {total} case(s) passed\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases_path_sits_beside_the_rule_file() {
        assert_eq!(
            cases_path(Path::new("/x/godot.toml")),
            Path::new("/x/godot.cases.toml")
        );
    }

    #[test]
    fn tree_paths_are_names_only() {
        assert!(components("a/b/c").is_ok());
        assert!(components("a/b/").is_ok());
        for bad in ["", "/", "a//b", "../x", "a/./b", "a\\b"] {
            assert!(components(bad).is_err(), "{bad:?}");
        }
    }
}
