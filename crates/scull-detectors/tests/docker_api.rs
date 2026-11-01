//! Docker via its API, end to end through the real transport (P5-DET-03): a fake Docker
//! daemon on a Unix socket in a temp dir answers canned JSON; no real daemon is ever reached
//! (`DOCKER_HOST` points at the fake, machine-wide default sockets are off).

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use scull_core::{Action, Confidence, Tier};
use scull_detectors::caches::CacheEnv;
use scull_detectors::rules::Platform as Os;
use scull_detectors::{
    DockerKind, DockerOptions, DockerStatus, EndpointSource, VmDiskKind, inspect_docker,
};
use scull_platform::Native;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const VERSION_ENGINE: &str = r#"{"Platform":{"Name":"Docker Engine - Community"},"Version":"27.3.1","ApiVersion":"1.47","Os":"linux","KernelVersion":"6.8.0-generic"}"#;
const VERSION_DESKTOP: &str = r#"{"Platform":{"Name":"Docker Desktop 4.36.0 (175267)"},"Version":"27.3.1","ApiVersion":"1.47","Os":"linux","KernelVersion":"6.10.14-linuxkit"}"#;
const BUILD_CACHE: &str = r#"{"BuildCache":[{"ID":"a","Type":"regular","InUse":false,"Shared":false,"Size":4000000000},{"ID":"b","Type":"regular","InUse":true,"Shared":false,"Size":100000000}]}"#;
const CONTAINERS: &str = r#"{"Containers":[{"Id":"1","State":"running","SizeRw":10},{"Id":"2","State":"exited","SizeRw":2000000}]}"#;
const IMAGES: &str = r#"[{"Id":"t","RepoTags":["app:1"],"Size":900000000,"SharedSize":-1,"Containers":1},{"Id":"d","RepoTags":[],"Size":300000000,"SharedSize":-1,"Containers":0}]"#;
const VOLUMES: &str = r#"{"Volumes":[{"Name":"pgdata"}],"Warnings":null}"#;

/// A temp dir with a short path (Unix socket paths are limited to ~104 bytes).
fn short_tempdir() -> tempfile::TempDir {
    let base = Path::new("/tmp");
    if base.is_dir() {
        tempfile::Builder::new()
            .prefix("scull-dk")
            .tempdir_in(base)
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    }
}

/// A fake daemon: answers each request by path, chunked like the real one, and records the
/// request lines. Unknown paths get a 404.
struct FakeDaemon {
    socket: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeDaemon {
    fn start(dir: &Path, version: &'static str) -> Self {
        let socket = dir.join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { break };
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    let mut r = BufReader::new(conn.try_clone().unwrap());
                    let mut first = String::new();
                    if r.read_line(&mut first).is_err() {
                        return;
                    }
                    let mut line = String::new();
                    while r.read_line(&mut line).unwrap_or(0) > 2 {
                        line.clear();
                    }
                    let path = first.split(' ').nth(1).unwrap_or_default().to_owned();
                    log.lock().unwrap().push(first.trim_end().to_owned());
                    let body = match path.as_str() {
                        "/_ping" => "OK",
                        "/version" => version,
                        "/system/df?type=build-cache" => BUILD_CACHE,
                        "/system/df?type=container" => CONTAINERS,
                        "/images/json" => IMAGES,
                        "/volumes" => VOLUMES,
                        _ => "",
                    };
                    let reply = if body.is_empty() {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 24\r\n\r\n{\"message\":\"not found\"}\n"
                            .to_owned()
                    } else {
                        // Two chunks, to exercise the decoder.
                        let (a, b) = body.split_at(body.len() / 2);
                        format!(
                            "HTTP/1.1 200 OK\r\nApi-Version: 1.47\r\nContent-Type: application/json\r\n\
                             Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                            a.len(),
                            b.len()
                        )
                    };
                    let _ = (&conn).write_all(reply.as_bytes());
                });
            }
        });
        Self { socket, requests }
    }

    fn env(&self, home: &Path) -> CacheEnv {
        CacheEnv::with_vars(
            home,
            Os::Linux,
            [(
                "DOCKER_HOST".to_owned(),
                format!("unix://{}", self.socket.display()),
            )],
        )
    }
}

fn opts() -> DockerOptions {
    DockerOptions {
        system_endpoints: false,
        ..DockerOptions::default()
    }
}

