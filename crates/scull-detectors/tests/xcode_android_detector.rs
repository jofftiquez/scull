//! Xcode and Android fixtures (P5-DET-02, PRD §7.1, §10.5): DerivedData (S0, S1 with Swift
//! package checkouts, project read from `info.plist`), device support files (S1), archives
//! (S2), unavailable simulators (`xcrun simctl delete unavailable`, tool-native), Android Gradle
//! Plugin build output (S0; S1 with release builds; S2 with mapping files), emulator images
//! (S2), and marker/placement gating (CLS-03, FX-CLS-10).
//!
//! The Xcode rules are macOS-only; to check their logic on every CI runner the tests load them
//! with all platforms enabled (the bundled set keeps them macOS-only, checked separately). Every
//! test builds its tree with `scull-fixtures`, detects read-only, and checks the universal
//! invariants (I-2 canary, I-3 tree unchanged, I-1 on the would-be plan). Nothing runs `xcrun`.

#![allow(clippy::unwrap_used)]

mod common;

use common::{
    age_tree, check, classify_expect, clean_caller, identity_available, plan_inputs, rel, scan_with,
};
use scull_core::{Action, Candidate, Evidence, Tier};
use scull_detectors::rules::{BUNDLED_RULE_FILES, RuleSet, RuleSource};
use scull_detectors::{DetectedArtifact, Detectors};
use scull_fixtures::{Fixture, days_ago};
use scull_safety::Regen;
use std::path::Path;

const HASH: &str = "abcdefghijklmnopqrstuvwxyzab";
const DEV1: &str = "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9";
const DEV2: &str = "11111111-2222-3333-4444-555555555555";
const XCODE: &str = "home/Library/Developer/Xcode";

/// The bundled detectors, with the macOS-only Xcode rules enabled on every OS.
fn detectors() -> Detectors {
    let mut set = RuleSet::new();
    for (origin, text) in BUNDLED_RULE_FILES {
        let text = if origin.ends_with("xcode.toml") {
            text.replace(
                "platforms = [\"macos\"]",
                "platforms = [\"macos\", \"linux\", \"windows\"]",
            )
        } else {
            (*text).to_owned()
        };
        set.add_file(&text, origin, RuleSource::Bundled).unwrap();
    }
    Detectors::from_rules(&set)
}

/// Everything found under `root`, as `(rule, relative path)`, sorted.
fn found(root: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = scan_with(root, &[], &detectors())
        .found
        .iter()
        .map(|a| (a.rule.id.clone(), rel(root, &a.path)))
        .collect();
    v.sort();
    v
}

fn at(root: &Path, rel_path: &str) -> DetectedArtifact {
    scan_with(root, &[], &detectors())
        .at(root, rel_path)
        .clone()
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

fn plist(workspace: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\
         \t<key>LastAccessedDate</key>\n\t<date>2026-10-01T09:00:00Z</date>\n\
         \t<key>WorkspacePath</key>\n\t<string>{workspace}</string>\n</dict>\n</plist>\n"
    )
}

