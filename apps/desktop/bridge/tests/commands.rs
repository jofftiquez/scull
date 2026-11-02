//! The Tauri commands (apps/desktop/src-tauri/src/lib.rs) and the frontend's command table
//! (apps/desktop/src/bridge/types.ts) name the same commands, and every command is registered
//! in the invoke handler. The menu bar widget's window commands (src-tauri/src/tray.rs) match
//! `WINDOW_COMMANDS` in src/bridge/tauri.ts, and tray monitoring's (src-tauri/src/monitor.rs)
//! match `COMMANDS.monitorStatus` plus `MONITOR_COMMANDS`. The approval commands (P4-MCP-02)
//! match `APPROVAL_COMMANDS` and the cleanup commands `CLEANUP_COMMANDS` in src/bridge/tauriPlans.ts,
//! and `main_window_handler` registers exactly them. Reads sources only (the app crate needs WebKit to build on Linux).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel)).unwrap()
}

/// `fn <name>` following each `#[tauri::command]`.
fn rust_commands(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut armed = false;
    for line in src.lines() {
        let l = line.trim();
        if l == "#[tauri::command]" {
            armed = true;
            continue;
        }
        if armed && let Some(i) = l.find("fn ") {
            let name: String = l[i + 3..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            out.insert(name);
            armed = false;
        }
    }
    out
}

/// Names inside each `generate_handler![...]`, in source order.
fn handlers(src: &str) -> Vec<BTreeSet<String>> {
    let mut out = vec![];
    let mut rest = src;
    while let Some(i) = rest.find("generate_handler![") {
        let start = i + "generate_handler![".len();
        let end = start + rest[start..].find(']').unwrap();
        out.push(
            rest[start..end]
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
        );
        rest = &rest[end..];
    }
    out
}

/// The app's invoke handler (the one registering `app_info`).
fn registered(src: &str) -> BTreeSet<String> {
    handlers(src)
        .into_iter()
        .find(|h| h.contains("app_info"))
        .expect("the app's invoke handler")
}

/// String values of `COMMANDS = { key: 'value', ... }` and `CANCEL_COMMAND = '...'`.
fn ts_commands(src: &str) -> BTreeSet<String> {
    let start = src
        .find("export const COMMANDS = {")
        .expect("COMMANDS table");
    let end = start + src[start..].find("} as const").unwrap();
    let mut out: BTreeSet<String> = src[start..end]
        .lines()
        .filter_map(|l| l.split('\'').nth(1).map(str::to_owned))
        .collect();
    let cancel = src
        .lines()
        .find(|l| l.contains("export const CANCEL_COMMAND"))
        .and_then(|l| l.split('\'').nth(1))
        .expect("CANCEL_COMMAND");
    out.insert(cancel.to_owned());
    out
}

/// String values of `WINDOW_COMMANDS = { key: 'value', ... }` in src/bridge/tauri.ts.
fn ts_window_commands(src: &str) -> BTreeSet<String> {
    ts_table(src, "WINDOW_COMMANDS")
}

/// String values of `export const <name> = { key: 'value', ... }`.
fn ts_table(src: &str, name: &str) -> BTreeSet<String> {
    let start = src
        .find(&format!("export const {name} = {{"))
        .expect("command table");
    let end = start + src[start..].find("} as const").unwrap();
    src[start..end]
        .lines()
        .filter_map(|l| l.split('\'').nth(1).map(str::to_owned))
        .collect()
}

#[test]
fn frontend_and_app_name_the_same_commands() {
    let lib = read("src-tauri/src/lib.rs");
    let rust = rust_commands(&lib);
    let approval = ts_table(&read("src/bridge/tauriPlans.ts"), "APPROVAL_COMMANDS");
    let cleanup = ts_table(&read("src/bridge/tauriPlans.ts"), "CLEANUP_COMMANDS");
    let mut ts = ts_commands(&read("src/bridge/types.ts"));
    ts.extend(approval.iter().cloned());
    ts.extend(cleanup.iter().cloned());
    assert!(rust.len() >= 9, "{rust:?}");
    // Tray monitoring: its read command is on ScullBridge, the rest in MONITOR_COMMANDS.
    let monitor = rust_commands(&read("src-tauri/src/monitor.rs"));
    let mut reads = rust.clone();
    reads.insert("monitor_status".to_owned());
    assert_eq!(
        reads, ts,
        "Rust commands vs src/bridge/types.ts COMMANDS and tauriPlans.ts APPROVAL/CLEANUP_COMMANDS"
    );
    let mut controls = monitor.clone();
    assert!(controls.remove("monitor_status"), "{monitor:?}");
    assert_eq!(
        controls,
        ts_table(&read("src/bridge/tauri.ts"), "MONITOR_COMMANDS"),
        "monitor.rs commands vs src/bridge/tauri.ts MONITOR_COMMANDS"
    );

    let window = rust_commands(&read("src-tauri/src/tray.rs"));
    assert_eq!(
        window,
        ts_window_commands(&read("src/bridge/tauri.ts")),
        "tray.rs commands vs src/bridge/tauri.ts WINDOW_COMMANDS"
    );
    let onboarding = rust_commands(&read("src-tauri/src/onboarding.rs"));
    assert_eq!(
        onboarding,
        ts_table(&read("src/bridge/tauri.ts"), "ONBOARDING_COMMANDS")
    );
    let mut all = rust.clone();
    all.extend(onboarding.iter().map(|c| format!("onboarding::{c}")));
    all.extend(window.iter().map(|c| format!("tray::{c}")));
    all.extend(monitor.iter().map(|c| format!("monitor::{c}")));
    assert_eq!(
        all,
        registered(&lib),
        "every command is registered, nothing else"
    );
    // `main_window_handler` (mounted by the Tauri command tests) registers exactly the
    // main-window-only commands: approvals and cleanups.
    let main_window = handlers(&lib)
        .into_iter()
        .find(|h| h.contains("approve_agent_plan") && !h.contains("app_info"))
        .expect("main_window_handler");
    let mut expected = &approval | &cleanup;
    expected.extend(onboarding.iter().map(|c| format!("onboarding::{c}")));
    assert_eq!(main_window, expected);
}

/// The quoted names in build.rs's `COMMANDS` (the app manifest).
fn manifest_commands() -> BTreeSet<String> {
    let build = read("src-tauri/build.rs");
    let start = build.find("const COMMANDS").expect("COMMANDS in build.rs");
    let end = start + build[start..].find("];").unwrap();
    build[start..end]
        .lines()
        .filter_map(|l| l.split('"').nth(1).map(str::to_owned))
        .collect()
}

/// The `allow-<command>` permissions a capability file grants, as command names.
fn granted(file: &str) -> BTreeSet<String> {
    let v: serde_json::Value = serde_json::from_str(&read(file)).unwrap();
    v["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| {
            p.as_str()?
                .strip_prefix("allow-")
                .map(|c| c.replace('-', "_"))
        })
        .collect()
}