#[test]
fn reads_docker_disk_usage_through_a_fake_daemon_socket() {
    let dir = short_tempdir();
    let daemon = FakeDaemon::start(dir.path(), VERSION_ENGINE);
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();

    let r = inspect_docker(&daemon.env(&home), &Native, None, &opts());

    let DockerStatus::Connected {
        target,
        daemon: info,
    } = &r.status
    else {
        panic!("{}", r.status.summary())
    };
    assert_eq!(target.source, EndpointSource::DockerHost);
    assert_eq!((info.version.as_str(), info.in_vm), ("27.3.1", false));
    assert!(r.unavailable.is_empty(), "{:?}", r.unavailable);

    let b = r.group(DockerKind::BuildCache).unwrap();
    assert_eq!((b.tier, b.reclaimable.bytes), (Tier::S0, 4_000_000_000));
    assert_eq!(b.reclaimable.confidence, Confidence::Medium);
    let d = r.group(DockerKind::DanglingImages).unwrap();
    assert_eq!(
        (d.tier, d.count, d.reclaimable.bytes),
        (Tier::S1, 1, 300_000_000)
    );
    let c = r.group(DockerKind::StoppedContainers).unwrap();
    assert_eq!((c.tier, c.count, &c.action), (Tier::S2, 1, &Action::None));
    let v = r.group(DockerKind::Volumes).unwrap();
    assert_eq!((v.tier, v.count, &v.action), (Tier::S3, 1, &Action::None));

    // Only GETs on read-only endpoints reached the daemon.
    let mut seen = daemon.requests.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        [
            "GET /_ping HTTP/1.1",
            "GET /images/json HTTP/1.1",
            "GET /system/df?type=build-cache HTTP/1.1",
            "GET /system/df?type=container HTTP/1.1",
            "GET /version HTTP/1.1",
            "GET /volumes HTTP/1.1",
        ]
    );
}

#[test]
fn docker_desktop_vm_disk_is_reported_inspect_only_with_the_caveat() {
    let dir = short_tempdir();
    let daemon = FakeDaemon::start(dir.path(), VERSION_DESKTOP);
    let home = dir.path().join("home");
    let data = home.join(".docker/desktop/vms/0/data");
    std::fs::create_dir_all(&data).unwrap();
    let raw = data.join("Docker.raw");
    // A sparse 1 GiB disk image, like Docker Desktop's (logical size = the limit).
    std::fs::File::create(&raw)
        .unwrap()
        .set_len(1 << 30)
        .unwrap();

    let r = inspect_docker(&daemon.env(&home), &Native, None, &opts());

    assert!(r.in_vm(), "{}", r.status.summary());
    assert_eq!(r.vm_disks.len(), 1);
    let disk = &r.vm_disks[0];
    assert_eq!(disk.kind, VmDiskKind::DockerDesktopRaw);
    assert_eq!(disk.path, raw);
    assert_eq!(disk.logical, 1 << 30);
    assert!(disk.allocated.unwrap_or(0) < disk.logical, "sparse");
    // The disk file is never offered.
    assert_eq!((disk.tier, &disk.action), (Tier::S3, &Action::None));
    assert!(
        disk.kind
            .caveat()
            .contains("Pruning frees space inside the VM")
    );
    // In-VM figures: unknown host effect until measured.
    let b = r.group(DockerKind::BuildCache).unwrap();
    assert_eq!(b.reclaimable.confidence, Confidence::Unknown);
    assert!(b.reason.contains("VM disk image"));
    // The file is untouched.
    assert_eq!(std::fs::metadata(&raw).unwrap().len(), 1 << 30);
}

#[test]
fn absent_docker_fails_fast_and_gracefully() {
    let dir = short_tempdir();
    let env = CacheEnv::with_vars(
        dir.path(),
        Os::Linux,
        [(
            "DOCKER_HOST".to_owned(),
            format!("unix://{}", dir.path().join("missing.sock").display()),
        )],
    );
    let t = Instant::now();
    let r = inspect_docker(&env, &Native, None, &opts());
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert!(
        matches!(&r.status, DockerStatus::NotRunning { tried } if tried.len() == 1),
        "{:?}",
        r.status
    );
    assert!(r.groups.is_empty() && r.vm_disks.is_empty());
}

#[test]
fn inaccessible_socket_is_reported_not_escalated() {
    use std::os::unix::fs::PermissionsExt;
    let dir = short_tempdir();
    let daemon = FakeDaemon::start(dir.path(), VERSION_ENGINE);
    std::fs::set_permissions(&daemon.socket, std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = inspect_docker(&daemon.env(dir.path()), &Native, None, &opts());
    if matches!(r.status, DockerStatus::Connected { .. }) {
        // Running as root: permissions don't apply; nothing to check.
        return;
    }
    assert!(
        matches!(r.status, DockerStatus::NotAccessible { .. }),
        "{:?}",
        r.status
    );
    assert!(r.status.summary().contains("permission denied"));
    assert!(daemon.requests.lock().unwrap().is_empty());
}

#[test]
fn silent_daemon_times_out_fast() {
    let dir = short_tempdir();
    let socket = dir.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let _held: Vec<_> = listener.incoming().take(4).collect();
        std::thread::sleep(Duration::from_secs(3));
    });
    let env = CacheEnv::with_vars(
        dir.path(),
        Os::Linux,
        [(
            "DOCKER_HOST".to_owned(),
            format!("unix://{}", socket.display()),
        )],
    );
    let opts = DockerOptions {
        ping_timeout: Duration::from_millis(300),
        ..opts()
    };
    let t = Instant::now();
    let r = inspect_docker(&env, &Native, None, &opts);
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    assert!(
        matches!(r.status, DockerStatus::NotResponding { .. }),
        "{:?}",
        r.status
    );
}
