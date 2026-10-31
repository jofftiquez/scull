//! Activity signals (P1-SAFE-02, PRD §8.4): FX-CLS-09, FX-GIT-14, FX-LNX-04, plus the
//! in-use combination rules with fake probes.
//!
//! The real-process tests spawn this test binary again as a child ("sleeper") whose working
//! directory, or one of whose open files, is inside a fixture. Children are killed by a drop
//! guard, so a failing assertion never leaves one behind.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: panics are failures

use scull_core::{Evidence, ReasonCode, Tier};
use scull_fixtures::{Fixture, InvariantInputs, ago};
use scull_platform::Native;
use scull_platform::activity::Limits;
use scull_safety::activity::{DEFAULT_MTIME_SAMPLE_ENTRIES, map_observation};
use scull_safety::{
    ActivityProbe, ActivitySignal, ActivityWindow, GapPolicy, GitTracking, Holder, MtimeSample,
    NativeActivityProbe, ProbeResult, Regen, Signals, Tri, Unavailable, check_in_use, classify,
    recent_activity, sample_newest_mtime,
};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------------------------
// Fake probe: the pure combination rules.

struct Fake {
    cwd: ProbeResult,
    open: ProbeResult,
    lock: ProbeResult,
    lock_calls: RefCell<usize>,
}

impl Fake {
    fn new(cwd: ProbeResult, open: ProbeResult, lock: ProbeResult) -> Self {
        Self {
            cwd,
            open,
            lock,
            lock_calls: RefCell::new(0),
        }
    }
}

impl ActivityProbe for Fake {
    fn process_cwds_under(&self, _dir: &Path) -> ProbeResult {
        self.cwd.clone()
    }
    fn open_files_under(&self, _dir: &Path) -> ProbeResult {
        self.open.clone()
    }
    fn files_held_open(&self, _files: &[PathBuf]) -> ProbeResult {
        *self.lock_calls.borrow_mut() += 1;
        self.lock.clone()
    }
}

fn no() -> ProbeResult {
    ProbeResult::not_in_use()
}
fn yes(pid: u32) -> ProbeResult {
    ProbeResult::InUse(vec![Holder {
        pid,
        name: Some("cargo".into()),
        path: PathBuf::from("/p/target"),
    }])
}
fn unknown() -> ProbeResult {
    ProbeResult::Unknown(Unavailable::PermissionLimited {
        processes: 2,
        other_users: false,
    })
}

fn verified_local() -> Signals {
    Signals {
        git: GitTracking::NotTracked,
        in_protected_root: false,
        user_excluded: false,
        protect_marker: false,
        in_use: Tri::No,
        crosses_mount: false,
        link_escape: false,
        cloud_sync: false,
        marker_present: true,
        lockfile_present: true,
        artifact_known: true,
        regen: Regen::Local,
        recent_activity: Tri::No,
    }
}

fn lock() -> Vec<PathBuf> {
    vec![PathBuf::from("/p/target/debug/.cargo-lock")]
}

#[test]
fn in_use_any_yes_wins() {
    for (cwd, open, lk) in [
        (yes(1), no(), no()),
        (no(), yes(2), unknown()),
        (unknown(), unknown(), yes(3)),
    ] {
        let r = check_in_use(Path::new("/p/target"), &lock(), &Fake::new(cwd, open, lk));
        assert_eq!(r.in_use, Tri::Yes);
        assert!(
            r.evidence
                .iter()
                .any(|e| matches!(e, Evidence::InUse { by: Some(_), .. }))
        );
    }
}

#[test]
fn in_use_no_only_when_every_answer_is_no() {
    let r = check_in_use(
        Path::new("/p/target"),
        &lock(),
        &Fake::new(no(), no(), no()),
    );
    assert_eq!(r.in_use, Tri::No);
    assert!(r.evidence.is_empty());
    for (cwd, open, lk) in [
        (unknown(), no(), no()),
        (no(), unknown(), no()),
        (no(), no(), unknown()),
    ] {
        let r = check_in_use(Path::new("/p/target"), &lock(), &Fake::new(cwd, open, lk));
        assert_eq!(r.in_use, Tri::Unknown, "missing evidence is never 'idle'");
        assert!(
            r.evidence
                .iter()
                .any(|e| matches!(e, Evidence::SignalUnavailable { .. })),
            "the gap is disclosed: {:?}",
            r.evidence
        );
    }
}

