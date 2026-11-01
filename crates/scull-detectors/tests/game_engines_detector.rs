//! Unity and Unreal Engine fixtures (P5-DET-04, PRD §7.1, FX-CLS-14): Unity `Library/` (S1),
//! `Temp/` (S0; S2 with scene backups), `obj/` (S0); Unreal `Intermediate/`, `Binaries/` and
//! `DerivedDataCache/` (S1; `Binaries/` S2 without `Source/`), `Saved/` (S2). Every folder is
//! gated on the engine's project markers (CLS-03), the folders holding the user's work are never
//! candidates, and engine, plugin and look-alike folders are left alone.
//!
//! Every test builds its tree with `scull-fixtures`, detects read-only through `scull-platform`,
//! and checks the universal invariants (I-2 canary, I-3 tree unchanged, I-1 on the would-be
//! plan).

#![allow(clippy::unwrap_used)]

mod common;

use common::{
    age_tree, check, classify_expect, clean_caller, identity_available, plan_inputs, rel, scan,
};
use scull_core::{Action, Candidate, Evidence, Tier};
use scull_detectors::DetectedArtifact;
use scull_fixtures::{Fixture, days_ago};
use scull_safety::Regen;
use std::path::Path;

/// Everything found under `root`, as `(rule, relative path)`, sorted.
fn found(root: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = scan(root, &[])
        .found
        .iter()
        .map(|a| (a.rule.id.clone(), rel(root, &a.path)))
        .collect();
    v.sort();
    v
}

fn at(root: &Path, rel_path: &str) -> DetectedArtifact {
    scan(root, &[]).at(root, rel_path).clone()
}

