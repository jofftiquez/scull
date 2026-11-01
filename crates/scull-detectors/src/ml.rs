//! ML / AI caches (P4-DET-01, PRD §7.1): Hugging Face, Ollama and PyTorch Hub. The conda
//! package cache is a package cache ([`crate::caches`], `conda.pkgs`).
//!
//! # Detection (`rules/ecosystems/ml.toml`)
//!
//! [`MlDetector`] runs the `huggingface.*`, `ollama.models` and `torch.hub` rules and adds what
//! data can't express:
//!
//! | Rule | Artifact | Tier |
//! |---|---|---|
//! | `huggingface.model` | one hub repository `models--<owner>--<name>` (`blobs/`, `snapshots/`) | **S1** (re-downloadable); **S2** without `refs/` (the revision it came from isn't recorded) |
//! | `huggingface.dataset` | one hub repository `datasets--<owner>--<name>` | **S1** with `refs/` (source recorded); otherwise **S2** (FX-CLS-13) |
//! | `huggingface.datasets-cache` | the `datasets` library's processed cache | **S2**: built from sources scull can't verify |
//! | `ollama.models` | the Ollama model store (`blobs/`, `manifests/`) | **S1**, cleaned with `ollama rm <models>` (tool-native; never run by the detector) |
//! | `torch.hub` | the PyTorch Hub cache (`checkpoints/`, hub repositories) | **S1** |
//!
//! ML caches belong to no project. In a scan walk, the folders whose names are generic
//! (`datasets`, `hub`) also need their usual parent (`huggingface`, `torch`, or torch's
//! `trusted_list`); folders moved elsewhere by environment variables are found by
//! [`locate_ml_caches`] instead.
//!
//! # Location ([`locate_ml_caches`])
//!
//! Everything machine-specific comes from [`CacheEnv`] (home, environment, OS layout); library
//! code never reads `std::env`.
//!
//! | Cache | Overrides, highest first | Default (every OS; `~` is `%USERPROFILE%` on Windows) |
//! |---|---|---|
//! | Hugging Face hub | `HF_HUB_CACHE`, `HUGGINGFACE_HUB_CACHE`, `$HF_HOME/hub`, `$XDG_CACHE_HOME/huggingface/hub` | `~/.cache/huggingface/hub` |
//! | Hugging Face datasets | `HF_DATASETS_CACHE`, `$HF_HOME/datasets`, `$XDG_CACHE_HOME/huggingface/datasets` | `~/.cache/huggingface/datasets` |
//! | Ollama | `OLLAMA_MODELS` | `~/.ollama/models` |
//! | PyTorch Hub | `$TORCH_HOME/hub`, `$XDG_CACHE_HOME/torch/hub` | `~/.cache/torch/hub` |
//!
//! A configured location that is relative, contains `..`, or is a filesystem root, the home
//! folder or an ancestor of it, or a shared folder (`~/.cache`, `%LocalAppData%`, ...) is
//! refused (T-23). So is a location that is a link: links are never followed.

use crate::caches::{CacheEnv, CacheSource, expand};
use crate::cargo::normalize;
use crate::detector::{DetectContext, DetectedArtifact, Detector, Listing, detect_with_rule, list};
use crate::rules::{NamePattern, Rule, RuleSet};
use scull_core::{Action, Evidence};
use scull_platform::{EntryKind, Platform};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// At most this many Ollama models are listed (and passed to `ollama rm`).
pub const MAX_OLLAMA_MODELS: usize = 512;

/// The Ollama registry whose `library` namespace is shown without a prefix.
const OLLAMA_REGISTRY: &str = "registry.ollama.ai";

/// What an ML rule detects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MlKind {
    /// A Hugging Face hub model repository.
    HubModel,
    /// A Hugging Face hub dataset repository.
    HubDataset,
    /// The Hugging Face `datasets` library's processed cache.
    DatasetsCache,
    /// The Ollama model store.
    Ollama,
    /// The PyTorch Hub cache.
    TorchHub,
}

