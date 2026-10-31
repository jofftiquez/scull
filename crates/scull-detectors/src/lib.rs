//! Ecosystem detectors and the data-only rule loader (CLS-01…05).
//!
//! * [`rules`]: the declarative rule format and loader.
//! * [`Detector`], [`Detectors`], [`DetectedArtifact`]: deciding whether a directory the
//!   scanner visited is an artifact, with evidence ([`detector`] module docs).
//! * [`cargo`]: shared Cargo target directories (`CARGO_TARGET_DIR`, `[build] target-dir`).
//! * [`caches`]: package-manager caches (npm, pnpm, Bun, Yarn) located per OS, without running
//!   any tool (Quick Clean, QC-01…05).
//! * [`large_files`]: the inspect-only large-files listing (never a cleanup candidate).
//! * [`node`]: `node_modules` (link awareness, workspace lockfiles) and framework outputs.
//!
//! See `docs/PRD.md` and `HANDOVER.md` for responsibilities and boundaries.

pub mod caches;
pub mod cargo;
pub mod detector;
pub mod large_files;
pub mod node;
pub mod rules;

pub use caches::{
    CacheCommands, CacheEnv, CacheProblem, CacheScan, CacheSource, CacheTool, LocatedCache,
    NativeCommand, RefusedCache, commands_for_rule, locate_caches,
};
pub use cargo::{SharedTargetDirs, TargetDirSource};
pub use detector::{
    CallerSignals, ConfigReader, DetectContext, DetectedArtifact, Detector, DetectorSignals,
    Detectors, MAX_CONFIG_BYTES, RuleDetector,
};
pub use large_files::{
    FileKind, LargeFile, LargeFileOptions, LargeFileReport, LargeFiles, inspect_large_files,
};
pub use node::{FrameworkDetector, NodeModulesDetector};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-detectors");
    }
}
