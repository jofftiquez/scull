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

/// `kind/source/reason xcount` of every agent audit event, oldest first.
fn reasons(env: &Env) -> Vec<String> {
    env.store()
        .agent_events(0, 10_000)
        .unwrap()
        .into_iter()
        .map(|e| format!("{}/{}/{} x{}", e.kind.as_str(), e.source, e.reason, e.count))
        .collect()
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
    if elevated_here() {
        // SAFE-14: an elevated server never offers destructive tools, flag and config or not.
        assert_eq!(names, scull_mcp::TOOL_NAMES, "read-only when elevated");
        let err = s.finish();
        assert!(err.contains("root/administrator"), "{err}");
    } else {
        for t in scull_mcp::PLAN_TOOL_NAMES {
            assert!(names.iter().any(|n| n == t), "{t} offered: {names:?}");
        }
        let err = s.finish();
        assert!(err.contains("scull approve"), "{err}");
    }
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Whether this test process runs elevated (GitHub's Windows runners do): scull's MCP server
/// then never offers destructive tools (SAFE-14), so the approval flow can't be exercised.
fn elevated_here() -> bool {
    scull_platform::is_elevated().blocks_destructive()
}

/// The approval-flow tests need the plan tools; elevated, the server stays read-only (checked
/// in `fx_mcp_07_binary_needs_both_flag_and_config`), so they only say so and stop.
fn skip_if_elevated(test: &str) -> bool {
    if elevated_here() {
        eprintln!("SKIP {test}: running elevated, so scull mcp serves read-only tools (SAFE-14)");
        return true;
    }
    false
}

/// The whole flow through the binary, with the adversarial steps an agent could try.
#[test]
fn agent_plan_runs_only_after_the_human_approves() {
    if skip_if_elevated("agent_plan_runs_only_after_the_human_approves") {
        return;
    }
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
    // Audited (PRD §13.3 step 5): the refused execution, and both attempts without a
    // terminal that named the plan (the usage error and the bare listing are not attempts).
    assert_eq!(
        reasons(&env),
        [
            "execute_refused/mcp/not_approved x1",
            "approval_denied/cli/no_terminal x2",
        ]
    );

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

    // `scull history` lists what was refused to the agent; `scull data show` counts it.
    let o = env.run(&["history", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let h: Value = serde_json::from_slice(&o.stdout).unwrap();
    let listed: Vec<(&str, &str, u64)> = h["data"]["agent_events"]
        .as_array()
        .unwrap_or_else(|| panic!("{h:#}"))
        .iter()
        .map(|e| {
            (
                e["kind"].as_str().unwrap(),
                e["reason"].as_str().unwrap(),
                e["count"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("execute_refused", "approval_used", 1),
            ("approval_denied", "no_terminal", 2),
            ("execute_refused", "not_approved", 1),
        ],
        "newest first"
    );
    assert!(
        h["data"]["agent_events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["plan_id"] == id.as_str()),
        "{h:#}"
    );
    let o = env.run(&["history"]);
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(text.contains("Refused to AI agents (3"), "{text}");
    assert!(
        text.contains("no_terminal") && text.contains("not_approved"),
        "{text}"
    );
    let o = env.run(&["data", "show", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let d: Value = serde_json::from_slice(&o.stdout).unwrap();
    let rows = |k: &str| {
        d["data"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == k)
            .map_or(0, |e| e["rows"].as_u64().unwrap())
    };
    assert_eq!(
        (rows("approval_denied"), rows("execute_refused")),
        (1, 2),
        "{d:#}"
    );

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
    if skip_if_elevated("approve_prompt_shows_the_plan_and_needs_the_shown_code") {
        return;
    }
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
    // Four denials (folded into one entry counting 4) and one give-up are audited, with the
    // plan's hash and never the code.
    let events = store.agent_events(0, 10_000).unwrap();
    let got: Vec<(&str, u64)> = events
        .iter()
        .map(|e| (e.reason.as_str(), e.count))
        .collect();
    assert_eq!(got, [("denied", 4), ("wrong_code", 1)]);
    for e in &events {
        assert_eq!(e.kind.as_str(), "approval_denied");
        assert_eq!((e.source.as_str(), e.channel.as_str()), ("cli", "terminal"));
        assert_eq!(e.plan_id.as_ref(), Some(&id));
        assert_eq!(
            e.content_hash.as_ref().map(|h| h.as_str()),
            plan["content_hash"].as_str()
        );
        assert_eq!(e.user, who.user);
    }
    assert!(
        !format!("{events:?}").contains("WXY"),
        "no code in the audit"
    );
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

/// `scull approve` in a **real** pseudo-terminal (tmux; skipped where tmux is not installed):
/// the full plan and the code are shown, a wrong code re-prompts, the shown code approves,
/// and the approval is recorded with channel `cli_tty`. (This is also how a PTY-driving
/// process could approve: T-41's documented residual, #302.)
#[cfg(unix)]
#[test]
fn approve_in_a_real_pseudo_terminal() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
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
    let (err, plan) = s.call("prepare_cleanup", json!({ "item_ids": ids }));
    assert!(!err, "{plan:#}");
    s.finish();
    let id = plan["plan_id"].as_str().unwrap().to_owned();

    // The same environment as `Env::cmd`, spelled out for the shell tmux starts.
    let q = |p: &std::path::Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"));
    let user_home = env.fx.base().join("userhome");
    // Without the agent markers of whatever runs the tests (#35 would rightly refuse them).
    let mut vars = format!(
        "env -u SCULL_TEST_ASSUME_ELEVATED {} SCULL_HOME={} HOME={} TERM=xterm NO_COLOR=1",
        unset_agent_vars(),
        q(&env.home()),
        q(&user_home)
    );
    for (k, v) in env.fx.sandbox_env() {
        vars.push_str(&format!(" {k}={}", q(v)));
    }
    let script = format!(
        "{vars} {} approve {id}; echo SCULL-EXIT=$?; sleep 60",
        q(std::path::Path::new(env!("CARGO_BIN_EXE_scull")))
    );
    let socket = env.fx.base().join("tmux.sock");
    let tmux = |args: &[&str]| {
        Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .args(args)
            .output()
            .unwrap()
    };
    let o = tmux(&[
        "new-session",
        "-d",
        "-s",
        "approve",
        "-x",
        "200",
        "-y",
        "60",
        &script,
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let pane = || {
        String::from_utf8_lossy(&tmux(&["capture-pane", "-p", "-t", "approve"]).stdout).into_owned()
    };
    let wait_for = |needle: &str| -> String {
        for _ in 0..200 {
            let p = pane();
            if p.contains(needle) {
                return p;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("{needle:?} never appeared:\n{}", pane());
    };
    let shown = wait_for(" to approve, or `deny`");
    let code = shown
        .split("Type ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .to_owned();
    assert_eq!(code.len(), 7, "{shown}");
    assert!(shown.contains("An AI agent asks to clean up"), "{shown}");
    assert!(shown.contains(&id), "{shown}");
    tmux(&["send-keys", "-t", "approve", "ZZZ-ZZZ", "Enter"]);
    wait_for("That is not the code");
    tmux(&["send-keys", "-t", "approve", &code.to_lowercase(), "Enter"]);
    let done = wait_for("SCULL-EXIT=");
    tmux(&["kill-server"]);
    assert!(done.contains("SCULL-EXIT=0"), "{done}");
    assert!(done.contains("Approved plan"), "{done}");
    let a = env
        .store()
        .get_approval(&PlanId::try_from(id).unwrap())
        .unwrap()
        .expect("approved");
    assert_eq!(a.method, ApprovalMethod::CliTty);
    assert_eq!(a.approver, env.who().user);
    let _ = std::fs::remove_file(&socket);
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// A refused `scull approve` without a terminal leaves no trace beyond the audit row: it
/// never creates scull's machine ID file (nor a database). With no machine ID, nothing is
/// recorded at all.
#[test]
fn approve_without_a_terminal_creates_no_machine_id() {
    if skip_if_elevated("approve_without_a_terminal_creates_no_machine_id") {
        return;
    }
    let env = Env::new(true);
    let mut s = Session::start(env.cmd(&["mcp", "serve", "--allow-destructive"]));
    let (_, cands) = s.call("get_cleanup_candidates", json!({}));
    let pick = cands["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| matches!(c["action"].as_str(), Some("delete" | "quarantine")))
        .unwrap()
        .clone();
    let (err, plan) = s.call("prepare_cleanup", json!({ "item_ids": [pick["id"]] }));
    assert!(!err, "{plan:#}");
    s.finish();
    let id = plan["plan_id"].as_str().unwrap().to_owned();
    let machine = env
        .home()
        .join("data")
        .join(scull_platform::MACHINE_ID_FILE);
    std::fs::remove_file(&machine).unwrap();
    let o = env.run(&["approve", &id]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert!(!machine.exists(), "the machine ID file was created");
    assert!(env.store().agent_events(0, 100).unwrap().is_empty());
    // Without a database either, nothing is created.
    let fresh = Env::new(true);
    let o = fresh.run(&["approve", &id]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert!(
        !fresh
            .home()
            .join("data")
            .join(scull_platform::MACHINE_ID_FILE)
            .exists()
    );
    assert!(
        !fresh
            .home()
            .join("data")
            .join(scull_storage::DB_FILE_NAME)
            .exists()
    );
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
    fresh
        .fx
        .check_invariants(&InvariantInputs::new())
        .assert_ok();
}

/// `scull history` stays bounded however many agent refusals were recorded: `--limit`
/// (default 50) entries, newest first, plus how many more there are.
#[test]
fn history_lists_a_bounded_number_of_agent_events() {
    let env = Env::new(true);
    let o = env.run(&["scan", "--json"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let mut store = env.store();
    let now = scull_engine::now_unix();
    for n in 0..120 {
        store
            .record_agent_event(&scull_storage::AgentAuditEvent {
                event_id: 0,
                at: now - 120 + n,
                kind: scull_storage::EventKind::ExecuteRefused,
                source: "mcp".into(),
                plan_id: None,
                content_hash: None,
                reason: format!("reason_{n}"),
                channel: "mcp".into(),
                machine_id: "m".into(),
                user: "u".into(),
                count: 1,
            })
            .unwrap();
    }
    let json = |args: &[&str]| -> Value {
        let o = env.run(args);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        serde_json::from_slice(&o.stdout).unwrap()
    };
    let h = json(&["history", "--json"]);
    assert_eq!(h["data"]["agent_events"].as_array().unwrap().len(), 50);
    assert_eq!(h["data"]["agent_events_more"], 70);
    assert_eq!(
        h["data"]["agent_events"][0]["reason"], "reason_119",
        "newest first"
    );
    let h = json(&["history", "--json", "--limit", "5"]);
    assert_eq!(h["data"]["agent_events"].as_array().unwrap().len(), 5);
    assert_eq!(h["data"]["agent_events_more"], 115);
    let o = env.run(&["history", "--limit", "3"]);
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(text.contains("and 117 more"), "{text}");
    assert_eq!(text.matches("reason_").count(), 3, "{text}");
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// #298 through the binary: `scull approve --revoke` refuses without an interactive terminal
/// (exit 5, nothing revoked) and with `--json` (exit 2); `--revoke` and a plan ID together are
/// a usage error. The terminal path (`cmd::approve::revoke`) withdraws the approval: the
/// agent's `execute_cleanup` is refused through the Executor-backed server, nothing on disk
/// changes, and `scull history` shows the revocation and the refusal.
#[test]
fn revoke_through_the_cli_stops_the_agent() {
    if skip_if_elevated("revoke_through_the_cli_stops_the_agent") {
        return;
    }
    let env = Env::new(true);
    let mut s = Session::start(env.cmd(&["mcp", "serve", "--allow-destructive"]));
    let (_, cands) = s.call("get_cleanup_candidates", json!({}));
    let pick = cands["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| matches!(c["action"].as_str(), Some("delete" | "quarantine")))
        .unwrap_or_else(|| panic!("something is cleanable: {cands:#}"))
        .clone();
    let (err, plan) = s.call("prepare_cleanup", json!({ "item_ids": [pick["id"]] }));
    assert!(!err, "{plan:#}");
    let id = PlanId::try_from(plan["plan_id"].as_str().unwrap().to_owned()).unwrap();
    let args = json!({ "plan_id": id.as_str() });
    env.human_approves(&id);

    let o = env.run(&["approve", "--revoke", id.as_str()]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("interactive terminal"),
        "{}",
        stderr(&o)
    );
    let o = env.run(&["approve", "--revoke", id.as_str(), "--json"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    let o = env.run(&["approve", id.as_str(), "--revoke", id.as_str()]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert_eq!(
        env.store().get_approval(&id).unwrap().unwrap().used_at,
        None,
        "nothing revoked"
    );

    let text = scull_cli::cmd::approve::revoke(
        &mut env.store(),
        &id,
        &env.who(),
        scull_engine::now_unix(),
    )
    .unwrap();
    assert!(text.contains("Revoked the approval"), "{text}");
    let e = scull_cli::cmd::approve::revoke(
        &mut env.store(),
        &id,
        &env.who(),
        scull_engine::now_unix(),
    )
    .unwrap_err();
    assert_eq!(
        e.kind,
        scull_core::ErrorKind::SafetyBlocked,
        "{}",
        e.message
    );

    let (err, r) = s.call("execute_cleanup", args.clone());
    assert!(err);
    assert_eq!(r["error"]["code"], "approval_revoked", "{r:#}");
    let (_, st) = s.call("get_cleanup_status", args);
    assert_eq!(st["state"], "revoked", "{st:#}");
    s.finish();
    assert!(env.store().operations(0).unwrap().is_empty(), "nothing ran");

    let o = env.run(&["history", "--json"]);
    let h: Value = serde_json::from_slice(&o.stdout).unwrap();
    let listed: Vec<(&str, &str)> = h["data"]["agent_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["kind"].as_str().unwrap(), e["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(
        listed,
        [
            ("execute_refused", "approval_revoked"),
            ("revoke_refused", "approval_revoked"),
            ("approval_revoked", "revoked"),
        ]
    );
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

// ---------------------------------------------------------------------------------------------
// #35: agents driving a pseudo-terminal (FX-MCP-11, -12, -13)
// ---------------------------------------------------------------------------------------------

/// `env -u` arguments removing every agent marker (`approval::AGENT_ENV_VARS`).
#[cfg(unix)]
fn unset_agent_vars() -> String {
    approval::AGENT_ENV_VARS
        .iter()
        .map(|v| format!("-u {v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(unix)]
fn shq(p: &std::path::Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}

/// A prepared agent plan and the shell command that runs `scull approve <plan>` with the
/// test's environment (scull home, sandbox) and `extra` variables, without agent markers.
#[cfg(unix)]
fn pty_case(extra: &str) -> (Env, PlanId, String) {
    let env = Env::new(true);
    let mut s = Session::start(env.cmd(&["mcp", "serve", "--allow-destructive"]));
    let (_, cands) = s.call("get_cleanup_candidates", json!({}));
    let pick = cands["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| matches!(c["action"].as_str(), Some("delete" | "quarantine")))
        .unwrap()
        .clone();
    let (err, plan) = s.call("prepare_cleanup", json!({ "item_ids": [pick["id"]] }));
    assert!(!err, "{plan:#}");
    s.finish();
    let id = PlanId::try_from(plan["plan_id"].as_str().unwrap().to_owned()).unwrap();
    let user_home = env.fx.base().join("userhome");
    let mut vars = format!(
        "env -u SCULL_TEST_ASSUME_ELEVATED {} SCULL_HOME={} HOME={} TERM=xterm NO_COLOR=1 {extra}",
        unset_agent_vars(),
        shq(&env.home()),
        shq(&user_home)
    );
    for (k, v) in env.fx.sandbox_env() {
        vars.push_str(&format!(" {k}={}", shq(v)));
    }
    let cmd = format!(
        "{vars} {} approve {}",
        shq(std::path::Path::new(env!("CARGO_BIN_EXE_scull"))),
        id.as_str()
    );
    (env, id, cmd)
}

/// `script` running `sh -c <cmd>` in a fresh pseudo-terminal (util-linux or BSD syntax), or
/// `None` where there is no `script`.
#[cfg(unix)]
fn under_script(program: &std::path::Path, cmd: &str) -> Option<Command> {
    if Command::new("script").arg("--version").output().is_err()
        && !std::path::Path::new("/usr/bin/script").exists()
    {
        return None;
    }
    let mut c = Command::new(program);
    // `; exit $?` keeps `program` alive as script's parent: a shell given a single command
    // with `-c` may exec it in place (bash does), and `program` would leave the parent chain.
    if cfg!(target_os = "macos") {
        c.args([
            "-c",
            &format!("script -q /dev/null sh -c {}; exit $?", shell_quote(cmd)),
        ]);
    } else {
        c.args([
            "-c",
            &format!("script -q -e -c {} /dev/null; exit $?", shell_quote(cmd)),
        ]);
    }
    Some(c)
}

#[cfg(unix)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a PTY-driven approval with "yes" and a guess typed in, and check it was refused with
/// `code` before any code was shown: nothing approved, the refusal audited.
#[cfg(unix)]
fn assert_refused(env: &Env, id: &PlanId, mut c: Command, code: &str) {
    let mut child = c
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let _ = child.stdin.take().unwrap().write_all(b"yes\nABCDEF\n");
    let o = child.wait_with_output().unwrap();
    let all = format!("{}{}", String::from_utf8_lossy(&o.stdout), stderr(&o));
    assert!(all.contains(code), "expected {code}:\n{all}");
    assert!(all.contains("Nothing was approved"), "{all}");
    assert!(
        !all.contains("to approve, or `deny`"),
        "no code is ever shown:\n{all}"
    );
    assert!(
        env.store().get_approval(id).unwrap().is_none(),
        "nothing approved"
    );
    let audited: Vec<String> = env
        .store()
        .agent_events(0, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.as_str() == "approval_denied")
        .map(|e| e.reason)
        .collect();
    assert_eq!(audited, [code], "audited");
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Whether the process running these tests was itself started by an AI agent (a developer's
/// agent running `cargo test`): then every approval below is, rightly, refused as
/// `agent_process` first.
#[cfg(unix)]
fn tests_run_under_an_agent() -> bool {
    let names: Vec<String> = scull_platform::ancestry()
        .unwrap()
        .iter()
        .flat_map(|p| p.names())
        .collect();
    approval::check_approver(Ok(&names), &|_| false)
        .is_err_and(|s| s.kind == approval::ApproverSignalKind::AgentProcess)
}

/// FX-MCP-11: `scull approve` under `script` (a real pseudo-terminal: stdin, stdout and
/// stderr are terminals, so FX-MCP-08 passes) is refused as `pty_driver` before the plan or
/// a code is shown (as `agent_process` when the tests themselves run under an agent).
/// Skipped where `script` is missing.
#[cfg(unix)]
#[test]
fn fx_mcp_11_approve_under_a_pty_driver_is_refused() {
    let (env, id, cmd) = pty_case("");
    let Some(c) = under_script(std::path::Path::new("/bin/sh"), &cmd) else {
        eprintln!("skipped: no script(1)");
        env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    };
    let want = if tests_run_under_an_agent() {
        "agent_process"
    } else {
        "pty_driver"
    };
    assert_refused(&env, &id, c, want);
}

/// FX-MCP-12: an agent's environment marker (here Claude Code's `CLAUDECODE=1`) refuses the
/// approval even in a clean pseudo-terminal: a `tmux` session the agent started inherits it.
/// Skipped where `tmux` is missing.
#[cfg(unix)]
#[test]
fn fx_mcp_12_agent_environment_in_a_tmux_session_is_refused() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipped: tmux is not installed");
        return;
    }
    let (env, id, cmd) = pty_case("CLAUDECODE=1");
    let socket = env.fx.base().join("t.sock");
    let tmux = |args: &[&str]| {
        Command::new("tmux")
            .current_dir(env.fx.base())
            .arg("-S")
            .arg("t.sock")
            .args(args)
            .output()
            .unwrap()
    };
    let o = tmux(&[
        "new-session",
        "-d",
        "-s",
        "agent",
        "-x",
        "220",
        "-y",
        "50",
        &format!("{cmd}; echo SCULL-EXIT=$?; sleep 30"),
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let mut pane = String::new();
    for _ in 0..200 {
        pane = String::from_utf8_lossy(&tmux(&["capture-pane", "-p", "-J", "-t", "agent"]).stdout)
            .into_owned();
        if pane.contains("SCULL-EXIT=") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    tmux(&["kill-server"]);
    let _ = std::fs::remove_file(&socket);
    assert!(pane.contains("SCULL-EXIT=5"), "{pane}");
    assert!(pane.contains("agent_environment"), "{pane}");
    assert!(!pane.contains("to approve, or `deny`"), "{pane}");
    assert!(env.store().get_approval(&id).unwrap().is_none());
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// FX-MCP-13: an AI agent client among the parent processes (a stand-in program named
/// `claude`, a copy of `/bin/sh`, running the approval under `script`) is refused as
/// `agent_process`, reported before the PTY driver.
#[cfg(unix)]
#[test]
fn fx_mcp_13_approve_started_by_an_agent_process_is_refused() {
    let (env, id, cmd) = pty_case("");
    let fake = env.fx.base().join("bin");
    std::fs::create_dir_all(&fake).unwrap();
    let claude = fake.join("claude");
    // A shell that stays the process it was started as: macOS's `/bin/sh` is a launcher that
    // re-executes the real shell, so a copy named `claude` would vanish from the parent chain.
    let shell = ["/bin/bash", "/bin/sh"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .unwrap();
    std::fs::copy(shell, &claude).unwrap();
    let Some(c) = under_script(&claude, &cmd) else {
        eprintln!("skipped: no script(1)");
        env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    };
    assert_refused(&env, &id, c, "agent_process");
}
