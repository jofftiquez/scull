//! The Tauri commands (apps/desktop/src-tauri/src/lib.rs) and the frontend's command table
//! (apps/desktop/src/bridge/types.ts) name the same commands, and every command is registered
//! in the invoke handler. The menu bar widget's window commands (src-tauri/src/tray.rs) match
//! `WINDOW_COMMANDS` in src/bridge/tauri.ts, and tray monitoring's (src-tauri/src/monitor.rs)
//! match `COMMANDS.monitorStatus` plus `MONITOR_COMMANDS`. Reads sources only (the app crate
//! needs WebKit to build on Linux).
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

/// Names inside `generate_handler![...]`.
fn registered(src: &str) -> BTreeSet<String> {
    let start =
        src.find("generate_handler![").expect("invoke handler") + "generate_handler![".len();
    let end = start + src[start..].find(']').unwrap();
    src[start..end]
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
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
    let ts = ts_commands(&read("src/bridge/types.ts"));
    assert!(rust.len() >= 9, "{rust:?}");
    // Tray monitoring: its read command is on ScullBridge, the rest in MONITOR_COMMANDS.
    let monitor = rust_commands(&read("src-tauri/src/monitor.rs"));
    let mut reads = rust.clone();
    reads.insert("monitor_status".to_owned());
    assert_eq!(reads, ts, "Rust commands vs src/bridge/types.ts COMMANDS");
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
    let mut all = rust.clone();
    all.extend(window.iter().map(|c| format!("tray::{c}")));
    all.extend(monitor.iter().map(|c| format!("monitor::{c}")));
    assert_eq!(
        all,
        registered(&lib),
        "every command is registered, nothing else"
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
    // SAFE-10: the command surface is read-only by name too.
    let mut names = rust_commands(&read("src-tauri/src/lib.rs"));
    names.extend(rust_commands(&read("src-tauri/src/tray.rs")));
    names.extend(rust_commands(&read("src-tauri/src/monitor.rs")));
    assert!(names.contains("open_main_window"), "{names:?}");
    for name in names {
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
