//! The desktop bridge end to end over a generated fixture tree: scan with progress events,
//! cancellation, projects, worktrees, caches, status and capabilities. Every payload is
//! validated against the committed schemas (`apps/desktop/schemas/`) the frontend's types are
//! generated from, and the tree is checked unchanged afterwards (I-2, I-3: the desktop never
//! mutates the filesystem, SAFE-10).
//!
//! `SCULL_UPDATE_MOCKS=1` also writes these real payloads (paths rewritten to an imagined
//! `/Users/ada`) to `apps/desktop/src/bridge/fixtures/`, which the in-browser mock bridge serves,
//! so the mock returns exactly what the real bridge returns.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::ErrorKind;
use scull_desktop_bridge::views::{Phase, StatusState};
use scull_desktop_bridge::{OpId, ProgressEvent, ScanParams, Session, Setup};
use scull_fixtures::{Fixture, InvariantInputs, NodeLock};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn validate(stem: &str, v: &Value) {
    let path = desktop()
        .join("schemas")
        .join(format!("{stem}.schema.json"));
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{stem}: {errors:?}\n{v:#}");
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A small developer machine: a Cargo project with `target/`, an npm project with
/// `node_modules/`, a project marked `.scull-protect`, a Git repository with a linked worktree
/// (when `git` exists) under `code/`, and an npm cache under `home/`.
fn machine() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("code/api", "api")
        .unwrap()
        .cargo_target("code/api", "api", 64 * 1024)
        .unwrap()
        .npm_project("code/web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("code/web", &["react", "vite"])
        .unwrap()
        .cargo_project("code/vault", "vault")
        .unwrap()
        .cargo_target("code/vault", "vault", 8 * 1024)
        .unwrap()
        .file("code/vault/.scull-protect", "")
        .unwrap()
        .file(
            "home/.npm/_cacache/index-v5/1a/2b/3c4d",
            "{\"key\":\"x\"}\n",
        )
        .unwrap()
        .file_of_size("home/.npm/_cacache/content-v2/sha512/ab/cd/ef01", 32 * 1024)
        .unwrap();
    if git_available() {
        let repo = fx.git_repo("code/app").unwrap();
        fx.file("code/app/.gitignore", "target/\n")
            .unwrap()
            .cargo_project("code/app", "app")
            .unwrap()
            .cargo_target("code/app", "app", 16 * 1024)
            .unwrap();
        repo.commit_all("initial").unwrap();
        repo.run([
            "worktree",
            "add",
            "--quiet",
            "-b",
            "agent-1",
            "../app-agent-1",
        ])
        .unwrap();
    }
    fx.checkpoint().unwrap();
    fx
}

fn setup(fx: &Fixture, data: &Path) -> Setup {
    Setup {
        home: Some(fx.root().join("home")),
        config_file: None,
        data_dir: Some(data.to_path_buf()),
        env: Arc::new(|_| None),
        record_history: true,
        git: true,
        detect_synced_roots: false,
        // Temp dirs are tmpfs on some machines (D-22 is tested in scull-platform).
        allow_network_roots: true,
        in_use: false,
    }
}

fn op(s: &str) -> OpId {
    OpId::parse(s).unwrap()
}

fn params(root: &Path) -> ScanParams {
    ScanParams {
        roots: vec![root.display().to_string()],
        ..ScanParams::default()
    }
}

#[test]
fn scan_streams_progress_and_every_payload_matches_its_schema() {
    let fx = machine();
    let data = tempfile::tempdir().unwrap();
    let session = Session::new(setup(&fx, data.path()));
    let code = fx.root().join("code");

    // Before any scan: empty views, no database.
    let status = serde_json::to_value(session.status(1_900_000_000)).unwrap();
    assert_eq!(status["state"], "no_database");
    validate("status", &status);
    let empty = serde_json::to_value(session.projects()).unwrap();
    assert_eq!(empty["projects"], Value::Array(vec![]));
    validate("projects", &empty);

    let events: Arc<Mutex<Vec<ProgressEvent>>> = Arc::default();
    let sink = Arc::clone(&events);
    let report = session
        .scan(&op("scan-1"), &params(&code), move |e| {
            sink.lock().unwrap().push(e)
        })
        .unwrap();
    assert!(!report.cancelled);
    assert!(
        session.running().is_empty(),
        "the operation is unregistered"
    );

    // Progress: tagged with the operation, at least one walk and one analysis event.
    let events = events.lock().unwrap().clone();
    assert!(events.iter().all(|e| e.op_id.as_str() == "scan-1"));
    assert!(
        events.iter().any(|e| e.phase == Phase::Scanning),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| e.phase == Phase::Analyzing),
        "{events:?}"
    );
    for e in &events {
        validate("progress-event", &serde_json::to_value(e).unwrap());
    }

    let scan = serde_json::to_value(&report).unwrap();
    validate("scan-report", &scan);
    let names: Vec<&str> = report.projects.iter().map(|p| p.name.as_str()).collect();
    for want in ["api", "web", "vault"] {
        assert!(names.contains(&want), "{names:?}");
    }
    // The protected project's target stays protected (S3), never actionable.
    let vault = report
        .candidates
        .iter()
        .find(|c| c.path.as_str().contains("vault"))
        .expect("vault target listed");
    assert_eq!(vault.tier, scull_core::Tier::S3, "{vault:#?}");

    let projects = serde_json::to_value(session.projects()).unwrap();
    validate("projects", &projects);
    let worktrees = serde_json::to_value(session.worktrees()).unwrap();
    validate("worktrees", &worktrees);
    if git_available() {
        assert!(
            report.worktrees.len() >= 2,
            "main + linked worktree: {:#?}",
            report.worktrees
        );
    }

    let caches = session.caches(&op("caches-1"), |_| {}).unwrap();
    let caches_v = serde_json::to_value(&caches).unwrap();
    validate("caches-report", &caches_v);
    if !cfg!(windows) {
        // npm's Windows default is %LocalAppData%\npm-cache, which this fixture doesn't have.
        assert_eq!(caches.candidates.len(), 1, "{caches:#?}");
    }

    // History was recorded: status now reads it.
    let status = session.status(report.finished_at + 240);
    assert_eq!(status.state, StatusState::Ok, "{status:?}");
    assert_eq!(status.last_scan.as_ref().unwrap().age_secs, 240);
    assert_eq!(status.last_scan.as_ref().unwrap().roots, Some(1));
    let status_v = serde_json::to_value(&status).unwrap();
    validate("status", &status_v);

    let caps = serde_json::to_value(session.capabilities()).unwrap();
    validate("capabilities", &caps);
    assert_eq!(caps["read_only"], true);
    assert_eq!(caps["history"], "present");

    // Read-only: the tree and the canary are unchanged (I-2, I-3).
    fx.check_invariants(&InvariantInputs::new()).assert_ok();

    if std::env::var_os("SCULL_UPDATE_MOCKS").is_some() && cfg!(unix) {
        write_mocks(
            &fx,
            data.path(),
            &[
                ("scan-report", &scan),
                ("projects", &projects),
                ("worktrees", &worktrees),
                ("caches-report", &caches_v),
                ("status", &status_v),
                ("capabilities", &caps),
                (
                    "progress-events",
                    &serde_json::to_value(&events[..events.len().min(6)]).unwrap(),
                ),
            ],
        );
    }
}

/// Rewrite fixture paths to an imagined home and write the mock bridge's data.
fn write_mocks(fx: &Fixture, data: &Path, docs: &[(&str, &Value)]) {
    let dir = desktop().join("src/bridge/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let base = fx.root().display().to_string();
    let data = data.display().to_string();
    for (name, v) in docs {
        let text = serde_json::to_string_pretty(v)
            .unwrap()
            .replace(&format!("{base}/home"), "/Users/ada")
            .replace(&format!("{base}/code"), "/Users/ada/code")
            .replace(&base, "/Users/ada")
            .replace(&data, "/Users/ada/Library/Application Support/scull");
        std::fs::write(dir.join(format!("{name}.json")), text + "\n").unwrap();
    }
}

#[test]
fn cancelling_returns_a_partial_report_and_frees_the_id() {
    let fx = machine();
    let data = tempfile::tempdir().unwrap();
    let session = Session::new(setup(&fx, data.path()));
    let code = fx.root().join("code");
    // The UI's cancel can overtake the scan it cancels: it is remembered, and the scan stops
    // before walking. The report says so and history marks nothing gone.
    assert!(!session.cancel(&op("scan-c")), "nothing running yet");
    let report = session.scan(&op("scan-c"), &params(&code), |_| {}).unwrap();
    assert!(report.cancelled, "{report:#?}");
    assert!(report.worktrees.is_empty());
    assert!(
        session.running().is_empty(),
        "the operation is unregistered"
    );

    // The early cancel was used up: the next scan runs to the end.
    let again = session.scan(&op("scan-d"), &params(&code), |_| {}).unwrap();
    assert!(!again.cancelled);
    // A cancelled (partial) scan never replaces a complete one for Projects/Worktrees.
    assert!(!session.cancel(&op("scan-e")));
    let partial = session.scan(&op("scan-e"), &params(&code), |_| {}).unwrap();
    assert!(partial.cancelled);
    assert_eq!(session.last_scan().unwrap().scan_id, again.scan_id);
    assert!(!session.projects().projects.is_empty());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn cancel_reaches_a_running_operation() {
    let fx = machine();
    let data = tempfile::tempdir().unwrap();
    let session = Arc::new(Session::new(setup(&fx, data.path())));
    let code = fx.root().join("code");
    // Cancel from inside the operation (any progress event): `cancel` finds it running.
    let s2 = Arc::clone(&session);
    let hit = Arc::new(Mutex::new(false));
    let h2 = Arc::clone(&hit);
    session
        .scan(&op("scan-r"), &params(&code), move |_| {
            if s2.cancel(&OpId::parse("scan-r").unwrap()) {
                *h2.lock().unwrap() = true;
            }
        })
        .unwrap();
    assert!(*hit.lock().unwrap(), "cancel saw the running operation");
    assert!(session.running().is_empty());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn errors_are_scull_errors() {
    let fx = machine();
    let data = tempfile::tempdir().unwrap();
    let session = Session::new(setup(&fx, data.path()));

    // A broad root (the home folder) needs explicit confirmation (CFG-04).
    let home = fx.root().join("home");
    let e = session.scan(&op("b"), &params(&home), |_| {}).unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");
    validate("error", &serde_json::to_value(&e).unwrap());

    // Relative roots are refused; so are missing ones.
    let e = session
        .scan(
            &op("r"),
            &ScanParams {
                roots: vec!["code".into()],
                ..ScanParams::default()
            },
            |_| {},
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Usage);
    let e = session
        .scan(&op("m"), &params(&fx.root().join("missing")), |_| {})
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::NotFound, "{e:?}");

    // No roots at all.
    let e = session
        .scan(&op("n"), &ScanParams::default(), |_| {})
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Usage);

    // Operation IDs are validated.
    assert!(OpId::parse("").is_err());
    assert!(OpId::parse("a b").is_err());
    assert!(OpId::parse(&"x".repeat(65)).is_err());
    assert!(OpId::parse("scan_1-a").is_ok());
    assert!(session.running().is_empty());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn history_off_writes_nothing() {
    let fx = machine();
    let data = tempfile::tempdir().unwrap();
    let mut s = setup(&fx, data.path());
    s.record_history = false;
    let session = Session::new(s);
    let r = session
        .scan(&op("h"), &params(&fx.root().join("code")), |_| {})
        .unwrap();
    assert!(!r.cancelled);
    assert!(
        std::fs::read_dir(data.path()).unwrap().next().is_none(),
        "no database created"
    );
    // The session's own scan still feeds the status line.
    let st = session.status(r.finished_at);
    assert_eq!(st.state, StatusState::NoDatabase);
    assert!(st.last_scan.is_some());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn config_file_roots_are_used_and_bad_config_is_a_config_error() {
    let fx = machine();
    let data = tempfile::tempdir().unwrap();
    let cfg_dir = tempfile::tempdir().unwrap();
    let cfg = cfg_dir.path().join("config.toml");
    let code = fx.root().join("code");
    std::fs::write(
        &cfg,
        format!("[scan]\nroots = [{:?}]\n", code.display().to_string()),
    )
    .unwrap();
    let mut s = setup(&fx, data.path());
    s.config_file = Some(cfg.clone());
    let session = Session::new(s.clone());
    let r = session
        .scan(&op("c"), &ScanParams::default(), |_| {})
        .unwrap();
    assert_eq!(r.roots.len(), 1);

    std::fs::write(&cfg, "[scan\nroots = 3").unwrap();
    let e = Session::new(s)
        .scan(&op("c2"), &ScanParams::default(), |_| {})
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config, "{e:?}");
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
