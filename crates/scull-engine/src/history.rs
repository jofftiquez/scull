//! Opening the history database with owner-only permissions (DATA-04).

use scull_storage::{DB_FILE_NAME, OwnerOnly, Store, StoreError};
use std::io;
use std::path::{Path, PathBuf};

/// [`OwnerOnly`] backed by `scull-platform`: Unix `0700`/`0600`, a protected owner-only DACL
/// on Windows. Never follows links.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformOwnerOnly;

impl OwnerOnly for PlatformOwnerOnly {
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        scull_platform::owner_only::create_dir(path)
    }

    fn create_file(&self, path: &Path) -> io::Result<()> {
        scull_platform::owner_only::create_file(path).map(drop)
    }

    fn restrict(&self, path: &Path) -> io::Result<()> {
        scull_platform::owner_only::restrict(path)
    }
}

/// The database file inside scull's local data directory.
pub fn history_db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DB_FILE_NAME)
}

/// Open (creating and migrating if needed) the history database in `data_dir`.
pub fn open_history(data_dir: &Path) -> Result<Store, StoreError> {
    Store::open(&history_db_path(data_dir), &PlatformOwnerOnly)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_creates_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        let store = open_history(&data).expect("opens");
        assert!(store.path().is_file());
        assert_eq!(store.path(), history_db_path(&data));
        scull_platform::ensure_owner_only(&data).expect("data dir is owner-only");
        scull_platform::ensure_owner_only(store.path()).expect("database is owner-only");
    }
}
