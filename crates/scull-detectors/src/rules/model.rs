//! Typed, validated rules. A [`Rule`] can only be built by the loader, so every value of this
//! type has passed the CLS-01…05 checks (marker required, no commands, tier caps).

use super::pattern::NamePattern;
use scull_core::{Category, RuleRef, Tier};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Most `..` steps a relative marker may take (candidate → parent → … → great-great-grandparent).
pub const MAX_UP: usize = 4;

/// Operating systems a rule applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// macOS.
    Macos,
    /// Linux.
    Linux,
    /// Windows.
    Windows,
}

impl Platform {
    /// The platform this binary was compiled for, if it's one scull supports.
    pub fn current() -> Option<Platform> {
        if cfg!(target_os = "macos") {
            Some(Platform::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Platform::Linux)
        } else if cfg!(target_os = "windows") {
            Some(Platform::Windows)
        } else {
            None
        }
    }
}

/// How a candidate from this rule is cleaned up (PRD §10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Removed through the safe executor: delete for S0, quarantine for S1/S2 (D-3).
    Filesystem,
    /// The owning tool's own cleanup (e.g. `pnpm store prune`). **Bundled rules only**, and
    /// the actual command is chosen by Rust code keyed by rule ID; a rule never carries one.
    ToolNative,
    /// Shown for inspection only; never offered for cleanup.
    InspectOnly,
}

/// Kinds of evidence a rule produces when it matches (maps onto [`scull_core::Evidence`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// [`scull_core::Evidence::Marker`]. Required for every rule (CLS-03).
    Marker,
    /// [`scull_core::Evidence::Lockfile`]. Requires `lockfiles` to be declared.
    Lockfile,
    /// [`scull_core::Evidence::KnownArtifact`].
    KnownArtifact,
}

/// Where a rule came from. Decides its trust level (CLS-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    /// Compiled into the binary from `rules/` (CLS-05). May assign S0 and use `tool_native`.
    Bundled,
    /// A user or community rule file. Capped at S1, no `tool_native`, disabled until enabled.
    User,
}

/// A name pattern at a fixed position relative to the candidate directory.
///
/// Written as `../Cargo.toml` (in the candidate's parent, i.e. a sibling), `../../pnpm-lock.yaml`
/// (grandparent) or `CMakeCache.txt` (directly inside the candidate). Only the last component
/// may contain a pattern; all others must be `..`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelativeEntry {
    raw: String,
    up: usize,
    name: NamePattern,
}

impl RelativeEntry {
    pub(crate) fn new(raw: String, up: usize, name: NamePattern) -> Self {
        Self { raw, up, name }
    }

    /// The entry as written in the rule.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// How many directories up from the candidate the entry lives. `0` = inside the candidate,
    /// `1` = in the candidate's parent (a sibling of the candidate), `2` = grandparent, ….
    pub fn up(&self) -> usize {
        self.up
    }

    /// The name pattern of the entry.
    pub fn name(&self) -> &NamePattern {
        &self.name
    }
}

impl fmt::Display for RelativeEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Activity checks the rule asks for, on top of the universal activity window (PRD §8.3–8.4),
/// which always applies and can't be turned off by a rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityChecks {
    pub(crate) processes: Vec<String>,
    pub(crate) lock_files: Vec<String>,
}

impl ActivityChecks {
    /// Process names whose open handles or working directory inside the candidate mean "in use"
    /// (e.g. `cargo`, `rust-analyzer`). Names to look for only; nothing is ever spawned.
    pub fn processes(&self) -> &[String] {
        &self.processes
    }

    /// Lock files, relative to the candidate (forward slashes, no `..`), whose presence or held
    /// lock means a tool is working in the candidate (e.g. `debug/.cargo-lock`).
    pub fn lock_files(&self) -> &[String] {
        &self.lock_files
    }
}

/// How to regenerate the artifact after removal. Shown in the preview; never executed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegenerationGuidance {
    pub(crate) instructions: Option<String>,
    pub(crate) network: Option<bool>,
    pub(crate) long_build: Option<bool>,
}

impl RegenerationGuidance {
    /// Human instructions, e.g. "Run `cargo build`". Display text only.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// Whether regenerating needs network access (`None` = unknown).
    pub fn network(&self) -> Option<bool> {
        self.network
    }

    /// Whether regenerating needs a long build (`None` = unknown).
    pub fn long_build(&self) -> Option<bool> {
        self.long_build
    }
}