#[test]
fn unsupported_signal_is_unknown_and_s2() {
    let unsupported = ProbeResult::Unknown(Unavailable::Unsupported("no cwd API".into()));
    let r = check_in_use(Path::new("/p/t"), &[], &Fake::new(unsupported, no(), no()));
    assert_eq!(r.in_use, Tri::Unknown);
    let c = classify(&Signals {
        in_use: r.in_use,
        ..verified_local()
    });
    assert_eq!(c.tier(), Tier::S2, "D-26: unknown in-use is S2");
}

#[test]
fn lock_files_only_checked_when_listed() {
    let fake = Fake::new(no(), no(), unknown());
    let r = check_in_use(Path::new("/p/target"), &[], &fake);
    assert_eq!(*fake.lock_calls.borrow(), 0);
    assert_eq!(r.in_use, Tri::No);
    assert_eq!(r.answers.len(), 2);
    let r = check_in_use(Path::new("/p/target"), &lock(), &fake);
    assert_eq!(*fake.lock_calls.borrow(), 1);
    assert_eq!(r.in_use, Tri::Unknown);
    assert_eq!(r.answers[2].0, ActivitySignal::ToolLock);
}

#[test]
fn other_user_notes_are_disclosed_but_not_gaps() {
    let mut o = scull_platform::activity::Observation::default();
    o.denied_other = 3;
    let lenient = map_observation(o, GapPolicy::OwnUser);
    let r = check_in_use(Path::new("/p/t"), &[], &Fake::new(lenient, no(), no()));
    assert_eq!(r.in_use, Tri::No);
    assert!(
        r.evidence
            .iter()
            .any(|e| matches!(e, Evidence::Note { text } if text.contains("other users")))
    );
}

// ---------------------------------------------------------------------------------------------
// Recent activity (FX-CLS-09).

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs() as i64
}

/// FX-CLS-09: `target/` with a file modified 1 hour ago is bumped one tier (S0 → S1),
/// reason "recent activity".
#[test]
fn fx_cls_09_recent_file_in_target_bumps_s0_to_s1() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app").unwrap();
    fx.cargo_target("app", "app", 64).unwrap();
    fx.file_with_mtime(
        "app/target/debug/app.d",
        "deps\n",
        ago(Duration::from_secs(3600)),
    )
    .unwrap();
    fx.checkpoint().unwrap();
    let target = fx.path("app/target").unwrap();

    let sample = sample_newest_mtime(&Native, &target, DEFAULT_MTIME_SAMPLE_ENTRIES);
    assert!(sample.complete, "small tree is fully sampled");
    let newest = sample.newest.expect("mtime readable");
    assert!(newest >= now_unix() - 3600 - 5, "newest {newest}");

    let ra = recent_activity(ActivityWindow::default(), sample, now_unix());
    assert_eq!(ra.recent_activity, Tri::Yes);
    assert!(matches!(
        ra.evidence,
        Evidence::LastModified { sampled: false, .. }
    ));
    let c = classify(&Signals {
        recent_activity: ra.recent_activity,
        ..verified_local()
    });
    assert_eq!(c.tier(), Tier::S1);
    assert!(
        c.reasons()
            .iter()
            .any(|r| r.code == ReasonCode::RecentActivity)
    );

    // Idle 3 days (scanner summary, complete): not bumped.
    let idle = MtimeSample::from_summary(Some(now_unix() - 3 * 86_400), true);
    let ra = recent_activity(ActivityWindow::default(), idle, now_unix());
    assert_eq!(ra.recent_activity, Tri::No);
    assert_eq!(
        classify(&Signals {
            recent_activity: ra.recent_activity,
            ..verified_local()
        })
        .tier(),
        Tier::S0
    );
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn bounded_sample_never_reads_as_idle() {
    let mut fx = Fixture::new().unwrap();
    for i in 0..10 {
        fx.file(format!("t/f{i}"), "x").unwrap();
    }
    fx.checkpoint().unwrap();
    let t = fx.path("t").unwrap();
    let sample = sample_newest_mtime(&Native, &t, 3);
    assert!(!sample.complete, "bound hit");
    // An old-looking, truncated sample is unknown (bumped like recent activity), never "No".
    let old = MtimeSample {
        newest: Some(now_unix() - 10 * 86_400),
        complete: false,
    };
    let ra = recent_activity(ActivityWindow::default(), old, now_unix());
    assert_eq!(ra.recent_activity, Tri::Unknown);
    assert!(matches!(
        ra.evidence,
        Evidence::LastModified { sampled: true, .. }
    ));
    // Unreadable candidate: unknown, disclosed.
    let missing = sample_newest_mtime(&Native, &t.join("missing"), 10);
    assert_eq!(missing.newest, None);
    let ra = recent_activity(ActivityWindow::default(), missing, now_unix());
    assert_eq!(ra.recent_activity, Tri::Unknown);
    assert!(matches!(ra.evidence, Evidence::SignalUnavailable { .. }));
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

// ---------------------------------------------------------------------------------------------
// Real processes.

const CHILD_ENV: &str = "SCULL_ACTIVITY_CHILD";
const HOLD_ENV: &str = "SCULL_ACTIVITY_HOLD";
const READY_ENV: &str = "SCULL_ACTIVITY_READY";

/// Not a test: the body of the child process the real-process tests spawn. Idle unless
/// `SCULL_ACTIVITY_CHILD` is set.
#[test]
#[ignore = "helper process for the activity tests"]
fn sleeper_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let _held =
        std::env::var_os(HOLD_ENV).map(|p| std::fs::File::open(p).expect("child: open held file"));
    if let Some(ready) = std::env::var_os(READY_ENV) {
        std::fs::write(ready, "ready").expect("child: write ready marker");
    }
    std::thread::sleep(Duration::from_secs(120));
}

