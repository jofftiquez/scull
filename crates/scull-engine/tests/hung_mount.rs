//! #301 (T-53, FX-LNX-03): a frozen network or FUSE mount directly inside a scanned folder
//! costs only itself. Its healthy siblings (here `proj/target`) are still scanned and
//! classified, instead of the whole folder timing out.
//!
//! The frozen mount is simulated behind the engine's seams, so these tests are deterministic
//! and unprivileged: a [`Platform`] over [`Native`] whose calls on the mount point block until
//! released (like `lstat` on the mount point of a `SIGSTOP`ped FUSE daemon), and a
//! [`VolumeSource`] that lists (or doesn't list) the mount point as a network volume. The
//! real thing (bindfs + `SIGSTOP`) runs in the nightly FX-LNX-03 fixture. Everything happens
//! in fixture temp dirs; nothing is modified (I-2, I-3 checked).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::config::Config;
use scull_core::{Action, Candidate, ReasonCode, Tier};
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, FixedInUse, InUseSource, IssueKind, NativeVolumes,
    ScanReport, ScanRequest, VolumeSource, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs, PlanEntry};
use scull_platform::{
    Capabilities, DirIter, Metadata, Native, Platform, PlatformError, VolumeInfo, VolumeKind,
};
use scull_safety::Tri;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const THREE_DAYS: i64 = 3 * 24 * 3600;

/// [`Native`], except that any call on `frozen` (or below it) blocks until `release` is set,
/// like a FUSE mount whose daemon was stopped. Records every path it was asked about there.
struct Frozen {
    frozen: PathBuf,
    release: Arc<AtomicBool>,
    touched: Mutex<Vec<PathBuf>>,
}

impl Frozen {
    fn hang_if_frozen(&self, path: &Path) {
        if path.starts_with(&self.frozen) {
            self.touched.lock().unwrap().push(path.to_path_buf());
            while !self.release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl Platform for Frozen {
    fn capabilities(&self) -> Capabilities {
        Native.capabilities()
    }

    fn metadata(&self, path: &Path) -> scull_platform::Result<Metadata> {
        self.hang_if_frozen(path);
        Native.metadata(path)
    }

    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<DirIter<'a>> {
        self.hang_if_frozen(path);
        Native.read_dir(path)
    }
}

/// The real mount table plus a network (FUSE) mount at `mount`, as `/proc/self/mountinfo`
/// lists a `fuse.bindfs` mount.
struct WithNetworkMount {
    mount: PathBuf,
}

impl VolumeSource for WithNetworkMount {
    fn volumes(&self) -> Result<Vec<VolumeInfo>, PlatformError> {
        let mut all = NativeVolumes.volumes()?;
        let mut fuse = scull_platform::volume_of(self.mount.parent().unwrap())?;
        fuse.mount_point = self.mount.clone();
        fuse.source = "bindfs".into();
        fuse.fs_type = "fuse.bindfs".into();
        fuse.kind = VolumeKind::Network;
        fuse.volume_id = Some(u64::MAX - 7);
        all.push(fuse);
        Ok(all)
    }
}

/// `proj` (a Cargo project with a built `target/`), `docs/`, and the folder `frozen` (a
/// relative path) that will act as the frozen mount, with a remote project behind it whose
/// `target/` must never be classified.
fn fixture(frozen: &str) -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("proj", "proj")
        .unwrap()
        .cargo_target("proj", "proj", 4096)
        .unwrap()
        .file("docs/readme.md", "hello\n")
        .unwrap()
        .cargo_project(format!("{frozen}/remote"), "remote")
        .unwrap()
        .cargo_target(format!("{frozen}/remote"), "remote", 2048)
        .unwrap();
    fx.checkpoint().unwrap();
    fx
}

fn engine(
    platform: Arc<dyn Platform>,
    volumes: Arc<dyn VolumeSource>,
    timeout: Duration,
) -> Engine {
    let mut cfg = EngineConfig::new(Config::default(), platform);
    // Temp dirs are tmpfs on some machines; the D-22 root policy is tested elsewhere.
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(Tri::No)));
    cfg.volumes = volumes;
    cfg.dir_timeout = Some(timeout);
    Engine::new(cfg).unwrap()
}

