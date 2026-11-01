# scull — Fixture Matrix

**Status:** v0.1 (Phase 0 deliverable) · **Updated:** October 31, 2026
**Companion docs:** [Architecture](./ARCHITECTURE.md) · [CI](./CI.md)

This is the list of scenarios scull must pass on every supported OS before release. Each fixture builds a small, realistic directory tree, runs scull, and checks both the specific expected outcome **and** the universal invariants in §1.

The MVP release gate (PRD §21.3) is: **every MVP-gated fixture passes on macOS, Linux and Windows, with zero invariant violations.**

---

## 1. Universal invariants (checked after every fixture)

| ID | Invariant | How it's checked |
|---|---|---|
| I-1 | **No S3 item ever appears in an executable plan** | Inspect every plan the fixture produces |
| I-2 | **Canary files outside the fixture root are untouched** | A canary directory (with files, a symlink target, and a junction target on Windows) sits beside every fixture root. Content hash, size, modification time and file identity are compared before and after |
| I-3 | Nothing inside the fixture root changes except plan items | Full tree snapshot diff (paths, identities, hashes) before vs. after |
| I-4 | The audit log matches what happened on disk | Each plan item has exactly one audit outcome consistent with the filesystem |
| I-5 | Predicted and measured bytes are both recorded | Operation record in the test database |
| I-6 | No network access unless the fixture is tagged `net` | Tests run with network disabled or blocked |
| I-7 | Only allowlisted tools are spawned, from trusted paths | Process-spawn hook in the test harness records every child process |
| I-8 | Tiers never decrease on revalidation | Compare scan-time and execute-time tiers per item |

## 2. How fixtures are built and run

- **Generated, not committed.** A `scull-fixtures` test crate builds each tree into a fresh temporary directory at test time: files, Git repos via the real `git` binary, symlinks, hardlinks, lock files and fake processes. Builders are deterministic, so failures reproduce.
- **One builder, three OSs.** Each fixture declares the OSs it runs on. OS-specific steps (junctions, APFS clones, mounts) live in the platform section of the builder.
- **Modes:**
  - **CI**: every pull request, all three OSs.
  - **Nightly**: slower or privileged tests (mounts, snapshots, 1M-entry trees, fuzzing).
  - **Manual**: needs a real desktop session or account state (e.g. iCloud, macOS Full Disk Access prompts); run before each release with a signed checklist.
- **CI environment notes:**
  - **Windows:** symlink creation needs Developer Mode or admin rights; hosted runners have admin. Junctions need neither.
  - **Linux:** mount and Btrfs tests need `sudo` (available on hosted runners) or a privileged container; run them nightly.
  - **macOS:** APFS clones via `cp -c`; local snapshots via `tmutil localsnapshot` (needs sudo; nightly or manual).
- **Gate column:** the release that first requires the fixture to pass. OS column: **M** macOS, **L** Linux, **W** Windows.

### 2.1 Running the Nightly fixtures

Nightly fixtures are ordinary tests marked `#[ignore]`, so `cargo test --workspace` (and the per-PR CI) never runs them. The scheduled workflow `.github/workflows/nightly-fixtures.yml` runs them every night on Linux and macOS (and on Windows for the OS-independent ones). It skips the scheduled run when `main` has had no commit in 25 hours, and it can be started by hand with `gh workflow run nightly-fixtures.yml`.

Run them locally with:

```sh
cargo test -p scull-fixtures --release --test fx_fs_nightly -- --ignored --nocapture --test-threads=1
```

