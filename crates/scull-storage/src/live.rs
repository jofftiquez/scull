//! Which databases this process holds a live [`Store`](crate::Store) connection to.
//!
//! On Unix, closing *any* descriptor of a file releases every POSIX record lock the process
//! holds on that file, and SQLite keeps its database and `-shm` locks that way. Restricting a
//! file owner-only may open and close it ([`crate::OwnerOnly::restrict`]), so `Store::open`
//! restricts the database, `-wal` and `-shm` only while this process has **no** live
//! connection to that database: otherwise the live connection would lose its locks, and
//! another process could reset the WAL shared memory under it or delete the WAL. The files
//! were restricted when the first live connection opened them. (SQLite's own connections
//! share their descriptors per file inside the process, so closing one SQLite connection
//! never drops another's locks.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

fn registry() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static LIVE: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Whether this process holds a live connection to the database at `path`.
pub(crate) fn is_live(path: &Path) -> bool {
    let key = key(path);
    registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .is_some_and(|n| *n > 0)
}

/// A live connection to one database, registered until dropped. (Boxed: one pointer in
/// every `Store`.)
#[derive(Debug)]
pub(crate) struct LiveConnection(Box<PathBuf>);

impl LiveConnection {
    /// Register a live connection to the database at `path`.
    pub(crate) fn register(path: &Path) -> Self {
        let key = key(path);
        *registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.clone())
            .or_insert(0) += 1;
        Self(Box::new(key))
    }
}

impl Drop for LiveConnection {
    fn drop(&mut self) {
        let mut live = registry().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = live.get_mut(&*self.0) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                live.remove(&*self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_live_connections_per_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("x.db");
        std::fs::write(&db, b"").expect("db");
        assert!(!is_live(&db));
        let a = LiveConnection::register(&db);
        let b = LiveConnection::register(&dir.path().join(".").join("x.db"));
        assert!(is_live(&db));
        drop(a);
        assert!(is_live(&db), "b is still live");
        drop(b);
        assert!(!is_live(&db));
    }
}
