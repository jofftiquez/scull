//! Declarative, data-only detector rules (PRD §8.1, CLS-01…05).
//!
//! Rules are TOML files under `rules/` in the repository, compiled into the binary
//! ([`BUNDLED_RULE_FILES`], CLS-05). The format is documented in `rules/README.md`.
//!
//! * [`parse_rules`] parses and strictly validates one file into typed [`Rule`]s.
//! * [`RuleSet`] holds loaded rules and which are enabled (user rules start disabled, CLS-04).
//! * [`Rule::check_markers`] decides, from entry names alone, whether a candidate satisfies a
//!   rule's marker requirements (CLS-03). Walking the filesystem is the scanner's job.
//!
//! Safety properties enforced by the loader:
//!
//! * a rule must require at least one marker that the candidate itself can't satisfy (CLS-03);
//! * no key that looks like a command, script or hook is accepted anywhere (CLS-04, FX-CFG-06);
//! * user/community rules can't assign S0 or use the tool-native strategy, and are loaded
//!   disabled (CLS-04, FX-CFG-05); they can't claim a folder that holds people's work
//!   ([`PROTECTED_FOLDER_NAMES`]), need two literal characters in every candidate name, and
//!   need a marker beside or above the candidate (a project file) (P6-COM-01);
//! * unknown fields, unsupported `schema_version`s, malformed patterns/paths and duplicate IDs
//!   are rejected.

mod error;
mod loader;
mod markers;
mod model;
mod pattern;

pub use error::{RuleError, RuleErrorKind};
pub use loader::{
    BUNDLED_RULE_FILES, PROTECTED_FOLDER_NAMES, RuleSet, SCHEMA_VERSION, parse_rules,
};
pub use markers::{FoundEntry, MarkerMatch, MarkerOutcome, Surroundings};
pub use model::{
    ActivityChecks, EvidenceKind, MAX_UP, Platform, RegenerationGuidance, RelativeEntry, Rule,
    RuleSource, Strategy,
};
pub use pattern::{MAX_EXPANSIONS, MAX_PATTERN_LEN, NamePattern, PatternError};
