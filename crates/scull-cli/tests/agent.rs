//! `scull agent install|uninstall <client>` end to end (P4-AI-01).
//!
//! **No test touches a real client config.** Every run gets a scull-fixtures tree as its home
//! folder (`HOME`, `USERPROFILE`, `APPDATA`, `XDG_CONFIG_HOME` all inside it; `CODEX_HOME` and
//! `CLAUDE_CONFIG_DIR` removed) and `SCULL_HOME` inside the same tree. After each test the
//! fixture invariants run: the canary beside the tree is untouched and nothing in the tree
//! changed except the files the command is expected to write.
// A command outside the personal MVP: tested only in the full build (#142).
#![cfg(feature = "full")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_fixtures::{Fixture, InvariantInputs};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ALL: [&str; 5] = ["claude-code", "codex", "cursor", "vscode", "gemini-cli"];

/// `(client, config path, guidance path)` relative to the fake home, for this OS.
fn files(client: &str) -> (PathBuf, Option<PathBuf>) {
    let p = |s: &str| s.split('/').collect::<PathBuf>();
    match client {
        "claude-code" => (p(".claude.json"), Some(p(".claude/CLAUDE.md"))),
        "codex" => (p(".codex/config.toml"), Some(p(".codex/AGENTS.md"))),
        "cursor" => (p(".cursor/mcp.json"), None),
        "vscode" => (
            if cfg!(windows) {
                p("AppData/Roaming/Code/User/mcp.json")
            } else if cfg!(target_os = "macos") {
                p("Library/Application Support/Code/User/mcp.json")
            } else {
                p(".config/Code/User/mcp.json")
            },
            None,
        ),
        "gemini-cli" => (p(".gemini/settings.json"), Some(p(".gemini/GEMINI.md"))),
        _ => unreachable!(),
    }
}

fn run(home: &Path, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
    c.args(args)
        .env("SCULL_HOME", home.join("scull-home"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData").join("Roaming"))
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            c.env_remove(k);
        }
    }
    c.output().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn schema() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/agent.schema.json");
    serde_json::from_str(&read(&p)).unwrap()
}

fn validate(v: &Value) {
    let s = schema();
    let val = jsonschema::validator_for(&s).unwrap();
    let errors: Vec<String> = val.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

/// The committed schema matches the code. Regenerate with
/// `SCULL_BLESS=1 cargo test -p scull-cli --test agent`.
#[test]
fn schema_is_current() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/agent.schema.json");
    let fresh = serde_json::to_string_pretty(&scull_cli::output::json::document_schema::<
        scull_cli::cmd::agent::AgentReport,
    >(scull_cli::cmd::agent::AGENT_SCHEMA))
    .unwrap()
        + "\n";
    if std::env::var_os("SCULL_BLESS").is_some() {
        std::fs::write(&path, &fresh).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap_or_default(),
        fresh,
        "schemas/agent.schema.json is stale; rerun with SCULL_BLESS=1"
    );
}

/// An existing config with another server and unrelated settings, per format.
fn seed(client: &str) -> String {
    match client {
        "codex" => "# my settings\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"other\"\n".into(),
        "vscode" => "{\n  \"servers\": {\n    \"other\": {\"type\": \"stdio\", \"command\": \"other\"}\n  },\n  \"inputs\": []\n}\n".into(),
        _ => "{\n  \"theme\": \"dark\",\n  \"mcpServers\": {\n    \"other\": {\"command\": \"other\", \"args\": [\"x\"]}\n  },\n  \"numStartups\": 42\n}\n".into(),
    }
}

fn scull_entry(client: &str, text: &str) -> Option<(String, Vec<String>)> {
    if client == "codex" {
        let v: toml::Value = toml::from_str(text).unwrap();
        let e = v.get("mcp_servers")?.get("scull")?;
        return Some((
            e.get("command")?.as_str()?.to_owned(),
            e.get("args")?
                .as_array()?
                .iter()
                .map(|a| a.as_str().unwrap().to_owned())
                .collect(),
        ));
    }
    let v: Value = serde_json::from_str(text).unwrap();
    let key = if client == "vscode" {
        "servers"
    } else {
        "mcpServers"
    };
    let e = v.get(key)?.get("scull")?;
    Some((
        e["command"].as_str()?.to_owned(),
        e["args"]
            .as_array()?
            .iter()
            .map(|a| a.as_str().unwrap().to_owned())
            .collect(),
    ))
}

