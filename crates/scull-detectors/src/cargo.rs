//! Rust: shared Cargo target directories (`CARGO_TARGET_DIR`, `[build] target-dir`), a case the
//! declarative format can't express (CLS-02). Project-local `target/` directories are the
//! bundled `rust.target` rule, driven by [`crate::RuleDetector`].
//!
//! A shared target directory lives outside the projects that build into it, so no marker sits
//! beside it. Its evidence is instead:
//!
//! 1. a **declaration**: a project's Cargo config (`.cargo/config.toml` or `.cargo/config` in
//!    the project or an ancestor, read as data through a [`ConfigReader`], never executed), or an
//!    explicit path the caller passes in (e.g. from `CARGO_TARGET_DIR`; reading the environment
//!    is the caller's job); **and**
//! 2. Cargo's `CACHEDIR.TAG` inside it (signature-checked when a reader is available), and no
//!    project or repository markers (`Cargo.toml`, `Cargo.lock`, `package.json`, `.git`) inside
//!    it; it must not contain any project that declared it.
//!
//! Each directory becomes **one** artifact attributed to every project that uses it, so its
//! bytes are counted once (FX-CLS-04). Usage: call [`SharedTargetDirs::add_project`] for each
//! Cargo project found and [`SharedTargetDirs::add_explicit`] for caller-supplied paths, then
//! [`SharedTargetDirs::finish`] once with the artifacts the per-directory walk produced; a
//! project-local `target/` that other projects also build into is merged into the shared
//! artifact instead of being reported twice.

use crate::detector::{
    ConfigReader, DetectContext, DetectedArtifact, DetectorSignals, Listing, MAX_CONFIG_BYTES,
    list, project_ref,
};
use crate::rules::Strategy;
use scull_core::{
    Category, Evidence, FileIdentity, FsPath, ProjectRef, Regeneration, RegenerationCost, RuleRef,
    Tier,
};
use scull_platform::{EntryKind, Platform};
use scull_safety::Regen;
use std::path::{Component, Path, PathBuf};

/// Rule ID recorded on shared target directory artifacts.
pub const SHARED_TARGET_RULE_ID: &str = "rust.shared-target";
/// Version of the shared target directory detector; bumps when matching or tiering changes.
pub const SHARED_TARGET_RULE_VERSION: u32 = 1;

/// First line of the `CACHEDIR.TAG` Cargo writes (and the cachedir spec requires).
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Entries that mean a directory is (or holds) a project or repository, never a target dir.
const FOREIGN_MARKERS: &[&str] = &["Cargo.toml", "Cargo.lock", "package.json", ".git"];

