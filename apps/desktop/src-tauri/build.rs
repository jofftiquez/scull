//! Tauri build script: embeds tauri.conf.json, the capabilities and the icons.
//!
//! The app manifest puts every app command under the ACL, so a window can call only what its
//! capability grants (`allow-<command>`): `capabilities/default.json` grants them to the main
//! window, the app's only window. `bridge/tests/commands.rs` checks these lists against
//! `src/lib.rs`.

/// Every app command registered in `src/lib.rs` (`handler`).
const COMMANDS: &[&str] = &[
    "settings",
    "scan",
    "caches",
    "cancel_operation",
    // Cleaning through the core Executor (P3-EXEC-01).
    "prepare_desktop_plan",
    "execute_desktop_plan",
    "execution_busy",
];

fn main() {
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));
    if let Err(e) = tauri_build::try_build(attributes) {
        panic!("tauri build failed: {e:#}");
    }
}
