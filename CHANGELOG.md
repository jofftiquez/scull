# Changelog

cargo-dist copies the section whose heading matches the tagged version into the GitHub
release notes, so keep one `## <version>` heading per release (see RELEASING.md).

## Unreleased

The MVP: safe cleanup. The [CLI reference](docs/user/CLI.md) and the
[safety model](docs/user/SAFETY.md) describe every command and safeguard below.

### Added

- **Cleanup.** `scull clean` plans a cleanup (a dry run by default), reviews a plan against a
  fresh scan, and runs it after your confirmation. S0 items are deleted. S1 and S2 items go to
  a same-drive quarantine for 7 days, or to the Trash where quarantine is impossible, and are
  left alone when neither works. Every item is checked again right before it is touched, one
  cleanup runs at a time, every step is journaled for crash recovery, and every outcome is
  audited.
- `scull history` and `scull restore`: past operations and putting quarantined items back,
  never overwriting. `scull clean --purge-quarantine` permanently deletes expired quarantine,
  with a stronger confirmation.
- `scull clean caches` (Quick Clean): package-manager caches cleaned with each tool's own
  command, including ccache, Conan, Arduino CLI, Deno and Cypress, each limited to the folder
  scull found. Tools whose command can't be limited that way (CocoaPods, Playwright,
  Puppeteer) are refused. Orphaned caches of uninstalled tools are removed only with a
  confirmation for that tool.
- `scull data purge` and `scull insights purge`: forget scull's own files or recorded trends.
- `scull headroom create|release|status`: an opt-in emergency reserve file, released on
  command when the disk is full. Only the exact file scull created (random marker, identity,
  size and modification time) is ever released.
- `scull check` and `scull schedule enable|disable|status`: a periodic, low-priority check
  through launchd, systemd or Task Scheduler, with forecasts, alerts and a ready-to-review plan
  that never runs by itself. One disk-full forecast is shared by every surface
  ([FORECAST.md](docs/FORECAST.md)).
- MCP destructive mode (opt-in twice): an agent can prepare a plan, and only you approve it
  in your own terminal with `scull approve`, or in the desktop app's approval dialog, and can
  withdraw an approval (`--revoke`, or **Revoke approval** in the app). Denials are final,
  pending plans and the preparation rate are capped, and everything is audited.
- `scull approve <plan-id> --run`: approve an agent's plan and run it yourself, in one
  terminal step (#102; review fixes follow).
- Opt-in auto-policies (`[auto]` in your user config file only, writable only by you): the
  scheduled `scull check` moves idle S0 build outputs into quarantine. It never deletes them
  and never uses the Trash. It runs a dry run first, stays within per-run and per-day caps,
  and works under the cleanup lock. Any kill switch stops it (`--no-auto-policies`,
  `SCULL_NO_AUTO_POLICIES`, an `auto-policies.off` file or `paused = true`), and it never runs
  as root (#74).
- `scull ask`: an optional assistant with your own model (or a local one) over scull's
  read-only tools, with a consent notice before the first request. With `[mcp] destructive =
  true` it may also prepare a plan, which runs only after you approve it (#105).
- `scull agent install|uninstall`, `scull insights`, `scull worktrees register --ttl`,
  `scull doctor --report`.
- In the repository, not in the CLI release: the desktop app (Tauri, Phase 3), with menu bar
  and tray monitoring and an opt-in weekly digest.

## 0.1.0-alpha.1

> **Alpha, inspect-only: no cleanup commands.** This build can scan and explain what is using
> your disk. It cannot delete, move, quarantine or restore anything: every cleanup command
> exits with "not implemented yet", and the MCP server exposes read-only tools only. The
> release workflow refuses to build if that is not true (`packaging/check_inspect_only.py`).

Unsigned builds (code signing and notarization come with the MVP, P2-REL-01). The installers
below do not trigger Gatekeeper; if you download an archive with a browser on macOS, clear the
quarantine attribute (`xattr -d com.apple.quarantine scull`). On Windows, SmartScreen may warn
on first run.

### What works

- `scull scan [PATH...]`: sizes, reclaim estimates and risk tiers (S0-S3) with the evidence for
  each, as a table, `--json` or `--ndjson`.
- `scull projects [inspect PATH]`: detected projects and their build artifacts (Rust, JS/TS).
- `scull worktrees [inspect PATH]`: Git worktrees and their state (dirty, unpushed, locked,
  in use), read with hardened Git calls that never run repository code.
- `scull caches`: package-manager caches and their sizes (read-only).
- `scull large-files PATH`: large files under a path (always inspect-only).
- `scull status`: cached one-line status for shell prompts.
- `scull doctor`: platform capabilities, data directories and anything degraded.
- `scull init`: first-run onboarding (toolchains, roots, profile).
- `scull rules list | validate`: bundled detection rules.
- `scull mcp serve`: opt-in, read-only MCP server over stdio with allowlisted roots and
  escaped, size-capped, path-redacted output. `--allow-destructive` has no effect.
- `scull completions SHELL`. JSON Schemas for every `--json`/`--ndjson` output ship in
  `crates/scull-cli/schemas/`.

### What does not work yet

- No cleanup of any kind: `scull clean`, `clean caches`, `restore`, `approve`, `history`,
  `data purge`, `insights` all exit 1 with "not implemented yet". Safe execution (plans,
  revalidation, Trash/quarantine, restore, audit) is Phase 2 (MVP).
- No Homebrew, Scoop/winget, MSI or crates.io packages; no signed builds; no self-updater.
- No desktop app (Phase 3).

### Known limitations

- **Verified locally on Linux only.** The pull requests below merged while GitHub Actions was
  unavailable; they were checked on Linux plus a Windows/macOS type-check, not on real macOS
  and Windows runners (`ci-pending`): #56, #60, #61, #63, #65, #151, #191.
- Engine safety gaps found in review are tracked in #203 (worktree candidates skipping
  protections, incomplete subtrees, unknown signals). This alpha deletes nothing, so they can
  only show a tier that is less protective than it should be. **This release is not tagged
  until #203 is merged.**
- Classification covers Rust and JS/TS ecosystems plus Git worktrees and common package
  caches; other ecosystems arrive in Phase 5.
- Report wrong tiers or crashes as issues; anything that could lead to data loss is a security
  report (SECURITY.md).

### Checksums

Every archive has a `.sha256` file, and `sha256.sum` lists them all. To verify by hand,
download the archive and run `sha256sum -c sha256.sum --ignore-missing` (macOS:
`shasum -a 256 -c`; Windows: compare `Get-FileHash <file>` with the `.sha256` file).