/// Scan `root` on another thread with the frozen mount at `frozen`; release it afterwards.
/// Fails if the scan doesn't finish within `limit` (T-53).
fn scan_frozen(
    root: &Path,
    frozen: &Path,
    listed: bool,
    timeout: Duration,
    limit: Duration,
) -> (ScanReport, Vec<PathBuf>, Duration) {
    let release = Arc::new(AtomicBool::new(false));
    let platform = Arc::new(Frozen {
        frozen: frozen.to_path_buf(),
        release: Arc::clone(&release),
        touched: Mutex::new(Vec::new()),
    });
    let volumes: Arc<dyn VolumeSource> = if listed {
        Arc::new(WithNetworkMount {
            mount: frozen.to_path_buf(),
        })
    } else {
        Arc::new(NativeVolumes)
    };
    let e = engine(Arc::clone(&platform) as Arc<dyn Platform>, volumes, timeout);
    let (tx, rx) = mpsc::channel();
    let r = root.to_path_buf();
    let t = Instant::now();
    std::thread::spawn(move || {
        let report = e.scan(
            &[r],
            &ScanRequest::full()
                .history(false)
                .at(now_unix() + THREE_DAYS),
            &CancelToken::new(),
            |_| {},
        );
        let _ = tx.send(report);
    });
    let outcome = rx.recv_timeout(limit);
    let took = t.elapsed();
    release.store(true, Ordering::Release);
    let report = outcome
        .expect("the scan hung on a frozen mount (T-53)")
        .expect("scan");
    let touched = platform.touched.lock().unwrap().clone();
    (report, touched, took)
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn candidate<'a>(r: &'a ScanReport, root: &Path, want: &str) -> &'a Candidate {
    r.candidates
        .iter()
        .find(|c| rel(root, &c.path.to_path_buf()) == want)
        .unwrap_or_else(|| {
            let all: Vec<String> = r
                .candidates
                .iter()
                .map(|c| format!("{} {}", rel(root, &c.path.to_path_buf()), c.tier))
                .collect();
            panic!("no candidate at {want}: {all:#?}\nissues: {:#?}", r.issues)
        })
}

fn issues(r: &ScanReport, root: &Path) -> Vec<(IssueKind, String)> {
    r.issues
        .iter()
        .map(|i| (i.kind, rel(root, &i.path.to_path_buf())))
        .collect()
}

fn assert_nothing_classified_behind(r: &ScanReport, root: &Path, frozen: &str) {
    for c in &r.candidates {
        let p = rel(root, &c.path.to_path_buf());
        assert!(
            !p.starts_with(frozen),
            "classified behind the frozen mount: {p}"
        );
    }
}

/// I-1 (S3 never actionable; every action fits its tier), I-2 and I-3.
fn assert_read_only(fx: &Fixture, candidates: &[Candidate]) {
    for c in candidates {
        assert!(c.is_consistent(), "{c:#?}");
        if c.tier == Tier::S3 {
            assert_eq!(c.action, Action::None, "S3 must not be actionable: {c:#?}");
        }
    }
    let plan: Vec<PlanEntry> = candidates
        .iter()
        .filter(|c| c.action != Action::None)
        .map(PlanEntry::from)
        .collect();
    fx.check_invariants(&InvariantInputs::new().plan(plan))
        .assert_ok();
}

#[test]
fn frozen_mount_in_the_mount_table_is_never_touched_and_siblings_are_classified() {
    let fx = fixture("hang");
    let root = canon(fx.root());
    let frozen = root.join("hang");
    // The default 60 s timeout: the frozen mount must cost nothing, not one timeout.
    let (r, touched, took) = scan_frozen(
        &root,
        &frozen,
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    );
    println!(
        "#301 mount-table case: scan took {took:?}; issues {:?}",
        issues(&r, &root)
    );
    assert!(took < Duration::from_secs(30), "took {took:?}");
    assert!(
        touched.is_empty(),
        "the frozen mount was touched: {touched:?}"
    );
    let target = candidate(&r, &root, "proj/target");
    println!("#301 mount-table case: proj/target -> {}", target.tier);
    let found = issues(&r, &root);
    assert!(
        found.contains(&(IssueKind::OtherVolume, "hang".into())),
        "{found:?}"
    );
    assert!(
        !found.iter().any(|(k, _)| *k == IssueKind::TimedOut),
        "{found:?}"
    );
    assert_nothing_classified_behind(&r, &root, "hang");
    assert_read_only(&fx, &r.candidates);
}

