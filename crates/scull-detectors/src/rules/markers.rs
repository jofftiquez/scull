//! Pure marker evaluation: does a rule's name, exclusion, marker and lockfile requirements hold
//! for a candidate, given the entry names around it? No I/O; the scanner supplies the names.

use super::model::{EvidenceKind, RelativeEntry, Rule};
use scull_core::{Evidence, FsPath};
use std::collections::BTreeSet;
use std::path::Path;

/// The entry names around a candidate directory, as listed by the scanner.
///
/// Levels count up from the candidate: level `0` is the candidate's own entries, level `1` its
/// parent's entries (the candidate's siblings, and the candidate itself), level `2` the
/// grandparent's, and so on. A level that wasn't listed is **unknown**, not empty: rules that
/// need it get [`MarkerOutcome::Incomplete`] instead of a guess (PRD §8.4: missing evidence is
/// never evidence).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Surroundings {
    candidate_name: String,
    levels: Vec<Option<BTreeSet<String>>>,
    ancestor_names: Option<Vec<String>>,
}

impl Surroundings {
    /// Surroundings of a candidate directory named `candidate_name`, with no levels listed yet.
    pub fn new(candidate_name: impl Into<String>) -> Self {
        Self {
            candidate_name: candidate_name.into(),
            levels: Vec::new(),
            ancestor_names: None,
        }
    }

    /// Set the entry names at `level` (0 = inside the candidate, 1 = parent, …).
    #[must_use]
    pub fn with_level<I, S>(mut self, level: usize, entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if self.levels.len() <= level {
            self.levels.resize(level + 1, None);
        }
        self.levels[level] = Some(entries.into_iter().map(Into::into).collect());
        self
    }

    /// Entries directly inside the candidate (level 0).
    #[must_use]
    pub fn with_inside<I, S>(self, entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_level(0, entries)
    }

    /// Entries in the candidate's parent, i.e. its siblings (level 1).
    #[must_use]
    pub fn with_siblings<I, S>(self, entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_level(1, entries)
    }

    /// Names of the candidate's ancestors, nearest first (parent, grandparent, …), up to the
    /// scan root. Needed by rules with `exclude.within`.
    #[must_use]
    pub fn with_ancestor_names<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.ancestor_names = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// The candidate's name.
    pub fn candidate_name(&self) -> &str {
        &self.candidate_name
    }

    fn level(&self, level: usize) -> Option<&BTreeSet<String>> {
        self.levels.get(level).and_then(Option::as_ref)
    }
}

/// A rule entry that was found, with the actual name that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundEntry {
    /// The rule's pattern as written, e.g. `../next.config.{js,mjs}`.
    pub pattern: String,
    /// Levels up from the candidate (see [`Surroundings`]).
    pub up: usize,
    /// The entry name that matched, e.g. `next.config.mjs`.
    pub name: String,
}

impl FoundEntry {
    /// The matched entry as a path relative to the candidate, e.g. `../Cargo.toml`.
    pub fn relative_path(&self) -> String {
        let mut s = "../".repeat(self.up);
        s.push_str(&self.name);
        s
    }
}

/// What the marker check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerMatch {
    /// Every required marker that was found (all of `markers.all`, and the first present entry
    /// of `markers.any`).
    pub markers: Vec<FoundEntry>,
    /// The first lockfile found, if the rule declares any.
    pub lockfile: Option<FoundEntry>,
    /// Whether the rule declares lockfiles but none was found. Such a candidate is at least S2
    /// (PRD §8.3, "marker found, but no lockfile").
    pub lockfile_missing: bool,
}

/// Result of [`Rule::check_markers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerOutcome {
    /// The candidate's name doesn't match the rule.
    NameMismatch,
    /// A level (or the ancestor names) the rule needs wasn't supplied. Not a candidate for this
    /// rule until the caller lists it.
    Incomplete {
        /// The first missing level, or `None` if the ancestor names are missing.
        missing_level: Option<usize>,
    },
    /// An exclusion applies.
    Excluded {
        /// The exclusion that matched (as written in the rule).
        by: String,
    },
    /// A required marker is absent: not a candidate (CLS-03).
    MissingMarker {
        /// The marker (or `any` group) that was not found.
        marker: String,
    },
    /// All marker requirements hold.
    Matched(MarkerMatch),
}

impl MarkerOutcome {
    /// The match, if the requirements hold.
    pub fn matched(&self) -> Option<&MarkerMatch> {
        match self {
            MarkerOutcome::Matched(m) => Some(m),
            _ => None,
        }
    }
}

