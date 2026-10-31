//! The Tauri commands (apps/desktop/src-tauri/src/lib.rs) and the frontend's command table
//! (apps/desktop/src/bridge/types.ts) name the same commands, and every command is registered
//! in the invoke handler. Reads sources only (the app crate needs WebKit to build on Linux).
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

#[test]
fn frontend_and_app_name_the_same_commands() {
    let lib = read("src-tauri/src/lib.rs");
    let rust = rust_commands(&lib);
    let ts = ts_commands(&read("src/bridge/types.ts"));
    assert!(rust.len() >= 9, "{rust:?}");
    assert_eq!(rust, ts, "Rust commands vs src/bridge/types.ts COMMANDS");
    assert_eq!(
        rust,
        registered(&lib),
        "every command is registered, nothing else"
    );
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
    for name in rust_commands(&read("src-tauri/src/lib.rs")) {
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