#[test]
fn frozen_mount_missing_from_the_mount_table_costs_one_timeout_for_itself() {
    let fx = fixture("hang");
    let root = canon(fx.root());
    let frozen = root.join("hang");
    let (r, _, took) = scan_frozen(
        &root,
        &frozen,
        false,
        Duration::from_millis(400),
        Duration::from_secs(30),
    );
    let found = issues(&r, &root);
    println!("#301 per-entry deadline: scan took {took:?}; issues {found:?}");
    // The hung entry alone is timed out; the scan root is not.
    assert_eq!(
        found
            .iter()
            .filter(|(k, _)| *k == IssueKind::TimedOut)
            .collect::<Vec<_>>(),
        [&(IssueKind::TimedOut, "hang".to_owned())],
        "{found:?}"
    );
    let message = &r
        .issues
        .iter()
        .find(|i| i.kind == IssueKind::TimedOut)
        .unwrap()
        .message;
    assert!(message.contains("metadata"), "{message}");
    // The healthy sibling is scanned and classified as without the frozen mount.
    let target = candidate(&r, &root, "proj/target");
    println!("#301 per-entry deadline: proj/target -> {}", target.tier);
    let baseline = {
        let clean = fixture("elsewhere");
        let croot = canon(clean.root());
        let e = engine(
            Arc::new(Native),
            Arc::new(NativeVolumes),
            Duration::from_secs(60),
        );
        let b = e
            .scan(
                std::slice::from_ref(&croot),
                &ScanRequest::full()
                    .history(false)
                    .at(now_unix() + THREE_DAYS),
                &CancelToken::new(),
                |_| {},
            )
            .unwrap();
        assert_read_only(&clean, &b.candidates);
        candidate(&b, &croot, "proj/target").tier
    };
    assert_eq!(target.tier, baseline);
    assert_nothing_classified_behind(&r, &root, "hang");
    assert_read_only(&fx, &r.candidates);
}

#[test]
fn frozen_mount_inside_a_candidate_keeps_it_s3_and_its_siblings_scanned() {
    // FX-LNX-03's `proj/target/net`, frozen. Listed in the mount table: the candidate crosses
    // a mount (S3). Missing from it: the candidate is incomplete (at least S2). Either way the
    // scan finishes and `docs`/`proj` are still seen.
    let fx = fixture("proj/target/net");
    let root = canon(fx.root());
    let frozen = root.join("proj/target/net");

    let (r, touched, _) = scan_frozen(
        &root,
        &frozen,
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    );
    assert!(touched.is_empty(), "{touched:?}");
    let t = candidate(&r, &root, "proj/target");
    assert_eq!(t.tier, Tier::S3, "{:#?}", t.reasons);
    assert!(t.reasons.iter().any(|x| x.code == ReasonCode::CrossesMount));
    assert_nothing_classified_behind(&r, &root, "proj/target/net/");
    assert_read_only(&fx, &r.candidates);

    let (r, _, _) = scan_frozen(
        &root,
        &frozen,
        false,
        Duration::from_millis(400),
        Duration::from_secs(30),
    );
    let t = candidate(&r, &root, "proj/target");
    assert!(t.tier >= Tier::S2, "{t:#?}");
    assert!(
        issues(&r, &root).contains(&(IssueKind::TimedOut, "proj/target/net".into())),
        "{:?}",
        issues(&r, &root)
    );
    assert_nothing_classified_behind(&r, &root, "proj/target/net/");
    assert_read_only(&fx, &r.candidates);
}
