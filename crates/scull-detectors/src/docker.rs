//! Docker through its own API, plus VM disk caveats (P5-DET-03, PRD §7.1, §9, §10.5).
//!
//! Docker's data directory is root-owned on Linux and lives inside a VM disk image on macOS
//! and Windows, so walking it would be wrong twice over. Instead [`inspect_docker`] asks the
//! daemon what it holds, through the read-only transport in [`scull_platform::docker`]
//! (`GET` on allow-listed endpoints only, hard deadlines), and reports it **inspect-only**:
//! nothing here runs a command or calls a mutating endpoint.
//!
//! # What is reported
//!
//! | Group | Source | Tier | Proposed action (never run here) |
//! |---|---|---|---|
//! | [`DockerKind::BuildCache`] | `GET /system/df?type=build-cache` | **S0**: rebuilt by the next build | `docker builder prune --all --force` (unused records only) |
//! | [`DockerKind::DanglingImages`] | `GET /images/json` (untagged images) | **S1**: rebuilt or re-pulled | `docker image prune --force` (dangling only) |
//! | [`DockerKind::StoppedContainers`] | `GET /system/df?type=container` (exited, created, dead) | **S2**: a writable layer may hold data that exists nowhere else | none: review with `docker ps -a` |
//! | [`DockerKind::Volumes`] | `GET /volumes` | **S3**: application data; **never offered** | none |
//!
//! Reclaimable bytes follow Docker's own accounting: build-cache records that are neither in
//! use nor shared; dangling images no container uses, minus their shared size when the daemon
//! reports it (otherwise confidence is low because layers may be shared). Every section has
//! its own deadline and fails on its own: a slow `/images/json` leaves the build cache report
//! intact ([`DockerReport::unavailable`]).
//!
//! # VM disks ([`locate_vm_disks`])
//!
//! When the daemon runs in a VM (Docker Desktop on macOS, Windows or Linux; colima, OrbStack,
//! WSL 2), pruning frees space **inside** the VM's disk image. The host file (`Docker.raw`,
//! `docker_data.vhdx`, `ext4.vhdx`, ...) shrinks only when the VM discards the blocks (sparse
//! `Docker.raw`) or after a manual compaction (`.vhdx`), so every in-VM figure carries
//! [`EstimateNote::InsideVmImage`] and [`Confidence::Unknown`] and only a measured delta can
//! tell what the host got back (PRD §9). The disk image file itself is reported with the
//! caveat and the documented compaction steps, always **S3** with [`Action::None`]: deleting it
//! deletes every image, container and volume.
//!
//! # Finding the daemon ([`docker_targets`])
//!
//! Like the Docker CLI: `DOCKER_HOST`, else the current Docker context (`DOCKER_CONTEXT`, or
//! `currentContext` in `$DOCKER_CONFIG/config.json`, default `~/.docker`), else the default
//! sockets for the OS. Remote, SSH and TLS endpoints are refused (a remote daemon's disk is not
//! this machine's disk). Everything machine-specific comes from [`CacheEnv`] and the
//! [`ConfigReader`]; library code never reads `std::env` or `std::fs`.

use crate::caches::CacheEnv;
use crate::detector::{ConfigReader, MAX_CONFIG_BYTES};
use crate::rules::Platform as Os;
use scull_core::{Action, Confidence, Reclaimable, Tier};
use scull_platform::docker::{DockerEndpoint, DockerError, DockerResponse, EndpointError};
use scull_platform::{EntryKind, Platform};
use scull_scanner::EstimateNote;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// How long to wait for the daemon, and where to look for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DockerOptions {
    /// Deadline for `GET /_ping` on each candidate endpoint (default 1 s).
    pub ping_timeout: Duration,
    /// Deadline for each data request (default 10 s; `/system/df` sections run in parallel).
    pub request_timeout: Duration,
    /// Also try the machine-wide default endpoints (`/var/run/docker.sock`,
    /// `\\.\pipe\docker_engine`, ...). Tests turn this off so they never reach a real daemon.
    pub system_endpoints: bool,
}

impl Default for DockerOptions {
    fn default() -> Self {
        Self {
            ping_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(10),
            system_endpoints: true,
        }
    }
}

/// Sends a read-only `GET` to a daemon. [`NativeTransport`] is the real one; tests fake it.
pub trait DockerTransport: Send + Sync {
    /// `GET path` from `endpoint` within `timeout`.
    fn get(
        &self,
        endpoint: &DockerEndpoint,
        path: &str,
        timeout: Duration,
    ) -> Result<DockerResponse, DockerError>;
}

/// The real transport: [`scull_platform::docker::docker_get`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeTransport;

impl DockerTransport for NativeTransport {
    fn get(
        &self,
        endpoint: &DockerEndpoint,
        path: &str,
        timeout: Duration,
    ) -> Result<DockerResponse, DockerError> {
        scull_platform::docker::docker_get(endpoint, path, timeout)
    }
}

/// Where an endpoint came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointSource {
    /// The `DOCKER_HOST` environment variable.
    DockerHost,
    /// A Docker context (`DOCKER_CONTEXT` or `currentContext`), by name.
    Context(String),
    /// One of Docker's default locations for this OS.
    Default,
}

impl fmt::Display for EndpointSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DockerHost => f.write_str("DOCKER_HOST"),
            Self::Context(name) => write!(f, "Docker context {name:?}"),
            Self::Default => f.write_str("default location"),
        }
    }
}

/// An endpoint to try, with its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerTarget {
    /// The endpoint.
    pub endpoint: DockerEndpoint,
    /// Where it came from.
    pub source: EndpointSource,
}

/// A configured endpoint scull won't use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedEndpoint {
    /// Which setting named it (`DOCKER_HOST`, a context).
    pub source: EndpointSource,
    /// Why it was refused.
    pub error: EndpointError,
}

/// What the daemon said about itself (`GET /version`); strings are sanitized and capped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonInfo {
    /// Engine version, e.g. `27.3.1`.
    pub version: String,
    /// API version, e.g. `1.47`.
    pub api_version: String,
    /// Platform name, e.g. `Docker Desktop 4.36.0 (175267)` or `Docker Engine - Community`.
    pub platform: String,
    /// Kernel the daemon runs on, e.g. `6.10.14-linuxkit`.
    pub kernel: String,
    /// Whether the daemon's data lives in a VM disk image (see the module docs).
    pub in_vm: bool,
}

