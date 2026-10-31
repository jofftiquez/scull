//! Python detectors (CLS-02): what the `python.*` rules can't express as data.
//!
//! * [`VenvDetector`] (`python.venv`): a virtual environment is rebuilt from a lockfile (`uv.lock`,
//!   `poetry.lock`, `Pipfile.lock`, `pdm.lock`, `pylock.toml`, `requirements*.lock`). When the
//!   rule finds none, a `requirements*.txt` beside the project counts as the lockfile if every
//!   requirement in it is pinned to one version ([`requirements_pinned`], read as data through a
//!   [`ConfigReader`](crate::ConfigReader)). Otherwise the environment stays "no lockfile"
//!   (S2, FX-CLS-07): it can't be reproduced reliably.
//!
//! * [`PycacheDetector`] (`python.pycache`): the rule accepts any `.py` beside `__pycache__` as
//!   the sources; this detector requires the source of **every** compiled module
//!   (`core.cpython-312.pyc` needs `core.py`) and nothing but compiled modules inside. A `.pyc`
//!   without its source may be the only copy of that code (a sourceless distribution), and an
//!   unexpected entry may not be bytecode at all: either one leaves the cache "no lockfile" (S2).
//!
//! The other Python artifacts (`.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.tox`, `.nox`)
//! are plain rules (`rules/ecosystems/python.toml`). The pip and uv download caches are package
//! caches ([`crate::caches`]).

use crate::detector::{DetectContext, DetectedArtifact, Detector, Listing, detect_with_rule, list};
use crate::node::read_small;
use crate::rules::{NamePattern, Rule};
use scull_core::{Evidence, FsPath};
use scull_platform::{EntryKind, Platform};
use std::path::Path;

/// Rule ID of Python virtual environments.
pub const VENV_RULE_ID: &str = "python.venv";

/// Rule ID of `__pycache__` directories.
pub const PYCACHE_RULE_ID: &str = "python.pycache";

/// At most this many orphaned modules are named in the evidence.
const MAX_SHOWN: usize = 3;

/// The ecosystem detector for `rule`, if it needs more than [`crate::RuleDetector`].
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    match rule.id() {
        VENV_RULE_ID => Some(Box::new(VenvDetector::new(rule.clone()))),
        PYCACHE_RULE_ID => Some(Box::new(PycacheDetector::new(rule.clone()))),
        _ => None,
    }
}

/// `__pycache__` whose every compiled module has its source beside it.
#[derive(Debug, Clone)]
pub struct PycacheDetector {
    rule: Rule,
}

impl PycacheDetector {
    /// A detector for the `python.pycache` rule (or a rule shaped like it).
    pub fn new(rule: Rule) -> Self {
        Self { rule }
    }
}

impl Detector for PycacheDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        _ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        let (mut a, inside) = detect_with_rule(&self.rule, dir, platform)?;
        if a.signals.lockfile_present {
            let problem = match list(platform, dir.parent()?) {
                Some(parent) => sources_problem(&inside, &parent),
                None => Some("the package folder could not be listed".to_owned()),
            };
            if let Some(why) = problem {
                a.signals.lockfile_present = false;
                a.evidence
                    .retain(|e| !matches!(e, Evidence::Lockfile { .. }));
                a.evidence.push(Evidence::Note { text: why });
            }
        }
        Some(a)
    }
}

