# Remote (SSH) scanning: exploration

P6-RES-01 (#26). Decision D-4 keeps remote scanning out of the MVP and v1.0; this document
records what was explored, what is recommended, the safety model any remote mode must follow,
and the small prototype that ships behind a flag (`scull scan --ssh HOST:PATH`).

**Outcome in one paragraph.** Run the remote host's own `scull scan --json` over the system
`ssh` client and bring back only the JSON report. That is the only approach that keeps every
safety signal (physical sizes, hardlinks, Git state, in-use detection, protected roots), costs
one round trip, and adds no network code, crypto or credentials handling to scull. Reading the
remote disk over SFTP was rejected: it is one to two orders of magnitude slower on real
latencies and loses most of the evidence the tiers depend on, so almost everything would
classify as S2 (uncertain). Uploading a helper binary is possible later as an explicit
"bootstrap" step, but it is intrusive and needs its own review. Remote mode stays
**inspect-only**: nothing in this phase can delete, move or change anything on a remote host.

## 1. Who and why

PRD §3 lists remote-server developers (CLI over SSH) as a secondary persona: build servers,
cloud dev boxes and lab machines that fill up with build outputs, caches and stale worktrees.
Today they can already SSH in and run `scull` there. Remote scanning would let one terminal
inspect several hosts and, later, let the desktop app show a remote host next to local drives.

## 2. Constraints

- **Read-only by construction.** No remote deletion, quarantine or restore in this phase. A
  remote report never feeds a local plan: its paths are not local paths.
- **No new network stack.** scull makes no network calls of its own (PRD §20). A remote mode
  must reuse the user's SSH client, configuration, keys and `known_hosts`, and never handle a
  password, private key or agent socket itself.
- **Same schemas.** Remote results use the published `scull/scan@1` document, so every
  consumer (CLI, MCP, desktop) understands them and validation is possible.
- **Untrusted output.** Everything that comes back (paths, names, error text) is attacker-
  controlled if the host is compromised; it is validated, size-capped and escaped, and never
  drives a local file operation.
- **Platform boundary (PLT-11).** Spawning `ssh` uses `std::process` (portable, no shell), like
  the hardened Git runner; no OS-specific code outside `scull-platform`.

## 3. Approaches compared

### A. Remote scull over SSH (recommended)

`ssh HOST -- scull scan --json --no-history -- '<path>'`, read stdout, validate, render.

- **Fidelity:** full. The engine runs where the files are: allocated sizes, hardlink and
  clone accounting, file identity, mount and network-volume detection, the hardened Git
  analysis, in-use detection, protected roots and broad-root refusal all apply on the host.
- **Transfer:** only the report. Measured locally (pretty-printed `scan --json`): about 2.4 KB
  per candidate (path, sizes, evidence, reasons) plus the project, worktree and issue lists; a
  folder with 95 candidates and dozens of Git worktrees produced a 2.1 MB document. JSON
  compresses well if the user's SSH config enables compression. The walk itself never crosses
  the network.
- **Latency:** one connection plus the scan time on the host.
- **Requirements:** scull installed on the host (static musl builds exist for x86_64 and
  aarch64 Linux), reachable on the non-interactive `PATH` or named with `--remote-bin`, with a
  matching schema major version (validation rejects anything else).
- **Cost to build:** small (the prototype is about 570 lines including unit tests, after formatting).

### B. Agent-less, over SFTP

Walk the remote tree with SFTP `readdir`/`lstat` and classify locally.

- **Fidelity:** poor. SFTP v3 attributes have no allocated size (`st_blocks`), no device or
  inode (so no hardlink de-duplication or file identity), no mount information. There is no way
  to run the hardened Git analysis (reading `.git` over SFTP and reimplementing status safely is
  not realistic), no in-use detection, no activity signals beyond mtimes. Most candidates would
  fall to S2 (uncertain) and reclaim estimates would be logical sizes only, which is exactly
  the over-promising the PRD forbids (PHY-01).
- **Speed:** one round trip per directory at least, plus per-file stats. With 200,000
  directories and a 30 ms RTT, an unpipelined walk is about 100 minutes before any file
  attributes; pipelining helps but stays far behind a local walk (the same tree scans in under
  a minute on the host).
- **Cost:** an in-process SSH implementation (crypto, key exchange, host-key verification,
  agent and config handling) or scripting the `sftp` batch client. Either way a large new
  attack surface for a safety tool.

Rejected.

### C. Upload a small helper

Copy a static helper (or `scull` itself) to a temporary directory, run it, delete it.

- **Fidelity:** as A.
- **Downsides:** writes and executes a binary on someone's server (often against policy;
  `/tmp` may be `noexec`), needs architecture detection, integrity checks (checksums,
  signatures) and clean-up that still leaves traces. A failed clean-up leaves an executable
  behind.
