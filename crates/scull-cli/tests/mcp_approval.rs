//! Agent cleanups end to end through the real binary (P4-MCP-01, MCP-04, PRD §13.3):
//! `scull mcp serve --allow-destructive` over stdio, `scull approve`, and the Executor.
//!
//! * FX-MCP-07: plan tools only with `[mcp] destructive = true` **and** the launch flag.
//! * FX-MCP-08: `scull approve` without an interactive terminal is refused (exit 5) and
//!   approves nothing; with `--json` it is a usage error.
//! * FX-MCP-01 / -04 through the binary: `execute_cleanup` fails closed without an approval;
//!   after the human approves, it executes once through the Executor (S0 deleted, S1/S2
//!   quarantined inside the fixture's temp dir), the operation records channel `cli_tty`; a
//!   second call is refused.
//!
//! The human step is `scull approve`'s own code path in a terminal; a test has no terminal
//! (that is FX-MCP-08), so the test approves through the same broker calls
//! (`approval::begin` + `approval::approve` with the terminal channel) against the same
//! database, and drives the interactive prompt with scripted input (`cmd::approve::ask`).
//! Every run uses a `scull-fixtures` tree with `SCULL_HOME` and `HOME` beside it; afterwards
//! the invariants run (I-1, I-2, I-3, I-8; I-4 and I-5 after the execution).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{ItemId, OperationId, PlanId, Tier};
use scull_fixtures::{Fixture, InvariantInputs, NodeLock};
use scull_mcp::approval::{self, ConfirmationCode, HumanChannel, Principal};
use scull_storage::{ApprovalMethod, DefaultPermissions, Store};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, SystemTime};

struct Env {
    fx: Fixture,
}

