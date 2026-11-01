# scull CLI reference

`scull` is a single command with subcommands. This page lists every one, its flags, its exit
codes and its machine-readable output. `scull <command> --help` prints the same flags.

Examples come from real runs of a development build (version `0.0.0`) against a small demo
tree. Paths are shortened so the demo folder reads as `/home/me`, and long output is cut
where you see `...`. In that sandbox scull could not inspect some processes, so it
could not prove the folders were idle and rated them **S2 uncertain**. That is the safety
model working: missing evidence counts as risk (see [SAFETY.md](SAFETY.md)). On a normal
machine the same folders are usually S0.

* [Global options](#global-options)
* [Output, JSON and NDJSON](#output-json-and-ndjson)
* [Exit codes](#exit-codes)
* [Commands at a glance](#commands-at-a-glance)
* [Inspect](#inspect): `scan`, `projects`, `worktrees`, `caches`, `large-files`, `status`
* [Clean up](#clean-up): `clean`, `clean caches`, `clean --purge-quarantine`, `history`,
  `restore`
* [Set up and maintain](#set-up-and-maintain): `init`, `doctor`, `rules`, `data`,
  `completions`, `mcp serve`
* [More commands](#more-commands)
* [Configuration and environment](#configuration-and-environment)
* [JSON Schemas](#json-schemas)

## Global options

Every command accepts these.

| Flag | Effect |
|---|---|
| `--json` | Write one JSON document to stdout |
| `--ndjson` | Write newline-delimited JSON records to stdout, one object per line |
| `-q`, `--quiet` | Hide progress and informational messages. Errors still show. |
| `--color auto\|always\|never` | When to color. `auto` (the default) colors a terminal unless `NO_COLOR` is set or `TERM=dumb`. |
| `--no-progress` | Don't show progress on stderr |
| `-v`, `--verbose` | More diagnostics on stderr; repeat for more |
| `-h`, `--help` | Help (`-h` for a summary) |
| `-V`, `--version` | Version and target, e.g. `scull 0.0.0 (x86_64-unknown-linux-gnu)` |

Commands that scan also take:

| Flag | Effect |
|---|---|
| `--allow-broad-roots` | Allow a drive root, your home folder, or a folder that contains it. Refused by default. |
| `--allow-network-roots` | Allow scan roots on network or virtual volumes (NFS, SMB, FUSE, tmpfs, overlay). Skipped by default. Backup volumes are never scanned. |

When you give no folder, scull scans `scan.roots` from the config file (`scull init` sets
it).

## Output, JSON and NDJSON

* **Data goes to stdout; progress and errors go to stderr.** You can pipe stdout safely.
* Terminal output escapes control and bidirectional characters in names and flags suspicious
  names.
* Sizes are labeled **logical** (file length), **allocated** (space on disk) or
  **reclaimable** (what removing it would free, with a confidence level).

### JSON

`--json` writes one document with a versioned envelope:

```json
{
  "schema": "scull/scan@1",
  "version": "0.0.0",
  "data": { "...": "command-specific" }
}
```

`schema` names the document type and its major version. New fields can appear within a major
version; removing or changing a field needs a new major version.

### NDJSON

`--ndjson` writes one JSON object per line, each with the same envelope plus a `type`:

* **List commands** (`scan`, `projects`, `projects inspect`, `worktrees`, `worktrees
  inspect`, `caches`, `large-files`) stream one record per item, then one `summary` record.
* **Report commands** (`clean`, `clean caches`, `history`, `restore`, `status`, `doctor`,
  `rules`, `data`, `init`) write a single record whose `type` names the report.

```console
$ scull scan ~/Projects --ndjson
{"schema":"scull/scan@1","version":"0.0.0","type":"candidate","data":{"id":"730c5895419441dfc34c60f5d34cd9fe","path":"/home/me/Projects/api-server/target",...}}
{"schema":"scull/scan@1","version":"0.0.0","type":"candidate","data":{"id":"b2e46b93da2ed5e6bdf1a48dabb0a734","path":"/home/me/Projects/web-app/node_modules",...}}
{"schema":"scull/scan@1","version":"0.0.0","type":"candidate","data":{"id":"391fa8de23d265361250c39be663d21c","path":"/home/me/Projects/sketch/node_modules",...}}
{"schema":"scull/scan@1","version":"0.0.0","type":"summary","data":{"scan_id":"scan_6ae653aaac4fc3f2d48b2e9c","roots":["/home/me/Projects"],"candidate_count":3,...}}
```

### Errors

In `--json` or `--ndjson` mode, an error is one JSON line on **stderr**, and stdout stays
empty:

```json
{"schema":"scull/error@1","version":"0.0.0","error":{"kind":"plan_tampered","message":"plan content does not match its hash; the plan file was edited"}}
```

`error` has `kind` (below), `message`, and sometimes `path` or `item_id`. Without `--json`
the same error reads `scull: error: <message>`.

## Exit codes

| Code | Meaning | Error `kind` |
|---|---|---|
| 0 | Success | — |
| 1 | General error, including commands not built yet | `io`, `permission_denied`, `not_found`, `unsupported_platform`, `internal` |
| 2 | Usage or configuration error | `usage`, `config` |
| 3 | Finished, but some items failed or were skipped (see the report) | `partial_failure` |
| 4 | Plan rejected: expired, edited, from another machine or user, or invalidated | `plan_expired`, `plan_tampered`, `plan_foreign`, `plan_invalidated` |
| 5 | Blocked by safety policy (e.g. a permanent delete without `--allow-permanent`, or running as root) | `safety_blocked` |
| 6 | Another cleanup holds the execution lock | `lock_held` |
| 130 | Cancelled with Ctrl-C | `cancelled` |

Ctrl-C stops before the next item. Nothing is left half-done.

## Commands at a glance

| Command | What it does | Changes disk? |
|---|---|---|
| `scull scan [PATH]...` | Find reclaimable developer storage, with tiers and reasons | No |
| `scull projects [inspect PATH]` | List detected projects, or show one | No |
| `scull worktrees [inspect PATH]` | List Git worktrees and their state, or show one | No |
| `scull caches` | List package-manager caches and their sizes | No |
| `scull large-files PATH` | List large files (never cleaned) | No |
| `scull status [--short]` | One cached line, e.g. for your prompt | No |
| `scull clean` | Plan a cleanup (dry run), review a plan, or run it | Only with `--plan` and confirmation |
| `scull clean caches` | Quick Clean: run package managers' own cache commands | Only with confirmation |
| `scull clean --purge-quarantine` | Permanently delete expired quarantine | Only with elevated confirmation |
| `scull approve [PLAN_ID]` | Approve (or `--revoke`) a cleanup plan an AI agent prepared | Records the approval; the agent's run changes the disk |
| `scull history [OPERATION]` | Show past cleanups | No |
| `scull restore OPERATION` | Put quarantined items back | Only with confirmation |
| `scull init` | First-run setup: scan roots and profile | Writes scull's config |
| `scull doctor` | What this machine supports; where files live | No |
| `scull rules list\|validate` | Detection rules | No |
| `scull data show\|export\|purge` | What scull has recorded; export or delete it | `export` writes files; `purge` deletes scull's own files |
| `scull headroom status\|create\|release` | Emergency reserve file, released when the disk is full (off by default) | `create` writes it; `release` deletes it (only that file) |
| `scull check` | Light scan, free-space forecast, alerts, a ready-to-review plan | Records history and writes a plan file; never deletes |
| `scull schedule enable\|disable\|status` | Run `scull check` periodically through the OS scheduler | `enable`/`disable` write or remove scheduler files, with consent |
| `scull insights [export\|purge]` | Local trends from scull's own history | `purge` deletes history rows only |
| `scull ask QUESTION` | Optional assistant over scull's read-only tools (off by default; your own model) | No |
| `scull completions SHELL` | Shell completion script | No |
| `scull mcp serve` | Read-only MCP server for AI agents | No |

## Inspect

These commands only read. Scans are recorded in scull's local history database so that
`status` and trends work; pass `--no-history` to `scan` to skip that.

### `scull scan`

```text
scull scan [OPTIONS] [PATH]...
```

Scans folders for build outputs, dependency folders, caches and worktrees. Each item gets a
tier, sizes and the reasons.

| Flag | Effect |
|---|---|
| `PATH...` | Folders to scan (default: `scan.roots`) |
| `--no-history` | Don't record this scan in the local database |
| `--allow-broad-roots`, `--allow-network-roots` | See [global options](#global-options) |

```console
$ scull scan ~/Projects
Scanned /home/me/Projects (24 entries)

Tier          Reclaimable  Allocated    Logical  Confidence  Artifact                    Path                  Why
S2 uncertain      4.0 MiB    4.0 MiB    4.0 MiB  high        Cargo build output          api-server/target     could not check whether it is in use; not treated as idle
S2 uncertain      1.9 MiB    1.9 MiB    1.9 MiB  high        Installed npm dependencies  web-app/node_modules  could not check whether it is in use; not treated as idle
S2 uncertain    500.0 KiB  500.0 KiB  496.3 KiB  high        Installed npm dependencies  sketch/node_modules   could not check whether it is in use; not treated as idle

Totals by tier:
Tier            Items  Reclaimable  Allocated  Logical
S0 regenerable      0          0 B        0 B      0 B
S1 costly           0          0 B        0 B      0 B
S2 uncertain        3      6.4 MiB    6.4 MiB  6.4 MiB
S3 protected        0          0 B        0 B      0 B
total               3      6.4 MiB    6.4 MiB  6.4 MiB

Up to 6.4 MiB reclaimable in S0-S2 (0 B preselectable as S0 regenerable). Nothing was changed: scan is read-only.

Signals unavailable (affected items are classified as if the answer were risky):
  process cwd (3 item(s)): 18 of your own processes could not be inspected (permissions)
  open file (3 item(s)): 2 of your own processes could not be inspected (permissions)

Scan ID scan_6ae6537ed5433fc0ef023c10 - history: recorded (3 new, 0 gone)
```

JSON: `scull/scan@1`. `data` holds `scan_id`, `started_at`, `finished_at`, `roots`,
`cancelled`, `candidates[]`, `projects[]`, `worktrees[]`, `worktree_problems[]`, `totals`
(count, logical, allocated, reclaimable per tier), `entries_scanned`, `issues[]`,
`signals_unavailable[]`, `config_warnings[]` and `history`. One candidate:

```json
{
  "id": "730c5895419441dfc34c60f5d34cd9fe",
  "path": "/home/me/Projects/api-server/target",
  "identity": { "volume": 2049, "file": "6c00ed" },
  "category": "project_artifact",
  "artifact": "Cargo build output",
  "rule": { "id": "rust.target", "version": 2 },
  "project": { "root": "/home/me/Projects/api-server", "name": "api-server", "ecosystem": "rust" },
  "tier": "S2",
  "reasons": [
    { "tier": "S2", "code": "signal_unavailable", "message": "could not check whether it is in use; not treated as idle" }
  ],
  "evidence": [
    { "kind": "marker", "path": "/home/me/Projects/api-server/Cargo.toml" },
    "..."
  ],
  "sizes": {
    "logical": 4212332,
    "allocated": 4218880,
    "reclaimable": { "bytes": 4218880, "confidence": "high", "shared_bytes": 0 }
  },
  "flags": { "active": null, "hardlinked": false, "cross_mount": false, "symlink": null, "cloud_placeholder": false },
  "regeneration": { "instructions": "Run `cargo build` in the project to rebuild.", "cost": { "type": "unknown" } },
  "action": { "type": "quarantine" }
}
```

A `null` flag means "not checked". The candidate shape is published as
[`schemas/v1/candidate.schema.json`](../../schemas/v1/candidate.schema.json).

### `scull projects`

```text
scull projects [--root PATH]...
scull projects inspect [--root PATH]... PATH
```

Lists detected projects with their reclaimable bytes and badge (the most protective tier
among their items). `inspect` shows one project's items in full. `--root` replaces
`scan.roots`; repeat it for several.

```console
$ scull projects
Projects under /home/me/Projects

Project     Ecosystems  Items  Reclaimable  Badge         Path
api-server  rust            1      4.0 MiB  S2 uncertain  api-server
sketch      node            1    500.0 KiB  S2 uncertain  sketch
web-app     node            1      1.9 MiB  S2 uncertain  web-app

3 project(s). Badge: most protective tier among a project's items.
```

```console
$ scull projects inspect ~/Projects/web-app
Project web-app
  path:        /home/me/Projects/web-app
  ecosystems:  node
  markers:     package.json
  size:        1.9 MiB allocated

S2 uncertain  /home/me/Projects/web-app/node_modules
  artifact:     Installed npm dependencies (rule node.node-modules v2)
  project:      web-app (/home/me/Projects/web-app)
  sizes:        1.9 MiB logical, 1.9 MiB allocated, 1.9 MiB reclaimable (high confidence)
  action:       quarantine (Restorable)
  why:
    S2 could not check whether it is in use; not treated as idle
  evidence:
    marker /home/me/Projects/web-app/package.json
    lockfile /home/me/Projects/web-app/package-lock.json
    known artifact: Installed npm dependencies
    ...
  regenerate:   Reinstall with your package manager (`npm ci`, `pnpm install --frozen-lockfile`, ...).
```

JSON: `scull/projects@1` (NDJSON: `project` records, then `summary`) and
`scull/project@1` (NDJSON: `candidate` records, then `summary`).

### `scull worktrees`

```text
scull worktrees [--root PATH]...
scull worktrees inspect [--root PATH]... PATH
```

Lists Git worktrees found under the roots, read from Git's own metadata: branch, upstream,
lock, uncommitted and untracked changes, unpushed commits, merge status, active use, and the
tier of removing each. Git runs with hooks, fsmonitor and fetches disabled. `inspect` shows
one worktree.

```console
$ scull worktrees
Git worktrees under /home/me/Projects

No Git worktrees found.
```

JSON: `scull/worktrees@1` (NDJSON: `worktree` records, then `summary`) and `scull/worktree@1`
(NDJSON: one `worktree` record).

### `scull caches`

```text
scull caches
```

Locates package-manager caches in their usual places and measures them. It also shows the
command `scull clean caches` would run, and whether the tier allows it.

```console
$ scull caches
Tier          Reclaimable  Allocated    Logical  Confidence  Artifact   Path                    Why
S2 uncertain    800.0 KiB  800.0 KiB  797.2 KiB  high        npm cache  /home/me/.npm/_cacache  could not check whether it is in use; not treated as idle

Cleanup commands (run by `scull clean caches`, never by this command):
  npm cache: npm cache verify (not offered at this tier)

Totals by tier:
...
```

JSON: `scull/caches@1` (NDJSON: `candidate` records, then `summary`).

### `scull large-files`

```text
scull large-files [--min-size SIZE] [--top N] PATH
```

Lists the largest files under a folder for you to review. scull never cleans them.

| Flag | Effect |
|---|---|
| `--min-size SIZE` | Smallest allocated size to list, in binary units (`500M`, `1.5G`, `2GiB`). Default `500MiB`. |
| `--top N` | List at most N files. Default 50. |

```console
$ scull large-files ~/Projects --min-size 1M
Allocated  Logical  Kind        Modified    Path
  2.9 MiB  2.9 MiB  Large file  2026-10-21  /home/me/Projects/api-server/target/debug/deps/libbig.rlib
  1.9 MiB  1.9 MiB  Large file  2026-10-21  /home/me/Projects/web-app/node_modules/left-pad/index.js
  1.1 MiB  1.1 MiB  Large file  2026-10-21  /home/me/Projects/api-server/target/debug/api-server

3 file(s) of 1.0 MiB or more under /home/me/Projects (5.9 MiB allocated).
Inspect only: scull lists large files for review and never deletes them.
```

JSON: `scull/large-files@1` (NDJSON: `candidate` records, then `summary`).

### `scull status`

```text
scull status [--short]
```

Prints one line from the last recorded scan. It reads only the database: no scan, no Git,
no network, so it is fast enough for a shell prompt.

```console
$ scull status
scull: 5.9 MiB reclaimable (0 B safe) · 129.2 GiB free on / · last scan <1m ago
$ scull status --short
☠ 129G free · <1m · 5.9M reclaimable
```

`--short` always exits 0 and prints nothing at all when there is nothing to show (no scan
yet, no database), so your prompt never shows error noise. JSON: `scull/status@1` with
`state` = `ok`, `no_scan`, `no_database` or `unavailable`.

## Clean up

All cleanup goes through the same steps: **plan, review, confirm, revalidate, run, report**.
[SAFETY.md](SAFETY.md) explains each guarantee.

### `scull clean`

```text
scull clean [--root PATH]... [--include-s1] [--item ID|PATH]... [--output FILE]   # plan (dry run)
scull clean --plan FILE [--confirm] [--allow-permanent]                         # review or run
scull clean --purge-quarantine [--older-than AGE | --include-unexpired] [--confirm --allow-permanent]
```

**Planning** scans the roots and selects what the tiers allow:

* S0 items are selected by default.
* S1 items join with `--include-s1`. They are quarantined, not deleted.
* S2 items join only when you name each one with `--item`.
* S3 items are never selected.
* Package caches are left to `scull clean caches`. Git worktree removal is not part of this
  version. Both are listed as excluded with the reason.

The plan is printed, recorded in the database, and with `--output` written to a file
readable only by you. Nothing changes on disk. A plan expires after 24 hours.

**Running** a plan file:

1. scull refuses to run as root or Administrator unless `--allow-root` (exit 5).
2. It rejects an edited, expired, foreign or already-used plan (exit 4).
3. It scans again and narrows the plan to what is still there at the same tier or safer.
   The preview lists anything that dropped out.
4. It asks for confirmation: `--confirm`, or `y` at the prompt on a terminal. Without a
   terminal and without `--confirm`, it only reviews.
5. S1/S2 items that can't be undone also need the elevated confirmation: type their count,
   or pass `--allow-permanent` without a terminal.
6. It takes the execution lock (exit 6 if held), recovers any crashed operation, and runs
   each item after checking it again. S0 items are deleted; S1/S2 items are quarantined, or
   moved to the Trash if quarantine is impossible, or left in place. Failed items are
   reported one by one (exit 3).
7. It reports each item, the measured change in free space, and how to restore.

| Flag | Effect |
|---|---|
| `--root PATH` | Scan root; repeat for several. Default: `scan.roots`; with `--plan`, the plan's own roots. |
| `--include-s1` | Also plan S1 (costly) items. They are quarantined. |
| `--item ID\|PATH` | Plan exactly these items (ID from `scan --json`, or path). The only way to plan an S2 item. |
| `--dry-run` | Only plan or review; never run or ask. This is the default without a terminal. |
| `--output FILE` | Write the plan to FILE (owner-only) |
| `--plan FILE` | Review or run this plan file |
| `--confirm` | Run `--plan` (or `--purge-quarantine`) without asking |
| `--allow-permanent` | Confirm S1/S2 items that can't be undone without typing their count. Required for them without a terminal. |
| `--allow-root` | Allow running as root or Administrator |
| `--purge-quarantine` | Permanently delete quarantined items whose 7 days are up. Needs the elevated confirmation. |
| `--older-than AGE` | With `--purge-quarantine`: also purge items quarantined more than AGE ago (`90m`, `12h`, `30d`, `2w`) |
| `--include-unexpired` | With `--purge-quarantine`: purge everything held (empties the quarantine) |

#### Walkthrough

Plan two uncertain items by name and save the plan:

```console
$ scull clean --item ~/Projects/web-app/node_modules --item ~/Projects/sketch/node_modules --output plan.json
Cleanup plan (dry run): nothing has been changed.
Scan roots: /home/me/Projects

#  Tier          Action      Undo           Reclaimable  Path
1  S2 uncertain  quarantine  restore (7 d)    500.0 KiB  sketch/node_modules
2  S2 uncertain  quarantine  restore (7 d)      1.9 MiB  web-app/node_modules

Details:
  1. /home/me/Projects/sketch/node_modules
       what      Installed npm dependencies (rule node.node-modules v2), project sketch (node) at /home/me/Projects/sketch
       sizes     logical 496.3 KiB · allocated 500.0 KiB · reclaimable 500.0 KiB (high confidence)
       tier      S2 uncertain: could not check whether it is in use; not treated as idle [signal_unavailable]
                 S2 uncertain: project marker found, but no lockfile or manifest to reproduce from [no_lockfile]
       evidence  project marker /home/me/Projects/sketch/package.json
                 known artifact type: Installed npm dependencies
                 newest change 2026-10-21 20:45 UTC
                 ...
       regen     Reinstall with your package manager (`npm ci`, ...). (cost unknown)
       flags     active unknown · hardlinked no · cross-mount no · symlink unknown · cloud placeholder no
       action    quarantine: restorable: `scull restore <operation>` brings it back; kept 7 days, then purged
  2. ...

Not in this plan (1):
  S2 uncertain      4.0 MiB  api-server/target: not named with --item

Total: 2 to quarantine, 2.4 MiB reclaimable.
  Quarantined items move 2.4 MiB aside on the same volume (restorable); the space is freed when the quarantine is purged.

Plan plan_6ae653dc05e1bc9672def624 · 2 item(s) · expires 2026-11-01 20:47 UTC · bound to this machine and user (uid:1000)
Plan file: /home/me/plan.json
To execute: scull clean --plan /home/me/plan.json --confirm
```

The plan file ([`schemas/v1/plan.schema.json`](../../schemas/v1/plan.schema.json)):

```json
{
  "schema_version": 1,
  "id": "plan_6ae653dc05e1bc9672def624",
  "created_unix": 1793479644,
  "expires_unix": 1793566044,
  "scan_id": "scan_6ae653dccd50f3d176daaf7a",
  "machine_id": "eb99b71b69691a896e606b0c848f229a",
  "user": "uid:1000",
  "origin": "cli",
  "items": [
    {
      "item_id": "391fa8de23d265361250c39be663d21c",
      "identity": { "volume": 2049, "file": "6c00ff" },
      "path": "/home/me/Projects/sketch/node_modules",
      "category": "project_artifact",
      "rule": { "id": "node.node-modules", "version": 2 },
      "tier": "S2",
      "action": { "type": "quarantine" },
      "reclaimable_bytes": 512000
    }
  ],
  "content_hash": "882b1d70bc52b5a4070e52998830a8500002d98e93ac84989f2f3f2962332b1b"
}
```

Edit it, and scull refuses it:

```console
$ scull clean --plan tampered.json --confirm
scull: error: plan content does not match its hash; the plan file was edited
$ echo $?
4
```

Run the real plan. The review is printed again, then the result:

```console
$ scull clean --plan plan.json --confirm
Reviewing plan plan_6ae653dc05e1bc9672def624 (made 2026-10-31 20:47 UTC): checked against a fresh scan.
...
Operation op_6ae653f0cf4209dcf71230f4 (plan plan_6ae653dc05e1bc9672def624)
Outcome      Tier          Removed      Moved  Path                                    Why / where
quarantined  S2 uncertain      0 B  500.0 KiB  /home/me/Projects/sketch/node_modules   -> /home/me/.scull-quarantine/op_6ae653f0cf4209dcf71230f4/0001
quarantined  S2 uncertain      0 B    1.9 MiB  /home/me/Projects/web-app/node_modules  -> /home/me/.scull-quarantine/op_6ae653f0cf4209dcf71230f4/0002

2 of 2 item(s) done: 0 B removed, 2.4 MiB moved into quarantine.
Free space: predicted +0 B now, measured 3.1 MiB.
  - 2.4 MiB moved to quarantine/Trash: moved, not freed until purged
  - difference within free-space reporting resolution (±3.4 MiB)
Quarantined items can be restored for 7 days: scull restore op_6ae653f0cf4209dcf71230f4
Recorded in the audit log (scull history op_6ae653f0cf4209dcf71230f4).
```

Run the same plan again, and scull refuses: its items changed since it was made.

```console
$ scull clean --plan plan.json --confirm
...
scull: error: another operation touched this plan's targets since it was created (op_6ae653f0cf4209dcf71230f4); create a new plan
$ echo $?
4
```

JSON: `scull/clean-plan@1` for plans and reviews (`mode`, `scan_id`, `roots`, `plan`,
`plan_file`, `items[]`, `dropped[]`, `excluded[]`, `totals`, `elevated_confirmation`,
`signals_unavailable[]`, `cancelled`, `next_step`); `scull/clean-run@1` for a run (per-item
outcome such as `deleted`, `quarantined`, `blocked`, `not_run`, with totals and the measured
delta).

#### Purging the quarantine

```console
$ scull clean --purge-quarantine --include-unexpired
Quarantine purge (nothing deleted yet)
Entry  Operation                       Held  Retention ends        Result  Original path
    3  op_6ae6543e5688399330f5e720  4.0 MiB  2026-11-07 20:49 UTC  none    /home/me/Projects/api-server/target

1 item(s), 4.0 MiB held. Purging is permanent: purged items can't be restored.
Nothing was deleted. To delete these 1 item(s) for good: type 1 at the prompt on a terminal, or pass --confirm --allow-permanent
$ scull clean --purge-quarantine --include-unexpired --confirm
scull: error: purging the quarantine permanently deletes 1 item(s) (S1/S2, not undoable): pass --allow-permanent with --confirm to do it without a terminal (SAFE-07). Nothing was deleted.
$ echo $?
5
```

Without `--older-than` or `--include-unexpired`, only items past their 7 days are purged.
JSON: `scull/quarantine-purge@1` (`mode` = `preview` or `run`).

### `scull clean caches`

```text
scull clean caches [--wipe] [--untick TOOL]... [--remove-orphaned TOOL]... [--dry-run | --confirm] [--allow-root]
```

Quick Clean: cleans package-manager caches with each tool's own command, preferring a prune.
The preview shows each tool's cache, size, the exact command with the resolved binary, and
the note "Not undoable. Packages re-download on next install; offline installs may fail
until then." One confirmation covers all tools; no elevated confirmation is needed.

| Flag | Effect |
|---|---|
| `--confirm` | Run the commands without asking |
| `--dry-run` | Only show what would run; never ask |
| `--wipe` | Wipe caches fully instead of pruning, where the tool has a prune (npm, uv). pnpm, conda and Cypress are never wiped. |
| `--untick TOOL` | Leave this tool alone; repeat for several (`npm`, `pnpm`, `bun`, `yarn`, `yarn-classic`, `yarn-berry`, `pip`, `uv`, `conda`, `go`, `go-build`, `go-mod`, `nuget`, `ccache`, `conan`, `arduino`, `deno`, `cypress`) |
| `--remove-orphaned TOOL` | Also remove this uninstalled tool's orphaned cache folder, if the preview lists it (not undoable); repeat for several. Never offered for conda's `pkgs/`, the Go module cache, ccache, Conan, Deno or Cypress. |
| `--allow-root` | Allow running as root or Administrator |

Before each tool runs, scull checks again that its cache is the same folder, not a link, on
the expected drive, and not in use. A tool that was planned but didn't finish makes the exit
code 3. A tool whose cache tier doesn't allow tool commands (S2 here) is listed under "Not
run":

```console
$ scull clean caches
Quick Clean: package caches (nothing has been run)

No package cache can be cleaned right now.

Not run (1):
  npm           /home/me/.npm/_cacache: classified S2: tool commands are allowed at S0/S1 only
```

Quick Clean also runs five newer tool caches, each pointed at exactly the folder scull found:

| Tool | Command | Notes |
|---|---|---|
| ccache | `ccache --clear` | Keeps `ccache.conf`. No config file is read during the run, so remote storage is never touched. |
| Conan 2 | `conan cache clean --source --build --temp` | Temporary folders only: packages and the download cache stay. Skipped (`not_scoped`) if a `.conanrc` above scull's working folder would redirect Conan. |
| Arduino CLI | `arduino-cli cache clean` | The download cache (`staging/`). Skipped (`not_scoped`) unless it holds only downloaded archives in `packages/` and `libraries/` (never a sketchbook). |
| Deno | `deno clean` | Removes the whole Deno cache folder. Skipped (`not_scoped`) if it holds anything that isn't Deno's own cache: `location_data/` (web storage of Deno programs), REPL history, an installed `bin/`, other files. |
| Cypress | `cypress cache prune` | Keeps the binary of the Cypress version that runs it. Skipped (`not_scoped`) if the folder holds anything but Cypress version folders (such as session records or `bundles/`, which older Cypress versions also delete); `--wipe` still prunes. |

The other newer tool caches `scull caches` lists (CocoaPods, Playwright, Puppeteer, and
inspect-only ones such as vcpkg, SwiftPM, Coursier, Zig, Electron, Homebrew and the Nix store)
are listed under "Not run" with the reason `not_supported_yet` and are never offered as orphaned
caches: `pod cache clean --all` and `playwright uninstall --all` can't be limited to the folder
scull found, and `puppeteer browsers clear` always asks for confirmation. Of the five, only the
Arduino download cache can be offered as an orphaned cache.

A cache whose tool is no longer installed can't be cleaned with the tool's command. If it is
still a recognized package cache in its usual place, not a link, on the expected drive and not
in use, the preview lists it under "Orphaned caches" with its size and the not-undoable note
(except conda's `pkgs/`: environments hard-link into it, so scull never deletes it itself);
otherwise it is listed under "Not run" and left for you to remove. scull deletes an orphaned
cache itself only after a confirmation **for that tool**: `--remove-orphaned TOOL` (with
`--confirm` when there is no terminal), or answering `y` to its own question on a terminal.
The single Quick Clean confirmation never covers it. scull repeats every check right before
deleting. A tool named with `--remove-orphaned` that has no removable orphaned cache is listed
as such and, with `--confirm`, makes the exit code 3.

JSON: `scull/clean-caches@1` (`mode` = `preview` or `run`, `tools[]`, `skipped[]`,
`results[]`, `orphaned[]`, `orphaned_results[]`, `orphaned_unmatched[]`).

### `scull approve`

```text
scull approve [PLAN_ID]
scull approve --revoke PLAN_ID
```

Approve, in your own terminal, a cleanup plan an AI agent prepared through scull's MCP
server (destructive mode, see [MCP.md](MCP.md#agent-cleanups-destructive-mode)), or withdraw
an approval before the agent uses it. Interactive terminal only: with a pipe, a script or an
agent's shell tool it exits 5 and changes nothing; with `--json` it exits 2. Never as root.

* Without a plan ID it lists the agent plans awaiting approval and the approved ones not yet
  run.
* With one, it shows the full plan and a short code. Type the code to approve (single use,
  lapses with the plan, at most 15 minutes after the agent prepared it), or `deny`.
* `--revoke PLAN_ID` withdraws an approval the agent has not used yet. If the agent's
  `execute_cleanup` already started, the revocation is refused (`approval_used`); otherwise
  the agent's execution is refused (`approval_revoked`) and nothing runs.

Denials, refused attempts and revocations are recorded (`scull history`).

```console
$ scull approve --revoke plan_6b4b2f1c8e0d9a7b3c2d1e0f
Revoked the approval of plan plan_6b4b2f1c8e0d9a7b3c2d1e0f. The agent can't run it; nothing was changed. See `scull history`.
```

### `scull history`

```text
scull history [--since AGE] [OPERATION_ID]
```

Lists past cleanups, newest first, from the audit log: items done, bytes freed and moved,
items still restorable, and whether the audit chain is intact. With an ID it shows one
operation in full.

Below the operations it lists what was refused to AI agents (MCP destructive mode), newest
first: denied approvals, refused `execute_cleanup` calls and revoked approvals, each with its
reason code, plan, channel, what recorded it and how many times it repeated within a minute
(repeats share one entry), at most `--limit` of them and a count of the rest
(`agent_events` in JSON; see
[MCP.md](MCP.md#what-is-recorded)).

```console
$ scull history
No cleanup operations recorded yet.

Refused to AI agents (2, newest first):
When                  What               Reason        Plan                           Channel   Recorded by  Times
2026-11-01 09:14 UTC  approval denied    no_terminal   plan_6b4b2f1c8e0d9a7b3c2d1e0f  terminal  cli              1
2026-11-01 09:13 UTC  execution refused  not_approved  plan_6b4b2f1c8e0d9a7b3c2d1e0f  mcp       mcp             12
```

| Flag | Effect |
|---|---|
| `OPERATION_ID` | Show this operation in detail |
| `--since AGE` | Only operations started within AGE (`90m`, `12h`, `30d`, `2w`) |
| `--limit N` | Most entries under "Refused to AI agents" (default 50; from the last 30 days unless `--since` is given) |

```console
$ scull history
Operation                    Started               Status     Items  Freed    Moved  Restorable  Audit
op_6ae653f0cf4209dcf71230f4  2026-10-31 20:47 UTC  completed    2/2    0 B  2.4 MiB           2  intact

Details: scull history <operation>. Restore quarantined items: scull restore <operation>.
```

```console
$ scull history op_6ae653f0cf4209dcf71230f4
Operation op_6ae653f0cf4209dcf71230f4
  plan         plan_6ae653dc05e1bc9672def624 (hash 882b1d70bc52b5a4070e52998830a8500002d98e93ac84989f2f3f2962332b1b)
  approved by  uid:1000 (cli_flag)
  ran          2026-10-31 20:47 UTC -> 2026-10-31 20:47 UTC
  status       completed
  items        2 planned: 2 done, 0 with errors, 0 skipped
  bytes        predicted 2.4 MiB, freed 0 B, moved 2.4 MiB, measured +3250585 B
  audit        intact (4 rows, hash-chained)

Items:
#  Outcome      Action      Freed      Moved  Path                                    Reason
1  quarantined  quarantine    0 B  500.0 KiB  /home/me/Projects/sketch/node_modules
2  quarantined  quarantine    0 B    1.9 MiB  /home/me/Projects/web-app/node_modules

Quarantine:
Entry  State       Held  Until                 Original path                           Now
    1  held   500.0 KiB  2026-11-07 20:47 UTC  /home/me/Projects/sketch/node_modules   /home/me/.scull-quarantine/op_6ae653f0cf4209dcf71230f4/0001
    2  held     1.9 MiB  2026-11-07 20:47 UTC  /home/me/Projects/web-app/node_modules  /home/me/.scull-quarantine/op_6ae653f0cf4209dcf71230f4/0002

To put the quarantined items back: scull restore op_6ae653f0cf4209dcf71230f4
```

JSON: `scull/history@1` and `scull/history-operation@1`. One operation in `scull/history@1`,
after both items were restored:

```json
{
  "operation_id": "op_6ae653f0cf4209dcf71230f4",
  "plan_id": "plan_6ae653dc05e1bc9672def624",
  "approver": "uid:1000",
  "method": "cli_flag",
  "started_at": 1793479664,
  "finished_at": 1793479665,
  "status": "completed",
  "items_planned": 2,
  "items_recorded": 2,
  "items_done": 2,
  "items_with_errors": 0,
  "items_skipped": 0,
  "predicted_bytes": 2527232,
  "freed_bytes": 0,
  "moved_bytes": 2527232,
  "measured_delta_bytes": 3250585,
  "quarantine_held": 0,
  "chain_intact": true
}
```

### `scull restore`

```text
scull restore [--item ITEM]... [--dry-run | --confirm] [--allow-root] OPERATION_ID
```

Puts an operation's quarantined items back where they were. It never overwrites: if the
original path is taken, the item comes back next to it as `<name>.scull-restored-<n>`.
Without `--confirm` it previews (and asks on a terminal). Deleted S0 items can't be restored
(rebuild them), and items in the Trash are restored from the Trash. Exit 3 if an entry was
refused.

| Flag | Effect |
|---|---|
| `--item ITEM` | Restore only this item: item ID, original path, or quarantine entry number. Repeat for several. |
| `--confirm` | Restore without asking. Required without a terminal. |
| `--dry-run` | Only show what would be restored |
| `--allow-root` | Allow running as root or Administrator |

```console
$ scull restore op_6ae653f0cf4209dcf71230f4 --item 2 --confirm
Restore from operation op_6ae653f0cf4209dcf71230f4
Entry  State  Result       Size  Original path                           Note
    2  held   restored  1.9 MiB  /home/me/Projects/web-app/node_modules  now at /home/me/Projects/web-app/node_modules

1 restored (0 next to a path that was taken; nothing was overwritten), 0 refused.
```

When the original path is taken, the restored entry reports where it went:

```json
{
  "entry_id": 1,
  "item_id": "391fa8de23d265361250c39be663d21c",
  "original_path": "/home/me/Projects/sketch/node_modules",
  "quarantine_path": "/home/me/.scull-quarantine/op_6ae653f0cf4209dcf71230f4/0001",
  "bytes": 512000,
  "state": "restored",
  "outcome": "none",
  "restored_to": "/home/me/Projects/sketch/node_modules.scull-restored-1",
  "message": null
}
```

JSON: `scull/restore@1` (`operation_id`, `mode` = `preview` or `run`, `entries[]`,
`not_restorable[]`, `restored`, `restored_to_sibling`, `refused`, `next_step`).

## Set up and maintain

### `scull init`

```text
scull init [-y] [--roots PATH...] [--builds KIND...] [--agents USE] [--priority GOAL] [--caution LEVEL] [--allow-broad-roots] [--force]
```

First-run setup. It detects your toolchains, caches and project folders, asks a few
questions, and writes `config.toml` (scan roots) and `profile.toml`. Other settings in an
existing config are kept, and the old file is saved as `config.toml.bak`.

| Flag | Effect |
|---|---|
| `-y`, `--yes` | Accept the detected defaults; no prompts |
| `--roots PATH...` | Scan roots (repeat or comma-separate); replaces the detected folders |
| `--builds KIND...` | What you build: `web`, `rust`, `python`, `ml`, `mobile`, `game`, `native`, `infra`, `other` |
| `--agents USE` | AI coding agents: `none`, `sometimes`, `daily` |
| `--priority GOAL` | `free-space`, `tidy` or `understand` |
| `--caution LEVEL` | `cautious`, `balanced` or `expert`. Never changes tiers or safety checks. |
| `--allow-broad-roots` | Confirm a drive root, home folder or its parent as a root |
| `--force` | Replace an existing profile and scan roots (other settings are kept) |

```console
$ scull init --yes --roots ~/Projects
Detected
  toolchains       Node.js (node, npm, pnpm, yarn, bun), Python (python3, uv), Containers (docker), C/C++ (gcc)
  caches           ~/.npm
  project folders  ~/Projects
  AI clients       -
  disk             size and free space not shown yet (no volumes API in this build)

Here's what scull will focus on
  builds           web, python, infra, native
  scan roots       ~/Projects
  AI agents        none
  priority         understand
  caution          balanced (never changes tiers or safety checks)

Wrote /home/me/.scull/config/config.toml and /home/me/.scull/config/profile.toml
Next: scull scan ~/Projects   (or `scull doctor` to check this machine)
```

JSON: `scull/init@1`.

### `scull doctor`

```text
scull doctor
```

Reports which safety signals work on this machine, whether you are elevated, where scull
keeps its files, the database state, the Git version, and every config value with where it
came from.

```console
$ scull doctor
scull 0.0.0 (x86_64-unknown-linux-gnu) on linux x86_64 (unix)

Signals
Signal               Status       Detail
file_identity        available    file identity (device + inode, volume + file ID)
allocated_size       available    allocated size on disk
...
elevation_detection  available    root/administrator detection (SAFE-14)

Privileges
Check            Value
elevation        not elevated
sudo user        -
destructive ops  allowed after review: `scull clean --plan FILE --confirm` (or the prompt on a terminal)

Directories
SCULL_HOME: /home/me/.scull
Role    Exists   Owner-only  Path
config  yes      yes         /home/me/.scull/config
data    yes      yes         /home/me/.scull/data
...

Config
file: /home/me/.scull/config/config.toml (ok)
Field                 Value       Origin
scan.roots            ~/Projects  user config
scan.activity_window  24h         default
mcp.enabled           false       default
mcp.redact_home       true        default
...
output.color          never       env NO_COLOR
```

JSON: `scull/doctor@1`.

### `scull rules`

```text
scull rules list
scull rules validate [PATHS]...
```

`list` shows the detection rules: bundled ones and any user rules in `<config>/rules/*.toml`
(off until enabled with `[rules] enabled` or `SCULL_RULES`). `validate` checks rule files
(default: the bundled rules). Rules are data only and can never run commands.

```console
$ scull rules list
ID                 Ver  Ecosystem  Base tier       Strategy    Platforms              Enabled  Source
rust.target          2  rust       S0 regenerable  filesystem  macos, linux, windows  yes      bundled
node.node-modules    2  node       S0 regenerable  filesystem  macos, linux, windows  yes      bundled
node.next            2  node       S0 regenerable  filesystem  macos, linux, windows  yes      bundled
node.nuxt            1  node       S0 regenerable  filesystem  macos, linux, windows  yes      bundled
node.output          1  node       S0 regenerable  filesystem  macos, linux, windows  yes      bundled
node.turbo           1  node       S0 regenerable  filesystem  macos, linux, windows  yes      bundled
node.parcel-cache    1  node       S0 regenerable  filesystem  macos, linux, windows  yes      bundled

7 rules (7 bundled, 0 user); user rule files: /home/me/.scull/config/rules
```

JSON: `scull/rules-list@1` and `scull/rules-validate@1`.

### `scull data`

```text
scull data show
scull data export [--format json|csv] [--out DIR]
scull data purge (--all | --older-than AGE) [--confirm]
```

What scull has recorded about this machine, and how to take it with you or delete it.

* **`show`** lists scull's folders, the database (path, schema version, rows per table, and
  the `events` rows by kind: alerts, review plans and the agent audit events
  `approval_denied` and `execute_refused`), scull's own files with sizes, and what is in
  quarantine. Read-only.
* **`export`** writes every table to DIR (default `./scull-export-<unix time>`), created
  owner-only. Existing files are never overwritten. `--format json` (default) writes one
  `scull-data.json` (`scull/data-dump@1`); `--format csv` writes one CSV per table.
* **`purge`** deletes scull's own files and nothing else: the database, its backups and
  damaged copies, audit mirrors and the MCP salt. The config and the quarantine are kept.
  * `--all` deletes all of them. It refuses while quarantined items still need their record
    (restore or purge them first).
  * `--older-than AGE` deletes only migration backups and damaged copies older than AGE.
  * `--confirm` skips the prompt (you type `purge`). Required without a terminal or with
    `--json`.
  * `--include-quarantine` is refused; use `scull clean --purge-quarantine`.

```console
$ scull data show
What scull has recorded on this machine

Data folder    /home/me/.scull/data
Config folder  /home/me/.scull/config (kept by purge)
State folder   /home/me/.scull/state

Database       /home/me/.scull/data/scull.db (schema v6)
  scans                       4 rows
  items                       4 rows
  plans                       2 rows
  operations                  2 rows
  audit                       7 rows
  quarantine_manifest         3 rows
  ...

Files          3 files, 257.3 KiB
   252.0 KiB  database      /home/me/.scull/data/scull.db
     3.1 KiB  audit_mirror  /home/me/.scull/data/audit/op_6ae653f0cf4209dcf71230f4.ndjson
     2.2 KiB  audit_mirror  /home/me/.scull/data/audit/op_6ae6543e5688399330f5e720.ndjson

Quarantine     nothing in quarantine
```

JSON: `scull/data-show@1`, `scull/data-export@1`, `scull/data-purge@1`.

### `scull headroom`

```text
scull headroom status
scull headroom create [--size SIZE] [--yes]
scull headroom release [--yes]
```

An emergency reserve. It is off by default. When the disk is completely full, there may not be
room to run a cleanup. A reserve file set aside earlier fixes that: release it, and you have
room again.

* **`create`** writes `headroom/reserve.bin` in scull's data folder, on that folder's volume.
  The file is owner-only and really allocated: scull writes data that doesn't compress, then
  checks that the disk holds every byte. On a filesystem that would keep it sparse, compressed
  or deduplicated, scull removes it again and says why. The default size is 2 GiB. `--size`
  takes 256 MiB to 10 GiB (`512M`, `4G`), and never more than 5% of the volume. scull refuses
  when free space is already below your forecast reserve (`[proactive] reserve_gib`, 10 GiB by
  default), or would drop below it.
* **`release`** deletes the reserve and frees its space. scull deletes only the exact file it
  created. At creation it writes 32 random bytes at the start of the file. `headroom.json`
  records them along with the file's identity, size and modification time. Before deleting,
  scull reads those first bytes back without following links. Anything that doesn't match is
  refused and left alone, even a file the OS gave the same identity. Links are never followed. The deletion uses the same safe removal
  path as every cleanup.
* **`status`** says whether there is a reserve, how big it is, and whether it can be released.
  `scull status` mentions a ready reserve too, and the desktop shows it in Settings.

`create` and `release` ask first on a terminal (`[y/N]`). Without one, pass `--yes`. Every
outcome is recorded in scull's history database (`headroom` events). A release is recorded
right after it runs, because on a full disk the database may need the freed space to write.

```console
$ scull headroom create
Reserve 2.0 GiB of disk space in /home/me/.scull/data/headroom/reserve.bin? It stays until you release it. [y/N] y
Reserved 2.0 GiB in /home/me/.scull/data/headroom/reserve.bin. Release it with `scull headroom release` when the disk is full.

$ scull headroom release --yes
Released the headroom file: 2.0 GiB free again. Run the cleanup now (`scull clean`), then `scull headroom create` to set a new reserve.
```

JSON: `scull/headroom@1`.

### `scull check`

```text
scull check [--no-scan] [--include-s1]
```

A light, unattended check, meant for the OS scheduler (`scull schedule`):

1. Scans `scan.roots`, like `scull scan`, and records it. `--no-scan` skips this. Roots that
   `scull scan` refuses without a flag (a filesystem root, your home folder, a network
   volume) are not scanned unattended; the check says why and uses recorded history.
2. Samples free space on every drive scull has seen before and on the drives holding your
   scan roots.
3. Forecasts each drive from its last 30 days of samples (see [FORECAST.md](../FORECAST.md)).
4. Raises alerts: a drive below its reserve or forecast to drop below it within
   `[proactive] alert_days`, budgets, stale worktrees, build outputs and caches, growth
   spikes and fast free-space drops. Staleness and ecosystem or path budgets need the scan.
5. Prepares a **ready-to-review** cleanup plan of S0 items (S1 too with `--include-s1`) and
   tells you how to review it: `scull clean --plan <file>`.

It never deletes anything. Alerts point to a reviewed cleanup, and the plan runs only through
`scull clean --plan`, with a fresh check of every item and your confirmation. It writes only
history rows, the plan record and the plan file. The exit code is 0 whether or not alerts
fired. JSON: `scull/check@1`.

### `scull schedule`

```text
scull schedule enable [--every DURATION] [--yes | --dry-run]
scull schedule disable [--yes | --dry-run]
scull schedule status
```

Registers `scull check --quiet --no-progress` with your OS scheduler: a launchd LaunchAgent
on macOS, a systemd user timer on Linux, Task Scheduler on Windows. It runs as you, at low
priority, every 6 hours by default (`--every` takes 15 minutes to 7 days: `30m`, `6h`, `1d`).

* `enable` and `disable` first print every file they will write or remove, with its full
  contents, and every command they will run. Then `--dry-run` stops, `--yes` applies, and
  otherwise scull asks `[y/N]` on a terminal. Without a terminal, or with `--json`, they need
  `--yes` and otherwise exit 2 with nothing changed.
* Applying is refused from an elevated shell (root, an Administrator prompt): the job must
  run as the user it belongs to. `--dry-run` and `status` still work.
* `status` shows whether checks are registered. It is read-only.

The scheduled job only reads and records, like `scull check`; it never deletes anything.
JSON: `scull/schedule@1`.

The desktop app can also watch from the menu bar or system tray while it runs. It shows OS
notifications for low space, disk-full forecasts and other alerts, and an optional weekly digest (Settings → Alerts,
off by default). The app only reads and notifies, and installs no background service.

### `scull insights`

```text
scull insights [--since WHEN]
scull insights export [--format json|csv] [--since WHEN]
scull insights purge [--older-than WHEN] [--confirm]
```

Trends computed from scull's own history, on your machine: top growers over 7 and 30 days,
artifacts that came back after a cleanup, duplication across projects, agent worktrees,
predicted vs. measured cleanups, the ecosystem mix and free space per drive with its forecast.
`--since` sets the window: a duration back from now (`7d`, `30d`, `12w`) or a UTC date
(`2026-10-01`); the default is 30 days. Read-only.

* **`export`** writes the report to stdout as JSON (default) or CSV. Redirect it to keep it.
* **`purge`** forgets recorded trends: history rows only (free-space and footprint samples,
  item and regrowth events, items that are gone) in scull's own database. Live items, scans,
  plans, operations, audit and every file are kept. `--older-than` limits it to history older
  than a duration or date. On a terminal you type `purge` to confirm; otherwise `--confirm` is
  required. Each purge is recorded as an `insights_purged` event.

JSON: `scull/insights@1`.

### `scull ask`

```text
scull ask [--show-request] [--yes] QUESTION...
```

An optional assistant that answers questions about your developer storage ("why did my disk
fill up this week?") with a model you choose. It is **off by default** and sends nothing
until you turn it on.

* **Setup.** Create `ask.toml` next to `config.toml` (`scull doctor` shows the folder) with
  `enabled = true` and a provider: `anthropic`, or `openai` for any OpenAI-compatible
  endpoint, including local servers such as Ollama or LM Studio via `base_url`.
  `scull ask --help` lists every setting.
* **Your key.** The API key is read from the environment (`ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, or the variable named by `api_key_env`). scull never stores or logs it,
  and sends it only to the configured endpoint. Endpoints must use `https://` unless they are
  on this machine, and redirects are never followed.
* **Consent.** Before the first question to a provider and endpoint, scull shows what will be
  sent, and to whom, and asks you to agree. `--show-request` prints the notice and the exact
  first request (key masked) and sends nothing. It works even while the assistant is off.
  `--yes` agrees without a prompt and is required when there is no terminal.
* **Read-only.** The model can only call scull's read-only tools, the same ones
  `scull mcp serve` offers minus `register_worktree`, with the same scan-root allowlist and
  path redaction (`[mcp] redact_home`, `hash_names`). It can't delete, move or change files,
  and it can't prepare or approve a cleanup. Letting it prepare a plan for your review is
  tracked in #22.

What you send goes to the provider you configured. Nothing else leaves your machine.

### `scull completions`

```text
scull completions bash|zsh|fish|powershell
```

Prints a completion script on stdout. See [INSTALL.md](INSTALL.md#shell-completions).

### `scull mcp serve`

```text
scull mcp serve [--allow-destructive]
```

Runs the MCP server over stdio for an AI client. It refuses to start until you enable it
(exit 2). It is read-only unless `--allow-destructive` is passed **and** `[mcp] destructive =
true` is in scull's config file; then the agent can prepare cleanup plans that run only after
you approve them with `scull approve` (never when scull runs elevated). See
[MCP.md](MCP.md#agent-cleanups-destructive-mode).

```console
$ scull mcp serve </dev/null
scull: error: the MCP server is not enabled: add `[mcp]` `enabled = true` to /home/me/.scull/config/config.toml, or run `scull mcp serve` once in a terminal to review the privacy notice and enable it
$ echo $?
2
```

## More commands

These commands are built; `scull <command> --help` describes their options:

| Command | Purpose |
|---|---|
| `scull approve [PLAN_ID]`, `scull approve --revoke PLAN_ID` | Approve a plan an AI agent prepared, or revoke an approval, in your own terminal (interactive only; see above) |
| `scull worktrees register PATH --ttl 7d` | Give an agent's worktree a time-to-live |
| `scull agent install\|uninstall CLIENT [--yes \| --dry-run]` | Write (or remove) scull's MCP entry and short guidance for an AI client (`claude-code`, `codex`, `cursor`, `vscode`, `gemini-cli`); shows each file's diff and asks first |
| `scull doctor --report` | A shareable, redacted diagnostic report, shown as a preview first |

## Configuration and environment

The config file is `config.toml` in scull's config folder (`scull doctor` prints the path;
see [INSTALL.md](INSTALL.md#where-scull-keeps-its-files)). Settings apply in this order, first
wins: **command-line flags, environment variables, the config file, built-in defaults.**

```toml
[scan]
roots = ["~/Projects", "~/work"]
exclude = ["~/Projects/huge-monorepo"]   # never scanned
protect = ["~/Projects/thesis"]          # everything inside is S3
activity_window = "24h"                  # recent changes bump one tier
threads = 0                              # 0 = automatic
low_resource = false

[scan.min_tier]
"~/Projects/legacy" = "S2"               # nothing inside is safer than S2

[rules]
enabled = []                             # optional rule IDs to turn on

[mcp]
enabled = false
redact_home = true
hash_names = false
max_results = 1000

[forge]
enabled = false                          # network lookups on a Git host; off by default

[output]
color = "auto"                           # auto, always, never
units = "binary"                         # binary (MiB) or decimal (MB)

[quarantine]                             # optional: where quarantined items go
base = "~/.scull-q"                      # for items on the drive holding this folder

[quarantine.bases]                       # optional, per drive: longest matching prefix wins
"/Volumes/Data" = "/Volumes/Data/me/.scull-q"
```

Without `[quarantine]`, scull picks the quarantine folder itself: the shallowest folder you
own above the item, on its drive (usually your home folder). A configured base holds
`.scull-quarantine/` and is used only for items **on its own drive**: an item on another drive
goes to the Trash instead, or is left in place. Nothing is ever moved across drives or deleted
because of it. The base is created readable by you only, must not be a link, and must not be
inside something being cleaned. `scull data show` prints where new quarantines go.

The schema is [`schemas/v1/config.schema.json`](../../schemas/v1/config.schema.json).
`exclude`, `protect` and `min_tier` add up across sources; other fields override.

| Variable | Sets | Value |
|---|---|---|
| `SCULL_ROOTS` | `scan.roots` | Path list (`:` on Unix, `;` on Windows); replaces configured roots |
| `SCULL_EXCLUDE` | `scan.exclude` | Path list; added to exclusions |
| `SCULL_PROTECT` | `scan.protect` | Path list; added to protected paths |
| `SCULL_ACTIVITY_WINDOW` | `scan.activity_window` | Duration, e.g. `24h`, `90m`, `7d` |
| `SCULL_THREADS` | `scan.threads` | Number; `0` = automatic |
| `SCULL_LOW_RESOURCE` | `scan.low_resource` | Boolean |
| `SCULL_RULES` | `rules.enabled` | Comma-separated rule IDs |
| `SCULL_MCP` | `mcp.enabled` | Boolean |
| `SCULL_MCP_REDACT_HOME` | `mcp.redact_home` | Boolean |
| `SCULL_MCP_HASH_NAMES` | `mcp.hash_names` | Boolean |
| `SCULL_MCP_MAX_RESULTS` | `mcp.max_results` | Positive number |
| `SCULL_FORGE` | `forge.enabled` | Boolean |
| `SCULL_COLOR` | `output.color` | `auto`, `always`, `never` |
| `SCULL_NO_COLOR`, `NO_COLOR` | `output.color` | Any non-empty value means `never` |
| `SCULL_UNITS` | `output.units` | `binary` or `decimal` |
| `SCULL_QUARANTINE_BASE` | `quarantine.base` | Absolute folder holding `.scull-quarantine` (same drive as the items) |
| `SCULL_HOME` | all of scull's folders | Absolute path; puts config, data, cache and state under it |

Booleans accept `1/0`, `true/false`, `yes/no`, `on/off`. `SCULL_COLOR` beats `SCULL_NO_COLOR`,
which beats `NO_COLOR`. On purpose, no variable can enable destructive MCP tools or lower a
tier.

Per-repository files can only add protection: a `.scull-protect` file makes its folder S3,
and `.scull.toml` can add exclusions, protected paths or higher tiers. Neither can lower a
tier, add roots, enable rules or move the quarantine.

## JSON Schemas

Every `--json`/`--ndjson` document has a published JSON Schema (draft 2020-12):

* Command envelopes: [`crates/scull-cli/schemas/`](../../crates/scull-cli/schemas/), one
  `<name>.schema.json` per document and `<name>.ndjson.schema.json` for its NDJSON lines
  (every command that prints NDJSON publishes one).
* Shared types (candidate, plan, error, tier breakdown, drop reason, config):
  [`schemas/v1/`](../../schemas/v1/), with example payloads in `schemas/v1/examples/`.

| Schema ID | Command |
|---|---|
| `scull/scan@1` | `scan` |
| `scull/projects@1`, `scull/project@1` | `projects`, `projects inspect` |
| `scull/worktrees@1`, `scull/worktree@1` | `worktrees`, `worktrees inspect` |
| `scull/caches@1` | `caches` |
| `scull/large-files@1` | `large-files` |
| `scull/status@1` | `status` |
| `scull/clean-plan@1`, `scull/clean-run@1` | `clean` (plan or review; run) |
| `scull/clean-caches@1` | `clean caches` |
| `scull/quarantine-purge@1` | `clean --purge-quarantine` |
| `scull/history@1`, `scull/history-operation@1` | `history`, `history OPERATION` |
| `scull/restore@1` | `restore` |
| `scull/init@1` | `init` |
| `scull/doctor@1` | `doctor` |
| `scull/rules-list@1`, `scull/rules-validate@1` | `rules list`, `rules validate` |
| `scull/data-show@1`, `scull/data-export@1`, `scull/data-purge@1`, `scull/data-dump@1` | `data` |
| `scull/doctor-report@1` | `doctor --report` |
| `scull/worktree-registration@1` | `worktrees register` |
| `scull/check@1`, `scull/schedule@1`, `scull/insights@1`, `scull/agent@1` | `check`, `schedule`, `insights`, `agent` |
| `scull/insights-purge@1` | `insights purge` |
| `scull/error@1` | any error in JSON mode (stderr) |
