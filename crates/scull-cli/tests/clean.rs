//! `scull clean` end to end (P2-CLI-01, CLI side of P2-EXEC-11): dry run, plan files,
//! confirmation, plan rejection (FX-EXEC-07…10), the execution lock (SAFE-09), the refusal to
//! run elevated (FX-EXEC-12) and the elevated confirmation for irreversible S1/S2 items
//! (FX-EXEC-11).
//!
//! Every run spawns the real binary with `SCULL_HOME` and `HOME` inside the fixture's temp
//! directory (beside the root and the canary, never inside them), over a `scull-fixtures` tree.
//! After each test the safety invariants run: nothing outside the executed items changed in the
//! root (I-3), the canary is untouched (I-2), nothing S3 was executed (I-1) and no executed
//! tier is lower than planned (I-8); after an execution, the audit matches the disk (I-4) and
//! the finished operation records the predicted bytes and the measured free-space delta the
//! run reported (I-5). Quarantine folders land in the fixture's temp directory
//! (the shallowest folder on the volume the test user owns), outside the root.
//!
//! The fixture tiers depend on what the machine can observe (an uninspectable process of the
//! test user makes "in use" unknown, hence S2), so the tests name items with `--item` and
//! accept either executed action: S0 is deleted, S1/S2 quarantined.
//!
//! Regenerate the schemas with `SCULL_BLESS=1 cargo test -p scull-cli --test clean`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{Action, ItemId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams, Tier};
use scull_fixtures::{Fixture, InvariantInputs, NodeLock};
use scull_storage::{ApprovalMethod, DefaultPermissions, Store};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

const TARGET: &str = "app/target";
const MODULES: &str = "web/node_modules";

struct Env {
    fx: Fixture,
}

