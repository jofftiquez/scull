//! The approval broker and the plan tools (P4-MCP-01, MCP-04, PRD §13.3), in process, against
//! generated fixture trees, with adversarial cases.
//!
//! * FX-MCP-07: the plan tools exist only with `mcp.destructive` in the config **and** the
//!   launch flag; otherwise they are absent and calling them is an unknown-tool error.
//! * FX-MCP-01…04: `execute_cleanup` without an approval, with an approval for another plan,
//!   with an expired approval, or a second time, fails closed and never reaches the runner.
//! * No token or code ever passes through MCP; the agent cannot approve its own plan.
//! * Forged, replayed and expired confirmation codes, plans of another user, concurrent
//!   approvals and concurrent consumption (exactly one wins), a plan whose item changed on
//!   disk after approval (void, D-34), S3 and unknown items refused at preparation.
//!
//! Execution here goes through a stand-in runner that does what the Executor does first
//! (consume the approval atomically, or refuse) and touches nothing on disk; the real
//! Executor path is tested in `scull-cleaner` (`tests/mcp_approval.rs`) and end to end through
//! the binary in `scull-cli` (`tests/mcp_approval.rs`). Every test checks the invariants.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use scull_core::config::{Config, McpConfig};
use scull_core::{ErrorKind, OperationId, Plan, PlanId, PlanOrigin, PlanParams, ScullError};
use scull_engine::{Engine, EngineConfig, FixedInUse, InUseSource, now_unix};
use scull_fixtures::{Fixture, InvariantInputs, NodeLock, days_ago};
use scull_mcp::approval::{self, ApprovalState, ConfirmationCode, HumanChannel, Principal, Reason};
use scull_mcp::{
    CleanupRunner, PLAN_TOOL_NAMES, RunItem, RunRequest, RunSummary, ScullMcp, ServerOptions,
    TOOL_NAMES,
};
use scull_storage::{ApprovalMethod, NewApproval, Store};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const MACHINE: &str = "machine-test";
const USER: &str = "tester";

fn who() -> Principal {
    Principal {
        machine_id: MACHINE.into(),
        user: USER.into(),
    }
}

/// A Cargo build output, an npm `node_modules`, and a protected project (S3), idle.
fn fixture() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 8192)
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
        .unwrap();
    for d in ["app", "web", "vault"] {
        fx.age(d, days_ago(3)).unwrap();
    }
    fx.checkpoint().unwrap();
    fx
}

fn engine() -> Engine {
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(scull_platform::Native));
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(scull_safety::Tri::No)));
    Engine::new(cfg).unwrap()
}

/// Stands in for the Executor-backed runner: counts calls, consumes the approval atomically
/// (what `Executor::begin_operation` does first) and touches nothing on disk.
#[derive(Default)]
struct StandIn {
    calls: AtomicUsize,
}

impl CleanupRunner for StandIn {
    fn run(&self, req: RunRequest<'_>) -> Result<RunSummary, ScullError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(req.approval.plan_id, *req.plan.id());
        assert!(HumanChannel::is_human(req.approval.method));
        let consumed = req
            .store
            .consume_approval(req.plan.id(), req.plan.content_hash(), now_unix())
            .map_err(ScullError::from)?;
        if !consumed {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "approval not consumable",
            ));
        }
        Ok(RunSummary {
            operation_id: OperationId::from_suffix("01JSTANDIN").unwrap(),
            items: req
                .plan
                .items()
                .iter()
                .map(|i| RunItem {
                    item_id: i.item_id.clone(),
                    outcome: "skipped".into(),
                    succeeded: false,
                    reason: Some("stand_in".into()),
                })
                .collect(),
            cancelled: false,
            measured_delta_bytes: None,
        })
    }
}

struct Env {
    fx: Fixture,
    data: tempfile::TempDir,
    runner: Arc<StandIn>,
}

impl Env {
    fn new() -> Self {
        Self {
            fx: fixture(),
            data: tempfile::tempdir().unwrap(),
            runner: Arc::new(StandIn::default()),
        }
    }

    fn server(&self, config_destructive: bool, launch_flag: bool) -> ScullMcp {
        let mcp = McpConfig {
            enabled: true,
            destructive: config_destructive,
            ..McpConfig::default()
        };
        ScullMcp::new(
            engine(),
            ServerOptions {
                roots: vec![self.fx.root().to_path_buf()],
                home: Some(self.fx.base().to_path_buf()),
                data_dir: Some(self.data.path().to_path_buf()),
                mcp,
                threads: 2,
                cache_env: None,
            },
        )
        .unwrap()
        .with_plan_tools(launch_flag, self.runner.clone(), who())
    }

    fn store(&self) -> Store {
        scull_engine::open_history(self.data.path()).unwrap()
    }

    fn calls(&self) -> usize {
        self.runner.calls.load(Ordering::SeqCst)
    }

    fn invariants(&self, allowed: &[&str]) {
        let mut inputs = InvariantInputs::new();
        for a in allowed {
            inputs = inputs.allow(*a);
        }
        let r = self.fx.check_invariants(&inputs);
        println!("{r}");
        r.assert_ok();
    }
}

async fn connect(server: ScullMcp) -> RunningService<RoleClient, ()> {
    let (s, c) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let _ = scull_mcp::serve(server, s).await;
    });
    ().serve(c).await.expect("client connects")
}

fn args(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => panic!("arguments must be an object"),
    }
}

async fn call(client: &RunningService<RoleClient, ()>, name: &str, a: Value) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(a)))
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn ok(name: &str, r: &CallToolResult) -> Value {
    assert_ne!(r.is_error, Some(true), "{name} failed: {:?}", r.content);
    r.structured_content.clone().expect("structured content")
}

