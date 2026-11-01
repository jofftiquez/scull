//! The approval commands through Tauri's IPC (mock runtime): list, open, approve with the code
//! (a wrong one is refused), report, deny and revoke an AI agent's plan, with the arguments the
//! frontend sends (`apps/desktop/src/bridge/tauriPlans.ts`). Everything lives in a temp data
//! directory; nothing is executed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{
    Action, Candidate, Category, FileIdentity, Flags, FsPath, ItemId, Plan, PlanId, PlanItem,
    PlanOrigin, PlanParams, ReasonCode, Regeneration, RuleRef, ScanId, Sizes, Tier, TierReason,
};
use scull_desktop::{DeepLinks, approval_handler, approve_link_plan};
use scull_desktop_bridge::{ApprovalDesk, Session, Setup, now_unix};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{INVOKE_KEY, MockRuntime, mock_builder, mock_context, noop_assets};
use tauri::webview::InvokeRequest;
use tauri::{WebviewWindow, WebviewWindowBuilder};

fn candidate(n: u128, path: &str) -> Candidate {
    let identity = FileIdentity::new(7, n);
    Candidate {
        id: ItemId::derive(identity, path.as_bytes()),
        path: FsPath::from(path),
        identity,
        category: Category::ProjectArtifact,
        artifact: "Cargo build output".into(),
        rule: RuleRef {
            id: "rust.target".into(),
            version: 1,
        },
        project: None,
        tier: Tier::S0,
        reasons: vec![TierReason::new(
            Tier::S0,
            ReasonCode::Regenerable,
            "rebuilds",
        )],
        evidence: vec![],
        sizes: Sizes {
            logical: 1000,
            allocated: 1024,
            reclaimable: scull_core::Reclaimable {
                bytes: 1000,
                confidence: scull_core::Confidence::High,
                shared_bytes: 0,
            },
        },
        flags: Flags::default(),
        regeneration: Regeneration::default(),
        action: Action::Delete,
    }
}

/// A data directory (temp) with this machine's ID and one agent plan for the current user.
fn data_with_agent_plan(dir: &Path) -> Plan {
    let machine_id = scull_platform::machine_id(dir).unwrap();
    let user = scull_platform::current_user().unwrap();
    let plan = Plan::new(
        PlanParams {
            id: PlanId::from_suffix("01JIPCTEST").unwrap(),
            scan_id: ScanId::from_suffix("S1").unwrap(),
            created_unix: now_unix() - 30,
            ttl_secs: None,
            machine_id,
            user,
            origin: PlanOrigin::Mcp,
        },
        vec![PlanItem::from_candidate(&candidate(1, "/tmp/never/touched/target")).unwrap()],
    )
    .unwrap();
    let mut store = scull_engine::open_history(dir).unwrap();
    store.insert_plan(&plan).unwrap();
    plan
}

fn app(dir: &Path) -> (tauri::App<MockRuntime>, WebviewWindow<MockRuntime>) {
    let setup = Setup {
        home: None,
        config_file: None,
        data_dir: Some(dir.to_path_buf()),
        env: Arc::new(|_| None),
        record_history: false,
        git: false,
        detect_synced_roots: false,
        allow_network_roots: false,
        in_use: false,
    };
    let app = mock_builder()
        .manage(Arc::new(ApprovalDesk::new(Some(dir.to_path_buf()))))
        .manage(Arc::new(Session::new(setup)))
        .manage(DeepLinks::default())
        .invoke_handler(approval_handler())
        .build(mock_context(noop_assets()))
        .unwrap();
    let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    (app, webview)
}

/// Invoke `cmd` with `args` like the webview does; `Ok(value)` or `Err(error JSON)`.
fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, args: Value) -> Result<Value, Value> {
    invoke_from(w, "tauri://localhost", cmd, args)
}

/// Invoke `cmd` from a page at `origin`.
fn invoke_from(
    w: &WebviewWindow<MockRuntime>,
    origin: &str,
    cmd: &str,
    args: Value,
) -> Result<Value, Value> {
    tauri::test::get_ipc_response(
        w,
        InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: origin.parse().unwrap(),
            body: InvokeBody::Json(args),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    )
    .map(|b| b.deserialize::<Value>().unwrap())
}