- **Privileges.** A fixture that mounts filesystems opts in with `scull_fixtures::require_privileges!`. The test binary then re-runs that one test in a child that can mount: in-process if it already runs as real root; otherwise in an unprivileged user and mount namespace (`unshare --user --map-root-user --mount`); otherwise, only when `SCULL_FIXTURES_ALLOW_SUDO=1`, as root through `sudo -n unshare --mount`. Every Linux child gets a private mount namespace, so a forgotten mount disappears with it. macOS attaches disk images with `hdiutil`, which needs no root.
- **Skips are loud.** When none of those works, the test prints `SKIPPED <test>: <reason>` and passes. With `SCULL_FIXTURES_REQUIRE_PRIVILEGED=1` (set by the nightly workflow), it fails instead. Ubuntu 23.10 and later block unprivileged uid mapping (`kernel.apparmor_restrict_unprivileged_userns=1`), so on a default desktop install these fixtures skip unless sudo is allowed.
- **Running as root without sudo on the host.** Run the test binary in a disposable privileged container. Only the binary is shared, read-only, and every fixture tree lives in the container's own `/tmp`:
  ```sh
  cargo test -p scull-fixtures --test fx_fs_nightly --no-run   # prints the binary path
  docker run --rm --privileged -e SCULL_FIXTURES_REQUIRE_PRIVILEGED=1 \
    -v "$PWD/target/debug/deps/fx_fs_nightly-<hash>:/fx:ro" ubuntu:24.04 /fx --ignored --nocapture
  ```
- **Mounts stay inside the fixture.** `scull_fixtures::Mounts` refuses mount points and sources outside the fixture's temporary directory. It unmounts everything (innermost first) before that directory is removed, and it falls back to a lazy unmount. If even that fails, it aborts the process, so the cleanup can never walk into a mounted filesystem.
- **FX-FS-15 knobs.** `SCULL_FX_FS15_ENTRIES` (default 1,000,000), `SCULL_FX_FS15_MAX_SECS` (15) and `SCULL_FX_FS15_MAX_MB` (400) set the size and the PRD §21.2 budgets. The scan runs in a child process, once to warm the cache and once to be judged. Its peak memory comes from `VmHWM` on Linux, `/usr/bin/time -l` on macOS and `PeakWorkingSet64` on Windows. The time budget is enforced only in optimized builds (`--release`), or always when `SCULL_FX_FS15_ENFORCE_TIME=1`.

### 2.2 Manual release checklist

Manual fixtures can't be automated: they need a real desktop session, an account state, or OS permission prompts that hosted runners can't produce. Before each release, a maintainer runs this checklist on a real machine with the **release build**, ticks every box, and signs it (PRD §21.3). The signed copy is attached to the release's tracking issue. A box that can't be ticked blocks the release if the fixture's gate is that release or earlier. The fixture registry marks these fixtures `ManualChecklist`, and `tests/registry.rs` fails if a registered one has no entry here.

Record once per run: scull version and commit, OS version and build, machine model, tester, date.

**FX-MAC-01** (gate MVP). Protected folders without Full Disk Access are reported as inaccessible, with no crash and no repeated permission prompts.
Setup: a macOS desktop session. In System Settings → Privacy & Security, scull, the scull desktop app and the terminal app you use must **not** have Full Disk Access. Reset earlier answers with `tccutil reset SystemPolicyAllFiles` and `tccutil reset All <bundle id>` for the desktop app.
- [ ] From Terminal, run `scull scan ~ --json > scan.json` and confirm scanning the home folder. It exits normally, with no crash or hang.
- [ ] Folders under macOS privacy protection (e.g. `~/Library/Mail`, `~/Library/Messages`, `~/Library/Safari`, `~/Library/Application Support/AddressBook`) are listed as inaccessible (permission-denied issues) in `scan.json`. None of them is a candidate.
- [ ] Each privacy category prompted at most once during the scan. Deny every prompt.
- [ ] Run the same scan again. No new prompts appear, and the same folders are reported as inaccessible.
- [ ] `scull doctor` lists Full Disk Access as unavailable, with the explanation.
- [ ] In the desktop app, scan the home folder. Inaccessible folders are shown as such, the app stays responsive and doesn't crash. Quit, reopen and scan again: no repeated prompts.

