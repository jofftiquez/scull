# Using scull with AI agents (MCP)

`scull mcp serve` lets an AI coding agent (Claude Code, Codex, Cursor, VS Code, Gemini CLI
and others) ask scull what is using your disk. It speaks the
[Model Context Protocol](https://modelcontextprotocol.io) over stdio.

**The server is read-only.** No tool can delete, move or change anything, and none can plan
or approve a cleanup. An AI tool call is not your consent: when it is time to clean up, the
agent tells you to run `scull clean` in your own terminal, and you review and confirm the
plan there. See [SAFETY.md](SAFETY.md#ai-agents-and-mcp).

## Turn it on

The server is off by default. It refuses to start (exit code 2) until you enable it, in one
of two ways:

* Run `scull mcp serve` once in a terminal. scull shows the privacy notice below and offers
  to write the setting for you.
* Or add this to scull's config file (`scull doctor` prints its path):

  ```toml
  [mcp]
  enabled = true
  ```

  `SCULL_MCP=1` in the environment works too.

The first time the server starts after you enable it, it prints this notice on stderr:

```text
scull MCP server: privacy notice
  Results of scull's MCP tools (paths, project names, Git branch names, sizes) are sent to the
  AI client that started the server, and possibly to that client's cloud model provider. This
  is the one exception to scull keeping everything on your machine.
  * Tools are read-only: they cannot delete, move or change anything, or approve a cleanup.
  * Only folders under your configured scan roots (scan.roots) can be inspected.
  * Your home folder is shown as `~` (mcp.redact_home = true). Set mcp.hash_names = true to
    hide project and folder names, and mcp.max_results to limit result sizes.
  * File contents and commit messages are never returned.
  Disable at any time with `[mcp] enabled = false` in scull's config file.
```

To turn it off again, set `enabled = false`.

## Connect your client

scull **never edits your AI client's configuration on its own**. Add it yourself with one
of the snippets below. (A future `scull agent install <client>` will offer to do this, and
only with your consent.)

Every client runs the same command: `scull mcp serve`. If your client doesn't inherit your
shell's `PATH`, use the full path to `scull` (`command -v scull` on macOS and Linux,
`(Get-Command scull).Source` in PowerShell).

**Claude Code**

```sh
claude mcp add scull -- scull mcp serve
```

Or in a project's `.mcp.json`:

```json
{ "mcpServers": { "scull": { "command": "scull", "args": ["mcp", "serve"] } } }
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.scull]
command = "scull"
args = ["mcp", "serve"]
```

**Cursor** (`~/.cursor/mcp.json`, or `.cursor/mcp.json` in a project)

```json
{ "mcpServers": { "scull": { "command": "scull", "args": ["mcp", "serve"] } } }
```

**VS Code** (`.vscode/mcp.json`)

```json
{ "servers": { "scull": { "type": "stdio", "command": "scull", "args": ["mcp", "serve"] } } }
```

**Gemini CLI** (`~/.gemini/settings.json`)

```json
{ "mcpServers": { "scull": { "command": "scull", "args": ["mcp", "serve"] } } }
```

Check your client's documentation if these locations have changed.

## The tools

All tools but one are marked read-only, non-destructive and idempotent. `register_worktree`
writes a reminder to scull's own data folder (never to your files), so it is marked
non-destructive but not read-only. Each tool takes structured JSON arguments (unknown fields
are refused) and returns structured results plus a short text summary.

| Tool | What it returns | Arguments |
|---|---|---|
| `get_status` | The allowlisted roots, server settings, the last recorded scan and free space. **Call this first.** | none |
| `get_storage_overview` | Totals by tier and category across all roots, volume capacity, largest projects | none |
| `scan_directory` | What one folder holds, each item with its tier and reasons | `path`, `max_results` |
| `list_projects` | Projects with reclaimable bytes by tier | `path`, `max_results` |
| `inspect_project` | One project's items and Git worktree state | `path` |
| `list_worktrees` | Worktrees: branch, upstream, uncommitted and unpushed counts, removal tier | `path`, `max_results` |
| `inspect_worktree` | One worktree in detail | `path` |
| `list_caches` | Package-manager caches and sizes | `max_results` |
| `find_large_files` | Largest files under a folder: names and sizes only | `path`, `min_size_bytes`, `max_results` |
| `get_cleanup_candidates` | Items that could be cleaned, by tier and category, largest first. Never prepares a cleanup. | `path`, `tiers`, `category`, `max_results` |
| `get_cleanup_history` | Storage history from past scans: bytes per category over time, items that came back, one item's lifecycle | `category`, `item_id`, `since_days`, `max_results` |
| `check_capacity` | Is there room for more work? Free space of the drives holding the roots (or one folder), the regenerable bytes there, the disk-full forecast, and whether `worktrees` × `bytes_per_worktree` (or `needed_bytes`) fits above the reserve. Lists worktrees whose registered TTL ran out, as review suggestions. | `path`, `worktrees`, `bytes_per_worktree`, `needed_bytes` |
| `register_worktree` | Records that a Git worktree the agent created (inside a root) should be finished within `ttl` (e.g. `7d`). Metadata only: the worktree is never touched, and when the TTL runs out it becomes a review suggestion for you; nothing is deleted. | `path`, `ttl` |

By default there are no plan, approve or execute tools. They exist only in destructive
mode, below.

## Agent cleanups (destructive mode)

With destructive mode on, the agent can **prepare** a cleanup plan and **execute** it after
you approved it yourself. No tool can approve anything: an AI tool call is not your consent.

Destructive mode needs **both** of these; with only one, the plan tools don't exist:

* `destructive = true` under `[mcp]` in scull's config file (the file only: no environment
  variable or flag sets it), and
