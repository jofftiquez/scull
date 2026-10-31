//! `scull data show|export|purge` (P2-CLI-04; PRD §12, DATA-05, DATA-07, INS-08, §20).
//!
//! What scull has recorded about this machine, and how to take it with you or get rid of it.
//!
//! * **`show`** lists scull's directories, the database (path, schema version, rows per table),
//!   every file of scull's own footprint (database, `-wal`/`-shm`, migration backups, damaged
//!   copies, audit mirrors, the MCP name-hashing salt) with its size, and the quarantine
//!   folders recorded in the manifest (read-only: names come from the database, nothing in
//!   quarantine is opened). It never creates or migrates the database ([`crate::db`]).
//!   Machine output: `scull/data-show@1`.
//! * **`export [--format json|csv] [--out DIR]`** writes every table to `DIR` (default
//!   `./scull-export-<unix time>`), created owner-only, with owner-only files that are never
//!   overwritten: JSON is one `scull-data.json` document (`scull/data-dump@1`), CSV is one
//!   `<table>.csv` per table (RFC 4180, header row, values exactly as recorded, so a cell can
//!   start with `=`; open it as text, not as a formula sheet). Paths are exported as recorded;
//!   the database holds metadata only, never file contents. Parquet (DATA-07) is not built
//!   yet. Machine output: `scull/data-export@1`.
//! * **`purge --all | --older-than AGE`** deletes scull's own files and nothing else:
//!   * The files are the ones [`scull_storage::footprint_files`] lists, plus SQLite side files
//!     of backups and damaged copies and the MCP salt. Plans, approvals and audit rows are
//!     append-only (enforced by triggers), so purging removes **files**, never rows.
//!   * Planning (here): each file must sit inside scull's data directory (a real directory,
//!     whose identity is captured), every directory in between must be a real directory, and
//!     the entry itself must be a regular file or a symlink; its no-follow identity and kind
//!     are captured. Anything else is refused and left in place.
//!   * Removal is not done here: the planned files go to
//!     `scull_cleaner::own_data::purge_own_files`, the single deletion path (SAFE-10), which
//!     removes each one fd-relatively from the data directory with the platform's no-follow
//!     walker, only if it still has the planned identity and kind. A symlink is removed as a
//!     link; its target is never touched. A changed data directory or a walk anomaly aborts
//!     the rest.
//!   * `--all` refuses while the quarantine manifest has items that are neither restored nor
//!     purged: deleting the database would lose the only record of where they came from.
//!     Quarantine contents are never touched by this command; `--include-quarantine` is
//!     refused until the Executor's quarantine purge (#71) exists. `--all` also refuses when
//!     the database can't be read (newer, damaged, or not scull's), since it then can't check.
//!   * `--older-than AGE` removes only migration backups and damaged copies last modified
//!     more than `AGE` ago (`90m`, `12h`, `30d`, `2w`); the live database stays.
//!   * Confirmation: `--confirm`, or typing `purge` at the prompt on a terminal. Without a
//!     terminal (or with `--json`/`--ndjson`) `--confirm` is required.
//!   * The configuration (`config.toml`, profile) is kept. Machine output: `scull/data-purge@1`.

use super::{emit_machine, emit_text};
use crate::cancel;
use crate::db::{self, NotOpened, Probe};
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::{SCULL_VERSION, SchemaId};
use schemars::JsonSchema;
use scull_cleaner::own_data::{OwnFileOutcome, OwnFileTarget, purge_own_files};
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_platform::{AppDirs, EntryKind, Native, Platform};
use scull_storage::{ExportSink, SCHEMA_VERSION, Store, StoreError};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, BufRead, BufWriter, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};

/// Schema of `scull data show --json`.
pub const DATA_SHOW_SCHEMA: SchemaId = SchemaId::new("data-show", 1);
/// Schema of `scull data export --json` (the report on stdout).
pub const DATA_EXPORT_SCHEMA: SchemaId = SchemaId::new("data-export", 1);
/// Schema of the `scull-data.json` file written by `scull data export --format json`.
pub const DATA_DUMP_SCHEMA: SchemaId = SchemaId::new("data-dump", 1);
/// Schema of `scull data purge --json`.
pub const DATA_PURGE_SCHEMA: SchemaId = SchemaId::new("data-purge", 1);

/// File name of the JSON export.
pub const JSON_EXPORT_FILE: &str = "scull-data.json";

/// The word typed at the prompt to confirm a purge.
pub const CONFIRM_WORD: &str = "purge";

// ---------------------------------------------------------------------------------------------
// Shared report pieces
// ---------------------------------------------------------------------------------------------

/// scull's per-user directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DirsReport {
    /// Configuration (`config.toml`, profile). Never purged.
    pub config: FsPath,
    /// Database, backups, audit mirrors, salt.
    pub data: FsPath,
    /// Disposable cache.
    pub cache: FsPath,
    /// Logs and other state.
    pub state: FsPath,
}

impl DirsReport {
    fn new(d: &AppDirs) -> Self {
        Self {
            config: FsPath::from(d.config.as_path()),
            data: FsPath::from(d.data.as_path()),
            cache: FsPath::from(d.cache.as_path()),
            state: FsPath::from(d.state.as_path()),
        }
    }
}

/// What a file of scull's own footprint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// The database.
    Database,
    /// Its write-ahead log.
    Wal,
    /// Its shared-memory index.
    Shm,
    /// A rollback journal.
    Journal,
    /// A backup taken before a migration.
    Backup,
    /// A damaged database set aside by a restore.
    Damaged,
    /// A `-wal`, `-shm` or `-journal` file of a backup or damaged copy.
    SideFile,
    /// A file in the audit mirror folder (`<operation-id>.ndjson`).
    AuditMirror,
    /// The MCP name-hashing salt (never printed).
    Salt,
    /// Another file the storage layer reports as part of the footprint.
    Other,
}