#[test]
fn approval_commands_are_granted_to_the_main_window_only() {
    let lib = read("src-tauri/src/lib.rs");
    let all: BTreeSet<String> = registered(&lib)
        .into_iter()
        .map(|c| c.rsplit("::").next().unwrap().to_owned())
        .collect();
    assert_eq!(
        manifest_commands(),
        all,
        "build.rs app manifest vs registered commands"
    );
    let mut main_only = ts_table(&read("src/bridge/tauriPlans.ts"), "APPROVAL_COMMANDS");
    main_only.extend(ts_table(
        &read("src/bridge/tauriPlans.ts"),
        "CLEANUP_COMMANDS",
    ));
    assert_eq!(
        granted("src-tauri/capabilities/default.json"),
        all,
        "main window: everything"
    );
    main_only.extend(ts_table(
        &read("src/bridge/tauri.ts"),
        "ONBOARDING_COMMANDS",
    ));
    let menubar = granted("src-tauri/capabilities/menubar.json");
    assert!(
        menubar.is_disjoint(&main_only),
        "the menu bar widget must not approve or clean: {menubar:?}"
    );
    assert_eq!(
        &menubar | &main_only,
        all,
        "menu bar widget: everything else"
    );
    let windows = |f: &str| {
        let v: serde_json::Value = serde_json::from_str(&read(f)).unwrap();
        v["windows"].clone()
    };
    assert_eq!(
        windows("src-tauri/capabilities/default.json"),
        serde_json::json!(["main"])
    );
    assert_eq!(
        windows("src-tauri/capabilities/menubar.json"),
        serde_json::json!(["menubar"])
    );
}

