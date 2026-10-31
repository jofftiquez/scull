//! End-to-end tests of `scull large-files` (P1-DET-05): human table, `--json` validated against
//! the committed envelope schema and every entry against `schemas/v1/candidate.schema.json`,
//! `--ndjson`, links not followed, and nothing on disk changed (I-2, I-3).

#![allow(clippy::unwrap_used)]

use scull_fixtures::{Fixture, InvariantInputs, LinkOutcome};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const KIB: u64 = 1024;

fn scull(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scull"))
        .args(args)
        .env("TERM", "dumb")
        .env_remove("NO_COLOR")
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn validator(path: &Path) -> jsonschema::Validator {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    jsonschema::validator_for(&v).unwrap()
}

fn errors(v: &jsonschema::Validator, doc: &Value) -> Vec<String> {
    v.iter_errors(doc).map(|e| e.to_string()).collect()
}

/// A tree with three large files of known kinds, a small one, and (where permitted) a symlink
/// to the largest file.
fn tree() -> (Fixture, bool) {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("vms/ubuntu.vmdk", 768 * KIB)
        .unwrap()
        .file_of_size("models/llama.gguf", 512 * KIB)
        .unwrap()
        .file_of_size("isos/debian.iso", 300 * KIB)
        .unwrap()
        .file("notes.txt", "small")
        .unwrap();
    let target = fx.path("vms/ubuntu.vmdk").unwrap();
    let linked = fx.symlink("models/link.vmdk", &target).unwrap() == LinkOutcome::Created;
    fx.checkpoint().unwrap();
    (fx, linked)
}

fn root(fx: &Fixture) -> String {
    fx.root().to_string_lossy().into_owned()
}

#[test]
fn human_output_lists_largest_files_inspect_only() {
    let (fx, _) = tree();
    let o = scull(&["large-files", &root(&fx), "--min-size", "256K"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let out = stdout(&o);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].contains("Allocated") && lines[0].contains("Logical"));
    assert!(lines[1].contains("VM disk image") && lines[1].contains("ubuntu.vmdk"));
    assert!(lines[2].contains("Model weights") && lines[2].contains("llama.gguf"));
    assert!(lines[3].contains("Disk image") && lines[3].contains("debian.iso"));
    assert!(!out.contains("notes.txt") && !out.contains("link.vmdk"));
    assert!(out.contains("3 file(s) of 256.0 KiB or more"));
    assert!(out.contains("Inspect only"));

    let top1 = stdout(&scull(&[
        "large-files",
        &root(&fx),
        "--min-size",
        "256K",
        "--top",
        "1",
    ]));
    assert!(top1.contains("showing the largest 1"));
    let none = stdout(&scull(&["large-files", &root(&fx)]));
    assert!(none.contains("No files of 500.0 MiB or more"), "{none}");
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// `crates/scull-cli/schemas/large-files.schema.json` is the published envelope schema.
/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test large_files`.
#[test]
fn json_validates_against_committed_schemas() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let envelope_path = manifest.join("schemas/large-files.schema.json");
    if std::env::var_os("SCULL_BLESS").is_some() {
        let s =
            serde_json::to_string_pretty(&scull_cli::large_files::large_files_schema()).unwrap();
        std::fs::write(&envelope_path, s + "\n").unwrap();
    }
    let committed: Value =
        serde_json::from_str(&std::fs::read_to_string(&envelope_path).unwrap()).unwrap();
    let generated = serde_json::to_value(scull_cli::large_files::large_files_schema()).unwrap();
    assert_eq!(
        committed, generated,
        "schema is stale; regenerate with SCULL_BLESS=1"
    );
    let envelope = validator(&envelope_path);
    let candidate = validator(&manifest.join("../../schemas/v1/candidate.schema.json"));

    let (fx, linked) = tree();
    let o = scull(&["large-files", &root(&fx), "--min-size", "256K", "--json"]);
    assert_eq!(o.status.code(), Some(0));
    let doc: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(doc["schema"], "scull/large-files@1");
    assert!(
        errors(&envelope, &doc).is_empty(),
        "{:?}",
        errors(&envelope, &doc)
    );
    let files = doc["data"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 3);
    for f in files {
        assert!(
            errors(&candidate, f).is_empty(),
            "{:?}",
            errors(&candidate, f)
        );
        assert_eq!(f["category"], "large_file");
        assert_eq!(f["tier"], "S2");
        assert_eq!(f["action"]["type"], "none");
        assert_eq!(f["reasons"][0]["code"], "inspect_only");
    }
    assert_eq!(files[0]["artifact"], "VM disk image");
    let summary = &doc["data"]["summary"];
    assert_eq!(summary["matched"], 3);
    assert_eq!(summary["threshold_bytes"], 256 * KIB);
    assert_eq!(summary["truncated"], false);
    assert_eq!(summary["complete"], true);
    if linked {
        assert!(
            !stdout(&o).contains("link.vmdk"),
            "links are never followed"
        );
    }
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn ndjson_streams_candidates_then_summary() {
    let (fx, _) = tree();
    let o = scull(&[
        "large-files",
        &root(&fx),
        "--min-size",
        "256K",
        "--ndjson",
        "--top",
        "2",
    ]);
    assert_eq!(o.status.code(), Some(0));
    let recs: Vec<Value> = stdout(&o)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let kinds: Vec<&str> = recs.iter().map(|r| r["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["candidate", "candidate", "summary"]);
    assert!(recs.iter().all(|r| r["schema"] == "scull/large-files@1"));
    assert_eq!(recs[2]["data"]["truncated"], true);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn bad_inputs_are_usage_or_not_found_errors() {
    let (fx, _) = tree();
    let missing = fx.root().join("missing");
    let o = scull(&["large-files", &missing.to_string_lossy(), "--json"]);
    assert_ne!(o.status.code(), Some(0));
    assert!(o.stdout.is_empty());
    let err: Value = serde_json::from_slice(&o.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "not_found");
    let file = fx.root().join("notes.txt");
    assert_eq!(
        scull(&["large-files", &file.to_string_lossy()])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        scull(&["large-files", ".", "--top", "0"]).status.code(),
        Some(2)
    );
    assert_eq!(
        scull(&["large-files", ".", "--min-size", "lots"])
            .status
            .code(),
        Some(2)
    );
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
