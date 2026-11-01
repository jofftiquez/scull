//! Drives the real `scull` binary for end-to-end fixtures (FX-OUT, FX-DATA): an isolated
//! environment per run (scull's directories under a temp `SCULL_HOME`, the user's home in
//! another temp dir, package-manager variables removed, stdin closed) and the published JSON
//! Schemas to validate what it prints.
//!
//! The binary is the one cargo builds for `scull-cli` into this target directory
//! (`target/<profile>/scull`). `cargo test --workspace` (the CI command) always builds it fresh
//! before any test runs, because `scull-cli`'s own integration tests need it. When this crate's
//! tests run alone (`cargo test -p scull-fixtures`), build it first with
//! `cargo build -p scull-cli`; a missing binary fails the test with that hint, never a pass.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;

/// Environment variables that point package managers (and so `scull caches`) elsewhere.
pub const CACHE_VARS: &[&str] = &[
    "npm_config_cache",
    "npm_config_userconfig",
    "npm_config_store_dir",
    "NPM_CONFIG_CACHE",
    "PNPM_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "BUN_INSTALL",
    "BUN_INSTALL_CACHE_DIR",
    "YARN_CACHE_FOLDER",
    "YARN_GLOBAL_FOLDER",
    "CARGO_TARGET_DIR",
];

/// The workspace root.
pub fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `scull` binary of this target directory.
pub fn scull_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable");
    // target/<profile>/deps/<test>-<hash> → target/<profile>/scull
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile> directory");
    let bin = profile_dir.join(format!("scull{}", std::env::consts::EXE_SUFFIX));
    assert!(
        bin.is_file(),
        "{} does not exist: build it with `cargo build -p scull-cli` (cargo test --workspace \
         builds it)",
        bin.display()
    );
    bin
}

/// One isolated environment for the binary.
pub struct Cli {
    scull_home: tempfile::TempDir,
    _user_home: Option<tempfile::TempDir>,
    home: PathBuf,
    extra_env: Vec<(String, String)>,
}

impl Cli {
    /// The user's home is an empty temp dir outside every fixture (so fixture roots are not
    /// "broad", CFG-04).
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            scull_home: tempfile::tempdir().unwrap(),
            home: dir.path().to_path_buf(),
            _user_home: Some(dir),
            extra_env: Vec::new(),
        }
    }

    /// `SCULL_HOME` (config/, data/, cache/, state/ below it).
    pub fn scull_home(&self) -> &Path {
        self.scull_home.path()
    }

    /// The user's home folder for the runs.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// scull's data directory.
    pub fn data_dir(&self) -> PathBuf {
        self.scull_home.path().join("data")
    }

    /// The history database, once a command created it.
    pub fn db(&self) -> PathBuf {
        self.data_dir().join(scull_storage::DB_FILE_NAME)
    }

    /// Set an extra environment variable on every run.
    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.extra_env.push((k.to_owned(), v.to_owned()));
        self
    }

    /// Run every command in `fx`'s sandbox (P2-EXEC-13, [`scull_fixtures::Fixture::sandbox_env`]):
    /// quarantine and the Trash fallback stay inside the fixture's temp dir.
    pub fn sandboxed(mut self, fx: &scull_fixtures::Fixture) -> Self {
        for (k, v) in fx.sandbox_env() {
            self.extra_env
                .push((k.to_owned(), v.to_string_lossy().into_owned()));
        }
        self
    }

    /// Write `config/config.toml`.
    pub fn write_config(&self, text: &str) {
        let dir = self.scull_home.path().join("config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    /// The command for `args`, environment isolated, stdin closed.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(scull_bin());
        c.args(args);
        // Temp dirs are tmpfs on some machines; the D-22 volume policy is tested in
        // scull-platform.
        // (`worktrees register` takes a worktree, not roots.)
        let roots = match args {
            ["scan" | "projects", ..] | ["worktrees"] => true,
            ["worktrees", sub, ..] => *sub != "register",
            _ => false,
        };
        if roots {
            c.arg("--allow-network-roots");
        }
        for (k, _) in std::env::vars_os() {
            if k.to_str()
                .is_some_and(|k| k.starts_with("SCULL_") || CACHE_VARS.contains(&k))
            {
                c.env_remove(k);
            }
        }
        // Agent clients' own config overrides would point outside the fixture home.
        c.env_remove("CODEX_HOME").env_remove("CLAUDE_CONFIG_DIR");
        c.env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("SCULL_HOME", self.scull_home.path())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join("AppData").join("Local"))
            .env("APPDATA", self.home.join("AppData").join("Roaming"))
            .stdin(Stdio::null());
        for (k, v) in &self.extra_env {
            c.env(k, v);
        }
        c
    }

    /// Run `args` to completion.
    pub fn run(&self, args: &[&str]) -> Output {
        let o = self.command(args).output().expect("run scull");
        println!("$ scull {} -> exit {:?}", args.join(" "), o.status.code());
        o
    }
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// The process exit code, asserting it exited normally.
pub fn code(o: &Output) -> i32 {
    o.status
        .code()
        .unwrap_or_else(|| panic!("terminated by a signal: {:?}", o.status))
}

