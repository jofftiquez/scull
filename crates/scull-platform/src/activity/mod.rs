//! Process activity facts: which running processes have their working directory, or an open
//! file, inside a path (PRD §8.4, P1-SAFE-02).
//!
//! The safety layer (`scull-safety`) turns these facts into the rubric's `in_use` signal. This
//! module only reports what the OS shows, with honest coverage: every process that could not
//! be inspected is counted, and a scan cut short by a [`Limits`] bound says so. Nothing here
//! decides whether a gap matters; that policy belongs to the caller.
//!
//! # How each OS is inspected
//!
//! | OS | Process working directories | Open files |
//! |---|---|---|
//! | Linux | `readlink /proc/<pid>/cwd` | `readlink /proc/<pid>/fd/*` |
//! | macOS | `proc_pidinfo(PROC_PIDVNODEPATHINFO)` | `proc_pidinfo(PROC_PIDLISTFDS)` + `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)` for vnode descriptors |
//! | Windows | the process's `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory`, read from its PEB (`NtQueryInformationProcess` + `ReadProcessMemory`), 64-bit builds only | Restart Manager (`RmRegisterResources` + `RmGetList`) over a bounded sample of the files under the path |
//!
//! Every call is read-only: links are read with `readlink`, never followed or opened; no file
//! a process holds is opened; nothing is signalled, suspended or modified.
//!
//! # Coverage
//!
//! A process that cannot be inspected (another user's process without privileges, a protected
//! process, a sandbox) is counted in [`Observation::denied_own`] (same user as this process)
//! or [`Observation::denied_other`] (another user, or owner unknown on Windows). On macOS an
//! unknown owner is counted as the same user, because the owner lookup itself needs no
//! privileges there. A [`Limits`] bound that cut the scan short sets
//! [`Observation::truncated`]. An absent hit is only evidence of "not in use" when
//! [`Observation::is_complete`] holds.
//!
//! Processes that exit while being inspected are skipped: they no longer hold anything.
//!
//! # Usage
//!
//! Take one [`Snapshot`] per scan (or per revalidation) and query it for each candidate:
//! process listing is the expensive part, matching is cheap.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

/// Processes holding `file` open, from Restart Manager (query only: `RmShutdown` is never
/// called). The removal walker uses it to name the holder of a locked file it skips
/// (FX-WIN-03); the path is used for this query only, never to delete.
#[cfg(all(windows, feature = "delete"))]
pub(crate) fn processes_holding(file: &Path) -> Result<Vec<Hit>, String> {
    windows::restart_manager(&[file.to_path_buf()], file)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
use unsupported as os;

/// Bounds on the work one [`Snapshot`] or query may do. Hitting any of them marks the result
/// as truncated (never as "not in use").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most processes inspected per snapshot.
    pub max_processes: usize,
    /// Most open file descriptors recorded per snapshot, over all processes (Linux, macOS).
    pub max_open_files: usize,
    /// Most files under a candidate registered with Restart Manager per query (Windows).
    pub max_sampled_files: usize,
    /// Wall-clock budget for one snapshot, and separately for one Windows open-file query.
    pub time_budget: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_processes: 20_000,
            max_open_files: 1_000_000,
            max_sampled_files: 2_048,
            time_budget: Duration::from_secs(10),
        }
    }
}

/// One process found holding something inside the queried path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Process ID.
    pub pid: u32,
    /// Process name, if the OS reports one.
    pub name: Option<String>,
    /// What it holds: its working directory, the open file, or (Restart Manager, which does not
    /// say which file) the queried path.
    pub path: PathBuf,
}

/// The answer to one query, with its coverage.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Observation {
    /// Processes found holding something inside the path, at most one per process.
    pub hits: Vec<Hit>,
    /// Why this signal is not available on this OS or build at all, if it isn't.
    pub unsupported: Option<String>,
    /// Processes of the current user that could not be inspected.
    pub denied_own: usize,
    /// Processes of other users (or of an unknown owner, Windows) that could not be inspected.
    pub denied_other: usize,
    /// A [`Limits`] bound cut the observation short.
    pub truncated: bool,
    /// The query itself failed (e.g. the path could not be resolved, Restart Manager failed).
    pub error: Option<String>,
}

