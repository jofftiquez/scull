//! The desktop approval dialog's backend (P4-MCP-02): agent plans are listed, opened with a
//! code kept in memory, approved only with that code through the broker's desktop channel,
//! denied or revoked, and their execution is reported from the store. Everything runs against
//! a temp data directory; nothing outside it is read or written, and nothing is executed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{
    Action, Candidate, Category, ErrorKind, FileIdentity, Flags, FsPath, ItemId, OperationId, Plan,
    PlanId, PlanItem, PlanOrigin, PlanParams, ReasonCode, Regeneration, RuleRef, ScanId, Sizes,
    Tier, TierReason,
};
use scull_desktop_bridge::approvals::{AgentItemOutcome, AgentPlanState};
use scull_desktop_bridge::{ApprovalDesk, ScanReport};
use scull_mcp::approval::{self, MAX_CODE_ATTEMPTS, Principal};
use scull_storage::{
    ApprovalMethod, DefaultPermissions, EventKind, ItemOutcome as StoredOutcome, ItemRecord,
    OperationEnd, OperationFinish, OperationStart, Store,
};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

const MACHINE: &str = "machine-a";
const USER: &str = "dev";
/// "Now" for every test: plans are made at `NOW - 60` and live 15 minutes.
const NOW: i64 = 1_900_000_000;

