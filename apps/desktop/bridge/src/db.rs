//! What scull's database file looks like, without opening it with SQLite: shared by the
//! `history` and `settings` commands, which must never create, migrate or write it.
//!
//! The header fields read are SQLite's documented ones: the 16-byte magic string, the
//! `user_version` at offset 60 and the `application_id` at offset 68, both big-endian (same
//! approach as `scull_cli::db::probe`, reimplemented because the bridge must not depend on the
//! CLI crate; SAFE-10). The entry's kind comes from the platform's no-follow metadata, so a link
//! at the path is reported, never followed.

use schemars::JsonSchema;
use scull_platform::{EntryKind, Native, Platform};
use scull_storage::{APPLICATION_ID, SCHEMA_VERSION};
use serde::Serialize;
use std::io::Read;
use std::path::Path;

/// How the database file looks (the same states as `scull data show`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DbState {
    /// A scull database with the schema this build uses.
    Ok,
    /// No file (nothing was ever recorded).
    Missing,
    /// An empty file, or a SQLite file with no schema yet.
    Empty,
    /// A newer scull wrote it; this build doesn't read it.
    NewerSchema,
    /// An older schema; the next `scull scan` in a terminal migrates it (with a backup).
    NeedsMigration,
    /// Not a scull database (another application's, or not SQLite).
    NotScull,
    /// It can't be read (permissions, not a regular file, I/O error).
    Unreadable,
}

/// The result of probing a database file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbProbe {
    /// How it looks.
    pub state: DbState,
    /// Its schema version, when it is a scull database.
    pub schema_version: Option<u32>,
    /// Why it isn't usable, when it isn't.
    pub detail: Option<String>,
}

impl DbProbe {
    fn of(state: DbState, schema_version: Option<u32>, detail: Option<String>) -> Self {
        Self {
            state,
            schema_version,
            detail,
        }
    }
}

const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Classify the first 100 bytes of a database file.
pub fn classify_header(header: &[u8]) -> DbProbe {
    if header.is_empty() {
        return DbProbe::of(DbState::Empty, None, None);
    }
    if header.len() < 100 || &header[..16] != MAGIC {
        return DbProbe::of(
            DbState::NotScull,
            None,
            Some("not a SQLite database".into()),
        );
    }
    let word = |at: usize| [header[at], header[at + 1], header[at + 2], header[at + 3]];
    let version = u32::from_be_bytes(word(60));
    let app = i32::from_be_bytes(word(68));
    match (app, version) {
        (0, 0) => DbProbe::of(DbState::Empty, None, None),
        (id, _) if id != APPLICATION_ID => DbProbe::of(
            DbState::NotScull,
            None,
            Some("not a scull database (another application's)".into()),
        ),
        (_, v) if v == SCHEMA_VERSION => DbProbe::of(DbState::Ok, Some(v), None),
        (_, v) if v > SCHEMA_VERSION => DbProbe::of(
            DbState::NewerSchema,
            Some(v),
            Some(format!(
                "schema version {v} (this build reads {SCHEMA_VERSION}); a newer scull wrote it"
            )),
        ),
        (_, v) => DbProbe::of(
            DbState::NeedsMigration,
            Some(v),
            Some(format!(
                "schema version {v} (this build reads {SCHEMA_VERSION}); run `scull status` in \
                 a terminal"
            )),
        ),
    }
}

/// Probe the database at `path` with a plain read of its header; never creates anything.
pub fn probe(path: &Path) -> DbProbe {
    match Native.metadata(path) {
        Ok(md) if md.kind == EntryKind::File => {}
        Ok(_) => {
            return DbProbe::of(DbState::Unreadable, None, Some("not a regular file".into()));
        }
        Err(e) if e.is_not_found() => return DbProbe::of(DbState::Missing, None, None),
        Err(e) => {
            return DbProbe::of(
                DbState::Unreadable,
                None,
                Some(format!("cannot read it: {e}")),
            );
        }
    }
    let mut header = Vec::with_capacity(100);
    match std::fs::File::open(path).and_then(|f| f.take(100).read_to_end(&mut header)) {
        Ok(_) => classify_header(&header),
        Err(e) => DbProbe::of(
            DbState::Unreadable,
            None,
            Some(format!("cannot read it: {e}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(version: u32, app: i32) -> Vec<u8> {
        let mut h = vec![0u8; 100];
        h[..16].copy_from_slice(MAGIC);
        h[60..64].copy_from_slice(&version.to_be_bytes());
        h[68..72].copy_from_slice(&app.to_be_bytes());
        h
    }

    #[test]
    fn headers_are_classified() {
        let st = |h: &[u8]| classify_header(h).state;
        assert_eq!(st(b""), DbState::Empty);
        assert_eq!(st(b"hello"), DbState::NotScull);
        assert_eq!(st(&header(0, 0)), DbState::Empty);
        assert_eq!(st(&header(SCHEMA_VERSION, APPLICATION_ID)), DbState::Ok);
        assert_eq!(
            st(&header(SCHEMA_VERSION + 1, APPLICATION_ID)),
            DbState::NewerSchema
        );
        assert_eq!(st(&header(1, 7)), DbState::NotScull);
        if SCHEMA_VERSION > 1 {
            let p = classify_header(&header(1, APPLICATION_ID));
            assert_eq!(p.state, DbState::NeedsMigration);
            assert_eq!(p.schema_version, Some(1));
        }
    }

    #[test]
    fn missing_file_is_missing_and_not_created() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let p = dir.path().join("scull.db");
        assert_eq!(probe(&p).state, DbState::Missing);
        assert!(!p.exists());
        assert_eq!(probe(dir.path()).state, DbState::Unreadable);
    }
}
