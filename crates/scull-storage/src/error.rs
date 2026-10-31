//! Store errors and their mapping to the shared [`ScullError`] model.

use scull_core::{ErrorKind, FsPath, ScullError};
use std::fmt;
use std::path::PathBuf;

/// Result alias for this crate.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Everything that can go wrong opening or using the store.
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// The database was written by a newer scull (DATA-03). The file was not modified.
    SchemaTooNew {
        /// Database file.
        path: PathBuf,
        /// `PRAGMA user_version` found in the file.
        found: u32,
        /// Newest schema this binary knows ([`crate::SCHEMA_VERSION`]).
        supported: u32,
    },
    /// The file is a SQLite database that belongs to another application.
    NotScullDatabase {
        /// Database file.
        path: PathBuf,
        /// `PRAGMA application_id` found in the file.
        application_id: i32,
    },
    /// The database file is damaged (FX-DATA-06). The file is left untouched; callers should
    /// offer [`crate::restore_backup`] with one of `backups` and keep read-only scans working
    /// without history.
    Corrupt {
        /// Database file.
        path: PathBuf,
        /// What SQLite reported.
        detail: String,
        /// Backups found next to the database, newest first.
        backups: Vec<PathBuf>,
    },
    /// Another scull process held the write lock for longer than the busy timeout.
    Busy {
        /// What SQLite reported.
        detail: String,
    },
    /// The owner-only permission step failed (DATA-04).
    Permissions {
        /// File or directory being restricted.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// Filesystem error outside SQLite.
    Io {
        /// File or directory involved.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// A request that the store refuses on purpose: wrong hash, duplicate append-only record,
    /// approval past the plan's expiry, unknown plan and so on.
    Rejected {
        /// Shared error kind (e.g. `plan_tampered`, `usage`, `not_found`).
        kind: ErrorKind,
        /// Human-readable reason.
        message: String,
    },
    /// Any other SQLite error.
    Sqlite(rusqlite::Error),
}

impl StoreError {
    /// The shared error kind for this error.
    pub fn kind(&self) -> ErrorKind {
        match self {
            StoreError::SchemaTooNew { .. } | StoreError::NotScullDatabase { .. } => {
                ErrorKind::Config
            }
            StoreError::Corrupt { .. } => ErrorKind::Io,
            StoreError::Busy { .. } => ErrorKind::LockHeld,
            StoreError::Permissions { source, .. } | StoreError::Io { source, .. } => {
                source.kind().into()
            }
            StoreError::Rejected { kind, .. } => *kind,
            StoreError::Sqlite(_) => ErrorKind::Internal,
        }
    }

    pub(crate) fn rejected(kind: ErrorKind, message: impl Into<String>) -> Self {
        StoreError::Rejected {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        StoreError::Io {
            path: path.into(),
            source,
        }
    }

    fn path(&self) -> Option<&PathBuf> {
        match self {
            StoreError::SchemaTooNew { path, .. }
            | StoreError::NotScullDatabase { path, .. }
            | StoreError::Corrupt { path, .. }
            | StoreError::Permissions { path, .. }
            | StoreError::Io { path, .. } => Some(path),
            _ => None,
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::SchemaTooNew {
                path,
                found,
                supported,
            } => write!(
                f,
                "database {} uses schema version {found}, but this scull only knows up to \
                 {supported}; it was written by a newer scull. Upgrade scull; the database was \
                 not modified",
                path.display()
            ),
            StoreError::NotScullDatabase {
                path,
                application_id,
            } => write!(
                f,
                "{} is not a scull database (application id {application_id:#x}); refusing to \
                 use it",
                path.display()
            ),
            StoreError::Corrupt {
                path,
                detail,
                backups,
            } => {
                write!(f, "database {} is damaged: {detail}", path.display())?;
                match backups.first() {
                    Some(b) => write!(
                        f,
                        "; {} backup(s) available, newest {}",
                        backups.len(),
                        b.display()
                    ),
                    None => write!(f, "; no backup is available"),
                }
            }
            StoreError::Busy { detail } => write!(
                f,
                "the database is busy (another scull is writing): {detail}"
            ),
            StoreError::Permissions { path, source } => write!(
                f,
                "cannot restrict {} to the current user: {source}",
                path.display()
            ),
            StoreError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            StoreError::Rejected { message, .. } => f.write_str(message),
            StoreError::Sqlite(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Permissions { source, .. } | StoreError::Io { source, .. } => Some(source),
            StoreError::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode;
        match e.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => StoreError::Busy {
                detail: e.to_string(),
            },
            _ => StoreError::Sqlite(e),
        }
    }
}

impl From<StoreError> for ScullError {
    fn from(e: StoreError) -> Self {
        let kind = e.kind();
        let path = e.path().map(|p| FsPath::from_path(p));
        let err = ScullError::new(kind, e.to_string());
        match path {
            Some(p) => err.with_path(p),
            None => err,
        }
    }
}

/// True if SQLite reports the file as damaged or not a database at all.
pub(crate) fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}
