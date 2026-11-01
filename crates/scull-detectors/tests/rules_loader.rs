//! Rule format + loader tests (P1-DET-01): CLS-01…05, FX-CFG-05, FX-CFG-06.

use scull_core::{Category, Evidence, Tier};
use scull_detectors::rules::{
    BUNDLED_RULE_FILES, EvidenceKind, MarkerOutcome, PROTECTED_FOLDER_NAMES, Platform,
    RuleErrorKind, RuleSet, RuleSource, SCHEMA_VERSION, Strategy, Surroundings, parse_rules,
};
use std::path::Path;

/// A minimal valid rule. Tests derive invalid variants with `str::replace`.
const BASE: &str = r#"
schema_version = 1

[[rule]]
id = "test.build"
version = 1
artifact = "Test build output"
ecosystem = "test"
category = "project_artifact"
platforms = ["macos", "linux", "windows"]
base_tier = "S1"
strategy = "filesystem"
evidence = ["marker", "lockfile"]

[rule.match]
names = ["build"]

[rule.markers]
all = ["../project.test"]

[rule.lockfiles]
any = ["../project.lock"]
"#;

fn user(text: &str) -> Result<Vec<scull_detectors::rules::Rule>, RuleErrorKind> {
    parse_rules(text, "user.toml", RuleSource::User).map_err(|e| e.kind)
}

fn bundled(text: &str) -> Result<Vec<scull_detectors::rules::Rule>, RuleErrorKind> {
    parse_rules(text, "bundled.toml", RuleSource::Bundled).map_err(|e| e.kind)
}

// ---------------------------------------------------------------------------------------------
// Valid rules and bundled rules (CLS-01, CLS-05).

#[test]
fn base_rule_loads_with_every_field_typed() {
    let rules = user(BASE).unwrap();
    assert_eq!(rules.len(), 1);
    let r = &rules[0];
    assert_eq!(r.id(), "test.build");
    assert_eq!(r.version(), 1);
    assert_eq!(r.rule_ref().id, "test.build");
    assert_eq!(r.rule_ref().version, 1);
    assert_eq!(r.category(), Category::ProjectArtifact);
    assert_eq!(r.base_tier(), Tier::S1);
    assert_eq!(r.strategy(), Strategy::Filesystem);
    assert_eq!(r.source(), RuleSource::User);
    assert_eq!(r.origin(), "user.toml");
    assert_eq!(r.markers_all()[0].as_str(), "../project.test");
    assert_eq!(r.markers_all()[0].up(), 1);
    assert_eq!(r.lockfiles()[0].as_str(), "../project.lock");
    assert_eq!(r.required_levels(), 2);
    assert!(r.applies_to(Platform::Windows));
}

#[test]
fn bundled_rules_all_load_and_are_enabled() {
    let set = RuleSet::bundled().unwrap();
    assert!(set.len() >= 7);
    for id in [
        "rust.target",
        "node.node-modules",
        "node.next",
        "node.nuxt",
        "node.output",
        "node.turbo",
        "node.parcel-cache",
    ] {
        let r = set.get(id).unwrap_or_else(|| panic!("{id} missing"));
        assert_eq!(r.source(), RuleSource::Bundled);
        assert!(set.is_enabled(id));
        assert!(!r.markers_all().is_empty(), "{id} needs a marker (CLS-03)");
    }
    assert_eq!(set.get("rust.target").unwrap().base_tier(), Tier::S0);
    assert_eq!(set.enabled().count(), set.len());
    // The `quick` regeneration hint loads (node_modules: network, but quick with a lockfile).
    let g = set.get("node.node-modules").unwrap().regeneration();
    assert_eq!(
        (g.network(), g.long_build(), g.quick()),
        (Some(true), Some(false), Some(true))
    );
    assert_eq!(set.get("rust.target").unwrap().regeneration().quick(), None);
}