#[test]
fn approve_through_ipc_needs_the_code_shown_in_the_dialog() {
    let dir = tempfile::tempdir().unwrap();
    let plan = data_with_agent_plan(dir.path());
    let (_app, w) = app(dir.path());
    let id = plan.id().as_str();

    let list = invoke(&w, "approval_requests", json!({})).unwrap();
    assert_eq!(list["requests"][0]["plan_id"], id, "{list}");

    // Approving a plan that was never opened is refused.
    let hash = plan.content_hash().as_str();
    let e = invoke(
        &w,
        "approve_agent_plan",
        json!({"planId": id, "contentHash": hash, "code": "ABCDEF"}),
    )
    .unwrap_err();
    assert_eq!(e["kind"], "safety_blocked", "{e}");

    let req = invoke(&w, "open_approval_request", json!({"planId": id})).unwrap();
    let code = req["code"].as_str().unwrap().to_owned();
    assert_eq!(req["content_hash"], hash);
    assert_eq!(req["items"].as_array().unwrap().len(), 1);

    let wrong = if code == "AAA-AAA" {
        "BBB-BBB"
    } else {
        "AAA-AAA"
    };
    let e = invoke(
        &w,
        "approve_agent_plan",
        json!({"planId": id, "contentHash": hash, "code": wrong}),
    )
    .unwrap_err();
    assert!(
        e["message"].as_str().unwrap().contains("does not match"),
        "{e}"
    );
    assert!(
        !e.to_string().contains(&code),
        "the code is never in an error"
    );

    let ok = invoke(
        &w,
        "approve_agent_plan",
        json!({"planId": id, "contentHash": hash, "code": code}),
    )
    .unwrap();
    assert_eq!(ok["plan_id"], id);

    let s = invoke(&w, "agent_plan_status", json!({"planId": id})).unwrap();
    assert_eq!(s["state"], "approved", "{s}");
    assert_eq!(s["channel"], "desktop");
    assert!(!s.to_string().contains(&code));

    let r = invoke(&w, "revoke_agent_approval", json!({"planId": id})).unwrap();
    assert_eq!(r["plan_id"], id);
    let s = invoke(&w, "agent_plan_status", json!({"planId": id})).unwrap();
    assert_eq!(s["state"], "revoked");

    // The code was never written to the data directory.
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            let bytes = std::fs::read(&p).unwrap();
            for c in [code.clone(), code.replace('-', "")] {
                assert!(
                    !bytes.windows(c.len()).any(|w| w == c.as_bytes()),
                    "{} holds the code",
                    p.display()
                );
            }
        }
    }
}

#[test]
fn deny_through_ipc_closes_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let plan = data_with_agent_plan(dir.path());
    let (_app, w) = app(dir.path());
    let id = plan.id().as_str();
    invoke(&w, "open_approval_request", json!({"planId": id})).unwrap();
    let d = invoke(&w, "deny_agent_plan", json!({"planId": id})).unwrap();
    assert_eq!(d["plan_id"], id);
    let list = invoke(&w, "approval_requests", json!({})).unwrap();
    assert_eq!(list["requests"], json!([]));
    let s = invoke(&w, "agent_plan_status", json!({"planId": id})).unwrap();
    assert_eq!(s["state"], "denied");
    assert!(invoke(&w, "open_approval_request", json!({"planId": id})).is_err());
}

#[test]
fn without_scull_data_there_is_nothing_to_approve() {
    let dir = tempfile::tempdir().unwrap();
    let (_app, w) = app(dir.path());
    let list = invoke(&w, "approval_requests", json!({})).unwrap();
    assert_eq!(list["requests"], json!([]));
    assert!(
        invoke(
            &w,
            "open_approval_request",
            json!({"planId": "plan_01JNONE"})
        )
        .is_err()
    );
    assert_eq!(
        invoke(&w, "take_deep_link", json!({})).unwrap(),
        Value::Null
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "nothing created"
    );
}

#[test]
fn a_remote_page_cannot_reach_the_approval_commands() {
    let dir = tempfile::tempdir().unwrap();
    let plan = data_with_agent_plan(dir.path());
    let (_app, w) = app(dir.path());
    let id = plan.id().as_str();
    for origin in ["https://example.com", "http://localhost:8080"] {
        for (cmd, args) in [
            ("approval_requests", json!({})),
            ("open_approval_request", json!({"planId": id})),
            ("deny_agent_plan", json!({"planId": id})),
        ] {
            let e = invoke_from(&w, origin, cmd, args).unwrap_err();
            assert!(e.to_string().contains("not allowed"), "{origin} {cmd}: {e}");
        }
    }
    // Nothing was opened or recorded.
    let list = invoke(&w, "approval_requests", json!({})).unwrap();
    assert_eq!(list["requests"][0]["plan_id"], id);
}

#[test]
fn only_well_formed_approve_links_are_followed() {
    assert_eq!(
        approve_link_plan("scull://approve/plan_01JIPCTEST").as_deref(),
        Some("plan_01JIPCTEST")
    );
    assert_eq!(
        approve_link_plan("scull://approve/plan_01JIPCTEST/").as_deref(),
        Some("plan_01JIPCTEST")
    );
    for bad in [
        "scull://approve/",
        "scull://approve/../../etc",
        "scull://approve/plan_01J?code=ABCDEF",
        "scull://execute/plan_01JIPCTEST",
        "https://example.com/approve/plan_01JIPCTEST",
        "plan_01JIPCTEST",
    ] {
        assert_eq!(approve_link_plan(bad), None, "{bad}");
    }
}
