# scull architecture

**Status:** living document (P0-ARCH-01). It describes the Rust engine as built, and links the
issue for each part that isn't built yet. rustdoc (`cargo doc --workspace --no-deps --open`) is
the API reference; this file explains how the parts fit together and why.

Companion docs: [PRD](./PRD.md) (requirements, IDs like SAFE-05) ·
[Decisions](./DECISIONS.md) · [Threat model](./THREAT_MODEL.md) · [Fixtures](./FIXTURES.md) ·
[CI](./CI.md) · [Benchmarks](./BENCHMARKS.md)

---

## 1. Principles that shape the code

1. **One engine.** CLI, desktop and MCP call the same crates, so the same input always gives
   the same classification (PRD principle 6).
2. **Only the Executor deletes** (SAFE-10). It lives in `scull-cleaner`. No other crate links
   filesystem-mutation code paths; desktop and MCP can't depend on `scull-cleaner` directly.
3. **Only `scull-platform` talks to the OS** (PLT-11). Enforced in CI by
   `scripts/ci/platform_boundary.py`.
4. **`scull-core` performs no I/O**: no filesystem, clock, network or process access. Every
   time-dependent function takes `now_unix` as a parameter, which keeps it deterministic and
   property-testable.
5. **Tiers only go up.** Combining evidence takes the most protective tier, and a tier never
   decreases between scan and execution (invariant I-8).
6. **Plans only narrow.** At execution the engine re-derives candidates and runs the
   intersection with the plan (SAFE-11).

## 2. Crates and dependencies

```text
  scull-cli (binary)        scull-mcp (stdio)        apps/desktop (Phase 3)
      │   │                       │                          │
      │   └───────────────────────┼──────────────┬───────────┘
      │                           ▼              ▼
      │                    scull-engine  (read-only pipeline, D-29)
      │                           │
      │     ┌──────────┬──────────┼───────────┬────────────┬─────────────┐
      │     ▼          ▼          ▼           ▼            ▼             ▼
      │  scanner   detectors     git        safety      storage      platform
      │
      └──► scull-cleaner  (Executor, the ONLY deleter; Phase 2; only the CLI links it)
              │  engine (read-only re-derivation for revalidation) · safety · storage · platform

  Everything depends on scull-core (types, no I/O). Only scull-platform calls OS APIs.
  scull-mcp and the desktop never depend on scull-cleaner (SAFE-10).
  scull-fixtures: dev-dependency only (test trees, canaries, invariants).
```

