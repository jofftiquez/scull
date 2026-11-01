//! The detector interface: given a directory the scanner visited, decide whether it is an
//! artifact of some ecosystem and say why (PRD §8.1, CLS-01…03).
//!
//! * [`Detector`]: one ecosystem detector. [`RuleDetector`] drives any declarative [`Rule`];
//!   ecosystem-specific detectors (CLS-02) wrap a rule and add what data can't express.
//! * [`Detectors`]: the registry built from a [`RuleSet`] ([`Detectors::bundled`]).
//! * [`DetectedArtifact`]: what a detector found. It carries only what the detector can know:
//!   rule, evidence, owning project(s), regeneration guidance and the [`DetectorSignals`].
//!   Everything else the rubric needs (Git tracking, in-use, activity, protected roots, mounts,
//!   cloud sync) is supplied by the caller as [`CallerSignals`] when it turns the artifact into
//!   a [`Candidate`] ([`DetectedArtifact::into_candidate`]).
//!
//! All filesystem access goes through [`scull_platform::Platform`]: no-follow metadata and
//! no-follow listings only. A detector never follows a link, never opens a special file and never
//! modifies anything.

use crate::rules::{
    MarkerMatch, MarkerOutcome, Platform as RulePlatform, RegenerationGuidance, Rule, RuleError,
    RuleSet, Strategy, Surroundings,
};
use scull_core::{
    Action, Candidate, Category, Evidence, FileIdentity, Flags, FsPath, ItemId, ProjectRef,
    ReasonCode, Regeneration, RegenerationCost, RuleRef, Sizes, Tier, TierReason,
};
use scull_platform::{EntryKind, Platform, native_path_bytes};
use scull_safety::{GitTracking, Regen, Signals, Tri};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Largest configuration file a detector hands to a [`ConfigReader`] (1 MiB). Real files (e.g.
/// `.cargo/config.toml`) are a few hundred bytes.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Reads small configuration files as **data** (e.g. `.cargo/config.toml`), for detectors that
/// need more than entry names. Nothing read this way is ever executed or interpreted as a
/// command.
///
/// `scull-platform` has no file-content API yet, so the caller supplies the reader. Before
/// calling it, detectors check through [`Platform::metadata`] (no-follow) that `path` is a
/// regular file no larger than `max_bytes`; implementations should still refuse anything else
/// (links, FIFOs, devices, larger files) and return `None` on any error. `None` is "unknown",
/// never "empty": detectors then skip whatever depended on the file (the protective choice).
pub trait ConfigReader: Send + Sync {
    /// The contents of `path` as UTF-8 text, or `None` if it can't be read.
    fn read_text(&self, path: &Path, max_bytes: u64) -> Option<String>;
}

/// Inputs a detection needs beyond the directory and the platform.
#[derive(Clone, Copy, Default)]
#[non_exhaustive]
pub struct DetectContext<'a> {
    /// Reader for small configuration files. Without one, detectors that need file contents
    /// (e.g. `.cargo/config.toml` `[build] target-dir`) skip that evidence.
    pub config_reader: Option<&'a dyn ConfigReader>,
}

impl fmt::Debug for DetectContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DetectContext")
            .field("config_reader", &self.config_reader.is_some())
            .finish()
    }
}

impl<'a> DetectContext<'a> {
    /// A context with nothing but defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use `reader` for configuration files.
    #[must_use]
    pub fn with_config_reader(mut self, reader: &'a dyn ConfigReader) -> Self {
        self.config_reader = Some(reader);
        self
    }
}

/// The rubric signals a detector can establish on its own (see [`Signals`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DetectorSignals {
    /// The required project marker was found at the expected relative location (CLS-03).
    /// Always `true` for a returned artifact: without it there is no artifact.
    pub marker_present: bool,
    /// A lockfile or manifest the artifact can be reproduced from is present.
    pub lockfile_present: bool,
    /// The artifact type is known to a rule.
    pub artifact_known: bool,
    /// Regeneration cost, from the rule's hints (unknown hints give [`Regen::Unknown`]).
    pub regen: Regen,
}