fn who() -> Principal {
    Principal {
        machine_id: MACHINE.into(),
        user: USER.into(),
    }
}

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Every answer validates against its committed schema.
fn validate<T: Serialize>(stem: &str, v: &T) -> Value {
    let v = serde_json::to_value(v).unwrap();
    let path = desktop().join(format!("schemas/{stem}.schema.json"));
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{stem}: {errors:?}\n{v:#}");
    v
}

fn candidate(n: u128, tier: Tier, action: Action, path: &str) -> Candidate {
    let identity = FileIdentity::new(7, n);
    Candidate {
        id: ItemId::derive(identity, path.as_bytes()),
        path: FsPath::from(path),
        identity,
        category: Category::ProjectArtifact,
        artifact: "Cargo build output".into(),
        rule: RuleRef {
            id: "rust.target".into(),
            version: 1,
        },
        project: None,
        tier,
        reasons: vec![TierReason::new(tier, ReasonCode::Regenerable, "rebuilds")],
        evidence: vec![],
        sizes: Sizes {
            logical: 1000,
            allocated: 1024,
            reclaimable: scull_core::Reclaimable {
                bytes: 900 + n as u64,
                confidence: scull_core::Confidence::High,
                shared_bytes: 0,
            },
        },
        flags: Flags {
            active: Some(false),
            ..Flags::default()
        },
        regeneration: Regeneration::default(),
        action,
    }
}

fn cands() -> Vec<Candidate> {
    vec![
        candidate(1, Tier::S0, Action::Delete, "/home/dev/code/app/target"),
        candidate(
            2,
            Tier::S1,
            Action::Quarantine,
            "/home/dev/code/web/node_modules",
        ),
    ]
}

fn plan(suffix: &str, origin: PlanOrigin, user: &str, at: i64, cands: &[Candidate]) -> Plan {
    Plan::new(
        PlanParams {
            id: PlanId::from_suffix(suffix).unwrap(),
            scan_id: ScanId::from_suffix("S1").unwrap(),
            created_unix: at,
            ttl_secs: None,
            machine_id: MACHINE.into(),
            user: user.into(),
            origin,
            operations_watermark: None,
        },
        cands
            .iter()
            .map(|c| PlanItem::from_candidate(c).unwrap())
            .collect(),
    )
    .unwrap()
}

struct Env {
    dir: tempfile::TempDir,
    desk: ApprovalDesk,
}

impl Env {
    /// A data directory with scull's database holding `plans`.
    fn with(plans: &[Plan]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut store = scull_engine::open_history(dir.path()).unwrap();
        for p in plans {
            store.insert_plan(p).unwrap();
        }
        drop(store);
        let desk = ApprovalDesk::with_principal(Some(dir.path().to_path_buf()), who());
        Self { dir, desk }
    }

    fn store(&self) -> Store {
        scull_engine::open_history(self.dir.path()).unwrap()
    }

    /// The bytes of every file of the data directory (database, WAL, ...).
    fn bytes(&self) -> Vec<u8> {
        let mut out = vec![];
        for e in std::fs::read_dir(self.dir.path()).unwrap() {
            let p = e.unwrap().path();
            if p.is_file() {
                out.extend(std::fs::read(p).unwrap());
            }
        }
        out
    }
}

fn mcp_plan() -> Plan {
    plan("01JAGENT1", PlanOrigin::Mcp, USER, NOW - 60, &cands())
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn without_a_database_there_are_no_requests_and_nothing_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let desk = ApprovalDesk::with_principal(Some(dir.path().to_path_buf()), who());
    let v = desk.awaiting(NOW).unwrap();
    assert!(v.requests.is_empty());
    validate("agent-requests", &v);
    assert_eq!(
        desk.open("plan_01JAGENT1", None, NOW).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        desk.deny("plan_01JAGENT1", NOW).unwrap_err().kind,
        ErrorKind::NotFound
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "nothing created"
    );
    // No data directory at all.
    let none = ApprovalDesk::with_principal(None, who());
    assert!(none.awaiting(NOW).unwrap().requests.is_empty());
}

#[test]
fn only_this_users_unexpired_agent_plans_are_listed() {
    let own = mcp_plan();
    let cli = plan("01JCLI1", PlanOrigin::Cli, USER, NOW - 30, &cands());
    let other_user = plan("01JOTHER1", PlanOrigin::Mcp, "someone", NOW - 30, &cands());
    let expired = plan("01JOLD1", PlanOrigin::Mcp, USER, NOW - 3600, &cands());
    let env = Env::with(&[own.clone(), cli.clone(), other_user.clone(), expired]);
    let v = env.desk.awaiting(NOW).unwrap();
    let json = validate("agent-requests", &v);
    assert_eq!(v.requests.len(), 1, "{json:#}");
    let r = &v.requests[0];
    assert_eq!(r.plan_id, own.id().as_str());
    assert_eq!(r.content_hash, own.content_hash().as_str());
    assert_eq!(r.item_count, 2);
    assert_eq!(r.expires_at, own.expires_unix());
    // A CLI plan or another user's can't be opened here.
    for p in [&cli, &other_user] {
        assert!(env.desk.open(p.id().as_str(), None, NOW).is_err());
    }
}

#[test]
fn approving_needs_the_dialog_the_shown_hash_and_the_code() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    let hash = p.content_hash().as_str();

    // Never opened in this app: refused, nothing recorded.
    let e = env.desk.approve(id, hash, "ABCDEF", NOW).unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked);
    assert!(env.store().get_approval(p.id()).unwrap().is_none());

    let req = env.desk.open(id, None, NOW).unwrap();
    let json = validate("agent-approval-request", &req);
    assert!(env.desk.is_open(id));
    assert_eq!(req.items.len(), 2);
    assert_eq!(req.attempts_left, MAX_CODE_ATTEMPTS);
    assert_eq!(req.code.len(), 7, "ABC-DEF: {json}");
    assert_eq!(&req.code[3..4], "-");

    // Another hash than the one shown: refused.
    let other = "0".repeat(64);
    let e = env.desk.approve(id, &other, &req.code, NOW).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanTampered);
    assert!(env.store().get_approval(p.id()).unwrap().is_none());

    // A wrong code: refused with the tries left; the dialog stays open.
    let wrong = if req.code == "AAA-AAA" {
        "BBBBBB"
    } else {
        "AAAAAA"
    };
    let e = env.desk.approve(id, hash, wrong, NOW).unwrap_err();
    assert!(e.message.contains("2 tries left"), "{}", e.message);
    assert!(
        !e.message.contains(&req.code),
        "the code is never in an error"
    );
    assert!(env.desk.is_open(id));

    // The right code, typed loosely (case, spaces and dashes don't matter).
    let typed = req.code.to_lowercase().replace('-', " ");
    let a = env.desk.approve(id, hash, &typed, NOW + 5).unwrap();
    validate("agent-approved", &a);
    assert_eq!(a.approved_at, NOW + 5);
    assert_eq!(a.approval_expires_at, p.expires_unix());
    assert!(!env.desk.is_open(id), "single use: the dialog is closed");
    let stored = env.store().get_approval(p.id()).unwrap().unwrap();
    assert_eq!(stored.method, ApprovalMethod::Desktop);
    assert_eq!(stored.approver, USER);
    assert_eq!(stored.content_hash, *p.content_hash());

    let s = env.desk.status(id, NOW + 6).unwrap();
    validate("agent-plan-status", &s);
    assert_eq!(s.state, AgentPlanState::Approved);
    assert_eq!(s.channel.as_deref(), Some("desktop"));

    // Approving again, or opening again, is refused.
    assert!(env.desk.approve(id, hash, &req.code, NOW + 7).is_err());
    assert!(env.desk.open(id, None, NOW + 7).is_err());

    // The code was never written anywhere.
    let bytes = env.bytes();
    assert!(!contains(&bytes, &req.code));
    assert!(!contains(&bytes, &req.code.replace('-', "")));
}