#[test]
fn every_rule_file_in_repo_is_bundled() {
    // CLS-05: a rule file that exists but isn't compiled in would silently never run.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut on_disk = Vec::new();
    let mut stack = vec![root.join("rules")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "toml") {
                let rel = path.strip_prefix(&root).unwrap();
                let rel: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                on_disk.push(rel.join("/"));
            }
        }
    }
    on_disk.sort();
    let mut bundled: Vec<String> = BUNDLED_RULE_FILES
        .iter()
        .map(|(o, _)| o.to_string())
        .collect();
    bundled.sort();
    assert_eq!(on_disk, bundled);
}

#[test]
fn bundled_rule_files_use_current_schema() {
    for (origin, text) in BUNDLED_RULE_FILES {
        let rules = parse_rules(text, origin, RuleSource::Bundled).unwrap();
        assert!(!rules.is_empty(), "{origin}");
        assert!(text.contains(&format!("schema_version = {SCHEMA_VERSION}")));
    }
}

#[test]
fn user_rules_load_disabled_until_enabled() {
    let mut set = RuleSet::bundled().unwrap();
    let ids = set.add_user_file(BASE, "user.toml").unwrap();
    assert_eq!(ids, vec!["test.build".to_string()]);
    assert!(!set.is_enabled("test.build"));
    assert!(set.enabled().all(|r| r.id() != "test.build"));
    set.enable("test.build").unwrap();
    assert!(set.is_enabled("test.build"));
    assert!(set.disable("test.build"));
    assert_eq!(
        set.enable("nope.nope").unwrap_err().kind,
        RuleErrorKind::UnknownRule
    );
}

#[test]
fn bundled_rules_may_assign_s0_and_tool_native() {
    let text = BASE
        .replace("base_tier = \"S1\"", "base_tier = \"S0\"")
        .replace("strategy = \"filesystem\"", "strategy = \"tool_native\"");
    let r = bundled(&text).unwrap();
    assert_eq!(r[0].base_tier(), Tier::S0);
    assert_eq!(r[0].strategy(), Strategy::ToolNative);
}

// ---------------------------------------------------------------------------------------------
// FX-CFG-06: a rule file with a command-execution field is rejected at load (CLS-04).

#[test]
fn fx_cfg_06_command_fields_are_rejected_at_load() {
    let cases = [
        ("command = \"rm -rf .\"\n", "rule[0].command"),
        (
            "cleanup_command = \"cargo clean\"\n",
            "rule[0].cleanup_command",
        ),
        ("cmd = [\"cargo\", \"clean\"]\n", "rule[0].cmd"),
        ("exec = \"sh\"\n", "rule[0].exec"),
        ("run = \"x\"\n", "rule[0].run"),
        ("args = [\"clean\"]\n", "rule[0].args"),
        ("shell = \"bash\"\n", "rule[0].shell"),
        ("preScript = \"x\"\n", "rule[0].preScript"),
        ("post_hook = \"x\"\n", "rule[0].post_hook"),
        ("program = \"/bin/sh\"\n", "rule[0].program"),
    ];
    for (line, key) in cases {
        let text = BASE.replace("\nversion = 1\n", &format!("\nversion = 1\n{line}"));
        for source in [RuleSource::User, RuleSource::Bundled] {
            let err = parse_rules(&text, "evil.toml", source).unwrap_err();
            assert_eq!(
                err.kind,
                RuleErrorKind::CommandField { key: key.into() },
                "{line:?} via {source:?}"
            );
            assert!(err.to_string().contains("CLS-04"));
        }
    }
}

#[test]
fn fx_cfg_06_command_fields_rejected_anywhere_in_the_file() {
    // Top level.
    let top = BASE.replace(
        "schema_version = 1\n",
        "schema_version = 1\ncommand = \"x\"\n",
    );
    assert_eq!(
        user(&top).unwrap_err(),
        RuleErrorKind::CommandField {
            key: "command".into()
        }
    );
    // Nested table.
    let nested = format!(
        "{BASE}\n[rule.regeneration]\ninstructions = \"x\"\nrun_command = \"cargo build\"\n"
    );
    assert_eq!(
        user(&nested).unwrap_err(),
        RuleErrorKind::CommandField {
            key: "rule[0].regeneration.run_command".into()
        }
    );
    // A dedicated table.
    let table = format!("{BASE}\n[rule.exec]\nprogram = \"sh\"\n");
    assert!(matches!(
        user(&table).unwrap_err(),
        RuleErrorKind::CommandField { .. }
    ));
    // Display text that merely mentions a command is fine (instructions are never executed).
    let ok = format!("{BASE}\n[rule.regeneration]\ninstructions = \"Run `cargo build`\"\n");
    assert!(user(&ok).is_ok());
}