fn error_code(r: &CallToolResult) -> String {
    assert_eq!(r.is_error, Some(true), "expected an error: {r:?}");
    r.structured_content.as_ref().unwrap()["error"]["code"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn text_of(r: &CallToolResult) -> String {
    let mut s = serde_json::to_string(&r.structured_content).unwrap();
    for c in &r.content {
        if let Some(t) = c.as_text() {
            s.push_str(&t.text);
        }
    }
    s
}

/// Candidate IDs by path suffix, from `get_cleanup_candidates` (all tiers).
async fn candidate(client: &RunningService<RoleClient, ()>, suffix: &str) -> Value {
    let r = call(
        client,
        "get_cleanup_candidates",
        json!({ "tiers": ["S0", "S1", "S2", "S3"] }),
    )
    .await;
    let v = ok("get_cleanup_candidates", &r);
    v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| {
            c["path"]
                .as_str()
                .unwrap()
                .replace('\\', "/")
                .ends_with(suffix)
        })
        .unwrap_or_else(|| panic!("no candidate {suffix}: {v:#}"))
        .clone()
}

async fn prepare(client: &RunningService<RoleClient, ()>, suffixes: &[&str]) -> Value {
    let mut ids = Vec::new();
    for s in suffixes {
        ids.push(candidate(client, s).await["id"].clone());
    }
    let r = call(client, "prepare_cleanup", json!({ "item_ids": ids })).await;
    ok("prepare_cleanup", &r)
}

fn id_of(v: &Value) -> PlanId {
    PlanId::try_from(v["plan_id"].as_str().unwrap().to_owned()).unwrap()
}

/// What the human does in `scull approve`: review, type the code shown.
fn human_approves(store: &mut Store, id: &PlanId) {
    let ch = approval::begin(store, id, &who(), now_unix())
        .unwrap()
        .with_code(ConfirmationCode::parse("HJK-MNP").unwrap());
    approval::approve(
        store,
        &ch,
        "hjk mnp",
        HumanChannel::Terminal,
        &who(),
        now_unix(),
    )
    .unwrap();
}

fn names(tools: &[rmcp::model::Tool]) -> Vec<String> {
    tools.iter().map(|t| t.name.to_string()).collect()
}

// ---------------------------------------------------------------------------------------------

/// FX-MCP-07: destructive tools are hidden with only the flag or only the config; both are
/// required. Hidden tools are unknown when called.
#[tokio::test(flavor = "multi_thread")]
async fn fx_mcp_07_plan_tools_need_both_the_config_and_the_flag() {
    let env = Env::new();
    for (config, flag) in [(false, false), (true, false), (false, true), (true, true)] {
        let server = env.server(config, flag);
        assert_eq!(server.plan_tools_enabled(), config && flag);
        let client = connect(server).await;
        let listed = names(&client.list_all_tools().await.unwrap());
        let mut want: Vec<String> = TOOL_NAMES.iter().map(|s| (*s).to_owned()).collect();
        if config && flag {
            want.extend(PLAN_TOOL_NAMES.iter().map(|s| (*s).to_owned()));
        }
        assert_eq!(listed, want, "config={config} flag={flag}");
        let status = ok("get_status", &call(&client, "get_status", json!({})).await);
        assert_eq!(status["server"]["read_only"], json!(!(config && flag)));
        if !(config && flag) {
            for t in PLAN_TOOL_NAMES {
                let r = client
                    .call_tool(
                        CallToolRequestParams::new(t.to_owned())
                            .with_arguments(args(json!({ "plan_id": "x" }))),
                    )
                    .await;
                assert!(
                    r.is_err(),
                    "{t} must be unknown (config={config} flag={flag})"
                );
            }
        }
        client.cancel().await.unwrap();
    }
    assert_eq!(env.calls(), 0);
    env.invariants(&[]);
}

/// The agent prepares a plan: recorded (origin mcp, 15 minutes), awaiting approval, with no
/// token or code anywhere in the result; nothing on disk changes.
#[tokio::test(flavor = "multi_thread")]
async fn prepare_records_a_plan_and_returns_no_token() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let v = prepare(&client, &["app/target"]).await;
    assert_eq!(v["approval"]["state"], "awaiting_approval");
    assert_eq!(v["item_count"], 1);
    assert_eq!(
        v["expires_at"].as_i64().unwrap() - v["created_at"].as_i64().unwrap(),
        15 * 60
    );
    assert!(v["next_step"].as_str().unwrap().contains("scull approve"));
    let flat = v.to_string().to_lowercase();
    for word in ["token", "code\"", "approval_id", "secret"] {
        assert!(!flat.contains(word), "{word} in {v:#}");
    }
    let store = env.store();
    let plan = store.get_plan(&id_of(&v)).unwrap().unwrap();
    assert_eq!(plan.origin(), scull_core::PlanOrigin::Mcp);
    assert_eq!(plan.user(), USER);
    assert!(store.get_approval(plan.id()).unwrap().is_none());
    let listed = approval::awaiting(&store, &who(), now_unix()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].plan_id, plan.id().as_str());
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// S3 items, inspect-only items and unknown IDs are never planned; nothing is recorded.
#[tokio::test(flavor = "multi_thread")]
async fn prepare_refuses_protected_and_unknown_items() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let vault = candidate(&client, "vault/target").await;
    assert_eq!(vault["tier"], "S3");
    let good = candidate(&client, "app/target").await;
    for ids in [
        json!([vault["id"], good["id"]]),
        json!(["item-does-not-exist"]),
        json!([]),
    ] {
        let r = call(&client, "prepare_cleanup", json!({ "item_ids": ids })).await;
        let code = error_code(&r);
        assert!(
            matches!(code.as_str(), "not_plannable" | "invalid_arguments"),
            "{code}"
        );
    }
    let r = call(
        &client,
        "prepare_cleanup",
        json!({ "item_ids": [good["id"]], "path": "/etc" }),
    )
    .await;
    assert_eq!(error_code(&r), "outside_allowlist");
    assert!(
        env.store()
            .plans_awaiting_approval(scull_core::PlanOrigin::Mcp, now_unix(), 10)
            .unwrap()
            .is_empty()
    );
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// FX-MCP-01, then the whole approved flow: without an approval `execute_cleanup` fails
/// closed (the runner is never called); after the human approves in the terminal it runs
/// once, with the approval's channel; FX-MCP-04: a second execution is refused.
#[tokio::test(flavor = "multi_thread")]
async fn fx_mcp_01_and_04_execute_needs_an_unused_human_approval() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let v = prepare(&client, &["app/target", "web/node_modules"]).await;
    let id = id_of(&v);
    let plan_id = json!({ "plan_id": id.as_str() });

    // FX-MCP-01
    let r = call(&client, "execute_cleanup", plan_id.clone()).await;
    assert_eq!(error_code(&r), "not_approved");
    assert!(text_of(&r).contains("scull approve"));
    assert_eq!(env.calls(), 0);
    let s = ok(
        "status",
        &call(&client, "get_approval_status", plan_id.clone()).await,
    );
    assert_eq!(s["state"], "awaiting_approval");
    assert_eq!(s["executable"], false);

    human_approves(&mut env.store(), &id);
    let s = ok(
        "status",
        &call(&client, "get_approval_status", plan_id.clone()).await,
    );
    assert_eq!(s["state"], "approved");
    assert_eq!(s["channel"], "terminal");
    assert_eq!(s["executable"], true);
    let shown = call(&client, "get_cleanup_plan", plan_id.clone()).await;
    assert!(
        !text_of(&shown).contains("HJK"),
        "the code never reaches the agent"
    );
    let shown = ok("get_cleanup_plan", &shown);
    assert_eq!(shown["approval"]["state"], "approved");
    let pending = ok(
        "get_cleanup_status",
        &call(&client, "get_cleanup_status", plan_id.clone()).await,
    );
    assert_eq!(pending["state"], "approved");

    let r = ok(
        "execute",
        &call(&client, "execute_cleanup", plan_id.clone()).await,
    );
    assert_eq!(r["approved_via"], "terminal");
    assert_eq!(r["items"].as_array().unwrap().len(), 2);
    assert_eq!(env.calls(), 1);

    // FX-MCP-04
    let again = call(&client, "execute_cleanup", plan_id.clone()).await;
    assert_eq!(error_code(&again), "approval_used");
    assert_eq!(env.calls(), 1, "never reaches the runner again");
    let used = ok(
        "status",
        &call(&client, "get_approval_status", plan_id).await,
    );
    assert_eq!(used["state"], "used");

    // Every structured result validates against the tool's published output schema.
    let tools = client.list_all_tools().await.unwrap();
    for (name, v) in [
        ("prepare_cleanup", &v),
        ("get_approval_status", &s),
        ("get_approval_status", &used),
        ("get_cleanup_plan", &shown),
        ("get_cleanup_status", &pending),
        ("execute_cleanup", &r),
    ] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        let schema = Value::Object(tool.output_schema.as_ref().unwrap().as_ref().clone());
        let validator = jsonschema::validator_for(&schema).expect("schema compiles");
        let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{name}: {errors:?}\n{v:#}");
    }
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// FX-MCP-02: an approval of one plan never authorizes another (different hash and ID).
#[tokio::test(flavor = "multi_thread")]
async fn fx_mcp_02_approval_for_a_different_plan_fails() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let a = id_of(&prepare(&client, &["app/target"]).await);
    let b = id_of(&prepare(&client, &["web/node_modules"]).await);
    let mut store = env.store();
    assert_ne!(
        store.get_plan(&a).unwrap().unwrap().content_hash(),
        store.get_plan(&b).unwrap().unwrap().content_hash()
    );
    human_approves(&mut store, &a);
    let r = call(&client, "execute_cleanup", json!({ "plan_id": b.as_str() })).await;
    assert_eq!(error_code(&r), "not_approved");
    // A hand-written approval row bound to the other plan's hash is refused by the store.
    let plan_a = store.get_plan(&a).unwrap().unwrap();
    let err = store
        .insert_approval(&NewApproval {
            plan_id: b.clone(),
            content_hash: plan_a.content_hash().clone(),
            method: ApprovalMethod::CliTty,
            approver: USER.into(),
            approved_at: now_unix(),
            expires_at: now_unix() + 60,
        })
        .unwrap_err();
    assert_eq!(ScullError::from(err).kind, ErrorKind::PlanTampered);
    assert_eq!(env.calls(), 0);
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// FX-MCP-03: an expired approval fails (server path: a 2-second approval; broker path: the
/// plan's 15 minutes), and so does approving an expired plan.
#[tokio::test(flavor = "multi_thread")]
async fn fx_mcp_03_expired_approval_fails() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target"]).await);
    let mut store = env.store();
    let plan = store.get_plan(&id).unwrap().unwrap();
    let now = now_unix();
    store
        .insert_approval(&NewApproval {
            plan_id: id.clone(),
            content_hash: plan.content_hash().clone(),
            method: ApprovalMethod::CliTty,
            approver: USER.into(),
            approved_at: now,
            expires_at: now + 2,
        })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3100));
    let r = call(
        &client,
        "execute_cleanup",
        json!({ "plan_id": id.as_str() }),
    )
    .await;
    assert_eq!(error_code(&r), "approval_expired");
    let s = ok(
        "status",
        &call(
            &client,
            "get_approval_status",
            json!({ "plan_id": id.as_str() }),
        )
        .await,
    );
    assert_eq!(s["state"], "expired");
    assert_eq!(env.calls(), 0);

    // Broker clock: after the plan's 15 minutes nothing authorizes or approves it.
    let id2 = id_of(&prepare(&client, &["web/node_modules"]).await);
    let later = now_unix() + 15 * 60;
    let ch = approval::begin(&store, &id2, &who(), now_unix())
        .unwrap()
        .with_code(ConfirmationCode::parse("ABCDEF").unwrap());
    let e = approval::approve(
        &mut store,
        &ch,
        "ABCDEF",
        HumanChannel::Terminal,
        &who(),
        later,
    )
    .unwrap_err();
    assert_eq!(e.reason, Reason::PlanExpired);
    assert!(store.get_approval(&id2).unwrap().is_none());
    human_approves(&mut store, &id2);
    let e = approval::authorize(&store, &id2, &who(), later).unwrap_err();
    assert_eq!(e.reason, Reason::PlanExpired);
    assert!(approval::authorize(&store, &id2, &who(), now_unix()).is_ok());
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// Forged and replayed confirmation codes: a wrong code, the code of another attempt, a
/// guess, or the right code a second time approve nothing more; another account can't even
/// start; an approved plan can't be challenged again.
#[tokio::test(flavor = "multi_thread")]
async fn forged_and_replayed_codes_approve_nothing() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target"]).await);
    let mut store = env.store();
    let now = now_unix();

    let first = approval::begin(&store, &id, &who(), now)
        .unwrap()
        .with_code(ConfirmationCode::parse("AAA-AAA").unwrap());
    let second = approval::begin(&store, &id, &who(), now)
        .unwrap()
        .with_code(ConfirmationCode::parse("BBB-BBB").unwrap());
    for typed in [
        "",
        "yes",
        "y",
        "AAA-AAB",
        "BBB-BBB",
        "AAAAAAA",
        "approve",
        "execute_cleanup",
    ] {
        let e = approval::approve(
            &mut store,
            &first,
            typed,
            HumanChannel::Terminal,
            &who(),
            now,
        )
        .unwrap_err();
        assert_eq!(e.reason, Reason::WrongCode, "{typed:?}");
    }
    assert!(
        store.get_approval(&id).unwrap().is_none(),
        "nothing recorded"
    );
    // Another OS user (or machine) can neither start nor finish an approval.
    let mallory = Principal {
        machine_id: MACHINE.into(),
        user: "mallory".into(),
    };
    assert_eq!(
        approval::begin(&store, &id, &mallory, now)
            .unwrap_err()
            .reason,
        Reason::PlanForeign
    );
    assert_eq!(
        approval::approve(
            &mut store,
            &first,
            "AAAAAA",
            HumanChannel::Terminal,
            &mallory,
            now
        )
        .unwrap_err()
        .reason,
        Reason::PlanForeign
    );
    // The right code approves once; replaying it, or the other attempt's code, does not.
    approval::approve(
        &mut store,
        &first,
        "aaa-aaa",
        HumanChannel::Terminal,
        &who(),
        now,
    )
    .unwrap();
    let e = approval::approve(
        &mut store,
        &first,
        "AAA-AAA",
        HumanChannel::Terminal,
        &who(),
        now,
    )
    .unwrap_err();
    assert_eq!(e.reason, Reason::AlreadyApproved);
    let e = approval::approve(
        &mut store,
        &second,
        "BBB-BBB",
        HumanChannel::Desktop,
        &who(),
        now,
    )
    .unwrap_err();
    assert_eq!(e.reason, Reason::AlreadyApproved);
    assert_eq!(
        approval::begin(&store, &id, &who(), now)
            .unwrap_err()
            .reason,
        Reason::AlreadyApproved
    );
    // CLI plans are not agent plans: the broker does not know them.
    assert_eq!(
        approval::begin(&store, &PlanId::from_suffix("nope").unwrap(), &who(), now)
            .unwrap_err()
            .reason,
        Reason::UnknownPlan
    );
    let a = store.get_approval(&id).unwrap().unwrap();
    assert_eq!(a.method, ApprovalMethod::CliTty);
    assert_eq!(a.approver, USER);
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// D-34: an item of an approved agent plan changed on disk (replaced by another folder):
/// the whole plan is void, its approval spent, the runner never called, and it stays void.
#[tokio::test(flavor = "multi_thread")]
async fn plan_mutated_on_disk_after_approval_is_void() {
    let mut env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target", "web/node_modules"]).await);
    human_approves(&mut env.store(), &id);
    // The build output is replaced (same path, new folder: a different file identity).
    let target = env.fx.root().join("app/target");
    std::fs::rename(&target, env.fx.root().join("app/target-old")).unwrap();
    std::fs::create_dir_all(target.join("debug")).unwrap();
    std::fs::write(target.join("debug/app"), b"rebuilt").unwrap();
    std::fs::write(
        target.join("CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55",
    )
    .unwrap();
    env.fx.age("app", days_ago(3)).unwrap();

    let p = json!({ "plan_id": id.as_str() });
    let r = call(&client, "execute_cleanup", p.clone()).await;
    assert_eq!(error_code(&r), "plan_changed");
    assert!(text_of(&r).contains("void"));
    assert_eq!(env.calls(), 0);
    let r = call(&client, "execute_cleanup", p.clone()).await;
    assert_eq!(error_code(&r), "approval_used", "stays void");
    let st = ok("status", &call(&client, "get_cleanup_status", p).await);
    assert_eq!(st["state"], "voided");
    assert!(st["operation"].is_null());
    assert_eq!(env.calls(), 0);
    // Both refusals are audited (PRD §13.3 step 5).
    assert_eq!(
        audited(&env),
        vec![
            refusal("plan_changed", Some(&id)),
            refusal("approval_used", Some(&id))
        ]
    );
    client.cancel().await.unwrap();
    env.invariants(&["app/target", "app/target-old", "app"]);
}

/// D-34, changes `narrow` would accept for a CLI plan: the same folder (same inode) whose
/// contents grew after approval voids the agent plan, spends its approval, never reaches
/// the runner, and reports `size_changed`.
#[tokio::test(flavor = "multi_thread")]
async fn same_folder_with_changed_contents_voids_the_plan() {
    let mut env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target"]).await);
    human_approves(&mut env.store(), &id);
    let target = env.fx.root().join("app/target");
    use scull_platform::Platform;
    let identity = |p: &std::path::Path| scull_platform::Native.metadata(p).unwrap().identity;
    let before = identity(&target);
    std::fs::write(target.join("debug/extra.bin"), vec![7u8; 256 * 1024]).unwrap();
    env.fx.age("app", days_ago(3)).unwrap();
    assert_eq!(before, identity(&target), "same folder object");
    let p = json!({ "plan_id": id.as_str() });
    let r = call(&client, "execute_cleanup", p.clone()).await;
    assert_eq!(error_code(&r), "plan_changed");
    assert!(text_of(&r).contains("size_changed"), "{}", text_of(&r));
    let st = ok("status", &call(&client, "get_cleanup_status", p).await);
    assert_eq!(st["state"], "voided");
    assert_eq!(env.calls(), 0);
    client.cancel().await.unwrap();
    env.invariants(&["app/target", "app"]);
}

/// D-34: a tier that went *down* after approval (a build output that was recently active,
/// S1, is idle by execution time, S0) still voids the agent plan; `narrow` alone would run
/// it at the lower tier.
#[tokio::test(flavor = "multi_thread")]
async fn lowered_tier_voids_the_plan() {
    let mut env = Env::new();
    // Recently written: the activity window makes it more protective at planning time.
    env.fx
        .set_mtime("app/target/debug/app", std::time::SystemTime::now())
        .unwrap();
    env.fx.checkpoint().unwrap();
    let client = connect(env.server(true, true)).await;
    let planned = candidate(&client, "app/target").await;
    assert_ne!(
        planned["tier"], "S0",
        "recent activity raises the tier: {planned:#}"
    );
    let id = id_of(&prepare(&client, &["app/target"]).await);
    human_approves(&mut env.store(), &id);
    env.fx
        .set_mtime("app/target/debug/app", days_ago(3))
        .unwrap();
    let r = call(
        &client,
        "execute_cleanup",
        json!({ "plan_id": id.as_str() }),
    )
    .await;
    assert_eq!(error_code(&r), "plan_changed");
    assert!(
        text_of(&r).contains("tier_changed") || text_of(&r).contains("action_changed"),
        "{}",
        text_of(&r)
    );
    assert_eq!(env.calls(), 0);
    assert!(
        env.store()
            .get_approval(&id)
            .unwrap()
            .unwrap()
            .used_at
            .is_some(),
        "approval spent"
    );
    client.cancel().await.unwrap();
    env.invariants(&["app/target"]);
}

/// Concurrent approvals of one plan: exactly one is recorded. Concurrent consumption of one
/// approval (several executors racing): exactly one wins.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_approvals_and_consumption_have_exactly_one_winner() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target"]).await);
    client.cancel().await.unwrap();
    let data: PathBuf = env.data.path().to_path_buf();
    // Four approval attempts (terminal and desktop) are all shown before any is answered,
    // then answered at the same instant, each through its own database connection.
    let codes = ["CCCCCC", "DDDDDD", "EEEEEE", "FFFFFF"];
    let barrier = Arc::new(std::sync::Barrier::new(codes.len()));
    let handles: Vec<_> = codes
        .into_iter()
        .enumerate()
        .map(|(n, code)| {
            let store = scull_engine::open_history(&data).unwrap();
            let ch = approval::begin(&store, &id, &who(), now_unix())
                .unwrap()
                .with_code(ConfirmationCode::parse(code).unwrap());
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut store = store;
                let channel = if n % 2 == 0 {
                    HumanChannel::Terminal
                } else {
                    HumanChannel::Desktop
                };
                barrier.wait();
                approval::approve(&mut store, &ch, code, channel, &who(), now_unix())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "{results:?}");
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(r.reason, Reason::AlreadyApproved, "{r}");
    }

    let plan = env.store().get_plan(&id).unwrap().unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let consumers: Vec<_> = (0..8)
        .map(|_| {
            let (data, plan) = (data.clone(), plan.clone());
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut store = scull_engine::open_history(&data).unwrap();
                barrier.wait();
                store
                    .consume_approval(plan.id(), plan.content_hash(), now_unix())
                    .unwrap()
            })
        })
        .collect();
    let consumed = consumers
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|c| *c)
        .count();
    assert_eq!(consumed, 1);
    let status = approval::status(&env.store(), &id, &who(), now_unix()).unwrap();
    assert_eq!(status.state, ApprovalState::Used);
    env.invariants(&[]);
}