#[test]
fn three_wrong_codes_close_the_dialog_and_are_audited() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    let hash = p.content_hash().as_str();
    let first = env.desk.open(id, None, NOW).unwrap();
    let wrong = if first.code == "AAA-AAA" {
        "BBBBBB"
    } else {
        "AAAAAA"
    };
    for i in 1..MAX_CODE_ATTEMPTS {
        let e = env
            .desk
            .approve(id, hash, wrong, NOW + i64::from(i))
            .unwrap_err();
        assert!(e.message.contains("left"), "{}", e.message);
    }
    let e = env.desk.approve(id, hash, wrong, NOW + 10).unwrap_err();
    assert!(e.message.contains("request is closed"), "{}", e.message);
    assert!(!env.desk.is_open(id));
    // Even the right code no longer works for that opening.
    assert!(env.desk.approve(id, hash, &first.code, NOW + 11).is_err());
    assert!(env.store().get_approval(p.id()).unwrap().is_none());
    let events = env.store().agent_events(0, 100).unwrap();
    assert!(
        events.iter().any(|e| e.kind == EventKind::ApprovalDenied
            && e.reason == "wrong_code"
            && e.channel == "desktop"),
        "{events:?}"
    );
    // Three wrong codes close the request like a denial (the broker no longer lists it), for
    // this desk and a fresh one.
    let fresh = ApprovalDesk::with_principal(Some(env.dir.path().to_path_buf()), who());
    for desk in [&env.desk, &fresh] {
        assert!(desk.awaiting(NOW + 12).unwrap().requests.is_empty());
        assert_eq!(
            desk.open(id, None, NOW + 12).unwrap_err().kind,
            ErrorKind::SafetyBlocked
        );
        assert_eq!(
            desk.status(id, NOW + 12).unwrap().state,
            AgentPlanState::Denied
        );
    }
}

#[test]
fn opening_again_draws_a_new_code_and_the_old_one_stops_working() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    let hash = p.content_hash().as_str();
    let first = env.desk.open(id, None, NOW).unwrap();
    let second = env.desk.open(id, None, NOW + 1).unwrap();
    assert_ne!(first.code, second.code);
    assert_eq!(second.attempts_left, MAX_CODE_ATTEMPTS);
    let e = env
        .desk
        .approve(id, hash, &first.code, NOW + 2)
        .unwrap_err();
    assert!(e.message.contains("does not match"), "{}", e.message);
    assert!(env.store().get_approval(p.id()).unwrap().is_none());
    env.desk.approve(id, hash, &second.code, NOW + 3).unwrap();
}