impl MlKind {
    /// Every kind.
    pub const ALL: [MlKind; 5] = [
        MlKind::HubModel,
        MlKind::HubDataset,
        MlKind::DatasetsCache,
        MlKind::Ollama,
        MlKind::TorchHub,
    ];

    /// The rule ID.
    pub fn rule_id(self) -> &'static str {
        match self {
            MlKind::HubModel => "huggingface.model",
            MlKind::HubDataset => "huggingface.dataset",
            MlKind::DatasetsCache => "huggingface.datasets-cache",
            MlKind::Ollama => "ollama.models",
            MlKind::TorchHub => "torch.hub",
        }
    }

    /// The kind for a rule ID.
    pub fn from_rule_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.rule_id() == id)
    }
}

/// The ecosystem detector for `rule`, if it is an ML rule.
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    MlKind::from_rule_id(rule.id())
        .map(|k| Box::new(MlDetector::new(rule.clone(), k)) as Box<dyn Detector>)
}

/// An ML cache rule plus the checks data can't express (module docs).
#[derive(Debug, Clone)]
pub struct MlDetector {
    rule: Rule,
    kind: MlKind,
}

impl MlDetector {
    /// A detector for `rule`, treated as `kind`.
    pub fn new(rule: Rule, kind: MlKind) -> Self {
        Self { rule, kind }
    }

    /// What it detects.
    pub fn kind(&self) -> MlKind {
        self.kind
    }

    /// Detect `dir`, a cache root found by [`locate_ml_caches`]: the folder's name and parent
    /// don't matter (the location is the evidence), every other check applies.
    pub fn detect_located(&self, dir: &Path, platform: &dyn Platform) -> Option<DetectedArtifact> {
        let name = dir.file_name()?.to_str()?;
        let literal = NamePattern::parse(name).ok()?;
        if !literal.matches(name) {
            return None;
        }
        let mut rule = self.rule.clone();
        rule.names = vec![literal];
        self.finish(&rule, dir, platform, true)
    }

    fn finish(
        &self,
        rule: &Rule,
        dir: &Path,
        platform: &dyn Platform,
        located: bool,
    ) -> Option<DetectedArtifact> {
        let (mut a, inside) = detect_with_rule(rule, dir, platform)?;
        a.projects.clear(); // ML caches belong to no project
        match self.kind {
            MlKind::HubModel | MlKind::HubDataset => hub_repo(&mut a, dir, &inside, platform),
            MlKind::DatasetsCache => {
                if !located && parent_name(dir) != Some("huggingface") {
                    return None;
                }
                // No recorded source to reproduce from (FX-CLS-13: S2).
                a.signals.lockfile_present = false;
                a.evidence.push(Evidence::Note {
                    text: "processed datasets are built from sources (hub files, scripts or \
                           local files) whose availability scull can't verify"
                        .into(),
                });
            }
            MlKind::Ollama => ollama(&mut a, dir, platform)?,
            MlKind::TorchHub => {
                let tagged = inside.get("trusted_list") == Some(&EntryKind::File);
                if !located && !tagged && parent_name(dir) != Some("torch") {
                    return None;
                }
            }
        }
        Some(a)
    }
}

impl Detector for MlDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        _ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        self.finish(&self.rule, dir, platform, false)
    }
}

fn parent_name(dir: &Path) -> Option<&str> {
    dir.parent()?.file_name()?.to_str()
}

/// A hub repository: record the revisions its `refs/` points at, or why the source is unknown.
fn hub_repo(a: &mut DetectedArtifact, dir: &Path, inside: &Listing, platform: &dyn Platform) {
    let refs = match inside.get("refs") {
        Some(EntryKind::Dir) => list(platform, &dir.join("refs")).unwrap_or_default(),
        _ => Listing::new(),
    };
    let names: Vec<&str> = refs
        .iter()
        .filter(|(_, k)| **k == EntryKind::File)
        .map(|(n, _)| n.as_str())
        .collect();
    if names.is_empty() {
        // `refs` missing, not a folder, or empty: the revision isn't recorded.
        a.signals.lockfile_present = false;
        a.evidence
            .retain(|e| !matches!(e, Evidence::Lockfile { .. }));
        a.evidence.push(Evidence::Note {
            text: "no `refs/` revision recorded: which revision these files came from (and \
                   whether it can be downloaded again) is unknown"
                .into(),
        });
    } else {
        a.evidence.push(Evidence::Note {
            text: format!(
                "downloaded from the Hugging Face Hub repository `{}` (refs: {})",
                hub_repo_id(dir).unwrap_or_else(|| "?".into()),
                names.join(", ")
            ),
        });
    }
}