fn other_server_kept(client: &str, text: &str) -> bool {
    if client == "codex" {
        text.contains("[mcp_servers.other]\ncommand = \"other\"") && text.contains("model = \"o3\"")
    } else {
        text.contains("\"other\": {") && !text.contains("\"other\": null")
    }
}

#[test]
fn every_client_install_and_uninstall_with_consent() {
    for client in ALL {
        let mut fx = Fixture::new().unwrap();
        let (cfg_rel, guide_rel) = files(client);
        fx.file(&cfg_rel, seed(client)).unwrap();
        fx.checkpoint().unwrap();
        let home = fx.root().to_path_buf();
        let cfg = home.join(&cfg_rel);
        let before = read(&cfg);

        // 1. --dry-run: exact path and diff, the disclosure, nothing written.
        let o = run(&home, &["agent", "install", client, "--dry-run"]);
        assert!(o.status.success(), "{client}: {}", err(&o));
        let text = out(&o);
        assert!(
            text.contains(&cfg.display().to_string()),
            "{client}: {text}"
        );
        assert!(text.contains("+++ "), "{client}: {text}");
        assert!(text.contains("privacy notice"), "{client}: {text}");
        assert!(
            text.contains("never --allow-destructive"),
            "{client}: {text}"
        );
        assert!(text.contains("Dry run: nothing was written."), "{client}");
        assert_eq!(read(&cfg), before, "{client}: dry run wrote");
        assert!(!home.join("scull-home").exists(), "{client}: dry run wrote");

        // 2. No terminal and no --yes: refused, nothing written.
        let o = run(&home, &["agent", "install", client]);
        assert_eq!(o.status.code(), Some(2), "{client}: {}", err(&o));
        assert!(err(&o).contains("--yes"), "{client}: {}", err(&o));
        assert_eq!(read(&cfg), before, "{client}");
        assert!(!home.join("scull-home").exists(), "{client}");

        // 3. --json without --yes or --dry-run never prompts.
        let o = run(&home, &["agent", "install", client, "--json"]);
        assert_eq!(o.status.code(), Some(2), "{client}");
        assert_eq!(read(&cfg), before, "{client}");

        // 4. --yes: applied, other entries kept, backup owner-only, scull enabled.
        let o = run(&home, &["agent", "install", client, "--yes", "--json"]);
        assert!(o.status.success(), "{client}: {}", err(&o));
        let v: Value = serde_json::from_str(&out(&o)).unwrap();
        validate(&v);
        let data = &v["data"];
        assert_eq!(data["applied"], true);
        assert_eq!(data["client"], client);
        assert!(
            data["disclosure"]
                .as_str()
                .unwrap()
                .contains("privacy notice")
        );
        let after = read(&cfg);
        assert_eq!(
            scull_entry(client, &after),
            Some(("scull".into(), vec!["mcp".into(), "serve".into()])),
            "{client}: {after}"
        );
        assert!(other_server_kept(client, &after), "{client}: {after}");
        let changes = data["changes"].as_array().unwrap();
        let backup = PathBuf::from(changes[0]["backup"].as_str().unwrap());
        assert_eq!(read(&backup), before, "{client}: backup holds the old file");
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".scull-bak-")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&backup, &cfg] {
                let mode = std::fs::metadata(p).unwrap().permissions().mode();
                assert_eq!(mode & 0o077, 0, "{client}: {} owner-only", p.display());
            }
        }
        if let Some(g) = &guide_rel {
            let t = read(&home.join(g));
            assert!(t.contains("<!-- scull:begin"), "{client}: {t}");
            assert!(t.contains("get_status"), "{client}");
        }
        let scull_cfg = home.join("scull-home").join("config").join("config.toml");
        assert!(read(&scull_cfg).contains("enabled = true"));
        assert!(
            home.join("scull-home")
                .join("state")
                .join("mcp-disclosure-shown")
                .exists()
        );

        // 5. Installing again changes nothing.
        let o = run(&home, &["agent", "install", client, "--yes"]);
        assert!(o.status.success(), "{client}: {}", err(&o));
        assert!(
            out(&o).contains("Nothing to change."),
            "{client}: {}",
            out(&o)
        );
        assert_eq!(read(&cfg), after);

        // 6. Uninstall removes only scull's entry and block.
        let o = run(&home, &["agent", "uninstall", client, "--dry-run"]);
        assert!(o.status.success(), "{client}: {}", err(&o));
        assert!(out(&o).contains("-"), "{client}");
        assert_eq!(read(&cfg), after, "{client}: uninstall dry run wrote");
        let o = run(&home, &["agent", "uninstall", client, "--yes", "--json"]);
        assert!(o.status.success(), "{client}: {}", err(&o));
        validate(&serde_json::from_str(&out(&o)).unwrap());
        let restored = read(&cfg);
        assert_eq!(scull_entry(client, &restored), None, "{client}: {restored}");
        assert_eq!(
            restored, before,
            "{client}: uninstall restores the original"
        );
        if let Some(g) = &guide_rel {
            assert!(!read(&home.join(g)).contains("scull:begin"), "{client}");
        }

        // Nothing else in the fake home changed; the canary is untouched.
        let mut allowed = InvariantInputs::new().allow(&cfg_rel).allow("scull-home");
        if let Some(g) = &guide_rel {
            allowed = allowed.allow(g.parent().unwrap());
        }
        for e in std::fs::read_dir(home.join(cfg_rel.parent().unwrap_or(Path::new("")))).unwrap() {
            let name = e.unwrap().file_name();
            if name.to_string_lossy().contains(".scull-bak-") {
                allowed = allowed.allow(cfg_rel.parent().unwrap_or(Path::new("")).join(&name));
            }
        }
        fx.check_invariants(&allowed).assert_ok();
    }
}