#[test]
fn a_denial_in_the_terminal_closes_a_dialog_that_is_open_here() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    let req = env.desk.open(id, None, NOW).unwrap();
    // `scull approve <plan>`: the human types `deny` in their terminal meanwhile.
    approval::record_denial(
        &mut env.store(),
        p.id(),
        Some(p.content_hash()),
        approval::Denial::Denied,
        approval::HumanChannel::Terminal,
        &who(),
        NOW + 1,
    )
    .unwrap();
    let e = env
        .desk
        .approve(id, p.content_hash().as_str(), &req.code, NOW + 2)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked);
    assert!(e.message.contains("denied"), "{}", e.message);
    assert!(!env.desk.is_open(id));
    assert!(env.store().get_approval(p.id()).unwrap().is_none());
    assert_eq!(
        env.desk.status(id, NOW + 3).unwrap().state,
        AgentPlanState::Denied
    );
}

#[test]
fn deny_closes_the_plan_here_and_is_audited() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    env.desk.open(id, None, NOW).unwrap();
    let d = env.desk.deny(id, NOW + 3).unwrap();
    validate("agent-denied", &d);
    assert_eq!(d.denied_at, NOW + 3);
    assert!(!env.desk.is_open(id));
    assert!(env.store().get_approval(p.id()).unwrap().is_none());
    let events = env.store().agent_events(0, 100).unwrap();
    let denial = events
        .iter()
        .find(|e| e.kind == EventKind::ApprovalDenied && e.reason == "denied")
        .expect("denial recorded");
    assert_eq!(denial.source, "desktop");
    assert_eq!(denial.content_hash.as_ref(), Some(p.content_hash()));

    // No longer listed, can't be opened, and reports Denied (also for a fresh desk: the
    // denial is read back from the store).
    let fresh = ApprovalDesk::with_principal(Some(env.dir.path().to_path_buf()), who());
    for desk in [&env.desk, &fresh] {
        assert!(desk.awaiting(NOW + 4).unwrap().requests.is_empty());
        assert_eq!(
            desk.open(id, None, NOW + 4).unwrap_err().kind,
            ErrorKind::SafetyBlocked
        );
        let s = desk.status(id, NOW + 4).unwrap();
        validate("agent-plan-status", &s);
        assert_eq!(s.state, AgentPlanState::Denied);
        assert_eq!(s.denied_at, Some(NOW + 3));
    }
}

#[test]
fn an_approved_plan_is_revoked_not_denied() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    let req = env.desk.open(id, None, NOW).unwrap();
    env.desk
        .approve(id, p.content_hash().as_str(), &req.code, NOW + 1)
        .unwrap();
    assert_eq!(
        env.desk.deny(id, NOW + 2).unwrap_err().kind,
        ErrorKind::SafetyBlocked
    );
    let r = env.desk.revoke(id, NOW + 3).unwrap();
    validate("agent-revoked", &r);
    assert_eq!(r.revoked_at, NOW + 3);
    let s = env.desk.status(id, NOW + 4).unwrap();
    assert_eq!(s.state, AgentPlanState::Revoked);
    // Spent: the agent can't execute it, and it can't be revoked or approved again.
    assert!(
        !env.store()
            .consume_approval(p.id(), p.content_hash(), NOW + 5)
            .unwrap()
    );
    assert!(env.desk.revoke(id, NOW + 6).is_err());
    assert!(env.desk.open(id, None, NOW + 6).is_err());
    // Revoking a plan nobody approved is refused.
    let q = plan("01JAGENT2", PlanOrigin::Mcp, USER, NOW - 10, &cands());
    let env = Env::with(std::slice::from_ref(&q));
    assert!(env.desk.revoke(q.id().as_str(), NOW).is_err());
}

