//! In-process access to the read-only tools, for callers inside scull such as `scull ask`
//! (PRD AI-05).
//!
//! This is the same surface the MCP server offers its clients, minus the metadata-only tools
//! ([`METADATA_TOOLS`]): the same handlers, the same allowlist (the configured scan roots),
//! and the same output backstop (every string escaped, length-capped and home-redacted, names
//! hashed when `mcp.hash_names` is set). Nothing here can delete, move or change the user's
//! files, write scull's registry, or approve or execute a cleanup.
//!
//! **Preparing plans (AI-05).** With [`ScullMcp::with_in_process_plan_tools`] (only when
//! `[mcp] destructive = true` is in the user config file and a data directory is known; the
//! caller checks elevation), an in-process caller also gets [`IN_PROCESS_PLAN_TOOL_NAMES`]:
//! `prepare_cleanup` and the read-only plan views. `execute_cleanup` is never offered or
//! dispatched here (it is an `unknown_tool`), and no tool can approve: a prepared plan is an
//! agent plan like any other (`PlanOrigin::Mcp`, the caller's client name recorded), it awaits
//! a human's `scull approve <plan-id>` (or the desktop dialog), and the same pending-plan cap
//! and per-session rate limit apply (`mcp.max_pending_plans`, `mcp.prepare_per_minute`).

use crate::approval::Principal;
use crate::plan_tools::{self, CleanupRunner, PlanTools, RunRequest, RunSummary};
use crate::server::ScullMcp;
use crate::tools::{self, METADATA_TOOLS, TOOL_NAMES};
use scull_core::{ErrorKind, ScullError};
use scull_engine::CancelToken;
use scull_storage::AgentClient;
use serde_json::{Map, Value};
use std::sync::Arc;

/// The plan tools an in-process caller may be given (AI-05): prepare and the read-only views.
/// Never `execute_cleanup`.
pub const IN_PROCESS_PLAN_TOOL_NAMES: [&str; 4] = [
    "prepare_cleanup",
    "get_cleanup_plan",
    "get_approval_status",
    "get_cleanup_status",
];

/// The next step an in-process caller's plan output names: approval by a human, and no
/// execution from here.
fn in_process_next_step(plan_id: &str) -> String {
    format!(
        "Nothing was changed. Tell the user to review and approve plan {plan_id} by running          `scull approve {plan_id}` in their own terminal (or in the scull desktop app). No tool          can approve it, and this assistant cannot execute it. It expires 15 minutes after it          was prepared."
    )
}

/// The runner of in-process plan tools: never executes anything (execution is not offered
/// in-process; this is the backstop).
struct NoExecution;

impl CleanupRunner for NoExecution {
    fn run(&self, _request: RunRequest<'_>) -> Result<RunSummary, ScullError> {
        Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            "plans prepared in-process are never executed in-process",
        ))
    }
}

/// The tools an in-process caller may use: every tool in [`TOOL_NAMES`] except the
/// metadata-only [`METADATA_TOOLS`], in listing order.
pub fn read_only_tool_names() -> Vec<&'static str> {
    TOOL_NAMES
        .iter()
        .copied()
        .filter(|n| !METADATA_TOOLS.contains(n))
        .collect()
}

/// One read-only tool as an in-process caller (and the model behind it) sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// Tool name, e.g. `list_projects`.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// JSON Schema of the arguments (an object schema; unknown fields are refused).
    pub input_schema: Value,
}

/// The outcome of one in-process call, already escaped, capped and redacted exactly like an
/// MCP `tools/call` result.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalResult {
    /// Whether the call failed (unknown tool, bad arguments, path outside the allowlist, ...).
    pub is_error: bool,
    /// The one-paragraph summary (or `error (<code>): <message>`).
    pub text: String,
    /// The structured result (or `{"error": {"code", "message"}}`).
    pub structured: Value,
}

fn spec(t: rmcp::model::Tool) -> ToolSpec {
    let mut schema = t.input_schema.as_ref().clone();
    // Draft markers and titles are noise to a model; the shape stays the same.
    schema.remove("$schema");
    schema.remove("title");
    ToolSpec {
        name: t.name.to_string(),
        description: t.description.map(|d| d.to_string()).unwrap_or_default(),
        input_schema: Value::Object(schema),
    }
}

impl ScullMcp {
    /// Give in-process callers [`IN_PROCESS_PLAN_TOOL_NAMES`] (AI-05), **only if**
    /// `[mcp] destructive = true` is in the user config file this server was built with and a
    /// data directory is known; otherwise nothing changes. The caller must not call this when
    /// running elevated. `client` is recorded with each prepared plan for the human to see.
    #[must_use]
    pub fn with_in_process_plan_tools(
        mut self,
        principal: Principal,
        client: Option<AgentClient>,
    ) -> Self {
        let Some(data_dir) = self.ctx().data_dir.clone() else {
            return self;
        };
        if !self.destructive_config() {
            return self;
        }
        let plans = PlanTools::new(
            Arc::new(NoExecution),
            principal,
            data_dir,
            self.plan_limits(),
        );
        self.in_process_plans = Some((Arc::new(plans), client));
        self
    }

