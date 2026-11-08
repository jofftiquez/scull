//! The approval commands through Tauri's IPC (mock runtime): list, open, approve with the code
//! (a wrong one is refused), report, deny and revoke an AI agent's plan, with the arguments the
//! frontend sends (`apps/desktop/src/bridge/tauriPlans.ts`). Everything lives in a temp data
//! directory; nothing is executed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{
    Action, Candidate, Category, FileIdentity, Flags, FsPath, ItemId, Plan, PlanId, PlanItem,
    PlanOrigin, PlanParams, ReasonCode, Regeneration, RuleRef, ScanId, Sizes, Tier, TierReason,
};
use scull_desktop::{DeepLinks, approve_link_plan, main_window_handler};
use scull_desktop_bridge::{ApprovalDesk, Session, Setup, now_unix};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{INVOKE_KEY, MockRuntime, mock_builder};
use tauri::webview::InvokeRequest;
use tauri::{Listener, Manager, WebviewWindow, WebviewWindowBuilder};

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
            operations_watermark: None,
        },
        vec![PlanItem::from_candidate(&candidate(1, "/tmp/never/touched/target")).unwrap()],
    )
    .unwrap();
    let mut store = scull_engine::open_history(dir).unwrap();
    store.insert_plan(&plan).unwrap();
    plan
}

/// The approval commands on Tauri's mock runtime with the app's real configuration and
/// capabilities (`generate_context!`: the app manifest of build.rs, capabilities/*.json), and a
/// webview labelled `label`.
fn app_as(dir: &Path, label: &str) -> (tauri::App<MockRuntime>, WebviewWindow<MockRuntime>) {
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
        .manage(Arc::new(scull_desktop_bridge::CleanupDesk::new()))
        .invoke_handler(main_window_handler())
        .build(tauri::generate_context!(test = true))
        .unwrap();
    let webview = match app.get_webview_window(label) {
        Some(w) => w,
        None => WebviewWindowBuilder::new(&app, label, Default::default())
            .build()
            .unwrap(),
    };
    (app, webview)
}