/// One file of scull's own footprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwnFile {
    /// Where it is.
    pub path: FsPath,
    /// What it is.
    pub kind: FileKind,
    /// Entry type, not following links (`file`, `symlink`, ...).
    pub entry: String,
    /// Logical size in bytes (of the link itself for a symlink).
    pub bytes: u64,
    /// Last modification (Unix seconds), if known.
    pub modified_at: Option<i64>,
}

/// Rows in one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableRows {
    /// Table name.
    pub table: String,
    /// Rows.
    pub rows: u64,
}

/// How the database looks to this command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DbState {
    /// Opened with the current schema.
    Ok,
    /// No database file.
    Missing,
    /// An empty database (never used).
    Empty,
    /// Written by a newer scull.
    NewerSchema,
    /// Older schema; the next command that writes to it migrates it.
    NeedsMigration,
    /// Something other than a scull database, or not a regular file.
    NotScull,
    /// Damaged or unreadable.
    Unreadable,
}

/// The database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DatabaseReport {
    /// Database file.
    pub path: FsPath,
    /// How it looks.
    pub state: DbState,
    /// Why it couldn't be read, when it couldn't.
    pub detail: Option<String>,
    /// Its schema version, when known.
    pub schema_version: Option<u32>,
    /// The schema version this scull uses.
    pub supported_schema_version: u32,
}

/// Quarantine as recorded in the manifest (the manifest of record, SAFE-16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuarantineReport {
    /// Whether the manifest could be read (false when the database couldn't).
    pub known: bool,
    /// Items quarantined and neither restored nor purged.
    pub outstanding_items: u64,
    /// Their recorded size in bytes.
    pub outstanding_bytes: u64,
    /// Folders holding them, as recorded (parents of the quarantine paths), sorted.
    pub folders: Vec<FsPath>,
}

// ---------------------------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------------------------

/// `scull data show` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataShowReport {
    /// scull's directories.
    pub dirs: DirsReport,
    /// The database.
    pub database: DatabaseReport,
    /// Rows per table (empty when the database can't be read).
    pub tables: Vec<TableRows>,
    /// scull's own files.
    pub files: Vec<OwnFile>,
    /// Their total size in bytes.
    pub footprint_bytes: u64,
    /// Quarantine, as recorded.
    pub quarantine: QuarantineReport,
}

/// `scull data show`.
pub fn run_show(ui: &Ui) -> Result<(), ScullError> {
    let dirs = app_dirs()?;
    let report = show(&dirs);
    if ui.format.is_machine() {
        return emit_machine(ui.format, DATA_SHOW_SCHEMA, "data", &report);
    }
    emit_text(&render_show(&report))
}

/// Collect `scull data show` for `dirs` (never creates or migrates anything).
pub fn show(dirs: &AppDirs) -> DataShowReport {
    let db_path = db::db_path(&dirs.data);
    let mut tables = Vec::new();
    let mut quarantine = QuarantineReport {
        known: false,
        outstanding_items: 0,
        outstanding_bytes: 0,
        folders: Vec::new(),
    };
    let database = match open_db(&db_path) {
        Ok(store) => {
            let mut detail = None;
            match store.table_counts() {
                Ok(c) => {
                    tables = c
                        .into_iter()
                        .map(|t| TableRows {
                            table: t.table.to_owned(),
                            rows: t.rows,
                        })
                        .collect();
                }
                Err(e) => detail = Some(e.to_string()),
            }
            match store.quarantine_outstanding() {
                Ok(q) => quarantine = quarantine_report(&q),
                Err(e) => detail = detail.or(Some(e.to_string())),
            }
            DatabaseReport {
                path: FsPath::from(db_path.as_path()),
                state: if detail.is_some() {
                    DbState::Unreadable
                } else {
                    DbState::Ok
                },
                detail,
                schema_version: store.schema_version().ok(),
                supported_schema_version: SCHEMA_VERSION,
            }
        }
        Err(e) => not_opened_report(&db_path, &e),
    };
    // After the store is closed: SQLite removes `-wal`/`-shm` on the last close.
    let files = own_files(&dirs.data);
    let footprint_bytes = files.iter().fold(0u64, |a, f| a.saturating_add(f.bytes));
    DataShowReport {
        dirs: DirsReport::new(dirs),
        database,
        tables,
        files,
        footprint_bytes,
        quarantine,
    }
}

fn quarantine_report(q: &[scull_storage::QuarantineSummary]) -> QuarantineReport {
    let mut folders: Vec<String> = q
        .iter()
        .map(|e| {
            Path::new(&e.quarantine_path)
                .parent()
                .map_or_else(|| e.quarantine_path.clone(), |p| p.display().to_string())
        })
        .collect();
    folders.sort();
    folders.dedup();
    QuarantineReport {
        known: true,
        outstanding_items: q.len() as u64,
        outstanding_bytes: q.iter().fold(0u64, |a, e| a.saturating_add(e.bytes)),
        folders: folders.iter().map(|s| FsPath::from(s.as_str())).collect(),
    }
}

fn not_opened_report(path: &Path, e: &NotOpened) -> DatabaseReport {
    let (state, detail, version) = match e {
        NotOpened::Probe(Probe::Missing) => (DbState::Missing, None, None),
        NotOpened::Probe(Probe::Uninitialized) => (DbState::Empty, None, None),
        NotOpened::Probe(Probe::Scull { version }) if *version > SCHEMA_VERSION => (
            DbState::NewerSchema,
            Some("written by a newer scull".to_owned()),
            Some(*version),
        ),
        NotOpened::Probe(Probe::Scull { version }) => (
            DbState::NeedsMigration,
            Some("older schema; the next scan migrates it (with a backup)".to_owned()),
            Some(*version),
        ),
        NotOpened::Probe(Probe::NotAFile) => (
            DbState::NotScull,
            Some("not a regular file".to_owned()),
            None,
        ),
        NotOpened::Probe(Probe::NotSqlite) => (
            DbState::NotScull,
            Some("not a SQLite database".to_owned()),
            None,
        ),
        NotOpened::Probe(Probe::Foreign { application_id }) => (
            DbState::NotScull,
            Some(format!(
                "a SQLite database of another application (id {application_id:#x})"
            )),
            None,
        ),
        NotOpened::Io(e) => (DbState::Unreadable, Some(e.to_string()), None),
        NotOpened::Store(e) => (DbState::Unreadable, Some(e.to_string()), None),
    };
    DatabaseReport {
        path: FsPath::from(path),
        state,
        detail,
        schema_version: version,
        supported_schema_version: SCHEMA_VERSION,
    }
}