    /// Whether in-process callers can prepare plans.
    pub fn in_process_plan_tools_enabled(&self) -> bool {
        self.in_process_plans.is_some()
    }

    /// Every tool an in-process caller may use: the read-only tools, then
    /// [`IN_PROCESS_PLAN_TOOL_NAMES`] when enabled.
    pub fn in_process_tools(&self) -> Vec<ToolSpec> {
        let mut v = self.read_only_tools();
        if self.in_process_plans.is_some() {
            v.extend(
                plan_tools::definitions()
                    .into_iter()
                    .filter(|t| IN_PROCESS_PLAN_TOOL_NAMES.contains(&t.name.as_ref()))
                    .map(spec),
            );
        }
        v
    }

    /// Run one in-process tool: a read-only tool, or (when enabled) one of
    /// [`IN_PROCESS_PLAN_TOOL_NAMES`]. `execute_cleanup`, anything that would approve, and any
    /// other name are refused with `unknown_tool` and run nothing.
    pub fn call_in_process(
        &self,
        name: &str,
        args: Map<String, Value>,
        cancel: &CancelToken,
    ) -> LocalResult {
        let Some((plans, client)) = self
            .in_process_plans
            .as_ref()
            .filter(|_| IN_PROCESS_PLAN_TOOL_NAMES.contains(&name))
        else {
            return self.call_read_only(name, args, cancel);
        };
        let ctx = self.ctx();
        let red = &ctx.redactor;
        match plan_tools::call(
            ctx,
            plans,
            &ctx.allowlist,
            name,
            args,
            cancel,
            client.as_ref(),
        ) {
            Some(Ok(mut out)) => {
                // The next step names approval only: this caller can't execute.
                if let Some(id) = out
                    .structured
                    .get("plan_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    && let Some(old) = out
                        .structured
                        .get("next_step")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                {
                    let new = in_process_next_step(&id);
                    out.summary = out.summary.replace(&old, &new);
                    out.structured["next_step"] = Value::String(new);
                }
                red.finish_value(&mut out.structured);
                LocalResult {
                    is_error: false,
                    text: red.finish_text(&out.summary),
                    structured: out.structured,
                }
            }
            Some(Err(e)) => crate::server::error_result(&e, red),
            None => crate::server::error_result(
                &tools::ToolError {
                    code: "unknown_tool".to_owned(),
                    message: "no such tool".to_owned(),
                },
                red,
            ),
        }
    }

    /// The read-only tools ([`read_only_tool_names`]) with their descriptions and argument
    /// schemas, in listing order.
    pub fn read_only_tools(&self) -> Vec<ToolSpec> {
        let allowed = read_only_tool_names();
        tools::definitions()
            .into_iter()
            .filter(|t| allowed.contains(&t.name.as_ref()))
            .map(|t| {
                let mut schema = t.input_schema.as_ref().clone();
                // Draft markers and titles are noise to a model; the shape stays the same.
                schema.remove("$schema");
                schema.remove("title");
                ToolSpec {
                    name: t.name.to_string(),
                    description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                    input_schema: Value::Object(schema),
                }
            })
            .collect()
    }

    /// Run one read-only tool in-process against the configured allowlist. Blocks (a scan may
    /// take a while). A metadata-only or unknown tool is refused with `unknown_tool` and runs
    /// nothing.
    pub fn call_read_only(
        &self,
        name: &str,
        args: Map<String, Value>,
        cancel: &CancelToken,
    ) -> LocalResult {
        let red = &self.ctx().redactor;
        if !read_only_tool_names().contains(&name) {
            let e = tools::ToolError {
                code: "unknown_tool".to_owned(),
                message: "no such read-only tool (only scull's read-only tools can be called)"
                    .to_owned(),
            };
            return crate::server::error_result(&e, red);
        }
        let ctx = self.ctx();
        match tools::call(ctx, &ctx.allowlist, name, args, cancel) {
            Some(Ok(mut out)) => {
                red.finish_value(&mut out.structured);
                LocalResult {
                    is_error: false,
                    text: red.finish_text(&out.summary),
                    structured: out.structured,
                }
            }
            Some(Err(e)) => crate::server::error_result(&e, red),
            None => crate::server::error_result(
                &tools::ToolError {
                    code: "unknown_tool".to_owned(),
                    message: "no such tool".to_owned(),
                },
                red,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_process_tools_are_the_read_only_tools_without_metadata_tools() {
        let names = read_only_tool_names();
        assert_eq!(names.len(), TOOL_NAMES.len() - METADATA_TOOLS.len());
        assert!(!names.contains(&"register_worktree"));
        assert!(names.contains(&"get_status") && names.contains(&"list_projects"));
    }
}