**FX-WIN-07** (gate v1.0). A OneDrive Files On-Demand placeholder is not hydrated and is classified S2.
Setup: Windows with OneDrive signed in. A folder inside OneDrive holds a `node_modules` (or `target`) whose files are "online-only" (cloud icon, "Free up space").
- [ ] `scull scan <OneDrive folder> --json`: the artifact containing placeholders is at least S2, with a cloud or placeholder reason.
- [ ] After the scan, every placeholder still shows the cloud icon (Explorer's "Status" column). Nothing was downloaded, and OneDrive's activity shows no downloads.
- [ ] No placeholder appears in an executable plan for deletion.

**FX-MAC-02** (gate v1.0). iCloud Drive files evicted by "Optimize Mac Storage" are not hydrated and are classified S2.
Setup: macOS with iCloud Drive and "Optimize Mac Storage" on. A project in iCloud Drive with an artifact folder evicted to the cloud (`brctl evict <path>`).
- [ ] `scull scan <iCloud project> --json`: the artifact is at least S2, with a cloud or placeholder reason.
- [ ] After the scan the files are still evicted (cloud icon in Finder, `brctl status` shows no download). Nothing was downloaded.
- [ ] No evicted file appears in an executable plan for deletion.

**FX-MAC-06** (gate v1.0). A change in purgeable space during measurement is noted in the result.
Setup: macOS with purgeable space present (e.g. local Time Machine snapshots or iCloud caches; `diskutil info /` shows "Purgeable").
- [ ] Clean an S0 item while purgeable space changes, e.g. while `tmutil thinlocalsnapshots / 1000000000 1` runs.
- [ ] The result's measured delta carries a note that purgeable space changed during measurement, and the confidence is lowered accordingly.
- [ ] The note appears in `scull history` for that operation.

Signed: ____________________ (name, date, release)

### 2.3 Nightly OS fixtures

FX-LNX-02, FX-LNX-03, FX-LNX-06 and FX-MAC-05 live in `crates/scull-fixtures/tests/fx_os_nightly.rs`. They are `#[ignore]`d and get their privileges from `scull_fixtures::require_privileges!`. Run them with:

```sh
cargo test -p scull-fixtures --test fx_os_nightly -- --ignored --nocapture --test-threads=1
```

On Linux, FX-LNX-03 and FX-LNX-06 need real root, either directly or through `SCULL_FIXTURES_ALLOW_SUDO=1`, which runs `sudo -n unshare --mount` so every mount stays in a private namespace. They also need `bindfs` and `btrfs-progs`. FX-LNX-03 additionally mounts an NFS export when `SCULL_FX_NFS_EXPORT` (e.g. `127.0.0.1:/srv/scull-nfs`) names one. FX-LNX-02 needs a user and mount namespace or root. FX-MAC-05 attaches a disk image with `hdiutil`, which needs no root. Without privileges the tests print `SKIPPED <test>: <reason>`. With `SCULL_FIXTURES_REQUIRE_PRIVILEGED=1` they fail instead. `.github/workflows/nightly-os-fixtures.yml` runs them every night once Phase 2 is on `main`.

---

## 3. Fixtures

### 3.1 Classification (FX-CLS)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-CLS-01 | `target/` in a Cargo project with `Cargo.toml` and `Cargo.lock`, idle 3 days | S0 with marker and lockfile evidence | MLW | MVP | CI |
| FX-CLS-02 | A folder named `target/` with photos, no `Cargo.toml` anywhere above | Not a candidate | MLW | MVP | CI |
| FX-CLS-03 | `dist/` committed to Git in a JS project | S3, reason "git-tracked"; never in a plan | MLW | MVP | CI |
| FX-CLS-04 | Shared `CARGO_TARGET_DIR` outside two projects | One candidate, attributed to both projects, bytes counted once | MLW | MVP | CI |
| FX-CLS-05 | `node_modules` with `package-lock.json` / `pnpm-lock.yaml` vs. without any lockfile | With lockfile: S0. Without: S2 | MLW | MVP | CI |
| FX-CLS-06 | `node_modules` containing `npm link` / workspace symlinks to sibling source folders | Deleting `node_modules` removes the links only; linked source folders intact | MLW | MVP | CI |
| FX-CLS-07 | `.venv` with `uv.lock` vs. without a lockfile | With: S1. Without: S2 | MLW | v1.0 | CI |
| FX-CLS-08 | pnpm monorepo with 30 packages, each with `node_modules` | Every candidate found once; totals match independent measurement | MLW | MVP | CI |
| FX-CLS-09 | `target/` with a file modified 1 hour ago | Bumped one tier (S0 → S1), reason "recent activity" | MLW | MVP | CI |
| FX-CLS-10 | `build/` with `CMakeLists.txt` but no `CMakeCache.txt` | Not S0 (S2 or not a candidate) | MLW | v1.x | CI |
| FX-CLS-11 | Project containing `.scull-protect` | All candidates inside are S3 | MLW | MVP | CI |
| FX-CLS-12 | Scan root is the home folder itself | Home, OS folders, `~/.ssh`, `~/.gnupg` never become candidates; confirmation required to scan home (CFG-04) | MLW | MVP | CI |
| FX-CLS-13 | Hugging Face cache and Ollama models | Models: S1; datasets without recorded source: S2 | MLW | v1.0 | CI |
| FX-CLS-14 | Unity project with `Library/` and Unreal project with `DerivedDataCache/` and `Saved/` | `Library/`, `DerivedDataCache/`: S1. `Saved/`: S2 | MW | v1.x | CI |

### 3.2 Git worktrees (FX-GIT)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-GIT-01 | Clean, unlocked worktree; branch merged into default and pushed | Whole worktree S1 (never S0); branch preserved after removal | MLW | MVP | CI |
| FX-GIT-02 | Modified tracked file | S3; artifact-only cleanup offered | MLW | MVP | CI |
| FX-GIT-03 | Untracked, non-ignored file | S3 | MLW | MVP | CI |
| FX-GIT-04 | Ignored `.env` and `dev.sqlite` not matched by any artifact rule | ≥ S2; both files listed by name in the preview | MLW | MVP | CI |
| FX-GIT-05 | Commits not pushed to any remote | S3 | MLW | MVP | CI |
| FX-GIT-06 | Branch squash-merged upstream (patch-equivalent), no forge lookup | S2, "merge unproven" | MLW | MVP | CI |
| FX-GIT-07 | Detached HEAD at a commit reachable from no ref | S3 | MLW | MVP | CI |
| FX-GIT-08 | Detached HEAD at a commit reachable from a pushed ref, clean | S1 | MLW | MVP | CI |
| FX-GIT-09 | Locked worktree | S3; lock reason shown | MLW | MVP | CI |
| FX-GIT-10 | Registered worktree whose folder was deleted | `git worktree prune` candidate, S0 (metadata only) | MLW | MVP | CI |
| FX-GIT-11 | No upstream configured; commits exist only locally | S3 | MLW | MVP | CI |
| FX-GIT-12 | Branch pushed recently, but local remote-tracking refs are stale | Classified protectively (S3/S2), never lower; no fetch performed | MLW | MVP | CI |
| FX-GIT-13 | Submodule inside a worktree with local changes | S3 | MLW | MVP | CI |
| FX-GIT-14 | A process has its working directory inside the worktree | S3 for this run, "in use" | MLW | MVP | CI |
| FX-GIT-15 | **Malicious repo:** `core.fsmonitor` set to a command that creates a sentinel file | Sentinel never created; scan completes | MLW | MVP | CI |
| FX-GIT-16 | Bare repository with worktrees; one worktree's `.git` file points outside the scan root | Found via Git metadata; reported correctly; nothing outside root touched | MLW | MVP | CI |
| FX-GIT-17 | 60 agent-style worktrees in mixed states | Each classified per its own state; totals correct; run time within budget | MLW | MVP | CI |

### 3.3 Filesystem edge cases (FX-FS)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-FS-01 | Symlink inside a candidate pointing to the canary directory | Link removed; canary intact (I-2) | MLW | MVP | CI |
| FX-FS-02 | Symlink loop inside a scan root | Scan terminates; loop reported once | MLW | MVP | CI |
| FX-FS-03 | Candidate path reached through a symlink pointing outside the scan root | S3 | MLW | MVP | CI |
| FX-FS-04 | Mount (bind/loop) inside a candidate | S3; mounted content untouched | L (M via disk image) | MVP | Nightly |
| FX-FS-05 | File hardlinked between a candidate and the canary directory | Canary file content and links intact; bytes attributed once | MLW | MVP | CI |
| FX-FS-06 | 10 GB sparse file with 1 MB allocated | Allocated size used for estimates | MLW | MVP | CI |
| FX-FS-07 | Read-only files and folders inside a candidate | Deleted if permitted within the candidate, or reported; permissions outside the candidate never changed | MLW | MVP | CI |
| FX-FS-08 | Permission-denied subfolder inside a candidate | Item reported as partial/failed with reason; no escalation | MLW | MVP | CI |
| FX-FS-09 | Unicode names: NFC vs. NFD duplicates, emoji, right-to-left override characters | Correct identity handling; right-to-left override escaped and flagged | MLW | MVP | CI |
| FX-FS-10 | Names containing newlines, tabs, ANSI escape codes | Escaped in table output; valid JSON; terminal state unchanged | MLW | MVP | CI |
| FX-FS-11 | Protected folder referenced through a case-different alias | Still protected (identity-based comparison) | MW | MVP | CI |
| FX-FS-12 | Path longer than 4096 bytes (Linux) / 260 characters (Windows) | Scanned and deleted correctly | MLW | MVP | CI |
| FX-FS-13 | Folder named `--force` or `-rf` passed as a root or target | Treated as a path, never as an option | MLW | MVP | CI |
| FX-FS-14 | FIFO, socket and device node inside a candidate | Never opened; scan doesn't hang; entries removed or item flagged | ML | MVP | CI |
| FX-FS-15 | 1M-entry tree | Completes within performance targets; memory bounded (PRD §21.2) | MLW | MVP | Nightly |

### 3.4 Races and crash recovery (FX-RACE)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-RACE-01 | Candidate replaced by a symlink to the canary directory between plan and execute | Item skipped (identity mismatch); canary intact | MLW | MVP | CI |
| FX-RACE-02 | Subfolder swapped for a symlink **during** the deletion walk (injected via test hook) | Handle-relative operations prevent escape; canary intact | MLW | MVP | CI |
| FX-RACE-03 | New untracked file appears in a worktree after the scan | Revalidation raises tier; item skipped | MLW | MVP | CI |
| FX-RACE-04 | Build starts after the scan (Cargo lock held) | Item skipped, "in use" | MLW | MVP | CI |
| FX-RACE-05 | Two executors start at once | Second exits with code 6; no overlap | MLW | MVP | CI |
| FX-RACE-06 | Cancel (Ctrl-C) mid-execution | Consistent state; audit shows partial; rerun completes remaining items | MLW | MVP | CI |
| FX-RACE-07 | Process killed during a quarantine move | On next start the journal reconciles; no orphaned or double-counted items | MLW | MVP | CI |

### 3.5 Executor, plans, quarantine, restore (FX-EXEC)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-EXEC-01 | S0 item deleted after confirmation | Gone; measured free-space delta reported | MLW | MVP | CI |
| FX-EXEC-02 | S1 item quarantined, then restored | Same-volume rename; restored tree identical (hashes) | MLW | MVP | CI |
| FX-EXEC-03 | Quarantine impossible (read-only parent); then Trash also unavailable | Falls back to Trash; if no Trash, item blocked. **Never** permanent deletion | MLW | MVP | CI |
| FX-EXEC-04 | Restore when the original path now exists | Restored to a sibling path; nothing overwritten | MLW | MVP | CI |
| FX-EXEC-05 | Tampered quarantine folder manifest pointing restore at `~/.ssh` | Refused; database manifest is authoritative | MLW | MVP | CI |
| FX-EXEC-06 | Quarantine retention expires | Purged; freed bytes recorded | MLW | MVP | CI |
| FX-EXEC-07 | Plan file edited to add a path | Rejected, exit code 4 | MLW | MVP | CI |
| FX-EXEC-08 | Expired plan | Rejected, exit code 4 | MLW | MVP | CI |
| FX-EXEC-09 | Plan created on another machine or user | Rejected, exit code 4 | MLW | MVP | CI |
| FX-EXEC-10 | Plan item became S3 since planning | Skipped with reason | MLW | MVP | CI |
| FX-EXEC-11 | Permanent deletion of S1 without `--allow-permanent` | Refused | MLW | MVP | CI |
| FX-EXEC-12 | Destructive command run as root/administrator | Refused without `--allow-root` | MLW | MVP | CI |
| FX-EXEC-13 | Normal run | Audit log append-only, one outcome per item | MLW | MVP | CI |
| FX-EXEC-14 | One of five items fails (locked or permission denied) | Exit code 3; other four complete; failure explained | MLW | MVP | CI |
| FX-EXEC-15 | Auto-policy run (v1.x) | Only S0 touched; quarantine-first; digest generated | MLW | v1.x | CI |
| FX-EXEC-16 | Quarantine on a shared volume | Folder owner-only; another test user can't read it | MLW | MVP | Nightly |

### 3.6 Physical reclaim accuracy (FX-PHY)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-PHY-01 | pnpm project: `node_modules` hardlinked to the store | Predicted reclaim ≈ 0 with explanation; measured delta agrees | MLW | MVP | CI |
| FX-PHY-02 | APFS clone (`cp -c`) of a large file | Confidence "low"; measured delta reported and explained | M | MVP | CI |
| FX-PHY-03 | Time Machine local snapshot holds deleted data | Warning shown; measured delta lower than predicted and explained | M | v1.0 | Manual |
| FX-PHY-04 | Btrfs snapshot holds deleted data | Same as above | L | v1.0 | Nightly |
| FX-PHY-05 | Compressed files (APFS, NTFS compression) | Allocated, not logical, size used | MW | MVP | CI |
| FX-PHY-06 | Reference corpus of realistic projects | Median prediction error ≤ 10% on supported filesystems (G3) | MLW | MVP | Nightly |
| FX-PHY-07 | Item goes to Trash (fallback) | Reported as "moved", not "freed" | MLW | MVP | CI |

### 3.7 Quick Clean: package caches (FX-QC)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-QC-01 | npm cache present | Native command used; measured freed space | MLW | MVP | CI |
| FX-QC-02 | pnpm store referenced by projects | `pnpm store prune` only; store folder never removed directly; explanation of partial reclaim | MLW | MVP | CI |
| FX-QC-03 | Package manager install in progress (lock held) | That tool skipped and reported | MLW | MVP | CI |
| FX-QC-04 | Cache path is a symlink to an unexpected location | Skipped and reported | MLW | MVP | CI |
| FX-QC-05 | Tool uninstalled, cache left behind | Folder removed through the safe executor | MLW | MVP | CI |
| FX-QC-06 | **Fake `npm` earlier in `PATH` and in a project `node_modules/.bin`**, each creating a sentinel | Neither runs; sentinel absent; resolved path shown in preview | MLW | MVP | CI |
| FX-QC-07 | **Project `.npmrc` sets `cache=` to the home folder** | Command run from a neutral directory; unexpected cache path refused | MLW | MVP | CI |
| FX-QC-08 | User unticks two tools in the Quick Clean card | Only ticked tools cleaned | MLW | MVP | CI |

### 3.8 Configuration and rules (FX-CFG)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-CFG-01 | Repo `.scull.toml` tries to lower tiers to S0 | Ignored with a warning | MLW | MVP | CI |
| FX-CFG-02 | Repo config adds a scan root or enables a rule | Ignored with a warning | MLW | MVP | CI |
| FX-CFG-03 | Repo `.scull-protect` / config adds protection | Honored | MLW | MVP | CI |
| FX-CFG-04 | User config protects a path | Honored everywhere (CLI, desktop, MCP) | MLW | MVP | CI |
| FX-CFG-05 | Community rule assigns S0 without passing fixtures | Rejected at load | MLW | v1.x | CI |
| FX-CFG-06 | Rule file contains a command-execution field | Rejected at load | MLW | MVP | CI |

### 3.9 MCP and agents (FX-MCP)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-MCP-01 | `execute_cleanup` without approval | Fails closed | MLW | v1.0 | CI |
| FX-MCP-02 | Approval for a different plan hash | Fails | MLW | v1.0 | CI |
| FX-MCP-03 | Expired approval | Fails | MLW | v1.0 | CI |
| FX-MCP-04 | Approval reused for a second execution | Fails | MLW | v1.0 | CI |
| FX-MCP-05 | Filenames and branch names containing prompt-injection text | Returned escaped, structured, length-capped; no change in tool behavior | MLW | MVP | CI |
| FX-MCP-06 | Roots outside the allowlist; `../` traversal; option-like strings | Rejected | MLW | MVP | CI |
| FX-MCP-07 | Destructive tools with only the flag, or only the config | Hidden; both required | MLW | v1.0 | CI |
| FX-MCP-08 | `scull approve` without an interactive terminal | Refused | MLW | v1.0 | CI |
| FX-MCP-09 | Default redaction | Home prefix shown as `~` in all MCP results | MLW | MVP | CI |
| FX-MCP-10 | `register_worktree` with an expired TTL | Staleness alert only; nothing deleted | MLW | v1.0 | CI |

### 3.10 Windows-specific (FX-WIN)

| ID | Scenario | Expected | Gate | Mode |
|---|---|---|---|---|
| FX-WIN-01 | Junction inside a candidate pointing at the canary folder | Junction removed; target intact | MVP | CI |
| FX-WIN-02 | Directory and file symlinks inside a candidate | Links removed; targets intact | MVP | CI |
| FX-WIN-03 | File held open by another process | Item partial/skipped; Restart Manager names the process | MVP | CI |
| FX-WIN-04 | Paths longer than 260 characters | Handled via `\\?\` paths | MVP | CI |
| FX-WIN-05 | Protected folder referenced by its 8.3 short name | Still protected | MVP | CI |
| FX-WIN-06 | Recycle Bin fallback | Works; reported as "moved" | MVP | CI |
| FX-WIN-07 | OneDrive Files On-Demand placeholder | Not hydrated; S2 | v1.0 | Manual |
| FX-WIN-08 | Defender real-time protection on | Scan completes; timing recorded for benchmarks | MVP | CI |
| FX-WIN-09 | Dev Drive (ReFS) block-cloned files | Confidence "low"; measured delta reported | v1.0 | Nightly |
| FX-WIN-10 | Reserved names (`CON`, `NUL`), trailing dots and spaces | Scanned and deleted correctly | MVP | CI |
| FX-WIN-11 | Files with the read-only attribute | Handled within the candidate only | MVP | CI |
| FX-WIN-12 | Data location | Database in non-roaming local app data, owner-only ACL | MVP | CI |

### 3.11 macOS-specific (FX-MAC)

| ID | Scenario | Expected | Gate | Mode |
|---|---|---|---|---|
| FX-MAC-01 | Protected folders without Full Disk Access | Reported as inaccessible; no crash; no repeated permission prompts | MVP | Manual |
| FX-MAC-02 | iCloud Drive evicted ("Optimize storage") files | Not hydrated; S2 | v1.0 | Manual |
| FX-MAC-03 | Firmlinked paths (`/System/Volumes/Data`) | No double counting | MVP | CI |
| FX-MAC-04 | `.app` bundles and packages inside scan roots | Treated as folders; not candidates unless a rule matches | MVP | CI |
| FX-MAC-05 | Trash fallback on an external volume | Uses that volume's `.Trashes`; reported as "moved" | MVP | Nightly |
| FX-MAC-06 | Purgeable space changes during measurement | Noted in the result | v1.0 | Manual |

### 3.12 Linux-specific (FX-LNX)

| ID | Scenario | Expected | Gate | Mode |
|---|---|---|---|---|
| FX-LNX-01 | Headless server with no Trash | Quarantine used; if impossible, blocked | MVP | CI |
| FX-LNX-02 | freedesktop Trash on another volume (`$topdir/.Trash-$uid`) | Correct per-volume Trash | MVP | Nightly |
| FX-LNX-03 | NFS/SMB/sshfs mount inside a scan root | Skipped by default; no hang (timeout) | MVP | Nightly |
| FX-LNX-04 | `/proc` open-file detection with and without permission | Works; warns when unavailable | MVP | CI |
| FX-LNX-05 | Flatpak sandbox (if packaged) | Clear message about filesystem access limits | v1.x | Manual |
| FX-LNX-06 | Btrfs subvolume inside a candidate | Treated as a mount boundary (S3) | MVP | Nightly |

### 3.13 Data store and privacy (FX-DATA)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-DATA-01 | Schema migration | Backup taken first; migration succeeds; older binary refuses the newer schema | MLW | MVP | CI |
| FX-DATA-02 | Storage budget reached | Oldest raw events downsampled first; budget respected | MLW | v1.0 | CI |
| FX-DATA-03 | Database file permissions | Owner-only | MLW | MVP | CI |
| FX-DATA-04 | `scull data purge --all` | All recorded data removed | MLW | MVP | CI |
| FX-DATA-05 | Database busy (another scull reading) | Readers proceed; writer waits; no corruption | MLW | MVP | CI |
| FX-DATA-06 | Corrupted database file | Detected; backup restore offered; read-only scan still works | MLW | MVP | CI |
| FX-DATA-07 | Audit logs and `doctor --report` | Contain no file contents or environment values; report shows a redaction preview | MLW | MVP | CI |

### 3.14 Output contracts (FX-OUT)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-OUT-01 | All JSON/NDJSON outputs | Validate against published schemas | MLW | MVP | CI |
| FX-OUT-02 | Each documented exit condition | Returns the documented exit code (PRD §12) | MLW | MVP | CI |
| FX-OUT-03 | Identical input via CLI and MCP | Identical classifications, tiers and plan contents | MLW | MVP | CI |

---

## 4. Coverage map: threats → fixtures

| Threat | Fixtures |
|---|---|
| T-01 Misclassification | FX-CLS-02, -03, -05, -07, -10 |
| T-02 Stale Git info | FX-GIT-05, -06, -11, -12 |
| T-03 Ignored unique files | FX-GIT-04 |
| T-04 Missed activity | FX-CLS-09, FX-RACE-04 |
| T-05 Auto-policy | FX-EXEC-15 |
| T-10 Link escape | FX-CLS-06, FX-FS-01, -03, FX-WIN-01, -02 |
| T-11 TOCTOU | FX-RACE-01, -02 |
| T-12 Mount crossing | FX-FS-04, FX-LNX-06 |
| T-13 Alias bypass | FX-FS-09, -11, FX-WIN-05 |
| T-14 Special files | FX-FS-14 |
| T-15 Crash recovery | FX-RACE-06, -07 |
| T-20 Repo config | FX-CFG-01, -02, -03 |
| T-21 Git config execution | FX-GIT-15 |
| T-22 Tool hijack | FX-QC-06 |
| T-23 Tool config redirect | FX-QC-07 |
| T-24 Argument injection | FX-FS-13 |
| T-25 Malicious rule | FX-CFG-05, -06 |
| T-30 Plan tampering | FX-EXEC-07, -08, -09, -10 |
| T-31 Concurrent executors | FX-RACE-05 |
| T-32 Quarantine manifest | FX-EXEC-04, -05 |
| T-33 Quarantine exposure | FX-EXEC-16 |
| T-34 Misleading permanence | FX-EXEC-11 |
| T-35 Running as root | FX-EXEC-12 |
| T-40 Prompt injection | FX-MCP-05 |
| T-41 Self-approval | FX-MCP-08 |
| T-42 Approval misuse | FX-MCP-01, -02, -03, -04 |
| T-43 MCP scope escape | FX-MCP-06 |
| T-44 Path leakage | FX-MCP-09 |
| T-50 Database exposure | FX-DATA-03, FX-WIN-12 |
| T-51 Output spoofing | FX-FS-09, -10 |
| T-52 Placeholder hydration | FX-WIN-07, FX-MAC-02 |
| T-53 Scan hangs | FX-FS-02, -15, FX-LNX-03 |
| T-56 Secrets in logs | FX-DATA-07 |

T-54 and T-55 (supply chain, release channel) are covered by CI policy and the release checklist, not fixtures.

## 5. Count by gate

| Gate | Fixtures |
|---|---|
| MVP | 113 |
| v1.0 | 16 |
| v1.x | 5 |
| **Total** | **134** |

Every bug that causes or nearly causes data loss adds a new fixture before the fix is merged.