/// The agent can't approve through any tool: no tool takes a code, no argument smuggles one.
#[tokio::test(flavor = "multi_thread")]
async fn no_tool_can_approve() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target"]).await);
    for t in PLAN_TOOL_NAMES {
        let r = call(
            &client,
            t,
            json!({ "plan_id": id.as_str(), "code": "ABCDEF", "approve": true }),
        )
        .await;
        assert_eq!(error_code(&r), "invalid_arguments", "{t}");
    }
    let tools = client.list_all_tools().await.unwrap();
    for t in &tools {
        let schema = serde_json::to_string(&t.input_schema).unwrap();
        assert!(!schema.contains("\"code\""), "{}", t.name);
        assert!(!schema.contains("approve\""), "{}", t.name);
    }
    assert!(env.store().get_approval(&id).unwrap().is_none());
    assert_eq!(env.calls(), 0);
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// The agent audit events recorded so far: `(kind, source, reason, plan)`, oldest first.
fn audited(env: &Env) -> Vec<(String, String, String, Option<String>)> {
    env.store()
        .agent_events(0, 10_000)
        .unwrap()
        .into_iter()
        .map(|e| {
            assert_eq!(e.machine_id, MACHINE);
            assert_eq!(e.user, USER);
            (
                e.kind.as_str().to_owned(),
                e.source,
                e.reason,
                e.plan_id.map(|p| p.as_str().to_owned()),
            )
        })
        .collect()
}

