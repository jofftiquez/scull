# CI

Workflow: `.github/workflows/ci.yml`.

| Job | Runs on | What |
|---|---|---|
| `rustfmt` | Linux | `cargo fmt --all --check` |
| `test (<os>)` | macOS, Linux, Windows on PRs; Linux on pushes to `main` | `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace`, `cargo doc -D warnings` (Linux) |
| `CI ok` | Linux | Passes only if every job above passed. |
| `cargo-deny` (`policy.yml`) | Linux | Licenses (permissive only, no copyleft), RustSec advisories (same DB as cargo-audit), yanked crates, sources. Config: `deny.toml`. Also weekly. |
| `platform-boundary` (`policy.yml`) | Linux | `scripts/ci/platform_boundary.py`: no `std::os::unix/windows`, `libc`, `windows-sys`, Apple frameworks or `extern "C"` outside `crates/scull-platform` (PLT-11). Test code and `crates/scull-fixtures` are exempt. Self-tested first. |
| `release-versions` (`policy.yml`) | Linux | `scripts/release/bump_version.py --check`: the workspace, Tauri crate, `tauri.conf.json` and `package.json` versions agree (one tag releases everything); self-tests of the version-bump and changelog scripts. |
| `release-desktop` (`release-desktop.yml`) | Linux on PRs that touch the bundling; macOS, Windows, Linux on `v1*` tags | Tauri bundles (DMG, MSI/NSIS, deb/rpm/AppImage), signed and notarized when the secrets exist, attached to the tag's release. PRs and manual runs are dry runs. See `docs/RELEASE_v1.md`. |

| `cli-deps` (`policy.yml`) | Linux | `packaging/check_inspect_only.py deps`: the `scull` binary's dependency tree has no Tauri/webview crates. Also self-tests the inspect-only gate and the release packaging scripts (`packaging/test_release_packaging.py`: CycloneDX SBOM, Scoop manifest). |
| `release-gates` (`release-gates.yml`) | Linux | Runs before every release build (and as a manual dry run): inspect-only binary gate (every destructive command still a stub, MCP read-only, canary unchanged), `dist plan`; the dry run also builds the SBOMs, one archive with its installers, Homebrew formula and `sha256.sum`, and reports which signing secrets are set. See `RELEASING.md`. |
| `release-extras`, `release-packages` | Linux, macOS, Windows | Tag releases only (called by `release.yml`): SBOMs, Scoop manifest, macOS notarization and signature checks; then the Homebrew tap and Scoop bucket. Each signing/publishing step is skipped with a notice when its secrets are absent. See `RELEASING.md`. |

**Required checks:** `CI ok`, `cargo-deny`, `platform-boundary`.

- Docs/design/task-only changes skip CI (`paths-ignore`).
- Superseded runs on the same branch are cancelled.
- Evidence screenshots are attached to issue comments (`gh issue comment --attach`), never committed (D-31).
- Windows runners enable `core.longpaths` and disable `autocrlf` so fixtures see real bytes.

## Branch protection (maintainer)

Require the `CI ok`, `cargo-deny` and `platform-boundary` checks and one review on `main`, squash merges, auto-delete branches
Branch protection on a **private** repository needs GitHub Pro/Team;
on a public repository it is free.

## Running the policy checks locally

```bash
cargo deny check                         # https://github.com/EmbarkStudios/cargo-deny
python3 scripts/ci/platform_boundary.py
```

The boundary check exempts the item right after a `#[cfg(test)]` attribute (found by brace
counting), files under `tests/`/`benches/`, `[dev-dependencies]`, and `crates/scull-fixtures`.

## Windows is advisory during the MVP push

Provisional maintainer decision (Nov 1, 2026), revisit before v1.0: the `test (windows-latest)`
job still runs on every push to `main`/`phase2`, nightly and on demand, but it is
`continue-on-error`, so a Windows failure does not fail `CI ok` or block merges. Linux and macOS
remain required. Windows failures are filed as issues and fixed outside the critical path.
Pull requests continue to run Linux only.
