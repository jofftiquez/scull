//! Onboarding context and picker permissions through the real app capabilities.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_desktop::main_window_handler;
use scull_desktop_bridge::{Session, Setup};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{INVOKE_KEY, MockRuntime, mock_builder};
use tauri::webview::InvokeRequest;
use tauri::{Manager, WebviewWindow, WebviewWindowBuilder};

fn app(
    label: &str,
    home: Option<PathBuf>,
) -> (tauri::App<MockRuntime>, WebviewWindow<MockRuntime>) {
    let setup = Setup {
        home,
        config_file: None,
        data_dir: None,
        env: Arc::new(|_| None),
        record_history: false,
        git: false,
        detect_synced_roots: false,
        allow_network_roots: false,
        in_use: false,
    };
    let app = mock_builder()
        .manage(Arc::new(Session::new(setup)))
        .invoke_handler(main_window_handler())
        .build(tauri::generate_context!(test = true))
        .unwrap();
    let w = app.get_webview_window(label).unwrap_or_else(|| {
        WebviewWindowBuilder::new(&app, label, Default::default())
            .build()
            .unwrap()
    });
    (app, w)
}

fn invoke(w: &WebviewWindow<MockRuntime>, origin: &str, cmd: &str) -> Result<Value, Value> {
    tauri::test::get_ipc_response(
        w,
        InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: origin.parse().unwrap(),
            body: InvokeBody::Json(json!({})),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        },
    )
    .map(|b| b.deserialize::<Value>().unwrap())
}

#[test]
fn native_context_uses_session_home_without_needing_a_previous_scan() {
    let (_app, w) = app("main", Some(PathBuf::from("/Users/actual-home")));
    assert_eq!(
        invoke(&w, "tauri://localhost", "onboarding_context").unwrap(),
        json!({"home": "/Users/actual-home"})
    );
    let (_app, w) = app("main", None);
    assert_eq!(
        invoke(&w, "tauri://localhost", "onboarding_context").unwrap(),
        json!({"home": null})
    );
}

#[test]
fn onboarding_commands_refuse_widget_and_remote_pages_before_opening_a_dialog() {
    for label in ["main", "menubar"] {
        let (_app, w) = app(label, None);
        for cmd in ["onboarding_context", "pick_project_folders"] {
            if label == "menubar" {
                let error = invoke(&w, "tauri://localhost", cmd).unwrap_err();
                assert!(error.to_string().contains("not allowed"), "{cmd}: {error}");
            }
            let error = invoke(&w, "https://example.org", cmd).unwrap_err();
            assert!(error.to_string().contains("not allowed"), "{cmd}: {error}");
        }
    }
}
