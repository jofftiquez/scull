//! Read-only MCP server over stdio (PRD §13, MCP-01…05).
//!
//! `scull mcp serve` runs [`ScullMcp`] over stdin/stdout with the official Rust SDK (`rmcp`,
//! pinned exactly). There is no network listener (MCP-01).
//!
//! * **Read-only tools by default** ([`TOOL_NAMES`]): `get_storage_overview`,
//!   `scan_directory`, `list_projects`, `inspect_project`, `list_worktrees`,
//!   `inspect_worktree`, `list_caches`, `find_large_files`, `get_cleanup_candidates`,
//!   `get_cleanup_history`, `get_status`.
//! * **Plan and execute tools only in destructive mode** ([`PLAN_TOOL_NAMES`], MCP-04):
//!   `prepare_cleanup`, `get_cleanup_plan`, `get_approval_status`, `execute_cleanup`,
//!   `get_cleanup_status`, offered only when `[mcp] destructive = true` is in the user config
//!   file **and** the server was launched with `--allow-destructive`
//!   ([`ScullMcp::with_plan_tools`], FX-MCP-07). The agent can prepare a plan; no tool can
//!   approve it: the human approves it outside the model with `scull approve <plan-id>` in their own
//!   terminal or in the desktop app ([`approval`], the approval broker). Execution runs through
//!   a [`CleanupRunner`] the CLI implements with `scull-cleaner`'s Executor, which consumes the
//!   approval and revalidates every item.
//! * **Allowlisted roots** ([`Allowlist`], MCP-03): the configured scan roots, narrowed to the
//!   client's MCP roots when it offers them. Every path argument is checked by shape, lexical
//!   containment and canonical (symlink-resolved) containment before anything else sees it;
//!   refusals never reveal anything about paths outside the allowlist.
//! * **Structured arguments**: every tool has a JSON input schema; unknown fields are refused.
//! * **Bounded, structured results**: every tool has an output schema, returns
//!   `structuredContent` plus a short text summary, and caps lists at `mcp.max_results` with a
//!   `truncated` flag. No file contents and no commit messages are ever returned.
//! * **Output safety** (PRD §13.4, SAFE-17, T-40): file and branch names are returned as
//!   structured fields, every string is escaped (control, bidi and invisible characters become
//!   visible ASCII) and length-capped, suspicious names are flagged (`suspicious`), and commit
//!   messages are never returned. See [`Redactor`].
//! * **Privacy** (MCP-05, T-44): the home folder prefix is shown as `~` by default
//!   (`mcp.redact_home`); `mcp.hash_names` hashes project and folder names with a per-machine
//!   salt that is never returned. The CLI prints [`DISCLOSURE`] when MCP is first enabled.
//! * **In-process use** ([`ScullMcp::read_only_tools`], [`ScullMcp::call_read_only`]): the
//!   same read-only tools, allowlist and redaction for callers inside scull (`scull ask`),
//!   without the metadata-only tools or the plan tools.
//! * **Nothing of yours is written** by the read-only tools: scans run without a history
//!   store and the history database is opened read-only. The one metadata-only tool,
//!   `register_worktree`, appends a TTL reminder to scull's own [`registry`] file; expiry raises
//!   a review suggestion and never deletes anything. In destructive mode, `prepare_cleanup`
//!   records the plan in scull's database. This crate never depends on `scull-cleaner`
//!   (SAFE-10).
//!
//! # Connecting a client
//!
//! Enable MCP once (`[mcp] enabled = true` in scull's config file, or run `scull mcp serve`
//! once in a terminal and confirm), then register the command with the client:
//!
//! * Claude Code: `claude mcp add scull -- scull mcp serve`, or in `.mcp.json`:
//!   `{"mcpServers": {"scull": {"command": "scull", "args": ["mcp", "serve"]}}}`
//! * Codex (`~/.codex/config.toml`):
//!   `[mcp_servers.scull]` / `command = "scull"` / `args = ["mcp", "serve"]`

mod allowlist;
pub mod approval;
mod escape;
mod history;
mod local;
mod plan_tools;
mod redact;
pub mod registry;
mod server;
mod tools;
mod views;

pub use allowlist::{Allowlist, MAX_PATH_ARG_BYTES, Refusal, Root, path_from_file_uri};
pub use escape::{Suspicion, TRUNCATION_MARKER, escape, needs_escape, suspicion};
pub use local::{LocalResult, ToolSpec, read_only_tool_names};
pub use plan_tools::{
    CleanupRunner, CleanupStatusOutput, ExecuteOutput, ItemOutcomeView, MAX_PLAN_ITEMS,
    OperationView, PLAN_INSTRUCTIONS, PLAN_TOOL_NAMES, PlanIdArgs, PlanItemView, PlanOutput,
    PrepareArgs, RunItem, RunRequest, RunSummary,
};
pub use redact::{
    HASH_PREFIX, MAX_NAME_CHARS, MAX_PATH_CHARS, MAX_STRING_CHARS, MAX_TEXT_CHARS, NameFlag,
    NameSalt, OMITTED_TEXT, Redactor, SALT_FILE,
};
pub use server::{
    INSTRUCTIONS, ROOTS_TIMEOUT, ScullMcp, ServerOptions, serve, serve_stdio, serve_until,
};
pub use tools::{METADATA_TOOLS, TOOL_NAMES, ToolError, definitions};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// The privacy disclosure shown when MCP is first enabled (PRD §13.5, MCP-05).
pub const DISCLOSURE: &str = "\
scull MCP server: privacy notice
  Results of scull's MCP tools (paths, project names, Git branch names, sizes) are sent to the
  AI client that started the server, and possibly to that client's cloud model provider. This
  is the one exception to scull keeping everything on your machine.
  * Tools are read-only: they cannot delete, move or change your files, or approve a cleanup.
    (register_worktree only records a reminder in scull's own data folder. Cleanup tools
    exist only if you set mcp.destructive = true and start the server with
    --allow-destructive; even then nothing runs until you approve the plan yourself with
    `scull approve` in your own terminal.)
  * Only folders under your configured scan roots (scan.roots) can be inspected.
  * Your home folder is shown as `~` (mcp.redact_home = true). Set mcp.hash_names = true to
    hide project and folder names, and mcp.max_results to limit result sizes.
  * File contents and commit messages are never returned.
  Disable at any time with `[mcp] enabled = false` in scull's config file.";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-mcp");
    }
}
