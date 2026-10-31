//! `scull doctor` (P1-CLI-05; PLT-08, PLT-09, SAFE-14): what this machine and build can do.
//!
//! Reports the platform, every capability signal (available or not, and why: PRD §8.4 "if a
//! signal is unavailable, the preview says so"), elevation, scull's directories and whether
//! they are owner-only, the database (schema version, integrity), the `git` binary scull would
//! run, and the user config with the origin of every effective value.
//!
//! Doctor is **read-only**: it creates no directory or file, never creates or migrates the
//! database (see [`crate::db`]), and never changes permissions; problems are reported, not
//! fixed. It always exits 0 when it could produce a report, since a broken setup is exactly
//! what it is for. Machine output: `scull/doctor@1` (schema in
//! `crates/scull-cli/schemas/doctor.schema.json`).

use super::{emit_machine, emit_text};
use crate::db::{self, NotOpened, Probe};
use crate::output::json::SchemaId;
use crate::output::style::Style;
use crate::output::table::{Align, Cell, Table};
use crate::output::{Ui, bytes::format_bytes};
use schemars::JsonSchema;
use scull_core::config::{ConfigLayer, EnvLayer, FIELDS, Origin, resolve};
use scull_core::{FsPath, ScullError};
use scull_platform::{
    Capabilities, ErrorKind as PlatformErrorKind, Native, Platform, Privileges, app_dirs,
    ensure_owner_only,
};
use scull_storage::{SCHEMA_VERSION, StoreError};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Schema of `scull doctor --json`.
pub const DOCTOR_SCHEMA: SchemaId = SchemaId::new("doctor", 1);

/// File name of the user config inside the config directory (CFG-01).
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Everything `scull doctor` reports.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DoctorReport {
    /// This scull build.
    pub scull: BuildInfo,
    /// The operating system and CPU architecture.
    pub platform: PlatformInfo,
    /// Every safety and accuracy signal, available or not.
    pub signals: Vec<Signal>,
    /// Root/administrator status (SAFE-14).
    pub privileges: PrivilegeInfo,
    /// scull's per-user directories.
    pub dirs: DirsReport,
    /// The local database.
    pub database: DatabaseReport,
    /// The `git` binary scull runs for repository checks.
    pub git: GitReport,
    /// The user config file and the effective configuration.
    pub config: ConfigReport,
    /// Problems found, one sentence each (empty when everything looks fine).
    pub warnings: Vec<String>,
}

/// This scull build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildInfo {
    /// scull version.
    pub version: String,
    /// Target triple the binary was built for.
    pub target: String,
}

/// The operating system and CPU architecture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlatformInfo {
    /// `linux`, `macos`, `windows`, ...
    pub os: String,
    /// `x86_64`, `aarch64`, ...
    pub arch: String,
    /// `unix` or `windows`.
    pub family: String,
}

/// One capability signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    /// Stable identifier (snake_case).
    pub name: String,
    /// What the signal is.
    pub description: String,
    /// Whether this OS build provides it.
    pub available: bool,
    /// When unavailable: why, and what scull does instead.
    pub reason: Option<String>,
}

/// Elevation as reported by the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElevationState {
    /// Root (effective UID 0) or an elevated Windows token.
    Elevated,
    /// An ordinary user.
    NotElevated,
    /// The OS could not tell; treated as elevated.
    Unknown,
}

/// Root/administrator status (SAFE-14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrivilegeInfo {
    /// Whether the process is elevated.
    pub elevation: ElevationState,
    /// The invoking user when started through `sudo`/`doas` (a hint from the environment).
    pub sudo_user: Option<String>,
    /// Whether destructive operations would refuse to run without `--allow-root`.
    pub destructive_blocked: bool,
}

/// scull's per-user directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DirsReport {
    /// Whether `SCULL_HOME` relocates every directory.
    pub scull_home: Option<FsPath>,
    /// Why the directories could not be located, if they couldn't.
    pub error: Option<String>,
    /// One entry per distinct directory.
    pub entries: Vec<DirReport>,
}

/// One of scull's directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DirReport {
    /// Roles (`config`, `data`, `cache`, `state`); several when they share a directory.
    pub roles: Vec<String>,
    /// The directory.
    pub path: FsPath,
    /// Whether anything exists at the path.
    pub exists: bool,
    /// Whether it is accessible only by the current user; `null` if missing or unknown.
    pub owner_only: Option<bool>,
    /// Every way access is broader than owner-only.
    pub exposures: Vec<String>,
    /// Error reading its permissions, if any.
    pub error: Option<String>,
}