/// `models--owner--name` → `owner/name`.
fn hub_repo_id(dir: &Path) -> Option<String> {
    let name = dir.file_name()?.to_str()?;
    let (_, rest) = name.split_once("--")?;
    Some(rest.replace("--", "/"))
}

/// The installed Ollama models, as `ollama rm` takes them. `None` when there is none: a store
/// without manifests holds nothing `ollama rm` could remove, so it is not a candidate.
fn ollama(a: &mut DetectedArtifact, dir: &Path, platform: &dyn Platform) -> Option<()> {
    let models = ollama_models(&dir.join("manifests"), platform);
    if models.is_empty() {
        return None;
    }
    let mut args = vec!["rm".to_owned()];
    args.extend(models.iter().cloned());
    a.tool_action = Some(Action::ToolCommand {
        tool: "ollama".into(),
        args,
    });
    a.evidence.push(Evidence::Note {
        text: format!(
            "installed models: {}. Cleaned with `ollama rm` (Ollama deletes only blobs no \
             remaining model uses). Not undoable: models download again with `ollama pull`.",
            models.join(", ")
        ),
    });
    Some(())
}

/// Model names from `manifests/<registry>/<namespace>/<model>/<tag>`, sorted.
pub(crate) fn ollama_models(manifests: &Path, platform: &dyn Platform) -> Vec<String> {
    let dirs = |p: &Path| -> Vec<String> {
        list(platform, p)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, k)| *k == EntryKind::Dir)
            .map(|(n, _)| n)
            .collect()
    };
    let mut out = Vec::new();
    for host in dirs(manifests) {
        for ns in dirs(&manifests.join(&host)) {
            for model in dirs(&manifests.join(&host).join(&ns)) {
                let at = manifests.join(&host).join(&ns).join(&model);
                for (tag, kind) in list(platform, &at).unwrap_or_default() {
                    if kind != EntryKind::File || out.len() >= MAX_OLLAMA_MODELS {
                        continue;
                    }
                    out.push(match (host.as_str(), ns.as_str()) {
                        (OLLAMA_REGISTRY, "library") => format!("{model}:{tag}"),
                        (OLLAMA_REGISTRY, _) => format!("{ns}/{model}:{tag}"),
                        _ => format!("{host}/{ns}/{model}:{tag}"),
                    });
                }
            }
        }
    }
    out.sort();
    out
}

/// Which cache a located root is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MlRootKind {
    /// The Hugging Face hub cache (holds `models--*` / `datasets--*` repositories).
    HuggingFaceHub,
    /// The Hugging Face `datasets` library cache.
    HuggingFaceDatasets,
    /// The Ollama model store.
    Ollama,
    /// The PyTorch Hub cache.
    TorchHub,
}

impl MlRootKind {
    /// Every kind, in reporting order.
    pub const ALL: [MlRootKind; 4] = [
        MlRootKind::HuggingFaceHub,
        MlRootKind::HuggingFaceDatasets,
        MlRootKind::Ollama,
        MlRootKind::TorchHub,
    ];
}

impl fmt::Display for MlRootKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MlRootKind::HuggingFaceHub => "Hugging Face hub cache",
            MlRootKind::HuggingFaceDatasets => "Hugging Face datasets cache",
            MlRootKind::Ollama => "Ollama models",
            MlRootKind::TorchHub => "PyTorch Hub cache",
        })
    }
}

/// A cache root that exists at a validated location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MlRoot {
    /// Which cache.
    pub kind: MlRootKind,
    /// Its folder.
    pub path: PathBuf,
    /// Where the location came from.
    pub source: CacheSource,
}