/// Why the entries of `__pycache__` (`inside`) aren't all reproducible from the sources in its
/// package folder (`parent`), or `None` if they are.
fn sources_problem(inside: &Listing, parent: &Listing) -> Option<String> {
    let mut orphans = Vec::new();
    for (name, kind) in inside {
        let module = name.strip_suffix(".pyc").and_then(|n| n.split('.').next());
        match (kind, module) {
            (EntryKind::File, Some(m)) if !m.is_empty() => {
                let has_source = [".py", ".pyw"]
                    .iter()
                    .any(|ext| parent.get(&format!("{m}{ext}")) == Some(&EntryKind::File));
                if !has_source {
                    orphans.push(name.as_str());
                }
            }
            _ => {
                return Some(format!(
                    "`{name}` in this __pycache__ is not compiled bytecode, so the folder may \
                     hold more than a regenerable cache"
                ));
            }
        }
    }
    if orphans.is_empty() {
        return None;
    }
    let more = orphans.len().saturating_sub(MAX_SHOWN);
    let mut shown = orphans
        .iter()
        .take(MAX_SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if more > 0 {
        shown.push_str(&format!(", and {more} more"));
    }
    Some(format!(
        "compiled modules without their `.py` source beside them ({shown}): the bytecode may be \
         the only copy of that code"
    ))
}

/// A project virtual environment, with pinned `requirements*.txt` accepted as its lockfile.
#[derive(Debug, Clone)]
pub struct VenvDetector {
    rule: Rule,
}

impl VenvDetector {
    /// A detector for the `python.venv` rule (or a rule shaped like it).
    pub fn new(rule: Rule) -> Self {
        Self { rule }
    }
}

impl Detector for VenvDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        let (mut a, _) = detect_with_rule(&self.rule, dir, platform)?;
        if !a.signals.lockfile_present {
            pinned_requirements(&mut a, dir.parent()?, platform, ctx);
        }
        Some(a)
    }
}

/// Accept the first fully pinned `requirements*.txt` in `project` as the lockfile.
fn pinned_requirements(
    a: &mut DetectedArtifact,
    project: &Path,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) {
    let Some(reader) = ctx.config_reader else {
        return; // contents unknown: stays "no lockfile"
    };
    let Some(listing) = list(platform, project) else {
        return;
    };
    let Ok(pattern) = NamePattern::parse("requirements*.txt") else {
        return;
    };
    for (name, kind) in &listing {
        if *kind != EntryKind::File || !pattern.matches(name) {
            continue;
        }
        let path = project.join(name);
        let Some(text) = read_small(platform, reader, &path) else {
            continue;
        };
        if requirements_pinned(&text) {
            a.signals.lockfile_present = true;
            a.evidence.push(Evidence::Lockfile {
                path: FsPath::from_path(&path),
            });
            a.evidence.push(Evidence::Note {
                text: format!(
                    "every requirement in {} is pinned to one version (`==`), so it serves as \
                     the lockfile",
                    path.display()
                ),
            });
            return;
        }
    }
}

/// Whether a pip requirements file pins every requirement to exactly one version, so installing
/// it reproduces the environment (as `pip-compile` / `uv pip compile` / `pip freeze` write it).
///
/// Pure: the text is data, nothing is followed or run. A requirement counts as pinned only with a
/// single `==` (or `===`) specifier and no wildcard version. Editable installs of the project's
/// own sources (`-e .`, `-e ./x`, `-e file:...`) are local inputs and allowed; any other
/// editable or direct URL reference, a range, or no requirement at all makes the file unpinned.
/// Include/constraint lines (`-r`, `-c`) are not followed; the other global options are ignored.
pub fn requirements_pinned(text: &str) -> bool {
    let mut any = false;
    let mut logical = String::new();
    for raw in text.lines() {
        // Line continuations join physical lines.
        if let Some(stripped) = raw.strip_suffix('\\') {
            logical.push_str(stripped);
            logical.push(' ');
            continue;
        }
        logical.push_str(raw);
        let line = std::mem::take(&mut logical);
        match classify_line(&line) {
            Line::Skip => {}
            Line::Pinned => any = true,
            Line::Unpinned => return false,
        }
    }
    if !logical.is_empty() && classify_line(&logical) == Line::Unpinned {
        return false;
    }
    any
}

#[derive(Debug, PartialEq, Eq)]
enum Line {
    Skip,
    Pinned,
    Unpinned,
}

