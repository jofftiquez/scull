//! The Executor's own approval gate for agent plans (P4-MCP-01, MCP-04, PRD §13.3 step 4).
//!
//! A plan made by the MCP server (`PlanOrigin::Mcp`) starts an operation only by consuming a
//! human approval (the `scull approve` terminal or the desktop dialog) bound to its exact
//! content hash, unexpired and unused, and `execute_plan` refuses it outside that operation.
//! Whatever calls the Executor, an agent plan never runs on the agent's say-so. Plans come
//! from a real engine scan of a `scull-fixtures` tree; the database and the lock live beside
//! the fixture root. Every test checks the invariants (I-1, I-2, I-3, I-8).
//!
//! Fixture IDs (docs/FIXTURES.md): FX-MCP-01 (no approval), FX-MCP-02 (approval for other
//! content), FX-MCP-03 (expired approval), FX-MCP-04 (approval reused).
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/planning.rs"]
mod planning;

use planning::*;
use scull_cleaner::{ExecutionReport, Executor, GATE_REASONS, RefusalSource};
use scull_core::{
    ErrorKind, OperationId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams, ScullError,
};
use scull_engine::{CancelToken, PlatformOwnerOnly, ScanReport, now_unix};
use scull_fixtures::{Fixture, InvariantInputs};
use scull_storage::{
    AgentAuditEvent, ApprovalMethod, EventKind, NewApproval, OperationEnd, OperationStart, Store,
};

fn db_path(fx: &Fixture) -> std::path::PathBuf {
    fx.base().join("data").join("scull.db")
}

fn store(fx: &Fixture) -> Store {
    Store::open(&db_path(fx), &PlatformOwnerOnly).unwrap()
}

/// An agent plan of the candidates at `rels`, created at `created`.
fn mcp_plan(fx: &Fixture, scan: &ScanReport, rels: &[&str], id: &str, created: i64) -> Plan {
    let items = rels
        .iter()
        .map(|p| PlanItem::from_candidate(at(fx, scan, p)).unwrap())
        .collect();
    Plan::new(
        PlanParams {
            id: PlanId::from_suffix(id).unwrap(),
            scan_id: scan.scan_id.clone(),
            created_unix: created,
            ttl_secs: None,
            machine_id: MACHINE.into(),
            user: USER.into(),
            origin: PlanOrigin::Mcp,
            operations_watermark: None,
        },
        items,
    )
    .unwrap()
}

/// The account this test runs as (what the Executor reads from the OS).
fn me() -> String {
    scull_platform::current_user().unwrap()
}

fn approve(s: &mut Store, plan: &Plan, method: ApprovalMethod, at: i64, expires: i64) {
    approve_as(s, plan, method, at, expires, &me());
}

fn approve_as(
    s: &mut Store,
    plan: &Plan,
    method: ApprovalMethod,
    at: i64,
    expires: i64,
    approver: &str,
) {
    s.insert_approval(&NewApproval {
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        method,
        approver: approver.into(),
        approved_at: at,
        expires_at: expires,
    })
    .unwrap();
}

/// The start the MCP runner sends: this account (from the OS) and the approval's channel.
fn start(plan: &Plan, op: &str, method: ApprovalMethod) -> OperationStart {
    OperationStart {
        operation_id: OperationId::from_suffix(op).unwrap(),
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        approver: me(),
        method,
        started_at: now_unix(),
    }
}

/// `begin_operation` + `execute_plan` + `finish`, as the MCP runner does.
fn run(
    fx: &Fixture,
    h: &Harness,
    s: &mut Store,
    exec: &Executor,
    plan: &Plan,
    op: &str,
) -> Result<ExecutionReport, ScullError> {
    let mut operation = exec.begin_operation(s, plan, &start(plan, op, ApprovalMethod::CliTty))?;
    let r = exec.execute_plan(
        plan,
        &ctx(),
        &h.revalidation(fx),
        Some(&mut operation),
        &CancelToken::new(),
    )?;
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    Ok(r)
}

/// Nothing changed in the tree and no operation was recorded.
fn nothing_happened(fx: &Fixture, s: &Store) {
    assert!(fx.root().join("app/target").exists());
    assert!(s.operations(0).unwrap().is_empty(), "no operation recorded");
    let inv = fx.check_invariants(&InvariantInputs::new());
    println!("{inv}");
    inv.assert_ok();
}