fn refusal(reason: &str, plan: Option<&PlanId>) -> (String, String, String, Option<String>) {
    (
        "execute_refused".into(),
        "mcp".into(),
        reason.into(),
        plan.map(|p| p.as_str().to_owned()),
    )
}

/// PRD §13.3 step 5: every refused `execute_cleanup` is audited with its reason (invalid
/// arguments, a smuggled code, unknown, foreign, expired, unapproved, non-human, used), the
/// plan and its hash, and no refusal creates, spends or changes an approval or reaches the
/// runner. The code never appears in the audit.
#[tokio::test(flavor = "multi_thread")]
async fn every_refused_execute_is_audited_without_touching_approvals() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let a = id_of(&prepare(&client, &["app/target"]).await);
    let b = id_of(&prepare(&client, &["web/node_modules"]).await);
    let mut store = env.store();
    let plan_a = store.get_plan(&a).unwrap().unwrap();
    let plan_b = store.get_plan(&b).unwrap().unwrap();
    let variant = |id: &str, created: i64, user: &str| {
        Plan::new(
            PlanParams {
                id: PlanId::from_suffix(id).unwrap(),
                scan_id: plan_a.scan_id().clone(),
                created_unix: created,
                ttl_secs: None,
                machine_id: MACHINE.into(),
                user: user.into(),
                origin: PlanOrigin::Mcp,
            },
            plan_a.items().to_vec(),
        )
        .unwrap()
    };
    let foreign = variant("01JAUDMALLORY", now_unix(), "mallory");
    let expired = variant("01JAUDEXPIRED", now_unix() - 16 * 60, USER);
    store.insert_plan(&foreign).unwrap();
    store.insert_plan(&expired).unwrap();
    store
        .insert_approval(&NewApproval {
            plan_id: b.clone(),
            content_hash: plan_b.content_hash().clone(),
            method: ApprovalMethod::CliFlag,
            approver: USER.into(),
            approved_at: now_unix(),
            expires_at: plan_b.expires_unix(),
        })
        .unwrap();
    let unknown = PlanId::from_suffix("01JAUDNOPE").unwrap();

    let cases: Vec<(Value, &str)> = vec![
        (json!({ "plan_id": "not a plan" }), "invalid_arguments"),
        (json!({ "plan_id": unknown.as_str() }), "unknown_plan"),
        (json!({ "plan_id": a.as_str() }), "not_approved"),
        (
            json!({ "plan_id": a.as_str(), "code": "ABCDEF", "approve": true }),
            "invalid_arguments",
        ),
        (json!({ "plan_id": foreign.id().as_str() }), "plan_foreign"),
        (json!({ "plan_id": expired.id().as_str() }), "plan_expired"),
        (json!({ "plan_id": b.as_str() }), "not_human"),
    ];
    for (args, code) in &cases {
        let r = call(&client, "execute_cleanup", args.clone()).await;
        assert_eq!(error_code(&r), *code, "{args}");
    }
    assert_eq!(env.calls(), 0, "no refusal reaches the runner");
    for id in [&a, foreign.id(), expired.id(), &unknown] {
        assert!(store.get_approval(id).unwrap().is_none(), "none created");
    }
    assert_eq!(store.get_approval(&b).unwrap().unwrap().used_at, None);

    human_approves(&mut store, &a);
    ok(
        "execute",
        &call(&client, "execute_cleanup", json!({ "plan_id": a.as_str() })).await,
    );
    let r = call(&client, "execute_cleanup", json!({ "plan_id": a.as_str() })).await;
    assert_eq!(error_code(&r), "approval_used");
    assert_eq!(env.calls(), 1);

    assert_eq!(
        audited(&env),
        vec![
            refusal("invalid_arguments", None),
            refusal("unknown_plan", Some(&unknown)),
            refusal("not_approved", Some(&a)),
            refusal("invalid_arguments", Some(&a)),
            refusal("plan_foreign", Some(foreign.id())),
            refusal("plan_expired", Some(expired.id())),
            refusal("not_human", Some(&b)),
            refusal("approval_used", Some(&a)),
        ]
    );
    let events = store.agent_events(0, 10_000).unwrap();
    // Plan hashes as recorded (none for an unknown or invalid plan); channel `mcp`.
    assert_eq!(events[2].content_hash.as_ref(), Some(plan_a.content_hash()));
    assert_eq!(events[6].content_hash.as_ref(), Some(plan_b.content_hash()));
    assert_eq!(events[1].content_hash, None);
    assert!(events.iter().all(|e| e.channel == "mcp"));
    let dump = format!("{events:?}");
    assert!(!dump.contains("ABCDEF"), "no code in the audit: {dump}");
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// The broker's denial API (what `scull approve` and the desktop dialog call): one
/// `approval_denied` event per denial with its reason, channel and source, and nothing about
/// the approval changes; a later approval still works.
#[tokio::test(flavor = "multi_thread")]
async fn denials_are_audited_and_leave_the_plan_approvable() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target"]).await);
    let mut store = env.store();
    let hash = approval::recorded_hash(&store, &id).unwrap();
    for (denial, channel) in [
        (approval::Denial::Denied, HumanChannel::Terminal),
        (approval::Denial::WrongCode, HumanChannel::Desktop),
        (approval::Denial::NoTerminal, HumanChannel::Terminal),
        (
            approval::Denial::Refused(Reason::PlanExpired),
            HumanChannel::Terminal,
        ),
    ] {
        approval::record_denial(
            &mut store,
            &id,
            Some(&hash),
            denial,
            channel,
            &who(),
            now_unix(),
        )
        .unwrap();
    }
    assert!(store.get_approval(&id).unwrap().is_none());
    let events = store.agent_events(0, 10_000).unwrap();
    let got: Vec<(&str, &str, &str, &str)> = events
        .iter()
        .map(|e| {
            (
                e.kind.as_str(),
                e.source.as_str(),
                e.channel.as_str(),
                e.reason.as_str(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("approval_denied", "cli", "terminal", "denied"),
            ("approval_denied", "desktop", "desktop", "wrong_code"),
            ("approval_denied", "cli", "terminal", "no_terminal"),
            ("approval_denied", "cli", "terminal", "plan_expired"),
        ]
    );
    assert!(
        events
            .iter()
            .all(|e| e.content_hash.as_ref() == Some(&hash))
    );
    human_approves(&mut store, &id);
    let r = ok(
        "execute",
        &call(
            &client,
            "execute_cleanup",
            json!({ "plan_id": id.as_str() }),
        )
        .await,
    );
    assert_eq!(r["approved_via"], "terminal");
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// Audit flooding: an agent looping on `execute_cleanup` (250 refusals of one plan, plus 100
/// made-up plan IDs) leaves a bounded number of audit rows, with the counts kept. (1000
/// refusals through the broker are checked below, and 1000 + 1000 in scull-storage.)
#[tokio::test(flavor = "multi_thread")]
async fn looping_refused_executes_leave_bounded_audit_rows() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target"]).await);
    for _ in 0..250 {
        let r = call(
            &client,
            "execute_cleanup",
            json!({ "plan_id": id.as_str() }),
        )
        .await;
        assert_eq!(error_code(&r), "not_approved");
    }
    for n in 0..100 {
        let made_up = PlanId::from_suffix(&format!("01JFAKE{n}")).unwrap();
        let r = call(
            &client,
            "execute_cleanup",
            json!({ "plan_id": made_up.as_str() }),
        )
        .await;
        assert_eq!(error_code(&r), "unknown_plan");
    }
    let store = env.store();
    let rows = store.agent_event_rows(0).unwrap();
    assert!(rows <= 4, "{rows} rows for 350 refusals");
    let events = store.agent_events(0, 100).unwrap();
    let total: u64 = events.iter().map(|e| e.count).sum();
    assert_eq!(total, 350, "every refusal is counted");
    // 1000 more through the broker's own recorder: still bounded.
    let mut store = store;
    for _ in 0..1000 {
        approval::record_execute_refusal(
            &mut store,
            Some(&id),
            None,
            "not_approved",
            &who(),
            now_unix(),
        )
        .unwrap();
    }
    assert!(store.agent_event_rows(0).unwrap() <= rows + 1);
    assert_eq!(env.calls(), 0);
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// #298: the human revokes an approval before the agent executes: the plan reports
/// `revoked`, `execute_cleanup` is refused (`approval_revoked`, audited) without reaching the
/// runner, and the revocation itself is audited. Revoking needs an unused approval of this
/// user's plan: an unapproved, foreign, already revoked or already executed plan is refused,
/// and no refusal creates or spends an approval.
#[tokio::test(flavor = "multi_thread")]
async fn revoked_approval_never_executes() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let id = id_of(&prepare(&client, &["app/target"]).await);
    let p = json!({ "plan_id": id.as_str() });
    let mut store = env.store();

    // Nothing to revoke yet: refused, and no approval appears.
    let e =
        approval::revoke(&mut store, &id, HumanChannel::Terminal, &who(), now_unix()).unwrap_err();
    assert_eq!(e.reason, Reason::NotApproved);
    assert!(store.get_approval(&id).unwrap().is_none());

    human_approves(&mut store, &id);
    let listed = approval::revocable(&store, &who(), now_unix()).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].plan_id, id.as_str());
    // Another account can't revoke it.
    let mallory = Principal {
        machine_id: MACHINE.into(),
        user: "mallory".into(),
    };
    let e = approval::revoke(
        &mut store,
        &id,
        HumanChannel::Terminal,
        &mallory,
        now_unix(),
    )
    .unwrap_err();
    assert_eq!(e.reason, Reason::PlanForeign);
    assert_eq!(store.get_approval(&id).unwrap().unwrap().used_at, None);

    let r = approval::revoke(&mut store, &id, HumanChannel::Desktop, &who(), now_unix()).unwrap();
    assert_eq!(r.channel, "desktop");
    assert!(
        approval::revocable(&store, &who(), now_unix())
            .unwrap()
            .is_empty()
    );
    let e =
        approval::revoke(&mut store, &id, HumanChannel::Terminal, &who(), now_unix()).unwrap_err();
    assert_eq!(e.reason, Reason::ApprovalRevoked, "revoked once");

    let s = ok(
        "status",
        &call(&client, "get_approval_status", p.clone()).await,
    );
    assert_eq!(s["state"], "revoked");
    assert_eq!(s["executable"], false);
    let r = call(&client, "execute_cleanup", p.clone()).await;
    assert_eq!(error_code(&r), "approval_revoked");
    assert!(text_of(&r).contains("revoked"), "{}", text_of(&r));
    assert_eq!(env.calls(), 0, "never reaches the runner");
    let st = ok(
        "cleanup status",
        &call(&client, "get_cleanup_status", p.clone()).await,
    );
    assert_eq!(st["state"], "revoked");
    let shown = ok("plan", &call(&client, "get_cleanup_plan", p).await);
    assert_eq!(shown["approval"]["state"], "revoked");
    // The published output schemas accept the new state.
    let tools = client.list_all_tools().await.unwrap();
    for (name, v) in [
        ("get_approval_status", &s),
        ("get_cleanup_status", &st),
        ("get_cleanup_plan", &shown),
    ] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        let schema = Value::Object(tool.output_schema.as_ref().unwrap().as_ref().clone());
        let validator = jsonschema::validator_for(&schema).expect("schema compiles");
        let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{name}: {errors:?}\n{v:#}");
    }
    let events = env.store().agent_events(0).unwrap();
    let got: Vec<(&str, &str, &str, &str)> = events
        .iter()
        .map(|e| {
            (
                e.kind.as_str(),
                e.source.as_str(),
                e.channel.as_str(),
                e.reason.as_str(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("approval_revoked", "desktop", "desktop", "revoked"),
            ("execute_refused", "mcp", "mcp", "approval_revoked"),
        ]
    );

    // An executed plan's approval can't be revoked after the fact.
    let done = id_of(&prepare(&client, &["web/node_modules"]).await);
    human_approves(&mut store, &done);
    ok(
        "execute",
        &call(
            &client,
            "execute_cleanup",
            json!({ "plan_id": done.as_str() }),
        )
        .await,
    );
    let e = approval::revoke(
        &mut store,
        &done,
        HumanChannel::Terminal,
        &who(),
        now_unix(),
    )
    .unwrap_err();
    assert_eq!(e.reason, Reason::ApprovalUsed);
    assert!(!store.approval_revoked(&done).unwrap());
    client.cancel().await.unwrap();
    env.invariants(&[]);
}

/// #298: `approval::revoke` racing the Executor's atomic consumption (what
/// `Executor::begin_operation` does first), each on its own connection: exactly one wins in
/// every round; the loser gets `approval_used` (revocation lost) or nothing consumed.
#[tokio::test(flavor = "multi_thread")]
async fn revoke_racing_consumption_has_exactly_one_winner() {
    let env = Env::new();
    let client = connect(env.server(true, true)).await;
    let data: PathBuf = env.data.path().to_path_buf();
    let mut revoked_wins = 0;
    for _ in 0..12 {
        let id = id_of(&prepare(&client, &["app/target"]).await);
        human_approves(&mut env.store(), &id);
        let plan = env.store().get_plan(&id).unwrap().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let consumer = {
            let (data, plan, barrier) = (data.clone(), plan.clone(), barrier.clone());
            std::thread::spawn(move || {
                let mut s = scull_engine::open_history(&data).unwrap();
                barrier.wait();
                s.consume_approval(plan.id(), plan.content_hash(), now_unix())
                    .unwrap()
            })
        };
        let revoker = {
            let (data, id) = (data.clone(), id.clone());
            std::thread::spawn(move || {
                let mut s = scull_engine::open_history(&data).unwrap();
                barrier.wait();
                approval::revoke(&mut s, &id, HumanChannel::Terminal, &who(), now_unix())
            })
        };
        let consumed = consumer.join().unwrap();
        let revoked = revoker.join().unwrap();
        assert!(
            consumed ^ revoked.is_ok(),
            "consumed={consumed} revoked={revoked:?}"
        );
        if let Err(e) = &revoked {
            assert_eq!(e.reason, Reason::ApprovalUsed, "{e}");
        } else {
            revoked_wins += 1;
        }
        let state = approval::status(&env.store(), &id, &who(), now_unix())
            .unwrap()
            .state;
        let want = if consumed {
            ApprovalState::Used
        } else {
            ApprovalState::Revoked
        };
        assert_eq!(state, want);
    }
    let revocations = env
        .store()
        .agent_events(0)
        .unwrap()
        .iter()
        .filter(|e| e.kind.as_str() == "approval_revoked")
        .count();
    assert_eq!(
        revocations, revoked_wins,
        "one audit per successful revocation"
    );
    client.cancel().await.unwrap();
    env.invariants(&[]);
}
