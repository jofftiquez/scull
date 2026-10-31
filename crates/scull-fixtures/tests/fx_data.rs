//! End-to-end FX-DATA fixtures (docs/FIXTURES.md §3.13) through the real `scull` binary and the
//! real Executor, beside the storage-level fixtures in `scull-storage`:
//!
//! * **FX-DATA-03**: the files scull itself creates (the history database the CLI writes, and
//!   the audit database and NDJSON mirror an Executor operation writes) are owner-only.
//! * **FX-DATA-06**: a corrupted database is detected and left byte-identical, a backup is
//!   offered, and the read-only scan still works.
//! * **FX-DATA-07**: no file contents and no environment values reach scull's database, its
//!   audit log (database rows and NDJSON mirror), exports, or any command's output.
//!
//! Every run uses a temp `SCULL_HOME`; every fixture checks the invariants.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cli;
mod exec;

use cli::{Cli, Schemas, assert_exit, stderr, stdout};
use exec::engine::{harness, inputs, plan, run, store, three_tiers};
use scull_core::OperationId;
use scull_fixtures::{Fixture, InvariantId, InvariantReport, NodeLock, Outcome};
use std::path::{Path, PathBuf};

fn check(fx: &Fixture, inputs: &scull_fixtures::InvariantInputs) -> InvariantReport {
    let report = fx.check_invariants(inputs);
    println!("{report}");
    report.assert_ok();
    report
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Every directory and file below `dir` (and `dir` itself), never following links.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.to_path_buf()];
    let mut i = 0;
    while i < out.len() {
        let p = out[i].clone();
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
            for e in std::fs::read_dir(&p).unwrap() {
                out.push(e.unwrap().path());
            }
        }
        i += 1;
    }
    out
}

/// Every entry below `dir` is private to the current user: the platform check
/// (`ensure_owner_only`: Unix mode bits, Windows DACL) and, on Unix, no group/other bits.
fn assert_owner_only_tree(dir: &Path) -> usize {
    let entries = walk(dir);
    for p in &entries {
        if let Err(e) = scull_platform::ensure_owner_only(p) {
            panic!("{} is not owner-only: {e}", p.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode & 0o077, 0, "{} has mode {mode:o}", p.display());
            println!("  {:o} {}", mode, p.display());
        }
        #[cfg(not(unix))]
        println!("  owner-only {}", p.display());
    }
    entries.len()
}

/// A small tree with an idle Cargo target and an npm project with a lockfile.
fn tree() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 16 * 1024)
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap();
    fx.checkpoint().unwrap();
    fx
}

/// FX-DATA-03 (CLI): the history database `scull scan` creates, its WAL side files while they
/// exist, and the data directory are owner-only.
#[test]
fn fx_data_03_database_files_created_by_the_cli_are_owner_only() {
    let fx = tree();
    let cli = Cli::new();
    let doc = Schemas::default().json(&cli, &["scan", &s(fx.root())], "scan");
    assert_eq!(doc["data"]["history"]["state"], "recorded", "{doc:#}");
    assert!(cli.db().is_file(), "history database created");
    println!("scull's data directory after `scull scan`:");
    let n = assert_owner_only_tree(&cli.data_dir());
    assert!(n >= 2, "data dir and database");
    // `scull doctor` reports the same.
    let doctor = Schemas::default().json(&cli, &["doctor"], "doctor");
    assert_eq!(doctor["data"]["database"]["owner_only"], true, "{doctor:#}");
    check(&fx, &scull_fixtures::InvariantInputs::new());
}