impl Observation {
    /// Whether every process was inspected and nothing failed, so that an empty
    /// [`Observation::hits`] means "nothing holds it".
    pub fn is_complete(&self) -> bool {
        self.unsupported.is_none()
            && self.error.is_none()
            && !self.truncated
            && self.denied_own == 0
            && self.denied_other == 0
    }

    fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            unsupported: Some(reason.into()),
            ..Self::default()
        }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

/// Who could not be inspected while collecting one kind of fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Gaps {
    denied_own: usize,
    denied_other: usize,
}

impl Gaps {
    fn deny(&mut self, own: bool) {
        if own {
            self.denied_own += 1;
        } else {
            self.denied_other += 1;
        }
    }
}

/// One inspected process.
#[derive(Debug, Clone, Default)]
struct Proc {
    pid: u32,
    name: Option<String>,
    /// Working directory as the OS reports it (Windows: resolved to its final long path).
    cwd: Option<PathBuf>,
    /// Paths of open regular files and directories (Linux, macOS).
    #[cfg_attr(windows, allow(dead_code))]
    files: Vec<PathBuf>,
}

/// What an OS module collects for a snapshot.
#[derive(Debug, Default)]
struct Collected {
    procs: Vec<Proc>,
    cwd_gaps: Gaps,
    #[cfg_attr(windows, allow(dead_code))]
    file_gaps: Gaps,
    truncated: bool,
    /// Set when the OS gives no access to working directories at all.
    cwd_unsupported: Option<String>,
    /// Set when the OS gives no process-wide list of open files (unused on Windows, where open
    /// files are queried per path through Restart Manager instead).
    #[cfg_attr(windows, allow(dead_code))]
    files_unsupported: Option<String>,
    /// The process listing itself failed.
    error: Option<String>,
}

/// A point-in-time view of running processes' working directories and open files.
///
/// Facts go stale as processes come and go: take a new snapshot for each scan and again when
/// revalidating before execution (SAFE-05).
#[derive(Debug)]
pub struct Snapshot {
    collected: Collected,
    limits: Limits,
}

impl Snapshot {
    /// Inspect every running process the current user may see, within `limits`.
    pub fn capture(limits: Limits) -> Self {
        let deadline = Instant::now() + limits.time_budget;
        Self {
            collected: os::collect(&limits, deadline),
            limits,
        }
    }

    /// The bounds this snapshot was taken with (also used by per-path queries).
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Number of processes inspected (at least partly).
    pub fn process_count(&self) -> usize {
        self.collected.procs.len()
    }

    /// Processes whose working directory is `dir` or inside it.
    pub fn cwds_under(&self, dir: &Path) -> Observation {
        let c = &self.collected;
        if let Some(reason) = &c.cwd_unsupported {
            return Observation::unsupported(reason.clone());
        }
        let root = match resolve(dir) {
            Ok(root) => root,
            Err(e) => return Observation::failed(e),
        };
        let hits = c
            .procs
            .iter()
            .filter_map(|p| {
                let cwd = p.cwd.as_deref()?;
                os::is_under(cwd, &root).then(|| Hit {
                    pid: p.pid,
                    name: p.name.clone(),
                    path: cwd.to_path_buf(),
                })
            })
            .collect();
        self.observation(hits, c.cwd_gaps)
    }

    /// Processes holding a file or directory open at `dir` or inside it.
    ///
    /// On Windows this asks Restart Manager about a bounded sample of the files under `dir`
    /// (at most [`Limits::max_sampled_files`]); a sample that does not cover every file is
    /// reported as truncated.
    pub fn open_files_under(&self, dir: &Path) -> Observation {
        #[cfg(windows)]
        return os::open_files_under(dir, &self.limits);
        #[cfg(not(windows))]
        self.open_files_under_snapshot(dir)
    }

