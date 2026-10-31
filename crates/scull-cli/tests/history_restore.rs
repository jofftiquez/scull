//! `scull history`, `scull restore` and `scull clean --purge-quarantine` end to end (P2-CLI-03).
//!
//! Each test runs a real `scull clean` over a `scull-fixtures` tree (canary beside the root,
//! `SCULL_HOME`/`HOME` in the temp dir) whose artifacts have no lockfile to reproduce from, so
//! they are S2 and **quarantined** (never deleted, D-28) on every machine. The
//! quarantine folder lands in the fixture's temp directory, outside the root. The invariants
//! run after each test (I-2 canary, I-3 only the restored/quarantined paths changed, I-4 the
//! audit of the clean operation matches the disk where nothing was restored since).
//!
//! Regenerate the schemas with `SCULL_BLESS=1 cargo test -p scull-cli --test history_restore`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_fixtures::{Fixture, InvariantInputs, NodeLock};
use scull_storage::{DefaultPermissions, Store};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const TARGET: &str = "app/target";
const MODULES: &str = "web/node_modules";

struct Env {
    fx: Fixture,
}

impl Env {
    /// A Cargo project with `target/` and an npm project with `node_modules`, both without a
    /// lockfile (marker but nothing to reproduce from: S2 on every machine, PRD §8.3), idle
    /// for 30 days.
    fn new() -> Self {
        let mut fx = Fixture::new().unwrap();
        fx.cargo_project("app", "app")
            .unwrap()
            .cargo_target("app", "app", 64 * 1024)
            .unwrap()
            .npm_project("web", "web", NodeLock::None)
            .unwrap()
            .node_modules("web", &["left-pad"])
            .unwrap()
            .file("notes/todo.txt", "keep me")
            .unwrap();
        // Built by the fixture's own helpers inside its temp root, before the checkpoint.
        std::fs::remove_file(fx.root().join("app/Cargo.lock")).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
        for d in ["app", "web", "notes"] {
            fx.age(d, old).unwrap();
        }
        fx.checkpoint().unwrap();
        std::fs::create_dir_all(fx.base().join("userhome")).unwrap();
        Self { fx }
    }

    fn home(&self) -> PathBuf {
        self.fx.base().join("scull-home")
    }

    fn root(&self) -> PathBuf {
        self.fx.root().to_path_buf()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(args);
        let user_home = self.fx.base().join("userhome");
        c.current_dir(self.fx.base())
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("HOME", &user_home)
            .env("USERPROFILE", &user_home)
            .stdin(Stdio::null());
        for (k, _) in std::env::vars() {
            if k.starts_with("SCULL_") {
                c.env_remove(k);
            }
        }
        c.env("SCULL_HOME", self.home());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn json(&self, args: &[&str], schema: &str) -> (i32, Value) {
        let o = self.run(args);
        let v: Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|_| panic!("JSON on stdout for {args:?}; stderr: {}", stderr(&o)));
        validate(schema, &v);
        (o.status.code().unwrap(), v)
    }

    /// Plan both artifacts and execute: returns the operation ID. Both are quarantined.
    fn clean(&self) -> String {
        let root = self.root();
        let t = root.join(TARGET);
        let m = root.join(MODULES);
        let o = self.run(&[
            "clean",
            "--root",
            root.to_str().unwrap(),
            "--allow-network-roots",
            "--item",
            t.to_str().unwrap(),
            "--item",
            m.to_str().unwrap(),
            "--output",
            "plan.json",
            "--json",
        ]);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        let o = self.run(&[
            "clean",
            "--plan",
            "plan.json",
            "--confirm",
            "--allow-network-roots",
            "--json",
        ]);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        let run: Value = serde_json::from_slice(&o.stdout).unwrap();
        for i in run["data"]["items"].as_array().unwrap() {
            assert_eq!(
                i["outcome"], "quarantined",
                "recently built: S1+ (D-28): {i:#}"
            );
        }
        assert!(!self.exists(TARGET) && !self.exists(MODULES));
        run["data"]["operation_id"].as_str().unwrap().to_owned()
    }

    fn exists(&self, rel: &str) -> bool {
        self.fx.root().join(rel).exists()
    }