fn refused(r: Result<ExecutionReport, ScullError>, kind: ErrorKind) -> ScullError {
    let e = r.expect_err("must be refused");
    assert_eq!(e.kind, kind, "{}", e.message);
    e
}

/// The happy path: a human (terminal) approval lets the agent plan run once, the operation
/// records the approval's channel and approver, and the approval is consumed.
#[test]
fn approved_agent_plan_runs_once_and_consumes_the_approval() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let now = now_unix();
    let plan = mcp_plan(&fx, &scan, &["app/target"], "01JMCPOK", now);
    s.insert_plan(&plan).unwrap();
    approve(
        &mut s,
        &plan,
        ApprovalMethod::CliTty,
        now,
        plan.expires_unix(),
    );
    let exec = locked(&fx);
    let r = run(&fx, &h, &mut s, &exec, &plan, "01JMCPOK").unwrap();
    assert_executed(&fx, &r, "app/target");
    let a = s.get_approval(plan.id()).unwrap().unwrap();
    assert!(a.used_at.is_some(), "consumed");
    let op = s
        .operation(&OperationId::from_suffix("01JMCPOK").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(op.method, ApprovalMethod::CliTty);
    assert_eq!(op.approver, me());
    check(&fx, &plan, &r, &["app/target"]);
}

/// FX-MCP-01: an agent plan without any approval fails closed, through `begin_operation` and
/// through `execute_plan` without an operation.
#[test]
fn fx_mcp_01_agent_plan_without_approval_fails_closed() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let plan = mcp_plan(&fx, &scan, &["app/target"], "01JMCP01", now_unix());
    s.insert_plan(&plan).unwrap();
    let exec = locked(&fx);
    let e = refused(
        run(&fx, &h, &mut s, &exec, &plan, "01JMCP01"),
        ErrorKind::SafetyBlocked,
    );
    assert!(e.message.contains("scull approve"), "{}", e.message);
    let e = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(&fx),
            None,
            &CancelToken::new(),
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{}", e.message);
    nothing_happened(&fx, &s);
}

/// FX-MCP-02: an approval binds to the exact plan content. A substitute plan under the
/// approved ID (other items) is refused without spending the approval; so is a different
/// plan that was never approved, and an operation for one plan can't carry another.
#[test]
fn fx_mcp_02_approval_for_other_plan_content_fails() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let now = now_unix();
    let approved = mcp_plan(&fx, &scan, &["lib/target"], "01JMCP02", now);
    s.insert_plan(&approved).unwrap();
    approve(
        &mut s,
        &approved,
        ApprovalMethod::CliTty,
        now,
        approved.expires_unix(),
    );
    let exec = locked(&fx);
    // Same ID, other content (never recorded).
    let substitute = mcp_plan(&fx, &scan, &["app/target"], "01JMCP02", now);
    assert_ne!(substitute.content_hash(), approved.content_hash());
    refused(
        run(&fx, &h, &mut s, &exec, &substitute, "01JMCP02A"),
        ErrorKind::PlanTampered,
    );
    // Another recorded plan, not approved.
    let other = mcp_plan(&fx, &scan, &["app/target"], "01JMCP02B", now);
    s.insert_plan(&other).unwrap();
    refused(
        run(&fx, &h, &mut s, &exec, &other, "01JMCP02B"),
        ErrorKind::SafetyBlocked,
    );
    assert_eq!(
        s.get_approval(approved.id()).unwrap().unwrap().used_at,
        None,
        "the real approval was not spent"
    );
    nothing_happened(&fx, &s);

    // An operation started for the approved plan can't execute the other one.
    let mut op = exec
        .begin_operation(
            &mut s,
            &approved,
            &start(&approved, "01JMCP02C", ApprovalMethod::CliTty),
        )
        .unwrap();
    let e = exec
        .execute_plan(
            &other,
            &ctx(),
            &h.revalidation(&fx),
            Some(&mut op),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{}", e.message);
    op.finish(OperationEnd::Cancelled, now_unix()).unwrap();
    assert!(fx.root().join("app/target").exists());
    assert!(fx.root().join("lib/target").exists());
    let inv = fx.check_invariants(&InvariantInputs::new());
    inv.assert_ok();
}

