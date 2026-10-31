//! The `settings` command reports the effective configuration with origins (same resolution as
//! the CLI), the built-in protected folders and scull's own data, without writing anything, and
//! every answer validates against the committed `settings` schema.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::config::Origin;
use scull_desktop_bridge::db::DbState;
use scull_desktop_bridge::settings::{ConfigFileState, OwnFileKind};
use scull_desktop_bridge::{Session, Setup};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn validate(v: &Value) {
    let path = desktop().join("schemas/settings.schema.json");
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "settings: {errors:?}\n{v:#}");
}

fn session(root: &Path, env: &[(&str, &str)]) -> Session {
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Session::new(Setup {
        home: Some(root.join("home")),
        config_file: Some(root.join("config/config.toml")),
        data_dir: Some(root.join("data")),
        env: Arc::new(move |k| env.get(k).cloned()),
        record_history: true,
        git: false,
        detect_synced_roots: false,
        allow_network_roots: true,
        in_use: false,
    })
}

/// Every file under `dir`, recursively, with its bytes.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else {
                out.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn defaults_without_a_config_file_and_nothing_created() {
    let tmp = tempfile::tempdir().unwrap();
    let v = session(tmp.path(), &[]).settings();
    assert_eq!(v.config_state, ConfigFileState::Missing);
    assert!(v.config_error.is_none());
    // PRD §13: MCP off, destructive off, home redaction on.
    assert!(!v.config.mcp.enabled);
    assert!(!v.config.mcp.destructive);
    assert!(v.config.mcp.redact_home);
    assert_eq!(v.origins["mcp.redact_home"], vec![Origin::Default]);
    assert_eq!(v.data.database.state, DbState::Missing);
    assert!(v.data.files.is_empty());
    assert!(
        v.builtin_protected
            .iter()
            .any(|p| p.ends_with(".ssh") && p.contains("home")),
        "{:?}",
        v.builtin_protected
    );
    validate(&serde_json::to_value(&v).unwrap());
    assert!(
        std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
        "settings must not create anything"
    );
}

#[test]
fn origins_follow_the_cli_resolution_and_paths_stay_as_written() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("config")).unwrap();
    std::fs::write(
        tmp.path().join("config/config.toml"),
        "[scan]\nroots = [\"~/code\", \"~/work\"]\nprotect = [\"~/code/payments\"]\n\n[mcp]\nenabled = true\nhash_names = true\n",
    )
    .unwrap();
    let before = snapshot(tmp.path());
    let v = session(tmp.path(), &[("SCULL_MCP_HASH_NAMES", "false")]).settings();
    assert_eq!(
        v.config_state,
        ConfigFileState::Present,
        "{:?}",
        v.config_error
    );
    let roots: Vec<String> = v
        .config
        .scan
        .roots
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(roots, ["~/code", "~/work"]);
    assert_eq!(v.origins["scan.roots"], vec![Origin::UserConfig]);
    assert_eq!(v.origins["mcp.enabled"], vec![Origin::UserConfig]);
    assert_eq!(v.origins["scan.activity_window"], vec![Origin::Default]);
    if !v.config.mcp.hash_names {
        // The environment overrides the file when the variable is supported.
        assert!(matches!(v.origins["mcp.hash_names"][0], Origin::Env { .. }));
    }
    validate(&serde_json::to_value(&v).unwrap());
    assert_eq!(snapshot(tmp.path()), before, "the config file is unchanged");
}

#[test]
fn an_invalid_config_is_reported_with_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("config")).unwrap();
    std::fs::write(tmp.path().join("config/config.toml"), "[scan\nroots = 3").unwrap();
    let v = session(tmp.path(), &[]).settings();
    assert_eq!(v.config_state, ConfigFileState::Invalid);
    assert!(v.config_error.is_some());
    assert!(v.config.scan.roots.is_empty());
    assert_eq!(v.origins["scan.roots"], vec![Origin::Default]);
    validate(&serde_json::to_value(&v).unwrap());
}

#[test]
fn own_data_is_listed_without_opening_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    drop(
        scull_storage::Store::open(
            &data.join(scull_storage::DB_FILE_NAME),
            &scull_storage::DefaultPermissions,
        )
        .unwrap(),
    );
    let before = snapshot(&data);
    let v = session(tmp.path(), &[]).settings();
    assert_eq!(v.data.database.state, DbState::Ok);
    assert_eq!(
        v.data.database.schema_version,
        Some(scull_storage::SCHEMA_VERSION)
    );
    let db = v
        .data
        .files
        .iter()
        .find(|f| f.kind == OwnFileKind::Database)
        .expect("database listed");
    assert!(db.bytes > 0);
    assert_eq!(
        v.data.footprint_bytes,
        v.data.files.iter().map(|f| f.bytes).sum::<u64>()
    );
    validate(&serde_json::to_value(&v).unwrap());
    assert_eq!(snapshot(&data), before, "database files unchanged");
}