/// Kills the child on drop.
struct Sleeper(Child);

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Sleeper {
    fn pid(&self) -> u32 {
        self.0.id()
    }
}

/// Spawn the sleeper with its working directory in `cwd`, optionally holding `hold` open;
/// returns once it is ready.
fn spawn_sleeper(cwd: &Path, hold: Option<&Path>) -> Sleeper {
    let ready_dir = tempfile::tempdir().unwrap();
    let ready = ready_dir.path().join("ready");
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "sleeper_child",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(CHILD_ENV, "1")
    .env(READY_ENV, &ready)
    .current_dir(cwd)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    if let Some(h) = hold {
        cmd.env(HOLD_ENV, h);
    }
    let child = Sleeper(cmd.spawn().unwrap());
    let start = Instant::now();
    while !ready.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "sleeper did not become ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

fn holders(r: &ProbeResult) -> Vec<u32> {
    match r {
        ProbeResult::InUse(h) => h.iter().map(|h| h.pid).collect(),
        _ => Vec::new(),
    }
}

/// FX-GIT-14: a process has its working directory inside the worktree ⇒ S3 for this run,
/// "in use". Process working directories are read from `/proc` (Linux), `libproc` (macOS)
/// and the process's PEB (Windows).
#[test]
fn fx_git_14_process_cwd_inside_worktree_is_in_use() {
    let mut fx = Fixture::new().unwrap();
    fx.git_repo("repo").unwrap();
    fx.dir("repo/src").unwrap();
    fx.checkpoint().unwrap();
    let repo = fx.path("repo").unwrap();
    let child = spawn_sleeper(&fx.path("repo/src").unwrap(), None);

    let probe = NativeActivityProbe::capture(Limits::default());
    let cwd = probe.process_cwds_under(&repo);
    assert!(
        holders(&cwd).contains(&child.pid()),
        "child {} not found: {cwd:?}",
        child.pid()
    );
    let r = check_in_use(&repo, &[], &probe);
    assert_eq!(r.in_use, Tri::Yes);
    assert!(r.evidence.iter().any(|e| matches!(
        e,
        Evidence::InUse { signal, .. } if signal == "process cwd"
    )));
    let c = classify(&Signals {
        in_use: r.in_use,
        ..verified_local()
    });
    assert_eq!(c.tier(), Tier::S3);
    assert!(c.reasons().iter().any(|r| r.code == ReasonCode::InUse));

    drop(child);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// A child holding a file open inside the candidate, and a held tool lock (Cargo's
/// `.cargo-lock`), are detected: `/proc/<pid>/fd` (Linux), `libproc` (macOS), Restart
/// Manager (Windows).
#[test]
fn open_file_and_held_lock_are_in_use() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app").unwrap();
    fx.cargo_target("app", "app", 16).unwrap();
    fx.file("app/target/debug/.cargo-lock", "").unwrap();
    fx.checkpoint().unwrap();
    let target = fx.path("app/target").unwrap();
    let lock = fx.path("app/target/debug/.cargo-lock").unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let child = spawn_sleeper(elsewhere.path(), Some(&lock));

    let probe = NativeActivityProbe::capture(Limits::default());
    let open = probe.open_files_under(&target);
    assert!(
        holders(&open).contains(&child.pid()),
        "child {} not found holding a file: {open:?}",
        child.pid()
    );
    let held = probe.files_held_open(std::slice::from_ref(&lock));
    assert!(
        holders(&held).contains(&child.pid()),
        "lock not reported held: {held:?}"
    );
    let r = check_in_use(&target, std::slice::from_ref(&lock), &probe);
    assert_eq!(r.in_use, Tri::Yes);
    assert!(r.evidence.iter().any(|e| matches!(
        e,
        Evidence::InUse { signal, .. } if signal == "tool lock"
    )));

    // Once the holder exits, the lock file merely existing is not "in use".
    drop(child);
    let probe = NativeActivityProbe::capture(Limits::default());
    assert_ne!(
        probe.files_held_open(std::slice::from_ref(&lock)).tri(),
        Tri::Yes
    );
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// No process holds the candidate ⇒ no holder is reported, and with full coverage of the
/// current user's processes the answer is "not in use".
#[test]
fn idle_candidate_is_not_in_use() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app").unwrap();
    fx.cargo_target("app", "app", 16).unwrap();
    fx.checkpoint().unwrap();
    let target = fx.path("app/target").unwrap();

    let probe = NativeActivityProbe::capture(Limits::default());
    let r = check_in_use(&target, &[], &probe);
    for (signal, answer) in &r.answers {
        assert!(
            holders(answer).is_empty(),
            "{signal:?}: unexpected holder {answer:?}"
        );
    }
    assert_ne!(r.in_use, Tri::Yes);
    let own_gaps = r.answers.iter().any(|(_, a)| {
        matches!(
            a,
            ProbeResult::Unknown(Unavailable::PermissionLimited {
                other_users: false,
                ..
            })
        )
    });
    if own_gaps {
        eprintln!("some of this user's processes could not be inspected: {r:?}");
        assert_eq!(r.in_use, Tri::Unknown);
    } else {
        assert_eq!(r.in_use, Tri::No, "{r:?}");
    }
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// FX-LNX-04 (and its macOS/Windows equivalents): detection works for processes we may
/// inspect, and processes we may not inspect are reported: as a disclosed note by default, and
/// as an unknown answer (S2, `signal_unavailable`) under the strict policy. Never as "idle".
#[test]
fn fx_lnx_04_detection_with_and_without_permission() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("work").unwrap();
    fx.dir("idle").unwrap();
    fx.checkpoint().unwrap();
    let work = fx.path("work").unwrap();
    let idle = fx.path("idle").unwrap();
    let child = spawn_sleeper(&work, None);

    // With permission: our own child is found.
    let snap = scull_platform::activity::Snapshot::capture(Limits::default());
    let obs = snap.cwds_under(&work);
    assert!(obs.hits.iter().any(|h| h.pid == child.pid()), "{obs:?}");

    // Without permission: other users' processes (root/SYSTEM daemons, when not elevated).
    let idle_obs = snap.cwds_under(&idle);
    assert!(idle_obs.hits.is_empty(), "{idle_obs:?}");
    eprintln!(
        "processes: {}, uninspectable: own {}, other users {}",
        snap.process_count(),
        idle_obs.denied_own,
        idle_obs.denied_other
    );
    let strict = NativeActivityProbe::from_snapshot(snap).with_policy(GapPolicy::Strict);
    if idle_obs.denied_other > 0 {
        let r = check_in_use(&idle, &[], &strict);
        assert_eq!(
            r.in_use,
            Tri::Unknown,
            "uninspected processes never read as idle under the strict policy"
        );
        assert!(
            r.evidence.iter().any(|e| matches!(
                e,
                Evidence::SignalUnavailable { reason, .. } if reason.contains("could not be inspected")
            )),
            "{:?}",
            r.evidence
        );
        if idle_obs.denied_own == 0 {
            let lenient = map_observation(idle_obs, GapPolicy::OwnUser);
            assert!(
                matches!(lenient, ProbeResult::NotInUse { ref notes } if !notes.is_empty()),
                "{lenient:?}"
            );
        }
    } else {
        eprintln!("running with full process visibility (elevated); no permission gap to report");
    }
    drop(child);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
