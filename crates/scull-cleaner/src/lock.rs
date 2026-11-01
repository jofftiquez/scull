//! The single execution lock per user account (SAFE-09, P2-EXEC-04).
//!
//! An [`Executor`](crate::Executor) that executes plans holds an [`ExecutionLock`] for its
//! whole lifetime: an OS file lock (`flock` on Unix, `LockFileEx` on Windows) on
//! `<state>/execution.lock` in the user's state directory. A second executor (CLI, desktop or
//! MCP, any process of the same user) fails at once with [`ErrorKind::LockHeld`] (exit code 6),
//! or waits up to a timeout when asked to. Scans never take the lock. The OS releases it when
//! the holder exits or is killed, so there is no stale-lock cleanup and no PID file.

use scull_core::{ErrorKind, FsPath, ScullError};
use scull_platform::app_dirs;
use scull_platform::exec_lock::{ExclusiveLock, exec_lock_path, lock_exclusive};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the lock is and how long to wait for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockOptions {
    /// The lock file.
    pub path: PathBuf,
    /// How long to wait for another executor to finish. `None` (the default) fails at once.
    pub wait: Option<Duration>,
}

impl LockOptions {
    /// The current user's lock (`<state>/execution.lock`, see `scull_platform::app_dirs`),
    /// failing fast.
    pub fn for_current_user() -> Result<Self, ScullError> {
        let dirs = app_dirs().map_err(|e| {
            ScullError::new(
                ErrorKind::Config,
                format!("cannot locate scull's state directory for the execution lock: {e}"),
            )
        })?;
        Ok(Self::at(exec_lock_path(&dirs)))
    }

    /// A lock at `path` (tests, portable setups), failing fast.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            wait: None,
        }
    }

    /// Wait up to `timeout` for the lock instead of failing at once.
    #[must_use]
    pub fn wait(mut self, timeout: Duration) -> Self {
        self.wait = Some(timeout);
        self
    }
}

/// The held execution lock. Released when dropped (or when the process ends, however it ends).
#[derive(Debug)]
pub struct ExecutionLock {
    inner: ExclusiveLock,
}

impl ExecutionLock {
    /// Takes the lock, or fails with [`ErrorKind::LockHeld`] if another executor holds it
    /// (after waiting up to [`LockOptions::wait`]).
    pub fn acquire(opts: &LockOptions) -> Result<Self, ScullError> {
        let path = FsPath::from_path(&opts.path);
        match lock_exclusive(&opts.path, opts.wait.unwrap_or(Duration::ZERO)) {
            Ok(Some(inner)) => Ok(Self { inner }),
            Ok(None) => Err(ScullError::new(
                ErrorKind::LockHeld,
                match opts.wait {
                    Some(w) => format!(
                        "another scull operation is running (waited {} ms); try again when it \
                         has finished",
                        w.as_millis()
                    ),
                    None => "another scull operation is running; try again when it has \
                             finished, or wait for it"
                        .to_owned(),
                },
            )
            .with_path(path)),
            Err(e) => Err(ScullError::new(
                ScullError::from_io(&e).kind,
                format!("cannot take the execution lock: {e}"),
            )
            .with_path(path)),
        }
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        self.inner.path()
    }
}