/// The rubric signals only the caller can establish (Git layer, platform layer, scanner,
/// config). There is deliberately no `Default`: each one must be stated, and anything not
/// observed must be given its protective value (PRD §8.4), never "safe".
///
/// | Field | Who fills it | Protective value when unknown |
/// |---|---|---|
/// | `git` | Git layer (`scull-git`) | [`GitTracking::Unknown`] |
/// | `in_protected_root`, `user_excluded`, `protect_marker` | config / protected roots (SAFE-03, SAFE-13) | `true` |
/// | `in_use` | platform (open handles, process cwd, the artifact's [`DetectedArtifact::activity_lock_files`]) | [`Tri::Unknown`] |
/// | `crosses_mount`, `link_escape`, `cloud_sync` | scanner / platform | `true` |
/// | `recent_activity` | scanner (newest mtime) via [`scull_safety::ActivityWindow::observe`] | [`Tri::Unknown`] |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallerSignals {
    /// Git tracking of files inside the artifact.
    pub git: GitTracking,
    /// Inside a protected root.
    pub in_protected_root: bool,
    /// Matches a user exclusion.
    pub user_excluded: bool,
    /// Covered by a `.scull-protect` marker.
    pub protect_marker: bool,
    /// In use by a process or tool.
    pub in_use: Tri,
    /// Crosses a mount or volume boundary.
    pub crosses_mount: bool,
    /// Reached through a link pointing outside the scan root.
    pub link_escape: bool,
    /// Cloud-sync placeholder or inside a synced folder.
    pub cloud_sync: bool,
    /// Modified within the activity window.
    pub recent_activity: Tri,
}

impl CallerSignals {
    /// Nothing observed: every signal at its protective value. An artifact classified with
    /// these is S3 (blocked). Use it only when the caller truly checked nothing.
    pub const fn unobserved() -> Self {
        Self {
            git: GitTracking::Unknown,
            in_protected_root: true,
            user_excluded: true,
            protect_marker: true,
            in_use: Tri::Unknown,
            crosses_mount: true,
            link_escape: true,
            cloud_sync: true,
            recent_activity: Tri::Unknown,
        }
    }
}

/// What a detector found: a pre-candidate. Sizes are the scanner's job and are not part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DetectedArtifact {
    /// The artifact directory, as visited.
    pub path: PathBuf,
    /// Its no-follow file identity, if the platform provides one.
    pub identity: Option<FileIdentity>,
    /// Rule (or Rust detector) that produced it.
    pub rule: RuleRef,
    /// Category for grouping.
    pub category: Category,
    /// Human-readable artifact type.
    pub artifact: String,
    /// Preferred cleanup strategy.
    pub strategy: Strategy,
    /// The rule's base tier: the classifier never goes below it.
    pub base_tier: Tier,
    /// Owning projects, sorted by root. Usually one; a shared Cargo target directory lists
    /// every project that uses it (its bytes still belong to this one artifact). The first is
    /// recorded as [`Candidate::project`].
    pub projects: Vec<ProjectRef>,
    /// Detection evidence (markers, lockfile, known artifact, notes).
    pub evidence: Vec<Evidence>,
    /// Regeneration guidance. The cost stays [`RegenerationCost::Unknown`] without evidence (D-5).
    pub regeneration: Regeneration,
    /// Signals the detector established.
    pub signals: DetectorSignals,
    /// `Some(true)` if the artifact contains symlinks/junctions the detector saw (never
    /// followed); `None` if it didn't look everywhere. Never `Some(false)` unless checked fully.
    pub contains_links: Option<bool>,
    /// Lock files inside the artifact whose held lock means a tool is working in it (from the
    /// rule's `activity.lock_files`). The caller checks them when filling
    /// [`CallerSignals::in_use`].
    pub activity_lock_files: Vec<PathBuf>,
    /// Process names whose open handles or cwd inside the artifact mean "in use".
    pub activity_processes: Vec<String>,
    /// For a [`Strategy::ToolNative`] rule: the owning tool's command for this artifact
    /// ([`Action::ToolCommand`]), chosen by Rust code keyed by rule ID (never by a rule file,
    /// CLS-04). `None` means no command is known, and the candidate is blocked (S3).
    pub tool_action: Option<Action>,
}