fn render_show(r: &DataShowReport) -> String {
    let esc = |s: &str| escape_for_terminal(s).into_owned();
    let mut out = String::new();
    let line = |out: &mut String, s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(
        &mut out,
        "What scull has recorded on this machine".to_owned(),
    );
    line(&mut out, String::new());
    line(
        &mut out,
        format!("Data folder    {}", esc(r.dirs.data.as_str())),
    );
    line(
        &mut out,
        format!(
            "Config folder  {} (kept by purge)",
            esc(r.dirs.config.as_str())
        ),
    );
    line(
        &mut out,
        format!("State folder   {}", esc(r.dirs.state.as_str())),
    );
    line(&mut out, String::new());
    let db = &r.database;
    let state = match db.state {
        DbState::Ok => format!(
            "schema v{}",
            db.schema_version.unwrap_or(db.supported_schema_version)
        ),
        DbState::Missing => "not created yet (nothing recorded)".to_owned(),
        DbState::Empty => "empty".to_owned(),
        _ => db.detail.clone().unwrap_or_else(|| "unreadable".to_owned()),
    };
    line(
        &mut out,
        format!("Database       {} ({})", esc(db.path.as_str()), esc(&state)),
    );
    if !r.tables.is_empty() {
        let width = r.tables.iter().map(|t| t.table.len()).max().unwrap_or(0);
        for t in &r.tables {
            line(
                &mut out,
                format!("  {:<width$}  {:>8} rows", t.table, t.rows),
            );
        }
    }
    line(&mut out, String::new());
    line(
        &mut out,
        format!(
            "Files          {} files, {}",
            r.files.len(),
            format_bytes(r.footprint_bytes)
        ),
    );
    for f in &r.files {
        line(
            &mut out,
            format!(
                "  {:>10}  {:<12}  {}{}",
                format_bytes(f.bytes),
                super::json_name(&f.kind),
                esc(f.path.as_str()),
                if f.entry == "file" {
                    String::new()
                } else {
                    format!(" ({})", f.entry)
                }
            ),
        );
    }
    line(&mut out, String::new());
    let q = &r.quarantine;
    if !q.known {
        line(
            &mut out,
            "Quarantine     unknown (the database can't be read)".to_owned(),
        );
    } else if q.outstanding_items == 0 {
        line(&mut out, "Quarantine     nothing in quarantine".to_owned());
    } else {
        line(
            &mut out,
            format!(
                "Quarantine     {} items, {} in:",
                q.outstanding_items,
                format_bytes(q.outstanding_bytes)
            ),
        );
        for f in &q.folders {
            line(&mut out, format!("  {}", esc(f.as_str())));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Own files
// ---------------------------------------------------------------------------------------------

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// [`db::open_current`], refusing first when the database's `-wal`, `-shm` or `-journal` is
/// anything but a regular file: SQLite would follow a link there and write through it.
fn open_db(path: &Path) -> Result<Store, NotOpened> {
    if let Some(why) = unsafe_side_file(path) {
        return Err(NotOpened::Io(io::Error::other(why)));
    }
    db::open_current(path)
}

fn unsafe_side_file(db: &Path) -> Option<String> {
    SIDE_SUFFIXES.iter().find_map(|s| {
        let p = with_suffix(db, s);
        let md = std::fs::symlink_metadata(&p).ok()?;
        (!md.file_type().is_file()).then(|| {
            format!(
                "{} is not a regular file; scull won't open the database through it",
                p.display()
            )
        })
    })
}

fn exists_no_follow(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

const SIDE_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Every file of scull's own footprint in `data_dir`, with sizes, in a stable order:
/// [`scull_storage::footprint_files`], plus database side files that exist as any entry (a
/// dangling link included), side files of backups and damaged copies, and the MCP salt.
pub fn own_files(data_dir: &Path) -> Vec<OwnFile> {
    let db = db::db_path(data_dir);
    let mut paths = scull_storage::footprint_files(&db);
    for s in ["", "-wal", "-shm", "-journal"] {
        let p = with_suffix(&db, s);
        if exists_no_follow(&p) && !paths.contains(&p) {
            paths.push(p);
        }
    }
    let copies: Vec<PathBuf> = paths
        .iter()
        .filter(|p| matches!(classify(p), FileKind::Backup | FileKind::Damaged))
        .cloned()
        .collect();
    for c in copies {
        for s in SIDE_SUFFIXES {
            let p = with_suffix(&c, s);
            if exists_no_follow(&p) && !paths.contains(&p) {
                paths.push(p);
            }
        }
    }
    let salt = data_dir.join(scull_mcp::SALT_FILE);
    if exists_no_follow(&salt) && !paths.contains(&salt) {
        paths.push(salt);
    }
    let native = Native::new();
    paths
        .into_iter()
        .map(|p| {
            let md = native.metadata(&p).ok();
            OwnFile {
                kind: classify(&p),
                entry: md
                    .as_ref()
                    .map_or("missing", |m| m.kind.as_str())
                    .to_owned(),
                bytes: md.as_ref().map_or(0, |m| m.logical_size),
                modified_at: md.as_ref().and_then(|m| m.mtime_unix),
                path: FsPath::from(p.as_path()),
            }
        })
        .collect()
}

/// What a footprint file is, from its name.
pub fn classify(path: &Path) -> FileKind {
    let db = scull_storage::DB_FILE_NAME;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let side = SIDE_SUFFIXES.iter().any(|s| name.ends_with(s));
    let parent_is_audit = path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|n| n == scull_storage::AUDIT_DIR_NAME);
    if name == db {
        FileKind::Database
    } else if name == format!("{db}-wal") {
        FileKind::Wal
    } else if name == format!("{db}-shm") {
        FileKind::Shm
    } else if name == format!("{db}-journal") {
        FileKind::Journal
    } else if name.starts_with(&format!("{db}.backup-")) {
        if side {
            FileKind::SideFile
        } else {
            FileKind::Backup
        }
    } else if name.starts_with(&format!("{db}.damaged-")) {
        if side {
            FileKind::SideFile
        } else {
            FileKind::Damaged
        }
    } else if name == scull_mcp::SALT_FILE {
        FileKind::Salt
    } else if parent_is_audit {
        FileKind::AuditMirror
    } else {
        FileKind::Other
    }
}

// ---------------------------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------------------------

/// Export file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, JsonSchema, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    /// One JSON document (`scull-data.json`, schema `scull/data-dump@1`).
    #[default]
    Json,
    /// One CSV file per table.
    Csv,
}

/// One file written by `scull data export`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportedFile {
    /// Where it was written.
    pub path: FsPath,
    /// The table it holds (`null` for the JSON document, which holds all of them).
    pub table: Option<String>,
    /// Rows written.
    pub rows: u64,
    /// File size in bytes.
    pub bytes: u64,
}

/// `scull data export` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataExportReport {
    /// Folder written to.
    pub out_dir: FsPath,
    /// Format written.
    pub format: ExportFormat,
    /// Database exported.
    pub database: FsPath,
    /// Its schema version.
    pub schema_version: u32,
    /// Files written.
    pub files: Vec<ExportedFile>,
    /// Rows written in total.
    pub rows: u64,
}