#[test]
fn open_request_event_name_matches() {
    let quoted = |src: &str, needle: &str| {
        let line = src.lines().find(|l| l.contains(needle)).unwrap().to_owned();
        line.split(['\'', '"']).nth(1).unwrap().to_owned()
    };
    assert_eq!(
        quoted(
            &read("src-tauri/src/lib.rs"),
            "pub const OPEN_REQUEST_EVENT"
        ),
        quoted(
            &read("src/bridge/tauriPlans.ts"),
            "export const OPEN_REQUEST_EVENT"
        )
    );
}

#[test]
fn navigate_event_name_matches() {
    let ts = read("src/bridge/tauri.ts");
    let line = ts
        .lines()
        .find(|l| l.contains("export const NAVIGATE_EVENT"))
        .unwrap();
    assert_eq!(
        line.split('\'').nth(1).unwrap(),
        scull_desktop_bridge::menubar::NAVIGATE_EVENT
    );
}

#[test]
fn monitor_event_name_matches() {
    let ts = read("src/bridge/tauri.ts");
    let line = ts
        .lines()
        .find(|l| l.contains("export const MONITOR_EVENT"))
        .unwrap();
    assert_eq!(
        line.split('\'').nth(1).unwrap(),
        scull_desktop_bridge::monitor::MONITOR_EVENT
    );
}

#[test]
fn notifications_are_sent_from_rust_only() {
    // The windows get no notification permission: only the tray's monitor thread notifies.
    let caps = read("src-tauri/capabilities/default.json");
    assert!(!caps.contains("notification"), "{caps}");
    let lib = read("src-tauri/src/lib.rs");
    assert!(lib.contains(".plugin(tauri_plugin_notification::init())"));
}

#[test]
fn progress_event_name_matches() {
    let ts = read("src/bridge/types.ts");
    let line = ts
        .lines()
        .find(|l| l.contains("export const PROGRESS_EVENT"))
        .unwrap();
    assert_eq!(
        line.split('\'').nth(1).unwrap(),
        scull_desktop_bridge::ProgressEvent::NAME
    );
}

#[test]
fn no_command_can_mutate() {
    // SAFE-10: the only commands that change files are the cleanup commands (P3-EXEC-01: through
    // the core Executor, main window only, checked above); every other command is read-only by
    // name too. (The approval commands record a human's decision in scull's database.)
    let cleanup = ts_table(&read("src/bridge/tauriPlans.ts"), "CLEANUP_COMMANDS");
    assert_eq!(cleanup.len(), 4, "{cleanup:?}");
    let mut names = rust_commands(&read("src-tauri/src/lib.rs"));
    names.extend(rust_commands(&read("src-tauri/src/tray.rs")));
    names.extend(rust_commands(&read("src-tauri/src/monitor.rs")));
    assert!(names.contains("open_main_window"), "{names:?}");
    for name in names.into_iter().filter(|n| !cleanup.contains(n)) {
        for bad in [
            "delete",
            "remove",
            "clean",
            "trash",
            "move",
            "rename",
            "write",
            "quarantine",
            "execute",
            "restore",
        ] {
            assert!(!name.contains(bad), "command {name} looks mutating");
        }
    }
}
