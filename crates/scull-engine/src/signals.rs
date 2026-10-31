//! Pluggable safety signals the engine can't observe on its own yet.
//!
//! In-use detection (open files, process working directories, held tool locks; PRD §8.4,
//! P1-SAFE-02) is an [`InUseProbe`]. Without one, every candidate is "in use: unknown", which
//! the rubric classifies as at least S2 and the report lists under `signals_unavailable`
//! (missing evidence never counts as evidence of inactivity).

use scull_core::Evidence;
use scull_platform::{PlatformError, VolumeInfo};
use scull_safety::{ActivityProbe, GapPolicy, Tri, check_in_use};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The answer of an [`InUseProbe`] for one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InUseAnswer {
    /// Whether something uses the directory. `Unknown` is never treated as `No`.
    pub in_use: Tri,
    /// Facts for the candidate's evidence (holders, unavailable signals).
    pub evidence: Vec<Evidence>,
}

/// Checks whether a directory is in use right now (open files, process cwd, held locks).
pub trait InUseProbe: Send + Sync {
    /// Check `dir`. `lock_files` are the rule's tool lock files (they count only while held);
    /// `processes` the rule's tool process names.
    fn check(&self, dir: &Path, lock_files: &[PathBuf], processes: &[String]) -> InUseAnswer;
}

/// Signal name used when no [`InUseProbe`] is configured.
pub const IN_USE_SIGNAL: &str = "in_use";

/// Signal name for a candidate whose subtree was not fully observed (a folder inside could not
/// be read, timed out, or was not descended): what it holds is unknown (at least S2).
pub const COMPLETENESS_SIGNAL: &str = "completeness";

/// Signal name for a mounted-volume list that could not be read: a bind mount inside a
/// candidate can't be ruled out, so it is treated as crossing a mount.
pub const MOUNTS_SIGNAL: &str = "mounts";

/// Signal name for a `scan.protect` / `scan.exclude` (or repository policy) entry that could
/// not be resolved: candidates it might cover are protected.
pub const PROTECTION_SIGNAL: &str = "protection";

/// Why in-use is unknown without a probe.
pub const IN_USE_UNAVAILABLE: &str = "open-file, process working directory and tool-lock checks \
     are not available in this build yet (P1-SAFE-02, #49); every candidate is treated as \
     possibly in use (at least S2)";

/// The answer used when no probe is configured: unknown, disclosed.
pub fn in_use_unavailable() -> InUseAnswer {
    InUseAnswer {
        in_use: Tri::Unknown,
        evidence: vec![Evidence::SignalUnavailable {
            signal: IN_USE_SIGNAL.into(),
            reason: IN_USE_UNAVAILABLE.into(),
        }],
    }
}

/// Where an engine gets in-use answers from.
#[derive(Clone, Default)]
pub enum InUseSource {
    /// No probe: every candidate is "in use: unknown" (at least S2), disclosed.
    #[default]
    Unavailable,
    /// The OS (P1-SAFE-02): a fresh process snapshot per run, with this gap policy (D-30).
    Native(GapPolicy),
    /// A caller-supplied probe (tests, or a caller that checked use itself).
    Probe(Arc<dyn InUseProbe>),
}

impl fmt::Debug for InUseSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InUseSource::Unavailable => f.write_str("Unavailable"),
            InUseSource::Native(p) => f.debug_tuple("Native").field(p).finish(),
            InUseSource::Probe(_) => f.write_str("Probe(..)"),
        }
    }
}

/// An [`InUseProbe`] over a `scull-safety` [`ActivityProbe`] (process working directories,
/// open files, held tool locks), combined by [`check_in_use`].
#[derive(Debug)]
pub struct ActivityInUse<P>(pub P);

impl<P: ActivityProbe + Send + Sync> InUseProbe for ActivityInUse<P> {
    fn check(&self, dir: &Path, lock_files: &[PathBuf], _processes: &[String]) -> InUseAnswer {
        let r = check_in_use(dir, lock_files, &self.0);
        InUseAnswer {
            in_use: r.in_use,
            evidence: r.evidence,
        }
    }
}

/// A probe with a fixed answer, for tests and for callers that have checked use themselves.
#[derive(Debug, Clone, Copy)]
pub struct FixedInUse(pub Tri);

impl InUseProbe for FixedInUse {
    fn check(&self, _dir: &Path, _lock_files: &[PathBuf], _processes: &[String]) -> InUseAnswer {
        InUseAnswer {
            in_use: self.0,
            evidence: Vec::new(),
        }
    }
}

/// Where the engine gets the list of mounted volumes (mount points for the "crosses a mount"
/// check, filesystem types for the reclaim estimate). Injectable so tests can simulate a
/// failure; the real machine uses [`NativeVolumes`].
pub trait VolumeSource: Send + Sync {
    /// Every mounted volume, or why the list could not be read.
    fn volumes(&self) -> Result<Vec<VolumeInfo>, PlatformError>;
}

/// The OS volume list ([`scull_platform::volumes()`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeVolumes;

impl VolumeSource for NativeVolumes {
    fn volumes(&self) -> Result<Vec<VolumeInfo>, PlatformError> {
        scull_platform::volumes()
    }
}
