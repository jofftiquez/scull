//! `scull worktrees register <path> --ttl 7d` and the TTL section of `scull worktrees`
//! (P4-AI-02, PRO-08, FX-MCP-10 via the CLI). Every run uses a temp home and a temp
//! `SCULL_HOME`; the fixture invariants prove the worktree and the canary are untouched: a
//! registration is metadata in scull's data folder, and an expired TTL is only a review
//! suggestion.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_fixtures::{Fixture, InvariantInputs};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct Env {
    scull_home: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            scull_home: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn data(&self) -> PathBuf {
        self.scull_home.path().join("data")
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(args);
        if args.first() == Some(&"worktrees") && args.get(1) != Some(&"register") {
            c.arg("--allow-network-roots");
        }
        let home = self.home.path();
        c.env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("SCULL_HOME", self.scull_home.path())
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("LOCALAPPDATA", home.join("AppData").join("Local"))
            .env("APPDATA", home.join("AppData").join("Roaming"))
            .env_remove("CARGO_TARGET_DIR")
            .stdin(Stdio::null());
        c.output().unwrap()
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn validate(name: &str, v: &Value) {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("schemas/{name}.schema.json"));
    let s: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let val = jsonschema::validator_for(&s).unwrap();
    let errors: Vec<String> = val.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{name}: {errors:?}\n{v:#}");
}

/// A repository with one commit and a linked worktree `repo-wt`.
fn fixture() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    let repo = fx.git_repo("repo").unwrap();
    fx.file("repo/README.md", "hello\n").unwrap();
    repo.commit_all("init").unwrap();
    repo.run([
        "worktree",
        "add",
        "--quiet",
        "-b",
        "agent-task",
        "../repo-wt",
    ])
    .unwrap();
    fx.checkpoint().unwrap();
    fx
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn register_lists_ttl_and_expiry_is_only_a_suggestion() {
    if !git_available() {
        eprintln!("skipping: `git` not found");
        return;
    }
    let fx = fixture();
    let env = Env::new();
    let root = s(fx.root());
    let wt = fx.root().join("repo-wt");

    // Register with --json: schema-valid, recorded in scull's data folder only.
    let o = env.run(&["worktrees", "register", &s(&wt), "--ttl", "7d", "--json"]);
    assert!(o.status.success(), "{}", err(&o));
    let doc: Value = serde_json::from_str(&out(&o)).unwrap();
    validate("worktree-registration", &doc);
    assert_eq!(doc["data"]["ttl_secs"], 7 * 86_400);
    assert_eq!(doc["data"]["ttl"], "7d");
    let reg_file = env.data().join(scull_mcp::registry::REGISTRY_FILE);
    assert!(reg_file.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&reg_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "registry is owner-only");
    }

    // Human output.
    let o = env.run(&["worktrees", "register", &s(&wt), "--ttl", "12h"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).contains("never removed automatically"),
        "{}",
        out(&o)
    );

    // `scull worktrees` shows the TTL (not expired; the latest registration wins). The
    // `scull/worktrees@1` document is unchanged and still validates.
    let o = env.run(&["worktrees", "--root", &root]);
    assert!(o.status.success(), "{}", err(&o));
    let text = out(&o);
    assert!(
        text.contains("Registered TTLs (never removed automatically)"),
        "{text}"
    );
    assert!(text.contains("TTL 12h, expires in "), "{text}");
    assert!(!text.contains("folder not found"), "{text}");
    let o = env.run(&["worktrees", "--root", &root, "--json"]);
    assert!(o.status.success(), "{}", err(&o));
    validate("worktrees", &serde_json::from_str(&out(&o)).unwrap());

    // FX-MCP-10 via the CLI: the TTL ran out (registered two days ago with a 1-day TTL).
    let canonical = std::fs::canonicalize(&wt).unwrap();
    let canonical = PathBuf::from(
        canonical
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_owned(),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    scull_mcp::registry::Registry::in_data_dir(&env.data())
        .record(
            &canonical,
            86_400,
            now - 2 * 86_400,
            scull_mcp::registry::Source::Mcp,
        )
        .unwrap();
    let o = env.run(&["worktrees", "--root", &root]);
    assert!(o.status.success(), "{}", err(&o));
    let text = out(&o);
    assert!(
        text.contains("Registered TTLs (never removed automatically)"),
        "{text}"
    );
    assert!(text.contains("expired 1d ago: review suggested"), "{text}");
    assert!(wt.join("README.md").exists(), "the worktree is untouched");

    // Refusals: a main checkout, a plain folder, bad TTLs. Nothing more is written.
    let before = std::fs::read(&reg_file).unwrap();
    for (args, code) in [
        (
            vec![s(&fx.root().join("repo")), "--ttl".into(), "7d".into()],
            2,
        ),
        (vec![s(&wt), "--ttl".into(), "7".into()], 2),
        (vec![s(&wt), "--ttl".into(), "400d".into()], 2),
    ] {
        let mut a = vec!["worktrees".to_owned(), "register".to_owned()];
        a.extend(args);
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        let o = env.run(&a);
        assert_eq!(o.status.code(), Some(code), "{a:?}: {}", err(&o));
    }
    assert_eq!(std::fs::read(&reg_file).unwrap(), before);

    // Nothing in the tree or the canary changed.
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn register_refuses_a_folder_that_is_not_a_worktree() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("plain").unwrap();
    fx.checkpoint().unwrap();
    let env = Env::new();
    let o = env.run(&[
        "worktrees",
        "register",
        &s(&fx.root().join("plain")),
        "--ttl",
        "1d",
    ]);
    assert_ne!(o.status.code(), Some(0), "{}", out(&o));
    assert!(!env.data().join(scull_mcp::registry::REGISTRY_FILE).exists());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