/// Whether and how the daemon was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerStatus {
    /// Connected; the groups in the report come from this daemon.
    Connected {
        /// The endpoint that answered.
        target: DockerTarget,
        /// What it said about itself.
        daemon: DaemonInfo,
    },
    /// No daemon: no socket at any candidate location, or nothing accepting connections.
    NotRunning {
        /// What was tried (empty: no socket exists at any default location).
        tried: Vec<DockerTarget>,
    },
    /// A socket exists but the OS refused access (Linux: the user isn't in the `docker`
    /// group). scull never asks for elevation (SAFE-14).
    NotAccessible {
        /// The endpoint.
        target: DockerTarget,
    },
    /// The daemon accepted the connection but didn't answer `/_ping` in time.
    NotResponding {
        /// The endpoint.
        target: DockerTarget,
    },
    /// The configured endpoint is remote, SSH, TLS or malformed.
    Refused(RefusedEndpoint),
    /// Any other failure (protocol error, unexpected status).
    Failed {
        /// The endpoint.
        target: DockerTarget,
        /// What went wrong.
        reason: String,
    },
}

impl DockerStatus {
    /// A one-line explanation for reports.
    pub fn summary(&self) -> String {
        match self {
            Self::Connected { target, daemon } => format!(
                "connected to Docker {} (API {}) at {} ({})",
                daemon.version, daemon.api_version, target.endpoint, target.source
            ),
            Self::NotRunning { tried } if tried.is_empty() => {
                "Docker not found: no daemon socket at the default locations".into()
            }
            Self::NotRunning { tried } => format!(
                "Docker is not running (tried {})",
                tried
                    .iter()
                    .map(|t| t.endpoint.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::NotAccessible { target } => format!(
                "permission denied on the Docker socket {}: Docker data was not inspected \
                 (scull never asks for elevation)",
                target.endpoint
            ),
            Self::NotResponding { target } => {
                format!(
                    "the Docker daemon at {} did not answer in time",
                    target.endpoint
                )
            }
            Self::Refused(r) => format!("{} ignored: {}", r.source, r.error),
            Self::Failed { target, reason } => {
                format!(
                    "Docker at {} could not be inspected: {reason}",
                    target.endpoint
                )
            }
        }
    }
}

/// A group of Docker objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DockerKind {
    /// BuildKit / builder cache records.
    BuildCache,
    /// Untagged (`<none>`) images.
    DanglingImages,
    /// Containers that are exited, created or dead.
    StoppedContainers,
    /// Named and anonymous volumes. Protected.
    Volumes,
}

impl DockerKind {
    /// Every kind, in report order.
    pub const ALL: [DockerKind; 4] = [
        DockerKind::BuildCache,
        DockerKind::DanglingImages,
        DockerKind::StoppedContainers,
        DockerKind::Volumes,
    ];

    /// Short label.
    pub fn label(self) -> &'static str {
        match self {
            Self::BuildCache => "build cache",
            Self::DanglingImages => "dangling images",
            Self::StoppedContainers => "stopped containers",
            Self::Volumes => "volumes",
        }
    }

    /// The tier the group is classified at (module docs).
    pub fn tier(self) -> Tier {
        match self {
            Self::BuildCache => Tier::S0,
            Self::DanglingImages => Tier::S1,
            Self::StoppedContainers => Tier::S2,
            Self::Volumes => Tier::S3,
        }
    }

    /// The tool-native step scull would propose (SAFE-06); never run by this module.
    pub fn action(self) -> Action {
        let docker = |args: &[&str]| Action::ToolCommand {
            tool: "docker".into(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
        };
        match self {
            Self::BuildCache => docker(&["builder", "prune", "--all", "--force"]),
            Self::DanglingImages => docker(&["image", "prune", "--force"]),
            Self::StoppedContainers | Self::Volumes => Action::None,
        }
    }
}

impl fmt::Display for DockerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One reported group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerGroup {
    /// Which group.
    pub kind: DockerKind,
    /// Number of objects.
    pub count: u64,
    /// Bytes the daemon reports for them (inside the VM when [`DaemonInfo::in_vm`]); `None`
    /// when not measured (volumes: sizing them is slow and they're never offered).
    pub total_bytes: Option<u64>,
    /// Estimated bytes the proposed action would free, inside Docker's storage.
    pub reclaimable: Reclaimable,
    /// [`DockerKind::tier`].
    pub tier: Tier,
    /// [`DockerKind::action`]: proposed only, never run here.
    pub action: Action,
    /// Why the estimate is what it is.
    pub notes: Vec<EstimateNote>,
    /// Why this tier, in plain words.
    pub reason: String,
}

/// All images, for context (not a cleanup group).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImageTotals {
    /// Number of images (top-level, as `docker images` lists them).
    pub count: u64,
    /// Sum of their sizes. Overstates disk use: images share layers.
    pub bytes: u64,
}

/// The kind of VM disk image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VmDiskKind {
    /// Docker Desktop's sparse raw disk (`Docker.raw`, macOS and Linux).
    DockerDesktopRaw,
    /// Docker Desktop's older qcow2 disk (`Docker.qcow2`, macOS).
    DockerDesktopQcow2,
    /// Docker Desktop's WSL 2 data disk (`docker_data.vhdx`, `ext4.vhdx`, Windows).
    DockerDesktopWsl,
    /// Docker Desktop's Hyper-V disk (`DockerDesktop.vhdx`, Windows).
    DockerDesktopHyperV,
}

impl VmDiskKind {
    /// What happens to the host file after pruning inside the VM.
    pub fn caveat(self) -> &'static str {
        match self {
            Self::DockerDesktopRaw => {
                "Docker Desktop keeps images, containers, volumes and build cache in this sparse \
                 disk image. Pruning frees space inside the VM; the host file gives blocks back \
                 only as the VM discards them, which can take minutes and needs Docker Desktop \
                 running. Its logical size is the configured limit, not what it uses."
            }
            Self::DockerDesktopQcow2 => {
                "Docker Desktop keeps all Docker data in this qcow2 disk image. It does not \
                 shrink when images or build cache are pruned."
            }
            Self::DockerDesktopWsl | Self::DockerDesktopHyperV => {
                "Docker Desktop keeps all Docker data in this virtual disk. It grows as Docker \
                 writes data and does not shrink when images, containers or build cache are \
                 pruned: the freed space stays inside the VHDX until it is compacted."
            }
        }
    }

    /// The documented way to give the space back to the host. scull never runs these steps.
    pub fn compaction(self) -> &'static str {
        match self {
            Self::DockerDesktopRaw => {
                "Prune inside Docker (for example `docker builder prune`), leave Docker Desktop \
                 running for a few minutes, then measure free space again. See Docker \
                 Desktop's documentation on disk usage (https://docs.docker.com/desktop/)."
            }
            Self::DockerDesktopQcow2 => {
                "Switch Docker Desktop to the raw disk format (newer releases use it by \
                 default), which returns pruned space to the host. See \
                 https://docs.docker.com/desktop/."
            }
            Self::DockerDesktopWsl => {
                "After pruning: quit Docker Desktop, run `wsl --shutdown`, then compact the \
                 file with `Optimize-VHD -Path <file> -Mode Full` (Hyper-V PowerShell module) \
                 or with diskpart (`select vdisk file=<file>`, `attach vdisk readonly`, \
                 `compact vdisk`, `detach vdisk`). See \
                 https://learn.microsoft.com/windows/wsl/disk-space."
            }
            Self::DockerDesktopHyperV => {
                "After pruning: quit Docker Desktop, then compact the file with \
                 `Optimize-VHD -Path <file> -Mode Full` (Hyper-V PowerShell module, as \
                 administrator)."
            }
        }
    }
}