/// The `scull-data.json` document: every table with its columns and rows (values as recorded).
/// Only used for the schema; the file is streamed in the same shape.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataDump {
    /// When it was exported (Unix seconds).
    pub exported_at: i64,
    /// Database exported.
    pub database: FsPath,
    /// Its schema version.
    pub schema_version: u32,
    /// Every table, in a fixed order.
    pub tables: Vec<TableDump>,
}

/// One table of [`DataDump`].
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TableDump {
    /// Table name.
    pub table: String,
    /// Column names.
    pub columns: Vec<String>,
    /// Rows, keyed by column name. Integers and reals are numbers, text is a string, NULL is
    /// `null`, a BLOB is a hex string.
    pub rows: Vec<serde_json::Map<String, Value>>,
}

/// `scull data export`.
pub fn run_export(ui: &Ui, format: ExportFormat, out: Option<&Path>) -> Result<(), ScullError> {
    let dirs = app_dirs()?;
    let now = scull_engine::now_unix();
    let out_dir = match out {
        Some(p) => p.to_owned(),
        None => PathBuf::from(format!("scull-export-{now}")),
    };
    let report = export(&dirs, format, &out_dir, now)?;
    if ui.format.is_machine() {
        return emit_machine(ui.format, DATA_EXPORT_SCHEMA, "data", &report);
    }
    let mut text = format!(
        "Exported {} rows from {} to {}:\n",
        report.rows,
        escape_for_terminal(report.database.as_str()),
        escape_for_terminal(report.out_dir.as_str())
    );
    for f in &report.files {
        text.push_str(&format!(
            "  {:>10}  {}\n",
            format_bytes(f.bytes),
            escape_for_terminal(f.path.as_str())
        ));
    }
    emit_text(&text)
}

/// Export every table of the database in `dirs` to `out_dir` (created owner-only if missing;
/// existing files are never overwritten).
pub fn export(
    dirs: &AppDirs,
    format: ExportFormat,
    out_dir: &Path,
    now: i64,
) -> Result<DataExportReport, ScullError> {
    let db_path = db::db_path(&dirs.data);
    let store = open_db(&db_path).map_err(|e| {
        let r = not_opened_report(&db_path, &e);
        let why = match r.state {
            DbState::Missing | DbState::Empty => "nothing is recorded yet".to_owned(),
            _ => r.detail.unwrap_or_default(),
        };
        ScullError::new(
            ErrorKind::Config,
            format!("cannot export the database: {why}"),
        )
        .with_path(FsPath::from(db_path.as_path()))
    })?;
    let schema_version = store.schema_version().map_err(store_error)?;
    scull_platform::create_dir_owner_only(out_dir).map_err(|e| {
        ScullError::new(ErrorKind::Io, e.to_string()).with_path(FsPath::from(out_dir))
    })?;
    let mut files = Vec::new();
    match format {
        ExportFormat::Json => {
            let path = out_dir.join(JSON_EXPORT_FILE);
            let rows = write_json_dump(&store, &db_path, &path, schema_version, now)?;
            files.push(exported(&path, None, rows));
        }
        ExportFormat::Csv => {
            for table in scull_storage::EXPORT_TABLES {
                cancel::check()?;
                let path = out_dir.join(format!("{table}.csv"));
                let mut w = create_new(&path)?;
                let mut sink = CsvSink { out: &mut w };
                let rows = store.export_table(table, &mut sink).map_err(store_error)?;
                w.flush().map_err(|e| io_error(&path, &e))?;
                drop(w);
                files.push(exported(&path, Some((*table).to_owned()), rows));
            }
        }
    }
    let rows = files.iter().map(|f| f.rows).sum();
    Ok(DataExportReport {
        out_dir: FsPath::from(out_dir),
        format,
        database: FsPath::from(db_path.as_path()),
        schema_version,
        files,
        rows,
    })
}

