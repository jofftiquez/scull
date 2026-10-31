//! Read-only access to scull's history database for `get_status` and `get_cleanup_history`.
//!
//! The server never creates or migrates the database: it reads the SQLite header first (a
//! plain file read) and opens only a database that already has the current schema, through an
//! [`OwnerOnly`] that refuses to create anything. Same approach as `scull status`.

use scull_storage::{APPLICATION_ID, OwnerOnly, SCHEMA_VERSION, Store};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Why no database is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// No data directory is known.
    NoDataDir,
    /// Nothing was ever recorded.
    Missing,
    /// The file is not a current scull database (newer, older, foreign or damaged).
    NotCurrent(String),
}

impl Unavailable {
    /// Stable state name.
    pub fn state(&self) -> &'static str {
        match self {
            Unavailable::NoDataDir | Unavailable::NotCurrent(_) => "unavailable",
            Unavailable::Missing => "no_database",
        }
    }

    /// Human explanation.
    pub fn detail(&self) -> String {
        match self {
            Unavailable::NoDataDir => "scull's data directory is not known".into(),
            Unavailable::Missing => {
                "no history yet: run `scull scan` in a terminal to record a scan".into()
            }
            Unavailable::NotCurrent(why) => why.clone(),
        }
    }
}

/// Open the database in `data_dir` if it exists with the current schema, without creating,
/// migrating or re-permissioning anything.
pub fn open_read_only(data_dir: Option<&Path>) -> Result<Store, Unavailable> {
    let dir = data_dir.ok_or(Unavailable::NoDataDir)?;
    let path: PathBuf = dir.join(scull_storage::DB_FILE_NAME);
    let md = match std::fs::symlink_metadata(&path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Unavailable::Missing),
        Err(e) => return Err(Unavailable::NotCurrent(format!("cannot read it: {e}"))),
    };
    if !md.file_type().is_file() {
        return Err(Unavailable::NotCurrent("not a regular file".into()));
    }
    let mut header = Vec::with_capacity(100);
    std::fs::File::open(&path)
        .and_then(|f| f.take(100).read_to_end(&mut header))
        .map_err(|e| Unavailable::NotCurrent(format!("cannot read it: {e}")))?;
    if header.is_empty() {
        return Err(Unavailable::Missing);
    }
    if header.len() < 100 || &header[..16] != b"SQLite format 3\0" {
        return Err(Unavailable::NotCurrent("not a SQLite database".into()));
    }
    let word = |at: usize| [header[at], header[at + 1], header[at + 2], header[at + 3]];
    let version = u32::from_be_bytes(word(60));
    let app = i32::from_be_bytes(word(68));
    if app != APPLICATION_ID {
        return Err(Unavailable::NotCurrent(
            "not a scull database (or never initialized)".into(),
        ));
    }
    if version != SCHEMA_VERSION {
        return Err(Unavailable::NotCurrent(format!(
            "schema version {version} (this scull reads {SCHEMA_VERSION}); run `scull status` \
             in a terminal"
        )));
    }
    Store::open(&path, &NoCreate).map_err(|e| Unavailable::NotCurrent(e.to_string()))
}

/// An [`OwnerOnly`] that refuses to create anything and leaves permissions alone.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCreate;

impl OwnerOnly for NoCreate {
    fn create_dir(&self, _path: &Path) -> io::Result<()> {
        Err(refuse())
    }

    fn create_file(&self, _path: &Path) -> io::Result<()> {
        Err(refuse())
    }

    fn restrict(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

fn refuse() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "the MCP server never creates files",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_never_created() {
        let d = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            open_read_only(Some(d.path())).err(),
            Some(Unavailable::Missing)
        );
        assert!(!d.path().join(scull_storage::DB_FILE_NAME).exists());
        assert_eq!(open_read_only(None).err(), Some(Unavailable::NoDataDir));
        std::fs::write(d.path().join(scull_storage::DB_FILE_NAME), b"hello").expect("write");
        assert!(matches!(
            open_read_only(Some(d.path())),
            Err(Unavailable::NotCurrent(_))
        ));
    }

    #[test]
    fn current_database_opens() {
        let d = tempfile::tempdir().expect("tempdir");
        drop(
            Store::open(
                &d.path().join(scull_storage::DB_FILE_NAME),
                &scull_storage::DefaultPermissions,
            )
            .expect("seed"),
        );
        assert!(open_read_only(Some(d.path())).is_ok());
    }
}