/// State of the database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseState {
    /// Opened with the current schema.
    Ok,
    /// Not created yet (created by the first scan).
    Missing,
    /// Exists but has no schema yet.
    Uninitialized,
    /// Older schema; the next scull command that uses the database migrates it (with a
    /// backup). Doctor does not migrate.
    NeedsMigration,
    /// Written by a newer scull.
    TooNew,
    /// Something other than a regular file is at the path.
    NotAFile,
    /// Not a SQLite database.
    NotSqlite,
    /// A SQLite database of another application.
    Foreign,
    /// Damaged.
    Corrupt,
    /// Could not be read or opened.
    Error,
    /// scull's directories could not be located.
    Unknown,
}

/// Result of the full integrity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Integrity {
    /// Every page reads back correctly.
    Ok,
    /// Damage found.
    Damaged,
    /// Not checked (the database was not opened).
    NotChecked,
}

/// The local database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DatabaseReport {
    /// Database file.
    pub path: Option<FsPath>,
    /// What was found.
    pub state: DatabaseState,
    /// Schema version found, if known.
    pub schema_version: Option<u32>,
    /// Newest schema version this scull supports.
    pub supported_schema_version: u32,
    /// Integrity check result.
    pub integrity: Integrity,
    /// scull's own disk use (database, WAL, backups), when opened.
    pub footprint_bytes: Option<u64>,
    /// Whether the file is accessible only by the current user; `null` if missing or unknown.
    pub owner_only: Option<bool>,
    /// Explanation for anything but `ok`.
    pub detail: Option<String>,
}

/// The `git` binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitReport {
    /// Whether a usable `git` was found.
    pub available: bool,
    /// Absolute path of the binary scull runs.
    pub binary: Option<FsPath>,
    /// Its version.
    pub version: Option<String>,
    /// Oldest supported version.
    pub min_version: String,
    /// Why it is unavailable.
    pub error: Option<String>,
}

/// State of the user config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFileState {
    /// Parsed successfully.
    Ok,
    /// No config file: built-in defaults apply.
    Missing,
    /// Not valid (other commands exit 2).
    Invalid,
    /// Could not be read.
    Unreadable,
    /// scull's directories could not be located.
    Unknown,
}

/// The user config and the effective configuration.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigReport {
    /// The user config file.
    pub path: Option<FsPath>,
    /// What was found there.
    pub file: ConfigFileState,
    /// Parse or read error of the file.
    pub error: Option<String>,
    /// Invalid `SCULL_*` environment variable, if any (other commands exit 2).
    pub env_error: Option<String>,
    /// Every effective value and where it came from (file and environment; no CLI flags).
    pub effective: Vec<ConfigValue>,
}

/// One effective config value.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigValue {
    /// Dotted key, e.g. `scan.roots`.
    pub field: String,
    /// The value.
    pub value: serde_json::Value,
    /// Where it came from, most important first.
    pub origins: Vec<Origin>,
}

/// `scull doctor`.
pub fn run(ui: &Ui) -> Result<(), ScullError> {
    let report = collect();
    if ui.format.is_machine() {
        emit_machine(ui.format, DOCTOR_SCHEMA, "doctor", &report)
    } else {
        emit_text(&render(&report, ui.color_out))
    }
}

/// Gather the report from the running system. Read-only.
pub fn collect() -> DoctorReport {
    let caps = Native.capabilities();
    let privileges = privilege_info(&Privileges::current());
    let (dirs, data_dir, config_dir) = dirs_report();
    let database = database_report(data_dir.as_deref());
    let git = git_report();
    let config = config_report(config_dir.as_deref(), |k| std::env::var(k).ok());
    let mut report = DoctorReport {
        scull: BuildInfo {
            version: crate::output::json::SCULL_VERSION.to_owned(),
            target: env!("SCULL_TARGET").to_owned(),
        },
        platform: PlatformInfo {
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            family: std::env::consts::FAMILY.to_owned(),
        },
        signals: signals(&caps),
        privileges,
        dirs,
        database,
        git,
        config,
        warnings: Vec::new(),
    };
    report.warnings = warnings(&report);
    report
}