/// FX-MCP-03: an expired approval fails, even when the caller claims an earlier start time.
#[test]
fn fx_mcp_03_expired_approval_fails() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let now = now_unix();
    // Prepared 14 minutes ago (expires in a minute); approved 13 minutes ago for 12 minutes.
    let plan = mcp_plan(&fx, &scan, &["app/target"], "01JMCP03", now - 14 * 60);
    s.insert_plan(&plan).unwrap();
    approve(
        &mut s,
        &plan,
        ApprovalMethod::CliTty,
        now - 13 * 60,
        now - 60,
    );
    let exec = locked(&fx);
    let mut st = start(&plan, "01JMCP03", ApprovalMethod::CliTty);
    st.started_at = now - 5 * 60; // A lie about the time does not help.
    let e = exec.begin_operation(&mut s, &plan, &st).unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{}", e.message);
    assert!(e.message.contains("expired"), "{}", e.message);
    assert_eq!(s.get_approval(plan.id()).unwrap().unwrap().used_at, None);
    nothing_happened(&fx, &s);
}

/// FX-MCP-04: an approval is single-use. Once consumed (by a run, or because the plan was
/// voided), it never starts another operation.
#[test]
fn fx_mcp_04_approval_reused_for_a_second_execution_fails() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let now = now_unix();
    let plan = mcp_plan(&fx, &scan, &["app/target"], "01JMCP04", now);
    s.insert_plan(&plan).unwrap();
    approve(
        &mut s,
        &plan,
        ApprovalMethod::Desktop,
        now,
        plan.expires_unix(),
    );
    // Voided before execution (D-34): the approval is spent without an operation.
    assert!(
        s.consume_approval(plan.id(), plan.content_hash(), now_unix())
            .unwrap()
    );
    let exec = locked(&fx);
    let e = exec
        .begin_operation(
            &mut s,
            &plan,
            &start(&plan, "01JMCP04", ApprovalMethod::Desktop),
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{}", e.message);
    nothing_happened(&fx, &s);

    // A plan that ran once can't run again (spent approval; its targets were touched).
    let plan2 = mcp_plan(&fx, &scan, &["lib/target"], "01JMCP04B", now);
    s.insert_plan(&plan2).unwrap();
    approve(
        &mut s,
        &plan2,
        ApprovalMethod::CliTty,
        now,
        plan2.expires_unix(),
    );
    let r = run(&fx, &h, &mut s, &exec, &plan2, "01JMCP04B").unwrap();
    assert_executed(&fx, &r, "lib/target");
    check(&fx, &plan2, &r, &["lib/target"]);
    let again = run(&fx, &h, &mut s, &exec, &plan2, "01JMCP04C");
    let e = again.unwrap_err();
    assert!(
        matches!(
            e.kind,
            ErrorKind::SafetyBlocked | ErrorKind::PlanInvalidated
        ),
        "{e:?}"
    );
    assert_eq!(s.operations(0).unwrap().len(), 1, "exactly one operation");
}

