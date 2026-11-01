//! Java/Kotlin and .NET project build folders (P5-DET-01, PRD §7.1): Gradle `build/` and
//! `.gradle/`, Maven `target/`, .NET `bin/` and `obj/`, marker gating (CLS-03, FX-CLS-10), and
//! game-engine projects left to their own rules (FX-CLS-14). Every test builds its tree with
//! `scull-fixtures`, detects read-only through `scull-platform`, and checks the universal
//! invariants (I-2 canary, I-3 tree unchanged, I-1 on the would-be plan).

#![allow(clippy::unwrap_used)]

mod common;

use common::{age_tree, check, classify_expect, clean_caller, plan_inputs, rel, scan};
use scull_core::{Action, Candidate, Evidence, Tier};
use scull_detectors::DetectedArtifact;
use scull_fixtures::{Fixture, days_ago};
use scull_safety::Regen;
use std::path::Path;

fn evidence_paths(a: &DetectedArtifact, root: &Path) -> Vec<String> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Marker { path } => Some(format!("marker {}", rel(root, &path.to_path_buf()))),
            Evidence::KnownArtifact { artifact } => Some(format!("known {artifact}")),
            _ => None,
        })
        .collect()
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

/// Rule IDs of everything found, with paths, sorted.
fn found(root: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = scan(root, &[])
        .found
        .iter()
        .map(|a| (a.rule.id.clone(), rel(root, &a.path)))
        .collect();
    v.sort();
    v
}

/// Classify every artifact as idle and clean, expecting `tier`, and return the candidates.
fn classify_all(arts: &[&DetectedArtifact], tier: Tier) -> Vec<Candidate> {
    arts.iter()
        .map(|a| classify_expect(a, &clean_caller(a), tier))
        .collect()
}

/// A Gradle (Kotlin DSL) project with an `app` module, built: `build/` and `.gradle/` as Gradle
/// writes them.
fn gradle_project(fx: &mut Fixture, dir: &str) {
    fx.file(
        format!("{dir}/settings.gradle.kts"),
        "rootProject.name = \"demo\"\ninclude(\":app\")\n",
    )
    .unwrap()
    .file(format!("{dir}/build.gradle.kts"), "plugins { java }\n")
    .unwrap()
    .file(format!("{dir}/gradlew"), "#!/bin/sh\n")
    .unwrap()
    .file(format!("{dir}/.gradle/8.10/fileHashes/fileHashes.bin"), "x")
    .unwrap()
    .file(
        format!("{dir}/.gradle/buildOutputCleanup/cache.properties"),
        "x",
    )
    .unwrap()
    .file(format!("{dir}/.gradle/file-system.probe"), "x")
    .unwrap()
    .file(
        format!("{dir}/app/build.gradle.kts"),
        "plugins { kotlin(\"jvm\") }\n",
    )
    .unwrap()
    .file(
        format!("{dir}/app/src/main/kotlin/Main.kt"),
        "fun main() {}\n",
    )
    .unwrap()
    .file_of_size(
        format!("{dir}/app/build/classes/kotlin/main/MainKt.class"),
        2048,
    )
    .unwrap()
    .file_of_size(format!("{dir}/app/build/libs/app.jar"), 4096)
    .unwrap()
    .file(
        format!("{dir}/app/build/tmp/jar/MANIFEST.MF"),
        "Manifest-Version: 1.0\n",
    )
    .unwrap()
    .file(
        format!("{dir}/app/build/kotlin/compileKotlin/cacheable/x"),
        "x",
    )
    .unwrap();
}

fn maven_project(fx: &mut Fixture, dir: &str) {
    fx.file(format!("{dir}/pom.xml"), "<project/>\n")
        .unwrap()
        .file(format!("{dir}/src/main/java/App.java"), "class App {}\n")
        .unwrap()
        .file_of_size(format!("{dir}/target/classes/App.class"), 2048)
        .unwrap()
        .file(
            format!("{dir}/target/maven-status/maven-compiler-plugin/compile/default-compile/inputFiles.lst"),
            "App.java\n",
        )
        .unwrap()
        .file_of_size(format!("{dir}/target/app-1.0.jar"), 4096)
        .unwrap();
}

/// A built SDK-style .NET project: `bin/<config>/<tfm>/` and `obj/` with restore output.
fn dotnet_project(fx: &mut Fixture, dir: &str, project_file: &str) {
    fx.file(
        format!("{dir}/{project_file}"),
        "<Project Sdk=\"Microsoft.NET.Sdk\" />\n",
    )
    .unwrap()
    .file(format!("{dir}/Program.cs"), "System.Console.WriteLine();\n")
    .unwrap()
    .file_of_size(format!("{dir}/bin/Debug/net8.0/App.dll"), 4096)
    .unwrap()
    .file_of_size(format!("{dir}/bin/Release/net8.0/publish/App.dll"), 4096)
    .unwrap()
    .file(format!("{dir}/obj/project.assets.json"), "{\"version\":3}")
    .unwrap()
    .file(
        format!("{dir}/obj/{project_file}.nuget.g.props"),
        "<Project/>",
    )
    .unwrap()
    .file_of_size(format!("{dir}/obj/Debug/net8.0/App.dll"), 2048)
    .unwrap();
}