// ---------------------------------------------------------------------------------------------
// FX-CFG-05 / CLS-04: non-bundled rules can't assign S0.

#[test]
fn fx_cfg_05_user_rule_assigning_s0_is_rejected_at_load() {
    let text = BASE.replace("base_tier = \"S1\"", "base_tier = \"S0\"");
    assert_eq!(user(&text).unwrap_err(), RuleErrorKind::S0NotAllowed);
    let mut set = RuleSet::new();
    assert!(set.add_user_file(&text, "user.toml").is_err());
    assert!(set.is_empty(), "nothing is added from a rejected file");
}

#[test]
fn user_rule_cannot_use_tool_native() {
    let text = BASE.replace("strategy = \"filesystem\"", "strategy = \"tool_native\"");
    assert_eq!(
        user(&text).unwrap_err(),
        RuleErrorKind::ToolNativeNotAllowed
    );
}

// ---------------------------------------------------------------------------------------------
// P6-COM-01: community rules never claim people's work folders and only apply inside projects.

#[test]
fn community_rules_cannot_claim_user_work_folders() {
    for name in [
        "src",
        "Source",
        "Documents",
        "Desktop",
        ".ssh",
        ".git",
        "Library",
        "Assets",
        "data",
        "Downloads",
        "OneDrive",
        "notes",
    ] {
        let text = BASE.replace("names = [\"build\"]", &format!("names = [\"{name}\"]"));
        let err = user(&text).unwrap_err();
        assert!(
            matches!(err, RuleErrorKind::UserWorkFolder { .. }),
            "{name}: {err:?}"
        );
        // Bundled rules are reviewed in-tree and keep their own folders (Unity `Library/`).
        if name != ".ssh" && name != ".git" {
            let _ = bundled(&text);
        }
    }
    // A pattern that merely *can* match a protected name is rejected too.
    for pattern in ["S{rc,ource}", "Doc*", "?ata"] {
        let text = BASE.replace("names = [\"build\"]", &format!("names = [\"{pattern}\"]"));
        assert!(
            matches!(
                user(&text).unwrap_err(),
                RuleErrorKind::UserWorkFolder { .. }
            ),
            "{pattern}"
        );
    }
    // Every protected name is lowercase so the case-insensitive match covers all spellings.
    for n in PROTECTED_FOLDER_NAMES {
        assert_eq!(*n, n.to_lowercase(), "{n}");
    }
}

#[test]
fn community_rules_need_specific_names() {
    for pattern in ["*", "?*", "{a,b}*", "x*"] {
        let text = BASE.replace("names = [\"build\"]", &format!("names = [\"{pattern}\"]"));
        assert!(
            matches!(user(&text).unwrap_err(), RuleErrorKind::WildcardName { .. }),
            "{pattern}"
        );
    }
    // Two literal characters are enough (`.gd*`), and bundled rules are not capped.
    let ok = BASE.replace("names = [\"build\"]", "names = [\"bu*\"]");
    assert!(user(&ok).is_ok());
}

#[test]
fn community_rules_need_a_project_marker_beside_or_above() {
    // Only a marker inside the candidate: not gated on a project.
    let inside = BASE.replace("all = [\"../project.test\"]", "all = [\"build.stamp\"]");
    assert_eq!(user(&inside).unwrap_err(), RuleErrorKind::NoProjectMarker);
    assert!(
        bundled(&inside).is_ok(),
        "bundled self-identifying folders stay allowed"
    );
    // `any` alternatives must all be outside, or one could pass with an inside marker only.
    let mixed = BASE.replace(
        "all = [\"../project.test\"]",
        "any = [\"../project.test\", \"build.stamp\"]",
    );
    assert_eq!(user(&mixed).unwrap_err(), RuleErrorKind::NoProjectMarker);
    let any_outside = BASE.replace(
        "all = [\"../project.test\"]",
        "any = [\"../project.test\", \"../../project.test\"]",
    );
    assert!(user(&any_outside).is_ok());
    let both = BASE.replace(
        "all = [\"../project.test\"]",
        "all = [\"../project.test\", \"build.stamp\"]",
    );
    assert!(
        user(&both).is_ok(),
        "an inside marker on top of a project marker is fine"
    );
}

