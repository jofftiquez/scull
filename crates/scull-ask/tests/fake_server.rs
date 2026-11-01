//! End-to-end tests of `scull ask`'s loop against a fake model server on 127.0.0.1 (no real API
//! is ever called; FIXTURES I-6 is checked: the process holds no non-loopback socket).
//!
//! The model is scripted: each test lists the responses the fake server returns in order, and
//! inspects the requests it received. The tools are the real in-process read-only tools of
//! `scull-mcp` over a generated fixture, so redaction, the allowlist and "nothing changes on
//! disk" are checked on real results.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_ask::{
    AskConfig, Event, ProviderKind, SYSTEM_PROMPT, Session, Stop, Transport, UreqTransport,
};
use scull_core::ErrorKind;
use scull_core::config::{Config, McpConfig};
use scull_engine::{CancelToken, Engine, EngineConfig, FixedInUse, InUseSource};
use scull_fixtures::{Fixture, InvariantInputs, NodeLock, days_ago};
use scull_mcp::{ScullMcp, ServerOptions, read_only_tool_names};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// An absolute path outside the allowlist (`/etc` is not absolute on Windows).
const OUTSIDE: &str = if cfg!(windows) { r"C:\Windows" } else { "/etc" };

/// Unique text inside fixture files: must never reach the model.
const CONTENT_MARKER: &str = "CONTENT-MARKER-ask-7d2c-never-sent";

/// The API key used by the tests: must only ever appear in the auth header.
const KEY: &str = "sk-test-KEY-0123456789-never-logged";

/// One request the fake server received.
#[derive(Debug, Clone)]
struct Received {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// One scripted response: status, extra headers, JSON body.
type Scripted = (u16, Vec<(&'static str, String)>, Value);

/// A fake model endpoint: answers each connection with the next scripted response.
struct FakeServer {
    base: String,
    received: Arc<Mutex<Vec<Received>>>,
}

impl FakeServer {
    /// `script`: (status, extra headers, JSON body) per request, in order.
    fn start(script: Vec<Scripted>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let rec = Arc::clone(&received);
        std::thread::spawn(move || {
            for (status, extra, body) in script {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let mut headers = Vec::new();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
                    }
                }
                let len: usize = headers
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .map_or(0, |(_, v)| v.parse().unwrap());
                let mut buf = vec![0; len];
                reader.read_exact(&mut buf).unwrap();
                rec.lock().unwrap().push(Received {
                    path,
                    headers,
                    body: serde_json::from_slice(&buf).unwrap_or(Value::Null),
                });
                let payload = body.to_string();
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                    payload.len()
                );
                for (k, v) in extra {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                out.push_str(&payload);
                let mut s = stream;
                s.write_all(out.as_bytes()).unwrap();
                s.flush().unwrap();
            }
        });
        Self { base, received }
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

/// A Cargo project with a build output, an npm project with `node_modules` and a large file,
/// all idle for three days. Files carry [`CONTENT_MARKER`].
fn fixture() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 4096)
        .unwrap()
        .file(
            "app/src/main.rs",
            format!("// {CONTENT_MARKER}\nfn main() {{}}\n"),
        )
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .file("web/notes.txt", CONTENT_MARKER)
        .unwrap()
        .file_of_size("media/big.bin", 3 * 1024 * 1024)
        .unwrap();
    for d in ["app", "web", "media"] {
        fx.age(d, days_ago(3)).unwrap();
    }
    fx.checkpoint().unwrap();
    fx
}

/// The real read-only tools over `fx` (home = the fixture base, so paths show as `~/...`).
fn tools(fx: &Fixture, data: &Path) -> ScullMcp {
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(scull_platform::Native));
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(scull_safety::Tri::No)));
    ScullMcp::new(
        Engine::new(cfg).unwrap(),
        ServerOptions {
            roots: vec![fx.root().to_path_buf()],
            home: Some(fx.base().to_path_buf()),
            data_dir: Some(data.to_path_buf()),
            mcp: McpConfig {
                enabled: true,
                ..McpConfig::default()
            },
            threads: 2,
            cache_env: None,
            forecast_policy: scull_proactive::ForecastPolicy::default(),
        },
    )
    .unwrap()
}