/// FX-DATA-03 (Executor): an audited operation's database, WAL files and NDJSON audit mirror
/// are owner-only; the audit matches the disk (I-4).
#[test]
fn fx_data_03_audit_database_and_mirror_written_by_the_executor_are_owner_only() {
    let fx = three_tiers();
    let h = harness();
    let scan = h.scan(&fx);
    let rels = ["lib/target", "app/target"];
    let p = plan(&fx, &scan, &rels, "PXD03");
    let mut s = store(&fx);
    let op = OperationId::from_suffix("OXD03").unwrap();
    let r = run(&fx, &h, &mut s, &p, &op);
    let data = fx.base().join("data");
    let mirror = data.join("audit").join(format!("{op}.ndjson"));
    assert!(mirror.is_file(), "audit mirror at {}", mirror.display());
    println!("audit store after the operation:");
    assert_owner_only_tree(&data);
    let inv = check(&fx, &inputs(&p, &r, &s, &op, &rels));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
}

/// FX-DATA-06: the database file is garbage. `scull scan` still scans (exit 0, candidates
/// listed) without recording history, says the database is damaged and offers the newest
/// backup; `status`, `doctor` and `data show` report it; the damaged file is left
/// byte-identical and nothing else in scull's data directory changes.
#[test]
fn fx_data_06_corrupted_database_detected_backup_offered_read_only_scan_works() {
    let fx = tree();
    let cli = Cli::new();
    let schemas = Schemas::default();
    let r = s(fx.root());
    // A real database first, kept as a migration backup, then the live file is overwritten.
    schemas.json(&cli, &["scan", &r], "scan");
    let backup = cli.data_dir().join("scull.db.backup-20260101T000000Z-v1");
    std::fs::copy(cli.db(), &backup).unwrap();
    for side in ["scull.db-wal", "scull.db-shm"] {
        let _ = std::fs::remove_file(cli.data_dir().join(side));
    }
    let garbage: Vec<u8> = (0..64 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(cli.db(), &garbage).unwrap();
    let before: Vec<PathBuf> = walk(&cli.data_dir());

    let o = cli.run(&["scan", &r, "--json"]);
    assert_exit(&o, 0);
    let doc: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    schemas.validate("scan.schema.json", &doc, "scan with a damaged database");
    let cands = doc["data"]["candidates"].as_array().unwrap();
    assert!(cands.len() >= 2, "the read-only scan still works: {doc:#}");
    assert_ne!(doc["data"]["history"]["state"], "recorded", "{doc:#}");
    let err = stderr(&o);
    println!("scan stderr: {err}");
    assert!(err.contains("damaged"), "{err}");
    assert!(err.contains("backup"), "{err}");
    assert!(
        err.contains("scull.db.backup-20260101T000000Z-v1"),
        "the backup is named: {err}"
    );

    let status = schemas.json(&cli, &["status"], "status");
    assert_eq!(status["data"]["state"], "unavailable", "{status:#}");
    let doctor = schemas.json(&cli, &["doctor"], "doctor");
    let db = &doctor["data"]["database"];
    println!("doctor database: {db:#}");
    assert_ne!(db["state"], "ok", "{db:#}");
    assert!(db["detail"].is_string(), "{db:#}");
    let show = schemas.json(&cli, &["data", "show"], "data-show");
    let kinds: Vec<&str> = show["data"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"backup"), "{show:#}");

    assert_eq!(
        std::fs::read(cli.db()).unwrap(),
        garbage,
        "the damaged file is left byte-identical"
    );
    assert_eq!(walk(&cli.data_dir()), before, "nothing added or removed");
    check(&fx, &scull_fixtures::InvariantInputs::new());
}

const SECRET_CONTENT: &str = "fx-data-07-secret-file-contents-6c1f0b";
const SECRET_ENV: &str = "fx-data-07-secret-environment-value-93a2d4";