impl Env {
    /// A Cargo project with `target/`, an npm project with `node_modules`, a protected Cargo
    /// project and a plain folder, all idle for 30 days.
    fn new() -> Self {
        let mut fx = Fixture::new().unwrap();
        fx.cargo_project("app", "app")
            .unwrap()
            .cargo_target("app", "app", 64 * 1024)
            .unwrap()
            .npm_project("web", "web", NodeLock::Npm)
            .unwrap()
            .node_modules("web", &["left-pad"])
            .unwrap()
            .cargo_project("vault", "vault")
            .unwrap()
            .cargo_target("vault", "vault", 1024)
            .unwrap()
            .file("vault/.scull-protect", "")
            .unwrap()
            .file("notes/todo.txt", "keep me")
            .unwrap();
        let old = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        for d in ["app", "web", "vault", "notes"] {
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

    /// Runs `args` (with `--allow-root` when this test runs elevated: [`allow_root_if_elevated`]).
    fn run(&self, args: &[&str]) -> Output {
        self.cmd(&allow_root_if_elevated(args)).output().unwrap()
    }

    /// `scull clean --root <root> --allow-network-roots <args>`.
    fn clean(&self, args: &[&str]) -> Output {
        let root = self.root();
        let mut all = vec![
            "clean",
            "--root",
            root.to_str().unwrap(),
            "--allow-network-roots",
            "--no-progress",
        ];
        all.extend_from_slice(args);
        self.run(&all)
    }

    /// Plan `items` into `plan.json` (JSON preview returned).
    fn plan(&self, items: &[&str]) -> Value {
        let paths: Vec<String> = items
            .iter()
            .map(|i| self.root().join(i).to_string_lossy().into_owned())
            .collect();
        let mut args: Vec<&str> = vec!["--json", "--output", "plan.json"];
        for p in &paths {
            args.push("--item");
            args.push(p);
        }
        let o = self.clean(&args);
        let v = json_ok(&o);
        validate("clean-plan", &v);
        v
    }

    fn plan_file(&self) -> PathBuf {
        self.fx.base().join("plan.json")
    }

    fn read_plan(&self) -> Plan {
        serde_json::from_slice(&std::fs::read(self.plan_file()).unwrap()).unwrap()
    }

    fn store(&self) -> Store {
        Store::open(
            &self.home().join("data").join(scull_storage::DB_FILE_NAME),
            &DefaultPermissions,
        )
        .unwrap()
    }

    fn machine_id(&self) -> String {
        std::fs::read_to_string(
            self.home()
                .join("data")
                .join(scull_platform::MACHINE_ID_FILE),
        )
        .unwrap()
        .trim()
        .to_owned()
    }

    /// Write `plan` as the plan file (and record it when `record`).
    fn put_plan(&self, plan: &Plan, record: bool) {
        if record {
            self.store().insert_plan(plan).unwrap();
        }
        std::fs::write(self.plan_file(), serde_json::to_vec_pretty(plan).unwrap()).unwrap();
    }

    fn exists(&self, rel: &str) -> bool {
        self.fx.root().join(rel).exists()
    }

    /// Invariants after a run that may have executed `executed` (relative paths).
    fn check(&self, executed: &[(&str, Option<&Value>)]) {
        self.check_inner(executed, None);
    }

    /// [`Env::check`] plus I-4 (the audit of `run`'s operation matches the disk) and I-5 (its
    /// predicted and measured bytes are recorded; the measured delta is the reported one).
    fn check_run(&self, run: &Value, executed: &[(&str, Option<&Value>)]) {
        self.check_inner(executed, Some(run));
    }

    fn check_inner(&self, executed: &[(&str, Option<&Value>)], run: Option<&Value>) {
        let mut inputs = InvariantInputs::new();
        if let Some(run) = run {
            let op = scull_core::OperationId::try_from(
                run["data"]["operation_id"].as_str().unwrap().to_owned(),
            )
            .unwrap();
            let store = self.store();
            inputs = inputs.audit(&store, &op).operation_bytes(&store, &op);
            // The recorded delta is the one the run reported (one measurement, PHY-02).
            let rec = store.operation(&op).unwrap().unwrap();
            let totals = rec.finished.expect("operation finished").totals;
            assert_eq!(
                serde_json::to_value(totals.measured_delta_bytes).unwrap(),
                run["data"]["measured"]["measured_delta_bytes"],
                "recorded vs reported measured delta"
            );
        }
        let mut scan = BTreeMap::new();
        let mut exec = BTreeMap::new();
        let mut entries = Vec::new();
        for (rel, item) in executed {
            inputs = inputs.allow(*rel);
            if let Some(item) = item {
                let id: ItemId = serde_json::from_value(item["item_id"].clone()).unwrap();
                let planned: Tier = serde_json::from_value(item["planned_tier"].clone()).unwrap();
                let executed_tier = item["fresh_tier"]
                    .as_str()
                    .map(|_| serde_json::from_value::<Tier>(item["fresh_tier"].clone()).unwrap())
                    .map_or(planned, |f| planned.max_protective(f));
                let action = match item["outcome"].as_str().unwrap() {
                    "deleted" | "partly_deleted" => Action::Delete,
                    "quarantined" => Action::Quarantine,
                    _ => continue,
                };
                scan.insert(id.clone(), planned);
                exec.insert(id.clone(), executed_tier);
                entries.push((id, executed_tier, action));
            }
        }
        let report = self
            .fx
            .check_invariants(&inputs.plan(entries).tiers(scan, exec));
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
    if matches!(args.first(), Some(&("clean" | "restore")))
        && scull_platform::is_elevated().blocks_destructive()
    {
        all.push("--allow-root");
    }
    all
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn json_ok(o: &Output) -> Value {
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(o));
    serde_json::from_slice(&o.stdout).expect("JSON on stdout")
}

/// The exit code and the JSON error kind on stderr.
fn rejected(o: &Output) -> (i32, String) {
    let v: Value = serde_json::from_slice(&o.stderr)
        .unwrap_or_else(|_| panic!("JSON error on stderr: {}", stderr(o)));
    validate("error", &v);
    (
        o.status.code().unwrap(),
        v["error"]["kind"].as_str().unwrap().to_owned(),
    )
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

fn run_item<'a>(run: &'a Value, root: &Path, rel: &str) -> &'a Value {
    let want = root.join(rel);
    run["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| Path::new(i["path"].as_str().unwrap()) == want)
        .unwrap_or_else(|| panic!("no run item for {rel}: {run:#}"))
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test clean`.
#[test]
fn committed_schemas_match_generated() {
    use scull_cli::cmd::clean::*;
    use scull_cli::output::json::document_schema;
    let generated = [
        (
            "clean-plan",
            document_schema::<CleanPlanReport>(CLEAN_PLAN_SCHEMA),
        ),
        (
            "clean-run",
            document_schema::<CleanRunReport>(CLEAN_RUN_SCHEMA),
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

/// SAFE-02: `scull clean` only plans. The default selection follows PRD §8.2 (S0 only; S1 and
/// S2 are excluded with the reason; S3 never), the plan file is owner-only, bound to this
/// machine and user, expires in 24 h and is recorded in the database. Nothing changes.
#[test]
fn dry_run_plans_and_changes_nothing() {
    let env = Env::new();
    let o = env.clean(&["--json", "--output", "plan.json"]);
    let v = json_ok(&o);
    validate("clean-plan", &v);
    let d = &v["data"];
    assert_eq!(d["mode"], "plan");
    for item in d["items"].as_array().unwrap() {
        assert_eq!(
            item["item"]["tier"], "S0",
            "only S0 is preselected: {item:#}"
        );
        assert_eq!(item["item"]["action"]["type"], "delete");
        assert_eq!(item["restorability"], "regenerate");
    }
    for e in d["excluded"].as_array().unwrap() {
        let want = match e["tier"].as_str().unwrap() {
            "S1" => "opt_in_s1",
            "S2" => "per_item_s2",
            "S3" => "protected",
            other => panic!("S0 item excluded: {other} {e:#}"),
        };
        assert_eq!(e["reason"], want, "{e:#}");
    }
    let vault = env.root().join("vault/target");
    assert!(
        d["excluded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| Path::new(e["path"].as_str().unwrap()) == vault && e["tier"] == "S3"),
        "the protected project is listed as excluded"
    );

    // With everything named, the plan holds both artifacts; S3 can't be named.
    let v = env.plan(&[TARGET, MODULES]);
    let d = &v["data"];
    assert_eq!(d["items"].as_array().unwrap().len(), 2);
    for item in d["items"].as_array().unwrap() {
        let tier = item["item"]["tier"].as_str().unwrap();
        let action = item["item"]["action"]["type"].as_str().unwrap();
        match tier {
            "S0" => assert_eq!(action, "delete"),
            "S1" | "S2" => assert_eq!(action, "quarantine", "D-28: Delete is S0-only"),
            other => panic!("{other} planned"),
        }
        // §10.2: everything the preview must show is in the item.
        let c = &item["candidate"];
        for key in [
            "path",
            "project",
            "artifact",
            "rule",
            "reasons",
            "evidence",
            "sizes",
            "flags",
            "regeneration",
        ] {
            assert!(!c[key].is_null(), "preview item lacks {key}: {item:#}");
        }
        assert!(item["undo"].as_str().unwrap().len() > 10);
        assert_eq!(item["needs_elevated_confirmation"], false);
    }
    let plan = env.read_plan();
    assert_eq!(plan.origin(), PlanOrigin::Cli);
    assert_eq!(plan.machine_id(), env.machine_id());
    assert_eq!(plan.user(), scull_platform::current_user().unwrap());
    assert_eq!(
        plan.expires_unix() - plan.created_unix(),
        scull_core::CLI_PLAN_TTL_SECS
    );
    assert_eq!(serde_json::to_value(&plan).unwrap(), d["plan"]);
    assert!(scull_platform::ensure_owner_only(&env.plan_file()).is_ok());
    assert_eq!(
        env.store().get_plan(plan.id()).unwrap().as_ref(),
        Some(&plan)
    );

    let o = env.clean(&["--item", env.root().join("vault/target").to_str().unwrap()]);
    assert_eq!(
        o.status.code(),
        Some(5),
        "S3 can't be planned: {}",
        stderr(&o)
    );
    let o = env.clean(&["--item", "/nope/not-a-candidate"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));

    // The human preview: table, details, plan line, next step.
    let o = env.clean(&[
        "--item",
        env.root().join(TARGET).to_str().unwrap(),
        "--output",
        "plan2.json",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let text = stdout(&o);
    for want in [
        "Cleanup plan (dry run): nothing has been changed.",
        "Details:",
        "evidence",
        "regen",
        "flags",
        "bound to this machine and user",
        "To execute: scull clean --plan",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
    env.check(&[]);
}

/// CLI-03: without a terminal, `--plan` without `--confirm` only reviews; it never executes.
#[test]
fn non_interactive_review_never_executes() {
    let env = Env::new();
    env.plan(&[TARGET, MODULES]);
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--allow-network-roots",
        "--json",
    ]);
    let v = json_ok(&o);
    validate("clean-plan", &v);
    assert_eq!(v["data"]["mode"], "review");
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 2);
    assert!(
        v["data"]["next_step"]
            .as_str()
            .unwrap()
            .contains("--confirm")
    );
    let o = env.run(&["clean", "--plan", "plan.json", "--allow-network-roots"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        stdout(&o).contains("Nothing was changed. To execute"),
        "{}",
        stdout(&o)
    );
    assert!(env.exists(TARGET) && env.exists(MODULES));
    env.check(&[]);
}

/// FX-EXEC-01/02 through the CLI: `--plan --confirm` revalidates and executes every item (S0
/// deleted, S1/S2 quarantined), records one audited operation with an intact hash chain, and
/// reports the measured free-space delta. Running the same plan again is refused (exit 4):
/// another operation touched its targets (SAFE-09).
#[test]
fn confirm_executes_audits_and_cannot_replay() {
    let env = Env::new();
    env.plan(&[TARGET, MODULES]);
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    let run = json_ok(&o);
    validate("clean-run", &run);
    let d = &run["data"];
    assert_eq!(d["confirmation"], "flag");
    assert_eq!(d["totals"]["succeeded"], 2, "{run:#}");
    let root = env.root();
    for rel in [TARGET, MODULES] {
        let item = run_item(&run, &root, rel);
        match item["outcome"].as_str().unwrap() {
            "deleted" => assert_eq!(item["restorability"], "regenerate"),
            "quarantined" => {
                let q = Path::new(item["quarantine_path"].as_str().unwrap());
                assert!(q.exists(), "quarantined copy exists");
                assert!(
                    q.starts_with(env.fx.base()),
                    "quarantine stays in the temp dir"
                );
                assert!(
                    !q.starts_with(env.fx.root()),
                    "quarantine is outside the root"
                );
            }
            other => panic!("{rel}: {other}: {item:#}"),
        }
        assert!(!env.exists(rel), "{rel} is gone from the root");
    }
    assert!(env.exists("app/Cargo.toml") && env.exists("notes/todo.txt"));
    assert!(env.exists("vault/target"));
    assert!(d["measured"]["predicted_freed_bytes"].is_u64());

    let op =
        scull_core::OperationId::try_from(d["operation_id"].as_str().unwrap().to_owned()).unwrap();
    let store = env.store();
    let chain = store.verify_chain(&op).unwrap();
    assert!(chain.is_intact(), "{chain:?}");
    assert_eq!(
        store.operation(&op).unwrap().unwrap().method,
        ApprovalMethod::CliFlag,
        "--confirm without a prompt"
    );
    assert_eq!(
        store.audit_items(&op).unwrap().len(),
        2,
        "one outcome per item (FX-EXEC-13)"
    );

    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_invalidated".into()));
    env.check_run(
        &run,
        &[
            (TARGET, Some(run_item(&run, &root, TARGET))),
            (MODULES, Some(run_item(&run, &root, MODULES))),
        ],
    );
}

/// FX-EXEC-07: a plan file edited to add a path is rejected (exit 4); so is a plan whose hash
/// was recomputed but that differs from the recorded plan, and one that was never recorded.
#[test]
fn fx_exec_07_tampered_plans_are_rejected() {
    let env = Env::new();
    env.plan(&[TARGET]);
    let original = std::fs::read_to_string(env.plan_file()).unwrap();

    // 1. Hand-edited: a path swapped in, hash unchanged.
    let edited = original.replace("app/target", "notes");
    assert_ne!(edited, original);
    std::fs::write(env.plan_file(), &edited).unwrap();
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_tampered".into()));

    // 2. Not a plan at all.
    std::fs::write(env.plan_file(), "{\"items\": []}").unwrap();
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_tampered".into()));

    // 3. An item added and the hash recomputed under the same ID: differs from the record.
    std::fs::write(env.plan_file(), &original).unwrap();
    let plan = env.read_plan();
    let mut items: Vec<PlanItem> = plan.items().to_vec();
    let mut extra = items[0].clone();
    extra.item_id = ItemId::derive(extra.identity, b"/elsewhere/notes");
    extra.path = scull_core::FsPath::from("/elsewhere/notes");
    items.push(extra);
    let forged = Plan::new(
        params_like(&plan, plan.id().clone(), plan.created_unix()),
        items.clone(),
    )
    .unwrap();
    env.put_plan(&forged, false);
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_tampered".into()));

    // 4. The same under a new ID: not recorded on this machine.
    let forged = Plan::new(
        params_like(
            &plan,
            PlanId::from_suffix("F0RGED").unwrap(),
            plan.created_unix(),
        ),
        items,
    )
    .unwrap();
    env.put_plan(&forged, false);
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_foreign".into()));

    assert!(env.exists(TARGET) && env.exists("notes/todo.txt"));
    env.check(&[]);
}

fn params_like(plan: &Plan, id: PlanId, created_unix: i64) -> PlanParams {
    PlanParams {
        id,
        scan_id: plan.scan_id().clone(),
        created_unix,
        ttl_secs: None,
        machine_id: plan.machine_id().to_owned(),
        user: plan.user().to_owned(),
        origin: PlanOrigin::Cli,
    }
}

/// FX-EXEC-08: an expired plan is rejected (exit 4), even though it is recorded.
#[test]
fn fx_exec_08_expired_plan_is_rejected() {
    let env = Env::new();
    env.plan(&[TARGET]);
    let plan = env.read_plan();
    let old = plan.created_unix() - scull_core::CLI_PLAN_TTL_SECS - 60;
    let expired = Plan::new(
        params_like(&plan, PlanId::from_suffix("EXPIRED1").unwrap(), old),
        plan.items().to_vec(),
    )
    .unwrap();
    env.put_plan(&expired, true);
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_expired".into()));
    assert!(env.exists(TARGET));
    env.check(&[]);
}

/// FX-EXEC-09: a plan made on another machine (another scull installation) or for another
/// user is rejected (exit 4).
#[test]
fn fx_exec_09_foreign_plans_are_rejected() {
    let env = Env::new();
    env.plan(&[TARGET]);
    let plan = env.read_plan();

    let mut p = params_like(
        &plan,
        PlanId::from_suffix("OTHERMACHINE").unwrap(),
        plan.created_unix(),
    );
    p.machine_id = "0123456789abcdef0123456789abcdef".into();
    let other_machine = Plan::new(p, plan.items().to_vec()).unwrap();
    env.put_plan(&other_machine, true);
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_foreign".into()));

    let mut p = params_like(
        &plan,
        PlanId::from_suffix("OTHERUSER").unwrap(),
        plan.created_unix(),
    );
    p.user = "uid:4242424".into();
    let other_user = Plan::new(p, plan.items().to_vec()).unwrap();
    env.put_plan(&other_user, true);
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_foreign".into()));

    // A plan from an MCP client is approved through the approval broker, not here.
    let mut p = params_like(
        &plan,
        PlanId::from_suffix("FROMMCP").unwrap(),
        plan.created_unix(),
    );
    p.origin = PlanOrigin::Mcp;
    p.ttl_secs = Some(60);
    let mcp = Plan::new(p, plan.items().to_vec()).unwrap();
    env.put_plan(&mcp, true);
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (4, "plan_foreign".into()));

    assert!(env.exists(TARGET));
    env.check(&[]);
}

/// FX-EXEC-10: an item protected after planning (`.scull-protect`, S3) is skipped with a
/// reason; the other item runs; exit 3.
#[test]
fn fx_exec_10_item_now_protected_is_skipped() {
    let env = Env::new();
    env.plan(&[TARGET, MODULES]);
    std::fs::write(env.fx.root().join("app/.scull-protect"), "").unwrap();
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(o.status.code(), Some(3), "partial: {}", stderr(&o));
    let run: Value = serde_json::from_slice(&o.stdout).unwrap();
    validate("clean-run", &run);
    let root = env.root();
    let t = run_item(&run, &root, TARGET);
    assert_eq!(t["outcome"], "skipped", "{t:#}");
    assert_eq!(t["reason_code"], "now_protected");
    assert!(env.exists(TARGET), "the protected item is untouched");
    let m = run_item(&run, &root, MODULES);
    assert!(matches!(
        m["outcome"].as_str(),
        Some("deleted" | "quarantined")
    ));
    assert!(!env.exists(MODULES));
    env.check_run(&run, &[("app/.scull-protect", None), (MODULES, Some(m))]);
}

/// SAFE-09: while another operation holds the execution lock, execution fails with exit 6 and
/// nothing changes.
#[test]
fn lock_held_exits_6() {
    let env = Env::new();
    env.plan(&[TARGET]);
    let lock = scull_cleaner::Executor::acquire(&scull_cleaner::LockOptions::at(
        env.home().join("state").join("execution.lock"),
    ))
    .unwrap();
    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(rejected(&o), (6, "lock_held".into()));
    assert!(env.exists(TARGET));
    drop(lock);
    env.check(&[]);
}

/// FX-EXEC-12: run as root/administrator (simulated), execution is refused without
/// `--allow-root` (exit 5) before anything is read or changed; planning still works.
#[test]
fn fx_exec_12_elevated_run_refused_without_allow_root() {
    let env = Env::new();
    env.plan(&[TARGET]);
    let elevated = |args: &[&str]| {
        env.cmd(args)
            .env("SCULL_TEST_ASSUME_ELEVATED", "1")
            .output()
            .unwrap()
    };
    let o = elevated(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    let (code, kind) = rejected(&o);
    assert_eq!((code, kind.as_str()), (5, "safety_blocked"));
    assert!(stderr(&o).contains("--allow-root"), "{}", stderr(&o));
    assert!(env.exists(TARGET));
    // Reviewing (no execution) is not refused.
    let o = elevated(&[
        "clean",
        "--plan",
        "plan.json",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    // With --allow-root it runs.
    let o = elevated(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-root",
        "--allow-network-roots",
        "--json",
    ]);
    let run = json_ok(&o);
    let root = env.root();
    assert!(!env.exists(TARGET));
    env.check_run(&run, &[(TARGET, Some(run_item(&run, &root, TARGET)))]);
}

/// FX-EXEC-11 / SAFE-07 / CLI-03: an S1 item whose action can't be undone (here a worktree
/// removal of a recorded plan) is refused without `--allow-permanent` when there is no
/// terminal (exit 5). With it, the gate passes and the Executor still revalidates: the fresh
/// action is not the same, so the item is skipped and nothing is removed (exit 3).
#[test]
fn fx_exec_11_irreversible_s1_needs_allow_permanent() {
    let env = Env::new();
    env.plan(&[TARGET]);
    let plan = env.read_plan();
    let mut item = plan.items()[0].clone();
    item.tier = Tier::S1;
    item.action = Action::GitWorktreeRemove;
    let permanent = Plan::new(
        params_like(
            &plan,
            PlanId::from_suffix("PERMANENT1").unwrap(),
            plan.created_unix(),
        ),
        vec![item],
    )
    .unwrap();
    env.put_plan(&permanent, true);

    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-network-roots",
        "--json",
    ]);
    let (code, kind) = rejected(&o);
    assert_eq!((code, kind.as_str()), (5, "safety_blocked"));
    assert!(stderr(&o).contains("--allow-permanent"), "{}", stderr(&o));
    assert!(env.exists(TARGET));

    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--allow-network-roots",
        "--json",
    ]);
    let review = json_ok(&o);
    assert_eq!(review["data"]["elevated_confirmation"]["required"], true);
    assert_eq!(review["data"]["elevated_confirmation"]["items"], 1);

    let o = env.run(&[
        "clean",
        "--plan",
        "plan.json",
        "--confirm",
        "--allow-permanent",
        "--allow-network-roots",
        "--json",
    ]);
    assert_eq!(o.status.code(), Some(3), "{}", stderr(&o));
    let run: Value = serde_json::from_slice(&o.stdout).unwrap();
    validate("clean-run", &run);
    assert_eq!(run["data"]["elevated_confirmation"], "flag");
    let t = run_item(&run, &env.root(), TARGET);
    assert_eq!(t["outcome"], "skipped", "{t:#}");
    assert!(env.exists(TARGET), "never removed");
    env.check(&[]);
}

/// A plan's roots come from history: executing without `--root` works from any directory,
/// also after later scans of the same root.
#[test]
fn plan_roots_come_from_history() {
    let env = Env::new();
    env.plan(&[MODULES]);
    let root = env.root();
    for _ in 0..2 {
        let o = env.run(&[
            "scan",
            root.to_str().unwrap(),
            "--allow-network-roots",
            "--json",
        ]);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    }
    let file = env.plan_file();
    let o = env
        .cmd(&[
            "clean",
            "--plan",
            file.to_str().unwrap(),
            "--confirm",
            "--allow-network-roots",
            "--json",
        ])
        .current_dir(env.fx.base().join("userhome"))
        .output()
        .unwrap();
    let run = json_ok(&o);
    assert!(!env.exists(MODULES));
    env.check_run(&run, &[(MODULES, Some(run_item(&run, &root, MODULES)))]);
}

/// Runs `args` under `script(1)` so scull sees a terminal, typing `input`. Linux only (BSD
/// `script` takes other flags); `None` when `script` can't run.
#[cfg(target_os = "linux")]
fn on_terminal(env: &Env, args: &[&str], input: &str) -> Option<Output> {
    use std::io::Write;
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let mut line = quote(env!("CARGO_BIN_EXE_scull"));
    for a in args {
        line.push(' ');
        line.push_str(&quote(a));
    }
    // Same environment as every other run, but through script(1).
    let template = env.cmd(&[]);
    let mut script = Command::new("script");
    script
        .args(["-qec", &line, "/dev/null"])
        .current_dir(env.fx.base())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in template.get_envs() {
        match v {
            Some(v) => script.env(k, v),
            None => script.env_remove(k),
        };
    }
    let mut child = script.spawn().ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    child.wait_with_output().ok()
}

/// On a terminal, `scull clean` ends with "Execute this plan now? [y/N]": anything but `y`
/// changes nothing; `y` executes. An irreversible S1/S2 item also needs its count typed: a
/// wrong count cancels (exit 130), the right one passes the gate.
#[cfg(target_os = "linux")]
#[test]
fn terminal_prompts_confirm_and_decline() {
    if Command::new("script").arg("--version").output().is_err() {
        eprintln!("SKIP: script(1) not available");
        return;
    }
    let env = Env::new();
    let root = env.root();
    let item = root.join(TARGET);
    let base = [
        "clean",
        "--root",
        root.to_str().unwrap(),
        "--allow-network-roots",
        "--no-progress",
        "--item",
        item.to_str().unwrap(),
    ];
    let o = on_terminal(&env, &base, "n\n").expect("script runs");
    let text = stdout(&o);
    assert!(text.contains("Execute this plan now"), "{text}");
    assert!(text.contains("Not executed"), "{text}");
    assert_eq!(o.status.code(), Some(0), "{text}");
    assert!(env.exists(TARGET), "declined: nothing changed");

    let o = on_terminal(&env, &base, "y\n").expect("script runs");
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{text}");
    assert!(text.contains("1 of 1 item(s) done"), "{text}");
    assert!(!env.exists(TARGET), "confirmed: executed");
    let h = json_ok(&env.run(&["history", "--json"]));
    assert_eq!(
        h["data"]["operations"][0]["method"], "cli_interactive",
        "answered at the prompt: {h:#}"
    );

    env.plan(&[MODULES]);
    let plan = env.read_plan();
    let mut it = plan.items()[0].clone();
    it.tier = Tier::S1;
    it.action = Action::GitWorktreeRemove;
    let permanent = Plan::new(
        params_like(
            &plan,
            PlanId::from_suffix("TYPED1").unwrap(),
            plan.created_unix(),
        ),
        vec![it],
    )
    .unwrap();
    env.put_plan(&permanent, true);
    let review = [
        "clean",
        "--plan",
        "plan.json",
        "--allow-network-roots",
        "--no-progress",
    ];
    let o = on_terminal(&env, &review, "y\n2\n").expect("script runs");
    let text = stdout(&o);
    assert!(text.contains("Type 1 to confirm"), "{text}");
    assert_eq!(o.status.code(), Some(130), "{text}");
    let o = on_terminal(&env, &review, "y\n1\n").expect("script runs");
    let text = stdout(&o);
    assert_eq!(
        o.status.code(),
        Some(3),
        "typed, then skipped at revalidation: {text}"
    );
    assert!(env.exists(MODULES));
    env.check(&[(TARGET, None)]);
}
