//! In-process access to the read-only tools, for callers inside scull such as `scull ask`
//! (PRD AI-05).
//!
//! This is the same surface the MCP server offers its clients, minus the metadata-only tools
//! ([`METADATA_TOOLS`]): the same handlers, the same allowlist (the configured scan roots),
//! and the same output backstop (every string escaped, length-capped and home-redacted, names
//! hashed when `mcp.hash_names` is set). Nothing here can delete, move or change the user's
//! files, write scull's registry, or prepare or approve a cleanup.

use crate::server::ScullMcp;
use crate::tools::{self, METADATA_TOOLS, TOOL_NAMES};
use scull_engine::CancelToken;
use serde_json::{Map, Value};

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

impl ScullMcp {
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