impl DetectedArtifact {
    /// The full rubric input: the detector's signals plus the caller's.
    pub fn signals(&self, caller: &CallerSignals) -> Signals {
        Signals {
            git: caller.git,
            in_protected_root: caller.in_protected_root,
            user_excluded: caller.user_excluded,
            protect_marker: caller.protect_marker,
            in_use: caller.in_use,
            crosses_mount: caller.crosses_mount,
            link_escape: caller.link_escape,
            cloud_sync: caller.cloud_sync,
            marker_present: self.signals.marker_present,
            lockfile_present: self.signals.lockfile_present,
            artifact_known: self.signals.artifact_known,
            regen: self.signals.regen,
            recent_activity: caller.recent_activity,
        }
    }

    /// Classify the artifact and build the [`Candidate`].
    ///
    /// The candidate starts at S0 with the strategy's action and is raised by, in order: the
    /// PRD §8.3 rubric ([`scull_safety::apply`]); the rule's base tier (never below it); S3 for
    /// non-filesystem strategies (inspect-only, or tool-native cleanup not wired yet); and S3 if
    /// the platform gave no file identity, since such an item can't be revalidated before
    /// removal (SAFE-05; the ID is then derived from the path with a zero identity). Sizes are
    /// zero with unknown confidence until the scanner measures them.
    pub fn into_candidate(self, caller: &CallerSignals) -> Candidate {
        let signals = self.signals(caller);
        let (identity, identity_missing) = match self.identity {
            Some(i) => (i, false),
            None => (FileIdentity::new(0, 0), true),
        };
        let mut candidate = Candidate {
            id: ItemId::derive(identity, &native_path_bytes(&self.path)),
            path: FsPath::from_path(&self.path),
            identity,
            category: self.category,
            artifact: self.artifact,
            rule: self.rule.clone(),
            project: self.projects.into_iter().next(),
            tier: Tier::S0,
            reasons: Vec::new(),
            evidence: self.evidence,
            sizes: Sizes::default(),
            flags: Flags {
                symlink: self.contains_links,
                ..Flags::default()
            },
            regeneration: self.regeneration,
            action: match (self.strategy, &self.tool_action) {
                (Strategy::Filesystem, _) => Action::default_for(Tier::S0),
                (Strategy::ToolNative, Some(a @ Action::ToolCommand { .. })) => a.clone(),
                (Strategy::ToolNative | Strategy::InspectOnly, _) => Action::None,
            },
        };
        scull_safety::apply(&mut candidate, &signals);
        if let Some(reason) = base_tier_reason(&self.rule.id, self.base_tier) {
            candidate.raise(reason);
        }
        match self.strategy {
            Strategy::Filesystem => {}
            Strategy::InspectOnly => candidate.raise(TierReason::new(
                Tier::S3,
                ReasonCode::InspectOnly,
                "inspect-only rule; never offered for cleanup",
            )),
            Strategy::ToolNative => {
                if !matches!(self.tool_action, Some(Action::ToolCommand { .. })) {
                    candidate.raise(TierReason::new(
                        Tier::S3,
                        ReasonCode::InspectOnly,
                        "cleaned by the owning tool's own command, which is not available yet",
                    ));
                }
            }
        }
        if identity_missing {
            candidate.raise(TierReason::new(
                Tier::S3,
                ReasonCode::SignalUnavailable,
                "file identity unavailable on this platform; the item can't be revalidated \
                 before removal",
            ));
            candidate.evidence.push(Evidence::SignalUnavailable {
                signal: "file_identity".into(),
                reason: "the platform reported no file identity for this directory".into(),
            });
        }
        candidate
    }
}

/// The reason that keeps a candidate at or above its rule's base tier, if that is above S0.
fn base_tier_reason(rule_id: &str, base: Tier) -> Option<TierReason> {
    let (code, what) = match base {
        Tier::S0 => return None,
        Tier::S1 => (ReasonCode::CostlyToRegenerate, "costly to recreate"),
        Tier::S2 => (ReasonCode::SignalUnavailable, "uncertain to reproduce"),
        Tier::S3 => (ReasonCode::InspectOnly, "protected"),
    };
    Some(TierReason::new(
        base,
        code,
        format!("rule `{rule_id}` rates this artifact {what} (base tier {base:?})"),
    ))
}