fn exported(path: &Path, table: Option<String>, rows: u64) -> ExportedFile {
    ExportedFile {
        path: FsPath::from(path),
        table,
        rows,
        bytes: std::fs::metadata(path).map_or(0, |m| m.len()),
    }
}

/// Exclusively create an owner-only file (never follows or overwrites anything at `path`).
fn create_new(path: &Path) -> Result<BufWriter<std::fs::File>, ScullError> {
    scull_platform::owner_only::create_file(path)
        .map(BufWriter::new)
        .map_err(|e| {
            let msg = if e.kind() == io::ErrorKind::AlreadyExists {
                "refusing to overwrite an existing file".to_owned()
            } else {
                e.to_string()
            };
            ScullError::new(ErrorKind::from(e.kind()), msg).with_path(FsPath::from(path))
        })
}

fn io_error(path: &Path, e: &io::Error) -> ScullError {
    ScullError::from_io(e).with_path(FsPath::from(path))
}

fn store_error(e: StoreError) -> ScullError {
    ScullError::new(e.kind(), e.to_string())
}

fn write_json_dump(
    store: &Store,
    db_path: &Path,
    path: &Path,
    schema_version: u32,
    now: i64,
) -> Result<u64, ScullError> {
    let mut w = create_new(path)?;
    let ioe = |e: io::Error| io_error(path, &e);
    let head = serde_json::json!({
        "schema": DATA_DUMP_SCHEMA.to_string(),
        "version": SCULL_VERSION,
    });
    // `{"schema":…,"version":…` then `,"data":{…,"tables":[` streamed table by table.
    let head = serde_json::to_string(&head).map_err(|e| internal(&e))?;
    let head = head.trim_end_matches('}');
    let meta = serde_json::json!({
        "exported_at": now,
        "database": FsPath::from(db_path),
        "schema_version": schema_version,
    });
    let meta = serde_json::to_string(&meta).map_err(|e| internal(&e))?;
    let meta = meta.trim_end_matches('}');
    writeln!(w, "{head},\"data\":{meta},\"tables\":[").map_err(ioe)?;
    let mut total = 0u64;
    for (i, table) in scull_storage::EXPORT_TABLES.iter().enumerate() {
        cancel::check()?;
        if i > 0 {
            w.write_all(b",\n").map_err(ioe)?;
        }
        let mut sink = JsonSink {
            out: &mut w,
            table,
            columns: Vec::new(),
            first: true,
        };
        total += store.export_table(table, &mut sink).map_err(store_error)?;
        w.write_all(b"]}").map_err(ioe)?;
    }
    w.write_all(b"\n]}}\n").map_err(ioe)?;
    w.flush().map_err(ioe)?;
    Ok(total)
}

fn internal(e: &serde_json::Error) -> ScullError {
    ScullError::new(ErrorKind::Internal, e.to_string())
}

/// Streams one table as `{"table":…,"columns":[…],"rows":[{…},…` (the caller closes it).
struct JsonSink<'a, W: Write> {
    out: &'a mut W,
    table: &'a str,
    columns: Vec<String>,
    first: bool,
}

impl<W: Write> ExportSink for JsonSink<'_, W> {
    fn columns(&mut self, names: &[String]) -> io::Result<()> {
        self.columns = names.to_vec();
        write!(
            self.out,
            "{{\"table\":{},\"columns\":{},\"rows\":[",
            serde_json::to_string(self.table)?,
            serde_json::to_string(names)?
        )
    }

    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        let obj: serde_json::Map<String, Value> = self
            .columns
            .iter()
            .cloned()
            .zip(values.iter().cloned())
            .collect();
        if !self.first {
            self.out.write_all(b",")?;
        }
        self.first = false;
        self.out.write_all(b"\n")?;
        serde_json::to_writer(&mut *self.out, &obj)?;
        Ok(())
    }
}

/// Writes one table as RFC 4180 CSV with a header row.
struct CsvSink<'a, W: Write> {
    out: &'a mut W,
}

impl<W: Write> CsvSink<'_, W> {
    fn record<'v>(
        &mut self,
        cells: impl Iterator<Item = std::borrow::Cow<'v, str>>,
    ) -> io::Result<()> {
        let line: Vec<String> = cells.map(|c| csv_cell(&c)).collect();
        self.out.write_all(line.join(",").as_bytes())?;
        self.out.write_all(b"\r\n")
    }
}

impl<W: Write> ExportSink for CsvSink<'_, W> {
    fn columns(&mut self, names: &[String]) -> io::Result<()> {
        self.record(names.iter().map(|n| std::borrow::Cow::Borrowed(n.as_str())))
    }

    fn row(&mut self, values: &[Value]) -> io::Result<()> {
        self.record(values.iter().map(|v| match v {
            Value::Null => std::borrow::Cow::Borrowed(""),
            Value::String(s) => std::borrow::Cow::Borrowed(s.as_str()),
            other => std::borrow::Cow::Owned(other.to_string()),
        }))
    }
}

/// One CSV cell: quoted when it holds a comma, quote, CR or LF (quotes doubled).
pub fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

// ---------------------------------------------------------------------------------------------
// purge
// ---------------------------------------------------------------------------------------------

/// What to purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeScope {
    /// Every file of scull's own footprint (FX-DATA-04).
    All,
    /// Migration backups and damaged copies last modified more than this many seconds ago.
    OlderThan(u64),
}

/// Options for [`purge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeOptions {
    /// What to purge.
    pub scope: PurgeScope,
    /// `--include-quarantine` (refused until #71).
    pub include_quarantine: bool,
}

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PurgeOutcome {
    /// Removed (a symlink is removed as a link; its target is untouched).
    Removed,
    /// Already gone when its turn came.
    AlreadyGone,
    /// Refused by the containment check and left in place.
    Refused,
    /// Removal failed.
    Failed,
    /// Not attempted: an earlier file aborted the purge (data folder changed, walk anomaly).
    NotAttempted,
}