| Crate | Owns | Status |
|---|---|---|
| `scull-core` | Types: `Tier`, `Candidate`, `Evidence`, `TierReason`, `Action`, `Sizes`, IDs, `Plan`/`narrow`, `ScullError`/`ExitCode`; `config` (layers + precedence, untrusted `RepoPolicy`, root checks). No I/O. | Built: #28, #29, #30; config #52 |
| `scull-platform` | All OS facts: no-follow metadata, identity, allocated size, nlink, entry kinds, directory listing, capabilities | Unix #31; Windows #32; mounts #33; elevation #34; cloud placeholders #35; app dirs #150 |
| `scull-scanner` | Parallel bounded traversal, sizing, progress, cancellation | #36, #37, #38; reclaim estimator #51 |
| `scull-detectors` | Data-only rule format + loader; ecosystem detectors | Rules #39; Rust #40; JS #41; caches #42; large files #43 |
| `scull-git` | Worktree discovery from metadata; hardened Git access; state analysis; GIT-03 table | Discovery #44; hardened access #47; state #45; classifier #46 |
| `scull-safety` | Classification rubric (PRD §8.3); activity signals; protected roots; approvals (v1.0) | Rubric #48; activity #49; protected roots #50 |
| `scull-storage` | SQLite (WAL): scans, rollups, items, plans, approvals, audit, quarantine manifest | #53, #54 |
| `scull-cleaner` | **Executor**: the only deleter; quarantine; restore; journal; lock | Phase 2 (#67…) |
| `scull-engine` | Read-only pipeline shared by CLI, MCP and desktop (D-29): scan → detect → Git → signals (in-use, activity, protected roots, mounts, cloud) → rubric → candidates; reclaim estimates; history recording. Never depends on `scull-cleaner` | #56 |
| `scull-cli` | `scull` binary: clap commands, tables/JSON/NDJSON, exit codes | #55, #56…#59, #159 |
| `scull-mcp` | Read-only MCP server (rmcp, stdio); output safety | #60, #61 |
| `scull-fixtures` | Generated fixture trees, canaries, snapshots, invariants I-1…I-8 | #62 (+ #151 for I-6) |

## 3. Pipeline

PRD §10.1: **Discover → Classify → Explain → Plan → Preview → Authorize → Revalidate → Execute
→ Verify and log.**

```text
 roots ─► scanner ──DirSummary/entries──► detectors ──pre-candidate + partial Signals──┐
            │  (scull-platform: lstat, no-follow read_dir, identity, allocated size)   │
            │                                                                          ▼
            │        git (tracked? worktree state)  safety: activity, protected roots, in-use
            │                         └──────────────► Signals (complete) ─► rubric::classify
            │                                                                │
            ▼                                                                ▼
     sizes per subtree ───────────────────────────────────────────►  Candidate (tier, reasons,
                                                                     evidence, sizes, action)
                                                                             │
          storage: scans, rollups, items ◄───────────────────────────────────┤
                                                                             ▼
                                    Plan::new (hash, expiry, machine/user binding; refuses S3)
                                                                             │
                     human authorization (CLI confirm / desktop / `scull approve`)
                                                                             ▼
             Executor: lock ─► re-derive candidates ─► narrow(plan, fresh) ─► per item:
             revalidate (identity, type, tier, not tracked, not active) ─► delete | quarantine
             | trash | tool command ─► audit row ─► measured free-space delta
```

### 3.1 Discover: `scull-platform` and `scull-scanner`

`scull_platform::Platform` (trait; `Native` is the per-OS implementation):

```rust
pub trait Platform: Send + Sync {
    fn capabilities(&self) -> Capabilities;               // what this OS/permission set can tell us
    fn metadata(&self, path: &Path) -> Result<Metadata>;  // lstat semantics: never follows links
    fn read_dir<'a>(&'a self, path: &Path) -> Result<DirIter<'a>>; // refuses links and non-dirs
}
pub struct Metadata { kind: EntryKind, identity: Option<FileIdentity>, logical_size: u64,
                      allocated_size: Option<u64>, nlink: Option<u64>, mtime_unix: Option<i64>,
                      readonly: bool }
pub enum EntryKind { File, Dir, Symlink, ReparsePoint, Fifo, Socket, BlockDevice, CharDevice, Other }
```

- `None` means *unknown*, and unknown is never treated as safe. `Capabilities` reports which
  facts are available, so `scull doctor` and the preview can say so (PRD §8.4).
- Special files (FIFO, socket, devices) are classified from directory entries and **never
  opened** (SAFE-18). Links are recorded, never followed (SAFE-04).
- `native_path_bytes(path)` gives the platform-native encoding (bytes on Unix, UTF-16LE on
  Windows), which is what `ItemId::derive` hashes.
- Known gap (tracked): `read_dir` checks then lists, so it isn't race-free
  (`Capabilities::race_free_read_dir = false`). That's fine for read-only scanning. The
  Executor uses fd/handle-relative operations instead (P2-EXEC-01/02).

The scanner (#36) walks roots with a bounded worker pool, emits per-directory summaries
bottom-up (logical/allocated bytes, counts, newest mtime), progress and error events, stops
within 500 ms of cancellation, and never crosses into another volume without recording it.
Hardlink-aware sizing (#37) dedupes by `FileIdentity` and attributes shared bytes once.

### 3.2 Classify: detectors, Git and the rubric

- **Rules** (`rules/**/*.toml`, `scull_detectors::rules`) are data only (CLS-01…05). Each
  rule needs a project marker at a relative location (a directory name alone is never enough,
  CLS-03), may name lockfiles, and declares base tier, strategy, regeneration hints and
  category. Rules can't execute commands. User rules load disabled and are capped below S0.
  Bundled rules are compiled in.
- **Detectors** (#40, #41, #42) turn a scanned directory plus its surroundings into a
  pre-candidate, filling the `Signals` they can know (marker, lockfile, artifact known,
  regeneration cost). Callers fill the rest; unknowns default to the protective answer.
- **Git** (`scull_git`): worktree discovery reads Git metadata files only, checks pointers in
  both directions, and treats malformed or crafted metadata as a *problem*, never as a prunable
  worktree. Commands that must run `git` go through the hardened runner (#47): trusted
  binary, sanitized environment, no repo-config code execution (SAFE-12, SAFE-15).
- **Rubric** (`scull_safety::rubric`): `classify(&Signals) -> Classification` implements PRD
  §8.3 exactly. The tier is the most protective row that fired, then the activity window bumps
  it one tier. Unknown in-use or Git-tracking gives at least S2 with a `signal_unavailable`
  reason; unknown activity bumps like recent activity; no marker means S3 and not a candidate.
  `apply(&mut Candidate, &Signals)` raises through `Candidate::raise`, which never lowers.
  Verified exhaustively over all 41,472 signal combinations against an independent model.

### 3.3 Explain: `Candidate`

`scull_core::Candidate` carries everything the preview needs (SAFE-01): path, `FileIdentity`,
category, rule, project, tier + every `TierReason`, `Evidence` list, `Sizes` (logical,
allocated, reclaimable + `Confidence` + shared bytes), risk `Flags` (tri-state: `None` =
unknown), regeneration instructions/cost (`Unknown` unless evidenced, D-5), and the proposed
`Action` with its `Restorability`.

`Candidate::is_consistent()` enforces: tier equals the most protective reason (no reasons means
S3); S3 never carries a mutating action (I-1); actions allowed per tier (D-28: `Delete` only S0; tool/Git actions only S0/S1; S2 only quarantine/Trash).

### 3.4 Plan: `scull_core::plan`

- `Plan::new(PlanParams, Vec<PlanItem>)` refuses S3 items and actions not allowed for a tier,
  and computes a SHA-256 **content hash over canonical JSON** (sorted keys, items sorted by
  `ItemId`, hash field excluded).
- Expiry by origin: CLI 24 h, MCP 15 min (SAFE-11), desktop 1 h (D-25). A recorded lifetime
  longer than the origin's maximum counts as tampering.
- `verify(now, machine_id, user)`: tampered (schema version, hash, S3, disallowed action,
  duplicates) → `plan_tampered`; other machine/user → `plan_foreign`; expired or from the future
  beyond 5 min skew → `plan_expired`. All exit with code 4.
- `narrow(&plan, &fresh) -> NarrowResult`: keeps an item only if a fresh candidate has the same
  `ItemId`, identity and path, is consistent, isn't S3, has a tier ≤ the planned tier, and the
  action is unchanged or safer (quarantine > trash > delete). Items not in the plan are never
  added. Every drop has a typed `DropReason`.
- The hash detects edits but **is not a secret**. Authority comes from the plan record in the
  local DB (append-only), the approval record (single use, unexpired), and re-derivation +
  narrowing at execution.

### 3.5 Execute: `scull-cleaner` (Phase 2)

Planned per PRD §10.3–10.6 and THREAT_MODEL: a single execution lock per user (SAFE-09,
P2-EXEC-04); fd/handle-relative, no-follow deletion (P2-EXEC-01/02); revalidation before every
item (SAFE-05, P2-EXEC-03); same-volume quarantine with a DB manifest of record and
never-overwrite restore (D-3, SAFE-16, P2-EXEC-05/06); write-ahead journal and crash recovery
(SAFE-19, P2-EXEC-08); append-only audit (SAFE-08, P2-EXEC-09); measured free-space delta
(PHY-02, P2-EXEC-10); refusal to run elevated (SAFE-14, P2-EXEC-11). Tool-native commands
(Quick Clean, `git worktree remove/prune`) go through the trusted-tool runner (SAFE-12).

## 4. Errors and exit codes

`scull_core::ScullError { kind: ErrorKind, message, path?, item_id? }` is serializable and
shared by every interface. `ErrorKind::exit_code()` maps to the stable CLI codes (CLI-05):

| Code | Meaning | Kinds |
|---|---|---|
| 0 | success | — |
| 1 | general error | `io`, `permission_denied`, `not_found`, `unsupported_platform`, `internal` |
| 2 | usage/config | `usage`, `config` |
| 3 | partial failure | `partial_failure` |
| 4 | plan rejected | `plan_expired`, `plan_tampered`, `plan_foreign`, `plan_invalidated` |
| 5 | blocked by safety policy | `safety_blocked` |
| 6 | execution lock held | `lock_held` |
| 130 | cancelled | `cancelled` |

## 5. Data store (`scull-storage`, D-14)

One SQLite file per user in the platform's **non-roaming** data directory (#150), WAL mode,
owner-only permissions (DATA-04). Versioned migrations with a backup before each one; a
database newer than the binary is refused, not modified (DATA-03). Logical areas (DATA-02):

| Area | Tables (planned names) | Write rule |
|---|---|---|
| profile/config | `meta` | mutable |
| scans | `scans`, `scan_rollups` (per category/tier; no per-file rows) | append + finish |
| items | `items` (candidate lifecycle by `ItemId`) | upsert |
| plans | `plans` (canonical JSON + hash) | **append-only** (triggers) |
| approvals | `approvals` (plan, hash, method, expiry, `used_at` once) | **append-only** |
| operations | `operations`, `audit` (per-item outcome, predicted vs measured) | **append-only** |
| quarantine | `quarantine_manifest` (manifest of record, SAFE-16) | append + state |
| events, metrics | `events`, `metrics` | append, downsampled (DATA-06) |

No interface writes SQL directly (DATA-01): everything goes through `scull-storage`'s typed API.
Schema details live in the migrations under `crates/scull-storage/` (#53).

## 6. Interfaces

- **CLI** (`scull-cli`, #55): clap; human tables with explicit size labels (PHY-01) and tier
  label + non-color cue; `--json`/`--ndjson` with versioned envelopes validated against
  `schemas/` (CLI-01, #21); progress on stderr, data on stdout (CLI-02); Ctrl-C exits 130; all
  names escaped for control and bidi characters (SAFE-17).
- **MCP** (`scull-mcp`, #60/#61): stdio only, read-only by default, allowlisted roots, escaped
  and length-capped fields, home prefix redacted (MCP-01…05). Destructive tools only in v1.0
  behind config + flag, executing through the approval broker; an AI tool call is never
  consent (§13.3).
- **Desktop** (`apps/desktop`, Phase 3): Tauri 2 + Vue 3. The Rust side is a thin bridge to
  these crates and never deletes directly.

## 7. Testing strategy

- **Unit + property tests** per crate (proptest): tier monotonicity, plan narrowing, hash
  tamper detection, rubric exhaustiveness, terminal escaping.
- **Fixtures** (`scull-fixtures`, `docs/FIXTURES.md`): every safety-relevant behavior gets a
  generated tree in a temp dir with a sibling **canary** directory. After each fixture the
  harness checks invariants I-1…I-8; unimplemented invariants report `Unimplemented`, never
  pass. Dropping a fixture without checking invariants panics.
- **CI** (`docs/CI.md`): macOS, Linux and Windows on every PR; clippy `-D warnings`; rustdoc
  `-D warnings`; cargo-deny (no copyleft, advisories); platform-boundary check.