impl fmt::Display for VmDiskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DockerDesktopRaw => "Docker Desktop disk image (raw)",
            Self::DockerDesktopQcow2 => "Docker Desktop disk image (qcow2)",
            Self::DockerDesktopWsl => "Docker Desktop WSL 2 virtual disk",
            Self::DockerDesktopHyperV => "Docker Desktop Hyper-V virtual disk",
        })
    }
}

/// A VM disk image found on the host. Always inspect-only: never a cleanup candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmDisk {
    /// Which kind.
    pub kind: VmDiskKind,
    /// The file.
    pub path: PathBuf,
    /// Its logical size (for a sparse file, the size limit).
    pub logical: u64,
    /// Bytes actually allocated on the host, when the platform reports it.
    pub allocated: Option<u64>,
    /// Always [`Tier::S3`].
    pub tier: Tier,
    /// Always [`Action::None`]: deleting it deletes every image, container and volume.
    pub action: Action,
}

/// Everything [`inspect_docker`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerReport {
    /// Whether and how the daemon was reached.
    pub status: DockerStatus,
    /// The groups whose section answered, in [`DockerKind::ALL`] order.
    pub groups: Vec<DockerGroup>,
    /// All images, when `/images/json` answered.
    pub images: Option<ImageTotals>,
    /// Sections that failed while connected, with the reason.
    pub unavailable: Vec<(DockerKind, String)>,
    /// VM disk images on the host (found whether or not the daemon runs).
    pub vm_disks: Vec<VmDisk>,
}

impl DockerReport {
    /// The group of `kind`, if its section answered.
    pub fn group(&self, kind: DockerKind) -> Option<&DockerGroup> {
        self.groups.iter().find(|g| g.kind == kind)
    }

    /// Whether the in-daemon figures are bytes inside a VM disk image.
    pub fn in_vm(&self) -> bool {
        matches!(&self.status, DockerStatus::Connected { daemon, .. } if daemon.in_vm)
    }
}

/// Inspect Docker with the real transport (module docs). Read-only; never fails: every problem
/// is part of the report.
pub fn inspect_docker(
    env: &CacheEnv,
    platform: &dyn Platform,
    reader: Option<&dyn ConfigReader>,
    opts: &DockerOptions,
) -> DockerReport {
    inspect_docker_with(&NativeTransport, env, platform, reader, opts)
}

/// [`inspect_docker`] with a given transport (tests use a fake one).
pub fn inspect_docker_with(
    transport: &dyn DockerTransport,
    env: &CacheEnv,
    platform: &dyn Platform,
    reader: Option<&dyn ConfigReader>,
    opts: &DockerOptions,
) -> DockerReport {
    let vm_disks = locate_vm_disks(env, platform);
    let mut report = DockerReport {
        status: DockerStatus::NotRunning { tried: Vec::new() },
        groups: Vec::new(),
        images: None,
        unavailable: Vec::new(),
        vm_disks,
    };
    let targets = match docker_targets(env, platform, reader, opts.system_endpoints) {
        Ok(t) => t,
        Err(refused) => {
            report.status = DockerStatus::Refused(refused);
            return report;
        }
    };
    let (target, failures) = ping_first(transport, &targets, opts.ping_timeout);
    let Some(target) = target else {
        report.status = status_from_failures(failures);
        return report;
    };

    let daemon = transport
        .get(&target.endpoint, "/version", opts.request_timeout)
        .ok()
        .map(|r| daemon_info(&r.body, env.os, &target.endpoint))
        .unwrap_or_else(|| DaemonInfo {
            in_vm: guess_in_vm(env.os, "", "", &target.endpoint),
            ..DaemonInfo::default()
        });
    let in_vm = daemon.in_vm;

    let get = |path: &str| -> Result<Vec<u8>, String> {
        transport
            .get(&target.endpoint, path, opts.request_timeout)
            .map(|r| r.body)
            .map_err(|e| e.to_string())
    };
    let (build, containers, images, volumes) = std::thread::scope(|s| {
        let b = s.spawn(|| get("/system/df?type=build-cache"));
        let c = s.spawn(|| get("/system/df?type=container"));
        let i = s.spawn(|| get("/images/json"));
        let v = s.spawn(|| get("/volumes"));
        let join = |h: std::thread::ScopedJoinHandle<'_, Result<Vec<u8>, String>>| {
            h.join()
                .unwrap_or_else(|_| Err("internal error while querying".into()))
        };
        (join(b), join(c), join(i), join(v))
    });

    let mut push = |kind: DockerKind, r: Result<DockerGroup, String>| match r {
        Ok(g) => report.groups.push(g),
        Err(e) => report.unavailable.push((kind, e)),
    };
    push(
        DockerKind::BuildCache,
        build.and_then(|b| parse_build_cache(&b, in_vm)),
    );
    match images.and_then(|b| parse_images(&b, in_vm)) {
        Ok((totals, dangling)) => {
            report.images = Some(totals);
            push(DockerKind::DanglingImages, Ok(dangling));
        }
        Err(e) => push(DockerKind::DanglingImages, Err(e)),
    }
    push(
        DockerKind::StoppedContainers,
        containers.and_then(|b| parse_containers(&b, in_vm)),
    );
    push(DockerKind::Volumes, volumes.and_then(|b| parse_volumes(&b)));
    report.status = DockerStatus::Connected { target, daemon };
    report
}

/// Ping each target in order; the first that answers wins.
fn ping_first(
    transport: &dyn DockerTransport,
    targets: &[DockerTarget],
    timeout: Duration,
) -> (Option<DockerTarget>, Vec<(DockerTarget, DockerError)>) {
    let mut failures = Vec::new();
    for t in targets {
        match transport.get(&t.endpoint, "/_ping", timeout) {
            Ok(_) => return (Some(t.clone()), failures),
            Err(e) => failures.push((t.clone(), e)),
        }
    }
    (None, failures)
}

/// The most useful status when no target answered: permission problems first, then silence,
/// then other errors; "not running" only when every target was simply absent or refusing.
fn status_from_failures(failures: Vec<(DockerTarget, DockerError)>) -> DockerStatus {
    let rank = |e: &DockerError| match e {
        DockerError::PermissionDenied => 0,
        DockerError::TimedOut => 1,
        DockerError::NotFound | DockerError::Refused => 3,
        _ => 2,
    };
    let best = failures
        .iter()
        .min_by_key(|(_, e)| rank(e))
        .map(|(t, e)| (t.clone(), rank(e), e.to_string()));
    match best {
        Some((target, 0, _)) => DockerStatus::NotAccessible { target },
        Some((target, 1, _)) => DockerStatus::NotResponding { target },
        Some((target, 2, reason)) => DockerStatus::Failed { target, reason },
        _ => DockerStatus::NotRunning {
            tried: failures.into_iter().map(|(t, _)| t).collect(),
        },
    }
}