/// A cache root that was refused (T-23): nothing in it is offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedMlRoot {
    /// Which cache.
    pub kind: MlRootKind,
    /// The refused location.
    pub path: PathBuf,
    /// Where it came from.
    pub source: CacheSource,
    /// Why.
    pub reason: String,
}

/// Everything [`locate_ml_caches`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MlScan {
    /// Roots that exist, in [`MlRootKind::ALL`] order.
    pub roots: Vec<MlRoot>,
    /// The artifacts in them (classify with [`DetectedArtifact::into_candidate`]).
    pub found: Vec<DetectedArtifact>,
    /// Roots refused; report them.
    pub refused: Vec<RefusedMlRoot>,
}

/// Locate the ML caches for `env` and detect the artifacts in them, read-only.
///
/// Missing caches are simply absent. Detection uses the bundled `huggingface.*`,
/// `ollama.models` and `torch.hub` rules (module docs).
pub fn locate_ml_caches(env: &CacheEnv, platform: &dyn Platform) -> MlScan {
    let mut out = MlScan::default();
    let Ok(rules) = RuleSet::bundled() else {
        return out;
    };
    let detector = |k: MlKind| {
        rules
            .get(k.rule_id())
            .map(|r| MlDetector::new(r.clone(), k))
    };
    for kind in MlRootKind::ALL {
        let (path, source) = match root_for(env, kind) {
            Ok(r) => r,
            Err(r) => {
                out.refused.push(*r);
                continue;
            }
        };
        let md = match platform.metadata(&path) {
            Ok(md) => md,
            Err(_) => continue,
        };
        if let Some(link) = link_below_anchor(env, &path, platform) {
            out.refused.push(RefusedMlRoot {
                kind,
                reason: format!(
                    "{} is a link; links are never followed, so the cache can't be shown to \
                     stay in the expected location",
                    link.display()
                ),
                path,
                source,
            });
            continue;
        }
        if md.kind != EntryKind::Dir {
            continue;
        }
        let note = Evidence::Note {
            text: format!("{kind} location from {source}"),
        };
        let mut found = Vec::new();
        match kind {
            MlRootKind::HuggingFaceHub => {
                let (Some(models), Some(datasets)) =
                    (detector(MlKind::HubModel), detector(MlKind::HubDataset))
                else {
                    continue;
                };
                for (name, k) in list(platform, &path).unwrap_or_default() {
                    if k != EntryKind::Dir {
                        continue;
                    }
                    let d = if name.starts_with("models--") {
                        &models
                    } else if name.starts_with("datasets--") {
                        &datasets
                    } else {
                        continue;
                    };
                    found.extend(d.finish(&d.rule, &path.join(&name), platform, true));
                }
            }
            MlRootKind::HuggingFaceDatasets => {
                found.extend(
                    detector(MlKind::DatasetsCache).and_then(|d| d.detect_located(&path, platform)),
                );
            }
            MlRootKind::Ollama => {
                found.extend(
                    detector(MlKind::Ollama).and_then(|d| d.detect_located(&path, platform)),
                );
            }
            MlRootKind::TorchHub => {
                found.extend(
                    detector(MlKind::TorchHub).and_then(|d| d.detect_located(&path, platform)),
                );
            }
        }
        for mut a in found {
            a.evidence.push(note.clone());
            out.found.push(a);
        }
        out.roots.push(MlRoot { kind, path, source });
    }
    out
}

/// The first link (symlink, junction, reparse point) on the way to `path`: every component
/// strictly below its anchor (the home folder when inside it, else the parent), and `path`
/// itself. Checked with no-follow metadata.
fn link_below_anchor(env: &CacheEnv, path: &Path, platform: &dyn Platform) -> Option<PathBuf> {
    let home = normalize(&env.home);
    let anchor = if path != home && path.starts_with(&home) {
        home
    } else {
        path.parent()?.to_path_buf()
    };
    let rest = path.strip_prefix(&anchor).ok()?;
    let mut at = anchor;
    for c in rest.components() {
        at.push(c.as_os_str());
        match platform.metadata(&at) {
            Ok(m) if m.kind.is_link() => return Some(at),
            Ok(_) => {}
            Err(_) => return Some(at), // unknown on the way: treat as unverifiable
        }
    }
    None
}

