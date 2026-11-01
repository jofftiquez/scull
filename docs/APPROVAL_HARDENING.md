# Hardening `scull approve` against agents that drive a terminal

Design note for #35 (threat T-41, residual). Status: the ancestry and environment checks
below are implemented; the other options are evaluated and not built.

## The problem

An AI agent may prepare a cleanup plan through scull's MCP server, but only a human may
approve it (PRD §13.3). `scull approve <plan-id>` already refuses unless stdin, stdout and
stderr are all terminals, and asks the human to type a random code shown next to the full
plan (FX-MCP-08). That stops pipes, scripts and agent shell tools without a terminal.

It does not stop a process that **drives a pseudo-terminal** on the same host: `script`,
`expect`, `unbuffer`, or `tmux new-session` + `send-keys`. They give `scull approve` a real
terminal, read the code from the screen and type it back. Such an agent has unrestricted
shell access and can delete files without scull, which PRD §13.4 puts out of scope. The goal
here is to raise the bar where that is cheap and portable, without claiming more than is
true.

## What is implemented

Before showing anything, `scull approve <plan-id>` refuses (exit 5, `safety_blocked`,
audited as `approval_denied` with the reason below, nothing shown or approved) when:

| Reason | Signal |
|---|---|
| `agent_environment` | An environment variable AI agent clients set for the commands they run: `CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`, `CODEX_SANDBOX`, `CODEX_SANDBOX_NETWORK_DISABLED`, `GEMINI_CLI`. It is inherited by everything the agent starts, including a `tmux` server and its panes. |
| `agent_process` | An AI agent client is among the parent processes: `claude`, `codex`, `gemini`, `aider`, `goose`, `opencode`, `cursor-agent`, `copilot`. Both the OS's process name and the executable's file name are compared. |
| `pty_driver` | A tool that drives a pseudo-terminal programmatically is among the parent processes: `expect`, `unbuffer`, `script`, `empty`. Nobody needs one to approve in their own terminal. |
| `ancestry_unknown` | The parent processes can't be read at all. It fails closed. |

The lists live in `scull_mcp::approval` (`AGENT_ENV_VARS`, `AGENT_PROCESSES`,
`PTY_DRIVERS`, `check_approver`). The process chain comes from
`scull_platform::ancestry`. Terminal multiplexers (`tmux`, `screen`) are **not** refused on
their own, because people use them every day. A multiplexer session an agent started is
caught by the inherited environment.

Fixtures: FX-MCP-11 (`scull approve` under `script`), FX-MCP-12 (agent environment in a
`tmux` session), FX-MCP-13 (an agent process among the parents). The existing real
pseudo-terminal test (a human typing the code in `tmux`) still approves, so there is no
regression for a real user terminal.

## Options compared

| Option | Linux | macOS | Windows | Stops | Does not stop | Decision |
|---|---|---|---|---|---|---|
| **Process ancestry** (agent clients, PTY drivers above `scull approve`) | `/proc/<pid>/stat`, `/proc/<pid>/exe` | `proc_pidinfo(PROC_PIDTBSDINFO)`, `proc_pidpath` | `NtQueryInformationProcess`, `QueryFullProcessImageNameW`; a parent created after its child is a reused PID and ends the chain | An agent's shell tool with a terminal; `script`/`expect`/`unbuffer` run by an agent; an agent running approve as a direct child | Renamed tools; a daemonized multiplexer (its server is re-parented to init, which cuts the chain); an agent that re-parents itself; Python `pexpect` (the process is `python`) | **Built** (signal, not a guarantee) |
| **Agent environment markers** | env | env | env | `tmux`/`screen` sessions and anything else started from an agent's shell, as long as the agent doesn't scrub its environment | An agent that runs `env -u ...` first; agents that set no marker | **Built** |
| **Read the code from the controlling terminal** (`/dev/tty`, `CONIN$`) instead of stdin | `open("/dev/tty")` | same | `CONIN$` | A redirected stdin while stdout is a terminal (already refused: all three streams must be terminals) | Any PTY driver: it *is* the controlling terminal | Not built: no added protection |
| **Session / login checks** (the terminal belongs to the console user's login session) | `loginctl`, utmp | console user (`SCDynamicStore`), utmp | `WTSGetActiveConsoleSessionId`, session of the process | A process in another user's or a remote session | The agent: it runs as the same user, in the same login session. Many legitimate terminals (VTE, `tmux`) don't register in utmp, so a utmp check would refuse real users. | Not built: wrong boundary |
| **Out-of-process approval** (the desktop Cleanup Review dialog, #16; `scull approve` prints a deep link) | desktop app | desktop app | desktop app | Everything that only controls a terminal | An agent that can drive the GUI (accessibility APIs, `xdotool`) or write the database | Preferred surface once #16 lands. The CLI stays as a fallback. |
| **Authenticated approval rows** (a MAC keyed by a secret the MCP server can't read) | No per-app secret store: any process of the user can read the key | Keychain ACLs are per application, but `scull approve` and `scull mcp serve` are the same binary | DPAPI is per user, so the same user can decrypt | Rows inserted by a process that has the database but not the key | The agent: same user, same binary. It can read the key or call the approving code itself. | Not built: needs a separately signed approval helper or the desktop app holding the key. Revisit with #16. |

## What this does not change

* The code shows that someone reads the screen the plan is on; it is not a proof of a human.
* Anything that can write the user's database can still insert an approval row (T-41, second
  residual). The Executor checks the approver is the OS account it runs as, and that the
  approval is bound to the plan's hash, unexpired and unused, but rows are not authenticated.
* An agent with unrestricted shell access is out of scope (PRD §13.4): it can delete files
  without scull. These checks make scull's own approval path refuse the easy routes and say
  why.

## False positives and the way out

A human whose terminal genuinely runs under one of these tools (for example a recording made
with `script`) gets a clear refusal naming the tool and the reason code. Running
`scull approve` in a plain terminal works. There is deliberately **no override flag or
setting**: an agent could set either. Once #16 lands, the desktop dialog is the alternative.
