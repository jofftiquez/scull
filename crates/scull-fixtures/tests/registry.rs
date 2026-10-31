//! The fixture registry against `docs/FIXTURES.md` (the source of truth) and the test sources
//! it points at. Paths are resolved from `CARGO_MANIFEST_DIR`, so this runs from any cwd.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_fixtures::registry::{
    COVERED_SECTIONS, DocFixture, REGISTRY, Status, TestRef, lookup, parse_fixture_matrix,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn doc_fixtures() -> Vec<DocFixture> {
    let path = workspace_root().join("docs/FIXTURES.md");
    let text = fs::read_to_string(&path).expect("read docs/FIXTURES.md");
    let rows = parse_fixture_matrix(&text);
    assert!(rows.len() > 100, "parsed only {} fixture rows", rows.len());
    rows
}

fn covered(id: &str) -> bool {
    COVERED_SECTIONS
        .iter()
        .any(|p| id.strip_prefix(p).is_some_and(|rest| rest.starts_with('-')))
}

/// Every fixture of a covered section is registered with the document's gate, OS and mode, and
/// nothing is registered that the document doesn't list.
#[test]
fn registry_matches_fixtures_md() {
    let doc = doc_fixtures();
    let mut problems = Vec::new();
    for d in doc.iter().filter(|d| covered(&d.id)) {
        let Some(r) = lookup(&d.id) else {
            problems.push(format!("{} ({}) is not registered", d.id, d.section));
            continue;
        };
        if Some(r.gate) != d.gate {
            problems.push(format!("{}: gate {:?} != doc {:?}", d.id, r.gate, d.gate));
        }
        if Some(r.mode) != d.mode {
            problems.push(format!("{}: mode {:?} != doc {:?}", d.id, r.mode, d.mode));
        }
        if r.os != d.os {
            problems.push(format!("{}: OS {:?} != doc {:?}", d.id, r.os, d.os));
        }
    }
    let doc_ids: BTreeSet<&str> = doc.iter().map(|d| d.id.as_str()).collect();
    for r in REGISTRY {
        if !doc_ids.contains(r.id) {
            problems.push(format!("{} is registered but not in FIXTURES.md", r.id));
        }
        if !covered(r.id) {
            problems.push(format!("{} is outside COVERED_SECTIONS", r.id));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The MVP release gate: every MVP-gated CI fixture of a covered section is implemented, or
/// explicitly tracked by an open issue with a reason.
#[test]
fn every_mvp_ci_fixture_is_implemented_or_tracked() {
    let mut problems = Vec::new();
    let mut report = Vec::new();
    for d in doc_fixtures()
        .iter()
        .filter(|d| covered(&d.id) && d.is_mvp_ci())
    {
        match lookup(&d.id).map(|r| r.status) {
            Some(Status::Implemented(tests)) if !tests.is_empty() => {
                report.push(format!("{}: implemented by {}", d.id, tests[0]));
            }
            Some(Status::Tracked {
                issue,
                reason,
                partial,
            }) if issue > 0 && reason.split_whitespace().count() >= 5 => {
                report.push(format!(
                    "{}: tracked by #{issue} ({} partial test(s))",
                    d.id,
                    partial.len()
                ));
            }
            other => problems.push(format!(
                "{} is MVP/CI but neither implemented nor tracked: {other:?}",
                d.id
            )),
        }
    }
    for line in &report {
        println!("{line}");
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// `Deferred` is only for fixtures the MVP CI gate doesn't require.
#[test]
fn deferred_fixtures_are_not_mvp_ci() {
    for r in REGISTRY {
        if r.status == Status::Deferred {
            assert!(!r.is_mvp_ci(), "{} is MVP/CI and can't be deferred", r.id);
        }
    }
}

/// Whether `source` defines `#[test] fn name(`: the function exists and one of the attribute
/// lines right above it is `#[test]`.
fn defines_test(source: &str, name: &str) -> bool {
    let lines: Vec<&str> = source.lines().collect();
    let sig = format!("fn {name}(");
    lines.iter().enumerate().any(|(i, l)| {
        let l = l.trim_start();
        (l.starts_with(&sig) || l.starts_with(&format!("pub {sig}")))
            && lines[..i]
                .iter()
                .rev()
                .take_while(|a| a.trim_start().starts_with("#[") || a.trim().starts_with("//"))
                .any(|a| a.trim() == "#[test]")
    })
}

/// Every test the registry names exists, as a `#[test]` function, in the file it names.
#[test]
fn every_registered_test_fn_exists() {
    let root = workspace_root();
    let mut problems = Vec::new();
    let mut checked = 0;
    for r in REGISTRY {
        for TestRef { file, test_fn } in r.status.tests() {
            let path = root.join(file);
            let Ok(source) = fs::read_to_string(&path) else {
                problems.push(format!("{}: {file} does not exist", r.id));
                continue;
            };
            if !defines_test(&source, test_fn) {
                problems.push(format!("{}: no #[test] fn {test_fn} in {file}", r.id));
            }
            let want = r.id.to_ascii_lowercase().replace('-', "_");
            if !test_fn.starts_with(&want) {
                problems.push(format!("{}: {test_fn} is not named after the ID", r.id));
            }
            checked += 1;
        }
    }
    assert!(checked > 0);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn defines_test_needs_the_attribute() {
    let src =
        "#[test]\n#[cfg(unix)]\nfn fx_a() {}\nfn fx_b() {}\n// x\n#[test]\n// y\nfn fx_c() {}\n";
    assert!(defines_test(src, "fx_a"));
    assert!(!defines_test(src, "fx_b"));
    assert!(defines_test(src, "fx_c"));
    assert!(!defines_test(src, "fx_d"));
}