fn find(entry: &RelativeEntry, s: &Surroundings) -> Option<FoundEntry> {
    let level = s.level(entry.up())?;
    level
        .iter()
        .find(|name| {
            // An entry in the parent level that *is* the candidate never counts as a marker.
            !(entry.up() == 1 && **name == s.candidate_name) && entry.name().matches(name)
        })
        .map(|name| FoundEntry {
            pattern: entry.as_str().to_string(),
            up: entry.up(),
            name: name.clone(),
        })
}

fn excluded_by(entry: &RelativeEntry, s: &Surroundings) -> bool {
    s.level(entry.up()).is_some_and(|level| {
        level.iter().any(|name| {
            !(entry.up() == 1 && *name == s.candidate_name)
                && entry.name().matches_ignore_case(name)
        })
    })
}

impl Rule {
    /// Decide whether this rule's name, exclusion, marker and lockfile requirements hold for a
    /// candidate, from entry names alone (no I/O).
    ///
    /// Order: candidate name → every needed level supplied → exclusions (case-insensitive) →
    /// `markers.all` → `markers.any` → lockfiles. Marker and lockfile names match
    /// case-sensitively (stricter, so fewer matches); exclusions case-insensitively (more
    /// exclusions). Both choices are the protective one.
    pub fn check_markers(&self, s: &Surroundings) -> MarkerOutcome {
        if !self.names.iter().any(|n| n.matches(&s.candidate_name)) {
            return MarkerOutcome::NameMismatch;
        }

        for level in 0..self.required_levels() {
            let needed = self
                .markers_all
                .iter()
                .chain(&self.markers_any)
                .chain(&self.lockfiles)
                .chain(&self.exclude_markers)
                .any(|e| e.up() == level);
            if needed && s.level(level).is_none() {
                return MarkerOutcome::Incomplete {
                    missing_level: Some(level),
                };
            }
        }
        if self.needs_ancestor_names() && s.ancestor_names.is_none() {
            return MarkerOutcome::Incomplete {
                missing_level: None,
            };
        }

        if let Some(e) = self.exclude_markers.iter().find(|e| excluded_by(e, s)) {
            return MarkerOutcome::Excluded {
                by: e.as_str().to_string(),
            };
        }
        if let Some(ancestors) = &s.ancestor_names
            && let Some(p) = self
                .exclude_within
                .iter()
                .find(|p| ancestors.iter().any(|a| p.matches_ignore_case(a)))
        {
            return MarkerOutcome::Excluded {
                by: format!("within {}", p.as_str()),
            };
        }

        let mut markers = Vec::new();
        for m in &self.markers_all {
            match find(m, s) {
                Some(found) => markers.push(found),
                None => {
                    return MarkerOutcome::MissingMarker {
                        marker: m.as_str().to_string(),
                    };
                }
            }
        }
        if !self.markers_any.is_empty() {
            match self.markers_any.iter().find_map(|m| find(m, s)) {
                Some(found) => markers.push(found),
                None => {
                    let group: Vec<&str> = self.markers_any.iter().map(|m| m.as_str()).collect();
                    return MarkerOutcome::MissingMarker {
                        marker: format!("any of [{}]", group.join(", ")),
                    };
                }
            }
        }

        let lockfile = self.lockfiles.iter().find_map(|l| find(l, s));
        MarkerOutcome::Matched(MarkerMatch {
            markers,
            lockfile_missing: !self.lockfiles.is_empty() && lockfile.is_none(),
            lockfile,
        })
    }
}

impl MarkerMatch {
    /// The [`scull_core::Evidence`] this match supports, given the candidate's path: one
    /// `Marker` per marker found, a `Lockfile` if one was found, and `KnownArtifact` if the
    /// rule declares it. Pure path arithmetic; nothing is read from disk.
    pub fn evidence(&self, rule: &Rule, candidate_dir: &Path) -> Vec<Evidence> {
        let locate = |f: &FoundEntry| {
            let base = candidate_dir.ancestors().nth(f.up).unwrap_or(candidate_dir);
            FsPath::from_path(&base.join(&f.name))
        };
        let mut out: Vec<Evidence> = self
            .markers
            .iter()
            .map(|m| Evidence::Marker { path: locate(m) })
            .collect();
        if let Some(l) = &self.lockfile {
            out.push(Evidence::Lockfile { path: locate(l) });
        }
        if rule.evidence().contains(&EvidenceKind::KnownArtifact) {
            out.push(Evidence::KnownArtifact {
                artifact: rule.artifact().to_string(),
            });
        }
        out
    }
}
