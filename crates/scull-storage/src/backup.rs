//! Backups taken before migrations (DATA-03) and restore after corruption (FX-DATA-06).
//!
//! Backups are written next to the database as
//! `<db file name>.backup-<UTC YYYYMMDDTHHMMSSZ>-v<schema version>[-<n>]` with the SQLite online
//! backup API, so they are consistent even while other scull processes read the database.

use crate::error::{Result, StoreError};
use crate::perms::OwnerOnly;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BACKUP_MARKER: &str = ".backup-";
const DAMAGED_MARKER: &str = ".damaged-";

/// Backups of the database at `db_path`, newest first. Missing directory → empty list.
pub fn list_backups(db_path: &Path) -> Vec<PathBuf> {
    list_siblings(db_path, BACKUP_MARKER)
}

/// Damaged databases set aside by [`restore_backup`], newest first.
pub(crate) fn list_damaged(db_path: &Path) -> Vec<PathBuf> {
    list_siblings(db_path, DAMAGED_MARKER)
}

fn list_siblings(db_path: &Path, marker: &str) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (parent_dir(db_path), file_name(db_path)) else {
        return Vec::new();
    };
    let prefix = format!("{name}{marker}");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_owned();
            // Skip SQLite side files of a backup (`-journal`, `-wal`, `-shm`).
            let is_backup = n.starts_with(&prefix)
                && !n.ends_with("-journal")
                && !n.ends_with("-wal")
                && !n.ends_with("-shm");
            is_backup.then(|| (n, e.path()))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Copy the database open on `src` to a new owner-only backup file next to `db_path`.
/// `schema_version` is the version being backed up (recorded in the name).
pub(crate) fn backup_to_sibling(
    src: &Connection,
    db_path: &Path,
    schema_version: u32,
    perms: &dyn OwnerOnly,
) -> Result<PathBuf> {
    let stem = format!("{BACKUP_MARKER}{}-v{schema_version}", utc_stamp(now_unix()));
    let dest = create_unique_sibling(db_path, &stem, perms)?;
    let mut dst = Connection::open(&dest)?;
    rusqlite::backup::Backup::new(src, &mut dst)?.run_to_completion(256, Duration::ZERO, None)?;
    dst.close().map_err(|(_, e)| StoreError::from(e))?;
    Ok(dest)
}

/// Replace a damaged database with a backup (FX-DATA-06).
///
/// Nothing is deleted: the damaged database and its `-wal`/`-shm` files are renamed to
/// `<name>.damaged-<UTC stamp>[-n]` (plus the same suffixes) before the backup is copied in with
/// the SQLite backup API. Returns the new path of the damaged database file. The backup must
/// pass `PRAGMA quick_check`; it is never modified. Do not call while a [`crate::Store`] is open
/// on `db_path`.
pub fn restore_backup(db_path: &Path, backup: &Path, perms: &dyn OwnerOnly) -> Result<PathBuf> {
    let src = Connection::open_with_flags(
        backup,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let check: String = src
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| {
            StoreError::rejected(
                scull_core::ErrorKind::Io,
                format!("backup {} is unusable: {e}", backup.display()),
            )
        })?;
    if check != "ok" {
        return Err(StoreError::rejected(
            scull_core::ErrorKind::Io,
            format!(
                "backup {} failed its integrity check: {check}",
                backup.display()
            ),
        ));
    }

    let stem = format!("{DAMAGED_MARKER}{}", utc_stamp(now_unix()));
    let moved = unique_sibling(db_path, &stem)?;
    std::fs::rename(db_path, &moved).map_err(|e| StoreError::io(db_path, e))?;
    for suffix in ["-wal", "-shm"] {
        let side = with_suffix(db_path, suffix);
        if side.exists() {
            let to = with_suffix(&moved, suffix);
            std::fs::rename(&side, &to).map_err(|e| StoreError::io(&side, e))?;
        }
    }

    perms
        .create_file(db_path)
        .map_err(|source| StoreError::Permissions {
            path: db_path.to_owned(),
            source,
        })?;
    let mut dst = Connection::open(db_path)?;
    rusqlite::backup::Backup::new(&src, &mut dst)?.run_to_completion(256, Duration::ZERO, None)?;
    dst.close().map_err(|(_, e)| StoreError::from(e))?;
    Ok(moved)
}

/// `path` with `suffix` appended to its file name (`scull.db` → `scull.db-wal`).
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn parent_dir(path: &Path) -> Option<PathBuf> {
    match path.parent() {
        Some(p) if p.as_os_str().is_empty() => Some(PathBuf::from(".")),
        Some(p) => Some(p.to_owned()),
        None => None,
    }
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_owned)
}

/// First free `<db_path><stem>[-n]`, without creating it.
fn unique_sibling(db_path: &Path, stem: &str) -> Result<PathBuf> {
    for n in 1..1000u32 {
        let suffix = if n == 1 {
            stem.to_owned()
        } else {
            format!("{stem}-{n}")
        };
        let candidate = with_suffix(db_path, &suffix);
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err(StoreError::rejected(
        scull_core::ErrorKind::Io,
        format!("too many files named like {}{stem}", db_path.display()),
    ))
}

/// Exclusively create an owner-only `<db_path><stem>[-n]`.
fn create_unique_sibling(db_path: &Path, stem: &str, perms: &dyn OwnerOnly) -> Result<PathBuf> {
    for _ in 0..1000u32 {
        let candidate = unique_sibling(db_path, stem)?;
        match perms.create_file(&candidate) {
            Ok(()) => return Ok(candidate),
            // Lost a race with another process; pick the next name.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(StoreError::Permissions {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Err(StoreError::rejected(
        scull_core::ErrorKind::Io,
        format!("cannot create a backup next to {}", db_path.display()),
    ))
}

fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// `YYYYMMDDTHHMMSSZ` for a Unix time (proleptic Gregorian, UTC).
pub(crate) fn utc_stamp(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}