/// Whether `haystack` contains `needle` as bytes.
fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Every file below `dir` that contains `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    walk(dir)
        .into_iter()
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file()))
        .filter(|p| contains(&std::fs::read(p).unwrap(), needle))
        .collect()
}

/// FX-DATA-07: a tree whose files hold a secret (a `.env`, build outputs, a lockfile) and an
/// environment carrying secret tokens. After `scan` (history recorded), `projects`,
/// `large-files`, `caches`, `status`, `doctor`, `data show` and `data export`, neither secret
/// appears in any output (stdout or stderr, human or JSON) or in any file scull wrote; after an
/// audited Executor operation over the same kind of tree, neither appears in the audit database
/// or its NDJSON mirror (paths and metadata only, INS-07).
#[test]
fn fx_data_07_no_file_contents_or_environment_values_in_data_audit_or_output() {
    let mut fx = Fixture::new().unwrap();
    let secret_file = format!("API_TOKEN={SECRET_CONTENT}\n");
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 8 * 1024)
        .unwrap()
        .file("app/.env", &secret_file)
        .unwrap()
        .file("app/target/debug/config.txt", &secret_file)
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .file("web/node_modules/left-pad/secret.js", &secret_file)
        .unwrap()
        .file_of_size("web/big.bin", 2 * 1024 * 1024)
        .unwrap();
    fx.checkpoint().unwrap();
    let r = s(fx.root());
    let mut cli = Cli::new();
    for var in [
        "NPM_TOKEN",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "CARGO_REGISTRY_TOKEN",
        "OPENAI_API_KEY",
    ] {
        cli = cli.env(var, SECRET_ENV);
    }
    let export_dir = cli.home().join("export");
    let export = s(&export_dir);
    let runs: Vec<Vec<&str>> = vec![
        vec!["scan", &r],
        vec!["scan", &r, "--json"],
        vec!["scan", &r, "--ndjson"],
        vec!["projects", "--root", &r, "--json"],
        vec!["large-files", &r, "--min-size", "1M", "--json"],
        vec!["caches", "--json"],
        vec!["status", "--json"],
        vec!["doctor"],
        vec!["doctor", "--json"],
        vec!["data", "show", "--json"],
        vec!["data", "export", "--out", &export, "--json"],
    ];
    for args in &runs {
        let o = cli.run(args);
        assert_exit(&o, 0);
        for (stream, bytes) in [("stdout", &o.stdout), ("stderr", &o.stderr)] {
            for secret in [SECRET_CONTENT, SECRET_ENV] {
                assert!(
                    !contains(bytes, secret),
                    "scull {args:?} {stream} leaks {secret}:\n{}{}",
                    stdout(&o),
                    stderr(&o)
                );
            }
        }
    }
    assert!(cli.db().is_file(), "history recorded");
    assert!(export_dir.join("scull-data.json").is_file());
    let mut scanned = 0;
    for dir in [cli.scull_home().to_path_buf(), export_dir.clone()] {
        scanned += walk(&dir).len();
        for secret in [SECRET_CONTENT, SECRET_ENV] {
            let leaks = files_containing(&dir, secret);
            assert!(leaks.is_empty(), "{secret} written to {leaks:?}");
        }
    }
    println!(
        "{} runs, {scanned} entries under SCULL_HOME and the export: no file contents, no \
         environment values",
        runs.len()
    );
    check(&fx, &scull_fixtures::InvariantInputs::new());

    // The audit log of a real operation over secret-holding items.
    let mut fx = three_tiers();
    fx.file("lib/target/debug/.env", &secret_file)
        .unwrap()
        .file("app/target/debug/notes.txt", &secret_file)
        .unwrap();
    fx.checkpoint().unwrap();
    let h = harness();
    let scan = h.scan(&fx);
    let rels = ["lib/target", "app/target"];
    let p = plan(&fx, &scan, &rels, "PXD07");
    let mut st = store(&fx);
    let op = OperationId::from_suffix("OXD07").unwrap();
    let rep = run(&fx, &h, &mut st, &p, &op);
    let items = st.audit_items(&op).unwrap();
    assert_eq!(items.len(), 2, "{items:?}");
    let data = fx.base().join("data");
    assert!(data.join("audit").join(format!("{op}.ndjson")).is_file());
    let leaks = files_containing(&data, SECRET_CONTENT);
    assert!(
        leaks.is_empty(),
        "file contents in the audit store: {leaks:?}"
    );
    let inv = check(&fx, &inputs(&p, &rep, &st, &op, &rels));
    assert_eq!(inv.outcome(InvariantId::I4), &Outcome::Passed, "{inv}");
}