#[test]
fn the_agents_execution_is_reported_running_then_executed() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    let req = env.desk.open(id, None, NOW).unwrap();
    env.desk
        .approve(id, p.content_hash().as_str(), &req.code, NOW + 1)
        .unwrap();

    // Stand-in for the agent's execution through the Executor: it consumes the approval and
    // records the operation. Nothing on disk is touched.
    let mut store = env.store();
    assert!(
        store
            .consume_approval(p.id(), p.content_hash(), NOW + 10)
            .unwrap()
    );
    let mut audit = store
        .begin_operation(
            &OperationStart {
                operation_id: OperationId::from_suffix("01JOPAGENT").unwrap(),
                plan_id: p.id().clone(),
                content_hash: p.content_hash().clone(),
                approver: USER.into(),
                method: ApprovalMethod::Desktop,
                started_at: NOW + 10,
            },
            &DefaultPermissions,
        )
        .unwrap();
    let mut first = ItemRecord::new(
        p.items()[0].item_id.clone(),
        NOW + 11,
        StoredOutcome::Removed,
    );
    first.freed_bytes = Some(800);
    first.entries_removed = Some(3);
    first.entries_kept = Some(0);
    store.record_item(&mut audit, &first).unwrap();

    let s = env.desk.status(id, NOW + 12).unwrap();
    validate("agent-plan-status", &s);
    assert_eq!(s.state, AgentPlanState::Running);
    let op = s.operation.unwrap();
    assert_eq!(op.items.len(), 1);
    assert_eq!(op.items[0].outcome, AgentItemOutcome::Removed);
    assert_eq!(op.finished_at, None);

    let second = ItemRecord::new(
        p.items()[1].item_id.clone(),
        NOW + 13,
        StoredOutcome::Skipped,
    )
    .with_reason("changed");
    store.record_item(&mut audit, &second).unwrap();
    store
        .finish_operation(
            &mut audit,
            &OperationFinish {
                finished_at: NOW + 20,
                end: OperationEnd::Normal,
                measured_delta_bytes: Some(790),
            },
        )
        .unwrap();
    let s = env.desk.status(id, NOW + 21).unwrap();
    validate("agent-plan-status", &s);
    assert_eq!(s.state, AgentPlanState::Executed);
    let op = s.operation.unwrap();
    assert_eq!(op.finished_at, Some(NOW + 20));
    assert_eq!(op.items[1].outcome, AgentItemOutcome::Skipped);
    assert_eq!(op.items[1].reason.as_deref(), Some("changed"));
    assert_eq!(op.freed_bytes, 800);
    assert_eq!(op.measured_delta_bytes, Some(790));
}

#[test]
fn a_plan_voided_before_it_ran_reports_voided() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    let req = env.desk.open(id, None, NOW).unwrap();
    env.desk
        .approve(id, p.content_hash().as_str(), &req.code, NOW + 1)
        .unwrap();
    // D-34: the agent's execute found a target changed and voided the plan.
    assert!(approval::void(&mut env.store(), &p, NOW + 5).unwrap());
    let s = env.desk.status(id, NOW + 6).unwrap();
    assert_eq!(s.state, AgentPlanState::Voided);
    assert!(s.operation.is_none());
}

#[test]
fn an_expired_plan_is_not_listed_or_opened_and_reports_expired() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let id = p.id().as_str();
    let later = p.expires_unix() + 1;
    assert!(env.desk.awaiting(later).unwrap().requests.is_empty());
    assert_eq!(
        env.desk.open(id, None, later).unwrap_err().kind,
        ErrorKind::PlanExpired
    );
    assert_eq!(
        env.desk.status(id, later).unwrap().state,
        AgentPlanState::Expired
    );

    // Opened in time, approved too late: refused, nothing recorded.
    let req = env.desk.open(id, None, NOW).unwrap();
    let e = env
        .desk
        .approve(id, p.content_hash().as_str(), &req.code, later)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanExpired);
    assert!(env.store().get_approval(p.id()).unwrap().is_none());
}

