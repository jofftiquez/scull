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

    /// The fixture root, canonical: the spelling scull records (on Windows the temp dir is an
    /// 8.3 short path such as `C:\Users\RUNNER~1\...`, which scull spells out in full).
    fn root(&self) -> PathBuf {
        scull_engine::paths::canon(self.fx.root())
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
        // P2-EXEC-13: quarantine (and the Trash fallback) stay inside the fixture's temp dir.
        self.fx.sandbox(&mut c);
        c
    }

    /// Runs `args` (with `--allow-root` when this test runs elevated: [`allow_root_if_elevated`]).
    fn run(&self, args: &[&str]) -> Output {
        self.cmd(&allow_root_if_elevated(args)).output().unwrap()
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
            let q = Path::new(i["quarantine_path"].as_str().unwrap());
            assert!(
                q.starts_with(self.fx.base()) && !q.starts_with(self.fx.root()),
                "the quarantine stays in the temp dir, outside the root: {i:#}"
            );
        }
        assert!(!self.exists(TARGET) && !self.exists(MODULES));
        run["data"]["operation_id"].as_str().unwrap().to_owned()
    }

    /// Plan both artifacts and execute with `env` changed on the execution (`None` removes the
    /// variable): returns the exit code and the `clean-run` document.
    fn clean_with(&self, env: &[(&str, Option<&Path>)]) -> (i32, Value) {
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
        let mut c = self.cmd(&allow_root_if_elevated(&[
            "clean",
            "--plan",
            "plan.json",
            "--confirm",
            "--allow-network-roots",
            "--json",
        ]));
        for (k, v) in env {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        let o = c.output().unwrap();
        let run: Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|_| panic!("clean-run JSON; stderr: {}", stderr(&o)));
        validate("clean-run", &run);
        println!("{:#}", run["data"]["items"]);
        (o.status.code().unwrap(), run)
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

/// GitHub's Windows runners run the tests as an elevated administrator, where scull refuses
/// to change anything without `--allow-root` (SAFE-14). When this test process itself runs
/// elevated, `clean` and `restore` get `--allow-root`, as a user running elevated would have
/// to pass; everywhere else the arguments are unchanged. The refusal itself is tested with
/// `SCULL_TEST_ASSUME_ELEVATED` through [`Env::cmd`], which never adds the flag.
fn allow_root_if_elevated<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut all = args.to_vec();
    if matches!(args.first(), Some(&("clean" | "restore"))) && needs_allow_root() {
        all.push("--allow-root");
    }
    all
}

/// Set by CI's standard-user run on Windows (`scripts/ci/run-as-standard-user.ps1`, #65): the
/// test process must then be non-elevated, so `--allow-root` is never added and the refusal
/// paths of a normal user run for real; an elevated process fails instead of quietly passing
/// the flag.
fn needs_allow_root() -> bool {
    let elevated = scull_platform::is_elevated().blocks_destructive();
    if std::env::var_os("SCULL_TEST_REQUIRE_NOT_ELEVATED").is_some_and(|v| !v.is_empty()) {
        assert!(
            !elevated,
            "SCULL_TEST_REQUIRE_NOT_ELEVATED is set but this process runs elevated"
        );
        static MARK: std::sync::Once = std::sync::Once::new();
        MARK.call_once(|| {
            eprintln!(
                "RUN {}: not elevated, no --allow-root (standard user)",
                env!("CARGO_CRATE_NAME")
            );
        });
    }
    elevated
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

// ---------------------------------------------------------------------------------------------
// P2-EXEC-13: where the quarantine lives; the test-harness guard; the Trash fallback.
// ---------------------------------------------------------------------------------------------

/// `SCULL_QUARANTINE_BASE` (here: a folder that doesn't exist yet, in the fixture's temp dir)
/// is where `scull clean` quarantines: the folder is created owner-only and every quarantine
/// path is below it. Nothing appears outside the temp dir (I-2).
#[test]
fn quarantine_goes_to_the_configured_base() {
    let env = Env::new();
    let base = env.fx.base().join("configured-q");
    let (code, run) = env.clean_with(&[("SCULL_QUARANTINE_BASE", Some(&base))]);
    assert_eq!(code, 0, "{run:#}");
    for i in run["data"]["items"].as_array().unwrap() {
        assert_eq!(i["outcome"], "quarantined", "{i:#}");
        let q = Path::new(i["quarantine_path"].as_str().unwrap());
        assert!(
            q.starts_with(base.join(".scull-quarantine")),
            "quarantined below the configured base: {i:#}"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&base), 0o700, "the base is created owner-only");
        assert_eq!(mode(&base.join(".scull-quarantine")), 0o700);
    }
    assert!(!env.exists(TARGET) && !env.exists(MODULES));
    let op = run["data"]["operation_id"].as_str().unwrap().to_owned();
    env.check(&[TARGET, MODULES], Some(&op));
}