/// One file considered by `scull data purge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PurgedFile {
    /// The file.
    pub file: OwnFile,
    /// What happened.
    pub outcome: PurgeOutcome,
    /// Why, when it was refused or failed.
    pub reason: Option<String>,
}

/// `scull data purge` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataPurgeReport {
    /// scull's directories.
    pub dirs: DirsReport,
    /// `all` or `older_than`.
    pub scope: String,
    /// For `older_than`: the age in seconds.
    pub older_than_secs: Option<u64>,
    /// Every file considered, with its outcome.
    pub files: Vec<PurgedFile>,
    /// Files removed.
    pub removed: u64,
    /// Bytes removed (logical sizes as listed).
    pub removed_bytes: u64,
    /// What was deliberately kept.
    pub kept: Vec<String>,
}

/// `scull data purge`.
pub fn run_purge(ui: &Ui, opts: &PurgeOptions, confirm: bool) -> Result<(), ScullError> {
    let dirs = app_dirs()?;
    let interactive = !confirm && !ui.format.is_machine() && io::stdin().is_terminal();
    if !confirm && !interactive {
        return Err(ScullError::new(
            ErrorKind::Usage,
            "scull data purge deletes files: pass --confirm (no terminal to ask on)",
        ));
    }
    let mut ask = |files: &[OwnFile]| -> Result<bool, ScullError> {
        if confirm {
            return Ok(true);
        }
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut err = io::stderr().lock();
        ask_confirmation(&mut input, &mut err, files).map_err(|e| ScullError::from_io(&e))
    };
    let report = purge(&dirs, opts, scull_engine::now_unix(), &mut ask)?;
    let failed = report
        .files
        .iter()
        .filter(|f| {
            matches!(
                f.outcome,
                PurgeOutcome::Refused | PurgeOutcome::Failed | PurgeOutcome::NotAttempted
            )
        })
        .count();
    if ui.format.is_machine() {
        emit_machine(ui.format, DATA_PURGE_SCHEMA, "data", &report)?;
    } else {
        emit_text(&render_purge(&report))?;
    }
    if failed > 0 {
        return Err(ScullError::new(
            ErrorKind::PartialFailure,
            format!("{failed} file(s) were not removed; see the report"),
        ));
    }
    Ok(())
}

/// Show `files` on `err` and read the confirmation word from `input`. Anything but
/// [`CONFIRM_WORD`] declines.
pub fn ask_confirmation(
    input: &mut dyn BufRead,
    err: &mut dyn Write,
    files: &[OwnFile],
) -> io::Result<bool> {
    let total = files.iter().fold(0u64, |a, f| a.saturating_add(f.bytes));
    writeln!(
        err,
        "scull data purge will delete {} of scull's own files ({}):",
        files.len(),
        format_bytes(total)
    )?;
    for f in files {
        writeln!(err, "  {}", escape_for_terminal(f.path.as_str()))?;
    }
    write!(err, "Type `{CONFIRM_WORD}` to delete them: ")?;
    err.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim() == CONFIRM_WORD)
}

/// Plan, confirm (through `confirm`, given the files to delete) and run a purge of scull's
/// own files in `dirs`. Declining returns [`ErrorKind::Cancelled`] with nothing removed.
pub fn purge(
    dirs: &AppDirs,
    opts: &PurgeOptions,
    now: i64,
    confirm: &mut dyn FnMut(&[OwnFile]) -> Result<bool, ScullError>,
) -> Result<DataPurgeReport, ScullError> {
    if opts.include_quarantine {
        return Err(blocked(
            "--include-quarantine is refused: quarantine contents are only ever removed by the \
             Executor's quarantine purge (#71), with its own confirmation: use `scull clean \
             --purge-quarantine`. Nothing was deleted.",
        ));
    }
    let native = Native::new();
    let data = &dirs.data;
    let data_md = match native.metadata(data) {
        Ok(md) => Some(md),
        Err(e) if e.is_not_found() => None,
        Err(e) => {
            return Err(ScullError::new(ErrorKind::Io, e.to_string())
                .with_path(FsPath::from(data.as_path())));
        }
    };
    if let Some(md) = &data_md
        && md.kind != EntryKind::Dir
    {
        return Err(blocked(&format!(
            "scull's data folder {} is a {}, not a real folder; refusing to delete through it",
            data.display(),
            md.kind.as_str()
        )));
    }
    if opts.scope == PurgeScope::All {
        check_database_for_purge(&db::db_path(data))?;
    }

    let mut targets = if data_md.is_some() {
        own_files(data)
    } else {
        Vec::new()
    };
    if let PurgeScope::OlderThan(secs) = opts.scope {
        let cutoff = now.saturating_sub(i64::try_from(secs).unwrap_or(i64::MAX));
        targets.retain(|f| {
            let copy = matches!(
                f.kind,
                FileKind::Backup | FileKind::Damaged | FileKind::SideFile
            );
            copy && f.modified_at.is_some_and(|m| m < cutoff)
        });
    }

    let mut report = DataPurgeReport {
        dirs: DirsReport::new(dirs),
        scope: match opts.scope {
            PurgeScope::All => "all",
            PurgeScope::OlderThan(_) => "older_than",
        }
        .to_owned(),
        older_than_secs: match opts.scope {
            PurgeScope::All => None,
            PurgeScope::OlderThan(s) => Some(s),
        },
        files: Vec::new(),
        removed: 0,
        removed_bytes: 0,
        kept: vec![
            format!("configuration in {}", dirs.config.display()),
            "quarantine contents (never touched by this command)".to_owned(),
        ],
    };
    if targets.is_empty() {
        return Ok(report);
    }
    let Some(data_identity) = data_md.as_ref().and_then(|m| m.identity) else {
        return Err(blocked(
            "this OS reports no identity for scull's data folder; refusing to delete",
        ));
    };

    // Plan: containment, kind and identity of every file, captured now and enforced by the
    // cleaner at removal (anchored at the data folder with the identity seen above).
    let mut planned: Vec<(OwnFile, Planned)> = Vec::with_capacity(targets.len());
    for file in targets {
        let path = file.path.to_path_buf();
        let plan = match verify_inside(&native, data, Some(&data_identity), &path) {
            Err(Gone) if !exists_no_follow(&path) => Err((PurgeOutcome::AlreadyGone, None)),
            Err(Gone) => Err((
                PurgeOutcome::Refused,
                Some("could not be checked".to_owned()),
            )),
            Err(Refusal(why)) => Err((PurgeOutcome::Refused, Some(why))),
            Ok(kind) => match (
                native.metadata(&path).ok().and_then(|m| m.identity),
                path.strip_prefix(data),
            ) {
                (Some(expected), Ok(rel)) => Ok(OwnFileTarget {
                    relative: rel.to_path_buf(),
                    expected,
                    kind,
                }),
                _ => Err((
                    PurgeOutcome::Refused,
                    Some("its identity could not be read".to_owned()),
                )),
            },
        };
        planned.push((file, plan));
    }
    let to_delete: Vec<OwnFile> = planned
        .iter()
        .filter(|(_, p)| p.is_ok())
        .map(|(f, _)| f.clone())
        .collect();
    if !to_delete.is_empty() && !confirm(&to_delete)? {
        return Err(ScullError::new(
            ErrorKind::Cancelled,
            "purge declined; nothing was deleted",
        ));
    }
    cancel::check()?;
    let requests: Vec<OwnFileTarget> = planned
        .iter()
        .filter_map(|(_, p)| p.as_ref().ok().cloned())
        .collect();
    // The single deletion path (SAFE-10): fd-relative, no-follow, identity-checked.
    let removal = purge_own_files(data, data_identity, &requests)?;
    let mut results = removal.files.into_iter();
    for (file, plan) in planned {
        let (outcome, reason) = match plan {
            Err(r) => r,
            Ok(_) => match results.next() {
                Some(r) => match r.outcome {
                    OwnFileOutcome::Removed => (PurgeOutcome::Removed, None),
                    OwnFileOutcome::AlreadyGone => (PurgeOutcome::AlreadyGone, None),
                    OwnFileOutcome::Refused => (PurgeOutcome::Refused, r.reason),
                    OwnFileOutcome::NotAttempted => {
                        (PurgeOutcome::NotAttempted, removal.aborted.clone())
                    }
                    _ => (PurgeOutcome::Failed, r.reason),
                },
                None => (PurgeOutcome::NotAttempted, None),
            },
        };
        if outcome == PurgeOutcome::Removed {
            report.removed += 1;
            report.removed_bytes = report.removed_bytes.saturating_add(file.bytes);
        }
        report.files.push(PurgedFile {
            file,
            outcome,
            reason,
        });
    }
    Ok(report)
}

