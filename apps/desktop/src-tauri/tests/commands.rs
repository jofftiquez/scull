//! The app's commands through Tauri's IPC (mock runtime, the app's real capabilities): only the
//! app's own page in the main window reaches them, the arguments the frontend sends
//! (`apps/desktop/src/bridge/tauri.ts`, `tauriPlans.ts`) reach the core, and the cleanup
//! confirmation survives the invoke payload. Everything lives in temp directories.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::Action;
use scull_desktop::handler;
use scull_desktop_bridge::{Session, Setup};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{INVOKE_KEY, MockRuntime, mock_builder};
use tauri::webview::InvokeRequest;
use tauri::{Manager, WebviewWindow, WebviewWindowBuilder};

/// Every command and arguments the frontend would send.
fn every_command() -> Vec<(&'static str, Value)> {
    vec![
        ("settings", json!({})),
        ("scan", json!({"opId": "op1", "params": {}})),
        ("caches", json!({"opId": "op2"})),
        ("cancel_operation", json!({"opId": "op1"})),
        ("prepare_desktop_plan", json!({"items": null})),
        (
            "execute_desktop_plan",
            json!({"planId": "plan_01JX", "contentHash": "0".repeat(64), "items": ["a"], "typedCount": null}),
        ),
        ("execution_busy", json!({})),
    ]
}

/// The app in a temp data directory, and its window `label` (created if it isn't `main`).
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
        .manage(Arc::new(Session::new(setup)))
        .manage(Arc::new(scull_desktop_bridge::CleanupDesk::new()))
        .invoke_handler(handler())
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

/// Every command is the main window's own: another window and any remote page are refused
/// before the command runs. From the main window the cleanup commands reach the core: with no
/// scan yet there is nothing to plan, and nothing is changed.
#[test]
fn commands_are_the_main_windows_only() {
    let dir = tempfile::tempdir().unwrap();
    let (_app, other) = app_as(dir.path(), "other");
    for (cmd, args) in every_command() {
        let e = invoke(&other, cmd, args).unwrap_err();
        assert!(
            e.to_string().contains("not allowed"),
            "other window {cmd}: {e}"
        );
    }
    let (_app2, main) = app(dir.path());
    for origin in ["https://example.com", "http://localhost:8080"] {
        for (cmd, args) in every_command() {
            let e = invoke_from(&main, origin, cmd, args).unwrap_err();
            assert!(e.to_string().contains("not allowed"), "{origin} {cmd}: {e}");
        }
    }
    let e = invoke(&main, "prepare_desktop_plan", json!({"items": null})).unwrap_err();
    assert_eq!(e["kind"], "usage", "no scan yet: {e}");
    let e = invoke(&main, "execute_desktop_plan", every_command()[5].1.clone()).unwrap_err();
    assert!(!e.to_string().contains("not allowed"), "{e}");
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
    let session = Arc::new(
        Session::new(Setup {
            home: Some(root.join("home")),
            config_file: None,
            data_dir: Some(data.clone()),
            env: Arc::new(|_| None),
            record_history: true,
            git: true,
            detect_synced_roots: false,
            allow_network_roots: true,
            in_use: false,
        })
        // Nothing holds this fresh tree; the real probe turns it S2 on a runner where some of
        // the user's processes can't be inspected (what this test checks is the confirmation).
        .with_in_use_source(scull_engine::InUseSource::Probe(Arc::new(
            scull_engine::FixedInUse(scull_safety::Tri::No),
        ))),
    );
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
        .invoke_handler(handler())
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
    // Execution re-checks activity with the real OS probe (no seam, by design). Where this
    // runner can't verify it, the right count must still be accepted and fail closed.
    let probe =
        scull_safety::NativeActivityProbe::capture(scull_platform::activity::Limits::default());
    let verifiable =
        scull_safety::check_in_use(&worktree, &[], &probe).in_use == scull_safety::Tri::No;
    args["typedCount"] = json!(1);
    let result = invoke(&w, "execute_desktop_plan", args).unwrap();
    if !verifiable {
        assert_eq!(result["items"][0]["outcome"], "skipped", "{result}");
        assert!(worktree.join("README.md").is_file());
        eprintln!("PARTIAL: native worktree activity could not be verified; removal skipped");
        return;
    }
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
}
