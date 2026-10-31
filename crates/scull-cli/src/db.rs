//! Read-mostly access to scull's database for commands that must never create or migrate it
//! (`scull doctor`, `scull status`).
//!
//! [`Store::open`] creates a missing database and migrates an old one. Diagnostic and
//! prompt commands must do neither, so they first [`probe`] the file header (plain file
//! read, no SQLite) and only open a database that already has the current schema, through
//! [`NoCreate`], an [`OwnerOnly`] that refuses to create anything and leaves permissions as
//! they are (doctor reports them instead of fixing them).
//!
//! The header fields read are SQLite's documented ones: the 16-byte magic string, the
//! `user_version` at offset 60 and the `application_id` at offset 68, both big-endian. With
//! uncheckpointed WAL content the header can lag behind the real schema version; the probe
//! then errs toward "needs migration", which only makes these commands do less.

use scull_storage::{APPLICATION_ID, OwnerOnly, SCHEMA_VERSION, Store};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// The database file inside scull's data directory.
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(scull_storage::DB_FILE_NAME)
}

/// What the database file looks like from its header, without opening it with SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Nothing at the path.
    Missing,
    /// Something other than a regular file at the path (directory, link, special file).
    NotAFile,
    /// An empty file, or a SQLite database with no schema yet (never used).
    Uninitialized,
    /// Not a SQLite database.
    NotSqlite,
    /// A SQLite database that belongs to another application.
    Foreign {
        /// The `application_id` found.
        application_id: i32,
    },
    /// A scull database with schema version `version` (compare with [`SCHEMA_VERSION`]).
    Scull {
        /// `PRAGMA user_version`, as stored in the header.
        version: u32,
    },
}

const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Classify a database header (the first 100 bytes, or fewer if the file is shorter).
pub fn classify_header(header: &[u8]) -> Probe {
    if header.is_empty() {
        return Probe::Uninitialized;
    }
    if header.len() < 100 || &header[..16] != MAGIC {
        return Probe::NotSqlite;
    }
    let word = |at: usize| [header[at], header[at + 1], header[at + 2], header[at + 3]];
    let version = u32::from_be_bytes(word(60));
    let application_id = i32::from_be_bytes(word(68));
    match (application_id, version) {
        (0, 0) => Probe::Uninitialized,
        (id, _) if id == APPLICATION_ID => Probe::Scull { version },
        (id, _) => Probe::Foreign { application_id: id },
    }
}

/// Probe the database at `path` by reading its header (never follows a symlink at `path`).
pub fn probe(path: &Path) -> io::Result<Probe> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Probe::Missing),
        Err(e) => return Err(e),
    };
    if !md.file_type().is_file() {
        return Ok(Probe::NotAFile);
    }
    let mut header = Vec::with_capacity(100);
    std::fs::File::open(path)?
        .take(100)
        .read_to_end(&mut header)?;
    Ok(classify_header(&header))
}

/// Why [`open_current`] did not open the database.
#[derive(Debug)]
pub enum NotOpened {
    /// The probe found something other than a current scull database.
    Probe(Probe),
    /// Reading the header failed.
    Io(io::Error),
    /// The store refused to open it.
    Store(scull_storage::StoreError),
}

/// Open the database at `path` only if it already exists with the current schema, without
/// creating, migrating or re-permissioning anything.
pub fn open_current(path: &Path) -> Result<Store, NotOpened> {
    match probe(path).map_err(NotOpened::Io)? {
        Probe::Scull { version } if version == SCHEMA_VERSION => {
            Store::open(path, &NoCreate).map_err(NotOpened::Store)
        }
        other => Err(NotOpened::Probe(other)),
    }
}

/// An [`OwnerOnly`] for read-mostly commands: creating anything fails, and existing
/// permissions are left as they are.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCreate;

impl OwnerOnly for NoCreate {
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        Err(refuse(path))
    }

    fn create_file(&self, path: &Path) -> io::Result<()> {
        Err(refuse(path))
    }

    fn restrict(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

fn refuse(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "this command never creates files (refused to create {})",
            path.display()
        ),
    )
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
        assert_eq!(classify_header(b""), Probe::Uninitialized);
        assert_eq!(classify_header(b"hello"), Probe::NotSqlite);
        assert_eq!(classify_header(&[b'x'; 100]), Probe::NotSqlite);
        assert_eq!(classify_header(&header(0, 0)), Probe::Uninitialized);
        assert_eq!(
            classify_header(&header(2, APPLICATION_ID)),
            Probe::Scull { version: 2 }
        );
        assert_eq!(
            classify_header(&header(3, 7)),
            Probe::Foreign { application_id: 7 }
        );
    }

    #[test]
    fn probe_and_open_never_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = db_path(dir.path());
        assert_eq!(probe(&path).expect("probe"), Probe::Missing);
        assert!(matches!(
            open_current(&path),
            Err(NotOpened::Probe(Probe::Missing))
        ));
        assert!(!path.exists(), "open_current must not create the database");
        assert_eq!(probe(dir.path()).expect("probe"), Probe::NotAFile);
    }

    #[test]
    fn current_database_opens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = db_path(dir.path());
        drop(Store::open(&path, &scull_storage::DefaultPermissions).expect("seed"));
        assert_eq!(
            probe(&path).expect("probe"),
            Probe::Scull {
                version: SCHEMA_VERSION
            }
        );
        let store = open_current(&path).expect("opens");
        assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    }

    #[test]
    fn no_create_refuses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("x");
        assert!(NoCreate.create_dir(&p).is_err());
        assert!(NoCreate.create_file(&p).is_err());
        assert!(!p.exists());
        assert!(NoCreate.restrict(dir.path()).is_ok());
    }
}