/// The main window.
fn app(dir: &Path) -> (tauri::App<MockRuntime>, WebviewWindow<MockRuntime>) {
    app_as(dir, "main")
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
fn worktree_confirmation_count_survives_the_frontend_invoke_payload() {
    if scull_platform::is_elevated().blocks_destructive() {
        assert!(std::env::var_os("SCULL_TEST_REQUIRE_NOT_ELEVATED").is_none());
        eprintln!("SKIPPED: worktree execution requires a non-elevated process");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let worktree = root.join("linked");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--initial-branch=main"]);
    std::fs::write(repo.join("README.md"), "fixture\n").unwrap();
    git(&["add", "README.md"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-m",
        "initial",
    ]);
    git(&["worktree", "add", "-b", "feature", "../linked", "main"]);

    let data = root.join("data");
    let machine_id = scull_platform::machine_id(&data).unwrap();
    let session = Arc::new(Session::new(Setup {
        home: Some(root.join("home")),
        config_file: None,
        data_dir: Some(data.clone()),
        env: Arc::new(|_| None),
        record_history: true,
        git: true,
        detect_synced_roots: false,
        allow_network_roots: true,
        in_use: true,
    }));
    session
        .scan(
            &scull_desktop_bridge::OpId::parse("ipc-worktree").unwrap(),
            &scull_desktop_bridge::views::ScanParams {
                roots: vec![root.to_string_lossy().into_owned()],
                allow_broad_roots: true,
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
    let scan = session.last_scan().unwrap();
    let candidate = scan
        .candidates
        .iter()
        .find(|c| c.path.to_path_buf() == worktree)
        .unwrap();
    assert_eq!(
        candidate.action,
        Action::GitWorktreeRemove,
        "{candidate:#?}"
    );
    let app = mock_builder()
        .manage(session)
        .manage(Arc::new(scull_desktop_bridge::CleanupDesk::with(
            scull_desktop_bridge::cleanup::Who {
                machine_id,
                user: scull_platform::current_user().unwrap(),
            },
            scull_platform::trash::TrashLocations {
                home: Some(root.join("Trash")),
                topdir: Some(root.clone()),
            },
            root.join("execution.lock"),
        )))
        .invoke_handler(main_window_handler())
        .build(tauri::generate_context!(test = true))
        .unwrap();
    let w = WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();
    let plan = invoke(
        &w,
        "prepare_desktop_plan",
        json!({"items": [candidate.id.as_str()]}),
    )
    .unwrap();
    let mut args = json!({
        "planId": plan["plan_id"],
        "contentHash": plan["content_hash"],
        "items": [candidate.id.as_str()],
    });
    for count in [None, Some(0), Some(2)] {
        args.as_object_mut().unwrap().remove("typedCount");
        if let Some(count) = count {
            args["typedCount"] = json!(count);
        }
        let error = invoke(&w, "execute_desktop_plan", args.clone()).unwrap_err();
        assert_eq!(error["kind"], "safety_blocked", "{error}");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("type 1 to confirm"),
            "{error}"
        );
        assert!(worktree.join("README.md").is_file());
    }
    args["typedCount"] = json!(1);
    let result = invoke(&w, "execute_desktop_plan", args).unwrap();
    assert_eq!(result["items"][0]["outcome"], "deleted", "{result}");
    assert!(!worktree.exists());
    git(&["show-ref", "--verify", "refs/heads/feature"]);
}

#[test]
fn execution_busy_tracks_the_live_lock_without_releasing_another_holder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("execution.lock");
    assert!(!scull_desktop::execution_busy_at(&path).unwrap());
    let held = scull_platform::exec_lock::try_lock_exclusive(&path)
        .unwrap()
        .unwrap();
    assert!(scull_desktop::execution_busy_at(&path).unwrap());
    assert!(
        scull_platform::exec_lock::try_lock_exclusive(&path)
            .unwrap()
            .is_none()
    );
    drop(held);
    assert!(!scull_desktop::execution_busy_at(&path).unwrap());
    assert!(scull_desktop::execution_busy_at(dir.path()).is_err());
}

#[test]
fn execution_busy_is_denied_to_the_menu_bar_and_remote_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (_app, widget) = app_as(dir.path(), "menubar");
    let error = invoke(&widget, "execution_busy", json!({})).unwrap_err();
    assert!(error.to_string().contains("not allowed"), "{error}");
    let (_app, main) = app(dir.path());
    let error = invoke_from(&main, "https://example.com", "execution_busy", json!({})).unwrap_err();
    assert!(error.to_string().contains("not allowed"), "{error}");
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
            (
                "approve_agent_plan",
                json!({"planId": id, "contentHash": plan.content_hash().as_str(), "code": "ABCDEF"}),
            ),
            ("deny_agent_plan", json!({"planId": id})),
            ("revoke_agent_approval", json!({"planId": id})),
            ("agent_plan_status", json!({"planId": id})),
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
fn the_menu_bar_widget_cannot_reach_the_approval_commands() {
    let dir = tempfile::tempdir().unwrap();
    let plan = data_with_agent_plan(dir.path());
    let id = plan.id().as_str();
    let (_app, widget) = app_as(dir.path(), "menubar");
    for (cmd, args) in [
        ("approval_requests", json!({})),
        ("open_approval_request", json!({"planId": id})),
        (
            "approve_agent_plan",
            json!({"planId": id, "contentHash": plan.content_hash().as_str(), "code": "ABCDEF"}),
        ),
        ("deny_agent_plan", json!({"planId": id})),
        ("revoke_agent_approval", json!({"planId": id})),
        ("agent_plan_status", json!({"planId": id})),
        ("take_deep_link", json!({})),
    ] {
        let e = invoke(&widget, cmd, args).unwrap_err();
        assert!(e.to_string().contains("not allowed"), "menubar {cmd}: {e}");
    }
    // The same commands work from the main window of the same app.
    let main = widget.app_handle().get_webview_window("main");
    let main = match main {
        Some(w) => w,
        None => WebviewWindowBuilder::new(widget.app_handle(), "main", Default::default())
            .build()
            .unwrap(),
    };
    let list = invoke(&main, "approval_requests", json!({})).unwrap();
    assert_eq!(list["requests"][0]["plan_id"], id);
}

/// P3-EXEC-01: the cleanup commands are the main window's only (the menu bar widget and any
/// remote page are refused before the command runs), and from the main window they reach the
/// core: with no scan yet there is nothing to plan, and nothing is changed.
#[test]
fn cleanup_commands_are_the_main_windows_only() {
    let dir = tempfile::tempdir().unwrap();
    let cmds = [
        ("prepare_desktop_plan", json!({"items": null})),
        (
            "execute_desktop_plan",
            json!({"planId": "plan_01JX", "contentHash": "0".repeat(64), "items": ["a"], "typedCount": null}),
        ),
        (
            "restore_quarantined",
            json!({"operationId": "op_01JX", "itemId": "a"}),
        ),
        ("quick_clean", json!({"items": []})),
        ("quick_clean_plan", json!({})),
        ("quick_clean_run", json!({"choices": []})),
    ];
    let (_app, widget) = app_as(dir.path(), "menubar");
    for (cmd, args) in &cmds {
        let e = invoke(&widget, cmd, args.clone()).unwrap_err();
        assert!(e.to_string().contains("not allowed"), "menubar {cmd}: {e}");
    }
    let (_app2, main) = app(dir.path());
    for (cmd, args) in &cmds {
        let e = invoke_from(&main, "https://example.com", cmd, args.clone()).unwrap_err();
        assert!(e.to_string().contains("not allowed"), "remote {cmd}: {e}");
    }
    let e = invoke(&main, "prepare_desktop_plan", json!({"items": null})).unwrap_err();
    assert_eq!(e["kind"], "usage", "no scan yet: {e}");
    let e = invoke(&main, "execute_desktop_plan", cmds[1].1.clone()).unwrap_err();
    assert!(!e.to_string().contains("not allowed"), "{e}");
}

#[test]
fn cache_progress_reaches_only_the_requesting_window() {
    let dir = tempfile::tempdir().unwrap();
    let (app, main) = app(dir.path());
    let widget = WebviewWindowBuilder::new(&app, "menubar", Default::default())
        .build()
        .unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let capture = received.clone();
    main.listen(
        scull_desktop_bridge::DesktopCacheProgress::NAME,
        move |event| {
            capture
                .lock()
                .unwrap()
                .push(serde_json::from_str::<Value>(event.payload()).unwrap());
        },
    );
    let leaked = Arc::new(Mutex::new(Vec::new()));
    let capture = leaked.clone();
    widget.listen(
        scull_desktop_bridge::DesktopCacheProgress::NAME,
        move |event| {
            capture.lock().unwrap().push(event.payload().to_owned());
        },
    );
    let capture = leaked.clone();
    app.listen(
        scull_desktop_bridge::DesktopCacheProgress::NAME,
        move |event| {
            capture.lock().unwrap().push(event.payload().to_owned());
        },
    );
    // With no configured home, preparation fails before any native tool can run.
    let error = invoke(
        &main,
        "quick_clean",
        json!({"items": ["selected"], "opId": "cleanup-stream"}),
    )
    .unwrap_err();
    assert!(!error.to_string().contains("not allowed"), "{error}");
    let received = received.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["kind"], "preparing");
    assert_eq!(received[0]["op_id"], "cleanup-stream");
    assert_eq!(received[0]["total"], 1);
    assert_eq!(received[0]["completed"], 0);
    assert!(leaked.lock().unwrap().is_empty());
}

#[test]
fn cache_commands_validate_optional_progress_operation_ids() {
    let dir = tempfile::tempdir().unwrap();
    let (_app, main) = app(dir.path());
    for command in ["quick_clean", "quick_clean_run"] {
        let args = if command == "quick_clean" {
            json!({"items": []})
        } else {
            json!({"choices": []})
        };
        // Old callers may omit opId and still reach the bridge validation.
        let old = invoke(&main, command, args.clone()).unwrap_err();
        assert!(
            !old.to_string().contains("missing required key"),
            "{command}: {old}"
        );
        for op_id in ["", "bad/id", "with space", &"a".repeat(65)] {
            let mut invalid = args.clone();
            invalid["opId"] = json!(op_id);
            let error = invoke(&main, command, invalid).unwrap_err();
            assert_eq!(error["kind"], "usage", "{command}: {error}");
            assert!(
                error["message"].as_str().unwrap().contains("operation IDs"),
                "{error}"
            );
        }
    }
}

#[test]
fn web_view_debugging_variables_are_cleared_first() {
    // Every variable that can turn on remote debugging, an inspector server or extra browser
    // arguments (P4-MCP-02 review) is on the list...
    for name in [
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
        "WEBVIEW2_USER_DATA_FOLDER",
        "WEBVIEW2_RELEASE_CHANNEL_PREFERENCE",
        "WEBVIEW2_PIPE_FOR_SCRIPT_DEBUGGER",
        "WEBVIEW2_WAIT_FOR_SCRIPT_DEBUGGER",
        "WEBKIT_INSPECTOR_SERVER",
        "WEBKIT_INSPECTOR_HTTP_SERVER",
    ] {
        assert!(scull_desktop::BROWSER_ENV.contains(&name), "{name}");
    }
    // ... and `run` clears them before doing anything else (no web view, no thread yet).
    let src =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs")).unwrap();
    let body = &src[src.find("pub fn run() {").unwrap()..];
    let first = body
        .lines()
        .skip(1)
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("//"))
        .unwrap();
    assert_eq!(first, "let cleared = sanitize_browser_env();");
    // The windows' browser arguments are explicit.
    let conf =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json"))
            .unwrap();
    assert!(conf.contains(&format!(
        "\"additionalBrowserArgs\": \"{}\"",
        scull_desktop::BROWSER_ARGS
    )));
    let tray =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tray.rs")).unwrap();
    assert!(tray.contains(".additional_browser_args(crate::BROWSER_ARGS)"));
}

#[test]
fn a_plan_brings_the_window_forward_once() {
    let links = DeepLinks::default();
    assert!(links.should_focus("plan_01JA"));
    assert!(
        !links.should_focus("plan_01JA"),
        "repeated links don't steal focus again"
    );
    assert!(links.should_focus("plan_01JB"));
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
