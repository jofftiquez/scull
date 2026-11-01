# How scull keeps your files safe

scull deletes files, so safety comes first. This page explains, in plain language, what scull
will and won't do. The architecture is described in [ARCHITECTURE.md](../ARCHITECTURE.md).

## The short version

* **Looking is free.** `scan`, `projects`, `worktrees`, `caches`, `large-files`, `history`
  and `status` only read. They never change your disk.
* **Nothing happens without your say-so.** Every cleanup starts as a dry run. scull acts only
  on a plan you reviewed, and only after you confirm it.
* **Source code and Git history are off-limits.** Anything tracked by Git is protected.
* **Most cleanups can be undone.** Uncertain or costly items go into quarantine for 7 days,
  and `scull restore` puts them back. Only regenerable items (S0) are deleted outright, and
  you can rebuild those.
* **Nothing is ever deleted for good behind your back.** When scull can't quarantine an
  item, it tries the Trash, and failing that it leaves the item alone.
* **An AI agent can't clean up for you.** The MCP server is read-only. An AI tool call is not
  your consent.
* **No telemetry.** Everything stays on your machine.

## Risk tiers

scull puts every item it finds into one of four tiers. The tier decides whether scull may
touch the item and how.

| Tier | Name | Meaning | Selected by default? | What a cleanup does |
|---|---|---|---|---|
| **S0** | Regenerable | A verified build output you can rebuild from files you have (e.g. `target/` next to `Cargo.toml` and `Cargo.lock`) | Yes, for review; never run without confirmation | Deleted. Rebuilding is the undo. |
| **S1** | Costly | Reproducible, but slow or large to rebuild or re-download | No: opt in with `--include-s1` | Quarantined (restorable for 7 days) |
| **S2** | Uncertain | Ownership, activity or reproducibility is unclear | Never: name each item with `--item` | Quarantined or moved to the Trash only |
| **S3** | Protected | Might hold unique data, is in use, or you protected it | Never | Nothing. scull won't touch it. |

### Telling tiers apart

Color is never the only cue. Every view pairs it with a word or a pattern.

| Tier | Terminal | Desktop and charts |
|---|---|---|
| S0 | `S0 regenerable`, bold magenta | solid violet (`#7C4DFF`) |
| S1 | `S1 costly`, magenta | solid lilac (`#DCCBFF`) |
| S2 | `S2 uncertain`, yellow | **amber diagonal stripes** |
| S3 | `S3 protected`, bold blue | **dark plum cross-hatch** |

The terminal always prints the tier in words, so the output reads the same without color
(`--color never` or `NO_COLOR=1`). In charts, a folder never looks "safe" if anything inside
it is S2 or S3: its badge shows the most protective tier among its contents.

### How scull picks a tier

Each item gets the **most protective** tier that any check triggers. Some examples:

| If... | the item is at least |
|---|---|
| any file inside it is tracked by Git | S3 |
| it is inside a protected folder, an exclusion, or a folder with a `.scull-protect` file | S3 |
| a program is using it (open files, a running process's working folder, a tool's lock file) | S3 for this run |
| it crosses onto another drive, or is reached through a link that leaves the scan folder | S3 |
| it lives in a cloud-synced folder (iCloud, OneDrive, Dropbox) | S2 |
| the project marker is there but no lockfile to rebuild from | S2 |
| scull **could not check** whether it is tracked or in use | S2 |
| it changed in the last 24 hours (configurable) | one tier more protective (S0 becomes S1) |

A folder name alone is never enough. `target`, `build` or `dist` count only next to the
project file that produces them. A folder with no matching project file is not a candidate
at all.

**Missing evidence counts as risk.** If scull can't tell whether a folder is in use, for
example because the OS won't let it inspect some of your processes, it treats the folder as
uncertain (S2) and says so: "could not check whether it is in use; not treated as idle".
`scull doctor` lists which checks work on your machine.

### Git worktrees

scull reads worktrees from Git itself, never from folder names. A worktree with uncommitted
or untracked changes, unpushed commits, a lock, or active use is S3. A clean, unlocked
worktree whose commits are all pushed or merged is at best S1, never S0. scull never removes
a worktree just because it is old, and it never deletes branches. Removing whole worktrees
is not part of `scull clean` in this version; it lists them as excluded.

## What scull never touches

