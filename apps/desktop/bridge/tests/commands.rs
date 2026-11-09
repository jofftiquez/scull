//! The Tauri commands (apps/desktop/src-tauri/src/lib.rs) and the frontend's command tables
//! (`COMMANDS` and `CANCEL_COMMAND` in src/bridge/types.ts, `CLEANUP_COMMANDS` in
//! src/bridge/tauriPlans.ts) name the same commands; the one invoke handler registers exactly
//! them; build.rs's app manifest lists them; and `capabilities/default.json` grants them to the
//! main window only. Reads sources only (the app crate needs WebKit to build on Linux).
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

/// The app's invoke handler (`handler`, the only one).
fn registered(src: &str) -> BTreeSet<String> {
    let all = handlers(src);
    assert_eq!(
        all.len(),
        1,
        "one invoke handler, shared by `run` and the tests"
    );
    all.into_iter().next().unwrap()
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

/// Every command the frontend calls.
fn frontend_commands() -> BTreeSet<String> {
    let mut ts = ts_commands(&read("src/bridge/types.ts"));
    ts.extend(ts_table(
        &read("src/bridge/tauriPlans.ts"),
        "CLEANUP_COMMANDS",
    ));
    ts
}

#[test]
fn frontend_and_app_name_the_same_commands() {
    let lib = read("src-tauri/src/lib.rs");
    assert_eq!(
        rust_commands(&lib),
        frontend_commands(),
        "lib.rs vs the frontend's tables"
    );
    assert_eq!(
        registered(&lib),
        frontend_commands(),
        "every command is registered, nothing else"
    );
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
fn commands_are_granted_to_the_main_window_only() {
    let all = registered(&read("src-tauri/src/lib.rs"));
    assert_eq!(
        manifest_commands(),
        all,
        "build.rs app manifest vs registered commands"
    );
    assert_eq!(
        granted("src-tauri/capabilities/default.json"),
        all,
        "main window: everything"
    );
    let caps: Vec<_> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../src-tauri/capabilities"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
    assert_eq!(caps, ["default.json"], "one capability, one window");
    let v: serde_json::Value =
        serde_json::from_str(&read("src-tauri/capabilities/default.json")).unwrap();
    assert_eq!(v["windows"], serde_json::json!(["main"]));
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
    // name too.
    let cleanup = ts_table(&read("src/bridge/tauriPlans.ts"), "CLEANUP_COMMANDS");
    assert_eq!(cleanup.len(), 3, "{cleanup:?}");
    for name in rust_commands(&read("src-tauri/src/lib.rs"))
        .into_iter()
        .filter(|n| !cleanup.contains(n))
    {
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
