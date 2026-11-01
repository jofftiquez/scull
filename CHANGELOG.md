# Changelog

cargo-dist copies the section whose heading matches the tagged version into the GitHub
release notes, so keep one `## <version>` heading per release (see RELEASING.md).

## Unreleased

Cleanup arrives: `scull clean` (reviewed plans, quarantine for S1/S2, deletion for S0),
`scull clean caches` (Quick Clean through each tool's own command), `scull restore`,
`scull history`, `scull data`, `scull insights purge`, the MCP approval broker
(`scull approve`, `scull mcp serve --allow-destructive`) and the emergency headroom file.
Every destructive path goes through the Executor (lock, write-ahead journal, audit,
revalidation right before each item). Release builds are gated by the MVP release gates
(`packaging/check_mvp_gates.py`) instead of the inspect-only gate. The alpha notes below are
kept as history.

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