/// The endpoints to try, in order (module docs), or the configured endpoint that was refused.
///
/// Default Unix sockets are listed only if something exists at the path (a socket, or a link
/// such as macOS's `/var/run/docker.sock`); nothing is connected here.
pub fn docker_targets(
    env: &CacheEnv,
    platform: &dyn Platform,
    reader: Option<&dyn ConfigReader>,
    system_endpoints: bool,
) -> Result<Vec<DockerTarget>, RefusedEndpoint> {
    if let Some(host) = env.var("DOCKER_HOST") {
        let source = EndpointSource::DockerHost;
        return match configured(&host, env) {
            Ok(endpoint) => Ok(vec![DockerTarget { endpoint, source }]),
            Err(error) => Err(RefusedEndpoint { source, error }),
        };
    }
    if let Some(name) = current_context(env, reader).filter(|n| n != "default")
        && let Some(host) = context_host(env, reader, &name)
    {
        let source = EndpointSource::Context(name);
        return match configured(&host, env) {
            Ok(endpoint) => Ok(vec![DockerTarget { endpoint, source }]),
            Err(error) => Err(RefusedEndpoint { source, error }),
        };
    }
    let target = |endpoint| DockerTarget {
        endpoint,
        source: EndpointSource::Default,
    };
    let h = &env.home;
    let mut sockets: Vec<PathBuf> = Vec::new();
    match env.os {
        Os::Windows => {
            if !system_endpoints {
                return Ok(Vec::new());
            }
            return Ok([
                r"\\.\pipe\docker_engine",
                r"\\.\pipe\dockerDesktopLinuxEngine",
            ]
            .iter()
            .map(|p| target(DockerEndpoint::NamedPipe((*p).to_owned())))
            .collect());
        }
        Os::Linux => {
            if system_endpoints {
                sockets.push(PathBuf::from("/var/run/docker.sock"));
            }
            if let Some(run) = env.abs_var("XDG_RUNTIME_DIR") {
                sockets.push(run.join("docker.sock")); // rootless Docker
            }
            sockets.push(h.join(".docker").join("desktop").join("docker.sock"));
        }
        Os::Macos => {
            sockets.push(h.join(".docker").join("run").join("docker.sock"));
            if system_endpoints {
                sockets.push(PathBuf::from("/var/run/docker.sock"));
            }
            sockets.push(h.join(".colima").join("default").join("docker.sock"));
            sockets.push(h.join(".orbstack").join("run").join("docker.sock"));
        }
    }
    Ok(sockets
        .into_iter()
        .filter(|p| {
            platform
                .metadata(p)
                .is_ok_and(|md| matches!(md.kind, EntryKind::Socket | EntryKind::Symlink))
        })
        .map(|p| target(DockerEndpoint::Unix(p)))
        .collect())
}

/// Parse a configured endpoint; TCP with TLS verification is refused (no TLS support).
fn configured(host: &str, env: &CacheEnv) -> Result<DockerEndpoint, EndpointError> {
    let endpoint = DockerEndpoint::parse(host)?;
    if matches!(endpoint, DockerEndpoint::Tcp(_)) && env.var("DOCKER_TLS_VERIFY").is_some() {
        return Err(EndpointError::Unsupported {
            url: host.to_owned(),
            reason: "TLS endpoints are not supported".into(),
        });
    }
    Ok(endpoint)
}

/// `$DOCKER_CONFIG`, or `~/.docker`.
fn docker_config_dir(env: &CacheEnv) -> PathBuf {
    env.abs_var("DOCKER_CONFIG")
        .unwrap_or_else(|| env.home.join(".docker"))
}

/// `DOCKER_CONTEXT`, or `currentContext` from `config.json`.
fn current_context(env: &CacheEnv, reader: Option<&dyn ConfigReader>) -> Option<String> {
    if let Some(name) = env.var("DOCKER_CONTEXT") {
        return Some(name);
    }
    #[derive(Deserialize)]
    struct Config {
        #[serde(rename = "currentContext", default)]
        current_context: Option<String>,
    }
    let text = reader?.read_text(
        &docker_config_dir(env).join("config.json"),
        MAX_CONFIG_BYTES,
    )?;
    serde_json::from_str::<Config>(&text)
        .ok()?
        .current_context
        .filter(|s| !s.is_empty())
}

/// `Endpoints.docker.Host` of context `name`, from
/// `contexts/meta/<sha256(name)>/meta.json` in the Docker config folder.
fn context_host(env: &CacheEnv, reader: Option<&dyn ConfigReader>, name: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Meta {
        #[serde(rename = "Endpoints", default)]
        endpoints: Endpoints,
    }
    #[derive(Deserialize, Default)]
    struct Endpoints {
        docker: Option<Endpoint>,
    }
    #[derive(Deserialize)]
    struct Endpoint {
        #[serde(rename = "Host")]
        host: Option<String>,
    }
    let digest = Sha256::digest(name.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let path = docker_config_dir(env)
        .join("contexts")
        .join("meta")
        .join(hex)
        .join("meta.json");
    let text = reader?.read_text(&path, MAX_CONFIG_BYTES)?;
    serde_json::from_str::<Meta>(&text)
        .ok()?
        .endpoints
        .docker?
        .host
        .filter(|h| !h.is_empty())
}

/// Daemon-supplied text, without control characters, at most 120 characters.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(120).collect()
}

/// Whether the daemon's storage is inside a VM disk image.
fn guess_in_vm(os: Os, platform: &str, kernel: &str, endpoint: &DockerEndpoint) -> bool {
    let kernel = kernel.to_ascii_lowercase();
    os != Os::Linux
        || platform.contains("Docker Desktop")
        || kernel.contains("linuxkit")
        || kernel.contains("microsoft")
        || matches!(endpoint, DockerEndpoint::Unix(p)
            if p.components().any(|c| c.as_os_str() == "desktop")
                && p.components().any(|c| c.as_os_str() == ".docker"))
}

/// Parse `GET /version`.
fn daemon_info(body: &[u8], os: Os, endpoint: &DockerEndpoint) -> DaemonInfo {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Version {
        #[serde(rename = "Platform")]
        platform: Option<Named>,
        #[serde(rename = "Version")]
        version: String,
        #[serde(rename = "ApiVersion")]
        api_version: String,
        #[serde(rename = "KernelVersion")]
        kernel: String,
    }
    #[derive(Deserialize, Default)]
    struct Named {
        #[serde(rename = "Name", default)]
        name: String,
    }
    let v: Version = serde_json::from_slice(body).unwrap_or_default();
    let platform = clean(&v.platform.unwrap_or_default().name);
    let kernel = clean(&v.kernel);
    DaemonInfo {
        in_vm: guess_in_vm(os, &platform, &kernel, endpoint),
        version: clean(&v.version),
        api_version: clean(&v.api_version),
        platform,
        kernel,
    }
}

