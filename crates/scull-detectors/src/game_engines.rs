//! Game engines (P5-DET-04, PRD §7.1, FX-CLS-14): Unity and Unreal Engine project folders.
//!
//! | Rule | Folder | Tier |
//! |---|---|---|
//! | `unity.library` | `Library/` (asset import cache) | **S1**: a full re-import, packages re-downloaded |
//! | `unity.temp` | `Temp/` | **S0**; **S2** with scene backups (`__Backupscenes/`) |
//! | `unity.obj` | `obj/` beside the generated `*.csproj` files | **S0** |
//! | `unreal.intermediate` | `Intermediate/` | **S1**: a rebuild |
//! | `unreal.binaries` | `Binaries/` | **S1**; **S2** without a `Source/` folder |
//! | `unreal.derived-data-cache` | `DerivedDataCache/` (project-local) | **S1**: shaders and derived data rebuilt |
//! | `unreal.saved` | `Saved/` | **S2**: settings, autosaves, screenshots, save games |
//!
//! Every rule is gated on the engine's project markers beside the folder (CLS-03): Unity's
//! `ProjectSettings/` and `Assets/`, Unreal's `.uproject`. [`GameEngineDetector`] adds what data
//! can't express:
//!
//! * Unity: `ProjectSettings/` must be a directory holding `ProjectVersion.txt`, and `Assets/` a
//!   directory. While the editor has the project open it holds `Temp/UnityLockfile`, so that
//!   lock is added to every Unity folder's activity lock files. Scene backups in `Temp/`
//!   (`__Backupscenes/`) are what the editor restores unsaved scenes from after a crash: S2.
//! * Unreal: the `.uproject` must be a regular file. Without `Source/` beside it, `Binaries/`
//!   can't be rebuilt from the project (a Blueprint-only copy, or binaries handed over by a
//!   programmer): S2. `Saved/` notes the user data it holds.
//!
//! The folders that hold the user's work (`Assets/`, `Packages/`, `ProjectSettings/`,
//! `UserSettings/`; `Content/`, `Config/`, `Source/`, `Plugins/`) are never matched by any rule.

use crate::detector::{DetectContext, DetectedArtifact, Detector, Listing, detect_with_rule, list};
use crate::rules::Rule;
use scull_core::{Evidence, Tier};
use scull_platform::{EntryKind, Platform};
use std::path::Path;

/// Rule ID of Unity's `Library/`.
pub const UNITY_LIBRARY_RULE_ID: &str = "unity.library";
/// Rule ID of Unity's `Temp/`.
pub const UNITY_TEMP_RULE_ID: &str = "unity.temp";
/// Rule ID of `obj/` in a Unity project.
pub const UNITY_OBJ_RULE_ID: &str = "unity.obj";
/// Rule ID of Unreal's `Intermediate/`.
pub const UNREAL_INTERMEDIATE_RULE_ID: &str = "unreal.intermediate";
/// Rule ID of Unreal's `Binaries/`.
pub const UNREAL_BINARIES_RULE_ID: &str = "unreal.binaries";
/// Rule ID of Unreal's project-local `DerivedDataCache/`.
pub const UNREAL_DDC_RULE_ID: &str = "unreal.derived-data-cache";
/// Rule ID of Unreal's `Saved/`.
pub const UNREAL_SAVED_RULE_ID: &str = "unreal.saved";

/// The file Unity writes into every project's `ProjectSettings/`.
const UNITY_VERSION_FILE: &str = "ProjectVersion.txt";

/// The lock file the Unity editor holds in `Temp/` while it has the project open.
const UNITY_LOCKFILE: &str = "UnityLockfile";

/// Folders in Unreal's `Saved/` that hold user data rather than regenerable output.
const UNREAL_USER_DATA: &[&str] = &[
    "Autosaves",
    "Config",
    "Screenshots",
    "SaveGames",
    "Collections",
    "Backup",
];

/// Which engine a rule belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Unity,
    Unreal,
}

impl Engine {
    fn of(rule_id: &str) -> Option<Self> {
        match rule_id {
            UNITY_LIBRARY_RULE_ID | UNITY_TEMP_RULE_ID | UNITY_OBJ_RULE_ID => Some(Self::Unity),
            UNREAL_INTERMEDIATE_RULE_ID
            | UNREAL_BINARIES_RULE_ID
            | UNREAL_DDC_RULE_ID
            | UNREAL_SAVED_RULE_ID => Some(Self::Unreal),
            _ => None,
        }
    }
}

/// The ecosystem detector for `rule`, if it is a bundled game-engine rule.
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    Engine::of(rule.id()).map(|engine| {
        Box::new(GameEngineDetector {
            rule: rule.clone(),
            engine,
        }) as Box<dyn Detector>
    })
}

/// A Unity or Unreal rule plus the checks data can't express (module docs).
#[derive(Debug, Clone)]
pub struct GameEngineDetector {
    rule: Rule,
    engine: Engine,
}