/// Regeneration cost from a rule's hints (PRD §8.3: S0 = "regenerable from local inputs or
/// quickly", S1 = "needs network or a long build"). A long build is always costly; an explicit
/// `quick` (with `long_build = false`) is quick even if it uses the network; otherwise only an
/// explicit "no network and no long build" counts as local. Anything unstated is unknown (D-5).
pub(crate) fn regen_from(g: &RegenerationGuidance) -> Regen {
    match (g.network(), g.long_build(), g.quick()) {
        (_, Some(true), _) => Regen::NetworkOrLong,
        (_, Some(false), Some(true)) => Regen::Local,
        (Some(true), _, _) => Regen::NetworkOrLong,
        (Some(false), Some(false), _) => Regen::Local,
        _ => Regen::Unknown,
    }
}

/// One ecosystem detector.
pub trait Detector: Send + Sync {
    /// The ID recorded on artifacts this detector produces (a rule ID).
    fn id(&self) -> &str;

    /// Decide whether `dir` (a directory the scanner visited, given as an absolute path) is an
    /// artifact of this detector's kind.
    ///
    /// Returns `None` when it is not, and also whenever the evidence can't be gathered (an
    /// unreadable directory level, a non-UTF-8 name, `dir` being a link): missing evidence is
    /// never evidence (PRD §8.4). The caller should not descend into a returned artifact;
    /// anything nested inside belongs to it.
    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact>;
}

/// Detects any declarative [`Rule`]: name, exclusions, markers and lockfile, from no-follow
/// listings of the directory and the ancestor levels the rule needs.
#[derive(Debug, Clone)]
pub struct RuleDetector {
    rule: Rule,
}

impl RuleDetector {
    /// A detector for `rule`.
    pub fn new(rule: Rule) -> Self {
        Self { rule }
    }

    /// The rule it applies.
    pub fn rule(&self) -> &Rule {
        &self.rule
    }
}

impl Detector for RuleDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        _ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        detect_with_rule(&self.rule, dir, platform).map(|(artifact, _)| artifact)
    }
}

/// The no-follow entries of one directory: name → kind. Names that aren't valid UTF-8 are left
/// out (no rule pattern can match them).
pub(crate) type Listing = BTreeMap<String, EntryKind>;

/// List `dir` without following links. `None` if the directory or **any** entry can't be read:
/// a partial listing could hide an exclusion marker, so it is treated as unknown.
pub(crate) fn list(platform: &dyn Platform, dir: &Path) -> Option<Listing> {
    let mut out = Listing::new();
    for entry in platform.read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if let Some(name) = entry.file_name.to_str() {
            out.insert(name.to_owned(), entry.kind);
        }
    }
    Some(out)
}

/// The directory's own name as UTF-8.
pub(crate) fn dir_name(dir: &Path) -> Option<&str> {
    dir.file_name()?.to_str()
}

/// What [`gather`] observed around a candidate directory.
pub(crate) struct Gathered {
    /// Entry names at every level `rule` needs, plus ancestor names.
    pub surroundings: Surroundings,
    /// The candidate's own entries, with kinds.
    pub inside: Listing,
    /// The candidate's no-follow identity.
    pub identity: Option<FileIdentity>,
}

/// Apply `rule` to `dir`. On a match returns the artifact and the listing of `dir` itself.
pub(crate) fn detect_with_rule(
    rule: &Rule,
    dir: &Path,
    platform: &dyn Platform,
) -> Option<(DetectedArtifact, Listing)> {
    let g = gather(rule, dir, platform)?;
    let MarkerOutcome::Matched(m) = rule.check_markers(&g.surroundings) else {
        return None;
    };
    let artifact = artifact_from_match(rule, dir, g.identity, &m, &g.inside);
    Some((artifact, g.inside))
}