// ---------------------------------------------------------------------------------------------
// Gradle

#[test]
fn gradle_build_and_project_cache_are_s0() {
    let mut fx = Fixture::new().unwrap();
    gradle_project(&mut fx, "demo");
    age_tree(&mut fx, "demo", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["demo/.gradle", "demo/app/build"]);
    let build = s.at(&root, "demo/app/build");
    assert_eq!(build.rule.id, "gradle.build");
    assert_eq!(build.projects[0].name, "app");
    assert_eq!(build.projects[0].ecosystem, "jvm");
    assert_eq!(
        evidence_paths(build, &root),
        [
            "marker demo/app/build.gradle.kts",
            "marker demo/app/build/classes",
            "known Gradle build output",
        ]
    );
    assert_eq!(build.signals.regen, Regen::Local);
    assert!(build.activity_processes.contains(&"java".to_owned()));
    let cache = s.at(&root, "demo/.gradle");
    assert_eq!(cache.rule.id, "gradle.project-cache");
    assert_eq!(cache.projects[0].name, "demo");

    let candidates = classify_all(&[build, cache], Tier::S0);
    for c in &candidates {
        if c.tier == Tier::S0 {
            assert_eq!(c.action, Action::Delete);
        }
    }
    check(&fx, &plan_inputs(&candidates));
}