- **Verdict:** possible later as an explicit, opt-in `--bootstrap` that installs a verified
  release into a user-chosen directory and says so. It is A plus an installer, not a different
  architecture.

### D. Mount and scan locally (sshfs, rclone mount)

Works today with `--allow-network-roots`. It has B's fidelity and speed problems (FUSE hides
allocated sizes and identities), which is why network and FUSE volumes are skipped by default
(D-22). Documented, not recommended.

| | A. remote scull | B. SFTP | C. helper upload | D. mount |
|---|---|---|---|---|
| Tier fidelity | full | poor (mostly S2) | full | poor |
| Physical sizes, hardlinks | yes | no | yes | no |
| Git analysis | yes (on host) | no | yes | slow, partial |
| Data crossing the network | report only | every directory | report + binary | every stat |
| Needs scull on the host | yes | no | uploaded | no |
| New code in scull | small | large (SSH stack) | medium | none |
| New attack surface | spawn `ssh` | SSH/crypto in-process | remote execution of uploads | FUSE |

## 4. Safety model (any remote mode)

1. **Inspect-only.** scull builds the remote command itself from a fixed allowlist (today only
   `scan --json --no-history`); no user string is ever run as a command. There is no remote
   `clean`, plan, restore or approval. A remote report cannot be turned into a local plan
   (`scull clean --plan` only accepts plans scull wrote for local roots), and the MCP server
   and `scull ask` do not see remote results. If remote cleanup ever comes, it runs the remote
   host's own scull in an interactive terminal there (`ssh -t host scull clean`), with that
   host's approval flow and safety gates.
2. **Nothing written remotely.** `--no-history` keeps the remote history database untouched.
   Nothing is uploaded. Nothing is recorded in the local history either.
3. **Host keys are verified, never accepted implicitly.** The system client runs with
   `StrictHostKeyChecking=yes` and `BatchMode=yes`: an unknown or changed host key fails the
   scan, and the user verifies the host once with plain `ssh` (their own `known_hosts`).
   `UpdateHostKeys=no` keeps scull from rewriting it.