/// Name check (pure, first), platform check, no-follow metadata (`dir` must be a real
/// directory), then no-follow listings of `dir` and of every ancestor level `rule` needs.
pub(crate) fn gather(rule: &Rule, dir: &Path, platform: &dyn Platform) -> Option<Gathered> {
    // Cheap, pure check first: most visited directories fail here without any I/O.
    let name = dir_name(dir)?;
    if !rule.names().iter().any(|n| n.matches(name)) {
        return None;
    }
    if let Some(current) = RulePlatform::current()
        && !rule.applies_to(current)
    {
        return None;
    }
    let md = platform.metadata(dir).ok()?;
    if md.kind != EntryKind::Dir {
        return None; // a link named like an artifact is never one
    }

    let inside = list(platform, dir)?;
    let mut s = Surroundings::new(name).with_inside(inside.keys().cloned());
    for level in 1..rule.required_levels() {
        let Some(at) = dir.ancestors().nth(level) else {
            break; // above the filesystem root: the level stays unknown
        };
        if let Some(entries) = list(platform, at) {
            s = s.with_level(level, entries.into_keys());
        }
    }
    s = s.with_ancestor_names(
        dir.ancestors()
            .skip(1)
            .filter_map(|a| a.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
    );
    Some(Gathered {
        surroundings: s,
        inside,
        identity: md.identity,
    })
}

/// Build the artifact for a marker match of `rule` on `dir`.
pub(crate) fn artifact_from_match(
    rule: &Rule,
    dir: &Path,
    identity: Option<FileIdentity>,
    m: &MarkerMatch,
    inside: &Listing,
) -> DetectedArtifact {
    // The project root is the nearest ancestor holding a marker (markers inside the artifact,
    // such as `CMakeCache.txt`, point at its parent).
    let up = m
        .markers
        .iter()
        .map(|f| f.up)
        .filter(|&u| u >= 1)
        .min()
        .unwrap_or(1);
    let projects = dir
        .ancestors()
        .nth(up)
        .map(|root| vec![project_ref(root, rule.ecosystem())])
        .unwrap_or_default();
    DetectedArtifact {
        path: dir.to_path_buf(),
        identity,
        rule: rule.rule_ref(),
        category: rule.category(),
        artifact: rule.artifact().to_owned(),
        strategy: rule.strategy(),
        base_tier: rule.base_tier(),
        projects,
        evidence: m.evidence(rule, dir),
        regeneration: Regeneration {
            instructions: rule.regeneration().instructions().map(str::to_owned),
            cost: RegenerationCost::Unknown,
        },
        signals: DetectorSignals {
            marker_present: true,
            lockfile_present: !m.lockfile_missing,
            artifact_known: true,
            regen: regen_from(rule.regeneration()),
        },
        contains_links: inside.values().any(|k| k.is_link()).then_some(true),
        activity_lock_files: rule
            .activity()
            .lock_files()
            .iter()
            .map(|l| l.split('/').fold(dir.to_path_buf(), |p, seg| p.join(seg)))
            .collect(),
        activity_processes: rule.activity().processes().to_vec(),
        tool_action: None,
    }
}

/// A [`ProjectRef`] for the project rooted at `root`, named after the directory.
pub(crate) fn project_ref(root: &Path, ecosystem: &str) -> ProjectRef {
    ProjectRef {
        root: FsPath::from_path(root),
        name: root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned()),
        ecosystem: ecosystem.to_owned(),
    }
}

/// The detector registry.
pub struct Detectors {
    detectors: Vec<Box<dyn Detector>>,
}

impl fmt::Debug for Detectors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.detectors.iter().map(|d| d.id()))
            .finish()
    }
}

impl Detectors {
    /// Detectors for the bundled rules (CLS-05).
    pub fn bundled() -> Result<Self, RuleError> {
        Ok(Self::from_rules(&RuleSet::bundled()?))
    }