/// A non-negative byte count from Docker's signed sizes (`-1` = not computed).
fn bytes(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// Inside a VM, nothing about host bytes is known until measured (PRD §9).
fn vm_adjust(confidence: Confidence, in_vm: bool, notes: &mut Vec<EstimateNote>) -> Confidence {
    if in_vm {
        notes.push(EstimateNote::InsideVmImage);
        Confidence::Unknown
    } else {
        confidence
    }
}

const VM_SUFFIX: &str = " These bytes are inside Docker's VM disk image: the host file shrinks \
                         only after the VM releases them or the disk is compacted.";

/// A group of `kind`; inside a VM the confidence becomes unknown and the reason says why.
fn group(
    kind: DockerKind,
    count: u64,
    total: Option<u64>,
    reclaimable: Reclaimable,
    in_vm: bool,
    reason: &str,
) -> DockerGroup {
    let mut notes = Vec::new();
    let in_vm = in_vm && kind != DockerKind::Volumes;
    let confidence = vm_adjust(reclaimable.confidence, in_vm, &mut notes);
    let mut reason = reason.to_owned();
    if in_vm {
        reason.push_str(VM_SUFFIX);
    }
    DockerGroup {
        kind,
        count,
        total_bytes: total,
        reclaimable: Reclaimable {
            confidence,
            ..reclaimable
        },
        tier: kind.tier(),
        action: kind.action(),
        notes,
        reason,
    }
}

/// Shorthand for a [`Reclaimable`].
fn reclaim(bytes: u64, confidence: Confidence, shared_bytes: u64) -> Reclaimable {
    Reclaimable {
        bytes,
        confidence,
        shared_bytes,
    }
}

/// Parse `GET /system/df?type=build-cache` (legacy `BuildCache` array, or the newer
/// `BuildCacheUsage.Items`).
pub fn parse_build_cache(body: &[u8], in_vm: bool) -> Result<DockerGroup, String> {
    #[derive(Deserialize)]
    struct Record {
        #[serde(rename = "Size", default)]
        size: i64,
        #[serde(rename = "InUse", default)]
        in_use: bool,
        #[serde(rename = "Shared", default)]
        shared: bool,
    }
    let records: Vec<Record> = df_items(body, "BuildCache", "BuildCacheUsage")?;
    let total: u64 = records.iter().map(|r| bytes(r.size)).sum();
    let reclaimable: u64 = records
        .iter()
        .filter(|r| !r.in_use && !r.shared)
        .map(|r| bytes(r.size))
        .sum();
    let shared: u64 = records
        .iter()
        .filter(|r| r.shared && !r.in_use)
        .map(|r| bytes(r.size))
        .sum();
    Ok(group(
        DockerKind::BuildCache,
        records.len() as u64,
        Some(total),
        reclaim(reclaimable, Confidence::Medium, shared),
        in_vm,
        "Build cache is regenerated by the next build. Records in use or shared with other \
         records are not counted as reclaimable.",
    ))
}

/// Parse `GET /system/df?type=container` (legacy `Containers` array, or the newer
/// `ContainerUsage.Items`).
pub fn parse_containers(body: &[u8], in_vm: bool) -> Result<DockerGroup, String> {
    #[derive(Deserialize)]
    struct Container {
        #[serde(rename = "State", default)]
        state: String,
        #[serde(rename = "SizeRw", default)]
        size_rw: Option<i64>,
    }
    let all: Vec<Container> = df_items(body, "Containers", "ContainerUsage")?;
    let stopped: Vec<&Container> = all
        .iter()
        .filter(|c| matches!(c.state.as_str(), "exited" | "created" | "dead"))
        .collect();
    let sized = stopped.iter().all(|c| c.size_rw.is_some());
    let total: u64 = stopped.iter().filter_map(|c| c.size_rw).map(bytes).sum();
    Ok(group(
        DockerKind::StoppedContainers,
        stopped.len() as u64,
        Some(total),
        reclaim(
            total,
            if sized {
                Confidence::Medium
            } else {
                Confidence::Unknown
            },
            0,
        ),
        in_vm,
        "A stopped container's writable layer may hold data that exists nowhere else (logs, \
         files written at run time). Never preselected; review with `docker ps -a`.",
    ))
}

/// Parse `GET /images/json`: totals for all images and the dangling-images group.
pub fn parse_images(body: &[u8], in_vm: bool) -> Result<(ImageTotals, DockerGroup), String> {
    #[derive(Deserialize)]
    struct Image {
        #[serde(rename = "RepoTags", default)]
        repo_tags: Option<Vec<String>>,
        #[serde(rename = "Size", default)]
        size: i64,
        #[serde(rename = "SharedSize", default = "minus_one")]
        shared_size: i64,
        #[serde(rename = "Containers", default = "minus_one")]
        containers: i64,
    }
    fn minus_one() -> i64 {
        -1
    }
    let images: Vec<Image> =
        serde_json::from_slice(body).map_err(|e| format!("unexpected /images/json: {e}"))?;
    let totals = ImageTotals {
        count: images.len() as u64,
        bytes: images.iter().map(|i| bytes(i.size)).sum(),
    };
    let dangling: Vec<&Image> = images
        .iter()
        .filter(|i| {
            i.repo_tags
                .as_ref()
                .is_none_or(|t| t.iter().all(|t| t == "<none>:<none>"))
        })
        .collect();
    let total: u64 = dangling.iter().map(|i| bytes(i.size)).sum();
    // `docker image prune` skips images a container uses.
    let removable: Vec<&&Image> = dangling.iter().filter(|i| i.containers <= 0).collect();
    let shared_known = removable.iter().all(|i| i.shared_size >= 0);
    let shared: u64 = removable.iter().map(|i| bytes(i.shared_size)).sum();
    let freed: u64 = removable
        .iter()
        .map(|i| bytes(i.size).saturating_sub(bytes(i.shared_size)))
        .sum();
    let usage_known = removable.iter().all(|i| i.containers == 0);
    let confidence = if shared_known && usage_known {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    let mut reason = String::from(
        "Untagged images left behind by rebuilds and pulls; rebuilt or re-pulled if needed.",
    );
    let used = dangling.len() - removable.len();
    if used > 0 {
        reason.push_str(&format!(
            " {used} of them are used by containers and would be kept."
        ));
    }
    if !shared_known {
        reason.push_str(" Layers may be shared with other images, so less may be freed.");
    }
    Ok((
        totals,
        group(
            DockerKind::DanglingImages,
            dangling.len() as u64,
            Some(total),
            reclaim(freed, confidence, shared),
            in_vm,
            &reason,
        ),
    ))
}

/// Parse `GET /volumes`: a count only. Volumes are protected (S3) and never offered.
pub fn parse_volumes(body: &[u8]) -> Result<DockerGroup, String> {
    #[derive(Deserialize)]
    struct Volumes {
        #[serde(rename = "Volumes", default)]
        volumes: Option<Vec<serde_json::Value>>,
    }
    let v: Volumes =
        serde_json::from_slice(body).map_err(|e| format!("unexpected /volumes: {e}"))?;
    let count = v.volumes.map_or(0, |v| v.len()) as u64;
    Ok(group(
        DockerKind::Volumes,
        count,
        None,
        reclaim(0, Confidence::Unknown, 0),
        false,
        "Volumes hold application data (databases, uploads) that may exist nowhere else. \
         Protected: never offered for cleanup.",
    ))
}

/// The item list of a `/system/df?type=...` answer: `legacy` array, else `usage.Items`.
fn df_items<T: for<'de> Deserialize<'de>>(
    body: &[u8],
    legacy: &str,
    usage: &str,
) -> Result<Vec<T>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("unexpected /system/df: {e}"))?;
    let items = match v.get(legacy) {
        Some(a) if a.is_array() => a.clone(),
        _ => match v.get(usage).and_then(|u| u.get("Items")) {
            Some(a) if a.is_array() => a.clone(),
            _ => serde_json::Value::Array(Vec::new()),
        },
    };
    serde_json::from_value(items).map_err(|e| format!("unexpected /system/df {legacy}: {e}"))
}

