//! The menu bar widget's wiring (P4-UI-01): the capability covers both windows, the main
//! window's label is the one the tray shows, and the widget page is part of the frontend build.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_desktop::tray::MAIN_LABEL;
use scull_desktop_bridge::menubar::{WIDGET_LABEL, WIDGET_URL};
use std::path::Path;

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

#[test]
fn capability_covers_the_main_window_and_the_widget_only() {
    let v: serde_json::Value = serde_json::from_str(&read("capabilities/default.json")).unwrap();
    let windows: Vec<&str> = v["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    assert_eq!(windows, [MAIN_LABEL, WIDGET_LABEL]);
    assert_eq!(v["permissions"], serde_json::json!(["core:default"]));
}

#[test]
fn the_configured_main_window_is_the_one_the_tray_shows() {
    let v: serde_json::Value = serde_json::from_str(&read("tauri.conf.json")).unwrap();
    let windows = v["app"]["windows"].as_array().unwrap();
    assert_eq!(
        windows.len(),
        1,
        "the widget window is created on first use"
    );
    assert_eq!(windows[0]["label"], MAIN_LABEL);
}

#[test]
fn the_widget_page_is_built_with_the_app() {
    assert!(read(&format!("../{WIDGET_URL}")).contains("/src/menubar.ts"));
    assert!(read("../vite.config.ts").contains(&format!("'{WIDGET_URL}'")));
}