/// Known capability fields, in display order: (field, description, what happens without it).
const SIGNALS: &[(&str, &str, &str)] = &[
    (
        "file_identity",
        "file identity (device + inode, volume + file ID)",
        "items can't be revalidated by identity before deletion, so deletion is blocked (SAFE-05)",
    ),
    (
        "allocated_size",
        "allocated size on disk",
        "sizes fall back to logical size and are labeled low confidence (PRD §9)",
    ),
    (
        "link_count",
        "hard-link count",
        "hard-linked files may be counted as reclaimable when they are not; estimates are \
         labeled low confidence (PHY-03)",
    ),
    (
        "mtime",
        "modification time",
        "recent activity can't be detected, so the activity window can't bump tiers; items \
         get lower confidence",
    ),
    (
        "no_follow_metadata",
        "metadata and listings never follow links (SAFE-04)",
        "links could be followed; scanning is refused",
    ),
    (
        "special_file_kinds",
        "FIFOs, sockets and devices identified without opening them (SAFE-18)",
        "special files can't be told apart safely and are skipped",
    ),
    (
        "reparse_points",
        "Windows reparse points told apart by tag (junctions, placeholders)",
        "reparse points are treated as unknown links and never followed",
    ),
    (
        "race_free_read_dir",
        "race-free directory listing (no-follow open of the checked handle)",
        "a directory swapped for a link between the check and the open could be listed; \
         listing is read-only and candidates are revalidated by identity before deletion \
         (SAFE-05)",
    ),
    (
        "elevation_detection",
        "root/administrator detection (SAFE-14)",
        "elevation can't be ruled out, so destructive operations are refused",
    ),
];

/// Why a signal is unavailable on this OS, when there is a more specific reason than the
/// generic consequence.
fn specific_reason(name: &str, os: &str) -> Option<String> {
    match (name, os) {
        ("reparse_points", os) if os != "windows" => Some(format!(
            "not applicable on {os}: there are no reparse points; symlinks are detected directly"
        )),
        ("race_free_read_dir", os) if os != "windows" => Some(
            "not implemented on Unix yet: listings check with lstat, then open by path. A \
             directory swapped for a link in between could be listed; listing is read-only \
             and candidates are revalidated by identity before deletion (SAFE-05)"
                .to_owned(),
        ),
        _ => None,
    }
}

/// Every capability signal, known ones first in a fixed order, then any field this CLI does
/// not know yet (so a new platform capability shows up without a CLI change).
pub fn signals(caps: &Capabilities) -> Vec<Signal> {
    let value = serde_json::to_value(caps).unwrap_or_default();
    let Some(fields) = value.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut push = |name: &str, description: &str, without: &str| {
        let Some(available) = fields.get(name).and_then(serde_json::Value::as_bool) else {
            return;
        };
        out.push(Signal {
            name: name.to_owned(),
            description: description.to_owned(),
            available,
            reason: (!available)
                .then(|| specific_reason(name, caps.os).unwrap_or_else(|| without.to_owned())),
        });
    };
    for (name, description, without) in SIGNALS {
        push(name, description, without);
    }
    for name in fields.keys() {
        if name != "os" && !SIGNALS.iter().any(|(n, _, _)| n == name) {
            push(
                name,
                name,
                "not provided by this build; affected items get lower confidence or are \
                 blocked (PLT-08)",
            );
        }
    }
    if caps.os == "macos" {
        out.push(Signal {
            name: "full_disk_access".to_owned(),
            description: "Full Disk Access (macOS privacy controls)".to_owned(),
            available: false,
            reason: Some(
                "not probed by this build: folders protected by macOS privacy controls are \
                 reported as inaccessible during scans, never skipped silently"
                    .to_owned(),
            ),
        });
    }
    out
}

/// The privilege section from the platform's report.
pub fn privilege_info(p: &Privileges) -> PrivilegeInfo {
    use scull_platform::Elevation;
    PrivilegeInfo {
        elevation: match p.elevation {
            Elevation::Elevated => ElevationState::Elevated,
            Elevation::NotElevated => ElevationState::NotElevated,
            Elevation::Unknown => ElevationState::Unknown,
        },
        sudo_user: p.sudo_user.clone(),
        destructive_blocked: p.elevation.blocks_destructive(),
    }
}