impl GameEngineDetector {
    /// A detector for one of the bundled `unity.*` or `unreal.*` rules; `None` for any other
    /// rule.
    pub fn new(rule: Rule) -> Option<Self> {
        let engine = Engine::of(rule.id())?;
        Some(Self { rule, engine })
    }
}

impl Detector for GameEngineDetector {
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
        let project = dir.parent()?;
        let beside = list(platform, project)?;
        match self.engine {
            Engine::Unity => {
                if !unity_project(project, &beside, platform) {
                    return None;
                }
                if self.rule.id() == UNITY_TEMP_RULE_ID {
                    unity_temp(&mut a, &inside);
                }
                let lock = project.join("Temp").join(UNITY_LOCKFILE);
                if beside.get("Temp") == Some(&EntryKind::Dir)
                    && !a.activity_lock_files.contains(&lock)
                {
                    a.activity_lock_files.push(lock);
                }
            }
            Engine::Unreal => {
                if !unreal_project(&beside) {
                    return None;
                }
                match self.rule.id() {
                    UNREAL_BINARIES_RULE_ID => unreal_binaries(&mut a, &beside),
                    UNREAL_SAVED_RULE_ID => unreal_saved(&mut a, &inside),
                    _ => {}
                }
            }
        }
        Some(a)
    }
}

/// A real Unity project: `ProjectSettings/` (a directory holding `ProjectVersion.txt`) and
/// `Assets/` (a directory) in `project`. Anything that can't be listed is not a project.
fn unity_project(project: &Path, beside: &Listing, platform: &dyn Platform) -> bool {
    if beside.get("ProjectSettings") != Some(&EntryKind::Dir)
        || beside.get("Assets") != Some(&EntryKind::Dir)
    {
        return false;
    }
    list(platform, &project.join("ProjectSettings"))
        .is_some_and(|s| s.get(UNITY_VERSION_FILE) == Some(&EntryKind::File))
}

/// Scene backups in `Temp/` are the editor's crash recovery copy of unsaved scenes.
fn unity_temp(a: &mut DetectedArtifact, inside: &Listing) {
    if inside.contains_key("__Backupscenes") {
        a.base_tier = a.base_tier.max(Tier::S2);
        a.evidence.push(Evidence::Note {
            text: "holds scene backups (Temp/__Backupscenes/), which the Unity editor restores \
                   unsaved scenes from after a crash; open the project first if you may have \
                   unsaved work"
                .into(),
        });
    }
}

/// A real Unreal project: a `.uproject` regular file in `project`.
fn unreal_project(beside: &Listing) -> bool {
    beside
        .iter()
        .any(|(n, k)| *k == EntryKind::File && n.ends_with(".uproject") && n.len() > 9)
}

/// `Binaries/` can only be rebuilt from the project's `Source/`.
fn unreal_binaries(a: &mut DetectedArtifact, beside: &Listing) {
    if beside.get("Source") != Some(&EntryKind::Dir) {
        a.base_tier = a.base_tier.max(Tier::S2);
        a.evidence.push(Evidence::Note {
            text: "the project has no Source/ folder, so these binaries can't be rebuilt from \
                   it (they may have been built elsewhere and handed over)"
                .into(),
        });
    }
}

/// Name the user data `Saved/` holds (it is S2 whatever it holds).
fn unreal_saved(a: &mut DetectedArtifact, inside: &Listing) {
    let found: Vec<&str> = UNREAL_USER_DATA
        .iter()
        .copied()
        .filter(|n| inside.contains_key(*n))
        .collect();
    if !found.is_empty() {
        a.evidence.push(Evidence::Note {
            text: format!(
                "holds user data that can't be regenerated: {}/",
                found.join("/, ")
            ),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(e: &[(&str, EntryKind)]) -> Listing {
        e.iter().map(|(n, k)| ((*n).to_owned(), *k)).collect()
    }

    #[test]
    fn engine_is_keyed_by_exact_rule_id() {
        assert_eq!(Engine::of("unity.library"), Some(Engine::Unity));
        assert_eq!(Engine::of("unreal.saved"), Some(Engine::Unreal));
        assert_eq!(Engine::of("community.unity-library"), None);
        assert_eq!(Engine::of("unity.other"), None);
    }

    #[test]
    fn uproject_must_be_a_named_regular_file() {
        assert!(unreal_project(&l(&[("Game.uproject", EntryKind::File)])));
        assert!(!unreal_project(&l(&[("Game.uproject", EntryKind::Dir)])));
        assert!(!unreal_project(&l(&[(
            "Game.uproject",
            EntryKind::Symlink
        )])));
        assert!(!unreal_project(&l(&[(".uproject", EntryKind::File)])));
        assert!(!unreal_project(&l(&[(
            "Game.uproject.bak",
            EntryKind::File
        )])));
    }
}