/// Where a shared target directory was declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetDirSource {
    /// Supplied by the caller, e.g. the `CARGO_TARGET_DIR` environment variable.
    Explicit {
        /// What supplied it, for the evidence (e.g. `CARGO_TARGET_DIR`).
        label: String,
    },
    /// `[build] target-dir` in a Cargo config file.
    CargoConfig {
        /// The config file.
        config: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoProject {
    root: PathBuf,
    manifest: PathBuf,
    lockfile: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct Declared {
    /// Lexically normalized absolute path.
    path: PathBuf,
    projects: Vec<CargoProject>,
    sources: Vec<TargetDirSource>,
}

/// Collects shared Cargo target directories and the projects that use them.
#[derive(Debug, Clone, Default)]
pub struct SharedTargetDirs {
    declared: Vec<Declared>,
}

impl SharedTargetDirs {
    /// An empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a target directory supplied by the caller (e.g. `CARGO_TARGET_DIR`, already
    /// resolved against the caller's working directory). `label` names the source in the
    /// evidence. Returns `false`, recording nothing, if `path` is not absolute.
    ///
    /// No project is attributed from an explicit path alone, so without a declaring project
    /// the artifact has no lockfile to reproduce from and classifies as S2.
    pub fn add_explicit(&mut self, path: impl AsRef<Path>, label: impl Into<String>) -> bool {
        let path = path.as_ref();
        if !path.is_absolute() {
            return false;
        }
        let source = TargetDirSource::Explicit {
            label: label.into(),
        };
        self.entry(normalize(path)).sources.push(source);
        true
    }

    /// Look up the Cargo config of the project at `project_root` (a directory containing
    /// `Cargo.toml`) and, if it redirects the target directory, attribute that directory to the
    /// project. Returns the redirected directory.
    ///
    /// Config files are looked up like Cargo does, nearest first: `.cargo/config` then
    /// `.cargo/config.toml` in the project root and each ancestor; the nearest file that sets
    /// `[build] target-dir` wins, and a relative value is relative to the directory holding
    /// `.cargo/`. The search stops (returning `None`) at the first config file that exists but
    /// can't be read or parsed, since Cargo would not build with it either. Needs
    /// [`DetectContext::config_reader`]; without one nothing is read.
    pub fn add_project(
        &mut self,
        project_root: &Path,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
    ) -> Option<PathBuf> {
        let reader = ctx.config_reader?;
        let manifest = project_root.join("Cargo.toml");
        if !is_file(platform, &manifest) {
            return None;
        }
        let (config, value) = find_target_dir_config(project_root, platform, reader)?;
        let base = config.parent()?.parent()?; // <base>/.cargo/config.toml
        let raw = base.join(&value);
        let path = normalize(&raw);
        if path != raw && !same_object(platform, &raw, &path) {
            // `..` through a symlinked component resolves elsewhere than the lexical path.
            return None;
        }
        let lock = project_root.join("Cargo.lock");
        let project = CargoProject {
            root: project_root.to_path_buf(),
            manifest,
            lockfile: is_file(platform, &lock).then_some(lock),
        };
        let entry = self.entry(path.clone());
        if !entry.projects.iter().any(|p| p.root == project.root) {
            entry.projects.push(project);
        }
        let source = TargetDirSource::CargoConfig { config };
        if !entry.sources.contains(&source) {
            entry.sources.push(source);
        }
        Some(path)
    }

    /// Whether `dir` was declared as a shared target directory (lexical comparison).
    pub fn contains(&self, dir: &Path) -> bool {
        let dir = normalize(dir);
        self.declared.iter().any(|d| d.path == dir)
    }

    /// Validate every declared directory and report it in `found`, the artifacts the
    /// per-directory walk produced: one artifact per directory (merged by file identity when the
    /// platform provides it, else by normalized path), attributed to every project that uses
    /// it. A project-local artifact for the same directory is replaced by the shared one, which
    /// also lists the local project; if only that same project declared it (e.g.
    /// `target-dir = "target"`), the local artifact is kept as is.
    ///
    /// Tier inputs: lockfile present only if at least one project uses the directory and every
    /// such project has a `Cargo.lock` (so an explicit path alone gives S2); regeneration is
    /// local for one project and costly for several, since each must rebuild (S1).
    pub fn finish(
        self,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
        found: &mut Vec<DetectedArtifact>,
    ) {
        // Validate, then merge entries that are the same directory under different paths.
        let mut valid: Vec<(Declared, Option<FileIdentity>)> = Vec::new();
        for d in self.declared {
            let Some(identity) = validate(&d, platform, ctx) else {
                continue;
            };
            match valid
                .iter_mut()
                .find(|(v, id)| v.path == d.path || (id.is_some() && *id == identity))
            {
                Some((v, _)) => {
                    for p in d.projects {
                        if !v.projects.iter().any(|q| q.root == p.root) {
                            v.projects.push(p);
                        }
                    }
                    for s in d.sources {
                        if !v.sources.contains(&s) {
                            v.sources.push(s);
                        }
                    }
                }
                None => valid.push((d, identity)),
            }
        }

        for (mut d, identity) in valid {
            let local = found.iter().position(|a| {
                a.path == d.path
                    || normalize(&a.path) == d.path
                    || (identity.is_some() && a.identity == identity)
            });
            let mut projects: Vec<ProjectRef> = d
                .projects
                .iter()
                .map(|p| project_ref(&p.root, "rust"))
                .collect();
            if let Some(i) = local {
                let only_self = d
                    .sources
                    .iter()
                    .all(|s| matches!(s, TargetDirSource::CargoConfig { .. }))
                    && !found[i].projects.is_empty()
                    && projects.iter().all(|p| found[i].projects.contains(p));
                if only_self {
                    continue;
                }
                for p in &found[i].projects {
                    if !projects.contains(p) {
                        projects.push(p.clone());
                    }
                }
                // The local project counts as a user too; its lockfile decides like the others'.
                for p in &found[i].projects {
                    let root = p.root.to_path_buf();
                    if !d.projects.iter().any(|q| q.root == root) {
                        let lock = root.join("Cargo.lock");
                        d.projects.push(CargoProject {
                            manifest: root.join("Cargo.toml"),
                            lockfile: is_file(platform, &lock).then_some(lock),
                            root,
                        });
                    }
                }
            }
            projects.sort_by(|a, b| a.root.cmp(&b.root));
            d.projects.sort_by(|a, b| a.root.cmp(&b.root));
            let artifact = shared_artifact(&d, identity, projects);
            match local {
                Some(i) => found[i] = artifact,
                None => found.push(artifact),
            }
        }
    }

    fn entry(&mut self, path: PathBuf) -> &mut Declared {
        match self.declared.iter().position(|d| d.path == path) {
            Some(i) => &mut self.declared[i],
            None => {
                self.declared.push(Declared {
                    path,
                    projects: Vec::new(),
                    sources: Vec::new(),
                });
                self.declared.last_mut().expect("just pushed")
            }
        }
    }
}

/// Checks a declared directory is a plausible Cargo target directory and safe to report.
/// Returns its identity (`Some(None)` when the platform has none), or `None` to drop it.
fn validate(
    d: &Declared,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Option<Option<FileIdentity>> {
    // A filesystem root is never a target directory.
    d.path.parent()?;
    // It must not be, or contain, a project that uses it or the config that declared it.
    let contains = |p: &Path| p.starts_with(&d.path);
    if d.projects.iter().any(|p| contains(&p.root))
        || d.sources.iter().any(|s| match s {
            TargetDirSource::CargoConfig { config } => contains(config),
            TargetDirSource::Explicit { .. } => false,
        })
    {
        return None;
    }
    let md = platform.metadata(&d.path).ok()?;
    if md.kind != EntryKind::Dir {
        return None; // links are never followed
    }
    let inside: Listing = list(platform, &d.path)?;
    if inside.get("CACHEDIR.TAG") != Some(&EntryKind::File) {
        return None;
    }
    if FOREIGN_MARKERS.iter().any(|m| inside.contains_key(*m)) {
        return None;
    }
    if let Some(reader) = ctx.config_reader {
        let tag = d.path.join("CACHEDIR.TAG");
        let text = read_small(platform, reader, &tag)?;
        if !text.starts_with(CACHEDIR_SIGNATURE) {
            return None;
        }
    }
    Some(md.identity)
}

fn shared_artifact(
    d: &Declared,
    identity: Option<FileIdentity>,
    projects: Vec<ProjectRef>,
) -> DetectedArtifact {
    let mut evidence = vec![Evidence::Marker {
        path: FsPath::from_path(&d.path.join("CACHEDIR.TAG")),
    }];
    for s in &d.sources {
        let text = match s {
            TargetDirSource::Explicit { label } => {
                format!("named as a Cargo target directory by {label}")
            }
            TargetDirSource::CargoConfig { config } => format!(
                "declared as the Cargo target directory by `[build] target-dir` in {}",
                config.display()
            ),
        };
        evidence.push(Evidence::Note { text });
    }
    for p in &d.projects {
        evidence.push(Evidence::Marker {
            path: FsPath::from_path(&p.manifest),
        });
        if let Some(l) = &p.lockfile {
            evidence.push(Evidence::Lockfile {
                path: FsPath::from_path(l),
            });
        }
    }
    let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
    if !projects.is_empty() {
        evidence.push(Evidence::Note {
            text: format!(
                "shared by {} project(s): {}; its bytes are counted once, on this item",
                projects.len(),
                names.join(", ")
            ),
        });
    }
    evidence.push(Evidence::KnownArtifact {
        artifact: "Shared Cargo build output".into(),
    });

    let lockfile_present =
        !d.projects.is_empty() && d.projects.iter().all(|p| p.lockfile.is_some());
    let regen = if d.projects.len() <= 1 {
        Regen::Local
    } else {
        Regen::NetworkOrLong
    };
    let instructions = if names.is_empty() {
        "Run `cargo build` in each project that uses this target directory.".to_owned()
    } else {
        format!(
            "Run `cargo build` in each project that uses it: {}.",
            names.join(", ")
        )
    };
    DetectedArtifact {
        path: d.path.clone(),
        identity,
        rule: RuleRef {
            id: SHARED_TARGET_RULE_ID.into(),
            version: SHARED_TARGET_RULE_VERSION,
        },
        category: Category::ProjectArtifact,
        artifact: "Shared Cargo build output".into(),
        strategy: Strategy::Filesystem,
        base_tier: Tier::S0,
        projects,
        evidence,
        regeneration: Regeneration {
            instructions: Some(instructions),
            cost: RegenerationCost::Unknown,
        },
        signals: DetectorSignals {
            marker_present: true,
            lockfile_present,
            artifact_known: true,
            regen,
        },
        contains_links: None,
        activity_lock_files: ["debug", "release"]
            .iter()
            .map(|p| d.path.join(p).join(".cargo-lock"))
            .collect(),
        activity_processes: ["cargo", "rustc", "rust-analyzer"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        tool_action: None,
    }
}

/// The nearest Cargo config setting `[build] target-dir`: `(config file, value)`.
fn find_target_dir_config(
    project_root: &Path,
    platform: &dyn Platform,
    reader: &dyn ConfigReader,
) -> Option<(PathBuf, String)> {
    for dir in project_root.ancestors() {
        // Cargo reads `config` when both exist.
        for name in ["config", "config.toml"] {
            let path = dir.join(".cargo").join(name);
            let Ok(md) = platform.metadata(&path) else {
                continue; // absent (or unreadable metadata): nothing to read
            };
            if md.kind != EntryKind::File || md.logical_size > MAX_CONFIG_BYTES {
                return None;
            }
            let text = reader.read_text(&path, MAX_CONFIG_BYTES)?;
            match parse_target_dir(&text) {
                Ok(Some(value)) => return Some((path, value)),
                Ok(None) => break, // this directory's config doesn't redirect; go up
                Err(()) => return None,
            }
        }
    }
    None
}

/// `[build] target-dir` from a Cargo config file's text. `Err` if the text isn't valid TOML or
/// `target-dir` isn't a non-empty string. Pure: the config is data, never executed.
pub(crate) fn parse_target_dir(text: &str) -> Result<Option<String>, ()> {
    let table: toml::Table = text.parse().map_err(|_| ())?;
    let Some(build) = table.get("build") else {
        return Ok(None);
    };
    let build = build.as_table().ok_or(())?;
    match build.get("target-dir") {
        None => Ok(None),
        Some(toml::Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(_) => Err(()),
    }
}

fn read_small(platform: &dyn Platform, reader: &dyn ConfigReader, path: &Path) -> Option<String> {
    let md = platform.metadata(path).ok()?;
    if md.kind != EntryKind::File || md.logical_size > MAX_CONFIG_BYTES {
        return None;
    }
    reader.read_text(path, MAX_CONFIG_BYTES)
}

fn is_file(platform: &dyn Platform, path: &Path) -> bool {
    platform
        .metadata(path)
        .is_ok_and(|m| m.kind == EntryKind::File)
}

/// Whether two paths name the same filesystem object. With no identity available (the
/// platform can't tell) the lexical path is trusted, which is how Windows resolves `..`.
fn same_object(platform: &dyn Platform, a: &Path, b: &Path) -> bool {
    match (platform.metadata(a), platform.metadata(b)) {
        (Ok(ma), Ok(mb)) => match (ma.identity, mb.identity) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        },
        _ => false,
    }
}

/// Resolve `.` and `..` lexically (`..` at the root stays at the root).
pub(crate) fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Nothing to pop at the root / prefix: stay there.
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_target_dir_as_data() {
        assert_eq!(
            parse_target_dir("[build]\ntarget-dir = \"../shared\"\n"),
            Ok(Some("../shared".into()))
        );
        assert_eq!(parse_target_dir("[build]\njobs = 4\n"), Ok(None));
        assert_eq!(parse_target_dir("[alias]\nb = \"build\"\n"), Ok(None));
        assert_eq!(parse_target_dir(""), Ok(None));
        assert_eq!(parse_target_dir("[build]\ntarget-dir = \"\"\n"), Err(()));
        assert_eq!(parse_target_dir("[build]\ntarget-dir = 3\n"), Err(()));
        assert_eq!(parse_target_dir("build = 1\n"), Err(()));
        assert_eq!(parse_target_dir("not toml ["), Err(()));
    }

    #[test]
    fn normalizes_lexically() {
        let n = |s: &str| normalize(Path::new(s));
        assert_eq!(n("/a/b/../c"), PathBuf::from("/a/c"));
        assert_eq!(n("/a/./b/"), PathBuf::from("/a/b"));
        assert_eq!(n("/../x"), PathBuf::from("/x"));
    }
}