4. **No forwarding.** `ForwardAgent=no`, `ForwardX11=no`, `ClearAllForwardings=yes`,
   `PermitLocalCommand=no`, `RequestTTY=no`, `-T`, and escape character `none`. The user's agent
   may authenticate the connection (that is the client's normal behavior) but its socket is
   never forwarded to the host. scull never reads keys, passwords or `SSH_AUTH_SOCK` itself.
5. **Trusted `ssh` binary.** Resolved like `git` (SAFE-12): absolute `PATH` entries only, never
   the current directory, relative entries or `node_modules`; spawned by absolute path with no
   shell. The user's `~/.ssh/config` still applies (aliases, `ProxyJump`, keys), which is the
   point of using their client.
6. **Quoting.** The host is validated (no leading `-`, whitespace, control or quote
   characters) and passed after `--`. Remote arguments are single-quoted for the remote POSIX
   shell; a leading `~/` is left unquoted so the remote shell expands it. Windows hosts
   (OpenSSH with cmd/PowerShell as the remote shell) are not supported by the prototype.
7. **Untrusted output.** stdout is capped (64 MiB), the whole run has a timeout, the document
   must validate against the published `scan.schema.json` (`schema` = `scull/scan@1`), and
   candidates are parsed with unknown fields rejected. Every string shown is escaped for the
   terminal (SAFE-17), and remote paths are shown as `HOST:/path` so they are never mistaken
   for local ones. Remote stderr is shown escaped and truncated.
8. **Redaction.** The prototype shows remote results only to the local user, so it prints
   them as they are (the remote home folder is not hidden). Before remote results ever reach
   an AI client (MCP, `scull ask`) they must go through the same redaction as local ones
   (MCP-05): the remote home prefix shown as `~` (learned from the remote scull, never
   guessed), host names hashed when `mcp.hash_names` is on, and the host allowlisted in config.
9. **Broad roots stay refused on the host.** The remote scull applies its own CFG-04 check
   (filesystem root, home folder), and the prototype offers no way to override it.

## 5. Performance notes

- The scan runs at the host's local disk speed; the network carries one report.
- Large reports: `--ndjson` over SSH would stream candidates as they are found and give
  progress; a later version can validate each line against `scan.ndjson.schema.json`. The
  prototype uses `--json` (one document) for simplicity.
- Compression: leave it to the user's SSH config (`Compression yes`) rather than forcing `-C`;
  on fast links compression costs more CPU than it saves.
- Cancellation: Ctrl-C terminates the local `ssh`, which closes the channel; the remote scull
  receives SIGHUP and stops (its partial report is discarded).
- Multiple hosts: run one `ssh` per host in parallel; connection multiplexing
  (`ControlMaster`) from the user's config makes repeated runs cheap.

## 6. Prototype: `scull scan --ssh HOST:PATH`

```sh
scull scan --ssh build-box:~/src            # human report, paths shown as build-box:/home/...
scull scan --ssh me@10.0.0.5:/srv/ci --json # the validated remote document, unchanged
scull scan --ssh build-box:~/src --remote-bin ~/.cargo/bin/scull
```

What it does:

1. Parses `HOST:PATH` (`[v6addr]:PATH` for IPv6 literals) and validates both parts.
2. Resolves a trusted `ssh` binary from `PATH`.
3. Runs `ssh -T -e none -o BatchMode=yes -o StrictHostKeyChecking=yes -o UpdateHostKeys=no
   -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes -o PermitLocalCommand=no
   -o RequestTTY=no -o ConnectTimeout=15 -- HOST 'scull' 'scan' '--json' '--no-history' '--' 'PATH'`
   with stdin closed.
4. Caps stdout, enforces a timeout, validates the document against the published schema, then
   prints it (`--json`) or renders the same tables as a local scan with remote paths prefixed
   by the host.
5. Exit codes: SSH failures (exit 255: unknown host key, authentication, connection) are I/O
   errors with SSH's message; a remote scull error is shown with its message; an invalid
   document is refused.

Limitations: key or agent authentication only (BatchMode, no password prompts); POSIX remote
shells only; `--ndjson` is refused with `--ssh`; a remote scull with a different schema major
version is refused; no remote history.

Tests run against a fake `ssh` shim that records its arguments and runs the local scull binary
through `sh -c`, exactly as a remote POSIX shell would, over generated fixtures. No real host is
contacted.

## 7. Recommendation and next steps

- Keep D-4: no remote scanning in v1.0. The prototype stays a documented, inspect-only
  preview flag.
- After v1.0, if users ask: stream `--ndjson` with progress; `--ssh` for `projects`,
  `worktrees` and `caches`; a `[remote]` config section with an allowlist of hosts; remote home
  redaction support in `scull scan --json`; desktop "Add a remote host" using the same command.
- Remote cleanup, if ever: only by running the host's own scull interactively there.