#[test]
fn install_into_a_home_without_the_client_creates_only_its_files() {
    let mut fx = Fixture::new().unwrap();
    fx.file("unrelated.txt", "keep me\n").unwrap();
    fx.checkpoint().unwrap();
    let home = fx.root().to_path_buf();
    let o = run(&home, &["agent", "install", "cursor", "--dry-run"]);
    assert!(o.status.success(), "{}", err(&o));
    let text = out(&o);
    assert!(text.contains("does not seem to be installed"), "{text}");
    assert!(text.contains("--- /dev/null"), "{text}");
    assert!(
        text.contains("Guidance (paste into your project rules)"),
        "{text}"
    );
    let o = run(
        &home,
        &[
            "agent",
            "install",
            "cursor",
            "--yes",
            "--command",
            "/opt/scull/bin/scull",
        ],
    );
    assert!(o.status.success(), "{}", err(&o));
    let cfg = read(&home.join(".cursor").join("mcp.json"));
    assert_eq!(
        scull_entry("cursor", &cfg),
        Some((
            "/opt/scull/bin/scull".into(),
            vec!["mcp".into(), "serve".into()]
        ))
    );
    fx.check_invariants(&InvariantInputs::new().allow(".cursor").allow("scull-home"))
        .assert_ok();
}

#[test]
fn jsonc_configs_are_refused_without_writing() {
    let mut fx = Fixture::new().unwrap();
    let (rel, _) = files("vscode");
    fx.file(&rel, "{\n  // my servers\n  \"servers\": {},\n}\n")
        .unwrap();
    fx.checkpoint().unwrap();
    let home = fx.root().to_path_buf();
    let o = run(&home, &["agent", "install", "vscode", "--yes"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(err(&o).contains("add the entry by hand"), "{}", err(&o));
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn unknown_client_is_a_usage_error() {
    let mut fx = Fixture::new().unwrap();
    fx.checkpoint().unwrap();
    let o = run(fx.root(), &["agent", "install", "notepad"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("claude-code"), "{}", err(&o));
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
