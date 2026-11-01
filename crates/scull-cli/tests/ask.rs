//! `scull ask` end to end (P5-AI-01) against a fake model server on 127.0.0.1: off by default,
//! `--show-request` sends nothing, consent is required before the first request and recorded
//! without the key, the answer is printed escaped, the tools read are listed on stderr, and
//! nothing on disk changes. No real API is ever called.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_fixtures::{Fixture, InvariantInputs};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const KEY: &str = "sk-cli-test-KEY-5150-never-logged";

/// A fake model endpoint answering each connection with the next scripted body (HTTP 200).
struct FakeServer {
    base: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    keys: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    fn start(script: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let keys = Arc::new(Mutex::new(Vec::new()));
        let (b, k) = (Arc::clone(&bodies), Arc::clone(&keys));
        std::thread::spawn(move || {
            for reply in script {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (name, value) = h.split_once(':').unwrap();
                    match name.trim().to_ascii_lowercase().as_str() {
                        "content-length" => len = value.trim().parse().unwrap(),
                        "x-api-key" => k.lock().unwrap().push(value.trim().to_owned()),
                        _ => {}
                    }
                }
                let mut buf = vec![0; len];
                r.read_exact(&mut buf).unwrap();
                b.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&buf).unwrap());
                let payload = reply.to_string();
                let mut s = stream;
                write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                    payload.len()
                )
                .unwrap();
            }
        });
        Self { base, bodies, keys }
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

struct Env {
    scull_home: tempfile::TempDir,
    fx: Fixture,
}

impl Env {
    fn new() -> Self {
        let mut fx = Fixture::new().unwrap();
        fx.cargo_project("app", "app")
            .unwrap()
            .cargo_target("app", "app", 4096)
            .unwrap();
        fx.checkpoint().unwrap();
        let env = Self {
            scull_home: tempfile::tempdir().unwrap(),
            fx,
        };
        let root = env.fx.root().to_string_lossy().replace('\\', "\\\\");
        env.write("config.toml", &format!("[scan]\nroots = [\"{root}\"]\n"));
        env
    }

    fn write(&self, name: &str, text: &str) {
        let dir = self.scull_home.path().join("config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn consent_file(&self) -> std::path::PathBuf {
        self.scull_home
            .path()
            .join("state")
            .join("ask-consent.json")
    }

    fn run(&self, args: &[&str]) -> Output {
        let home: &Path = self.fx.base();
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(args)
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("SCULL_HOME", self.scull_home.path())
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("LOCALAPPDATA", home.join("AppData").join("Local"))
            .env("APPDATA", home.join("AppData").join("Roaming"))
            .env("ANTHROPIC_API_KEY", KEY)
            .env_remove("OPENAI_API_KEY")
            .env_remove("HTTPS_PROXY")
            .env_remove("HTTP_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("CARGO_TARGET_DIR")
            .stdin(Stdio::null());
        for (k, _) in std::env::vars() {
            if k.starts_with("SCULL_ASK") {
                c.env_remove(k);
            }
        }
        c.output().unwrap()
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn reply(content: Value, stop: &str) -> Value {
    json!({ "content": content, "stop_reason": stop,
            "usage": { "input_tokens": 10, "output_tokens": 3 } })
}

#[test]
fn off_by_default_and_show_request_sends_nothing() {
    let env = Env::new();
    let server = FakeServer::start(vec![]);
    let out = env.run(&["ask", "why is my disk full?"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("off (the default)"),
        "{}",
        text(&out.stderr)
    );

    env.write("ask.toml", &format!("base_url = \"{}\"\n", server.base));
    let out = env.run(&["ask", "--show-request", "why", "is", "my", "disk", "full?"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("scull ask: privacy notice"));
    assert!(stdout.contains("scull ask is off. First request (nothing was sent):"));
    assert!(stdout.contains(&format!("POST {}/v1/messages", server.base)));
    assert!(stdout.contains("x-api-key: <redacted>"));
    assert!(stdout.contains("\"content\": \"why is my disk full?\""));
    assert!(stdout.contains("\"name\": \"get_status\""));
    assert!(!stdout.contains("register_worktree"));
    assert!(!stdout.contains(KEY) && !text(&out.stderr).contains(KEY));

    // Machine output is refused.
    let out = env.run(&["ask", "--json", "q"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(server.bodies().is_empty(), "nothing may be sent");
    assert!(!env.consent_file().exists());
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn consent_is_required_then_recorded_and_the_answer_is_printed() {
    let env = Env::new();
    let server = FakeServer::start(vec![
        reply(
            json!([{ "type": "tool_use", "id": "t1", "name": "get_status", "input": {} }]),
            "tool_use",
        ),
        reply(
            json!([{ "type": "text", "text": "Line one.\nYour root is ~/root \u{1b}[31m." }]),
            "end_turn",
        ),
        reply(json!([{ "type": "text", "text": "Again." }]), "end_turn"),
    ]);
    env.write(
        "ask.toml",
        &format!("enabled = true\nbase_url = \"{}\"\n", server.base),
    );

    // No terminal and no --yes: refused, nothing sent.
    let out = env.run(&["ask", "where", "is", "my", "space?"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
    let err = text(&out.stderr);
    assert!(err.contains("scull ask: privacy notice"), "{err}");
    assert!(err.contains("needs your consent"), "{err}");
    assert!(server.bodies().is_empty());

    // --yes: the notice is shown, consent recorded, the question answered.
    let out = env.run(&["ask", "--yes", "where", "is", "my", "space?"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert_eq!(stdout, "Line one.\nYour root is ~/root \\x1b[31m.\n");
    let err = text(&out.stderr);
    assert!(err.contains("reading: get_status"), "{err}");
    assert!(err.contains("2 model call(s), 1 tool call(s)"), "{err}");
    assert!(!err.contains(KEY) && !stdout.contains(KEY));
    let consent = std::fs::read_to_string(env.consent_file()).unwrap();
    assert!(consent.contains("\"provider\": \"anthropic\""), "{consent}");
    assert!(consent.contains(&server.base), "{consent}");
    assert!(!consent.contains(KEY) && !consent.contains("space"));

    // Recorded: no --yes needed next time, and no notice.
    let out = env.run(&["ask", "again?"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "Again.\n");
    assert!(!text(&out.stderr).contains("privacy notice"));

    let bodies = server.bodies();
    assert_eq!(bodies.len(), 3);
    for b in &bodies {
        assert!(!b.to_string().contains(KEY));
    }
    assert_eq!(*server.keys.lock().unwrap(), vec![KEY; 3]);
    // The tool result is redacted: the fixture root is shown under `~`.
    let result = bodies[1]["messages"][2]["content"][0]["content"]
        .as_str()
        .unwrap();
    assert!(result.contains("allowlisted_roots"), "{result}");
    assert!(!result.contains(&env.fx.base().to_string_lossy().into_owned()));
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
