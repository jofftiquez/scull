//! `scull init` end to end (P1-CLI-03), non-interactive. Each run gets a fake home (with fake
//! toolchain binaries on `PATH`) and a separate temp `SCULL_HOME`; nothing else is touched.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Env {
    home: tempfile::TempDir,
    scull_home: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home");
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let exe = if cfg!(windows) { ".exe" } else { "" };
        for b in ["cargo", "rustc", "node", "pnpm", "ollama"] {
            std::fs::write(bin.join(format!("{b}{exe}")), "").expect("fake binary");
        }
        for d in ["Projects", "code", ".cargo/registry", ".claude"] {
            std::fs::create_dir_all(home.path().join(d)).expect("dir");
        }
        Self {
            home,
            scull_home: tempfile::tempdir().expect("scull home"),
        }
    }

    fn config_dir(&self) -> PathBuf {
        self.scull_home.path().join("config")
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
        cmd.args(args)
            .env("SCULL_HOME", self.scull_home.path())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("PATH", self.home.path().join("bin"))
            .env("TERM", "dumb")
            .stdin(Stdio::null());
        for (k, _) in std::env::vars() {
            if k.starts_with("SCULL_") && k != "SCULL_HOME" {
                cmd.env_remove(k);
            }
        }
        cmd.output().expect("run scull")
    }
}

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/init.schema.json")
}

fn validate(v: &Value) {
    let s: Value = serde_json::from_str(&std::fs::read_to_string(schema_path()).expect("schema"))
        .expect("schema JSON");
    let val = jsonschema::validator_for(&s).expect("compiles");
    let errors: Vec<String> = val.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).expect("read_dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            out.extend(tree(&p));
        }
        out.push(p);
    }
    out.sort();
    out
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test init`.
#[test]
fn committed_schema_matches_generated() {
    let generated = scull_cli::output::json::document_schema::<scull_cli::cmd::init::InitReport>(
        scull_cli::cmd::init::INIT_SCHEMA,
    );
    let text = serde_json::to_string_pretty(&generated).expect("serialize") + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(schema_path(), &text).expect("write");
    }
    let committed = std::fs::read_to_string(schema_path()).expect("schema file");
    assert_eq!(
        committed.replace("\r\n", "\n"),
        text,
        "schemas/init.schema.json is stale; rerun with SCULL_BLESS=1"
    );
}

#[test]
fn yes_accepts_detected_defaults_and_writes_owner_only_files() {
    let env = Env::new();
    let home_before = tree(env.home.path());
    let o = env.run(&["init", "--yes", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate(&v);
    assert_eq!(v["schema"], "scull/init@1");
    let d = &v["data"];
    let ids: Vec<&str> = d["detected"]["toolchains"]
        .as_array()
        .expect("toolchains")
        .iter()
        .map(|t| t["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["rust", "node", "ml"]);
    assert_eq!(
        d["detected"]["ai_clients"],
        serde_json::json!(["claude-code"])
    );
    assert_eq!(
        d["profile"]["builds"],
        serde_json::json!(["rust", "web", "ml"])
    );
    let roots = d["profile"]["roots"].as_array().expect("roots");
    assert!(roots.contains(&Value::from("~/Projects")), "{roots:?}");
    assert!(roots.contains(&Value::from("~/code")), "{roots:?}");
    assert_eq!(d["profile"]["agents"], "sometimes");

    assert_eq!(
        tree(env.home.path()),
        home_before,
        "the home dir is only probed"
    );
    let config = env.config_dir().join("config.toml");
    let profile = env.config_dir().join("profile.toml");
    let layer = scull_core::config::ConfigLayer::from_toml(
        &std::fs::read_to_string(&config).expect("config"),
    )
    .expect("config parses");
    let written: Vec<String> = layer
        .scan
        .roots
        .expect("roots")
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(Value::from(written), d["profile"]["roots"]);
    assert!(
        std::fs::read_to_string(&profile)
            .expect("profile")
            .contains("caution = \"balanced\"")
    );
    for p in [&env.config_dir(), &config, &profile] {
        scull_platform::ensure_owner_only(p)
            .unwrap_or_else(|e| panic!("{} not owner-only: {e}", p.display()));
    }
    // Only the config dir, the two files: no temp files left behind.
    assert_eq!(
        tree(env.scull_home.path()),
        vec![env.config_dir(), config, profile]
    );
}

#[test]
fn rerun_needs_force_and_keeps_other_settings() {
    let env = Env::new();
    std::fs::create_dir_all(env.config_dir()).expect("dir");
    std::fs::write(
        env.config_dir().join("config.toml"),
        "[mcp]\nenabled = true\n",
    )
    .expect("config");
    let o = env.run(&["init", "--yes", "--roots", "~/Projects"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let out = String::from_utf8(o.stdout).expect("utf-8");
    assert!(out.contains("Here's what scull will focus on"), "{out}");
    assert!(out.contains("Next: scull scan ~/Projects"), "{out}");
    let text = std::fs::read_to_string(env.config_dir().join("config.toml")).expect("config");
    let layer = scull_core::config::ConfigLayer::from_toml(&text).expect("parses");
    assert_eq!(
        layer.mcp.enabled,
        Some(true),
        "existing settings kept:\n{text}"
    );
    assert_eq!(layer.scan.roots, Some(vec![PathBuf::from("~/Projects")]));
    assert_eq!(
        std::fs::read_to_string(env.config_dir().join("config.toml.bak")).expect("backup"),
        "[mcp]\nenabled = true\n"
    );

    let o = env.run(&["init", "--yes"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--force"), "{}", stderr(&o));
    let o = env.run(&[
        "init",
        "--yes",
        "--force",
        "--roots",
        "~/code",
        "--caution",
        "cautious",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let profile = std::fs::read_to_string(env.config_dir().join("profile.toml")).expect("p");
    assert!(profile.contains("caution = \"cautious\""), "{profile}");
    assert!(profile.contains("~/code"), "{profile}");
}

#[test]
fn broad_roots_are_refused_without_explicit_confirmation() {
    let env = Env::new();
    for root in ["~", "/"] {
        let o = env.run(&["init", "--yes", "--roots", root]);
        assert_eq!(o.status.code(), Some(2), "{root}: {}", stderr(&o));
        assert!(stderr(&o).contains("--allow-broad-roots"), "{root}");
        assert!(!env.config_dir().exists(), "{root}: nothing written");
    }
    let o = env.run(&[
        "init",
        "--yes",
        "--roots",
        "~",
        "--allow-broad-roots",
        "--json",
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    assert_eq!(v["data"]["confirmed_broad_roots"], serde_json::json!(["~"]));
}

#[test]
fn needs_a_terminal_or_yes_and_validates_answers() {
    let env = Env::new();
    let o = env.run(&["init"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--yes"), "{}", stderr(&o));
    let o = env.run(&["init", "--yes", "--builds", "rust,cobol"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("cobol"));
    let o = env.run(&["init", "--yes", "--priority", "everything"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        !env.config_dir().exists(),
        "nothing written on usage errors"
    );
}