/// A planned removal, or why a file won't be removed.
type Planned = Result<OwnFileTarget, (PurgeOutcome, Option<String>)>;

/// Why [`verify_inside`] did not clear a path.
#[derive(Debug)]
enum Unverified {
    /// The path (or a folder on the way) no longer exists.
    Gone,
    /// It is not safe to remove; the reason.
    Refusal(String),
}
use Unverified::{Gone, Refusal};

/// Clear `path` for removal: it must be strictly inside `data` (lexically, with no `..`),
/// every folder between them must be a real folder, `data` must still be a real folder with
/// `identity`, and `path` itself must be a regular file or a symlink (never followed).
fn verify_inside(
    native: &Native,
    data: &Path,
    identity: Option<&scull_core::FileIdentity>,
    path: &Path,
) -> Result<EntryKind, Unverified> {
    let Ok(rel) = path.strip_prefix(data) else {
        return Err(Refusal(format!(
            "outside scull's data folder {}",
            data.display()
        )));
    };
    let comps: Vec<Component<'_>> = rel.components().collect();
    if comps.is_empty() || comps.iter().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(Refusal(
            "not a plain path inside scull's data folder".to_owned(),
        ));
    }
    let md = |p: &Path| match native.metadata(p) {
        Ok(m) => Ok(m),
        Err(e) if e.is_not_found() => Err(Gone),
        Err(e) => Err(Refusal(e.to_string())),
    };
    let top = md(data)?;
    if top.kind != EntryKind::Dir || top.identity.as_ref() != identity {
        return Err(Refusal(
            "scull's data folder changed while purging".to_owned(),
        ));
    }
    let mut cur = data.to_path_buf();
    for c in &comps[..comps.len() - 1] {
        cur.push(c);
        let m = md(&cur)?;
        if m.kind != EntryKind::Dir {
            return Err(Refusal(format!(
                "{} is a {}, not a real folder",
                cur.display(),
                m.kind.as_str()
            )));
        }
    }
    let leaf = md(path)?;
    match leaf.kind {
        EntryKind::File | EntryKind::Symlink => Ok(leaf.kind),
        k => Err(Refusal(format!(
            "is a {}, not a file; left in place",
            k.as_str()
        ))),
    }
}

