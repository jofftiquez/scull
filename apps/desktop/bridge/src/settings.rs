//! `settings`: what the Settings screen shows (design 10), read-only.
//!
//! * The effective configuration as written (`~` kept) and the origin of every value (`default`,
//!   the user config file, an environment variable), exactly what `scull_core::config::resolve`
//!   produces for the CLI. An unreadable or invalid config file or variable is reported, not
//!   hidden: the view then shows the built-in defaults with the error.
//! * The built-in protected folders for this OS (`scull_safety::default_protected`).
//! * scull's own data, like `scull data show`: its directories, the database state (header probe
//!   only, see [`crate::db`]) and its own files with their sizes.
//!
//! Nothing here writes: changing a setting is a mutation, so the Settings screen calls a
//! screen-local stub (`apps/desktop/src/components/settings/bridge.ts`) until config writes go
//! through a core command.

use crate::db::{DbState, probe};
use crate::session::Session;
use schemars::JsonSchema;
use scull_core::config::{Config, FIELDS, Origin};
use scull_platform::{Native, Platform};
use scull_safety::{Scope, TargetOs, default_protected};
use scull_storage::SCHEMA_VERSION;
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

/// State of the user config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFileState {
    /// Read and valid.
    Present,
    /// Not there: every value is a default or from the environment.
    Missing,
    /// The file or a `SCULL_*` variable can't be used; `error` says why. Defaults are shown.
    Invalid,
    /// No config location is known on this machine.
    Unknown,
}

/// What kind of file one of scull's own files is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OwnFileKind {
    /// The database.
    Database,
    /// Its write-ahead log.
    Wal,
    /// Its shared-memory index.
    Shm,
    /// A backup taken before a migration.
    Backup,
    /// A damaged copy set aside.
    Damaged,
}

/// One of scull's own files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnFile {
    /// Where it is.
    pub path: String,
    /// What it is.
    pub kind: OwnFileKind,
    /// Logical size in bytes.
    pub bytes: u64,
}

/// The database, from its header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DatabaseInfo {
    /// Database file, when a data directory is known.
    pub path: Option<String>,
    /// How it looks.
    pub state: DbState,
    /// Its schema version, when known.
    pub schema_version: Option<u32>,
    /// The schema version this build uses.
    pub supported_schema_version: u32,
    /// Why it isn't usable, when it isn't.
    pub detail: Option<String>,
}

/// scull's own data (`scull data show`, without opening the database).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataInfo {
    /// Configuration directory (`config.toml`). Never purged.
    pub config_dir: Option<String>,
    /// Data directory (database, backups).
    pub data_dir: Option<String>,
    /// The database.
    pub database: DatabaseInfo,
    /// scull's own files that exist.
    pub files: Vec<OwnFile>,
    /// Their total size in bytes.
    pub footprint_bytes: u64,
}

/// `settings`: the Settings screen's data.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SettingsView {
    /// The user config file, when a location is known.
    pub config_file: Option<String>,
    /// Its state.
    pub config_state: ConfigFileState,
    /// Why it can't be used, when it can't (the CLI's message).
    pub config_error: Option<String>,
    /// The effective configuration, paths as written (`~` not expanded).
    pub config: Config,
    /// For every config key (`scan.roots`, `mcp.redact_home`, ...): where the value came from,
    /// most important first.
    pub origins: BTreeMap<String, Vec<Origin>>,
    /// Folders protected on this OS whatever the config says (credentials, system folders
    /// under home), with their contents.
    pub builtin_protected: Vec<String>,
    /// scull's own data.
    pub data: DataInfo,
}

fn path_string(p: &Path) -> String {
    p.display().to_string()
}

fn file_kind(db: &Path, p: &Path) -> OwnFileKind {
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
    let base = db.file_name().map(|n| n.to_string_lossy().into_owned());
    match (name, base) {
        (Some(n), Some(b)) if n == b => OwnFileKind::Database,
        (Some(n), _) if n.ends_with("-wal") => OwnFileKind::Wal,
        (Some(n), _) if n.ends_with("-shm") => OwnFileKind::Shm,
        (Some(n), _) if n.contains("damaged") || n.contains("corrupt") => OwnFileKind::Damaged,
        _ => OwnFileKind::Backup,
    }
}

/// scull's own data in `data_dir` (read-only).
pub fn data_info(config_dir: Option<&Path>, data_dir: Option<&Path>) -> DataInfo {
    let db = data_dir.map(|d| d.join(scull_storage::DB_FILE_NAME));
    let probe = match &db {
        Some(p) => probe(p),
        None => crate::db::DbProbe {
            state: DbState::Missing,
            schema_version: None,
            detail: Some("scull's data directory is not known".into()),
        },
    };
    let files: Vec<OwnFile> = match &db {
        Some(db) => scull_storage::footprint_files(db)
            .into_iter()
            .map(|p| OwnFile {
                kind: file_kind(db, &p),
                bytes: Native.metadata(&p).map(|m| m.logical_size).unwrap_or(0),
                path: path_string(&p),
            })
            .collect(),
        None => Vec::new(),
    };
    DataInfo {
        config_dir: config_dir.map(path_string),
        data_dir: data_dir.map(path_string),
        database: DatabaseInfo {
            path: db.as_deref().map(path_string),
            state: probe.state,
            schema_version: probe.schema_version,
            supported_schema_version: SCHEMA_VERSION,
            detail: probe.detail,
        },
        footprint_bytes: files.iter().map(|f| f.bytes).sum(),
        files,
    }
}

/// The built-in protected subtrees for this OS (credentials, `~/Library` on macOS, ...).
pub fn builtin_protected(home: Option<&Path>, env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let os_env = |k: &str| env(k).map(OsString::from);
    default_protected(TargetOs::current(), home, &os_env)
        .protected
        .into_iter()
        .filter(|p| p.scope == Scope::Subtree)
        .map(|p| path_string(&p.path))
        .collect()
}

/// Assemble the view for a session (see the module docs).
pub fn settings_view(session: &Session) -> SettingsView {
    let setup = session.setup();
    let (config, origins, state, error) = match session.resolved_config() {
        Ok(r) => {
            let present = setup.config_file.as_deref().is_some_and(|p| {
                Native
                    .metadata(p)
                    .map(|m| m.kind == scull_platform::EntryKind::File)
                    .unwrap_or(false)
            });
            let state = match (&setup.config_file, present) {
                (None, _) => ConfigFileState::Unknown,
                (Some(_), true) => ConfigFileState::Present,
                (Some(_), false) => ConfigFileState::Missing,
            };
            (r.config, r.origins, state, None)
        }
        Err(e) => {
            let defaults = FIELDS
                .iter()
                .map(|f| ((*f).to_owned(), vec![Origin::Default]))
                .collect();
            (
                Config::default(),
                defaults,
                ConfigFileState::Invalid,
                Some(e.to_string()),
            )
        }
    };
    let env = std::sync::Arc::clone(&setup.env);
    SettingsView {
        config_file: setup.config_file.as_deref().map(path_string),
        config_state: state,
        config_error: error,
        config,
        origins,
        builtin_protected: builtin_protected(setup.home.as_deref(), &move |k| env(k)),
        data: data_info(
            setup.config_file.as_deref().and_then(Path::parent),
            setup.data_dir.as_deref(),
        ),
    }
}