fn dirs_report() -> (DirsReport, Option<PathBuf>, Option<PathBuf>) {
    let scull_home = std::env::var_os(scull_platform::HOME_OVERRIDE_ENV)
        .filter(|v| !v.is_empty())
        .map(|v| FsPath::from_path(Path::new(&v)));
    match app_dirs() {
        Err(e) => (
            DirsReport {
                scull_home,
                error: Some(e.to_string()),
                entries: Vec::new(),
            },
            None,
            None,
        ),
        Ok(d) => {
            let roles = [
                ("config", &d.config),
                ("data", &d.data),
                ("cache", &d.cache),
                ("state", &d.state),
            ];
            let mut entries: Vec<DirReport> = Vec::new();
            for (role, path) in roles {
                let fs = FsPath::from_path(path);
                if let Some(e) = entries.iter_mut().find(|e| e.path == fs) {
                    e.roles.push(role.to_owned());
                    continue;
                }
                let perms = permissions(path);
                entries.push(DirReport {
                    roles: vec![role.to_owned()],
                    path: fs,
                    exists: perms.exists,
                    owner_only: perms.owner_only,
                    exposures: perms.exposures,
                    error: perms.error,
                });
            }
            (
                DirsReport {
                    scull_home,
                    error: None,
                    entries,
                },
                Some(d.data.clone()),
                Some(d.config.clone()),
            )
        }
    }
}

struct Perms {
    exists: bool,
    owner_only: Option<bool>,
    exposures: Vec<String>,
    error: Option<String>,
}

/// Owner-only check of an existing path (never follows a link, changes nothing).
fn permissions(path: &Path) -> Perms {
    let mut p = Perms {
        exists: std::fs::symlink_metadata(path).is_ok(),
        owner_only: None,
        exposures: Vec::new(),
        error: None,
    };
    if !p.exists {
        return p;
    }
    match ensure_owner_only(path) {
        Ok(()) => p.owner_only = Some(true),
        Err(e) => match e.kind() {
            PlatformErrorKind::NotOwnerOnly { exposures } => {
                p.owner_only = Some(false);
                p.exposures = exposures.iter().map(ToString::to_string).collect();
            }
            _ => p.error = Some(e.to_string()),
        },
    }
    p
}

fn database_report(data_dir: Option<&Path>) -> DatabaseReport {
    let mut r = DatabaseReport {
        path: None,
        state: DatabaseState::Unknown,
        schema_version: None,
        supported_schema_version: SCHEMA_VERSION,
        integrity: Integrity::NotChecked,
        footprint_bytes: None,
        owner_only: None,
        detail: Some("scull's data directory could not be located".to_owned()),
    };
    let Some(data_dir) = data_dir else {
        return r;
    };
    let path = db::db_path(data_dir);
    r.path = Some(FsPath::from_path(&path));
    r.detail = None;
    let perms = permissions(&path);
    r.owner_only = perms.owner_only;
    if let Some(e) = perms.error {
        r.detail = Some(e);
    }
    match db::open_current(&path) {
        Ok(store) => {
            r.state = DatabaseState::Ok;
            r.schema_version = store.schema_version().ok();
            match store.check_integrity() {
                Ok(()) => r.integrity = Integrity::Ok,
                Err(e) => {
                    r.state = DatabaseState::Corrupt;
                    r.integrity = Integrity::Damaged;
                    r.detail = Some(e.to_string());
                }
            }
            r.footprint_bytes = Some(store.footprint().total());
        }
        Err(NotOpened::Probe(p)) => {
            let (state, detail) = match p {
                Probe::Missing => (
                    DatabaseState::Missing,
                    "not created yet; the first scan creates it".to_owned(),
                ),
                Probe::NotAFile => (
                    DatabaseState::NotAFile,
                    "something other than a regular file is at this path".to_owned(),
                ),
                Probe::Uninitialized => (
                    DatabaseState::Uninitialized,
                    "the file exists but has no schema yet; the next scan initializes it"
                        .to_owned(),
                ),
                Probe::NotSqlite => (
                    DatabaseState::NotSqlite,
                    "not a SQLite database; scull refuses to use it".to_owned(),
                ),
                Probe::Foreign { application_id } => (
                    DatabaseState::Foreign,
                    format!(
                        "a SQLite database of another application (application id \
                         {application_id:#x}); scull refuses to use it"
                    ),
                ),
                Probe::Scull { version } if version > SCHEMA_VERSION => {
                    r.schema_version = Some(version);
                    (
                        DatabaseState::TooNew,
                        format!(
                            "schema version {version} was written by a newer scull (this one \
                             supports up to {SCHEMA_VERSION}); upgrade scull"
                        ),
                    )
                }
                Probe::Scull { version } => {
                    r.schema_version = Some(version);
                    (
                        DatabaseState::NeedsMigration,
                        format!(
                            "schema version {version}; the next command that uses the \
                             database migrates it to {SCHEMA_VERSION} after taking a backup \
                             (doctor does not migrate)"
                        ),
                    )
                }
            };
            r.state = state;
            r.detail = Some(detail);
        }
        Err(NotOpened::Io(e)) => {
            r.state = DatabaseState::Error;
            r.detail = Some(format!("cannot read {}: {e}", path.display()));
        }
        Err(NotOpened::Store(e)) => {
            r.state = match e {
                StoreError::Corrupt { .. } => {
                    r.integrity = Integrity::Damaged;
                    DatabaseState::Corrupt
                }
                StoreError::SchemaTooNew { .. } => DatabaseState::TooNew,
                StoreError::NotScullDatabase { .. } => DatabaseState::Foreign,
                _ => DatabaseState::Error,
            };
            r.detail = Some(e.to_string());
        }
    }
    r
}