pub fn assert_exit(o: &Output, want: i32) {
    assert_eq!(
        code(o),
        want,
        "stdout: {}\nstderr: {}",
        stdout(o),
        stderr(o)
    );
}

/// A published schema file: `crates/scull-cli/schemas/<name>` (per-command documents) or, with
/// a `v1/` prefix, `schemas/v1/<name>` (the core types).
pub fn schema_path(name: &str) -> PathBuf {
    match name.strip_prefix("v1/") {
        Some(core) => workspace().join("schemas/v1").join(core),
        None => workspace().join("crates/scull-cli/schemas").join(name),
    }
}

/// Validates documents against published schemas and records which schemas were used.
#[derive(Default)]
pub struct Schemas {
    used: Mutex<BTreeSet<String>>,
}

impl Schemas {
    /// `doc` is valid against the schema file `name` (see [`schema_path`]).
    pub fn validate(&self, name: &str, doc: &Value, what: &str) {
        let path = schema_path(name);
        let schema: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
        )
        .unwrap();
        let v = jsonschema::validator_for(&schema)
            .unwrap_or_else(|e| panic!("{name} does not compile: {e}"));
        let errors: Vec<String> = v
            .iter_errors(doc)
            .map(|e| format!("{e} at {}", e.instance_path()))
            .collect();
        assert!(errors.is_empty(), "{what} vs {name}: {errors:#?}\n{doc:#}");
        self.used.lock().unwrap().insert(name.to_owned());
    }

    /// The schema files validated against so far.
    pub fn used(&self) -> BTreeSet<String> {
        self.used.lock().unwrap().clone()
    }

    /// `--json`: exit 0, one document on stdout naming `scull/<name>@1`, valid against
    /// `<name>.schema.json`; every candidate in it valid against `v1/candidate.schema.json`.
    pub fn json(&self, cli: &Cli, args: &[&str], name: &str) -> Value {
        let mut a = args.to_vec();
        a.push("--json");
        let o = cli.run(&a);
        assert_exit(&o, 0);
        let doc: Value = serde_json::from_slice(&o.stdout)
            .unwrap_or_else(|e| panic!("stdout is one JSON document ({e}): {}", stdout(&o)));
        assert_eq!(doc["schema"], format!("scull/{name}@1"), "{doc:#}");
        self.validate(&format!("{name}.schema.json"), &doc, &a.join(" "));
        if let Some(cands) = doc["data"]["candidates"].as_array() {
            for c in cands {
                self.validate("v1/candidate.schema.json", c, "candidate");
            }
        }
        doc
    }

    /// `--ndjson`: exit 0, every stdout line one JSON object naming `scull/<name>@1`, valid
    /// against `<name>.ndjson.schema.json`; candidate records valid against the candidate
    /// schema.
    pub fn ndjson(&self, cli: &Cli, args: &[&str], name: &str) -> Vec<Value> {
        let mut a = args.to_vec();
        a.push("--ndjson");
        let o = cli.run(&a);
        assert_exit(&o, 0);
        let text = stdout(&o);
        assert!(text.ends_with('\n'), "NDJSON ends with a newline: {text:?}");
        let mut lines = Vec::new();
        for l in text.lines() {
            let v: Value = serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("NDJSON line is not JSON ({e}): {l:?}"));
            assert!(v.is_object(), "{l}");
            assert_eq!(v["schema"], format!("scull/{name}@1"), "{l}");
            self.validate(&format!("{name}.ndjson.schema.json"), &v, &a.join(" "));
            if v["type"] == "candidate" {
                self.validate("v1/candidate.schema.json", &v["data"], "candidate line");
            }
            lines.push(v);
        }
        assert!(!lines.is_empty(), "no NDJSON output for {a:?}");
        lines
    }

    /// A failed run in a machine mode: stdout empty, stderr one error envelope valid against
    /// `error.schema.json` (and `v1/error.schema.json` for the error inside it). Returns the
    /// envelope.
    pub fn error(&self, o: &Output, what: &str) -> Value {
        assert!(
            o.stdout.is_empty(),
            "{what}: stdout stays empty: {}",
            stdout(o)
        );
        let env: Value = serde_json::from_slice(&o.stderr)
            .unwrap_or_else(|e| panic!("{what}: stderr is one JSON error ({e}): {}", stderr(o)));
        assert_eq!(env["schema"], "scull/error@1", "{env:#}");
        self.validate("error.schema.json", &env, what);
        self.validate("v1/error.schema.json", &env["error"], what);
        env
    }
}

/// TOML basic string content for a path (backslashes escaped for Windows).
pub fn toml_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "\\\\")
}
