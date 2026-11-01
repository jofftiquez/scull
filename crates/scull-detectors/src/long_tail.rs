//! Long-tail ecosystems (P5-DET-05, PRD §7.1): the checks data can't express for CMake build
//! folders, Bundler's `vendor/bundle`, Terraform, Bazel output bases, Podman storage and the
//! minikube cache. The other long-tail project folders (Meson, PlatformIO, SwiftPM, CocoaPods,
//! sbt, Bloop, Metals, Stack, Cabal, Mix, Zig) are plain rules; IDE caches are [`crate::ide`];
//! the global caches are located in [`crate::caches`].
//!
//! | Rule | Folder | Tier | Added here |
//! |---|---|---|---|
//! | `cmake.build` | a build folder with `CMakeCache.txt` and `CMakeFiles/` | **S1** | the source folder from `CMAKE_HOME_DIRECTORY` is the project; an in-source build is never a candidate |
//! | `bundler.vendor-bundle` | `<project>/vendor/bundle` | **S1** | must sit in `vendor/` |
//! | `terraform.dir` | `.terraform/` beside `*.tf` | **S1**; **S2** with a recorded backend or selected workspace | the S2 raise |
//! | `bazel.output-base` | `_bazel_<user>/<md5>/` | inspect-only | placement, the workspace from `DO_NOT_BUILD_HERE` |
//! | `podman.storage` | `containers/storage` | inspect-only | placement |
//! | `minikube.cache` | `.minikube/cache` | inspect-only | placement |
//!
//! Files are read as data through the [`ConfigReader`](crate::ConfigReader), never executed.
//! An unreadable file is unknown: whatever depended on it is skipped (the protective choice).

use crate::detector::{DetectContext, DetectedArtifact, Detector, Listing, detect_with_rule};
use crate::node::read_small;
use crate::rules::Rule;
use scull_core::{Evidence, FsPath, ProjectRef, Tier};
use scull_platform::{EntryKind, Platform};
use std::path::{Path, PathBuf};

/// What a long-tail rule detects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LongTailKind {
    /// A CMake build folder (`cmake.build`).
    CmakeBuild,
    /// Bundler's `vendor/bundle` (`bundler.vendor-bundle`).
    VendorBundle,
    /// A Terraform working folder (`terraform.dir`).
    Terraform,
    /// A Bazel output base (`bazel.output-base`).
    BazelOutputBase,
    /// Podman's container storage (`podman.storage`).
    PodmanStorage,
    /// minikube's download cache (`minikube.cache`).
    MinikubeCache,
}

impl LongTailKind {
    /// Every kind.
    pub const ALL: [LongTailKind; 6] = [
        LongTailKind::CmakeBuild,
        LongTailKind::VendorBundle,
        LongTailKind::Terraform,
        LongTailKind::BazelOutputBase,
        LongTailKind::PodmanStorage,
        LongTailKind::MinikubeCache,
    ];

    /// The rule ID.
    pub fn rule_id(self) -> &'static str {
        match self {
            LongTailKind::CmakeBuild => "cmake.build",
            LongTailKind::VendorBundle => "bundler.vendor-bundle",
            LongTailKind::Terraform => "terraform.dir",
            LongTailKind::BazelOutputBase => "bazel.output-base",
            LongTailKind::PodmanStorage => "podman.storage",
            LongTailKind::MinikubeCache => "minikube.cache",
        }
    }

    /// The kind for a rule ID.
    pub fn from_rule_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.rule_id() == id)
    }
}

/// The ecosystem detector for `rule`, if it is a long-tail rule that needs one.
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    LongTailKind::from_rule_id(rule.id())
        .map(|k| Box::new(LongTailDetector::new(rule.clone(), k)) as Box<dyn Detector>)
}

/// A long-tail rule plus the checks data can't express (module docs).
#[derive(Debug, Clone)]
pub struct LongTailDetector {
    rule: Rule,
    kind: LongTailKind,
}

impl LongTailDetector {
    /// A detector for `rule`, treated as `kind`.
    pub fn new(rule: Rule, kind: LongTailKind) -> Self {
        Self { rule, kind }
    }

    /// What it detects.
    pub fn kind(&self) -> LongTailKind {
        self.kind
    }
}

impl Detector for LongTailDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        let name = dir.file_name()?.to_str()?;
        let parent = dir.parent()?.file_name()?.to_str()?;
        // Cheap name checks before any I/O.
        let placed = match self.kind {
            LongTailKind::VendorBundle => parent == "vendor",
            LongTailKind::BazelOutputBase => {
                parent.starts_with("_bazel_") && is_lower_hex(name, 32)
            }
            LongTailKind::PodmanStorage => parent == "containers",
            LongTailKind::MinikubeCache => parent == ".minikube",
            LongTailKind::CmakeBuild | LongTailKind::Terraform => true,
        };
        if !placed {
            return None;
        }
        let (mut a, inside) = detect_with_rule(&self.rule, dir, platform)?;
        match self.kind {
            LongTailKind::CmakeBuild => cmake_build(&mut a, dir, platform, ctx)?,
            LongTailKind::Terraform => terraform(&mut a, &inside),
            LongTailKind::BazelOutputBase => bazel(&mut a, dir, platform, ctx),
            LongTailKind::PodmanStorage | LongTailKind::MinikubeCache => {
                a.projects.clear(); // tool-wide folders belong to no project
            }
            LongTailKind::VendorBundle => {}
        }
        Some(a)
    }
}