fn config(file: &str, key: Option<&str>) -> AskConfig {
    AskConfig::resolve(Some(file), |k| {
        (k == "ANTHROPIC_API_KEY" || k == "OPENAI_API_KEY")
            .then(|| key.map(str::to_owned))
            .flatten()
    })
    .unwrap()
}

fn anthropic(base: &str) -> AskConfig {
    config(
        &format!("enabled = true\nbase_url = \"{base}\"\nmax_turns = 5"),
        Some(KEY),
    )
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

fn anthropic_reply(content: Vec<Value>, stop: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": content, "stop_reason": stop,
        "usage": { "input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 50 }
    })
}

/// Everything sent to the model in one request, as text.
fn sent_text(r: &Received) -> String {
    r.body.to_string()
}

#[test]
fn anthropic_loop_runs_only_read_only_tools_and_sends_redacted_results() {
    let fx = fixture();
    let data = tempfile::tempdir().unwrap();
    let host = tools(&fx, data.path());
    let thinking = json!({ "type": "thinking", "thinking": "", "signature": "sig-abc" });
    let server = FakeServer::start(vec![
        (
            200,
            vec![],
            anthropic_reply(
                vec![
                    thinking.clone(),
                    json!({ "type": "text", "text": "Let me look." }),
                    tool_use("toolu_1", "get_status", json!({})),
                    tool_use("toolu_2", "list_projects", json!({})),
                ],
                "tool_use",
            ),
        ),
        (
            200,
            vec![],
            anthropic_reply(
                vec![
                    tool_use(
                        "toolu_3",
                        "register_worktree",
                        json!({ "path": "~/root/app", "ttl": "1d" }),
                    ),
                    tool_use("toolu_4", "scan_directory", json!({ "path": OUTSIDE })),
                    tool_use("toolu_5", "delete_everything", json!({})),
                ],
                "tool_use",
            ),
        ),
        (
            200,
            vec![],
            anthropic_reply(
                vec![json!({ "type": "text", "text": "app/target is regenerable (S0)." })],
                "end_turn",
            ),
        ),
    ]);
    let cfg = anthropic(&server.base);
    let transport = UreqTransport::new(cfg.timeout, false);
    let session = Session::new(&cfg, &transport, &host);
    let mut events = Vec::new();
    let answer = session
        .run("why is my disk full?", &CancelToken::new(), &mut |e| {
            events.push(format!("{e:?}"))
        })
        .unwrap();

    assert_eq!(answer.text, "app/target is regenerable (S0).");
    assert_eq!(answer.stop, Stop::Done);
    assert_eq!(answer.turns, 3);
    assert_eq!(answer.usage.input_tokens, 300);
    assert_eq!(answer.usage.cache_read_tokens, 150);
    assert_eq!(
        answer.tool_calls,
        vec![
            ("get_status".to_owned(), false),
            ("list_projects".to_owned(), false),
            ("register_worktree".to_owned(), true),
            ("scan_directory".to_owned(), true),
            ("delete_everything".to_owned(), true),
        ]
    );
    // Refused calls never reach the tools (no ToolCall event for them).
    assert_eq!(
        events.iter().filter(|e| e.starts_with("ToolCall")).count(),
        3
    );

    let reqs = server.received();
    assert_eq!(reqs.len(), 3);
    let first = &reqs[0];
    assert_eq!(first.path, "/v1/messages");
    assert_eq!(first.header("x-api-key"), Some(KEY));
    assert_eq!(first.header("anthropic-version"), Some("2026-10-21"));
    // Not Anthropic's own API: no server-side fallback beta.
    assert_eq!(first.header("anthropic-beta"), None);
    assert_eq!(first.body["model"], "claude-opus-5-5");
    assert_eq!(first.body["system"], SYSTEM_PROMPT);
    assert_eq!(first.body["cache_control"], json!({ "type": "ephemeral" }));
    assert_eq!(
        first.body["messages"],
        json!([{ "role": "user", "content": "why is my disk full?" }])
    );
    let names: Vec<&str> = first.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, read_only_tool_names());
    assert!(!names.contains(&"register_worktree"));
    // The tool prefix and system prompt are identical on every turn (prompt cache).
    for r in &reqs[1..] {
        assert_eq!(r.body["tools"], first.body["tools"]);
        assert_eq!(r.body["system"], first.body["system"]);
    }

    // Turn 2: the assistant turn is echoed back unchanged (thinking block included), then one
    // user message with both results.
    let msgs = reqs[1].body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"][0], thinking);
    let results = msgs[2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["tool_use_id"], "toolu_1");
    assert_eq!(results[1]["tool_use_id"], "toolu_2");
    assert!(results.iter().all(|r| r.get("is_error").is_none()));
    let projects = results[1]["content"].as_str().unwrap();
    let sep = std::path::MAIN_SEPARATOR;
    assert!(projects.contains(&format!("~{sep}")), "{projects}");

    // Turn 3: the metadata tool, the path outside the allowlist and the made-up tool all fail.
    let msgs = reqs[2].body["messages"].as_array().unwrap();
    let results = msgs[4]["content"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r["is_error"] == json!(true)));
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap()
            .contains("unknown_tool")
    );
    assert!(
        results[1]["content"]
            .as_str()
            .unwrap()
            .contains("outside_allowlist")
    );
    assert!(
        results[2]["content"]
            .as_str()
            .unwrap()
            .contains("unknown_tool")
    );

    // Nothing private was sent: no key in bodies, no absolute home path, no file contents.
    let home = fx.base().to_string_lossy().into_owned();
    for r in &reqs {
        let t = sent_text(r);
        assert!(!t.contains(KEY));
        assert!(!t.contains(&home), "home path leaked: {t}");
        assert!(!t.contains(CONTENT_MARKER));
    }
    // register_worktree never ran: scull's registry was not written.
    assert!(
        !data
            .path()
            .join(scull_mcp::registry::REGISTRY_FILE)
            .exists()
    );
    // The fixture is byte-for-byte unchanged, and no non-loopback socket is held (I-6).
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn openai_compatible_loop_with_a_local_server_and_no_key() {
    let fx = fixture();
    let data = tempfile::tempdir().unwrap();
    let host = tools(&fx, data.path());
    let server = FakeServer::start(vec![
        (
            200,
            vec![],
            json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{
                            "id": "call_1", "type": "function",
                            "function": { "name": "get_cleanup_candidates", "arguments": "{\"tiers\":[\"S0\"]}" }
                        }, {
                            "id": "call_2", "type": "function",
                            "function": { "name": "get_status", "arguments": "not json" }
                        }]
                    }
                }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
            }),
        ),
        (
            200,
            vec![],
            json!({
                "choices": [{ "finish_reason": "stop",
                              "message": { "role": "assistant", "content": "Mostly node_modules." } }]
            }),
        ),
    ]);
    let cfg = config(
        &format!(
            "enabled = true\nprovider = \"openai\"\nmodel = \"llama3.2\"\nbase_url = \"{}/v1\"",
            server.base
        ),
        None,
    );
    assert_eq!(cfg.provider, ProviderKind::OpenAi);
    let transport = UreqTransport::new(cfg.timeout, false);
    let answer = Session::new(&cfg, &transport, &host)
        .run("what can I clean?", &CancelToken::new(), &mut |_| {})
        .unwrap();
    assert_eq!(answer.text, "Mostly node_modules.");
    assert_eq!(answer.tool_calls.len(), 2);
    assert!(!answer.tool_calls[0].1, "candidates call succeeds");
    assert!(answer.tool_calls[1].1, "bad arguments are refused");

    let reqs = server.received();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].path, "/v1/chat/completions");
    assert_eq!(reqs[0].header("authorization"), None);
    let msgs = reqs[0].body["messages"].as_array().unwrap();
    assert_eq!(
        msgs[0],
        json!({ "role": "system", "content": SYSTEM_PROMPT })
    );
    assert_eq!(reqs[0].body["tools"][0]["type"], "function");
    let msgs = reqs[1].body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5);
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_1");
    assert_eq!(msgs[4]["tool_call_id"], "call_2");
    assert!(
        msgs[4]["content"]
            .as_str()
            .unwrap()
            .contains("invalid_arguments")
    );
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// A tool host with no tools, for tests that never get as far as a tool call.
struct NoTools;

