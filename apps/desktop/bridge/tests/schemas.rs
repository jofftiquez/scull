//! The committed bridge schemas (`apps/desktop/schemas/`) are what the Rust types produce, and
//! every type the desktop shares with the CLI has exactly the CLI's schema, so the app, the CLI
//! and the MCP server describe the same data the same way (one engine, PRD principle 6).
//!
//! Regenerate: `SCULL_UPDATE_SCHEMAS=1 cargo test -p scull-desktop-bridge --test schemas`, then
//! `pnpm --filter @scull/desktop types` for the TypeScript side.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;
use std::path::{Path, PathBuf};

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{p:?}: {e}")))
        .unwrap()
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap() + "\n"
}

#[test]
fn committed_schemas_are_up_to_date() {
    let dir = desktop().join("schemas");
    let update = std::env::var_os("SCULL_UPDATE_SCHEMAS").is_some();
    if update {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let all = scull_desktop_bridge::schemas::all();
    let mut stale = Vec::new();
    for (stem, schema) in &all {
        let path = dir.join(format!("{stem}.schema.json"));
        let text = pretty(schema);
        if update {
            std::fs::write(&path, &text).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
            stale.push(stem.to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "stale schemas {stale:?}: run SCULL_UPDATE_SCHEMAS=1 cargo test -p scull-desktop-bridge \
         --test schemas, then pnpm --filter @scull/desktop types"
    );
    // No leftovers from removed payloads.
    let committed = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(committed, all.len(), "unexpected files in {dir:?}");
}

fn cli_schema(name: &str) -> Value {
    read(
        &desktop()
            .join("../../crates/scull-cli/schemas")
            .join(format!("{name}.schema.json")),
    )
}

fn desktop_schema(stem: &str) -> Value {
    read(
        &desktop()
            .join("schemas")
            .join(format!("{stem}.schema.json")),
    )
}

/// Every definition the desktop schema shares by name with the CLI schema is identical.
fn assert_shared_defs_equal(desk: &Value, cli: &Value, label: &str) -> usize {
    let (Some(d), Some(c)) = (desk["$defs"].as_object(), cli["$defs"].as_object()) else {
        panic!("{label}: no $defs");
    };
    let mut shared = 0;
    for (name, def) in d {
        if let Some(cdef) = c.get(name) {
            assert_eq!(
                def, cdef,
                "{label}: definition {name} differs from the CLI's"
            );
            shared += 1;
        }
    }
    shared
}

/// The desktop's top-level payload equals a CLI definition (same fields, same types).
fn assert_root_equals_cli_def(desk: &Value, cli: &Value, def: &str, label: &str) {
    let cdef = &cli["$defs"][def];
    assert!(cdef.is_object(), "{label}: CLI has no {def}");
    for key in ["type", "properties", "required", "additionalProperties"] {
        assert_eq!(
            desk[key], cdef[key],
            "{label}: `{key}` differs from the CLI's {def}"
        );
    }
}

#[test]
fn scan_report_is_the_cli_scan_payload() {
    let desk = desktop_schema("scan-report");
    let cli = cli_schema("scan");
    assert_root_equals_cli_def(&desk, &cli, "ScanReport", "scan-report");
    let shared = assert_shared_defs_equal(&desk, &cli, "scan-report");
    assert!(shared > 20, "only {shared} shared definitions");
}

#[test]
fn caches_report_is_the_cli_caches_payload() {
    let desk = desktop_schema("caches-report");
    let mut cli = cli_schema("caches");
    // The CLI's `--json` data is the engine's report plus the CLI-only `commands` lines
    // (#286, additive and optional); everything else is the same payload.
    let doc = &mut cli["$defs"]["CachesDocument"];
    let extra = doc["properties"]
        .as_object_mut()
        .unwrap()
        .remove("commands");
    assert!(
        extra.is_some(),
        "the CLI's CachesDocument has no `commands`"
    );
    assert_root_equals_cli_def(&desk, &cli, "CachesDocument", "caches-report");
    assert_shared_defs_equal(&desk, &cli, "caches-report");
}

#[test]
fn projects_and_worktrees_match_the_cli_reports() {
    let desk = desktop_schema("projects");
    let cli = cli_schema("projects");
    assert_root_equals_cli_def(&desk, &cli, "ProjectsReport", "projects");
    assert_shared_defs_equal(&desk, &cli, "projects");

    let desk = desktop_schema("worktrees");
    let cli = cli_schema("worktrees");
    assert_root_equals_cli_def(&desk, &cli, "WorktreesReport", "worktrees");
    assert_shared_defs_equal(&desk, &cli, "worktrees");
}

#[test]
fn errors_are_the_shared_scull_error() {
    let desk = desktop_schema("error");
    let cli = cli_schema("error");
    assert_root_equals_cli_def(&desk, &cli, "ScullError", "error");
    assert_shared_defs_equal(&desk, &cli, "error");
}