impl Env {
    fn new(destructive: bool) -> Self {
        let mut fx = Fixture::new().unwrap();
        fx.cargo_project("app", "app")
            .unwrap()
            .cargo_target("app", "app", 64 * 1024)
            .unwrap()
            .npm_project("web", "web", NodeLock::Npm)
            .unwrap()
            .node_modules("web", &["left-pad"])
            .unwrap()
            .file("notes/todo.txt", "keep me")
            .unwrap();
        let old = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        for d in ["app", "web", "notes"] {
            fx.age(d, old).unwrap();
        }
        fx.checkpoint().unwrap();
        std::fs::create_dir_all(fx.base().join("userhome")).unwrap();
        let env = Self { fx };
        let dir = env.home().join("config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[scan]\nroots = [\"{}\"]\n\n[mcp]\nenabled = true\ndestructive = {destructive}\n",
                env.fx.root().to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        // The disclosure was shown before (keeps stderr short).
        std::fs::create_dir_all(env.home().join("state")).unwrap();
        std::fs::write(env.home().join("state").join("mcp-disclosure-shown"), "").unwrap();
        env
    }

    fn home(&self) -> PathBuf {
        self.fx.base().join("scull-home")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(args);
        let user_home = self.fx.base().join("userhome");
        c.current_dir(self.fx.base())
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("HOME", &user_home)
            .env("USERPROFILE", &user_home)
            .env("LOCALAPPDATA", user_home.join("AppData").join("Local"))
            .env("APPDATA", user_home.join("AppData").join("Roaming"))
            .env_remove("CARGO_TARGET_DIR");
        for (k, _) in std::env::vars() {
            if k.starts_with("SCULL_") {
                c.env_remove(k);
            }
        }
        c.env("SCULL_HOME", self.home());
        // P2-EXEC-13: quarantine (and the Trash fallback) stay inside the fixture's temp dir.
        self.fx.sandbox(&mut c);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    fn store(&self) -> Store {
        Store::open(
            &self.home().join("data").join(scull_storage::DB_FILE_NAME),
            &DefaultPermissions,
        )
        .unwrap()
    }

    fn who(&self) -> Principal {
        let machine_id = std::fs::read_to_string(
            self.home()
                .join("data")
                .join(scull_platform::MACHINE_ID_FILE),
        )
        .unwrap()
        .trim()
        .to_owned();
        Principal {
            machine_id,
            user: scull_platform::current_user().unwrap(),
        }
    }

    /// What `scull approve` records after the human typed the code shown.
    fn human_approves(&self, id: &PlanId) {
        let mut store = self.store();
        let who = self.who();
        let now = scull_engine::now_unix();
        let ch = approval::begin(&store, id, &who, now)
            .unwrap()
            .with_code(ConfirmationCode::parse("QRS-TUV").unwrap());
        approval::approve(
            &mut store,
            &ch,
            "qrs-tuv",
            HumanChannel::Terminal,
            &who,
            now,
        )
        .unwrap();
    }
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
        let mut s = Self {
            child,
            out,
            next_id: 1,
        };
        s.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "scull-cli-test", "version": "0" }
            }),
        );
        s.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        s
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

    fn tools(&mut self) -> Vec<String> {
        let list = self.request("tools/list", json!({}));
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// `tools/call`: (is_error, structuredContent).
    fn call(&mut self, name: &str, arguments: Value) -> (bool, Value) {
        let r = self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let res = &r["result"];
        assert!(res.is_object(), "{name}: {r}");
        (
            res["isError"].as_bool().unwrap_or(false),
            res["structuredContent"].clone(),
        )
    }

    fn finish(mut self) -> String {
        drop(self.child.stdin.take());
        self.child.wait().unwrap();
        let mut err = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        err
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// FX-MCP-07 through the binary: the flag without the config, or the config without the
/// flag, serves the read-only tools only.
#[test]
fn fx_mcp_07_binary_needs_both_flag_and_config() {
    let flag_only = Env::new(false);
    let mut s = Session::start(flag_only.cmd(&["mcp", "serve", "--allow-destructive"]));
    assert_eq!(s.tools(), scull_mcp::TOOL_NAMES);
    let err = s.finish();
    assert!(err.contains("both are required"), "{err}");
    flag_only
        .fx
        .check_invariants(&InvariantInputs::new())
        .assert_ok();

    let env = Env::new(true);
    let mut s = Session::start(env.cmd(&["mcp", "serve"]));
    assert_eq!(s.tools(), scull_mcp::TOOL_NAMES);
    s.finish();

    let mut s = Session::start(env.cmd(&["mcp", "serve", "--allow-destructive"]));
    let names = s.tools();
    for t in scull_mcp::PLAN_TOOL_NAMES {
        assert!(names.iter().any(|n| n == t), "{t} offered: {names:?}");
    }
    let err = s.finish();
    assert!(err.contains("scull approve"), "{err}");
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// The whole flow through the binary, with the adversarial steps an agent could try.
#[test]
fn agent_plan_runs_only_after_the_human_approves() {
    let env = Env::new(true);
    let mut s = Session::start(env.cmd(&["mcp", "serve", "--allow-destructive"]));
    let (_, cands) = s.call("get_cleanup_candidates", json!({}));
    let pick = cands["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| {
            c["path"]
                .as_str()
                .unwrap()
                .replace('\\', "/")
                .ends_with("app/target")
                && matches!(c["action"].as_str(), Some("delete" | "quarantine"))
        })
        .unwrap_or_else(|| panic!("app/target is cleanable: {cands:#}"))
        .clone();
    let (err, plan) = s.call("prepare_cleanup", json!({ "item_ids": [pick["id"]] }));
    assert!(!err, "{plan:#}");
    let id = PlanId::try_from(plan["plan_id"].as_str().unwrap().to_owned()).unwrap();
    let args = json!({ "plan_id": id.as_str() });

    // FX-MCP-01: no approval, no execution.
    let (err, r) = s.call("execute_cleanup", args.clone());
    assert!(err);
    assert_eq!(r["error"]["code"], "not_approved", "{r:#}");
    assert!(env.fx.root().join("app/target").exists());

    // FX-MCP-08: the agent's shell can't approve: no terminal.
    let o = env.run(&["approve", id.as_str()]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("interactive terminal"),
        "{}",
        stderr(&o)
    );
    assert!(o.stdout.is_empty());
    let o = env.run(&["approve", id.as_str(), "--json"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    let o = env.run(&["approve"]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    // Piping a "yes" or the right answer does not make a terminal either.
    let mut c = env.cmd(&["approve", id.as_str()]);
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"yes\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert!(
        env.store().get_approval(&id).unwrap().is_none(),
        "nothing approved"
    );
    let (_, st) = s.call("get_approval_status", args.clone());
    assert_eq!(st["state"], "awaiting_approval");

    // The human approves in their terminal.
    env.human_approves(&id);
    let (err, run) = s.call("execute_cleanup", args.clone());
    assert!(!err, "{run:#}");
    assert_eq!(run["approved_via"], "terminal");
    let item = &run["items"][0];
    assert!(
        matches!(item["outcome"].as_str(), Some("deleted" | "quarantined")),
        "{run:#}"
    );
    assert!(!env.fx.root().join("app/target").exists());

    // FX-MCP-04: once.
    let (err, r) = s.call("execute_cleanup", args.clone());
    assert!(err);
    assert_eq!(r["error"]["code"], "approval_used", "{r:#}");
    let (_, st) = s.call("get_cleanup_status", args);
    assert_eq!(st["state"], "executed", "{st:#}");
    s.finish();

    // The operation is recorded with the approval's channel and approver.
    let store = env.store();
    let op = OperationId::try_from(run["operation_id"].as_str().unwrap().to_owned()).unwrap();
    let rec = store.operation(&op).unwrap().unwrap();
    assert_eq!(rec.method, ApprovalMethod::CliTty);
    assert_eq!(rec.approver, env.who().user);
    assert_eq!(store.operations(0).unwrap().len(), 1);

    // Invariants: only app/target changed; nothing S3; audit matches disk; bytes recorded.
    let item_id: ItemId = serde_json::from_value(pick["id"].clone()).unwrap();
    let tier: Tier = serde_json::from_value(pick["tier"].clone()).unwrap();
    let action = match item["outcome"].as_str().unwrap() {
        "deleted" => scull_core::Action::Delete,
        _ => scull_core::Action::Quarantine,
    };
    let inputs = InvariantInputs::new()
        .allow("app/target")
        .audit(&store, &op)
        .operation_bytes(&store, &op)
        .plan(vec![(item_id.clone(), tier, action)])
        .tiers(
            BTreeMap::from([(item_id.clone(), tier)]),
            BTreeMap::from([(item_id, tier)]),
        );
    let report = env.fx.check_invariants(&inputs);
    println!("{report}");
    report.assert_ok();
}

/// `scull approve`'s prompt (what runs in a real terminal), with scripted answers: a wrong
/// code re-prompts, `deny` and end of input approve nothing, three wrong codes give up, the
/// shown code approves; the full plan with its hash is shown first.
#[test]
fn approve_prompt_shows_the_plan_and_needs_the_shown_code() {
    let env = Env::new(true);
    let mut s = Session::start(env.cmd(&["mcp", "serve", "--allow-destructive"]));
    let (_, cands) = s.call("get_cleanup_candidates", json!({}));
    let ids: Vec<Value> = cands["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| matches!(c["action"].as_str(), Some("delete" | "quarantine")))
        .map(|c| c["id"].clone())
        .collect();
    assert!(!ids.is_empty(), "{cands:#}");
    let (err, plan) = s.call("prepare_cleanup", json!({ "item_ids": ids }));
    assert!(!err, "{plan:#}");
    s.finish();
    let id = PlanId::try_from(plan["plan_id"].as_str().unwrap().to_owned()).unwrap();
    let who = env.who();
    let mut store = env.store();
    let answer = |store: &mut Store, input: &str| {
        let ch = approval::begin(store, &id, &who, scull_engine::now_unix())
            .unwrap()
            .with_code(ConfirmationCode::parse("WXY-Z23").unwrap());
        let mut out = Vec::new();
        let r = scull_cli::cmd::approve::ask(
            store,
            &ch,
            &who,
            &mut std::io::Cursor::new(input.as_bytes().to_vec()),
            &mut out,
            false,
        );
        (r, String::from_utf8(out).unwrap())
    };
    for denial in ["deny\n", "DENY\n", "\n", ""] {
        let (r, out) = answer(&mut store, denial);
        assert_eq!(
            r.unwrap(),
            scull_cli::cmd::approve::Answer::Denied,
            "{denial:?}"
        );
        assert!(out.contains("Not approved"), "{out}");
    }
    let (r, out) = answer(&mut store, "AAAAAA\nBBBBBB\nyes\n");
    assert_eq!(r.unwrap_err().kind, scull_core::ErrorKind::SafetyBlocked);
    assert_eq!(out.matches("Type WXY-Z23 to approve").count(), 3, "{out}");
    assert!(store.get_approval(&id).unwrap().is_none());
    let (r, out) = answer(&mut store, "nope\nwxy z23\n");
    assert!(
        matches!(r.unwrap(), scull_cli::cmd::approve::Answer::Approved(_)),
        "{out}"
    );
    assert!(out.contains("An AI agent asks to clean up"), "{out}");
    assert!(
        out.contains(&plan["content_hash"].as_str().unwrap()[..12]),
        "{out}"
    );
    assert!(out.contains("app") || out.contains("web"), "{out}");
    assert!(out.contains("That is not the code"), "{out}");
    assert!(out.contains("Approved plan"), "{out}");
    let a = store.get_approval(&id).unwrap().unwrap();
    assert_eq!(a.method, ApprovalMethod::CliTty);
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
