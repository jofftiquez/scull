//! End-to-end FX-MAC fixtures (docs/FIXTURES.md §3.11).
//!
//! * **FX-MAC-04**: `.app` bundles and packages inside scan roots are treated as folders
//!   (walked and measured like any directory) and are not candidates unless a rule matches.
//!
//! Bundles are plain directories with a name suffix, so this fixture builds and runs on every
//! OS; the registry counts it for macOS (its only OS) once it has run there.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cli;
mod exec;

use cli::{Cli, Schemas};
use exec::engine::{harness, rel};
use scull_core::Tier;
use scull_fixtures::{Fixture, InvariantInputs};

const BUNDLES: &[&str] = &[
    "Applications/Tool.app",
    "Installers/Setup.pkg",
    "Plugins/Filter.bundle",
    "Library/Frameworks/Kit.framework",
    "Docs/Report.rtfd",
];

/// FX-MAC-04: an application bundle, an installer package, a plug-in bundle, a framework and a
/// document package, none matching a rule, beside a Cargo project whose folder happens to be
/// named `Demo.app`. The bundles are walked (their files counted and listed by `large-files`)
/// but never become candidates; `Demo.app/target` does, because the Cargo rule matches it.
#[test]
fn fx_mac_04_bundles_are_folders_and_not_candidates_unless_a_rule_matches() {
    let mut fx = Fixture::new().unwrap();
    fx.file("Applications/Tool.app/Contents/Info.plist", "<plist/>")
        .unwrap()
        .file_of_size("Applications/Tool.app/Contents/MacOS/Tool", 64 * 1024)
        .unwrap()
        .file_of_size(
            "Applications/Tool.app/Contents/Resources/assets.car",
            600 * 1024,
        )
        .unwrap()
        .file(
            "Applications/Tool.app/Contents/Resources/node_modules.txt",
            "a file, not a folder",
        )
        .unwrap()
        .file_of_size("Installers/Setup.pkg/Contents/Archive.pax.gz", 32 * 1024)
        .unwrap()
        .file("Installers/Setup.pkg/Contents/PackageInfo", "<pkg-info/>")
        .unwrap()
        .file_of_size("Plugins/Filter.bundle/Contents/MacOS/Filter", 16 * 1024)
        .unwrap()
        .file_of_size("Library/Frameworks/Kit.framework/Versions/A/Kit", 24 * 1024)
        .unwrap()
        .file("Docs/Report.rtfd/TXT.rtf", "{\\rtf1 report}")
        .unwrap()
        .cargo_project("Demo.app", "demo")
        .unwrap()
        .cargo_target("Demo.app", "demo", 8 * 1024)
        .unwrap();
    fx.checkpoint().unwrap();

    let r = harness().scan(&fx);
    let cands: Vec<(String, Tier)> = r
        .candidates
        .iter()
        .map(|c| (rel(&fx, &c.path.to_path_buf()), c.tier))
        .collect();
    println!("candidates: {cands:#?}");
    for b in BUNDLES {
        assert!(
            !cands
                .iter()
                .any(|(p, _)| p == b || p.starts_with(&format!("{b}/"))),
            "{b} (no rule matches) became a candidate: {cands:?}"
        );
    }
    let demo = cands
        .iter()
        .find(|(p, _)| p == "Demo.app/target")
        .unwrap_or_else(|| panic!("the Cargo rule matches inside Demo.app: {cands:?}"));
    assert_eq!(demo.1, Tier::S0, "{demo:?}");
    // Walked like folders: every file inside the bundles was counted ...
    let bundle_files = 9;
    assert!(
        r.entries_scanned >= bundle_files + 5,
        "entries scanned: {}",
        r.entries_scanned
    );
    // ... and a large file deep inside an app bundle is found (inspect-only).
    let cli = Cli::new();
    let root = fx.root().to_string_lossy().into_owned();
    let large = Schemas::default().json(
        &cli,
        &["large-files", &root, "--min-size", "512K"],
        "large-files",
    );
    let files: Vec<String> = large["data"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| rel(&fx, std::path::Path::new(f["path"].as_str().unwrap())))
        .collect();
    println!("large files: {files:?}");
    assert_eq!(
        files,
        vec!["Applications/Tool.app/Contents/Resources/assets.car".to_owned()]
    );
    for f in large["data"]["files"].as_array().unwrap() {
        assert_eq!(f["action"]["type"], "none", "inspect-only: {f:#}");
    }
    let report = fx.check_invariants(&InvariantInputs::new());
    println!("{report}");
    report.assert_ok();
}