    fn store(&self) -> Store {
        Store::open(
            &self.home().join("data").join(scull_storage::DB_FILE_NAME),
            &DefaultPermissions,
        )
        .unwrap()
    }

    fn check(&self, allowed: &[&str], audited: Option<&str>) {
        let mut inputs = InvariantInputs::new();
        for a in allowed {
            inputs = inputs.allow(*a);
        }
        if let Some(op) = audited {
            let op = scull_core::OperationId::try_from(op.to_owned()).unwrap();
            inputs = inputs.audit(&self.store(), &op);
        }
        let report = self.fx.check_invariants(&inputs);
        println!("{report}");
        report.assert_ok();
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn schema_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("schemas/{name}.schema.json"))
}

fn validate(name: &str, v: &Value) {
    let schema: Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path(name)).expect("schema file"))
            .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{name}: {errors:?}\n{v:#}");
}

fn error_kind(o: &Output) -> String {
    let v: Value = serde_json::from_slice(&o.stderr)
        .unwrap_or_else(|_| panic!("JSON error on stderr: {}", stderr(o)));
    v["error"]["kind"].as_str().unwrap().to_owned()
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test history_restore`.
#[test]
fn committed_schemas_match_generated() {
    use scull_cli::cmd::{history, restore};
    use scull_cli::output::json::document_schema;
    let generated = [
        (
            "history",
            document_schema::<history::HistoryReport>(history::HISTORY_SCHEMA),
        ),
        (
            "history-operation",
            document_schema::<history::OperationDetail>(history::HISTORY_OPERATION_SCHEMA),
        ),
        (
            "restore",
            document_schema::<restore::RestoreReport>(restore::RESTORE_SCHEMA),
        ),
        (
            "quarantine-purge",
            document_schema::<restore::PurgeReport>(restore::PURGE_SCHEMA),
        ),
    ];
    for (name, schema) in generated {
        let path = schema_path(name);
        let text = serde_json::to_string_pretty(&schema).expect("serialize") + "\n";
        if std::env::var_os("SCULL_BLESS").is_some() {
            std::fs::write(&path, &text).expect("write");
        }
        let committed = std::fs::read_to_string(&path).expect("schema file");
        assert_eq!(
            committed.replace("\r\n", "\n"),
            text,
            "schemas/{name}.schema.json is stale; rerun with SCULL_BLESS=1"
        );
    }
}

/// `scull history` lists operations from the audit log (empty before any); `scull history
/// <op>` shows the plan, the intact hash chain, one outcome per item and the quarantine
/// entries with the restore command.
#[test]
#[cfg_attr(
    windows,
    ignore = "needs quarantined items: quarantine moves are Unix-only so far (Windows blocks them)"
)]
fn history_lists_and_shows_an_operation() {
    let env = Env::new();
    let (code, v) = env.json(&["history", "--json"], "history");
    assert_eq!(code, 0);
    assert!(v["data"]["operations"].as_array().unwrap().is_empty());
    let o = env.run(&["history"]);
    assert!(stdout(&o).contains("No cleanup operations recorded yet"));

    let op = env.clean();
    let (code, v) = env.json(&["history", "--json"], "history");
    assert_eq!(code, 0);
    let ops = v["data"]["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["operation_id"], op.as_str());
    assert_eq!(ops[0]["status"], "completed");
    assert_eq!(ops[0]["items_done"], 2);
    assert_eq!(ops[0]["quarantine_held"], 2);
    assert_eq!(ops[0]["chain_intact"], true);
    let (code, v) = env.json(&["history", "--since", "1h", "--json"], "history");
    assert_eq!(code, 0);
    assert_eq!(v["data"]["operations"].as_array().unwrap().len(), 1);

    let (code, d) = env.json(&["history", &op, "--json"], "history-operation");
    assert_eq!(code, 0);
    let d = &d["data"];
    assert_eq!(d["chain"]["intact"], true);
    assert_eq!(d["items"].as_array().unwrap().len(), 2);
    for i in d["items"].as_array().unwrap() {
        assert_eq!(i["outcome"], "quarantined");
    }
    let q = d["quarantine"].as_array().unwrap();
    assert_eq!(q.len(), 2);
    assert!(q.iter().all(|e| e["state"] == "held"));
    assert!(
        d["restore_hint"]
            .as_str()
            .unwrap()
            .contains(&format!("scull restore {op}"))
    );

    let o = env.run(&["history", &op]);
    let text = stdout(&o);
    for want in [
        "Operation",
        "audit",
        "intact",
        "Items:",
        "Quarantine:",
        "scull restore",
    ] {
        assert!(text.contains(want), "missing {want:?}:\n{text}");
    }
    assert_eq!(
        error_kind(&env.run(&["history", "op_nope", "--json"])),
        "not_found"
    );
    assert_eq!(env.run(&["history", "not-an-id"]).status.code(), Some(2));
    env.check(&[TARGET, MODULES], Some(&op));
}

/// FX-EXEC-02/-04 through the CLI: `scull restore <op> --confirm` moves both items back; the
/// one whose original path was taken in the meantime goes to a `.scull-restored-1` sibling and
/// nothing is overwritten. History then shows them restored; restoring again does nothing.
#[test]
#[cfg_attr(
    windows,
    ignore = "needs quarantined items: quarantine moves are Unix-only so far (Windows blocks them)"
)]
fn restore_puts_items_back_and_never_overwrites() {
    let env = Env::new();
    let before_target = files_below(&env.root().join(TARGET));
    let op = env.clean();
    // Someone reinstalled node_modules since: the original path is taken.
    std::fs::create_dir_all(env.root().join(MODULES)).unwrap();
    std::fs::write(env.root().join(MODULES).join("fresh.txt"), "new install").unwrap();

    let (code, v) = env.json(&["restore", &op, "--confirm", "--json"], "restore");
    assert_eq!(code, 0, "{v:#}");
    let d = &v["data"];
    assert_eq!(d["mode"], "run");
    assert_eq!(d["restored"], 2);
    assert_eq!(d["restored_to_sibling"], 1);
    let entry = |rel: &str| {
        let want = env.root().join(rel);
        d["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| Path::new(e["original_path"].as_str().unwrap()) == want)
            .unwrap()
            .clone()
    };
    assert_eq!(entry(TARGET)["outcome"], "restored");
    assert_eq!(
        files_below(&env.root().join(TARGET)),
        before_target,
        "identical tree"
    );
    let m = entry(MODULES);
    assert_eq!(m["outcome"], "restored_to_sibling");
    let sibling = env.root().join("web/node_modules.scull-restored-1");
    assert_eq!(Path::new(m["restored_to"].as_str().unwrap()), sibling);
    assert!(sibling.join("left-pad").exists());
    assert_eq!(
        std::fs::read_to_string(env.root().join(MODULES).join("fresh.txt")).unwrap(),
        "new install",
        "nothing overwritten"
    );

    let (_, h) = env.json(&["history", &op, "--json"], "history-operation");
    assert!(
        h["data"]["quarantine"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["state"] == "restored")
    );
    assert!(h["data"]["restore_hint"].is_null());
    let (code, v) = env.json(&["restore", &op, "--confirm", "--json"], "restore");
    assert_eq!(code, 0);
    assert_eq!(v["data"]["restored"], 0, "idempotent");
    env.check(
        &[TARGET, MODULES, "web/node_modules.scull-restored-1"],
        None,
    );
}

/// Without `--confirm` and without a terminal, restore only previews; `--item` restores a
/// subset; unknown items, operations and malformed IDs are errors; elevated runs are refused.
#[test]
#[cfg_attr(
    windows,
    ignore = "needs quarantined items: quarantine moves are Unix-only so far (Windows blocks them)"
)]
fn restore_preview_selection_and_refusals() {
    let env = Env::new();
    let op = env.clean();
    let (code, v) = env.json(&["restore", &op, "--json"], "restore");
    assert_eq!(code, 0);
    assert_eq!(v["data"]["mode"], "preview");
    assert!(
        v["data"]["next_step"]
            .as_str()
            .unwrap()
            .contains("--confirm")
    );
    assert!(
        !env.exists(TARGET) && !env.exists(MODULES),
        "preview moves nothing"
    );

    let o = env
        .cmd(&["restore", &op, "--confirm", "--json"])
        .env("SCULL_TEST_ASSUME_ELEVATED", "1")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(5));
    assert_eq!(error_kind(&o), "safety_blocked");
    assert!(!env.exists(TARGET));

    assert_eq!(
        env.run(&["restore", &op, "--item", "nope", "--confirm"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        error_kind(&env.run(&["restore", "op_0000", "--json"])),
        "not_found"
    );
    assert_eq!(env.run(&["restore", "../etc"]).status.code(), Some(2));

    let target = env.root().join(TARGET);
    let (code, v) = env.json(
        &[
            "restore",
            &op,
            "--item",
            target.to_str().unwrap(),
            "--confirm",
            "--json",
        ],
        "restore",
    );
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["data"]["entries"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"]["restored"], 1);
    assert!(env.exists(TARGET) && !env.exists(MODULES));
    env.check(&[TARGET, MODULES], None);
}

/// SAFE-07: purging the quarantine permanently deletes S1/S2 items, so without a terminal it
/// needs `--confirm --allow-permanent` (exit 5 otherwise). Nothing has expired right after a
/// run; `--include-unexpired` selects everything held. Purged items can't be restored.
#[test]
#[cfg_attr(
    windows,
    ignore = "needs quarantined items: quarantine moves are Unix-only so far (Windows blocks them)"
)]
fn purge_quarantine_needs_elevated_confirmation() {
    let env = Env::new();
    let op = env.clean();
    let (code, v) = env.json(
        &["clean", "--purge-quarantine", "--json"],
        "quarantine-purge",
    );
    assert_eq!(code, 0);
    assert!(
        v["data"]["entries"].as_array().unwrap().is_empty(),
        "nothing expired yet"
    );

    let (code, v) = env.json(
        &[
            "clean",
            "--purge-quarantine",
            "--include-unexpired",
            "--json",
        ],
        "quarantine-purge",
    );
    assert_eq!(code, 0);
    assert_eq!(v["data"]["mode"], "preview");
    assert_eq!(v["data"]["entries"].as_array().unwrap().len(), 2);

    let o = env.run(&[
        "clean",
        "--purge-quarantine",
        "--include-unexpired",
        "--confirm",
        "--json",
    ]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert_eq!(error_kind(&o), "safety_blocked");
    assert!(stderr(&o).contains("--allow-permanent"));
    let o = env
        .cmd(&[
            "clean",
            "--purge-quarantine",
            "--include-unexpired",
            "--confirm",
            "--allow-permanent",
            "--json",
        ])
        .env("SCULL_TEST_ASSUME_ELEVATED", "1")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(5), "elevated: {}", stderr(&o));

    let held: Vec<PathBuf> = env
        .store()
        .quarantine_held()
        .unwrap()
        .iter()
        .map(|e| e.entry.quarantine_path.to_path_buf())
        .collect();
    assert!(held.iter().all(|p| p.exists()));
    let (code, v) = env.json(
        &[
            "clean",
            "--purge-quarantine",
            "--include-unexpired",
            "--confirm",
            "--allow-permanent",
            "--json",
        ],
        "quarantine-purge",
    );
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["data"]["mode"], "run");
    assert!(
        v["data"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["outcome"] == "purged")
    );
    assert!(v["data"]["freed_bytes"].as_u64().unwrap() > 0);
    assert!(
        held.iter().all(|p| !p.exists()),
        "purged from the quarantine folder"
    );

    let (code, r) = env.json(&["restore", &op, "--confirm", "--json"], "restore");
    assert_eq!(code, 0);
    assert_eq!(r["data"]["restored"], 0);
    assert!(
        r["data"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["state"] == "purged")
    );
    assert!(!env.exists(TARGET) && !env.exists(MODULES));
    env.check(&[TARGET, MODULES], None);
}

/// Relative paths of every file below `dir`, with sizes, sorted.
fn files_below(dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                stack.push(p);
            } else {
                let rel = p
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, e.metadata().unwrap().len()));
            }
        }
    }
    out.sort();
    out
}