impl scull_ask::ToolHost for NoTools {
    fn tools(&self) -> Vec<scull_mcp::ToolSpec> {
        Vec::new()
    }
    fn call(
        &self,
        _: &str,
        _: serde_json::Map<String, Value>,
        _: &CancelToken,
    ) -> scull_mcp::LocalResult {
        panic!("no tool may run")
    }
}

#[test]
fn provider_errors_are_reported_without_the_key() {
    let server = FakeServer::start(vec![
        (
            401,
            vec![],
            json!({ "type": "error", "error": { "type": "authentication_error",
                    "message": format!("invalid x-api-key {KEY}\u{1b}[31m") } }),
        ),
        (
            529,
            vec![],
            json!({ "type": "error", "error": { "message": "Overloaded" } }),
        ),
    ]);
    let cfg = anthropic(&server.base);
    let transport = UreqTransport::new(cfg.timeout, false);
    let s = Session::new(&cfg, &transport, &NoTools);
    let e = s.run("q", &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);
    assert!(e.message.contains("HTTP 401"), "{}", e.message);
    assert!(e.message.contains("check the API key"));
    assert!(!e.message.contains(KEY), "{}", e.message);
    assert!(!e.message.contains('\u{1b}'), "escaped: {}", e.message);
    let e = s.run("q", &CancelToken::new(), &mut |_| {}).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Io);
    assert!(e.message.contains("HTTP 529") && e.message.contains("Overloaded"));
}