#[test]
fn items_found_unchanged_by_the_last_scan_show_its_details() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let mut scanned = cands();
    scanned[0].evidence = vec![scull_core::Evidence::KnownArtifact {
        artifact: "Cargo build output".into(),
    }];
    // The second item changed tier since the plan: its scan details are not used.
    scanned[1].tier = Tier::S2;
    let report = ScanReport {
        scan_id: ScanId::from_suffix("S2").unwrap(),
        started_at: NOW - 100,
        finished_at: NOW - 90,
        roots: vec![FsPath::from("/home/dev/code")],
        cancelled: false,
        candidates: scanned,
        projects: vec![],
        worktrees: vec![],
        worktree_problems: vec![],
        totals: Default::default(),
        entries_scanned: 10,
        issues: vec![],
        issues_truncated: 0,
        signals_unavailable: vec![],
        config_warnings: vec![],
        history: scull_engine::HistoryOutcome::Disabled,
    };
    let req = env.desk.open(p.id().as_str(), Some(&report), NOW).unwrap();
    validate("agent-approval-request", &req);
    // Plans keep their items sorted by ID: look them up.
    let item = |c: &Candidate| req.items.iter().find(|i| i.candidate.id == c.id).unwrap();
    let (unchanged, changed) = (item(&report.candidates[0]), item(&report.candidates[1]));
    assert!(unchanged.from_last_scan);
    assert_eq!(unchanged.candidate.evidence.len(), 1);
    assert!(!changed.from_last_scan);
    let c = &changed.candidate;
    assert_eq!(c.tier, Tier::S1, "the plan's tier, not the scan's");
    assert!(c.evidence.is_empty() && c.reasons.is_empty());
    assert_eq!(c.flags.active, None, "unknown, never \"no\"");
    let recorded = p.items().iter().find(|i| i.item_id == c.id).unwrap();
    assert_eq!(c.sizes.reclaimable.bytes, recorded.reclaimable_bytes);
}

#[test]
fn each_new_request_is_announced_once_and_names_no_client() {
    let p = mcp_plan();
    let env = Env::with(std::slice::from_ref(&p));
    let mut announced = std::collections::HashSet::new();
    let pending = env.desk.awaiting(NOW).unwrap();
    let first = scull_desktop_bridge::approvals::request_notices(&pending, &mut announced, NOW);
    assert_eq!(first.len(), 1);
    let n = &first[0];
    assert_eq!(n.plan_id, p.id().as_str());
    assert!(
        n.title.starts_with("An AI agent wants to clean "),
        "{}",
        n.title
    );
    assert!(
        n.body
            .starts_with("2 items · plan 01JA·GENT · decide within 14 min."),
        "{}",
        n.body
    );
    assert!(
        n.body.contains("Nothing runs until you approve it"),
        "{}",
        n.body
    );
    // Once per plan; nothing for an expired one.
    assert!(
        scull_desktop_bridge::approvals::request_notices(&pending, &mut announced, NOW + 5)
            .is_empty()
    );
    let mut fresh = std::collections::HashSet::new();
    assert!(
        scull_desktop_bridge::approvals::request_notices(&pending, &mut fresh, p.expires_unix())
            .is_empty()
    );
}

#[test]
fn invalid_plan_ids_are_refused() {
    let env = Env::with(&[]);
    for id in ["", "not-a-plan", "plan_../../x"] {
        assert_eq!(
            env.desk.open(id, None, NOW).unwrap_err().kind,
            ErrorKind::Usage
        );
        assert_eq!(
            env.desk
                .approve(id, &"0".repeat(64), "ABCDEF", NOW)
                .unwrap_err()
                .kind,
            ErrorKind::Usage
        );
    }
}