#[test]
fn fx_cls_10_build_dir_needs_gradle_build_file_and_gradle_output() {
    let mut fx = Fixture::new().unwrap();
    // FX-CLS-10 itself: a CMake `build/` without CMakeCache.txt is not a candidate.
    fx.file("cmake/CMakeLists.txt", "project(x)\n")
        .unwrap()
        .file_of_size("cmake/build/x.o", 1024)
        .unwrap()
        .dir("cmake/build/tmp")
        .unwrap();
    // A CMake + Gradle project (e.g. an NDK module): ownership of `build/` is ambiguous.
    fx.file("ndk/CMakeLists.txt", "project(x)\n")
        .unwrap()
        .file("ndk/build.gradle", "apply plugin: 'java'\n")
        .unwrap()
        .file_of_size("ndk/build/classes/A.class", 1024)
        .unwrap();
    // `build/` beside a build script but holding only the user's own files.
    fx.file("handmade/build.gradle", "\n")
        .unwrap()
        .file("handmade/build/release-notes.txt", "notes\n")
        .unwrap()
        .file("handmade/build/deploy/run.sh", "#!/bin/sh\n")
        .unwrap();
    // Gradle-looking output with no build script anywhere beside it.
    fx.file_of_size("orphan/build/classes/A.class", 1024)
        .unwrap()
        .dir("orphan/build/tmp")
        .unwrap();
    // A `.gradle` user home next to a stray build script is never a project cache.
    fx.file("home/build.gradle", "\n")
        .unwrap()
        .file("home/.gradle/caches/modules-2/x", "x")
        .unwrap()
        .file("home/.gradle/file-system.probe", "x")
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(found(&root), Vec::<(String, String)>::new());
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// Maven

#[test]
fn maven_target_is_s0_and_ambiguous_target_is_left_alone() {
    let mut fx = Fixture::new().unwrap();
    maven_project(&mut fx, "svc");
    // A POM and a Cargo manifest side by side: neither rule claims `target/`.
    maven_project(&mut fx, "mixed");
    fx.file("mixed/Cargo.toml", "[package]\nname = \"m\"\n")
        .unwrap()
        .file("mixed/target/CACHEDIR.TAG", scull_fixtures::CACHEDIR_TAG)
        .unwrap();
    // `target/` beside a POM with photos in it (FX-CLS-02's twin).
    fx.file("photos/pom.xml", "<project/>\n")
        .unwrap()
        .file_of_size("photos/target/IMG_0001.jpg", 4096)
        .unwrap();
    age_tree(&mut fx, "svc", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["svc/target"]);
    let a = s.at(&root, "svc/target");
    assert_eq!(a.rule.id, "maven.target");
    assert_eq!(
        evidence_paths(a, &root),
        [
            "marker svc/pom.xml",
            "marker svc/target/classes",
            "known Maven build output",
        ]
    );
    let candidates = classify_all(&[a], Tier::S0);
    check(&fx, &plan_inputs(&candidates));
}

// ---------------------------------------------------------------------------------------------
// .NET

#[test]
fn dotnet_bin_and_obj_beside_a_project_file_are_s0() {
    let mut fx = Fixture::new().unwrap();
    dotnet_project(&mut fx, "src/App", "App.csproj");
    dotnet_project(&mut fx, "src/Lib", "Lib.fsproj");
    fx.file("App.sln", "Microsoft Visual Studio Solution File\n")
        .unwrap();
    age_tree(&mut fx, "src", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        ["src/App/bin", "src/App/obj", "src/Lib/bin", "src/Lib/obj"]
    );
    let bin = s.at(&root, "src/App/bin");
    assert_eq!(bin.rule.id, "dotnet.bin");
    assert_eq!(bin.projects[0].name, "App");
    assert_eq!(
        evidence_paths(bin, &root),
        [
            "marker src/App/App.csproj",
            "marker src/App/bin/Debug",
            "known .NET build output",
        ]
    );
    assert!(notes(bin).is_empty(), "only configuration folders inside");
    let obj = s.at(&root, "src/Lib/obj");
    assert_eq!(obj.rule.id, "dotnet.obj");
    assert!(
        evidence_paths(obj, &root).contains(&"marker src/Lib/obj/project.assets.json".to_owned())
    );
    let arts: Vec<&DetectedArtifact> = s.found.iter().collect();
    let candidates = classify_all(&arts, Tier::S0);
    check(&fx, &plan_inputs(&candidates));
}

#[test]
fn cls_03_dotnet_bin_needs_a_project_file_and_only_build_output() {
    let mut fx = Fixture::new().unwrap();
    // A solution file alone is not a project marker.
    fx.file(
        "sln-only/All.sln",
        "Microsoft Visual Studio Solution File\n",
    )
    .unwrap()
    .file_of_size("sln-only/bin/Debug/tool.exe", 1024)
    .unwrap();
    // A `bin/` of scripts beside a project file (no configuration folder).
    fx.file("scripts/Tool.csproj", "<Project/>\n")
        .unwrap()
        .file("scripts/bin/deploy.sh", "#!/bin/sh\n")
        .unwrap();
    // `obj/` beside a project file but with no restore or build output.
    fx.file("scripts/obj/readme.txt", "mine\n").unwrap();
    // Build output mixed with a loose file: may hold more than build output, so S2.
    fx.file("mixed/Mixed.csproj", "<Project/>\n")
        .unwrap()
        .file_of_size("mixed/bin/Debug/net8.0/Mixed.dll", 1024)
        .unwrap()
        .file("mixed/bin/install.ps1", "Write-Host hi\n")
        .unwrap();
    age_tree(&mut fx, "mixed", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["mixed/bin"]);
    let a = s.at(&root, "mixed/bin");
    assert_eq!(a.base_tier, Tier::S2);
    assert!(
        notes(a).iter().any(|n| n.contains("install.ps1")),
        "{:?}",
        notes(a)
    );
    let c = a.clone().into_candidate(&clean_caller(a));
    assert!(c.is_consistent());
    assert!(c.tier >= Tier::S2, "never S0 with a loose file");
    assert_ne!(c.action, Action::Delete);
    check(&fx, &plan_inputs(&[c]));
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-14: game engine projects are left to their own rules

#[test]
fn fx_cls_14_unity_and_unreal_folders_are_not_claimed_as_dotnet_or_gradle() {
    let mut fx = Fixture::new().unwrap();
    // Unity: generated C# projects at the root, `obj/` and `Library/` beside them.
    fx.file("unity/Assets/Player.cs", "class Player {}\n")
        .unwrap()
        .file(
            "unity/ProjectSettings/ProjectVersion.txt",
            "m_EditorVersion: 6000.0.1f1\n",
        )
        .unwrap()
        .file("unity/Assembly-CSharp.csproj", "<Project/>\n")
        .unwrap()
        .file("unity/unity.sln", "\n")
        .unwrap()
        .file_of_size("unity/obj/Debug/Assembly-CSharp.dll", 2048)
        .unwrap()
        .file("unity/obj/project.assets.json", "{}")
        .unwrap()
        .file_of_size("unity/Library/ArtifactDB", 4096)
        .unwrap()
        .file_of_size("unity/bin/Debug/Assembly-CSharp.dll", 1024)
        .unwrap();
    // Unreal: `.uproject`, `Binaries/`, `Intermediate/`, `Saved/`, `DerivedDataCache/`; a C#
    // build-tool project beside it must not make its `bin/` a .NET candidate.
    fx.file("unreal/Shooter.uproject", "{\"FileVersion\":3}\n")
        .unwrap()
        .file("unreal/Shooter.csproj", "<Project/>\n")
        .unwrap()
        .file_of_size("unreal/bin/Debug/Shooter.dll", 1024)
        .unwrap()
        .file_of_size("unreal/Binaries/Win64/Shooter.exe", 4096)
        .unwrap()
        .file_of_size("unreal/Intermediate/Build/x.obj", 2048)
        .unwrap()
        .file("unreal/Saved/Config/Game.ini", "[x]\n")
        .unwrap()
        .file_of_size("unreal/DerivedDataCache/x.udd", 4096)
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let mine: Vec<(String, String)> = found(&root)
        .into_iter()
        .filter(|(id, _)| {
            ["dotnet.", "gradle.", "maven."]
                .iter()
                .any(|p| id.starts_with(p))
        })
        .collect();
    assert_eq!(mine, Vec::<(String, String)>::new());
    check(&fx, &plan_inputs(&[]));
}