fn derived_data(fx: &mut Fixture, dir: &str, workspace: &str) {
    fx.file(format!("{dir}/info.plist"), plist(workspace))
        .unwrap()
        .file_of_size(
            format!("{dir}/Build/Products/Debug-iphonesimulator/App.app/App"),
            4096,
        )
        .unwrap()
        .file_of_size(format!("{dir}/Index.noindex/DataStore/v5/units/x"), 2048)
        .unwrap()
        .file(
            format!("{dir}/Logs/Build/LogStoreManifest.plist"),
            "<plist/>",
        )
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// Xcode

#[test]
fn derived_data_is_s0_attributed_to_its_project_and_s1_with_packages() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("projects/App/App.xcodeproj")
        .unwrap()
        .dir("projects/Gone")
        .unwrap();
    let dd = format!("{XCODE}/DerivedData");
    let app_ws = fx.path("projects/App/App.xcodeproj").unwrap();
    let gone_ws = fx.root().join("projects/Gone/Gone.xcworkspace");
    derived_data(
        &mut fx,
        &format!("{dd}/App-{HASH}"),
        &app_ws.to_string_lossy(),
    );
    derived_data(
        &mut fx,
        &format!("{dd}/Gone-{HASH}"),
        &gone_ws.to_string_lossy(),
    );
    derived_data(
        &mut fx,
        &format!("{dd}/Pkg-{HASH}"),
        "/nonexistent/Pkg.xcodeproj",
    );
    fx.file_of_size(
        format!("{dd}/Pkg-{HASH}/SourcePackages/checkouts/swift-log/Package.swift"),
        1024,
    )
    .unwrap()
    .file_of_size(format!("{dd}/ModuleCache.noindex/ABC/Foundation.pcm"), 4096)
    .unwrap();
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    assert_eq!(
        found(&root),
        [
            ("xcode.derived-data".to_owned(), format!("{dd}/App-{HASH}")),
            ("xcode.derived-data".to_owned(), format!("{dd}/Gone-{HASH}")),
            ("xcode.derived-data".to_owned(), format!("{dd}/Pkg-{HASH}")),
        ]
    );
    let app = at(&root, &format!("{dd}/App-{HASH}"));
    assert_eq!(app.projects.len(), 1);
    assert_eq!(app.projects[0].name, "App");
    assert_eq!(app.projects[0].ecosystem, "xcode");
    assert_eq!(
        app.projects[0].root.to_path_buf(),
        fx.path("projects/App").unwrap()
    );
    assert!(notes(&app).iter().any(|n| n.starts_with("built from")));
    assert!(!notes(&app).iter().any(|n| n.contains("leftover")));
    let gone = at(&root, &format!("{dd}/Gone-{HASH}"));
    assert!(
        notes(&gone).iter().any(|n| n.contains("no longer exists")),
        "{:?}",
        notes(&gone)
    );
    let pkg = at(&root, &format!("{dd}/Pkg-{HASH}"));
    assert_eq!(pkg.signals.regen, Regen::NetworkOrLong);

    let mut cands = vec![
        classify_expect(&app, &clean_caller(&app), Tier::S0),
        classify_expect(&gone, &clean_caller(&gone), Tier::S0),
        classify_expect(&pkg, &clean_caller(&pkg), Tier::S1),
    ];
    if identity_available() {
        assert_eq!(cands[0].action, Action::Delete);
        assert_eq!(cands[2].action, Action::Quarantine);
    }
    cands.push(candidate(&pkg, Tier::S1));
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn derived_data_look_alikes_are_not_candidates() {
    let mut fx = Fixture::new().unwrap();
    // Right shape, wrong parent.
    derived_data(&mut fx, &format!("backup/App-{HASH}"), "/x/App.xcodeproj");
    // Right parent, not Xcode's hash.
    derived_data(
        &mut fx,
        "home/Library/Developer/Xcode/DerivedData/App-ABCDEFGHIJKLMNOPQRSTUVWXYZAB",
        "/x/App.xcodeproj",
    );
    // Right parent and name, no info.plist.
    fx.file_of_size(
        format!("home/Library/Developer/Xcode/DerivedData/Raw-{HASH}/Build/x"),
        1024,
    )
    .unwrap();
    fx.checkpoint().unwrap();
    assert_eq!(found(fx.root()), Vec::<(String, String)>::new());
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn device_support_is_s1_and_archives_are_s2() {
    let mut fx = Fixture::new().unwrap();
    let ios = format!("{XCODE}/iOS DeviceSupport/iPhone15,2 17.0 (21A329)");
    let watch = format!("{XCODE}/watchOS DeviceSupport/10.0 (21R356)");
    let archive = format!("{XCODE}/Archives/2026-10-01/App 01-10-2026, 10.00.xcarchive");
    fx.file_of_size(format!("{ios}/Symbols/usr/lib/dyld"), 8192)
        .unwrap()
        .file(format!("{ios}/Info.plist"), "<plist/>")
        .unwrap()
        .file_of_size(format!("{watch}/Symbols/usr/lib/dyld"), 4096)
        .unwrap()
        .file(format!("{archive}/Info.plist"), "<plist/>")
        .unwrap()
        .file_of_size(format!("{archive}/Products/Applications/App.app/App"), 4096)
        .unwrap()
        .file_of_size(
            format!("{archive}/dSYMs/App.app.dSYM/Contents/Resources/DWARF/App"),
            4096,
        )
        .unwrap();
    // A version-named folder with symbols outside `<OS> DeviceSupport` is not a candidate.
    fx.file_of_size("notes/17.0 (21A329)/Symbols/x", 1024)
        .unwrap();
    age_tree(&mut fx, "home", days_ago(10));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    assert_eq!(
        found(&root),
        [
            ("xcode.archive".to_owned(), archive.clone()),
            ("xcode.device-support".to_owned(), ios.clone()),
            ("xcode.device-support".to_owned(), watch.clone()),
        ]
    );
    let ds = at(&root, &ios);
    assert!(ds.projects.is_empty());
    let a = at(&root, &archive);
    let cands = vec![
        classify_expect(&ds, &clean_caller(&ds), Tier::S1),
        candidate(&at(&root, &watch), Tier::S1),
        candidate(&a, Tier::S2),
    ];
    if identity_available() {
        assert_eq!(
            cands[2].action,
            Action::Quarantine,
            "signed builds are restorable"
        );
    }
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn unavailable_simulators_use_simctl_delete_unavailable() {
    let mut fx = Fixture::new().unwrap();
    let devices = "home/Library/Developer/CoreSimulator/Devices";
    fx.file(format!("{devices}/device_set.plist"), "<plist/>")
        .unwrap()
        .file(format!("{devices}/{DEV1}/device.plist"), "<plist/>")
        .unwrap()
        .file_of_size(format!("{devices}/{DEV1}/data/Library/x"), 4096)
        .unwrap()
        .file(format!("{devices}/{DEV2}/device.plist"), "<plist/>")
        .unwrap();
    // A `Devices` folder elsewhere is not the simulator device set.
    fx.file("work/Devices/device_set.plist", "<plist/>")
        .unwrap();
    age_tree(&mut fx, "home", days_ago(10));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    assert_eq!(
        found(&root),
        [("xcode.simulators".to_owned(), devices.to_owned())]
    );
    let a = at(&root, devices);
    let want = Action::ToolCommand {
        tool: "xcrun".into(),
        args: vec!["simctl".into(), "delete".into(), "unavailable".into()],
    };
    assert_eq!(a.tool_action, Some(want.clone()));
    assert!(
        notes(&a)
            .iter()
            .any(|n| n.starts_with("2 simulator device(s)") && n.contains("only")),
        "{:?}",
        notes(&a)
    );
    let c = candidate(&a, Tier::S1);
    if identity_available() {
        assert_eq!(c.action, want);
    }
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn xcode_rules_are_bundled_for_macos_only() {
    let bundled = Detectors::bundled().unwrap();
    let ids: Vec<&str> = bundled.ids().filter(|i| i.starts_with("xcode.")).collect();
    if cfg!(target_os = "macos") {
        assert_eq!(
            ids,
            [
                "xcode.archive",
                "xcode.derived-data",
                "xcode.device-support",
                "xcode.simulators"
            ]
        );
    } else {
        assert!(ids.is_empty(), "{ids:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Android

fn android_module(fx: &mut Fixture, dir: &str) {
    fx.file(
        format!("{dir}/build.gradle.kts"),
        "plugins { id(\"com.android.application\") }\n",
    )
    .unwrap()
    .file(
        format!("{dir}/src/main/AndroidManifest.xml"),
        "<manifest/>\n",
    )
    .unwrap()
    .file_of_size(
        format!("{dir}/build/intermediates/dex/debug/classes.dex"),
        4096,
    )
    .unwrap()
    .file(format!("{dir}/build/tmp/kotlin-classes/x"), "x")
    .unwrap()
    .file_of_size(format!("{dir}/build/outputs/apk/debug/app-debug.apk"), 4096)
    .unwrap();
}

#[test]
fn android_build_s0_debug_s1_release_s2_mapping() {
    let mut fx = Fixture::new().unwrap();
    fx.file(
        "demo/settings.gradle.kts",
        "include(\":app\", \":wear\", \":tv\")\n",
    )
    .unwrap();
    android_module(&mut fx, "demo/app");
    android_module(&mut fx, "demo/wear");
    fx.file_of_size(
        "demo/wear/build/outputs/bundle/release/wear-release.aab",
        4096,
    )
    .unwrap();
    android_module(&mut fx, "demo/tv");
    fx.file_of_size("demo/tv/build/outputs/apk/release/tv-release.apk", 4096)
        .unwrap()
        .file(
            "demo/tv/build/outputs/mapping/release/mapping.txt",
            "a -> b\n",
        )
        .unwrap();
    age_tree(&mut fx, "demo", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    // Android modules are claimed by `android.build` alone: it runs before `gradle.build`
    // (rule ID order), so the release-aware tiers apply and nothing is listed twice.
    assert_eq!(
        found(&root),
        [
            ("android.build".to_owned(), "demo/app/build".to_owned()),
            ("android.build".to_owned(), "demo/tv/build".to_owned()),
            ("android.build".to_owned(), "demo/wear/build".to_owned()),
        ]
    );
    let app = at(&root, "demo/app/build");
    assert_eq!(app.projects[0].name, "app");
    assert!(notes(&app).is_empty(), "{:?}", notes(&app));
    let wear = at(&root, "demo/wear/build");
    assert!(
        notes(&wear)
            .iter()
            .any(|n| n.contains("signed release build"))
    );
    let tv = at(&root, "demo/tv/build");
    assert!(notes(&tv).iter().any(|n| n.contains("mapping files")));
    let cands = vec![
        candidate(&app, Tier::S0),
        candidate(&wear, Tier::S1),
        candidate(&tv, Tier::S2),
    ];
    if identity_available() {
        assert_eq!(cands[0].action, Action::Delete);
        assert_eq!(cands[2].action, Action::Quarantine);
    }
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn fx_cls_10_android_build_needs_the_plugin_output_and_a_sole_owner() {
    let mut fx = Fixture::new().unwrap();
    // An NDK-style module with CMake beside the Gradle script: ownership is ambiguous.
    android_module(&mut fx, "ndk");
    fx.file("ndk/CMakeLists.txt", "project(native)\n").unwrap();
    // `build/` beside a Gradle script without the plugin's intermediates/.
    fx.file("plain/build.gradle", "\n")
        .unwrap()
        .file("plain/build/notes.txt", "mine\n")
        .unwrap();
    // intermediates/ without any build script.
    fx.file_of_size("loose/build/intermediates/x", 1024)
        .unwrap();
    fx.checkpoint().unwrap();
    // Neither `android.build` nor the plain `gradle.build` rule claims any of them.
    assert_eq!(found(fx.root()), Vec::<(String, String)>::new());
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn android_emulator_images_are_s2_only_in_an_avd_folder() {
    let mut fx = Fixture::new().unwrap();
    let avd = "home/.android/avd";
    fx.file(format!("{avd}/Pixel_8.ini"), "path=Pixel_8.avd\n")
        .unwrap()
        .file(
            format!("{avd}/Pixel_8.avd/config.ini"),
            "hw.device.name=pixel_8\n",
        )
        .unwrap()
        .file_of_size(format!("{avd}/Pixel_8.avd/userdata-qemu.img"), 8192)
        .unwrap();
    // The same shape outside an `avd` folder is not a candidate.
    fx.file("backup/Old.ini", "path=Old.avd\n")
        .unwrap()
        .file("backup/Old.avd/config.ini", "x\n")
        .unwrap();
    age_tree(&mut fx, "home", days_ago(10));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let rel_avd = format!("{avd}/Pixel_8.avd");
    assert_eq!(found(&root), [("android.avd".to_owned(), rel_avd.clone())]);
    let a = at(&root, &rel_avd);
    assert!(a.projects.is_empty());
    assert!(
        a.activity_lock_files
            .contains(&root.join(&rel_avd).join("multiinstance.lock"))
    );
    let c = candidate(&a, Tier::S2);
    if identity_available() {
        assert_eq!(c.action, Action::Quarantine);
    }
    check(&fx, &plan_inputs(&[c]));
}