fn git_report() -> GitReport {
    let min_version = scull_git::MIN_GIT_VERSION.to_string();
    match scull_git::GitRunner::new() {
        Ok(g) => GitReport {
            available: true,
            binary: Some(FsPath::from_path(g.resolved_binary())),
            version: Some(g.version().to_string()),
            min_version,
            error: None,
        },
        Err(e) => {
            let binary = match &e {
                scull_git::GitError::UnsupportedVersion { binary, .. } => {
                    Some(FsPath::from_path(binary))
                }
                _ => None,
            };
            GitReport {
                available: false,
                binary,
                version: None,
                min_version,
                error: Some(e.to_string()),
            }
        }
    }
}

/// The config section: read and parse `<config dir>/config.toml`, read the environment
/// through `env`, and resolve the effective values with their origins.
pub fn config_report(
    config_dir: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> ConfigReport {
    let mut report = ConfigReport {
        path: None,
        file: ConfigFileState::Unknown,
        error: None,
        env_error: None,
        effective: Vec::new(),
    };
    let mut user = ConfigLayer::default();
    if let Some(dir) = config_dir {
        let path = dir.join(CONFIG_FILE_NAME);
        report.path = Some(FsPath::from_path(&path));
        match std::fs::read_to_string(&path) {
            Ok(text) => match ConfigLayer::from_toml(&text) {
                Ok(layer) => {
                    user = layer;
                    report.file = ConfigFileState::Ok;
                }
                Err(e) => {
                    report.file = ConfigFileState::Invalid;
                    report.error = Some(e.message);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.file = ConfigFileState::Missing;
            }
            Err(e) => {
                report.file = ConfigFileState::Unreadable;
                report.error = Some(e.to_string());
            }
        }
    }
    let env_layer = match EnvLayer::from_lookup(env) {
        Ok(l) => l,
        Err(e) => {
            report.env_error = Some(e.message);
            EnvLayer::default()
        }
    };
    let resolved = resolve(&ConfigLayer::default(), &env_layer, &user);
    let values = serde_json::to_value(&resolved.config).unwrap_or_default();
    report.effective = FIELDS
        .iter()
        .map(|field| {
            let value = field.split('.').fold(&values, |v, k| &v[k]).clone();
            ConfigValue {
                field: (*field).to_owned(),
                value,
                origins: resolved.origin(field).to_vec(),
            }
        })
        .collect();
    report
}

/// Problems worth a line of their own.
pub fn warnings(r: &DoctorReport) -> Vec<String> {
    let mut w = Vec::new();
    match r.privileges.elevation {
        ElevationState::Elevated => {
            let via = r
                .privileges
                .sudo_user
                .as_deref()
                .map(|u| format!(" (started by {u} through sudo/doas)"))
                .unwrap_or_default();
            w.push(format!(
                "running as root/administrator{via}: destructive operations (clean, restore) \
                 will refuse unless --allow-root, and scull's directories resolve for the \
                 elevated account (SAFE-14); run scull as your own user"
            ));
        }
        ElevationState::Unknown => w.push(
            "could not tell whether scull runs elevated: destructive operations will refuse \
             unless --allow-root (SAFE-14)"
                .to_owned(),
        ),
        ElevationState::NotElevated => {}
    }
    if let Some(e) = &r.dirs.error {
        w.push(e.clone());
    }
    for d in &r.dirs.entries {
        if d.owner_only == Some(false) {
            w.push(format!(
                "{} is accessible by other users: {}",
                d.path,
                d.exposures.join("; ")
            ));
        }
        if let Some(e) = &d.error {
            w.push(e.clone());
        }
    }
    let db = &r.database;
    match db.state {
        DatabaseState::Ok | DatabaseState::Missing | DatabaseState::Unknown => {}
        _ => w.push(format!(
            "database: {}",
            db.detail.as_deref().unwrap_or("problem")
        )),
    }
    if db.owner_only == Some(false) {
        w.push("the database file is accessible by other users".to_owned());
    }
    if !r.git.available {
        w.push(format!(
            "git: {}; repository safety checks will block Git-related items",
            r.git.error.as_deref().unwrap_or("unavailable")
        ));
    }
    if let Some(e) = &r.config.error {
        w.push(format!("config file: {}", one_line(e)));
    }
    if let Some(e) = &r.config.env_error {
        w.push(e.clone());
    }
    w
}

fn yes_no(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "yes",
        Some(false) => "NO",
        None => "-",
    }
}

fn snake_to_words(s: &str) -> String {
    s.replace('_', " ")
}

/// The human report. Every value from outside the binary is escaped by [`Cell`].
pub fn render(r: &DoctorReport, color: bool) -> String {
    let heading = |s: &str| format!("\n{}\n", Style::Bold.paint(s, color));
    let mut out = format!(
        "scull {} ({}) on {} {} ({})\n",
        r.scull.version, r.scull.target, r.platform.os, r.platform.arch, r.platform.family
    );

    out.push_str(&heading("Signals"));
    let mut t = Table::new([
        ("Signal", Align::Left),
        ("Status", Align::Left),
        ("Detail", Align::Left),
    ]);
    for s in &r.signals {
        let status = if s.available {
            Cell::text("available")
        } else {
            Cell::text("unavailable").styled(Style::Warn)
        };
        let detail = s.reason.as_deref().unwrap_or(&s.description);
        t.push(vec![Cell::text(&s.name), status, Cell::text(detail)]);
    }
    out.push_str(&t.render(color));

    out.push_str(&heading("Privileges"));
    let p = &r.privileges;
    let mut t = Table::new([("Check", Align::Left), ("Value", Align::Left)]);
    t.push(vec![
        Cell::text("elevation"),
        Cell::text(&snake_to_words(match p.elevation {
            ElevationState::Elevated => "elevated",
            ElevationState::NotElevated => "not_elevated",
            ElevationState::Unknown => "unknown",
        })),
    ]);
    t.push(vec![
        Cell::text("sudo user"),
        Cell::text(p.sudo_user.as_deref().unwrap_or("-")),
    ]);
    t.push(vec![
        Cell::text("destructive ops"),
        if p.destructive_blocked {
            Cell::text("refused without --allow-root (SAFE-14)").styled(Style::Warn)
        } else {
            Cell::text("allowed (after a reviewed plan and confirmation)")
        },
    ]);
    out.push_str(&t.render(color));

    out.push_str(&heading("Directories"));
    if let Some(h) = &r.dirs.scull_home {
        out.push_str("SCULL_HOME: ");
        out.push_str(Cell::path(h.as_str()).as_str());
        out.push('\n');
    }
    if let Some(e) = &r.dirs.error {
        out.push_str(Cell::text(e).as_str());
        out.push('\n');
    } else {
        let mut t = Table::new([
            ("Role", Align::Left),
            ("Exists", Align::Left),
            ("Owner-only", Align::Left),
            ("Path", Align::Left),
        ]);
        for d in &r.dirs.entries {
            let owner = match (&d.error, d.owner_only) {
                (Some(_), _) => Cell::text("error").styled(Style::Warn),
                (None, Some(false)) => Cell::text("NO").styled(Style::Warn),
                (None, b) => Cell::text(yes_no(b)),
            };
            t.push(vec![
                Cell::text(&d.roles.join(", ")),
                Cell::text(if d.exists { "yes" } else { "not yet" }),
                owner,
                Cell::path(d.path.as_str()),
            ]);
        }
        out.push_str(&t.render(color));
    }

    out.push_str(&heading("Database"));
    let db = &r.database;
    let mut t = Table::new([("Check", Align::Left), ("Value", Align::Left)]);
    if let Some(path) = &db.path {
        t.push(vec![Cell::text("path"), Cell::path(path.as_str())]);
    }
    let state = Cell::text(&snake_to_words(&json_name(&db.state)));
    t.push(vec![
        Cell::text("state"),
        if db.state == DatabaseState::Ok || db.state == DatabaseState::Missing {
            state
        } else {
            state.styled(Style::Warn)
        },
    ]);
    t.push(vec![
        Cell::text("schema"),
        Cell::text(&match db.schema_version {
            Some(v) => format!("v{v} (supported: v{})", db.supported_schema_version),
            None => format!("- (supported: v{})", db.supported_schema_version),
        }),
    ]);
    t.push(vec![
        Cell::text("integrity"),
        Cell::text(&snake_to_words(&json_name(&db.integrity))),
    ]);
    if let Some(b) = db.footprint_bytes {
        t.push(vec![
            Cell::text("footprint"),
            Cell::text(&format!("{} (logical)", format_bytes(b))),
        ]);
    }
    if db.path.is_some() && db.owner_only.is_some() {
        t.push(vec![
            Cell::text("owner-only"),
            Cell::text(yes_no(db.owner_only)),
        ]);
    }
    if let Some(d) = &db.detail {
        t.push(vec![Cell::text("detail"), Cell::text(d)]);
    }
    out.push_str(&t.render(color));

    out.push_str(&heading("Git"));
    let g = &r.git;
    let mut t = Table::new([("Check", Align::Left), ("Value", Align::Left)]);
    if let Some(b) = &g.binary {
        t.push(vec![Cell::text("binary"), Cell::path(b.as_str())]);
    }
    t.push(vec![
        Cell::text("version"),
        match &g.version {
            Some(v) => Cell::text(&format!("{v} (minimum {})", g.min_version)),
            None => Cell::text("unavailable").styled(Style::Warn),
        },
    ]);
    if let Some(e) = &g.error {
        t.push(vec![Cell::text("error"), Cell::text(e)]);
    }
    out.push_str(&t.render(color));

    out.push_str(&heading("Config"));
    let c = &r.config;
    if let Some(path) = &c.path {
        out.push_str(&format!(
            "file: {} ({})\n",
            Cell::path(path.as_str()).as_str(),
            snake_to_words(&json_name(&c.file))
        ));
    }
    for e in [&c.error, &c.env_error].into_iter().flatten() {
        out.push_str(&Style::Warn.paint(Cell::text(&one_line(e)).as_str(), color));
        out.push('\n');
    }
    let mut t = Table::new([
        ("Field", Align::Left),
        ("Value", Align::Left),
        ("Origin", Align::Left),
    ]);
    for v in &c.effective {
        let origins: Vec<String> = v.origins.iter().map(ToString::to_string).collect();
        t.push(vec![
            Cell::text(&v.field),
            Cell::text(&compact_value(&v.value)),
            Cell::text(&origins.join(", ")),
        ]);
    }
    out.push_str(&t.render(color));

    out.push_str(&heading("Warnings"));
    if r.warnings.is_empty() {
        out.push_str("none\n");
    } else {
        for w in &r.warnings {
            out.push_str("- ");
            out.push_str(&Style::Warn.paint(Cell::text(w).as_str(), color));
            out.push('\n');
        }
    }
    out
}

/// A multi-line parser message (e.g. TOML's, with a source excerpt and caret line) on one
/// line: the excerpt is dropped and the remaining lines are joined with `: `.
fn one_line(msg: &str) -> String {
    msg.lines()
        .map(str::trim)
        .filter(|l| {
            let excerpt = l.starts_with('|')
                || l.split_once(" |")
                    .is_some_and(|(n, _)| n.chars().all(|c| c.is_ascii_digit()));
            !l.is_empty() && !excerpt
        })
        .collect::<Vec<_>>()
        .join(": ")
}

/// The serde name of a unit enum variant.
fn json_name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// A config value on one line: strings bare, empty collections as `-`, others as JSON.
fn compact_value(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) if a.is_empty() => "-".to_owned(),
        Value::Object(o) if o.is_empty() => "-".to_owned(),
        Value::Array(a) => a.iter().map(compact_value).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_platform::Elevation;
    use std::ffi::OsString;

    fn caps(os: &'static str) -> Capabilities {
        let mut c = Native.capabilities();
        c.os = os;
        c.race_free_read_dir = false;
        c.reparse_points = false;
        c
    }

    /// A report built without touching the real home directory or environment.
    fn sample() -> DoctorReport {
        let caps = caps("linux");
        DoctorReport {
            scull: BuildInfo {
                version: "0.0.0".into(),
                target: "x86_64-unknown-linux-gnu".into(),
            },
            platform: PlatformInfo {
                os: "linux".into(),
                arch: "x86_64".into(),
                family: "unix".into(),
            },
            signals: signals(&caps),
            privileges: privilege_info(&Privileges::from_parts(Elevation::NotElevated, |_| None)),
            dirs: DirsReport {
                scull_home: None,
                error: None,
                entries: vec![DirReport {
                    roles: vec!["data".into()],
                    path: FsPath::from("/home/u/.local/share/scull"),
                    exists: false,
                    owner_only: None,
                    exposures: Vec::new(),
                    error: None,
                }],
            },
            database: database_report(None),
            git: GitReport {
                available: false,
                binary: None,
                version: None,
                min_version: "2.20.0".into(),
                error: Some("not found".into()),
            },
            config: config_report(None, |_| None),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn every_capability_field_is_a_signal_with_reasons_when_unavailable() {
        let c = caps("linux");
        let sig = signals(&c);
        let fields = serde_json::to_value(c).expect("caps serialize");
        let n = fields.as_object().expect("object").len() - 1; // minus `os`
        assert_eq!(sig.len(), n);
        for s in &sig {
            assert_eq!(s.available, s.reason.is_none(), "{s:?}");
        }
        let race = sig
            .iter()
            .find(|s| s.name == "race_free_read_dir")
            .expect("race signal");
        assert!(!race.available);
        assert!(race.reason.as_deref().expect("reason").contains("lstat"));
        let reparse = sig.iter().find(|s| s.name == "reparse_points").expect("x");
        assert!(
            reparse
                .reason
                .as_deref()
                .expect("reason")
                .contains("not applicable on linux")
        );
    }

    #[test]
    fn macos_reports_full_disk_access_as_unprobed() {
        let sig = signals(&caps("macos"));
        let fda = sig.last().expect("signals");
        assert_eq!(fda.name, "full_disk_access");
        assert!(!fda.available);
        assert!(fda.reason.is_some());
    }

    #[test]
    fn elevation_warns_with_sudo_user() {
        let env = |k: &str| (k == "SUDO_USER").then(|| OsString::from("alice"));
        let p = privilege_info(&Privileges::from_parts(Elevation::Elevated, env));
        assert!(p.destructive_blocked);
        assert_eq!(p.sudo_user.as_deref(), Some("alice"));
        let mut r = sample();
        r.privileges = p;
        let w = warnings(&r);
        assert!(
            w.iter()
                .any(|w| w.contains("--allow-root") && w.contains("alice")),
            "{w:?}"
        );
        let p = privilege_info(&Privileges::from_parts(Elevation::Unknown, |_| None));
        assert!(p.destructive_blocked);
    }

    #[test]
    fn config_origins_from_file_and_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[scan]\nroots = [\"/p\"]\nthreads = 2\n",
        )
        .expect("write");
        let env = |k: &str| (k == "SCULL_THREADS").then(|| "4".to_owned());
        let r = config_report(Some(dir.path()), env);
        assert_eq!(r.file, ConfigFileState::Ok);
        let get = |f: &str| r.effective.iter().find(|v| v.field == f).expect(f);
        assert_eq!(get("scan.roots").value, serde_json::json!(["/p"]));
        assert_eq!(get("scan.roots").origins, vec![Origin::UserConfig]);
        assert_eq!(get("scan.threads").value, serde_json::json!(4));
        assert_eq!(
            get("scan.threads").origins,
            vec![Origin::Env {
                var: "SCULL_THREADS".into()
            }]
        );
        assert_eq!(get("mcp.enabled").origins, vec![Origin::Default]);
        assert_eq!(r.effective.len(), FIELDS.len());
    }

    #[test]
    fn invalid_config_and_env_are_reported_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), "[scan]\nrootz = 1\n").expect("write");
        let env = |k: &str| (k == "SCULL_THREADS").then(|| "many".to_owned());
        let r = config_report(Some(dir.path()), env);
        assert_eq!(r.file, ConfigFileState::Invalid);
        assert!(r.error.as_deref().expect("error").contains("rootz"));
        assert!(
            r.env_error
                .as_deref()
                .expect("env")
                .contains("SCULL_THREADS")
        );
        let missing = config_report(Some(&dir.path().join("nope")), |_| None);
        assert_eq!(missing.file, ConfigFileState::Missing);
    }

    #[test]
    fn parser_messages_fit_on_one_line() {
        let msg = "invalid user config: TOML parse error at line 2, column 1\n  |\n2 | rootz = 1\n  | ^^^^^\nunknown field `rootz`";
        assert_eq!(
            one_line(msg),
            "invalid user config: TOML parse error at line 2, column 1: unknown field `rootz`"
        );
        assert_eq!(one_line("plain"), "plain");
    }

    #[test]
    fn render_escapes_hostile_values() {
        let mut r = sample();
        r.config.error = Some("bad\x1b[2Jvalue".to_owned());
        r.warnings = vec!["evil\u{202e}name".to_owned()];
        let out = render(&r, false);
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\u{202e}'));
        for section in [
            "Signals",
            "Privileges",
            "Directories",
            "Database",
            "Git",
            "Config",
            "Warnings",
        ] {
            assert!(out.contains(&format!("\n{section}\n")), "{section}");
        }
    }
}
