//! The fixture registry against `docs/FIXTURES.md` (the source of truth) and the test sources
//! it points at. Paths are resolved from `CARGO_MANIFEST_DIR`, so this runs from any cwd.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_fixtures::registry::{
    COVERED_SECTIONS, DocFixture, OsRun, REGISTRY, Status, TestRef, lookup, parse_fixture_matrix,
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

/// The signed manual release checklist (docs/FIXTURES.md §2.2): the text from its heading to
/// the next `### ` or `## ` heading.
fn manual_checklist() -> String {
    let text = fs::read_to_string(workspace_root().join("docs/FIXTURES.md")).unwrap();
    let start = text
        .find("### 2.2 Manual release checklist")
        .expect("docs/FIXTURES.md has §2.2 Manual release checklist");
    let rest = &text[start + 4..];
    let end = [rest.find("\n### "), rest.find("\n## ")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

/// `ManualChecklist` is only for Manual-mode fixtures, and each has a checklist entry
/// (a `**FX-...**` item with checkboxes) in docs/FIXTURES.md §2.2.
#[test]
fn manual_checklist_fixtures_have_a_checklist_entry() {
    let checklist = manual_checklist();
    let mut checked = 0;
    for r in REGISTRY {
        if r.status != Status::ManualChecklist {
            continue;
        }
        assert_eq!(r.mode, scull_fixtures::registry::Mode::Manual, "{}", r.id);
        let entry = format!("**{}**", r.id);
        let at = checklist
            .find(&entry)
            .unwrap_or_else(|| panic!("{}: no {entry} entry in docs/FIXTURES.md §2.2", r.id));
        let body = &checklist[at..];
        let body = &body[..body[entry.len()..]
            .find("\n**FX-")
            .map_or(body.len(), |i| i + entry.len())];
        assert!(
            body.matches("- [ ]").count() >= 3,
            "{}: the checklist entry needs at least three steps to tick",
            r.id
        );
        checked += 1;
    }
    assert!(checked > 0);
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

/// Whether `source` is a custom test harness (`harness = false`) that registers `name`: it
/// defines `fn name()` and lists `"name"` in its `const TESTS` table.
fn defines_custom_harness_test(source: &str, name: &str) -> bool {
    source.contains("const TESTS")
        && source.contains(&format!("\"{name}\""))
        && source
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("fn {name}()")))
}

/// Every test the registry names exists, as a `#[test]` function (or an entry of a custom
/// harness's `TESTS` table), in the file it names.
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
            if !defines_test(&source, test_fn) && !defines_custom_harness_test(&source, test_fn) {
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

#[test]
fn defines_custom_harness_test_needs_the_table_entry() {
    let src = "const TESTS: &[Test] = &[(\"fx_a\", fx_a)];\nfn fx_a() {}\nfn fx_b() {}\n";
    assert!(defines_custom_harness_test(src, "fx_a"));
    assert!(!defines_custom_harness_test(src, "fx_b"));
    assert!(!defines_custom_harness_test(
        "fn fx_a() {}\n\"fx_a\"",
        "fx_a"
    ));
}

/// Per-OS run status: `pending_ci` and `missing_os` name only OSs the fixture runs on (never the
/// same one), `pending_ci` only for fixtures with tests, `missing_os` only for tracked ones; the
/// report lists, for every implemented or tracked fixture, where its tests ran green, where they
/// wait for CI and where none exists yet.
#[test]
fn per_os_status_is_consistent_and_reported() {
    let mut problems = Vec::new();
    for r in REGISTRY {
        let letters = r.os_letters();
        for (field, value) in [("pending_ci", r.pending_ci), ("missing_os", r.missing_os)] {
            for c in value.chars() {
                if !letters.contains(&c) {
                    problems.push(format!(
                        "{}: {field} {c:?} is not one of its OSs {:?}",
                        r.id, r.os
                    ));
                }
            }
        }
        if let Some(c) = r.pending_ci.chars().find(|c| r.missing_os.contains(*c)) {
            problems.push(format!("{}: {c:?} is both pending CI and missing", r.id));
        }
        if !r.pending_ci.is_empty() && r.status.tests().is_empty() {
            problems.push(format!("{}: pending_ci without any test", r.id));
        }
        if !r.missing_os.is_empty() && !matches!(r.status, Status::Tracked { .. }) {
            problems.push(format!(
                "{}: misses {:?} but is not tracked by an issue",
                r.id, r.missing_os
            ));
        }
        if r.status.tests().is_empty() {
            continue;
        }
        let status: Vec<String> = r
            .os_status()
            .iter()
            .map(|(c, run)| match run {
                OsRun::Verified => format!("{c}:ran"),
                OsRun::PendingCi => format!("{c}:pending-CI"),
                OsRun::Missing => format!("{c}:MISSING"),
            })
            .collect();
        let kind = match r.status {
            Status::Implemented(_) => "implemented",
            Status::Tracked { .. } => "partial",
            Status::ManualChecklist => "manual",
            Status::Deferred => "deferred",
        };
        println!("{} [{kind}] {}", r.id, status.join(" "));
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