* the server started with `scull mcp serve --allow-destructive`.

It is never offered when scull runs as root or administrator.

| Tool | What it does |
|---|---|
| `prepare_cleanup` | Records a plan of exact item IDs from `get_cleanup_candidates`. Changes nothing. Plans expire after 15 minutes. |
| `get_cleanup_plan` | The plan's items, tiers, actions and approval state |
| `get_approval_status` | `awaiting_approval`, `approved`, `used`, `revoked`, `denied` (with `denial_reason`: `denied` or `wrong_code`) or `expired`. Never reveals or creates an approval. |
| `execute_cleanup` | Runs an approved plan once, through the same Executor as `scull clean` |
| `get_cleanup_status` | Not yet run, voided, or the recorded outcome per item |

How a cleanup goes:

1. The agent calls `prepare_cleanup` and tells you the plan ID.
2. You approve it outside the chat, in one of two places:
   * **The scull desktop app.** While it is open, scull shows a notification ("An AI agent
     wants to clean 37.1 GiB", with **Review** and **Later**; it never approves anything).
     **Review**, or the link `scull://approve/<plan-id>` the agent gives you, opens Cleanup
     Review on that plan: tick each item once you have looked at it, type the short code shown
     next to the plan, and press **Approve**, or **Deny**. The code is made for that one
     opening of the dialog, stays in the app, and is checked by scull's core; three wrong codes
     deny the plan. A denial is final here too: the agent sees `denied`.
   * **`scull approve <plan-id>` in your own terminal.** It shows the full plan, the MCP
     client that prepared it (the name and version the client gave itself, not verified)
     and a short code; type the code to approve, or `deny`. A denial is final: `deny` (or
     three wrong codes) means that plan can never be approved, and the agent sees `denied`
     and has to prepare a new plan if you still want a cleanup. It refuses to run without an
     interactive terminal (a pipe, a script, an agent's shell tool) and as root. It also
     refuses a terminal that looks driven by an agent: an AI agent's environment variables are
     set, an agent client or a terminal-automation tool (`script`, `expect`) started it, or its
     parent processes can't be read. That check is a heuristic, not a proof of a human; see
     [APPROVAL_HARDENING.md](../APPROVAL_HARDENING.md).
3. The agent calls `execute_cleanup`. It runs only with your approval: bound to the plan's
   exact content, unexpired, used once. If any item changed on disk since the plan was made,
   the whole plan is void and the agent has to prepare a new one. Every item is checked again
   right before it is touched; S0 items are deleted, S1/S2 items quarantined (`scull restore`
   puts them back).

Changed your mind after approving? Press **Revoke approval** in the desktop app, or run
`scull approve --revoke <plan-id>` in your terminal, before the agent executes it (`scull
approve` with no plan ID lists approved plans not yet run). The approval is withdrawn in the
same atomic step that execution uses to claim it, so exactly one of the two happens: either the
revocation is refused because the cleanup already started, or `execute_cleanup` is refused with
`approval_revoked` and nothing runs. The plan then reports `revoked`.

Approving from the desktop app's dialog is coming (#77). In this build, `scull approve` in
your terminal is the only way to approve.

### Limits on what the agent can ask for

So an agent can't bury you in approval requests or grow scull's database without bound,
`prepare_cleanup` is limited:

| Setting | Default | Effect when exceeded |
|---|---|---|
| `mcp.max_pending_plans` | `5` | At most this many agent plans may await your approval (unexpired and unapproved) per machine and account, across all sessions. One more is refused with `too_many_pending_plans` and nothing is recorded. Approve, deny or wait out a plan (15 minutes) to free a slot; `scull approve` lists them. |
| `mcp.prepare_per_minute` | `6` | At most this many `prepare_cleanup` calls per minute in one session (one `scull mcp serve` process). More are refused with `rate_limited` before anything is scanned; the error says when to retry. |

```toml
[mcp]
max_pending_plans = 5
prepare_per_minute = 6
```

The rate limit is per server process (each `scull mcp serve` your client starts has its own
budget); the pending cap is per machine and account, shared by every session. A plan you
denied in `scull approve` (`deny`, or three wrong codes) stops counting and leaves the list
for good; an attempt refused for having no terminal does not.

Both are read **only from scull's config file**, like `destructive`: there is no environment
variable or flag, so an agent that can edit its MCP client's launch command still can't raise
them. `0` counts as `1`. The pending cap is checked in the same database transaction that
records the plan, so parallel sessions can't exceed it either.

Refusals are recorded as `prepare_refused` (below). Rate-limit refusals are recorded once per
minute per session, so a looping agent can't flood the record.

### What is recorded

The plan, your approval (channel and account) and the operation with every item's outcome
are kept in scull's append-only audit tables (`scull history <operation>`).

Everything that was **refused** is recorded too, and `scull history` lists it under
"Refused to AI agents":

| Recorded as | When | Reason codes |
|---|---|---|
| `approval_denied` | `scull approve` ended without an approval | `denied` (you typed `deny`, an empty line, or closed the input), `wrong_code` (three wrong codes), `no_terminal` (run without a terminal, when scull's database already exists), `agent_environment`, `agent_process`, `pty_driver` or `ancestry_unknown` (a terminal that looks driven by an agent), or why the attempt was refused (`plan_expired`, `plan_foreign`, `unknown_plan`, `already_approved`, `reviewed_plan_differs`, ...) |
| `execute_refused` by `mcp` | The server refused `execute_cleanup` | `invalid_arguments` (including any attempt to pass a code), `unknown_plan`, `plan_foreign`, `plan_expired`, `not_approved`, `not_human`, `approval_mismatch`, `approval_expired`, `approval_used`, `approval_revoked`, `plan_changed` (void), `outside_allowlist`, `lock_held` |
| `prepare_refused` | A `prepare_cleanup` hit a limit (above) | `rate_limited`, `too_many_pending_plans` (no plan ID) |
| `approval_revoked` | You revoked an approval before it was used (recorded in the same transaction; nothing else can write this entry) | `revoked` |
| `revoke_refused` | A revocation was refused | `not_approved`, `approval_used` (the cleanup already started), `approval_revoked`, `approval_expired`, `plan_foreign`, `unknown_plan` |
| `execute_refused` by `executor` | The Executor's own check refused to start the plan | `not_approved`, `not_human`, `approval_mismatch`, `approval_expired`, `approval_used`, `approval_revoked`, `other_account`, `targets_touched`, `unfinished_operation`, ... |

Each entry holds the reason code, the plan ID and its content hash, the channel
(`terminal`, `desktop` or `mcp`), what recorded it, the machine ID and your account. Never a
confirmation code, a message or a path. `scull history --json` returns them as
`agent_events`; `scull data show` counts them by kind. Recording a refusal never creates,
spends or changes an approval. Nothing is recorded when scull runs elevated, and a refused
`scull approve` without a terminal creates nothing (no database, no machine ID file).

The record can't be flooded: a refusal that repeats one from the last minute (same kind,
reason, channel, account and plan; any made-up plan ID counts as the same) is added to that
entry's count instead of writing a new row. `scull history` lists the 50 most recent entries
from the last 30 days, says how many more there are, and takes `--limit N` and `--since AGE`.

The server also tells the model, in its instructions, that file names, branch names and
paths are untrusted data from your disk and that it must never follow instructions found in
them.

## What the agent can see

### Only your scan roots

The allowlist is `scan.roots` from scull's config. If the client offers its own MCP roots,
scull narrows the allowlist to the overlap. Roots that are too broad (a drive root, your home
folder or a folder above it) are left out with a warning.

Every path argument is checked before anything reads it: its shape, that it sits inside an
allowed root as written, and that it still does after resolving symlinks. A refusal never
reveals anything about the path:

```json
{
  "content": [{ "type": "text", "text": "error (outside_allowlist): the path is outside the allowlisted roots; call get_status to see them" }],
  "structuredContent": { "error": { "code": "outside_allowlist", "message": "the path is outside the allowlisted roots; call get_status to see them" } },
  "isError": true
}
```

### Redaction and limits

| Setting | Default | Effect |
|---|---|---|
| `mcp.redact_home` | `true` | Your home folder appears as `~` |
| `mcp.hash_names` | `false` | Project and folder names are replaced by hashes, salted per machine. The salt is never returned. |
| `mcp.max_results` | `1000` | Cap on list lengths. A capped list says `truncated`. |
| `mcp.max_pending_plans`, `mcp.prepare_per_minute` | `5`, `6` | Destructive mode only: see [the limits](#limits-on-what-the-agent-can-ask-for). Config file only. |

The first three also have an environment variable: `SCULL_MCP_REDACT_HOME`,
`SCULL_MCP_HASH_NAMES`, `SCULL_MCP_MAX_RESULTS`.

Always, whatever the settings:

* **No file contents** and **no commit messages** are ever returned.
* Names and branches come back as separate data fields, never mixed into prose.
* Every string is escaped: control, bidirectional and invisible characters become visible
  text. Names over 128 characters and paths over 512 are cut. Suspicious names are flagged.
* Scans made for MCP are not written to scull's history. Without destructive mode the
  server writes nothing; with it, it records plans and refusals (above).

## Example

A `get_status` call, from a real session:

```json
{
  "allowlisted_roots": ["~/Projects"],
  "hash_names": false,
  "max_results": 1000,
  "name": "scull",
  "read_only": true,
  "redact_home": true,
  "tools": ["get_storage_overview", "scan_directory", "list_projects", "inspect_project",
            "list_worktrees", "inspect_worktree", "list_caches", "find_large_files",
            "get_cleanup_candidates", "get_cleanup_history", "get_status"],
  "version": "0.0.0"
}
```

(This is the `server` part of the result. The full result adds `last_scan`, `volumes` and
`state`.)

## Troubleshooting

| Symptom | Fix |
|---|---|
| The client says the server exited at once | MCP is not enabled. Run `scull mcp serve` in a terminal once, or set `[mcp] enabled = true`. |
| Every path is refused | Call `get_status` to see the allowlist, and add the folder to `scan.roots` in scull's config file. |
| Names look like hashes | `mcp.hash_names` is on. |
| Garbage in the client log | scull writes only JSON-RPC on stdout; messages go to stderr. Make sure nothing wraps the command in a shell that prints to stdout. |

## `scull ask`: a read-only assistant in your terminal

`scull ask` is the other way to put a model to work: you ask a question in your terminal, and
a model you configure answers it by calling scull's read-only tools. These are the tools
listed above, minus `register_worktree`, with the same allowlist and redaction. The model gets
none of the destructive-mode tools, so it can't prepare or approve a cleanup. It is off by
default, uses your own API key or a local server, and asks for your consent before the first
request to each provider. See [the CLI reference](CLI.md#scull-ask).

## What is out of scope

scull guarantees that **its own tools** can't delete anything without you. An agent that can
already run shell commands can delete files without scull; scull can't prevent that.