/// `--all` deletes the database: refuse unless it is absent, empty, or a scull database
/// whose quarantine manifest has nothing outstanding.
fn check_database_for_purge(db_path: &Path) -> Result<(), ScullError> {
    let probe = db::probe(db_path).map_err(|e| io_error(db_path, &e))?;
    let refuse = |why: &str| {
        Err(blocked(&format!(
            "{why}; scull can't check that nothing is in quarantine, so it deletes nothing. {}",
            db_path.display()
        )))
    };
    let store = match probe {
        Probe::Missing | Probe::Uninitialized => return Ok(()),
        Probe::NotAFile => return refuse("the database is not a regular file"),
        Probe::NotSqlite => {
            return refuse("the database file is not a SQLite database (move it away yourself)");
        }
        Probe::Foreign { .. } => return refuse("the database file belongs to another application"),
        Probe::Scull { version } if version > SCHEMA_VERSION => {
            return refuse("the database was written by a newer scull (purge with that version)");
        }
        // Older schema: open it the normal way (migrates, with a backup) to read the manifest.
        Probe::Scull { version } if version < SCHEMA_VERSION => {
            if let Some(why) = unsafe_side_file(db_path) {
                return refuse(&why);
            }
            scull_engine::open_history(db_path.parent().unwrap_or(Path::new(".")))
                .map_err(|e| store_refusal(db_path, &e))?
        }
        Probe::Scull { .. } => open_db(db_path).map_err(|e| match e {
            NotOpened::Store(e) => store_refusal(db_path, &e),
            NotOpened::Io(e) => blocked(&format!(
                "{e}; scull can't check that nothing is in quarantine, so it deletes nothing"
            )),
            NotOpened::Probe(_) => blocked("the database changed while purging"),
        })?,
    };
    let outstanding = store
        .quarantine_outstanding()
        .map_err(|e| store_refusal(db_path, &e))?;
    drop(store);
    if outstanding.is_empty() {
        return Ok(());
    }
    let q = quarantine_report(&outstanding);
    let folders: Vec<String> = q.folders.iter().map(|f| f.as_str().to_owned()).collect();
    Err(blocked(&format!(
        "{} item(s) ({}) are still in quarantine in {}; deleting the database would lose the \
         only record of where they came from. Restore or purge them first (scull restore, \
         #81); quarantine purge arrives with #71. Nothing was deleted.",
        q.outstanding_items,
        format_bytes(q.outstanding_bytes),
        folders.join(", ")
    )))
}

fn store_refusal(db_path: &Path, e: &StoreError) -> ScullError {
    blocked(&format!(
        "the database can't be read ({e}); scull can't check that nothing is in quarantine, \
         so it deletes nothing. Restore a backup first (scull doctor). {}",
        db_path.display()
    ))
}

fn blocked(msg: &str) -> ScullError {
    ScullError::new(ErrorKind::SafetyBlocked, msg)
}

fn render_purge(r: &DataPurgeReport) -> String {
    if r.files.is_empty() {
        return "Nothing to purge: scull has no files to delete here.\n".to_owned();
    }
    let mut out = format!(
        "Removed {} of {} files ({}).\n",
        r.removed,
        r.files.len(),
        format_bytes(r.removed_bytes)
    );
    for f in &r.files {
        let what = super::json_name(&f.outcome);
        out.push_str(&format!(
            "  {:<12}  {}{}\n",
            what,
            escape_for_terminal(f.file.path.as_str()),
            f.reason
                .as_deref()
                .map(|r| format!(" ({})", escape_for_terminal(r)))
                .unwrap_or_default()
        ));
    }
    for k in &r.kept {
        out.push_str(&format!("Kept: {}\n", escape_for_terminal(k)));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn app_dirs() -> Result<AppDirs, ScullError> {
    scull_platform::app_dirs().map_err(|e| ScullError::new(ErrorKind::Config, e.to_string()))
}

/// Parse `--older-than`: a positive whole number with a unit, `m` (minutes), `h`, `d` or `w`.
pub fn parse_age(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let mult: u64 = match unit {
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => {
            return Err(format!(
                "`{s}`: expected a number and a unit (m, h, d, w), e.g. 30d"
            ));
        }
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("`{s}`: expected a number and a unit (m, h, d, w), e.g. 30d"))?;
    if n == 0 {
        return Err(format!("`{s}`: the age must be greater than zero"));
    }
    n.checked_mul(mult)
        .ok_or_else(|| format!("`{s}`: too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_parse() {
        assert_eq!(parse_age("90m"), Ok(5_400));
        assert_eq!(parse_age("12h"), Ok(43_200));
        assert_eq!(parse_age("30d"), Ok(2_592_000));
        assert_eq!(parse_age("2w"), Ok(1_209_600));
        for bad in [
            "",
            "d",
            "0d",
            "5",
            "5y",
            "-1d",
            "1.5d",
            "99999999999999999999w",
        ] {
            assert!(parse_age(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn csv_cells_are_quoted_only_when_needed() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("two\nlines"), "\"two\nlines\"");
        assert_eq!(csv_cell("=1+1"), "=1+1");
    }

    #[test]
    fn footprint_names_are_classified() {
        let d = Path::new("/d");
        let db = scull_storage::DB_FILE_NAME;
        let k = |n: &str| classify(&d.join(n));
        assert_eq!(k(db), FileKind::Database);
        assert_eq!(k(&format!("{db}-wal")), FileKind::Wal);
        assert_eq!(k(&format!("{db}-shm")), FileKind::Shm);
        assert_eq!(
            k(&format!("{db}.backup-20260101T000000Z-v1")),
            FileKind::Backup
        );
        assert_eq!(
            k(&format!("{db}.backup-20260101T000000Z-v1-wal")),
            FileKind::SideFile
        );
        assert_eq!(
            k(&format!("{db}.damaged-20260101T000000Z")),
            FileKind::Damaged
        );
        assert_eq!(k(scull_mcp::SALT_FILE), FileKind::Salt);
        assert_eq!(
            classify(&d.join("audit").join("op-1.ndjson")),
            FileKind::AuditMirror
        );
        assert_eq!(k("notes.txt"), FileKind::Other);
    }

    #[test]
    fn confirmation_needs_the_exact_word() {
        let files = [];
        for (input, ok) in [
            ("purge\n", true),
            ("  purge  \n", true),
            ("y\n", false),
            ("yes\n", false),
            ("PURGE\n", false),
            ("", false),
        ] {
            let mut err = Vec::new();
            let got = ask_confirmation(&mut input.as_bytes(), &mut err, &files).expect("io");
            assert_eq!(got, ok, "{input:?}");
            assert!(
                String::from_utf8(err)
                    .expect("utf-8")
                    .contains("Type `purge`")
            );
        }
    }
}