fn classify_line(line: &str) -> Line {
    // A comment starts at a `#` at the beginning or after whitespace.
    let line = match line.find(" #").or_else(|| line.find("\t#")) {
        Some(i) => &line[..i],
        None => line,
    };
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Line::Skip;
    }
    if let Some(opt) = line.strip_prefix('-') {
        let (flag, value) = match opt.split_once(|c: char| c == '=' || c.is_whitespace()) {
            Some((f, v)) => (f, v.trim()),
            None => (opt, ""),
        };
        return match flag {
            "e" | "-editable" => {
                let local = value == "."
                    || value.starts_with("./")
                    || value.starts_with(".\\")
                    || value.starts_with("file:");
                if local { Line::Skip } else { Line::Unpinned }
            }
            // Includes and constraints are not followed; global options don't install anything.
            _ => Line::Skip,
        };
    }
    // Drop per-requirement options (`--hash=...`) and environment markers (`; python_version`).
    let spec = line.split(" --").next().unwrap_or(line);
    let spec = spec.split(';').next().unwrap_or(spec).trim();
    if spec.contains(" @ ") || spec.contains("://") {
        return Line::Unpinned; // direct reference: what it points to can change
    }
    let Some(i) = spec.find("==") else {
        return Line::Unpinned;
    };
    let (name, version) = (spec[..i].trim(), spec[i..].trim_start_matches('=').trim());
    let pinned = !name.is_empty()
        && !version.is_empty()
        && !version.contains('*')
        && !version.contains(',')
        && !name.contains(['<', '>', '!', '~', ','])
        && !version.contains(['<', '>', '!', '~', '=']);
    if pinned { Line::Pinned } else { Line::Unpinned }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(entries: &[(&str, EntryKind)]) -> Listing {
        entries.iter().map(|(n, k)| ((*n).to_owned(), *k)).collect()
    }

    #[test]
    fn every_compiled_module_needs_its_source() {
        use EntryKind::{Dir, File};
        let parent = listing(&[
            ("core.py", File),
            ("gui.pyw", File),
            ("data.py", Dir),
            ("__pycache__", Dir),
        ]);
        let ok = listing(&[
            ("core.cpython-312.pyc", File),
            ("core.cpython-311.opt-1.pyc", File),
            ("gui.cpython-312.pyc", File),
        ]);
        assert_eq!(sources_problem(&ok, &parent), None);
        let orphan = listing(&[
            ("core.cpython-312.pyc", File),
            ("data.cpython-312.pyc", File),
        ]);
        let why = sources_problem(&orphan, &parent).unwrap();
        assert!(
            why.contains("data.cpython-312.pyc"),
            "a folder isn't a source"
        );
        let odd = listing(&[("core.cpython-312.pyc", File), ("x.pyc", Dir)]);
        assert!(sources_problem(&odd, &parent).unwrap().contains("`x.pyc`"));
        let many = listing(&[
            ("a.cpython-312.pyc", File),
            ("b.cpython-312.pyc", File),
            ("c.cpython-312.pyc", File),
            ("d.cpython-312.pyc", File),
            ("e.cpython-312.pyc", File),
        ]);
        assert!(
            sources_problem(&many, &parent)
                .unwrap()
                .contains("and 2 more")
        );
    }

    #[test]
    fn pinned_requirement_files() {
        assert!(requirements_pinned("requests==2.32.3\nidna==3.7\n"));
        assert!(requirements_pinned(
            "# This file is autogenerated by pip-compile\n\
             --index-url https://pypi.org/simple\n\
             -e .\n\
             certifi==2024.8.30 \\\n    --hash=sha256:abc \\\n    --hash=sha256:def\n    \
             # via requests\n\
             urllib3==2.2.3 ; python_version >= \"3.8\"  # via requests\n\
             Django[argon2]===5.1\n"
        ));
        assert!(requirements_pinned("-r base.txt\nflask==3.0.3\n"));
    }

    #[test]
    fn unpinned_requirement_files() {
        assert!(!requirements_pinned(""), "nothing pinned");
        assert!(!requirements_pinned("# only comments\n-r base.txt\n"));
        assert!(!requirements_pinned("requests\n"));
        assert!(!requirements_pinned("requests>=2\n"));
        assert!(!requirements_pinned("requests==2.*\n"));
        assert!(!requirements_pinned("requests~=2.32\n"));
        assert!(!requirements_pinned("requests>=2,==2.32.3\n"));
        assert!(!requirements_pinned("requests==2.32.3\nflask\n"));
        assert!(!requirements_pinned(
            "pkg @ https://example.com/pkg-1.0.whl\n"
        ));
        assert!(!requirements_pinned(
            "-e git+https://github.com/x/y#egg=y\n"
        ));
        assert!(!requirements_pinned("git+https://github.com/x/y\n"));
        assert!(!requirements_pinned("==1.0\n"));
        assert!(!requirements_pinned("requests==\n"));
    }
}