/// Only a human channel approves an agent plan, and the operation must record the approval's
/// own approver and channel (SAFE-08): a mismatch is refused without spending it.
#[test]
fn non_human_or_misrecorded_approvals_are_refused() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let now = now_unix();
    let exec = locked(&fx);
    let flagged = mcp_plan(&fx, &scan, &["app/target"], "01JMCPFLAG", now);
    s.insert_plan(&flagged).unwrap();
    approve(
        &mut s,
        &flagged,
        ApprovalMethod::CliFlag,
        now,
        flagged.expires_unix(),
    );
    let e = exec
        .begin_operation(
            &mut s,
            &flagged,
            &start(&flagged, "01JMCPFLAG", ApprovalMethod::CliFlag),
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{}", e.message);

    let plan = mcp_plan(&fx, &scan, &["app/target"], "01JMCPWHO", now);
    s.insert_plan(&plan).unwrap();
    approve(
        &mut s,
        &plan,
        ApprovalMethod::CliTty,
        now,
        plan.expires_unix(),
    );
    let e = exec
        .begin_operation(
            &mut s,
            &plan,
            &start(&plan, "01JMCPWHO", ApprovalMethod::Desktop),
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanTampered, "{}", e.message);
    let mut st = start(&plan, "01JMCPWHO", ApprovalMethod::CliTty);
    st.approver = "someone-else".into();
    let e = exec.begin_operation(&mut s, &plan, &st).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanTampered, "{}", e.message);
    assert_eq!(s.get_approval(plan.id()).unwrap().unwrap().used_at, None);

    // An approval recorded for another account never runs here, whatever the caller claims:
    // the Executor reads the account it runs as from the OS.
    let theirs = mcp_plan(&fx, &scan, &["app/target"], "01JMCPOTHER", now);
    s.insert_plan(&theirs).unwrap();
    approve_as(
        &mut s,
        &theirs,
        ApprovalMethod::CliTty,
        now,
        theirs.expires_unix(),
        "other-account",
    );
    for approver in [me(), "other-account".to_owned()] {
        let mut st = start(&theirs, "01JMCPOTHER", ApprovalMethod::CliTty);
        st.approver = approver;
        let e = exec.begin_operation(&mut s, &theirs, &st).unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{}", e.message);
    }
    assert_eq!(s.get_approval(theirs.id()).unwrap().unwrap().used_at, None);
    nothing_happened(&fx, &s);
}

/// CLI plans are unaffected: they still run on the user's own confirmation.
#[test]
fn cli_plans_need_no_broker_approval() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let plan = plan_named(&fx, &scan, &["app/target"], "01JCLIPLAN");
    s.insert_plan(&plan).unwrap();
    let exec = locked(&fx);
    let mut op = exec
        .begin_operation(
            &mut s,
            &plan,
            &start(&plan, "01JCLIPLAN", ApprovalMethod::CliInteractive),
        )
        .unwrap();
    let r = exec
        .execute_plan(
            &plan,
            &ctx(),
            &h.revalidation(&fx),
            Some(&mut op),
            &CancelToken::new(),
        )
        .unwrap();
    op.finish(OperationEnd::Normal, now_unix()).unwrap();
    assert_executed(&fx, &r, "app/target");
    check(&fx, &plan, &r, &["app/target"]);
}

/// The reasons the gate recorded for `plan` (agent audit events with its ID and the hash of
/// the plan presented, source `executor`), oldest first. Every one carries channel `mcp` and
/// this account.
fn gate_reasons(s: &Store, plan: &Plan) -> Vec<String> {
    s.agent_events(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|e| {
            e.kind == EventKind::ExecuteRefused
                && e.plan_id.as_ref() == Some(plan.id())
                && e.content_hash.as_ref() == Some(plan.content_hash())
        })
        .map(|e| {
            assert_eq!(e.source, "executor");
            assert_eq!(e.channel, "mcp");
            assert_eq!(e.user, me());
            assert_eq!(e.machine_id, MACHINE);
            assert!(GATE_REASONS.contains(&e.reason.as_str()), "{}", e.reason);
            e.reason
        })
        .collect()
}

