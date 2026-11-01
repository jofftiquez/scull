//! Ecosystem detectors and the data-only rule loader (CLS-01…05).
//!
//! * [`rules`]: the declarative rule format and loader.
//! * [`Detector`], [`Detectors`], [`DetectedArtifact`]: deciding whether a directory the
//!   scanner visited is an artifact, with evidence ([`detector`] module docs).
//! * [`cargo`]: shared Cargo target directories (`CARGO_TARGET_DIR`, `[build] target-dir`).
//! * [`caches`]: package-manager and tool caches (npm, pnpm, Bun, Yarn, pip, uv, conda, Go,
//!   Gradle, Maven, NuGet, and the long-tail tools in [`CacheTool::ALL`]) located per OS,
//!   without running any tool (Quick Clean, QC-01…05).
//! * [`go`]: the Go build and module caches (locations, layouts).
//! * [`jvm`]: Gradle and Maven caches (inspect-only); the build folders are plain rules.
//! * [`docker`]: build cache, dangling images, stopped containers and (protected) volumes via
//!   the Docker Engine API (read-only), plus Docker Desktop VM disk caveats.
//! * [`dotnet`]: `bin/` (configuration folders only) and the NuGet global packages folder.
//! * [`xcode`]: DerivedData, device support files, archives and unavailable simulators (macOS).
//! * [`android`]: Android Gradle Plugin build output (release-aware) and emulator images.
//! * [`game_engines`]: Unity and Unreal Engine project folders (engine markers, user data kept).
//! * [`large_files`]: the inspect-only large-files listing (never a cleanup candidate).
//! * [`long_tail`]: CMake build folders, Bundler's `vendor/bundle`, Terraform, Bazel output
//!   bases, Podman storage and the minikube cache (the other long-tail folders are plain rules).
//! * [`ide`]: JetBrains caches, editor workspace storage of deleted workspaces, and extension
//!   versions the editor marked obsolete (never settings or extension state in use).
//! * [`tool_caches`]: where the long-tail tool caches are (ccache … Nix) and their layouts.
//! * [`ml`]: Hugging Face, Ollama and PyTorch Hub caches (scan rules plus a per-OS locator).
//! * [`node`]: `node_modules` (link awareness, workspace lockfiles) and framework outputs.
//! * [`python`]: virtual environments (lockfiles, pinned requirements) and `__pycache__` (every
//!   module's source present); the tool caches are plain rules.
//!
//! See `docs/ARCHITECTURE.md` for responsibilities and boundaries.

pub mod android;
pub mod caches;
pub mod cargo;
pub mod detector;
pub mod docker;
pub mod dotnet;
pub mod game_engines;
pub mod go;
pub mod ide;
pub mod jvm;
pub mod large_files;
pub mod long_tail;
pub mod ml;
pub mod node;
pub mod python;
pub mod rules;
pub mod tool_caches;
pub mod xcode;

pub use android::AndroidDetector;
pub use caches::{
    CacheCommands, CacheEnv, CacheProblem, CacheScan, CacheSource, CacheTool, LocatedCache,
    NativeCommand, RefusedCache, commands_for_rule, locate_caches,
};
pub use cargo::{SharedTargetDirs, TargetDirSource};
pub use detector::{
    CallerSignals, ConfigReader, DetectContext, DetectedArtifact, Detector, DetectorSignals,
    Detectors, MAX_CONFIG_BYTES, RuleDetector,
};
pub use docker::{
    DaemonInfo, DockerGroup, DockerKind, DockerOptions, DockerReport, DockerStatus, DockerTarget,
    DockerTransport, EndpointSource, ImageTotals, NativeTransport, RefusedEndpoint, VmDisk,
    VmDiskKind, docker_targets, inspect_docker, inspect_docker_with, locate_vm_disks,
};
pub use dotnet::DotnetBinDetector;
pub use game_engines::GameEngineDetector;
pub use ide::IdeDetector;
pub use large_files::{
    FileKind, LargeFile, LargeFileOptions, LargeFileReport, LargeFiles, inspect_large_files,
};
pub use long_tail::{LongTailDetector, LongTailKind};
pub use ml::{MlDetector, MlKind, MlRoot, MlRootKind, MlScan, RefusedMlRoot, locate_ml_caches};
pub use node::{FrameworkDetector, NodeModulesDetector};
pub use python::{PycacheDetector, VenvDetector, requirements_pinned};
pub use xcode::{XcodeDetector, XcodeKind};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-detectors");
    }
}
