//! Executing approved agent plans for `scull mcp serve --allow-destructive` (P4-MCP-01) and
//! `scull approve <plan-id> --run`: the
//! [`scull_mcp::CleanupRunner`] backed by `scull-cleaner`'s Executor. `scull-mcp` never
//! depends on the cleaner (SAFE-10); the CLI binary wires the two together.
//!
//! One run is exactly what `scull clean --plan FILE --confirm` does after its own checks:
//!
//! 1. refuse to run elevated (SAFE-14; no override for agents);
//! 2. take the per-user execution lock, failing at once if another operation holds it
//!    (SAFE-09, `lock_held`; the approval is not spent; the refusal is audited);
//! 3. reconcile operations a crash interrupted (SAFE-19);
//! 4. start the operation with this account (from the OS) as the approver and the approval's
//!    channel: `Executor::begin_operation` consumes the approval atomically and refuses an
//!    agent plan without a matching, unexpired, unused human approval given by this account
//!    (MCP-04), or whose targets another operation touched (SAFE-09);
//! 5. execute with per-item revalidation (SAFE-05): S0 deleted, S1/S2 quarantined (the
//!    configured location, P2-EXEC-13), the Trash only where quarantine is impossible (D-3);
//!    the free-space delta is measured and recorded (I-5);
//! 6. finish the audit (SAFE-08).

use super::exec;
use scull_cleaner::trash::TrashLocations;
use scull_cleaner::{ItemOutcome, PlanContext, QuarantineSettings, RemoveOutcome, Revalidation};
use scull_core::ScullError;
use scull_engine::now_unix;
use scull_mcp::{CleanupRunner, RunItem, RunRequest, RunSummary};
use scull_storage::{OperationEnd, OperationStart};
use std::io::Write;

/// The Executor-backed runner.
#[derive(Debug, Clone)]
pub struct ExecutorRunner {
    /// The configured quarantine location (`[quarantine]`, P2-EXEC-13).
    pub quarantine: QuarantineSettings,
    /// The Trash fallback (the sandbox's under the test harness).
    pub trash: TrashLocations,
    /// Audit a refusal to take the execution lock as an agent's refused `execute_cleanup`
    /// (`scull mcp serve`). `scull approve --run` records its own outcome instead.
    pub audit_lock_refusal: bool,
}

impl CleanupRunner for ExecutorRunner {
    fn run(&self, req: RunRequest<'_>) -> Result<RunSummary, ScullError> {
        exec::check_not_elevated(exec::elevation(), false, "execute_cleanup")?;
        let executor = match exec::lock() {
            Ok(e) => e,
            Err(e) => {
                // Audited (PRD §13.3 step 5); the approval is not spent.
                if !self.audit_lock_refusal {
                    return Err(e);
                }
                let _ = scull_mcp::approval::record_execute_refusal(
                    req.store,
                    Some(req.plan.id()),
                    Some(req.plan.content_hash()),
                    &super::json_name(&e.kind),
                    req.principal,
                    now_unix(),
                );
                return Err(e);
            }
        };
        let recovered = executor.recover(req.store, now_unix())?;
        for op in &recovered.operations {
            // stderr: the client's log, never the JSON-RPC stream on stdout.
            let _ = writeln!(
                std::io::stderr(),
                "scull: recovered interrupted operation {} before executing an agent plan",
                op.operation_id
            );
        }
        let started_at = now_unix();
        let operation_id = exec::new_operation_id(started_at)?;
        let mut op = executor
            .begin_operation(
                req.store,
                req.plan,
                &OperationStart {
                    operation_id: operation_id.clone(),
                    plan_id: req.plan.id().clone(),
                    content_hash: req.plan.content_hash().clone(),
                    // The account this process runs as, from the OS (the Executor checks the
                    // approval was given by it); the channel is the approval's.
                    approver: exec::who()?.user,
                    method: req.approval.method,
                    started_at,
                },
            )?
            .with_quarantine(self.quarantine.clone())
            .with_trash(self.trash.clone());
        let revalidation = Revalidation::new(req.engine, req.roots);
        let ctx = PlanContext {
            now_unix: now_unix(),
            machine_id: &req.principal.machine_id,
            user: &req.principal.user,
        };
        let report =
            match executor.execute_plan(req.plan, &ctx, &revalidation, Some(&mut op), req.cancel) {
                Ok(r) => r,
                Err(e) => {
                    let _ = op.finish(OperationEnd::Cancelled, now_unix());
                    return Err(e);
                }
            };
        op.finish(
            if report.cancelled {
                OperationEnd::Cancelled
            } else {
                OperationEnd::Normal
            },
            now_unix(),
        )?;
        Ok(RunSummary {
            operation_id,
            items: report
                .items
                .iter()
                .map(|i| RunItem {
                    item_id: i.item_id.clone(),
                    outcome: outcome_name(&i.outcome).to_owned(),
                    succeeded: i.succeeded(),
                    reason: i.reason_code().map(str::to_owned),
                })
                .collect(),
            cancelled: report.cancelled,
            measured_delta_bytes: report
                .measurement
                .as_ref()
                .and_then(|m| m.measured_delta_bytes()),
        })
    }
}

fn outcome_name(o: &ItemOutcome) -> &'static str {
    match o {
        ItemOutcome::Deleted(r) if r.outcome == RemoveOutcome::Complete => "deleted",
        ItemOutcome::Deleted(_) => "partly_deleted",
        ItemOutcome::Quarantined(_) => "quarantined",
        ItemOutcome::Trashed(_) => "moved_to_trash",
        ItemOutcome::Skipped(_) => "skipped",
        ItemOutcome::Refused(_) => "refused",
        ItemOutcome::Blocked { .. } => "blocked",
        ItemOutcome::NotRun => "not_run",
        _ => "other",
    }
}