/// PRD §13.3 step 5: every refusal of the Executor's gate is audited with its reason, and
/// recording it never creates, spends or changes an approval. CLI plans are not audited
/// here (they are not agent requests).
#[test]
fn every_gate_refusal_is_audited_without_touching_the_approval() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let now = now_unix();
    let exec = locked(&fx);

    // not_approved, then lock_not_held and operation_mismatch (no approval exists).
    let none = mcp_plan(&fx, &scan, &["app/target"], "01JAUDNONE", now);
    s.insert_plan(&none).unwrap();
    let st = start(&none, "01JAUDNONE", ApprovalMethod::CliTty);
    exec.begin_operation(&mut s, &none, &st).unwrap_err();
    Executor::new()
        .begin_operation(&mut s, &none, &st)
        .unwrap_err();
    let mut wrong = st.clone();
    wrong.plan_id = PlanId::from_suffix("01JAUDOTHER").unwrap();
    exec.begin_operation(&mut s, &none, &wrong).unwrap_err();
    assert_eq!(
        gate_reasons(&s, &none),
        ["not_approved", "lock_not_held", "operation_mismatch"]
    );
    assert!(s.get_approval(none.id()).unwrap().is_none(), "none created");

    // not_human (a non-human method).
    let flag = mcp_plan(&fx, &scan, &["app/target"], "01JAUDFLAG", now);
    s.insert_plan(&flag).unwrap();
    approve(
        &mut s,
        &flag,
        ApprovalMethod::CliFlag,
        now,
        flag.expires_unix(),
    );
    exec.begin_operation(
        &mut s,
        &flag,
        &start(&flag, "01JAUDFLAG", ApprovalMethod::CliFlag),
    )
    .unwrap_err();
    assert_eq!(gate_reasons(&s, &flag), ["not_human"]);
    assert_eq!(s.get_approval(flag.id()).unwrap().unwrap().used_at, None);

    // channel_mismatch, approver_not_recorded, approval_mismatch (a substitute under the
    // approved ID), other_account.
    let who = mcp_plan(&fx, &scan, &["app/target"], "01JAUDWHO", now);
    s.insert_plan(&who).unwrap();
    approve(
        &mut s,
        &who,
        ApprovalMethod::CliTty,
        now,
        who.expires_unix(),
    );
    exec.begin_operation(
        &mut s,
        &who,
        &start(&who, "01JAUDWHO", ApprovalMethod::Desktop),
    )
    .unwrap_err();
    let mut st = start(&who, "01JAUDWHO", ApprovalMethod::CliTty);
    st.approver = "someone-else".into();
    exec.begin_operation(&mut s, &who, &st).unwrap_err();
    assert_eq!(
        gate_reasons(&s, &who),
        ["channel_mismatch", "approver_not_recorded"]
    );
    assert_eq!(s.get_approval(who.id()).unwrap().unwrap().used_at, None);
    let substitute = mcp_plan(&fx, &scan, &["lib/target"], "01JAUDWHO", now);
    exec.begin_operation(
        &mut s,
        &substitute,
        &start(&substitute, "01JAUDSUB", ApprovalMethod::CliTty),
    )
    .unwrap_err();
    assert_eq!(gate_reasons(&s, &substitute), ["approval_mismatch"]);
    let theirs = mcp_plan(&fx, &scan, &["app/target"], "01JAUDTHEIRS", now);
    s.insert_plan(&theirs).unwrap();
    approve_as(
        &mut s,
        &theirs,
        ApprovalMethod::CliTty,
        now,
        theirs.expires_unix(),
        "other-account",
    );
    exec.begin_operation(
        &mut s,
        &theirs,
        &start(&theirs, "01JAUDTHEIRS", ApprovalMethod::CliTty),
    )
    .unwrap_err();
    assert_eq!(gate_reasons(&s, &theirs), ["other_account"]);
    assert_eq!(s.get_approval(theirs.id()).unwrap().unwrap().used_at, None);

    // approval_expired (unused), approval_used (spent by a void).
    let old = mcp_plan(&fx, &scan, &["app/target"], "01JAUDOLD", now - 14 * 60);
    s.insert_plan(&old).unwrap();
    approve(
        &mut s,
        &old,
        ApprovalMethod::CliTty,
        now - 13 * 60,
        now - 60,
    );
    exec.begin_operation(
        &mut s,
        &old,
        &start(&old, "01JAUDOLD", ApprovalMethod::CliTty),
    )
    .unwrap_err();
    assert_eq!(gate_reasons(&s, &old), ["approval_expired"]);
    assert_eq!(s.get_approval(old.id()).unwrap().unwrap().used_at, None);
    let spent = mcp_plan(&fx, &scan, &["app/target"], "01JAUDSPENT", now);
    s.insert_plan(&spent).unwrap();
    approve(
        &mut s,
        &spent,
        ApprovalMethod::Desktop,
        now,
        spent.expires_unix(),
    );
    assert!(
        s.consume_approval(spent.id(), spent.content_hash(), now_unix())
            .unwrap()
    );
    let used_at = s.get_approval(spent.id()).unwrap().unwrap().used_at;
    exec.begin_operation(
        &mut s,
        &spent,
        &start(&spent, "01JAUDSPENT", ApprovalMethod::Desktop),
    )
    .unwrap_err();
    assert_eq!(gate_reasons(&s, &spent), ["approval_used"]);
    assert_eq!(
        s.get_approval(spent.id()).unwrap().unwrap().used_at,
        used_at
    );
    nothing_happened(&fx, &s);

    // unfinished_operation: an earlier operation did not finish (a crash).
    let cli = plan_named(&fx, &scan, &["lib/target"], "01JAUDCLI");
    s.insert_plan(&cli).unwrap();
    let op = exec
        .begin_operation(
            &mut s,
            &cli,
            &start(&cli, "01JAUDCLI", ApprovalMethod::CliInteractive),
        )
        .unwrap();
    drop(op);
    let after = mcp_plan(&fx, &scan, &["app/target"], "01JAUDAFTER", now_unix());
    s.insert_plan(&after).unwrap();
    approve(
        &mut s,
        &after,
        ApprovalMethod::CliTty,
        now_unix(),
        after.expires_unix(),
    );
    exec.begin_operation(
        &mut s,
        &after,
        &start(&after, "01JAUDAFTER", ApprovalMethod::CliTty),
    )
    .unwrap_err();
    assert_eq!(gate_reasons(&s, &after), ["unfinished_operation"]);
    assert_eq!(s.get_approval(after.id()).unwrap().unwrap().used_at, None);
    // The CLI plan's own refusals are not agent events.
    assert!(gate_reasons(&s, &cli).is_empty());
    let inv = fx.check_invariants(&InvariantInputs::new());
    inv.assert_ok();
}