// ---------------------------------------------------------------------------------------------
// CLS-03: a directory name alone is never enough.

#[test]
fn name_only_rule_is_rejected() {
    let no_markers = BASE.replace("[rule.markers]\nall = [\"../project.test\"]\n", "");
    for source in [RuleSource::User, RuleSource::Bundled] {
        let err = parse_rules(&no_markers, "x.toml", source).unwrap_err();
        assert_eq!(err.kind, RuleErrorKind::NameOnly);
    }
    let empty = BASE.replace("all = [\"../project.test\"]", "all = []");
    assert_eq!(bundled(&empty).unwrap_err(), RuleErrorKind::NameOnly);
}

#[test]
fn marker_satisfiable_by_the_candidate_itself_is_rejected() {
    for marker in ["../build", "../BUILD", "../bui?d", "../{build,out}"] {
        let text = BASE.replace("../project.test", marker);
        assert!(
            matches!(
                bundled(&text).unwrap_err(),
                RuleErrorKind::MarkerMatchesCandidate { .. }
            ),
            "{marker}"
        );
    }
    // Wildcard candidate names with a wildcard sibling marker can't be proven disjoint.
    let text = BASE
        .replace("names = [\"build\"]", "names = [\"build-*\"]")
        .replace("../project.test", "../*.test");
    assert!(matches!(
        bundled(&text).unwrap_err(),
        RuleErrorKind::MarkerMatchesCandidate { .. }
    ));
}