#[test]
fn redirects_are_never_followed() {
    let server = FakeServer::start(vec![
        (
            307,
            vec![("location", "http://127.0.0.1:9/steal".to_owned())],
            json!({}),
        ),
        (200, vec![], anthropic_reply(vec![], "end_turn")),
    ]);
    let cfg = anthropic(&server.base);
    let transport = UreqTransport::new(cfg.timeout, false);
    let e = Session::new(&cfg, &transport, &NoTools)
        .run("q", &CancelToken::new(), &mut |_| {})
        .unwrap_err();
    assert!(e.message.contains("HTTP 307"), "{}", e.message);
    assert_eq!(server.received().len(), 1);
}

#[test]
fn refusals_and_the_turn_limit_end_the_loop() {
    let refusal = json!({
        "content": [], "stop_reason": "refusal",
        "stop_details": { "type": "refusal", "category": "cyber", "explanation": null },
        "usage": { "input_tokens": 1, "output_tokens": 0 }
    });
    let looping = anthropic_reply(vec![tool_use("t", "get_status", json!({}))], "tool_use");
    let mut script = vec![(200, vec![], refusal)];
    script.extend((0..2).map(|_| (200, vec![], looping.clone())));
    let server = FakeServer::start(script);
    let mut cfg = anthropic(&server.base);
    cfg.max_turns = 2;
    let transport = UreqTransport::new(cfg.timeout, false);
    let fx = fixture();
    let data = tempfile::tempdir().unwrap();
    let host = tools(&fx, data.path());
    let s = Session::new(&cfg, &transport, &host);
    let a = s.run("q", &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(a.stop, Stop::Refusal(Some("cyber".to_owned())));
    let a = s.run("q", &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(a.stop, Stop::Other("turn_limit".to_owned()));
    assert_eq!(a.turns, 2);
    assert_eq!(server.received().len(), 3);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// A transport that fails the test if anything is sent.
struct NeverSend;

impl Transport for NeverSend {
    fn post_json(
        &self,
        url: &str,
        _: &[(&'static str, String)],
        _: &[u8],
    ) -> Result<scull_ask::HttpResponse, scull_core::ScullError> {
        panic!("nothing may be sent, but a request to {url} was attempted")
    }
}

#[test]
fn nothing_is_sent_when_off_without_a_key_or_when_cancelled() {
    // Off by default.
    let off = config("", Some(KEY));
    let e = Session::new(&off, &NeverSend, &NoTools)
        .run("q", &CancelToken::new(), &mut |_| {})
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);
    // On, but no key for a remote provider.
    let no_key = config("enabled = true", None);
    let e = Session::new(&no_key, &NeverSend, &NoTools)
        .run("q", &CancelToken::new(), &mut |_| {})
        .unwrap_err();
    assert!(e.message.contains("ANTHROPIC_API_KEY"));
    // Cancelled before the first request.
    let on = config("enabled = true", Some(KEY));
    let cancel = CancelToken::new();
    cancel.cancel();
    let e = Session::new(&on, &NeverSend, &NoTools)
        .run("q", &cancel, &mut |_| {})
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cancelled);
}

#[test]
fn the_first_request_preview_is_what_is_sent() {
    let server = FakeServer::start(vec![(
        200,
        vec![],
        anthropic_reply(vec![json!({ "type": "text", "text": "ok" })], "end_turn"),
    )]);
    let fx = fixture();
    let data = tempfile::tempdir().unwrap();
    let host = tools(&fx, data.path());
    let cfg = anthropic(&server.base);
    let transport = UreqTransport::new(cfg.timeout, false);
    let s = Session::new(&cfg, &transport, &host);
    let preview = s.first_request("how big is web?");
    s.run("how big is web?", &CancelToken::new(), &mut |_| {})
        .unwrap();
    assert_eq!(server.received()[0].body, preview);
    let shown = scull_ask::render_request(&cfg, &preview);
    assert!(!shown.contains(KEY) && shown.contains("<redacted>"));
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn events_report_each_tool_before_and_after_it_runs() {
    let server = FakeServer::start(vec![
        (
            200,
            vec![],
            anthropic_reply(vec![tool_use("a", "get_status", json!({}))], "tool_use"),
        ),
        (
            200,
            vec![],
            anthropic_reply(vec![json!({ "type": "text", "text": "done" })], "end_turn"),
        ),
    ]);
    let fx = fixture();
    let data = tempfile::tempdir().unwrap();
    let host = tools(&fx, data.path());
    let cfg = anthropic(&server.base);
    let transport = UreqTransport::new(cfg.timeout, false);
    let mut seen = Vec::new();
    Session::new(&cfg, &transport, &host)
        .run("q", &CancelToken::new(), &mut |e| {
            seen.push(match e {
                Event::Asking { turn } => format!("ask {turn}"),
                Event::ToolCall { name, .. } => format!("call {name}"),
                Event::ToolDone { name, is_error } => format!("done {name} {is_error}"),
            });
        })
        .unwrap();
    assert_eq!(
        seen,
        ["ask 1", "call get_status", "done get_status false", "ask 2"]
    );
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// SAFE-10 for the assistant: scull-ask and its scull dependencies never reach scull-cleaner.
#[test]
fn never_depends_on_the_cleaner() {
    let crates = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut seen: Vec<String> = Vec::new();
    let mut todo = vec!["scull-ask".to_owned()];
    while let Some(name) = todo.pop() {
        if seen.contains(&name) {
            continue;
        }
        let text = std::fs::read_to_string(crates.join(&name).join("Cargo.toml")).unwrap();
        let mut in_deps = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_deps = line == "[dependencies]";
                continue;
            }
            if in_deps
                && let Some(dep) = line.split(['.', ' ', '=']).next()
                && dep.starts_with("scull-")
            {
                todo.push(dep.to_owned());
            }
        }
        seen.push(name);
    }
    assert!(seen.contains(&"scull-mcp".to_owned()), "{seen:?}");
    assert!(!seen.iter().any(|c| c == "scull-cleaner"), "{seen:?}");
}