/// The guard: under the test harness (`SCULL_TEST_SANDBOX` set by the fixtures) with no
/// quarantine base configured, quarantine is blocked instead of using the automatic location
/// (which on CI runners is outside the temp dir). Both items stay where they were; nothing is
/// created anywhere (I-2, I-3).
#[test]
fn under_the_test_harness_no_base_means_blocked() {
    let env = Env::new();
    let (code, run) = env.clean_with(&[("SCULL_QUARANTINE_BASE", None)]);
    assert_eq!(code, 3, "partial failure: {run:#}");
    for i in run["data"]["items"].as_array().unwrap() {
        assert_eq!(i["outcome"], "blocked", "{i:#}");
        assert_eq!(i["reason_code"], "quarantine_base_required", "{i:#}");
        assert!(i["quarantine_path"].is_null(), "{i:#}");
    }
    assert!(env.exists(TARGET) && env.exists(MODULES), "left in place");
    assert!(!env.fx.base().join(".scull-quarantine").exists());
    let op = run["data"]["operation_id"].as_str().unwrap().to_owned();
    env.check(&[], Some(&op));
}

/// A configured base inside an item being cleaned is refused (quarantine impossible), so
/// `scull clean` falls back to the Trash, which under the test harness is `<temp>/Trash`, never
/// the real one (D-3: quarantine → Trash → blocked). On Windows the sandbox has no Recycle Bin,
/// so the chain ends in "blocked". Never deleted either way.
#[test]
fn impossible_quarantine_falls_back_to_the_sandbox_trash() {
    let env = Env::new();
    let inside_item = env.root().join(TARGET).join("q");
    let (code, run) = env.clean_with(&[("SCULL_QUARANTINE_BASE", Some(&inside_item))]);
    let trash = env.fx.base().join("Trash");
    for i in run["data"]["items"].as_array().unwrap() {
        if cfg!(windows) {
            assert_eq!(i["outcome"], "blocked", "{i:#}");
            assert_eq!(i["reason_code"], "quarantine_and_trash_impossible", "{i:#}");
        } else {
            assert_eq!(i["outcome"], "trashed", "{i:#}");
            assert_eq!(i["restorability"], "via_trash", "{i:#}");
            let at = Path::new(i["trash_path"].as_str().unwrap());
            assert!(at.starts_with(&trash), "into the sandbox Trash: {i:#}");
            assert!(at.exists(), "{i:#}");
        }
    }
    assert!(!inside_item.exists(), "the refused base was never created");
    let op = run["data"]["operation_id"].as_str().unwrap().to_owned();
    if cfg!(windows) {
        assert_eq!(code, 3, "{run:#}");
        assert!(env.exists(TARGET) && env.exists(MODULES));
        env.check(&[], Some(&op));
    } else {
        assert_eq!(code, 0, "{run:#}");
        assert!(!env.exists(TARGET) && !env.exists(MODULES));
        env.check(&[TARGET, MODULES], Some(&op));
    }
}

/// Under the test harness, `scull restore` and `--purge-quarantine` touch no quarantine entry
/// outside the sandbox: with the sandbox pointed elsewhere, both refuse (or keep) every entry
/// and nothing moves or is deleted.
#[test]
fn restore_and_purge_refuse_entries_outside_the_test_sandbox() {
    let env = Env::new();
    let op = env.clean();
    let elsewhere = env.fx.base().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let held: Vec<PathBuf> = env
        .store()
        .quarantine_held()
        .unwrap()
        .iter()
        .map(|e| e.entry.quarantine_path.to_path_buf())
        .collect();
    assert_eq!(held.len(), 2);

    let o = env
        .cmd(&allow_root_if_elevated(&[
            "restore",
            &op,
            "--confirm",
            "--json",
        ]))
        .env("SCULL_TEST_SANDBOX", &elsewhere)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|_| panic!("restore JSON; stderr: {}", stderr(&o)));
    println!("{:#}", v["data"]["entries"]);
    assert_ne!(o.status.code(), Some(0), "{v:#}");
    for e in v["data"]["entries"].as_array().unwrap() {
        assert_eq!(e["outcome"], "refused", "{e:#}");
        assert!(
            e["message"].as_str().unwrap().contains("test sandbox"),
            "{e:#}"
        );
    }

    let o = env
        .cmd(&allow_root_if_elevated(&[
            "clean",
            "--purge-quarantine",
            "--include-unexpired",
            "--confirm",
            "--allow-permanent",
            "--json",
        ]))
        .env("SCULL_TEST_SANDBOX", &elsewhere)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|_| panic!("purge JSON; stderr: {}", stderr(&o)));
    println!("{:#}", v["data"]["entries"]);
    for e in v["data"]["entries"].as_array().unwrap() {
        assert_eq!(e["outcome"], "kept", "{e:#}");
    }
    assert!(held.iter().all(|p| p.exists()), "nothing moved or deleted");
    assert!(!env.exists(TARGET) && !env.exists(MODULES));
    env.check(&[TARGET, MODULES], Some(&op));
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