    /// Detectors for every **enabled** rule in `rules` that applies to this platform, in rule
    /// ID order. Rules that need more than data get their ecosystem detector (CLS-02).
    pub fn from_rules(rules: &RuleSet) -> Self {
        let mut list: Vec<&Rule> = match RulePlatform::current() {
            Some(p) => rules.enabled_for(p).collect(),
            None => Vec::new(),
        };
        list.sort_by(|a, b| a.id().cmp(b.id()));
        let detectors = list
            .into_iter()
            .map(|r| {
                crate::node::detector_for(r)
                    .or_else(|| crate::python::detector_for(r))
                    .or_else(|| crate::ml::detector_for(r))
                    .or_else(|| crate::dotnet::detector_for(r))
                    .or_else(|| crate::xcode::detector_for(r))
                    .or_else(|| crate::android::detector_for(r))
                    .or_else(|| crate::game_engines::detector_for(r))
                    .unwrap_or_else(|| Box::new(RuleDetector::new(r.clone())) as Box<dyn Detector>)
            })
            .collect();
        Self { detectors }
    }

    /// Add a detector (tried after the existing ones).
    pub fn push(&mut self, detector: Box<dyn Detector>) {
        self.detectors.push(detector);
    }

    /// IDs of the registered detectors, in the order they are tried.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.detectors.iter().map(|d| d.id())
    }

    /// Number of detectors.
    pub fn len(&self) -> usize {
        self.detectors.len()
    }

    /// Whether there are no detectors.
    pub fn is_empty(&self) -> bool {
        self.detectors.is_empty()
    }

    /// The first detector's artifact for `dir`, if any (see [`Detector::detect`]).
    pub fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        self.detectors
            .iter()
            .find_map(|d| d.detect(dir, platform, ctx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regen_needs_explicit_local_hints() {
        let g = |network, long_build, quick| RegenerationGuidance {
            instructions: None,
            network,
            long_build,
            quick,
        };
        let r = |n, l, q| regen_from(&g(n, l, q));
        assert_eq!(r(Some(false), Some(false), None), Regen::Local);
        assert_eq!(r(Some(true), Some(false), None), Regen::NetworkOrLong);
        assert_eq!(r(Some(false), Some(true), None), Regen::NetworkOrLong);
        assert_eq!(r(None, Some(false), None), Regen::Unknown);
        assert_eq!(r(None, None, None), Regen::Unknown);
        // `quick` makes a network reinstall quick, but never overrides a long build and needs
        // `long_build = false` stated.
        assert_eq!(r(Some(true), Some(false), Some(true)), Regen::Local);
        assert_eq!(r(Some(true), Some(true), Some(true)), Regen::NetworkOrLong);
        assert_eq!(r(Some(true), None, Some(true)), Regen::NetworkOrLong);
        assert_eq!(r(None, None, Some(true)), Regen::Unknown);
        assert_eq!(
            r(Some(true), Some(false), Some(false)),
            Regen::NetworkOrLong
        );
    }

    #[test]
    fn base_tier_floor_only_above_s0() {
        assert!(base_tier_reason("x.y", Tier::S0).is_none());
        for t in [Tier::S1, Tier::S2, Tier::S3] {
            assert_eq!(base_tier_reason("x.y", t).unwrap().tier, t);
        }
    }

    #[test]
    fn unobserved_caller_signals_block() {
        let a = DetectedArtifact {
            path: PathBuf::from("/p/target"),
            identity: Some(FileIdentity::new(1, 2)),
            rule: RuleRef {
                id: "rust.target".into(),
                version: 1,
            },
            category: Category::ProjectArtifact,
            artifact: "Cargo build output".into(),
            strategy: Strategy::Filesystem,
            base_tier: Tier::S0,
            projects: Vec::new(),
            evidence: Vec::new(),
            regeneration: Regeneration::default(),
            signals: DetectorSignals {
                marker_present: true,
                lockfile_present: true,
                artifact_known: true,
                regen: Regen::Local,
            },
            contains_links: None,
            activity_lock_files: Vec::new(),
            activity_processes: Vec::new(),
            tool_action: None,
        };
        let c = a.into_candidate(&CallerSignals::unobserved());
        assert_eq!(c.tier, Tier::S3);
        assert_eq!(c.action, Action::None);
        assert!(c.is_consistent());
    }
}
