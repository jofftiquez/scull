//! Errors returned by the rule loader. Every rejection names the file (`origin`) and, when
//! known, the rule ID, so `scull doctor` and the desktop app can point at the exact problem.

use std::fmt;

/// A rule file or rule was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    /// Where the rule came from (bundled file name or user path).
    pub origin: String,
    /// The rule ID, if the failure is about one rule.
    pub rule_id: Option<String>,
    /// What went wrong.
    pub kind: RuleErrorKind,
}

/// The reason a rule file or rule was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuleErrorKind {
    /// The file is not valid TOML.
    Syntax(String),
    /// The file has no top-level `schema_version`.
    MissingSchemaVersion,
    /// `schema_version` is not one this build understands.
    UnsupportedSchemaVersion {
        /// The version in the file.
        found: i64,
        /// The version this build supports.
        supported: u32,
    },
    /// A key that looks like a command, script or hook (CLS-04, FX-CFG-06). Rules are data
    /// only; tool-native cleanup commands are chosen by Rust code, never by rules.
    CommandField {
        /// Dotted path of the offending key, e.g. `rule[0].cleanup_command`.
        key: String,
    },
    /// The file doesn't match the schema (unknown field, wrong type, missing field).
    Schema(String),
    /// The file declares no rules.
    NoRules,
    /// The rule declares no required marker, so it would match on a directory name alone
    /// (CLS-03).
    NameOnly,
    /// A marker could be satisfied by the candidate directory itself (e.g. `../target` for a
    /// rule matching `target`), which is name-only matching in disguise (CLS-03).
    MarkerMatchesCandidate {
        /// The marker as written.
        marker: String,
    },
    /// A marker is a bare wildcard such as `../*`, which any directory satisfies (CLS-03).
    WildcardMarker {
        /// The marker as written.
        marker: String,
    },
    /// The rule ID is malformed.
    InvalidId(String),
    /// The rule version is 0.
    InvalidVersion,
    /// A name pattern is malformed.
    InvalidPattern {
        /// The pattern as written.
        pattern: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A relative path (marker, lockfile, exclusion or activity lock file) is malformed.
    InvalidPath {
        /// The path as written.
        path: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A required field is empty or a list has duplicates.
    InvalidField {
        /// Field name.
        field: &'static str,
        /// Why it was rejected.
        reason: String,
    },
    /// The declared `evidence` doesn't match what the rule can actually produce.
    EvidenceMismatch(String),
    /// A non-bundled rule assigns S0. User and community rules are capped at S1 until they can
    /// ship fixture tests that pass in CI (CLS-04, FX-CFG-05).
    S0NotAllowed,
    /// A non-bundled rule asks for the tool-native strategy. Only bundled rules may, and even
    /// then the command is decided by Rust code (CLS-04).
    ToolNativeNotAllowed,
    /// The category can't be produced by rules (e.g. scull's own data).
    ForbiddenCategory(String),
    /// Two rules share an ID.
    DuplicateId,
    /// No rule with that ID is loaded.
    UnknownRule,
}

impl RuleError {
    pub(crate) fn file(origin: &str, kind: RuleErrorKind) -> Self {
        Self {
            origin: origin.to_string(),
            rule_id: None,
            kind,
        }
    }

    pub(crate) fn rule(origin: &str, id: &str, kind: RuleErrorKind) -> Self {
        Self {
            origin: origin.to_string(),
            rule_id: Some(id.to_string()),
            kind,
        }
    }
}

impl fmt::Display for RuleErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(m) => write!(f, "invalid TOML: {m}"),
            Self::MissingSchemaVersion => f.write_str("missing top-level `schema_version`"),
            Self::UnsupportedSchemaVersion { found, supported } => write!(
                f,
                "unsupported schema_version {found} (this build supports {supported})"
            ),
            Self::CommandField { key } => write!(
                f,
                "`{key}` looks like a command field; rules are data only and can never run \
                 commands (CLS-04)"
            ),
            Self::Schema(m) => write!(f, "schema error: {m}"),
            Self::NoRules => f.write_str("file declares no `[[rule]]`"),
            Self::NameOnly => f.write_str(
                "rule has no required marker; a directory name alone is never enough (CLS-03)",
            ),
            Self::MarkerMatchesCandidate { marker } => write!(
                f,
                "marker `{marker}` can be satisfied by the candidate itself (CLS-03)"
            ),
            Self::WildcardMarker { marker } => write!(
                f,
                "marker `{marker}` is a bare wildcard and proves nothing (CLS-03)"
            ),
            Self::InvalidId(m) => write!(f, "invalid rule id: {m}"),
            Self::InvalidVersion => f.write_str("rule version must be at least 1"),
            Self::InvalidPattern { pattern, reason } => {
                write!(f, "invalid pattern `{pattern}`: {reason}")
            }
            Self::InvalidPath { path, reason } => write!(f, "invalid path `{path}`: {reason}"),
            Self::InvalidField { field, reason } => write!(f, "invalid `{field}`: {reason}"),
            Self::EvidenceMismatch(m) => write!(f, "evidence mismatch: {m}"),
            Self::S0NotAllowed => f.write_str(
                "only bundled rules may assign S0; user and community rules are capped at S1 \
                 (CLS-04)",
            ),
            Self::ToolNativeNotAllowed => {
                f.write_str("only bundled rules may use the tool_native strategy (CLS-04)")
            }
            Self::ForbiddenCategory(c) => write!(f, "category `{c}` can't be produced by rules"),
            Self::DuplicateId => f.write_str("duplicate rule id"),
            Self::UnknownRule => f.write_str("no rule with that id"),
        }
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.rule_id {
            Some(id) => write!(f, "{} (rule `{id}`): {}", self.origin, self.kind),
            None => write!(f, "{}: {}", self.origin, self.kind),
        }
    }
}

impl std::error::Error for RuleError {}
