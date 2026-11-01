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
use scull_cleaner::{ExecutionReport, Executor};
use scull_core::{
    ErrorKind, OperationId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams, ScullError,
};
use scull_engine::{CancelToken, PlatformOwnerOnly, ScanReport, now_unix};
use scull_fixtures::{Fixture, InvariantInputs};
use scull_storage::{ApprovalMethod, NewApproval, OperationEnd, OperationStart, Store};

fn store(fx: &Fixture) -> Store {
    Store::open(&fx.base().join("data").join("scull.db"), &PlatformOwnerOnly).unwrap()
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