fn notes(a: &DetectedArtifact) -> Vec<&str> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The candidate for an idle, clean artifact, expecting `tier` (S3 where the platform has no
/// file identity). Unlike `classify_expect`, this includes the rule's base tier floor.
fn candidate(a: &DetectedArtifact, tier: Tier) -> Candidate {
    let c = a.clone().into_candidate(&clean_caller(a));
    assert!(c.is_consistent());
    let want = if identity_available() { tier } else { Tier::S3 };
    assert_eq!(c.tier, want, "{}", a.path.display());
    c
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

/// A Unity project at `dir`: user work (Assets, Packages, ProjectSettings, UserSettings), the
/// generated C# projects, and `Library/` with the asset database.
fn unity_project(fx: &mut Fixture, dir: &str) {
    fx.file(
        format!("{dir}/Assets/Scripts/Player.cs"),
        "class Player {}\n",
    )
    .unwrap()
    .file(format!("{dir}/Assets/Scripts/Player.cs.meta"), "guid: 1\n")
    .unwrap()
    .file_of_size(format!("{dir}/Assets/Art/hero.png"), 4096)
    .unwrap()
    .file(format!("{dir}/Packages/manifest.json"), "{}\n")
    .unwrap()
    .file(
        format!("{dir}/ProjectSettings/ProjectVersion.txt"),
        "m_EditorVersion: 6000.0.23f1\n",
    )
    .unwrap()
    .file(
        format!("{dir}/ProjectSettings/ProjectSettings.asset"),
        "%YAML 1.1\n",
    )
    .unwrap()
    .file(
        format!("{dir}/UserSettings/EditorUserSettings.asset"),
        "x\n",
    )
    .unwrap()
    .file(format!("{dir}/Assembly-CSharp.csproj"), "<Project/>\n")
    .unwrap()
    .file(format!("{dir}/kart.sln"), "\n")
    .unwrap()
    .file_of_size(format!("{dir}/Library/ArtifactDB"), 8192)
    .unwrap()
    .file_of_size(format!("{dir}/Library/SourceAssetDB"), 4096)
    .unwrap()
    .file_of_size(format!("{dir}/Library/Artifacts/0a/0a1b2c"), 4096)
    .unwrap()
    .file_of_size(
        format!("{dir}/Library/ScriptAssemblies/Assembly-CSharp.dll"),
        2048,
    )
    .unwrap()
    .file_of_size(
        format!("{dir}/Library/PackageCache/com.unity.ugui@2.0.0/package.json"),
        512,
    )
    .unwrap();
}

/// An Unreal C++ project at `dir`: user work (Content, Config, Source, Plugins), the
/// `.uproject`, and the generated folders.
fn unreal_project(fx: &mut Fixture, dir: &str) {
    fx.file(format!("{dir}/Shooter.uproject"), "{\"FileVersion\": 3}\n")
        .unwrap()
        .file_of_size(format!("{dir}/Content/Maps/Level01.umap"), 8192)
        .unwrap()
        .file(
            format!("{dir}/Config/DefaultEngine.ini"),
            "[/Script/Engine]\n",
        )
        .unwrap()
        .file(format!("{dir}/Source/Shooter/Shooter.cpp"), "// game\n")
        .unwrap()
        .file(
            format!("{dir}/Source/Shooter.Target.cs"),
            "public class ShooterTarget {}\n",
        )
        .unwrap()
        .file_of_size(
            format!("{dir}/Binaries/Win64/UnrealEditor-Shooter.dll"),
            4096,
        )
        .unwrap()
        .file(format!("{dir}/Binaries/Win64/UnrealEditor.modules"), "{}\n")
        .unwrap()
        .file_of_size(
            format!("{dir}/Intermediate/Build/Win64/UnrealEditor/Development/x.obj"),
            4096,
        )
        .unwrap()
        .file(
            format!("{dir}/Intermediate/ProjectFiles/Shooter.vcxproj"),
            "<x/>\n",
        )
        .unwrap()
        .file(format!("{dir}/Intermediate/TargetInfo.json"), "{}\n")
        .unwrap()
        .file_of_size(format!("{dir}/DerivedDataCache/0/1/2/abc.udd"), 8192)
        .unwrap()
        .file(
            format!("{dir}/Saved/Config/WindowsEditor/EditorPerProjectUserSettings.ini"),
            "[x]\n",
        )
        .unwrap()
        .file_of_size(
            format!("{dir}/Saved/Autosaves/Game/Maps/Level01_Auto1.umap"),
            4096,
        )
        .unwrap()
        .file_of_size(
            format!("{dir}/Saved/Screenshots/WindowsEditor/Shot00001.png"),
            2048,
        )
        .unwrap()
        .file(format!("{dir}/Saved/Logs/Shooter.log"), "log\n")
        .unwrap()
        // A plugin: its own binaries (possibly shipped without source) and intermediates.
        .file(format!("{dir}/Plugins/Fx/Fx.uplugin"), "{}\n")
        .unwrap()
        .file_of_size(
            format!("{dir}/Plugins/Fx/Binaries/Win64/UnrealEditor-Fx.dll"),
            2048,
        )
        .unwrap()
        .file_of_size(
            format!("{dir}/Plugins/Fx/Intermediate/Build/Win64/x.obj"),
            1024,
        )
        .unwrap();
}

/// Folders holding the user's work: nothing at or under them is ever a candidate.
const USER_WORK: &[&str] = &[
    "kart/Assets",
    "kart/Packages",
    "kart/ProjectSettings",
    "kart/UserSettings",
    "shooter/Content",
    "shooter/Config",
    "shooter/Source",
    "shooter/Plugins",
];

// ---------------------------------------------------------------------------------------------
// FX-CLS-14

#[test]
fn fx_cls_14_unity_library_and_unreal_ddc_are_s1_saved_is_s2() {
    let mut fx = Fixture::new().unwrap();
    unity_project(&mut fx, "kart");
    fx.file_of_size("kart/obj/Debug/Assembly-CSharp.dll", 2048)
        .unwrap()
        .file("kart/obj/project.assets.json", "{}")
        .unwrap()
        // Not written by Unity (its C# projects build into Temp/): no one's candidate.
        .file_of_size("kart/bin/Debug/Assembly-CSharp.dll", 1024)
        .unwrap();
    unreal_project(&mut fx, "shooter");
    // A C# tool project beside the .uproject must not make bin/ or obj/ a .NET candidate.
    fx.file("shooter/Tools.csproj", "<Project/>\n")
        .unwrap()
        .file_of_size("shooter/bin/Debug/Tools.dll", 1024)
        .unwrap()
        .file("shooter/obj/project.assets.json", "{}")
        .unwrap();
    age_tree(&mut fx, "kart", days_ago(5));
    age_tree(&mut fx, "shooter", days_ago(5));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let all = found(&root);
    assert_eq!(
        all,
        pairs(&[
            ("unity.library", "kart/Library"),
            ("unity.obj", "kart/obj"),
            ("unreal.binaries", "shooter/Binaries"),
            ("unreal.derived-data-cache", "shooter/DerivedDataCache"),
            ("unreal.intermediate", "shooter/Intermediate"),
            ("unreal.saved", "shooter/Saved"),
        ])
    );
    for (_, p) in &all {
        assert!(
            !USER_WORK
                .iter()
                .any(|u| p == u || p.starts_with(&format!("{u}/"))),
            "{p} holds user work"
        );
    }

    let library = at(&root, "kart/Library");
    assert_eq!(library.projects[0].name, "kart");
    assert_eq!(library.projects[0].ecosystem, "unity");
    assert_eq!(
        library.projects[0].root.to_path_buf(),
        fx.path("kart").unwrap()
    );
    assert_eq!(library.signals.regen, Regen::NetworkOrLong);
    let ddc = at(&root, "shooter/DerivedDataCache");
    assert_eq!(ddc.projects[0].ecosystem, "unreal");
    assert_eq!(ddc.signals.regen, Regen::NetworkOrLong);
    let saved = at(&root, "shooter/Saved");
    let user_data = notes(&saved);
    assert!(
        user_data.iter().any(|n| n.contains("Autosaves/")
            && n.contains("Config/")
            && n.contains("Screenshots/")),
        "{user_data:?}"
    );
    let obj = at(&root, "kart/obj");
    let inter = at(&root, "shooter/Intermediate");
    let bins = at(&root, "shooter/Binaries");
    assert!(notes(&bins).is_empty(), "{:?}", notes(&bins));

    let cands = vec![
        classify_expect(&library, &clean_caller(&library), Tier::S1),
        classify_expect(&ddc, &clean_caller(&ddc), Tier::S1),
        classify_expect(&obj, &clean_caller(&obj), Tier::S0),
        classify_expect(&inter, &clean_caller(&inter), Tier::S1),
        classify_expect(&bins, &clean_caller(&bins), Tier::S1),
        candidate(&library, Tier::S1),
        candidate(&ddc, Tier::S1),
        candidate(&saved, Tier::S2),
    ];
    if identity_available() {
        assert_eq!(cands[0].action, Action::Quarantine);
        assert_eq!(cands[2].action, Action::Delete);
        assert_eq!(cands[7].action, Action::Quarantine, "Saved/ is restorable");
    }
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn saved_is_s2_even_with_only_logs() {
    let mut fx = Fixture::new().unwrap();
    fx.file("bp/Blueprints.uproject", "{}\n")
        .unwrap()
        .file_of_size("bp/Content/Maps/Main.umap", 4096)
        .unwrap()
        .file("bp/Saved/Logs/Blueprints.log", "log\n")
        .unwrap();
    age_tree(&mut fx, "bp", days_ago(5));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(found(&root), pairs(&[("unreal.saved", "bp/Saved")]));
    let saved = at(&root, "bp/Saved");
    assert!(notes(&saved).is_empty(), "{:?}", notes(&saved));
    let c = candidate(&saved, Tier::S2);
    check(&fx, &plan_inputs(&[c]));
}

// ---------------------------------------------------------------------------------------------
// Unity specifics

#[test]
fn unity_temp_is_s0_s2_with_scene_backups_and_the_editor_lock_is_watched() {
    let mut fx = Fixture::new().unwrap();
    unity_project(&mut fx, "kart");
    fx.file("kart/Temp/UnityLockfile", "")
        .unwrap()
        .file_of_size("kart/Temp/bin/Debug/Assembly-CSharp.dll", 1024)
        .unwrap();
    unity_project(&mut fx, "crashed");
    fx.file("crashed/Temp/UnityLockfile", "")
        .unwrap()
        .file_of_size("crashed/Temp/__Backupscenes/0.backup", 4096)
        .unwrap();
    age_tree(&mut fx, "kart", days_ago(5));
    age_tree(&mut fx, "crashed", days_ago(5));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    assert_eq!(
        found(&root),
        pairs(&[
            ("unity.library", "crashed/Library"),
            ("unity.library", "kart/Library"),
            ("unity.temp", "crashed/Temp"),
            ("unity.temp", "kart/Temp"),
        ])
    );
    let lock = root.join("kart/Temp/UnityLockfile");
    let library = at(&root, "kart/Library");
    assert!(
        library.activity_lock_files.contains(&lock),
        "{:?}",
        library.activity_lock_files
    );
    assert!(
        library
            .activity_lock_files
            .contains(&root.join("kart/Library/ArtifactDB-lock"))
    );
    let temp = at(&root, "kart/Temp");
    assert_eq!(
        temp.activity_lock_files
            .iter()
            .filter(|l| **l == lock)
            .count(),
        1,
        "listed once"
    );
    assert!(notes(&temp).is_empty());
    let crashed = at(&root, "crashed/Temp");
    assert!(
        notes(&crashed).iter().any(|n| n.contains("__Backupscenes")),
        "{:?}",
        notes(&crashed)
    );
    let cands = vec![
        classify_expect(&temp, &clean_caller(&temp), Tier::S0),
        candidate(&temp, Tier::S0),
        candidate(&crashed, Tier::S2),
    ];
    if identity_available() {
        assert_eq!(cands[1].action, Action::Delete);
        assert_eq!(cands[2].action, Action::Quarantine);
    }
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn unity_look_alikes_are_not_candidates() {
    let mut fx = Fixture::new().unwrap();
    // `Library/` beside Assets but no ProjectSettings.
    fx.dir("a/Assets")
        .unwrap()
        .file_of_size("a/Library/ArtifactDB", 1024)
        .unwrap();
    // ProjectSettings without ProjectVersion.txt.
    fx.dir("b/Assets")
        .unwrap()
        .file("b/ProjectSettings/notes.txt", "x\n")
        .unwrap()
        .file_of_size("b/Library/ArtifactDB", 1024)
        .unwrap();
    // ProjectSettings and Assets as files.
    fx.file("c/Assets", "x\n")
        .unwrap()
        .file("c/ProjectSettings", "x\n")
        .unwrap()
        .file_of_size("c/Library/ArtifactDB", 1024)
        .unwrap();
    // A real project whose Library holds none of the editor's entries (a user's own folder).
    fx.file(
        "d/ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 6000.0.1f1\n",
    )
    .unwrap()
    .dir("d/Assets")
    .unwrap()
    .file("d/Library/books.txt", "mine\n")
    .unwrap();
    // A user folder named Library inside Assets, holding asset-database look-alikes.
    unity_project(&mut fx, "e");
    fx.file_of_size("e/Assets/Library/ArtifactDB", 1024)
        .unwrap()
        .file_of_size("e/Assets/Temp/UnityLockfile", 10)
        .unwrap();
    // Temp in a project that has no Library; obj without the generated C# projects.
    fx.file(
        "f/ProjectSettings/ProjectVersion.txt",
        "m_EditorVersion: 6000.0.1f1\n",
    )
    .unwrap()
    .dir("f/Assets")
    .unwrap()
    .file("f/Temp/UnityLockfile", "")
    .unwrap()
    .file("f/obj/project.assets.json", "{}")
    .unwrap();
    // A macOS-like home: ~/Library beside an Assets folder.
    fx.dir("home/Assets")
        .unwrap()
        .file("home/Library/Preferences/com.apple.finder.plist", "x\n")
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(found(&root), pairs(&[("unity.library", "e/Library")]));
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// Unreal specifics

#[test]
fn unreal_binaries_without_source_are_s2() {
    let mut fx = Fixture::new().unwrap();
    fx.file("handed/Handed.uproject", "{}\n")
        .unwrap()
        .file_of_size("handed/Content/Maps/Main.umap", 4096)
        .unwrap()
        .file_of_size("handed/Binaries/Win64/UnrealEditor-Handed.dll", 4096)
        .unwrap();
    age_tree(&mut fx, "handed", days_ago(5));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(
        found(&root),
        pairs(&[("unreal.binaries", "handed/Binaries")])
    );
    let bins = at(&root, "handed/Binaries");
    assert!(
        notes(&bins).iter().any(|n| n.contains("no Source/ folder")),
        "{:?}",
        notes(&bins)
    );
    let c = candidate(&bins, Tier::S2);
    if identity_available() {
        assert_eq!(c.action, Action::Quarantine);
    }
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn unreal_engine_plugin_and_look_alike_folders_are_not_candidates() {
    let mut fx = Fixture::new().unwrap();
    // An engine install: no .uproject beside its folders.
    for f in [
        "Binaries/Win64/UnrealEditor.exe",
        "Intermediate/Build/x.obj",
        "DerivedDataCache/a.udd",
        "Saved/Logs/x.log",
    ] {
        fx.file_of_size(format!("UE_5.4/Engine/{f}"), 1024).unwrap();
    }
    // The shared DerivedDataCache outside any project.
    fx.file_of_size(
        "home/.config/Epic/UnrealEngine/Common/DerivedDataCache/0/a.udd",
        1024,
    )
    .unwrap();
    // A plugin's own folders inside a project (prebuilt binaries may have no source).
    unreal_project(&mut fx, "shooter");
    // `.uproject` as a directory; a backup copy of one.
    fx.dir("odd/Odd.uproject")
        .unwrap()
        .file_of_size("odd/Intermediate/Build/x.obj", 1024)
        .unwrap()
        .file("bak/Old.uproject.bak", "{}\n")
        .unwrap()
        .file_of_size("bak/DerivedDataCache/a.udd", 1024)
        .unwrap();
    // Binaries without a platform folder; Saved and Intermediate without engine output.
    fx.file("thin/Thin.uproject", "{}\n")
        .unwrap()
        .file("thin/Binaries/readme.txt", "x\n")
        .unwrap()
        .file("thin/Saved/notes.txt", "mine\n")
        .unwrap()
        .file("thin/Intermediate/notes.txt", "mine\n")
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let all = found(&root);
    // Only the project's own folders; nothing under the engine, the shared cache, the plugin,
    // the look-alikes or the user's work.
    assert_eq!(
        all,
        pairs(&[
            ("unreal.binaries", "shooter/Binaries"),
            ("unreal.derived-data-cache", "shooter/DerivedDataCache"),
            ("unreal.intermediate", "shooter/Intermediate"),
            ("unreal.saved", "shooter/Saved"),
        ])
    );
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn game_engine_folders_that_are_links_are_not_candidates() {
    let mut fx = Fixture::new().unwrap();
    unreal_project(&mut fx, "shooter");
    fx.dir("elsewhere/ddc").unwrap();
    fx.file_of_size("elsewhere/ddc/a.udd", 1024).unwrap();
    let created = fx
        .symlink("shooter2/DerivedDataCache", "../elsewhere/ddc")
        .unwrap();
    fx.file("shooter2/Shooter2.uproject", "{}\n").unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let all = found(&root);
    assert!(
        !all.iter().any(|(_, p)| p.starts_with("shooter2")),
        "{all:?} ({created:?})"
    );
    check(&fx, &plan_inputs(&[]));
}
