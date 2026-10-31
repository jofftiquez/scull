//! Versioned, embedded schema migrations tracked with `PRAGMA user_version` (DATA-03).

use crate::backup::backup_to_sibling;
use crate::error::{Result, StoreError};
use crate::perms::OwnerOnly;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use std::path::{Path, PathBuf};

/// One embedded migration: SQL, then an optional Rust step for what SQL cannot do (hashing),
/// both inside the same transaction.
#[derive(Clone, Copy)]
pub(crate) struct Migration {
    /// SQL run with `execute_batch`.
    pub(crate) sql: &'static str,
    /// Runs after `sql`, before `user_version` is bumped.
    pub(crate) post: Option<fn(&Transaction<'_>) -> Result<()>>,
}

impl Migration {
    /// A pure SQL migration.
    pub(crate) const fn sql(sql: &'static str) -> Self {
        Self { sql, post: None }
    }

    /// SQL followed by a Rust step.
    pub(crate) const fn with_post(
        sql: &'static str,
        post: fn(&Transaction<'_>) -> Result<()>,
    ) -> Self {
        Self {
            sql,
            post: Some(post),
        }
    }
}

/// Embedded migrations; `MIGRATIONS[n]` upgrades schema version `n` to `n + 1`.
/// Never edit a released migration: append a new one.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration::sql(include_str!("migrations/v001_initial.sql")),
    Migration::sql(include_str!("migrations/v002_history.sql")),
    Migration::with_post(
        include_str!("migrations/v003_audit.sql"),
        crate::audit::migrate_v3_legacy,
    ),
    Migration::sql(include_str!("migrations/v004_quarantine.sql")),
    Migration::sql(include_str!("migrations/v005_quarantine_log.sql")),
    Migration::sql(include_str!("migrations/v006_journal.sql")),
];

/// Newest schema version this binary knows. A database with a higher `user_version` is refused.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// `PRAGMA application_id` of a scull store (`0x5343554C`, "SCUL").
pub const APPLICATION_ID: i32 = 0x5343_554C;

pub(crate) fn user_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
}

pub(crate) fn application_id(conn: &Connection) -> rusqlite::Result<i32> {
    conn.pragma_query_value(None, "application_id", |r| r.get(0))
}

pub(crate) fn has_schema_objects(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT count(*) > 0 FROM sqlite_schema", [], |r| r.get(0))
}

/// Refuse databases this binary must not touch. Called read-only, before anything is written.
pub(crate) fn check_compatible(
    conn: &Connection,
    path: &Path,
    known: u32,
) -> std::result::Result<(), StoreError> {
    let app = application_id(conn)?;
    let version = user_version(conn)?;
    if version > known {
        return Err(StoreError::SchemaTooNew {
            path: path.to_owned(),
            found: version,
            supported: known,
        });
    }
    let foreign_tables = version == 0 && app != APPLICATION_ID && has_schema_objects(conn)?;
    if (app != 0 && app != APPLICATION_ID) || foreign_tables {
        return Err(StoreError::NotScullDatabase {
            path: path.to_owned(),
            application_id: app,
        });
    }
    Ok(())
}

/// Apply pending migrations one at a time, each in its own `BEGIN IMMEDIATE` transaction.
///
/// Before each migration of a database that already has a schema (version ≥ 1), a consistent
/// copy is written next to it with the online backup API through a separate read-only
/// connection (WAL lets it read while this connection holds the write lock, so the copy is
/// exactly the state being migrated). A new, empty database needs no backup, including between
/// the migrations that create it. Returns the backups taken.
pub(crate) fn migrate(
    conn: &mut Connection,
    path: &Path,
    migrations: &[Migration],
    perms: &dyn OwnerOnly,
) -> Result<Vec<PathBuf>> {
    let target = u32::try_from(migrations.len()).unwrap_or(u32::MAX);
    let mut backups = Vec::new();
    // Fast path without the write lock, so opening never waits for another process's write
    // transaction when there is nothing to migrate (readers proceed, FX-DATA-05).
    check_compatible(conn, path, target)?;
    if user_version(conn)? == target {
        return Ok(backups);
    }
    // Whether the schema existed before this open. A database this open created from scratch
    // has nothing worth backing up between its own migrations.
    let mut had_schema: Option<bool> = None;
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Re-check under the write lock: another process may have migrated meanwhile.
        check_compatible(&tx, path, target)?;
        let version = user_version(&tx)?;
        if version == target {
            tx.commit()?;
            return Ok(backups);
        }
        if *had_schema.get_or_insert(version > 0) {
            let src = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            backups.push(backup_to_sibling(&src, path, version, perms)?);
        }
        let m = migrations[version as usize];
        tx.execute_batch(m.sql)?;
        if let Some(post) = m.post {
            post(&tx)?;
        }
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
    }
}