    #[cfg(not(windows))]
    fn open_files_under_snapshot(&self, dir: &Path) -> Observation {
        let c = &self.collected;
        if let Some(reason) = &c.files_unsupported {
            return Observation::unsupported(reason.clone());
        }
        let root = match resolve(dir) {
            Ok(root) => root,
            Err(e) => return Observation::failed(e),
        };
        let hits = c
            .procs
            .iter()
            .filter_map(|p| {
                let file = p.files.iter().find(|f| os::is_under(f, &root))?;
                Some(Hit {
                    pid: p.pid,
                    name: p.name.clone(),
                    path: file.clone(),
                })
            })
            .collect();
        self.observation(hits, c.file_gaps)
    }

    /// Processes holding any of `files` open (e.g. a build tool's lock file). Files that do not
    /// exist are skipped: nobody holds them.
    pub fn files_open(&self, files: &[PathBuf]) -> Observation {
        let mut existing = Vec::with_capacity(files.len());
        let mut resolved = Vec::with_capacity(files.len());
        for f in files {
            match resolve(f) {
                Ok(p) => {
                    existing.push(f.clone());
                    resolved.push(p);
                }
                Err(_) if !exists_no_follow(f) => {}
                Err(e) => return Observation::failed(e),
            }
        }
        if resolved.is_empty() {
            return Observation::default();
        }
        // Restart Manager gets the paths as given (it resolves them itself).
        #[cfg(windows)]
        return os::files_open(&existing, &self.limits);
        #[cfg(not(windows))]
        self.files_open_snapshot(&resolved)
    }

    #[cfg(not(windows))]
    fn files_open_snapshot(&self, resolved: &[PathBuf]) -> Observation {
        let c = &self.collected;
        if let Some(reason) = &c.files_unsupported {
            return Observation::unsupported(reason.clone());
        }
        let hits = c
            .procs
            .iter()
            .filter_map(|p| {
                let file = p
                    .files
                    .iter()
                    .find(|f| resolved.iter().any(|r| os::same_path(f, r)))?;
                Some(Hit {
                    pid: p.pid,
                    name: p.name.clone(),
                    path: file.clone(),
                })
            })
            .collect();
        self.observation(hits, c.file_gaps)
    }

    fn observation(&self, hits: Vec<Hit>, gaps: Gaps) -> Observation {
        let c = &self.collected;
        Observation {
            hits,
            unsupported: None,
            denied_own: gaps.denied_own,
            denied_other: gaps.denied_other,
            truncated: c.truncated,
            error: c.error.clone(),
        }
    }
}

/// Resolve `path` to the form the OS reports for process paths (all links and aliases
/// resolved: `/var` → `/private/var` on macOS, 8.3 names on Windows). Resolving reads link
/// targets and attributes only; nothing is opened for reading.
fn resolve(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|e| format!("cannot resolve {}: {e}", path.display()))
}

fn exists_no_follow(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Component-wise prefix test for already-resolved paths (Unix).
#[cfg(unix)]
fn is_under_exact(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_only_without_gaps() {
        assert!(Observation::default().is_complete());
        assert!(!Observation::unsupported("x").is_complete());
        assert!(!Observation::failed("x").is_complete());
        for o in [
            Observation {
                denied_own: 1,
                ..Observation::default()
            },
            Observation {
                denied_other: 1,
                ..Observation::default()
            },
            Observation {
                truncated: true,
                ..Observation::default()
            },
        ] {
            assert!(!o.is_complete());
        }
    }

    #[test]
    fn snapshot_sees_this_process() {
        let snap = Snapshot::capture(Limits::default());
        if cfg!(any(target_os = "linux", target_os = "macos", windows)) {
            assert!(snap.process_count() > 0);
        }
        // Our own working directory holds our own process (where cwds are supported).
        let here = std::env::current_dir().expect("cwd");
        let obs = snap.cwds_under(&here);
        if obs.unsupported.is_none() && obs.error.is_none() {
            assert!(
                obs.hits.iter().any(|h| h.pid == std::process::id()),
                "own pid not found under {}: {obs:?}",
                here.display()
            );
        }
    }

    #[test]
    fn missing_lock_files_are_skipped() {
        let snap = Snapshot::capture(Limits::default());
        let dir = tempfile::tempdir().expect("tempdir");
        let obs = snap.files_open(&[dir.path().join("no-such.lock")]);
        assert!(obs.hits.is_empty());
        assert!(obs.error.is_none());
    }
}