/// Docker Desktop's VM disk images for `env`, read-only (no-follow metadata; links and
/// anything but a regular file are skipped). Always S3, [`Action::None`].
pub fn locate_vm_disks(env: &CacheEnv, platform: &dyn Platform) -> Vec<VmDisk> {
    let h = &env.home;
    let candidates: Vec<(VmDiskKind, PathBuf)> = match env.os {
        Os::Macos => {
            let data = h
                .join("Library")
                .join("Containers")
                .join("com.docker.docker")
                .join("Data")
                .join("vms")
                .join("0")
                .join("data");
            vec![
                (VmDiskKind::DockerDesktopRaw, data.join("Docker.raw")),
                (VmDiskKind::DockerDesktopQcow2, data.join("Docker.qcow2")),
            ]
        }
        Os::Linux => vec![(
            VmDiskKind::DockerDesktopRaw,
            h.join(".docker")
                .join("desktop")
                .join("vms")
                .join("0")
                .join("data")
                .join("Docker.raw"),
        )],
        Os::Windows => {
            let wsl = env.local_app_data().join("Docker").join("wsl");
            let program_data = env
                .abs_var("ProgramData")
                .or_else(|| env.abs_var("PROGRAMDATA"))
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
            vec![
                (
                    VmDiskKind::DockerDesktopWsl,
                    wsl.join("disk").join("docker_data.vhdx"),
                ),
                (
                    VmDiskKind::DockerDesktopWsl,
                    wsl.join("data").join("ext4.vhdx"),
                ),
                (
                    VmDiskKind::DockerDesktopHyperV,
                    program_data
                        .join("DockerDesktop")
                        .join("vm-data")
                        .join("DockerDesktop.vhdx"),
                ),
            ]
        }
    };
    candidates
        .into_iter()
        .filter_map(|(kind, path)| {
            let md = platform.metadata(&path).ok()?;
            (md.kind == EntryKind::File).then_some(VmDisk {
                kind,
                logical: md.logical_size,
                allocated: md.allocated_size,
                path,
                tier: Tier::S3,
                action: Action::None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Builds the error a fake request fails with.
    type ErrFn = fn() -> DockerError;

    /// Serves canned bodies by path; records every request.
    #[derive(Default)]
    struct Fake {
        replies: BTreeMap<String, Result<String, ErrFn>>,
        log: Mutex<Vec<String>>,
    }

    impl Fake {
        fn ok(mut self, path: &str, body: &str) -> Self {
            self.replies.insert(path.into(), Ok(body.into()));
            self
        }
        fn err(mut self, path: &str, e: ErrFn) -> Self {
            self.replies.insert(path.into(), Err(e));
            self
        }
        fn requests(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl DockerTransport for Fake {
        fn get(
            &self,
            endpoint: &DockerEndpoint,
            path: &str,
            _timeout: Duration,
        ) -> Result<DockerResponse, DockerError> {
            self.log.lock().unwrap().push(format!("{endpoint} {path}"));
            // The real transport refuses these; the detector must never ask.
            assert!(
                scull_platform::docker::is_read_only_path(path),
                "mutating path {path}"
            );
            match self.replies.get(path) {
                Some(Ok(b)) => Ok(DockerResponse {
                    api_version: Some("1.47".into()),
                    body: b.clone().into_bytes(),
                }),
                Some(Err(e)) => Err(e()),
                None => Err(DockerError::NotFound),
            }
        }
    }

    /// A platform on which nothing exists (never touches the real home).
    struct NoFs;
    const MISSING: &str = "/nonexistent-scull-test/none";
    impl Platform for NoFs {
        fn capabilities(&self) -> scull_platform::Capabilities {
            scull_platform::Native.capabilities()
        }
        fn metadata(
            &self,
            _path: &std::path::Path,
        ) -> scull_platform::Result<scull_platform::Metadata> {
            scull_platform::Native.metadata(std::path::Path::new(MISSING))
        }
        fn read_dir<'a>(
            &'a self,
            _path: &std::path::Path,
        ) -> scull_platform::Result<scull_platform::DirIter<'a>> {
            scull_platform::Native.read_dir(std::path::Path::new(MISSING))
        }
    }

    fn env(os: Os, vars: &[(&str, &str)]) -> CacheEnv {
        CacheEnv::with_vars("/home/u", os, vars.iter().map(|(k, v)| (*k, *v)))
    }

    fn opts() -> DockerOptions {
        DockerOptions {
            system_endpoints: false,
            ..DockerOptions::default()
        }
    }

    const BUILD_CACHE: &str = r#"{"BuildCache":[
        {"ID":"a","Type":"regular","InUse":false,"Shared":false,"Size":1000},
        {"ID":"b","Type":"regular","InUse":true,"Shared":false,"Size":200},
        {"ID":"c","Type":"regular","InUse":false,"Shared":true,"Size":30},
        {"ID":"d","Type":"source.local","InUse":false,"Shared":false,"Size":4}]}"#;
    const CONTAINERS: &str = r#"{"Containers":[
        {"Id":"1","State":"running","SizeRw":5000},
        {"Id":"2","State":"exited","SizeRw":70},
        {"Id":"3","State":"created","SizeRw":0},
        {"Id":"4","State":"dead","SizeRw":8},
        {"Id":"5","State":"paused","SizeRw":900}]}"#;
    const IMAGES: &str = r#"[
        {"Id":"t","RepoTags":["app:1"],"Size":3000,"SharedSize":-1,"Containers":1},
        {"Id":"d1","RepoTags":[],"Size":500,"SharedSize":-1,"Containers":0},
        {"Id":"d2","RepoTags":["<none>:<none>"],"Size":250,"SharedSize":-1,"Containers":0},
        {"Id":"d3","RepoTags":null,"Size":100,"SharedSize":-1,"Containers":2}]"#;
    const VOLUMES: &str = r#"{"Volumes":[{"Name":"pgdata"},{"Name":"abc123"}],"Warnings":null}"#;
    const VERSION: &str = r#"{"Platform":{"Name":"Docker Engine - Community"},"Version":"27.3.1",
        "ApiVersion":"1.47","Os":"linux","KernelVersion":"6.8.0-generic"}"#;

    fn full_daemon() -> Fake {
        Fake::default()
            .ok("/_ping", "OK")
            .ok("/version", VERSION)
            .ok("/system/df?type=build-cache", BUILD_CACHE)
            .ok("/system/df?type=container", CONTAINERS)
            .ok("/images/json", IMAGES)
            .ok("/volumes", VOLUMES)
    }

    fn docker_host(e: &str) -> Vec<(&'static str, String)> {
        vec![("DOCKER_HOST", e.to_owned())]
    }

    fn inspect(fake: &Fake, os: Os, host: &str) -> DockerReport {
        let vars = docker_host(host);
        let env = CacheEnv::with_vars("/home/u", os, vars);
        inspect_docker_with(fake, &env, &NoFs, None, &opts())
    }

    #[test]
    fn reports_every_group_with_tiers_and_proposed_actions() {
        let fake = full_daemon();
        let r = inspect(&fake, Os::Linux, "unix:///run/fake/docker.sock");
        let DockerStatus::Connected { target, daemon } = &r.status else {
            panic!("{:?}", r.status)
        };
        assert_eq!(target.source, EndpointSource::DockerHost);
        assert_eq!(daemon.version, "27.3.1");
        assert!(!daemon.in_vm);
        assert!(r.unavailable.is_empty(), "{:?}", r.unavailable);

        let b = r.group(DockerKind::BuildCache).unwrap();
        assert_eq!(
            (b.count, b.total_bytes, b.reclaimable.bytes),
            (4, Some(1234), 1004)
        );
        assert_eq!(b.reclaimable.shared_bytes, 30);
        assert_eq!(b.tier, Tier::S0);
        assert_eq!(
            b.action,
            Action::ToolCommand {
                tool: "docker".into(),
                args: vec![
                    "builder".into(),
                    "prune".into(),
                    "--all".into(),
                    "--force".into()
                ]
            }
        );

        let d = r.group(DockerKind::DanglingImages).unwrap();
        assert_eq!(
            (d.count, d.total_bytes, d.reclaimable.bytes),
            (3, Some(850), 750)
        );
        assert_eq!(d.reclaimable.confidence, Confidence::Low); // shared size unknown
        assert_eq!(d.tier, Tier::S1);
        assert!(
            d.reason.contains("1 of them are used by containers"),
            "{}",
            d.reason
        );
        assert_eq!(
            r.images,
            Some(ImageTotals {
                count: 4,
                bytes: 3850
            })
        );

        let c = r.group(DockerKind::StoppedContainers).unwrap();
        assert_eq!((c.count, c.reclaimable.bytes), (3, 78));
        assert_eq!((c.tier, &c.action), (Tier::S2, &Action::None));

        let v = r.group(DockerKind::Volumes).unwrap();
        assert_eq!((v.count, v.total_bytes), (2, None));
        assert_eq!((v.tier, &v.action), (Tier::S3, &Action::None));

        // Every proposed action is allowed at its tier (S2/S3: nothing).
        for g in &r.groups {
            assert!(g.action.allowed_for(g.tier), "{:?}", g.kind);
        }
        assert_eq!(
            fake.requests().len(),
            6,
            "ping, version and four sections: {:?}",
            fake.requests()
        );
    }

    #[test]
    fn never_calls_anything_but_read_only_endpoints() {
        let fake = full_daemon();
        inspect(&fake, Os::Linux, "unix:///run/fake/docker.sock");
        for req in fake.requests() {
            let path = req.split_once(' ').unwrap().1;
            assert!(scull_platform::docker::is_read_only_path(path), "{req}");
            assert!(!path.contains("prune"), "{req}");
        }
    }

    #[test]
    fn newer_df_shapes_are_understood() {
        let b = parse_build_cache(
            br#"{"BuildCacheUsage":{"TotalSize":9,"Items":[{"Size":9,"InUse":false,"Shared":false}]}}"#,
            false,
        )
        .unwrap();
        assert_eq!((b.count, b.reclaimable.bytes), (1, 9));
        let c = parse_containers(
            br#"{"ContainerUsage":{"Items":[{"State":"exited","SizeRw":11}]}}"#,
            false,
        )
        .unwrap();
        assert_eq!((c.count, c.reclaimable.bytes), (1, 11));
        // An empty daemon: no lists at all.
        assert_eq!(parse_build_cache(b"{}", false).unwrap().count, 0);
        assert_eq!(parse_volumes(br#"{"Volumes":null}"#).unwrap().count, 0);
        assert!(parse_images(b"not json", false).is_err());
    }

    #[test]
    fn known_shared_size_raises_confidence() {
        let (_, d) = parse_images(
            br#"[{"RepoTags":[],"Size":500,"SharedSize":200,"Containers":0}]"#,
            false,
        )
        .unwrap();
        assert_eq!(d.reclaimable.bytes, 300);
        assert_eq!(d.reclaimable.shared_bytes, 200);
        assert_eq!(d.reclaimable.confidence, Confidence::Medium);
    }

    #[test]
    fn unsized_containers_have_unknown_confidence() {
        let c = parse_containers(br#"{"Containers":[{"State":"exited"}]}"#, false).unwrap();
        assert_eq!(c.reclaimable.confidence, Confidence::Unknown);
    }

    #[test]
    fn docker_desktop_figures_are_in_vm_bytes() {
        let fake = full_daemon().ok(
            "/version",
            r#"{"Platform":{"Name":"Docker Desktop 4.36.0 (175267)"},"Version":"27.3.1",
                "ApiVersion":"1.47","KernelVersion":"6.10.14-linuxkit"}"#,
        );
        let r = inspect(
            &fake,
            Os::Linux,
            "unix:///home/u/.docker/desktop/docker.sock",
        );
        assert!(r.in_vm());
        for g in r.groups.iter().filter(|g| g.kind != DockerKind::Volumes) {
            assert_eq!(
                g.reclaimable.confidence,
                Confidence::Unknown,
                "{:?}",
                g.kind
            );
            assert_eq!(g.notes, vec![EstimateNote::InsideVmImage]);
            assert!(g.reason.contains("VM disk image"), "{}", g.reason);
        }
        // On macOS and Windows the daemon is always in a VM, whatever it says.
        for os in [Os::Macos, Os::Windows] {
            let r = inspect(&full_daemon(), os, "tcp://127.0.0.1:2375");
            assert!(r.in_vm(), "{os:?}");
        }
    }

    #[test]
    fn one_slow_section_does_not_hide_the_others() {
        let fake = full_daemon().err("/images/json", || DockerError::TimedOut);
        let r = inspect(&fake, Os::Linux, "unix:///run/fake/docker.sock");
        assert!(matches!(r.status, DockerStatus::Connected { .. }));
        assert!(r.group(DockerKind::DanglingImages).is_none());
        assert!(r.images.is_none());
        assert_eq!(r.unavailable.len(), 1);
        assert_eq!(r.unavailable[0].0, DockerKind::DanglingImages);
        assert!(r.unavailable[0].1.contains("did not answer in time"));
        assert!(r.group(DockerKind::BuildCache).is_some());
    }

    #[test]
    fn absent_docker_degrades_to_not_running() {
        // No DOCKER_HOST, no system endpoints, nothing at the home sockets.
        let env = env(Os::Linux, &[]);
        let fake = Fake::default();
        let r = inspect_docker_with(&fake, &env, &NoFs, None, &opts());
        assert_eq!(r.status, DockerStatus::NotRunning { tried: vec![] });
        assert!(r.groups.is_empty());
        assert!(fake.requests().is_empty(), "nothing to connect to");
        assert!(r.status.summary().contains("Docker not found"));
    }

    #[test]
    fn connection_failures_map_to_statuses() {
        let host = "unix:///run/fake/docker.sock";
        let cases: [(ErrFn, &str); 5] = [
            (|| DockerError::NotFound, "not running"),
            (|| DockerError::Refused, "not running"),
            (|| DockerError::PermissionDenied, "permission denied"),
            (|| DockerError::TimedOut, "did not answer"),
            (
                || DockerError::Protocol("garbage".into()),
                "could not be inspected",
            ),
        ];
        for (e, want) in cases {
            let fake = Fake::default().err("/_ping", e);
            let r = inspect(&fake, Os::Linux, host);
            assert!(r.groups.is_empty());
            assert!(r.status.summary().contains(want), "{}", r.status.summary());
            assert_eq!(fake.requests().len(), 1, "only the ping");
        }
    }

    #[test]
    fn remote_and_tls_endpoints_are_refused_without_connecting() {
        for (vars, want) in [
            (vec![("DOCKER_HOST", "tcp://10.1.2.3:2375")], "remote"),
            (vec![("DOCKER_HOST", "ssh://me@build-box")], "remote"),
            (
                vec![
                    ("DOCKER_HOST", "tcp://127.0.0.1:2376"),
                    ("DOCKER_TLS_VERIFY", "1"),
                ],
                "TLS",
            ),
        ] {
            let env = env(Os::Linux, &vars);
            let fake = Fake::default();
            let r = inspect_docker_with(&fake, &env, &NoFs, None, &opts());
            let DockerStatus::Refused(refused) = &r.status else {
                panic!("{:?}", r.status)
            };
            assert_eq!(refused.source, EndpointSource::DockerHost);
            assert!(r.status.summary().contains(want), "{}", r.status.summary());
            assert!(fake.requests().is_empty());
        }
    }

    #[test]
    fn docker_contexts_are_followed() {
        struct Files(BTreeMap<PathBuf, String>);
        impl ConfigReader for Files {
            fn read_text(&self, path: &std::path::Path, _max: u64) -> Option<String> {
                self.0.get(path).cloned()
            }
        }
        // sha256("desktop-linux")
        let digest: String = Sha256::digest(b"desktop-linux")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let cfg = PathBuf::from("/home/u/.docker");
        let files = Files(BTreeMap::from([
            (
                cfg.join("config.json"),
                r#"{"auths":{},"currentContext":"desktop-linux"}"#.to_owned(),
            ),
            (
                cfg.join("contexts/meta").join(&digest).join("meta.json"),
                r#"{"Name":"desktop-linux","Endpoints":{"docker":{"Host":"unix:///home/u/.docker/desktop/docker.sock","SkipTLSVerify":false}}}"#.to_owned(),
            ),
        ]));
        let t = docker_targets(&env(Os::Linux, &[]), &NoFs, Some(&files), false).unwrap();
        assert_eq!(
            t,
            vec![DockerTarget {
                endpoint: DockerEndpoint::unix("/home/u/.docker/desktop/docker.sock"),
                source: EndpointSource::Context("desktop-linux".into()),
            }]
        );
        // DOCKER_HOST wins over the context; "default" means the default sockets.
        let t = docker_targets(
            &env(Os::Linux, &[("DOCKER_HOST", "unix:///x.sock")]),
            &NoFs,
            Some(&files),
            false,
        )
        .unwrap();
        assert_eq!(t[0].source, EndpointSource::DockerHost);
        let t = docker_targets(
            &env(Os::Linux, &[("DOCKER_CONTEXT", "default")]),
            &NoFs,
            Some(&files),
            false,
        )
        .unwrap();
        assert!(t.is_empty());
    }

    #[test]
    fn windows_defaults_are_local_named_pipes() {
        let t = docker_targets(&env(Os::Windows, &[]), &NoFs, None, true).unwrap();
        assert_eq!(
            t.iter().map(|t| t.endpoint.to_string()).collect::<Vec<_>>(),
            [
                "npipe:////./pipe/docker_engine",
                "npipe:////./pipe/dockerDesktopLinuxEngine"
            ]
        );
        assert!(
            docker_targets(&env(Os::Windows, &[]), &NoFs, None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn vm_disk_caveats_name_compaction_and_never_offer_the_file() {
        for kind in [
            VmDiskKind::DockerDesktopRaw,
            VmDiskKind::DockerDesktopQcow2,
            VmDiskKind::DockerDesktopWsl,
            VmDiskKind::DockerDesktopHyperV,
        ] {
            assert!(
                kind.caveat().contains("shrink") || kind.caveat().contains("gives blocks back")
            );
            assert!(!kind.compaction().is_empty());
        }
        assert!(
            VmDiskKind::DockerDesktopWsl
                .compaction()
                .contains("wsl --shutdown")
        );
        assert!(
            VmDiskKind::DockerDesktopWsl
                .compaction()
                .contains("Optimize-VHD")
        );
        // Nothing found under a home that doesn't exist.
        assert!(locate_vm_disks(&env(Os::Macos, &[]), &NoFs).is_empty());
    }
}
