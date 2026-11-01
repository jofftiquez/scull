//! `scull mcp serve` end to end over real stdio (P1-MCP-01): opt-in enablement, the privacy
//! disclosure on first start, a JSON-RPC session (initialize → tools/list → tools/call), a
//! refused out-of-allowlist call, and nothing on disk changes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_fixtures::{Fixture, InvariantInputs};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

struct Env {
    scull_home: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            scull_home: tempfile::tempdir().unwrap(),
        }
    }

    fn write_config(&self, text: &str) {
        let dir = self.scull_home.path().join("config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    fn command(&self, home: &Path, extra: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(["mcp", "serve"])
            .args(extra)
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("SCULL_HOME", self.scull_home.path())
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("LOCALAPPDATA", home.join("AppData").join("Local"))
            .env("APPDATA", home.join("AppData").join("Roaming"))
            .env_remove("SCULL_MCP")
            .env_remove("CARGO_TARGET_DIR");
        c
    }
}

fn toml_path(p: &Path) -> String {
    // TOML basic strings: escape backslashes (Windows paths).
    p.to_string_lossy().replace('\\', "\\\\")
}

/// A running `scull mcp serve` speaking JSON-RPC lines.
struct Session {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start(mut c: Command) -> Self {
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            out,
            next_id: 1,
        }
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            assert!(
                self.out.read_line(&mut line).unwrap() > 0,
                "server closed stdout"
            );
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout carries only JSON-RPC ({e}): {line:?}"));
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    /// Close stdin and wait: returns (exit code, stderr).
    fn finish(mut self) -> (Option<i32>, String) {
        drop(self.child.stdin.take());
        let status = self.child.wait().unwrap();
        let mut rest = String::new();
        self.out.read_to_string(&mut rest).unwrap();
        assert!(rest.trim().is_empty(), "unexpected stdout: {rest}");
        let mut err = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        (status.code(), err)
    }

    fn initialize(&mut self) -> Value {
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "scull-cli-test", "version": "0" }
            }),
        );
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        init
    }
}

#[test]
fn refuses_to_start_unless_enabled() {
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let o = env
        .command(home.path(), &[])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2), "configuration error");
    assert!(o.stdout.is_empty(), "stdout stays empty");
    let err = String::from_utf8(o.stderr).unwrap();
    assert!(err.contains("not enabled"), "{err}");
    assert!(err.contains("enabled = true"), "{err}");
    // Nothing was written.
    assert!(!env.scull_home.path().join("config").exists());
    assert!(!env.scull_home.path().join("state").exists());
}

#[test]
fn serves_read_only_tools_over_stdio() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 4096)
        .unwrap();
    fx.checkpoint().unwrap();
    let env = Env::new();
    env.write_config(&format!(
        "[scan]\nroots = [\"{}\"]\n\n[mcp]\nenabled = true\n",
        toml_path(fx.root())
    ));
    let home = fx.base();

    let mut s = Session::start(env.command(home, &[]));
    let init = s.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "scull", "{init}");
    let list = s.request("tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, scull_mcp::TOOL_NAMES);
    let status = s.request(
        "tools/call",
        json!({ "name": "get_status", "arguments": {} }),
    );
    let sc = &status["result"]["structuredContent"];
    assert_eq!(sc["server"]["read_only"], true, "{status}");
    assert_eq!(
        sc["server"]["allowlisted_roots"],
        json!(["~/root"]),
        "{status}"
    );
    let refused = s.request(
        "tools/call",
        json!({ "name": "scan_directory", "arguments": { "path": fx.canary().dir } }),
    );
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(
        refused["result"]["structuredContent"]["error"]["code"],
        "outside_allowlist"
    );
    let (code, err) = s.finish();
    assert_eq!(code, Some(0), "{err}");
    assert!(
        err.contains("privacy notice"),
        "disclosure on first start: {err}"
    );
    assert!(
        env.scull_home
            .path()
            .join("state")
            .join("mcp-disclosure-shown")
            .exists()
    );

    // Second start: no disclosure; --allow-destructive changes nothing.
    let mut s = Session::start(env.command(home, &["--allow-destructive"]));
    s.initialize();
    let list = s.request("tools/list", json!({}));
    assert_eq!(
        list["result"]["tools"].as_array().unwrap().len(),
        scull_mcp::TOOL_NAMES.len()
    );
    let (code, err) = s.finish();
    assert_eq!(code, Some(0), "{err}");
    assert!(!err.contains("privacy notice"), "{err}");
    assert!(err.contains("no destructive MCP tools"), "{err}");
    // No history database: MCP calls record nothing.
    assert!(!env.scull_home.path().join("data").join("scull.db").exists());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Ctrl-C (SIGINT) to `scull mcp serve` exits 130, "cancelled by user" (CLI-05), both before
/// the client sent `initialize` (#251: someone started the server in a terminal by hand) and
/// during a session; promptly, while stdin is still open.
///
/// Unix only: the test sends SIGINT with `kill -INT`. Windows has no equivalent a test can
/// aim at one child process: `GenerateConsoleCtrlEvent` delivers CTRL_BREAK/CTRL_C to a whole
/// console process group, which needs a new console group for the child and an OS API outside
/// scull-platform (PLT-11). The cancel path itself is platform-independent (`ctrlc` sets the
/// same flag on both).
#[cfg(unix)]
#[test]
fn ctrl_c_exits_130_before_and_after_initialize() {
    use std::time::{Duration, Instant};

    let mut fx = Fixture::new().unwrap();
    fx.checkpoint().unwrap();
    let env = Env::new();
    env.write_config(&format!(
        "[scan]\nroots = [\"{}\"]\n\n[mcp]\nenabled = true\n",
        toml_path(fx.root())
    ));

    for handshake in [false, true] {
        let mut s = Session::start(env.command(fx.base(), &[]));
        if handshake {
            s.initialize();
        } else {
            // Wait until the server is up (the disclosure is on stderr before it serves);
            // a fixed pause keeps the test simple and the handler installed long before.
            std::thread::sleep(Duration::from_millis(700));
        }
        let pid = s.child.id().to_string();
        let k = Command::new("kill").args(["-INT", &pid]).status().unwrap();
        assert!(k.success(), "kill -INT {pid}");
        let sent = Instant::now();
        // stdin stays open: the server must stop on the signal, not on EOF.
        let status = s.child.wait().unwrap();
        let elapsed = sent.elapsed();
        let mut err = String::new();
        s.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        assert_eq!(status.code(), Some(130), "handshake={handshake}: {err}");
        assert!(
            err.contains("cancelled by user"),
            "handshake={handshake}: {err}"
        );
        assert!(
            !err.contains("failed to start"),
            "handshake={handshake}: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "handshake={handshake}: stopped promptly, took {elapsed:?}"
        );
        drop(s.child.stdin.take());
    }
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