* Files tracked by Git, and Git history.
* Your home folder itself, drive roots, OS and system folders.
* Credentials: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.password-store`, and on macOS
  `~/Library` (except its caches and Xcode's DerivedData).
* Anything you protect: `scan.protect` in the config, `SCULL_PROTECT`, or a `.scull-protect`
  file in a folder.
* Anything outside the folders you asked it to scan. scull never follows a symlink or
  junction out of a scan folder.
* Other drives reached from inside a scan folder, network drives (unless you allow them) and
  backup drives such as Time Machine.
* Cloud placeholders. scull never downloads ("hydrates") files that only exist in the cloud.
* Special files: pipes, sockets and devices are never opened.
* Large files. `scull large-files` lists them for you to review; scull never cleans them.
* Docker volumes, unless you choose them explicitly.

Protection is matched by file identity (device and inode, or volume and file ID), not by
path text. A different spelling, case, Unicode form or Windows short name can't slip past it.

A repository's own settings (`.scull-protect`, `.scull.toml`) can only **add** protection.
They can never lower a tier or widen what scull scans, because a cloned repository is
untrusted.

## Every cleanup is a dry run first

`scull clean` with no flags only plans. It shows what it would do and changes nothing. You see,
for every item:

* the exact path and the project it belongs to;
* what it is and the evidence for that (marker files, lockfile, rule);
* its size three ways: logical, allocated on disk, and reclaimable, with a confidence level;
* its tier and every reason for it;
* how to rebuild it, and the cost if known ("unknown" otherwise);
* warning flags: in use, hard-linked, crosses a drive, symlink, cloud placeholder;
* the action (delete, quarantine, Trash, a tool's own command) and whether you can undo it.

## Plans

A plan is the exact list of items you approved. scull writes it as a JSON file
(`--output plan.json`) and records it in its database.

* **It expires.** A plan from the CLI is valid for 24 hours, a desktop plan for 1 hour. Agent
  plans, planned for a later version, will last 15 minutes.
* **It is sealed.** The plan carries a hash of its content. An edited plan is rejected.
* **It belongs to you.** A plan is bound to the machine and user account that made it. A
  plan from another machine or user is rejected.
* **It can only shrink.** When you run a plan, scull scans again and runs only the items that
  are still there and still at the same tier or safer. It never adds anything. An item that
  changed (moved, replaced, now in use, tier raised) is dropped and reported.
* **It is single-use.** Once a cleanup touched a plan's items, the plan is rejected. Make a
  new one.

A rejected plan exits with code 4 and changes nothing.

## Checked again, right before each item

Just before scull touches an item, it checks again that the item is the same folder (same
identity and type), still not tracked by Git, still not in use, and still at the same tier.
If any check fails, scull skips that item and tells you why. It never makes a "best effort"
attempt.

## What happens to each item

| Tier | Action | How to undo |
|---|---|---|
| S0 | Deleted after your confirmation | Rebuild it (`cargo build`, `npm ci`, ...) |
| S1, S2 | Moved into quarantine | `scull restore <operation>` within 7 days |
| S1, S2 when quarantine is impossible | Moved to the Trash / Recycle Bin | Restore it from the Trash |
| S1, S2 when the Trash is impossible too | **Left in place** and reported as blocked | Nothing to undo |
| S3 | Never touched | — |

scull deletes with its own careful code: it works relative to verified folder handles and
never follows links. It never runs a shell `rm`.

### Quarantine

Quarantine moves an item aside instead of deleting it. The move is a rename on the same
drive, so it is instant and never copies data across drives.

* Items go to a `.scull-quarantine/<operation>/` folder on the same drive, inside the
  shallowest folder you own there (usually your home folder), or inside the folder you set
  with `[quarantine] base` in the config when it is on the item's drive. The folder is
  readable by you only.
* Quarantined items stay for **7 days**. Until then `scull restore <operation>` puts them back.
* **Quarantine does not free space yet**: the bytes have only moved. scull reports them as
  "moved", not "freed". The space comes back when you purge.
* Restore puts items back exactly where they were and **never overwrites**. If something
  now sits at the original path, the item comes back next to it as
  `<name>.scull-restored-1`.
* scull's database is the record of what was quarantined from where. Restore trusts it, not
  files inside the quarantine folder.

### Purging is permanent, and needs a stronger yes

Purging the quarantine (`scull clean --purge-quarantine`) deletes items for good. So does any
other action that would permanently remove an S1 or S2 item. These need an **elevated
confirmation**: type the number of items at the prompt, or pass `--confirm --allow-permanent`
when there is no terminal. Without it scull refuses with exit code 5. Permanent deletion is
never presented as reversible.

### Package caches (Quick Clean)

Package-manager caches (npm, pnpm, Yarn, Bun and others) hold nothing you can't download
again, so they get their own path, `scull clean caches`:

* scull runs **each tool's own command** (for example `npm cache verify`, `pnpm store
  prune`). It prefers a prune and offers a full wipe only with `--wipe`. It never wipes the
  pnpm store, because your projects hard-link into it.
* It runs the real tool from a trusted location, never from a project folder or
  `node_modules/.bin`, with a clean environment and no shell. The preview shows the exact
  binary and command.
* It skips a tool that is running, or whose cache is a link or sits on an unexpected drive,
  and tells you.
* **This is not undoable**, and the preview says so: "Not undoable. Packages re-download on
  next install; offline installs may fail until then." One confirmation covers all tools.
* A cache whose tool is no longer installed is listed. scull removes it itself only if you
  confirm **that tool** (`--remove-orphaned TOOL`, or its own question on a terminal), with
  the same not-undoable note and every check repeated right before; otherwise it is left in
  place for you to remove.

## One cleanup at a time

Only one cleanup runs per user account at any moment, whether from the CLI, the desktop app
or anywhere else. A second one fails at once with exit code 6. Scans can still run alongside.
The OS releases the lock if scull exits or crashes, so there is never a stale lock to clear.

## Crash-safe

Before scull touches an item, it writes its intent to a journal in its database, and it
closes the entry afterwards. If scull, the machine or the power fails mid-cleanup, the next
cleanup command first compares the journal with the disk and records what really happened
to each item. Recovery **never resumes** a deletion or a move. It only records the facts, and
you plan again for what is left.

## Every cleanup is recorded

Each `scull clean` run gets an operation ID and an entry in an append-only audit log: the
plan and its hash, who approved it and how, and one outcome per item (removed, quarantined,
moved to the Trash, skipped and why, failed). It also records predicted bytes, freed bytes,
moved bytes and the measured change in free space. Rows are hash-chained, so a gap or an
edit shows up. Restores and purges are recorded against the quarantine entries.
`scull history` lists operations and checks the chain; `scull history <operation>` shows one
in detail, including where each quarantined item is now.

Freed space is measured, not guessed. Hard links, file-system snapshots and compression can
make the real gain differ from the estimate, and scull explains differences over 10%.

## scull refuses to run as root or Administrator

Commands that change the disk refuse to run elevated (root, `sudo`, an Administrator prompt).
An elevated run could reach files your normal account can't. Pass `--allow-root` only if you
really mean it. scull never asks for elevation itself.

## AI agents and MCP

scull can serve AI coding agents over the Model Context Protocol (`scull mcp serve`). See
[MCP.md](MCP.md). The rules:

* **An AI tool call is not your consent.** No MCP tool can delete, move, plan or approve a
  cleanup. The agent can only tell you to run `scull clean` yourself.
* The server is off until you enable it, and it can only see your configured scan folders.
* Names from your disk reach the model as escaped data, and the model is told to treat them
  as untrusted. File contents and commit messages are never sent.
* **Out of scope:** an agent that can already run shell commands can delete files without
  scull. scull only guarantees that *its own* tools can't delete without you.

## Privacy

* **No telemetry**, no analytics, no crash reporting, no update checks.
* **No network by default.** Scans never fetch anything. Git is read with hooks, fsmonitor
  and fetches disabled, so a repository can't run code on your machine. Lookups on a Git
  host (for squash-merge detection) are off unless you enable them.
* scull's database holds metadata (paths, sizes, tiers, history), never file contents.
  `scull data show` shows what is recorded, `scull data export` copies it out, and
  `scull data purge --all` deletes it.
* The MCP server is the one exception: its results go to your AI client and possibly its
  model provider. scull shows a notice before you enable it.

## Terminal output is safe to read

Control characters and right-to-left overrides in file names are escaped before printing,
and suspicious names are flagged. A crafted file name can't rewrite your terminal or disguise
a path.

## Found a problem?

Anything that could make scull delete, move or expose what it shouldn't is a security issue.
Report it privately as described in [SECURITY.md](../../SECURITY.md). Wrong tiers and crashes
are welcome as regular issues.