/// The configured or default root of `kind`, validated.
fn root_for(
    env: &CacheEnv,
    kind: MlRootKind,
) -> Result<(PathBuf, CacheSource), Box<RefusedMlRoot>> {
    // (variable, sub-path below its value)
    let vars: &[(&str, &[&str])] = match kind {
        MlRootKind::HuggingFaceHub => &[
            ("HF_HUB_CACHE", &[]),
            ("HUGGINGFACE_HUB_CACHE", &[]),
            ("HF_HOME", &["hub"]),
            ("XDG_CACHE_HOME", &["huggingface", "hub"]),
        ],
        MlRootKind::HuggingFaceDatasets => &[
            ("HF_DATASETS_CACHE", &[]),
            ("HF_HOME", &["datasets"]),
            ("XDG_CACHE_HOME", &["huggingface", "datasets"]),
        ],
        MlRootKind::Ollama => &[("OLLAMA_MODELS", &[])],
        MlRootKind::TorchHub => &[
            ("TORCH_HOME", &["hub"]),
            ("XDG_CACHE_HOME", &["torch", "hub"]),
        ],
    };
    for (var, sub) in vars {
        let Some(raw) = env.var(var) else {
            continue;
        };
        let source = CacheSource::Env {
            var: (*var).to_owned(),
        };
        // XDG variables must be absolute (relative values are ignored, as the spec requires).
        if *var == "XDG_CACHE_HOME" && env.abs_var(var).is_none() {
            continue;
        }
        let base = match expand(&raw, env) {
            Ok(p) => p,
            Err(why) => {
                return Err(Box::new(RefusedMlRoot {
                    kind,
                    path: PathBuf::from(&raw),
                    source,
                    reason: format!("`{raw}`: {why}"),
                }));
            }
        };
        let path = sub.iter().fold(base, |p, s| p.join(s));
        return validate(env, kind, path, source);
    }
    let home = &env.home;
    let path = match kind {
        MlRootKind::HuggingFaceHub => home.join(".cache").join("huggingface").join("hub"),
        MlRootKind::HuggingFaceDatasets => home.join(".cache").join("huggingface").join("datasets"),
        MlRootKind::Ollama => home.join(".ollama").join("models"),
        MlRootKind::TorchHub => home.join(".cache").join("torch").join("hub"),
    };
    validate(env, kind, path, CacheSource::Default)
}

/// T-23 location checks.
fn validate(
    env: &CacheEnv,
    kind: MlRootKind,
    path: PathBuf,
    source: CacheSource,
) -> Result<(PathBuf, CacheSource), Box<RefusedMlRoot>> {
    let refuse = |path: PathBuf, source: CacheSource, why: &str| {
        Err(Box::new(RefusedMlRoot {
            kind,
            reason: format!("{} {why}", path.display()),
            path,
            source,
        }))
    };
    if !path.is_absolute() {
        return refuse(path, source, "is not an absolute path");
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return refuse(path, source, "contains `.` or `..` components");
    }
    let norm = normalize(&path);
    if norm.parent().is_none() {
        return refuse(path, source, "is a filesystem root");
    }
    if normalize(&env.home).starts_with(&norm) {
        return refuse(path, source, "is the home folder or one of its ancestors");
    }
    if env.shared_folders().contains(&norm) {
        return refuse(
            path,
            source,
            "is a shared folder holding other programs' data, not a cache of its own",
        );
    }
    Ok((norm, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_and_repo_ids() {
        for k in MlKind::ALL {
            assert_eq!(MlKind::from_rule_id(k.rule_id()), Some(k));
        }
        assert_eq!(MlKind::from_rule_id("python.venv"), None);
        assert_eq!(
            hub_repo_id(Path::new("/c/models--google--gemma-2b")).as_deref(),
            Some("google/gemma-2b")
        );
        assert_eq!(
            hub_repo_id(Path::new("/c/datasets--squad")).as_deref(),
            Some("squad")
        );
    }
}