#[test]
fn bare_wildcard_marker_is_rejected() {
    for marker in ["../*", "../?*", "*"] {
        let text = BASE.replace("../project.test", marker);
        assert!(
            matches!(
                bundled(&text).unwrap_err(),
                RuleErrorKind::WildcardMarker { .. }
            ),
            "{marker}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Strict schema.

#[test]
fn unknown_fields_are_rejected() {
    let top = BASE.replace("schema_version = 1\n", "schema_version = 1\nextra = true\n");
    assert!(matches!(user(&top).unwrap_err(), RuleErrorKind::Schema(_)));
    let in_rule = BASE.replace("\nversion = 1\n", "\nversion = 1\ncolour = \"red\"\n");
    assert!(matches!(
        user(&in_rule).unwrap_err(),
        RuleErrorKind::Schema(_)
    ));
    let nested = BASE.replace("names = [\"build\"]", "names = [\"build\"]\nregex = \".*\"");
    assert!(matches!(
        user(&nested).unwrap_err(),
        RuleErrorKind::Schema(_)
    ));
    let enabled = BASE.replace("\nversion = 1\n", "\nversion = 1\nenabled = true\n");
    assert!(
        matches!(user(&enabled).unwrap_err(), RuleErrorKind::Schema(_)),
        "a rule file can't enable itself"
    );
}

#[test]
fn bad_enum_values_and_missing_fields_are_rejected() {
    let tier = BASE.replace("base_tier = \"S1\"", "base_tier = \"S9\"");
    assert!(matches!(user(&tier).unwrap_err(), RuleErrorKind::Schema(_)));
    let platform = BASE.replace("\"windows\"]", "\"plan9\"]");
    assert!(matches!(
        user(&platform).unwrap_err(),
        RuleErrorKind::Schema(_)
    ));
    let missing = BASE.replace("artifact = \"Test build output\"\n", "");
    assert!(matches!(
        user(&missing).unwrap_err(),
        RuleErrorKind::Schema(_)
    ));
    let category = BASE.replace("project_artifact", "scull_data");
    assert!(matches!(
        user(&category).unwrap_err(),
        RuleErrorKind::ForbiddenCategory(_)
    ));
}

#[test]
fn schema_version_is_checked() {
    let wrong = BASE.replace("schema_version = 1", "schema_version = 2");
    assert_eq!(
        user(&wrong).unwrap_err(),
        RuleErrorKind::UnsupportedSchemaVersion {
            found: 2,
            supported: SCHEMA_VERSION
        }
    );
    let zero = BASE.replace("schema_version = 1", "schema_version = 0");
    assert!(matches!(
        user(&zero).unwrap_err(),
        RuleErrorKind::UnsupportedSchemaVersion { found: 0, .. }
    ));
    let missing = BASE.replace("schema_version = 1\n", "");
    assert_eq!(
        user(&missing).unwrap_err(),
        RuleErrorKind::MissingSchemaVersion
    );
    let string = BASE.replace("schema_version = 1", "schema_version = \"1\"");
    assert!(matches!(
        user(&string).unwrap_err(),
        RuleErrorKind::Schema(_)
    ));
}

#[test]
fn syntax_errors_and_empty_files_are_rejected() {
    assert!(matches!(
        user("[[rule").unwrap_err(),
        RuleErrorKind::Syntax(_)
    ));
    assert_eq!(
        user("schema_version = 1\n").unwrap_err(),
        RuleErrorKind::NoRules
    );
}

#[test]
fn duplicate_ids_are_rejected() {
    let second = BASE.split_once("[[rule]]").unwrap().1;
    let twice = format!("{BASE}\n[[rule]]{second}");
    assert_eq!(bundled(&twice).unwrap_err(), RuleErrorKind::DuplicateId);

    // Across files: a user rule can't shadow a bundled one.
    let mut set = RuleSet::bundled().unwrap();
    let shadow = BASE.replace("test.build", "rust.target");
    let err = set.add_user_file(&shadow, "user.toml").unwrap_err();
    assert_eq!(err.kind, RuleErrorKind::DuplicateId);
    assert_eq!(err.rule_id.as_deref(), Some("rust.target"));
    assert_eq!(
        set.get("rust.target").unwrap().source(),
        RuleSource::Bundled
    );
}

#[test]
fn ids_and_versions_are_validated() {
    for id in [
        "build",
        "Test.build",
        "test..build",
        "test.",
        ".x",
        "1x.y",
        "test.bu ild",
    ] {
        let text = BASE.replace("\"test.build\"", &format!("\"{id}\""));
        assert!(
            matches!(user(&text).unwrap_err(), RuleErrorKind::InvalidId(_)),
            "{id}"
        );
    }
    let v0 = BASE.replace("\nversion = 1\n", "\nversion = 0\n");
    assert_eq!(user(&v0).unwrap_err(), RuleErrorKind::InvalidVersion);
}

#[test]
fn bad_globs_are_rejected() {
    for pat in ["", "a/b", "**", "[ab]", "{a", "a}", "{a}", "..", "x\\\\y"] {
        let text = BASE.replace("names = [\"build\"]", &format!("names = [\"{pat}\"]"));
        assert!(
            matches!(
                user(&text).unwrap_err(),
                RuleErrorKind::InvalidPattern { .. }
            ),
            "{pat:?}"
        );
    }
    let within = format!("{BASE}\n[rule.exclude]\nwithin = [\"a/**\"]\n");
    assert!(matches!(
        user(&within).unwrap_err(),
        RuleErrorKind::InvalidPattern { .. }
    ));
}

#[test]
fn bad_relative_paths_are_rejected() {
    for path in [
        "/etc/passwd",
        "~/x",
        "C:/x",
        "..\\\\x",
        "../a/b",
        "a/../b",
        "../../../../../x",
        "..//x",
        "../",
        "../{a",
    ] {
        let text = BASE.replace("../project.test", path);
        assert!(
            matches!(user(&text).unwrap_err(), RuleErrorKind::InvalidPath { .. }),
            "{path:?}"
        );
    }
    for lock in ["../x", "/abs", "a/*", "a/./b"] {
        let text = format!("{BASE}\n[rule.activity]\nlock_files = [\"{lock}\"]\n");
        assert!(
            matches!(user(&text).unwrap_err(), RuleErrorKind::InvalidPath { .. }),
            "{lock:?}"
        );
    }
}

#[test]
fn evidence_must_match_rule() {
    let no_marker = BASE.replace("[\"marker\", \"lockfile\"]", "[\"lockfile\"]");
    assert!(matches!(
        user(&no_marker).unwrap_err(),
        RuleErrorKind::EvidenceMismatch(_)
    ));
    let no_lock_ev = BASE.replace("[\"marker\", \"lockfile\"]", "[\"marker\"]");
    assert!(matches!(
        user(&no_lock_ev).unwrap_err(),
        RuleErrorKind::EvidenceMismatch(_)
    ));
    let no_lockfiles = BASE.replace("[rule.lockfiles]\nany = [\"../project.lock\"]\n", "");
    assert!(matches!(
        user(&no_lockfiles).unwrap_err(),
        RuleErrorKind::EvidenceMismatch(_)
    ));
    let dup = BASE.replace(
        "[\"marker\", \"lockfile\"]",
        "[\"marker\", \"marker\", \"lockfile\"]",
    );
    assert!(matches!(
        user(&dup).unwrap_err(),
        RuleErrorKind::InvalidField {
            field: "evidence",
            ..
        }
    ));
}

#[test]
fn other_field_validation() {
    let no_platforms = BASE.replace("[\"macos\", \"linux\", \"windows\"]", "[]");
    assert!(matches!(
        user(&no_platforms).unwrap_err(),
        RuleErrorKind::InvalidField {
            field: "platforms",
            ..
        }
    ));
    let dup_platform = BASE.replace(
        "[\"macos\", \"linux\", \"windows\"]",
        "[\"linux\", \"linux\"]",
    );
    assert!(matches!(
        user(&dup_platform).unwrap_err(),
        RuleErrorKind::InvalidField {
            field: "platforms",
            ..
        }
    ));
    let no_names = BASE.replace("names = [\"build\"]", "names = []");
    assert!(matches!(
        user(&no_names).unwrap_err(),
        RuleErrorKind::InvalidField {
            field: "match.names",
            ..
        }
    ));
    let process = format!("{BASE}\n[rule.activity]\nprocesses = [\"cargo; rm -rf /\"]\n");
    assert!(matches!(
        user(&process).unwrap_err(),
        RuleErrorKind::InvalidField {
            field: "activity.processes",
            ..
        }
    ));
    let single_any = BASE.replace(
        "all = [\"../project.test\"]",
        "all = [\"../project.test\"]\nany = [\"../other\"]",
    );
    assert!(matches!(
        user(&single_any).unwrap_err(),
        RuleErrorKind::InvalidField {
            field: "markers.any",
            ..
        }
    ));
    let empty_artifact = BASE.replace("\"Test build output\"", "\"  \"");
    assert!(matches!(
        user(&empty_artifact).unwrap_err(),
        RuleErrorKind::InvalidField {
            field: "artifact",
            ..
        }
    ));
}

#[test]
fn errors_name_the_file_and_rule() {
    let text = BASE.replace("base_tier = \"S1\"", "base_tier = \"S0\"");
    let err = parse_rules(&text, "~/.config/scull/rules/mine.toml", RuleSource::User).unwrap_err();
    assert_eq!(err.origin, "~/.config/scull/rules/mine.toml");
    assert_eq!(err.rule_id.as_deref(), Some("test.build"));
    let msg = err.to_string();
    assert!(
        msg.contains("mine.toml") && msg.contains("test.build"),
        "{msg}"
    );
}

// ---------------------------------------------------------------------------------------------
// Pure marker evaluation (no I/O).

fn rule(id: &str) -> scull_detectors::rules::Rule {
    RuleSet::bundled()
        .expect("bundled rules load")
        .get(id)
        .expect("bundled rule exists")
        .clone()
}

#[test]
fn rust_target_with_manifest_and_lockfile_matches() {
    // FX-CLS-01 shape: target/ beside Cargo.toml and Cargo.lock.
    let r = rule("rust.target");
    let s = Surroundings::new("target")
        .with_inside(["CACHEDIR.TAG", "debug"])
        .with_siblings(["Cargo.toml", "Cargo.lock", "src", "target"])
        .with_ancestor_names(["proj"]);
    let m = r.check_markers(&s);
    let m = m.matched().expect("matched");
    assert_eq!(m.markers[0].relative_path(), "../Cargo.toml");
    assert_eq!(m.markers[1].relative_path(), "CACHEDIR.TAG");
    assert_eq!(
        m.lockfile.as_ref().unwrap().relative_path(),
        "../Cargo.lock"
    );
    assert!(!m.lockfile_missing);

    let ev = m.evidence(&r, Path::new("/proj/target"));
    assert_eq!(ev.len(), 4);
    assert!(
        matches!(&ev[0], Evidence::Marker { path } if path.to_path_buf() == Path::new("/proj").join("Cargo.toml"))
    );
    assert!(
        matches!(&ev[1], Evidence::Marker { path } if path.to_path_buf() == Path::new("/proj/target").join("CACHEDIR.TAG"))
    );
    assert!(
        matches!(&ev[2], Evidence::Lockfile { path } if path.to_path_buf() == Path::new("/proj").join("Cargo.lock"))
    );
    assert!(matches!(&ev[3], Evidence::KnownArtifact { .. }));
    assert!(r.evidence().contains(&EvidenceKind::KnownArtifact));
}

#[test]
fn target_without_cargo_toml_is_not_a_candidate() {
    // FX-CLS-02 shape: a folder named target/ full of photos, no Cargo.toml.
    let r = rule("rust.target");
    let s = Surroundings::new("target")
        .with_inside(["IMG_0001.jpg", "IMG_0002.jpg"])
        .with_siblings(["target", "Photos", "notes.txt"])
        .with_ancestor_names(["me"]);
    assert_eq!(
        r.check_markers(&s),
        MarkerOutcome::MissingMarker {
            marker: "../Cargo.toml".into()
        }
    );
    // Marker names match case-sensitively: `cargo.toml` is not a Cargo marker.
    let s = Surroundings::new("target")
        .with_inside(["CACHEDIR.TAG"])
        .with_siblings(["target", "cargo.toml"])
        .with_ancestor_names(["me"]);
    assert!(r.check_markers(&s).matched().is_none());
    // A Cargo.toml beside it is not enough: Cargo didn't create this target/ (no CACHEDIR.TAG).
    let s = Surroundings::new("target")
        .with_inside(["IMG_0001.jpg"])
        .with_siblings(["target", "Cargo.toml", "Cargo.lock"])
        .with_ancestor_names(["me"]);
    assert_eq!(
        r.check_markers(&s),
        MarkerOutcome::MissingMarker {
            marker: "CACHEDIR.TAG".into()
        }
    );
}

#[test]
fn missing_lockfile_is_reported() {
    // FX-CLS-05 shape (no lockfile): still a candidate, flagged so the classifier makes it S2.
    let r = rule("node.node-modules");
    let s = Surroundings::new("node_modules")
        .with_siblings(["package.json", "node_modules", "src"])
        .with_ancestor_names(["app", "home"]);
    let m = r.check_markers(&s);
    let m = m.matched().unwrap();
    assert!(m.lockfile_missing && m.lockfile.is_none());
}

#[test]
fn exclusions_apply_case_insensitively() {
    let r = rule("rust.target");
    let s = Surroundings::new("target")
        .with_inside(["CACHEDIR.TAG"])
        .with_siblings(["Cargo.toml", "Cargo.lock", "POM.XML"])
        .with_ancestor_names(["proj"]);
    assert_eq!(
        r.check_markers(&s),
        MarkerOutcome::Excluded {
            by: "../pom.xml".into()
        }
    );
}

#[test]
fn nested_node_modules_is_excluded() {
    let r = rule("node.node-modules");
    let s = Surroundings::new("node_modules")
        .with_siblings(["package.json", "package-lock.json", "node_modules"])
        .with_ancestor_names(["left-pad", "node_modules", "app"]);
    assert!(matches!(
        r.check_markers(&s),
        MarkerOutcome::Excluded { .. }
    ));
}

#[test]
fn unknown_levels_are_incomplete_not_absent() {
    let r = rule("rust.target");
    assert_eq!(
        r.check_markers(&Surroundings::new("target")),
        MarkerOutcome::Incomplete {
            missing_level: Some(0)
        }
    );
    assert_eq!(
        r.check_markers(&Surroundings::new("target").with_inside(["CACHEDIR.TAG"])),
        MarkerOutcome::Incomplete {
            missing_level: Some(1)
        }
    );
    let r = rule("node.node-modules");
    let s = Surroundings::new("node_modules").with_siblings(["package.json", "yarn.lock"]);
    assert_eq!(
        r.check_markers(&s),
        MarkerOutcome::Incomplete {
            missing_level: None
        }
    );
}

#[test]
fn name_mismatch_and_any_group() {
    let r = rule("node.next");
    let s = Surroundings::new("dist").with_siblings(["package.json"]);
    assert_eq!(r.check_markers(&s), MarkerOutcome::NameMismatch);

    let base = ["package.json", "pnpm-lock.yaml", ".next"];
    let s = Surroundings::new(".next")
        .with_siblings(base)
        .with_ancestor_names(Vec::<String>::new());
    assert!(matches!(
        r.check_markers(&s),
        MarkerOutcome::MissingMarker { marker } if marker.starts_with("any of")
    ));

    let mut with_config = base.to_vec();
    with_config.push("next.config.mjs");
    let s = Surroundings::new(".next")
        .with_siblings(with_config)
        .with_ancestor_names(Vec::<String>::new());
    let out = r.check_markers(&s);
    let m = out.matched().unwrap();
    assert_eq!(m.markers.len(), 2);
    assert_eq!(m.markers[1].name, "next.config.mjs");
    assert_eq!(m.lockfile.as_ref().unwrap().name, "pnpm-lock.yaml");
}

#[test]
fn candidate_never_counts_as_its_own_marker() {
    // Even if a wildcard sibling marker would match the candidate's own name.
    let text = BASE
        .replace("names = [\"build\"]", "names = [\"out\"]")
        .replace("../project.test", "../*.test");
    let r = &bundled(&text).unwrap()[0];
    let s = Surroundings::new("out").with_siblings(["out"]);
    assert!(r.check_markers(&s).matched().is_none());
    // A marker inside the candidate (level 0), e.g. CMakeCache.txt in a build dir.
    let inside = BASE
        .replace("../project.test", "CMakeCache.txt")
        .replace("names = [\"build\"]", "names = [\"*\"]");
    let r = &bundled(&inside).unwrap()[0];
    assert_eq!(r.required_levels(), 2);
    let s = Surroundings::new("cmake-build-debug")
        .with_inside(["CMakeCache.txt", "CMakeFiles"])
        .with_siblings(["project.lock"]);
    assert!(r.check_markers(&s).matched().is_some());
}

#[test]
fn platform_current_is_supported_on_ci_targets() {
    if cfg!(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )) {
        assert!(Platform::current().is_some());
    }
    let set = RuleSet::bundled().unwrap();
    if let Some(p) = Platform::current() {
        // Every bundled rule applies everywhere except a few single-OS ones: the macOS-only
        // Xcode rules and Linux-only Podman storage.
        let single_os = |os: Platform| {
            set.all()
                .filter(move |r| r.platforms() == [os])
                .map(|r| r.id().to_owned())
                .collect::<Vec<_>>()
        };
        assert!(
            single_os(Platform::Macos)
                .iter()
                .all(|id| id.starts_with("xcode.")),
            "{:?}",
            single_os(Platform::Macos)
        );
        assert_eq!(single_os(Platform::Linux), ["podman.storage"]);
        assert!(single_os(Platform::Windows).is_empty());
        let want = set.all().filter(|r| r.platforms().contains(&p)).count();
        assert_eq!(set.enabled_for(p).count(), want);
        let elsewhere_only = [Platform::Linux, Platform::Macos, Platform::Windows]
            .into_iter()
            .filter(|os| *os != p)
            .map(|os| single_os(os).len())
            .sum::<usize>();
        assert_eq!(want, set.len() - elsewhere_only);
    }
}