/// A validated detector rule (CLS-01).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub(crate) id: String,
    pub(crate) version: u32,
    pub(crate) artifact: String,
    pub(crate) ecosystem: String,
    pub(crate) category: Category,
    pub(crate) platforms: Vec<Platform>,
    pub(crate) base_tier: Tier,
    pub(crate) strategy: Strategy,
    pub(crate) evidence: Vec<EvidenceKind>,
    pub(crate) names: Vec<NamePattern>,
    pub(crate) markers_all: Vec<RelativeEntry>,
    pub(crate) markers_any: Vec<RelativeEntry>,
    pub(crate) lockfiles: Vec<RelativeEntry>,
    pub(crate) exclude_markers: Vec<RelativeEntry>,
    pub(crate) exclude_within: Vec<NamePattern>,
    pub(crate) activity: ActivityChecks,
    pub(crate) regeneration: RegenerationGuidance,
    pub(crate) source: RuleSource,
    pub(crate) origin: String,
}

impl Rule {
    /// Stable, namespaced ID, e.g. `rust.target`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Rule version; bumps whenever matching or tiering changes.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Reference recorded on every candidate the rule produces.
    pub fn rule_ref(&self) -> RuleRef {
        RuleRef {
            id: self.id.clone(),
            version: self.version,
        }
    }

    /// Human-readable artifact type, e.g. "Cargo build output".
    pub fn artifact(&self) -> &str {
        &self.artifact
    }

    /// Ecosystem, e.g. `rust`, `node`.
    pub fn ecosystem(&self) -> &str {
        &self.ecosystem
    }

    /// Category for grouping.
    pub fn category(&self) -> Category {
        self.category
    }

    /// Platforms the rule applies to.
    pub fn platforms(&self) -> &[Platform] {
        &self.platforms
    }

    /// Whether the rule applies on `platform`.
    pub fn applies_to(&self, platform: Platform) -> bool {
        self.platforms.contains(&platform)
    }

    /// The tier when every check passes (marker + lockfile present, idle, untracked…). The
    /// classifier only ever raises it (PRD §8.3). Never S0 for non-bundled rules.
    pub fn base_tier(&self) -> Tier {
        self.base_tier
    }

    /// Preferred cleanup strategy.
    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Evidence kinds the rule produces.
    pub fn evidence(&self) -> &[EvidenceKind] {
        &self.evidence
    }

    /// Candidate directory name patterns.
    pub fn names(&self) -> &[NamePattern] {
        &self.names
    }

    /// Markers that must **all** be present.
    pub fn markers_all(&self) -> &[RelativeEntry] {
        &self.markers_all
    }

    /// Markers of which **at least one** must be present (if any are listed).
    pub fn markers_any(&self) -> &[RelativeEntry] {
        &self.markers_any
    }

    /// Lockfiles/manifests to reproduce from; at least one must be present for the base tier,
    /// otherwise the candidate is at least S2 (PRD §8.3). Empty = the rule needs none.
    pub fn lockfiles(&self) -> &[RelativeEntry] {
        &self.lockfiles
    }

    /// Entries whose presence means the rule does **not** apply (matched case-insensitively,
    /// which excludes more and is therefore the protective choice).
    pub fn exclude_markers(&self) -> &[RelativeEntry] {
        &self.exclude_markers
    }

    /// Ancestor directory names that exclude the candidate (e.g. a `node_modules` nested in
    /// another `node_modules` is part of the outer one). Matched case-insensitively.
    pub fn exclude_within(&self) -> &[NamePattern] {
        &self.exclude_within
    }

    /// Rule-specific activity checks.
    pub fn activity(&self) -> &ActivityChecks {
        &self.activity
    }

    /// Regeneration guidance.
    pub fn regeneration(&self) -> &RegenerationGuidance {
        &self.regeneration
    }

    /// Where the rule came from.
    pub fn source(&self) -> RuleSource {
        self.source
    }

    /// File the rule was loaded from.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Number of directory levels the marker check needs, counting from the candidate:
    /// `1` = only entries inside the candidate, `2` = also the parent's entries, ….
    pub fn required_levels(&self) -> usize {
        self.markers_all
            .iter()
            .chain(&self.markers_any)
            .chain(&self.lockfiles)
            .chain(&self.exclude_markers)
            .map(|e| e.up + 1)
            .max()
            .unwrap_or(0)
    }

    /// Whether the marker check needs the candidate's ancestor directory names.
    pub fn needs_ancestor_names(&self) -> bool {
        !self.exclude_within.is_empty()
    }
}