/// `targets_touched` and `outside_operation`: an agent plan whose targets another operation
/// touched is refused (and audited) without spending its approval, and an operation started
/// for one plan can't run an agent plan (audited through that operation's store).
#[test]
fn touched_targets_and_foreign_operations_are_audited() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let now = now_unix();
    let exec = locked(&fx);
    let agent = mcp_plan(&fx, &scan, &["app/target"], "01JAUDTOUCH", now);
    s.insert_plan(&agent).unwrap();
    approve(
        &mut s,
        &agent,
        ApprovalMethod::CliTty,
        now,
        agent.expires_unix(),
    );
    // An operation for a CLI plan of the same target runs first and finishes.
    let cli = plan_named(&fx, &scan, &["app/target"], "01JAUDFIRST");
    s.insert_plan(&cli).unwrap();
    let mut op = exec
        .begin_operation(
            &mut s,
            &cli,
            &start(&cli, "01JAUDFIRST", ApprovalMethod::CliInteractive),
        )
        .unwrap();
    // While it is open, it can't carry the agent plan.
    let e = exec
        .execute_plan(
            &agent,
            &ctx(),
            &h.revalidation(&fx),
            Some(&mut op),
            &CancelToken::new(),
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{}", e.message);
    op.finish(OperationEnd::Cancelled, now_unix()).unwrap();
    let e = exec
        .begin_operation(
            &mut s,
            &agent,
            &start(&agent, "01JAUDTOUCH", ApprovalMethod::CliTty),
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{}", e.message);
    assert_eq!(
        gate_reasons(&s, &agent),
        ["outside_operation", "targets_touched"]
    );
    assert_eq!(s.get_approval(agent.id()).unwrap().unwrap().used_at, None);
    assert!(fx.root().join("app/target").exists());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

fn revocation(plan: &Plan) -> AgentAuditEvent {
    AgentAuditEvent {
        event_id: 0,
        at: now_unix(),
        kind: EventKind::ApprovalRevoked,
        source: "cli".into(),
        plan_id: Some(plan.id().clone()),
        content_hash: Some(plan.content_hash().clone()),
        reason: "revoked".into(),
        channel: "terminal".into(),
        machine_id: MACHINE.into(),
        user: me(),
        count: 1,
    }
}

/// #298: an approval revoked before execution is refused by the Executor's gate (audited as
/// `approval_revoked`), and nothing runs.
#[test]
fn revoked_approval_is_refused_by_the_gate() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let now = now_unix();
    let plan = mcp_plan(&fx, &scan, &["app/target"], "01JREVOKED", now);
    s.insert_plan(&plan).unwrap();
    approve(
        &mut s,
        &plan,
        ApprovalMethod::CliTty,
        now,
        plan.expires_unix(),
    );
    assert!(
        s.revoke_approval(
            plan.id(),
            plan.content_hash(),
            now_unix(),
            &revocation(&plan)
        )
        .unwrap()
    );
    let exec = locked(&fx);
    let e = refused(
        run(&fx, &h, &mut s, &exec, &plan, "01JREVOKED"),
        ErrorKind::SafetyBlocked,
    );
    assert!(e.message.contains("revoked"), "{}", e.message);
    assert_eq!(gate_reasons(&s, &plan), ["approval_revoked"]);
    nothing_happened(&fx, &s);
}

/// #298: a revocation racing `Executor::begin_operation` for the same approval, each on its
/// own database connection and released at the same instant: exactly one wins, every round.
/// When the operation wins, the revocation is refused and the operation runs; when the
/// revocation wins, nothing runs and no operation is recorded.
#[test]
fn revocation_racing_begin_operation_has_exactly_one_winner() {
    let (mut ops, mut revs) = (0, 0);
    for round in 0..8 {
        let fx = two_cargo_projects();
        let h = harness(false);
        let mut s = store(&fx);
        let scan = h.scan(&fx);
        let now = now_unix();
        let id = format!("01JRACE{round}");
        let plan = mcp_plan(&fx, &scan, &["app/target"], &id, now);
        s.insert_plan(&plan).unwrap();
        approve(
            &mut s,
            &plan,
            ApprovalMethod::CliTty,
            now,
            plan.expires_unix(),
        );
        let exec = locked(&fx);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let starter = {
            let (barrier, plan, db) = (barrier.clone(), plan.clone(), db_path(&fx));
            std::thread::spawn(move || {
                let mut s = Store::open(&db, &PlatformOwnerOnly).unwrap();
                barrier.wait();
                let started = exec
                    .begin_operation(&mut s, &plan, &start(&plan, &id, ApprovalMethod::CliTty))
                    .map(|op| op.finish(OperationEnd::Cancelled, now_unix()).unwrap());
                started.is_ok()
            })
        };
        let revoker = {
            let (barrier, plan, db) = (barrier, plan.clone(), db_path(&fx));
            std::thread::spawn(move || {
                let mut s = Store::open(&db, &PlatformOwnerOnly).unwrap();
                barrier.wait();
                s.revoke_approval(
                    plan.id(),
                    plan.content_hash(),
                    now_unix(),
                    &revocation(&plan),
                )
                .unwrap()
            })
        };
        let (started, revoked) = (starter.join().unwrap(), revoker.join().unwrap());
        assert!(
            started ^ revoked,
            "round {round}: started={started} revoked={revoked}"
        );
        let recorded = s.operations(0).unwrap().len();
        assert_eq!(recorded, usize::from(started), "round {round}");
        assert_eq!(s.approval_revoked(plan.id()).unwrap(), revoked);
        if revoked {
            assert_eq!(gate_reasons(&s, &plan), ["approval_revoked"]);
        }
        ops += usize::from(started);
        revs += usize::from(revoked);
        assert!(fx.root().join("app/target").exists(), "nothing executed");
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
    }
    println!("operation won {ops}, revocation won {revs}");
}

/// A refused start is attributed to the [`RefusalSource`] the Executor was given: the
/// human's `scull approve --run` records `cli` through the `terminal` channel, not the
/// `executor` acting for the MCP server (the default, above). Same refusal, same approval
/// untouched.
#[test]
fn gate_refusals_are_attributed_to_the_given_source() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let mut s = store(&fx);
    let scan = h.scan(&fx);
    let plan = mcp_plan(&fx, &scan, &["app/target"], "01JMCPSRC", now_unix());
    s.insert_plan(&plan).unwrap();
    let exec = locked(&fx).with_refusal_source(RefusalSource {
        source: "cli",
        channel: "terminal",
    });
    refused(
        run(&fx, &h, &mut s, &exec, &plan, "01JMCPSRC"),
        ErrorKind::SafetyBlocked,
    );
    let events: Vec<(String, String, String)> = s
        .agent_events(0, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::ExecuteRefused)
        .map(|e| (e.source, e.channel, e.reason))
        .collect();
    assert_eq!(
        events,
        [("cli".into(), "terminal".into(), "not_approved".into())]
    );
    nothing_happened(&fx, &s);
}