/// Whether `s` is exactly `len` lowercase hexadecimal digits.
pub(crate) fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The value of `KEY:TYPE=value` in a `CMakeCache.txt`.
pub fn cmake_cache_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        let (name, _ty) = k.split_once(':')?;
        (name.trim() == key)
            .then(|| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// `None` (not a candidate) for an in-source build. Otherwise the source folder recorded in
/// `CMakeCache.txt` becomes the project, with a note when it no longer exists.
fn cmake_build(
    a: &mut DetectedArtifact,
    dir: &Path,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Option<()> {
    let Some(reader) = ctx.config_reader else {
        return Some(());
    };
    let Some(text) = read_small(platform, reader, &dir.join("CMakeCache.txt")) else {
        return Some(());
    };
    let Some(source) = cmake_cache_value(&text, "CMAKE_HOME_DIRECTORY").map(PathBuf::from) else {
        return Some(());
    };
    if !source.is_absolute() {
        return Some(());
    }
    if same_path(&source, dir) {
        return None; // in-source build: the folder is the project's source tree
    }
    let exists = match platform.metadata(&source) {
        Ok(_) => true,
        Err(e) if e.is_not_found() => false,
        Err(_) => return Some(()), // unknown: say nothing
    };
    a.projects = vec![ProjectRef {
        root: FsPath::from_path(&source),
        name: source
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        ecosystem: "cmake".into(),
    }];
    a.evidence.push(Evidence::Note {
        text: if exists {
            format!("configured for the sources in {}", source.display())
        } else {
            format!(
                "configured for the sources in {}, which no longer exist: this build folder is \
                 a leftover",
                source.display()
            )
        },
    });
    Some(())
}

/// Lexical comparison that ignores a trailing separator and, on Windows, ASCII case and
/// `/` vs `\` (CMake writes forward slashes there).
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        let s = s.trim_end_matches('/').to_owned();
        if cfg!(windows) {
            s.to_ascii_lowercase()
        } else {
            s
        }
    };
    norm(a) == norm(b)
}

/// Raise `.terraform/` to S2 when it records a backend or a selected workspace.
fn terraform(a: &mut DetectedArtifact, inside: &Listing) {
    if inside.get("terraform.tfstate") == Some(&EntryKind::File) {
        a.base_tier = a.base_tier.max(Tier::S2);
        a.evidence.push(Evidence::Note {
            text: "records the backend configuration (.terraform/terraform.tfstate); \
                   re-initializing may need backend settings that were passed to \
                   `terraform init` on the command line"
                .into(),
        });
    }
    if inside.get("environment") == Some(&EntryKind::File) {
        a.base_tier = a.base_tier.max(Tier::S2);
        a.evidence.push(Evidence::Note {
            text: "records the selected workspace (.terraform/environment); without it the \
                   next run silently uses the `default` workspace"
                .into(),
        });
    }
}

/// The workspace an output base belongs to, from `DO_NOT_BUILD_HERE` (as data).
fn bazel(a: &mut DetectedArtifact, dir: &Path, platform: &dyn Platform, ctx: &DetectContext<'_>) {
    a.projects.clear();
    let Some(reader) = ctx.config_reader else {
        return;
    };
    let Some(text) = read_small(platform, reader, &dir.join("DO_NOT_BUILD_HERE")) else {
        return;
    };
    let workspace = PathBuf::from(text.trim());
    if text.trim().is_empty() || text.trim().contains('\n') || !workspace.is_absolute() {
        return;
    }
    let exists = match platform.metadata(&workspace) {
        Ok(_) => true,
        Err(e) if e.is_not_found() => false,
        Err(_) => return,
    };
    a.projects = vec![ProjectRef {
        root: FsPath::from_path(&workspace),
        name: workspace
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        ecosystem: "bazel".into(),
    }];
    a.evidence.push(Evidence::Note {
        text: if exists {
            format!(
                "output base of the workspace {}; run `bazel clean --expunge` there to remove it",
                workspace.display()
            )
        } else {
            format!(
                "output base of the workspace {}, which no longer exists: this output base is a \
                 leftover (Bazel's files in it are read-only; scull only shows it)",
                workspace.display()
            )
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmake_cache_values() {
        let text = "# This is the CMakeCache file.\n\
                    CMAKE_BUILD_TYPE:STRING=Release\n\
                    //Source directory\n\
                    CMAKE_HOME_DIRECTORY:INTERNAL=/src/app\n\
                    EMPTY:STRING=\n";
        assert_eq!(
            cmake_cache_value(text, "CMAKE_HOME_DIRECTORY").as_deref(),
            Some("/src/app")
        );
        assert_eq!(
            cmake_cache_value(text, "CMAKE_BUILD_TYPE").as_deref(),
            Some("Release")
        );
        assert_eq!(cmake_cache_value(text, "EMPTY"), None);
        assert_eq!(cmake_cache_value(text, "MISSING"), None);
    }

    #[test]
    fn hex_names() {
        assert!(is_lower_hex("0123456789abcdef0123456789abcdef", 32));
        assert!(!is_lower_hex("0123456789ABCDEF0123456789abcdef", 32));
        assert!(!is_lower_hex("0123456789abcdef", 32));
        assert!(!is_lower_hex("0123456789abcdef0123456789abcdeg", 32));
    }

    #[test]
    fn same_path_ignores_trailing_separator() {
        assert!(same_path(Path::new("/a/b/"), Path::new("/a/b")));
        assert!(!same_path(Path::new("/a/b"), Path::new("/a/c")));
    }

    #[test]
    fn kinds_round_trip() {
        for k in LongTailKind::ALL {
            assert_eq!(LongTailKind::from_rule_id(k.rule_id()), Some(k));
        }
        assert_eq!(LongTailKind::from_rule_id("meson.build"), None);
    }
}
