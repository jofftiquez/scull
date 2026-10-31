# Releasing scull

How the maintainer cuts a release. The first one is the **inspect-only alpha**
(`v0.1.0-alpha.1`, P1-REL-01 / #66, PRD §22 Phase 1). Agents prepare the pipeline; only the
maintainer pushes tags.

## What is in the pipeline

| File | Role |
|---|---|
| `dist-workspace.toml` | [cargo-dist](https://github.com/axodotdev/cargo-dist) 0.33.1 config: targets, installers, checksums. |
| `.github/workflows/release.yml` | **Generated** by `dist generate`; do not edit by hand. Runs on version tags only. |
| `.github/workflows/release-gates.yml` | Gates every build waits for; also a manual dry run. |
| `packaging/check_inspect_only.py` | The gates: no Tauri in the CLI tree; no destructive command or MCP tool wired. |
| `CHANGELOG.md` | Release notes. dist copies the `## <version>` section into the GitHub release. |
| `crates/scull-cli/Cargo.toml` | `[package.metadata.dist] dist = true`: only the `scull` binary ships. |
| `.cargo/config.toml` | `[profile.dist]` (= `release`), the profile dist builds with. |

What a tag push produces (a GitHub **prerelease** when the version has a `-alpha`/`-beta`
suffix):

- `scull-cli-<target>.tar.gz` (macOS, Linux) or `.zip` (Windows), each with a `.sha256`, for
  `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`,
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`, `x86_64-unknown-linux-musl`,
  `x86_64-pc-windows-msvc`. The binary inside is `scull` (`scull.exe`).
- `scull-cli-installer.sh` and `scull-cli-installer.ps1`: download the right archive and
  install it into `$CARGO_HOME/bin` (`~/.cargo/bin`). No self-updater. They rely on HTTPS;
  checksum verification is by hand against the `.sha256` files (step 4).
- `sha256.sum` (all checksums) and `source.tar.gz`.

Not yet (P2-REL-01, MVP): code signing and notarization, Homebrew, Scoop/winget, MSI,
crates.io, SBOM.

Job order: `plan` → `custom-release-gates` (Linux: dependency check, release build, inspect-only
check) → 7 `build-local-artifacts` jobs → `build-global-artifacts` (installers, checksums) →
`host` (creates the GitHub release and uploads) → `announce`. Nothing is uploaded unless every
gate and build passed. Rough cost per release: about 25 Linux minutes plus 2 macOS jobs (10x
multiplier on a private repository).

## Before the first alpha (blockers)

Do not tag until **all** of these hold:

- [ ] GitHub Actions is available again (billing), and `CI ok`, `cargo-deny`, `cli-deps`,
      `platform-boundary` are green on `main`.
- [ ] **#203** (engine safety gaps: worktree candidates bypassing protections, incomplete
      subtrees, unknown signals) is merged.
- [ ] Every `ci-pending` issue has a green **3-OS** CI run: #56, #60, #61, #63, #65, #151,
      #191 (`gh issue list --label ci-pending`). Pushes to `main` test Linux only, so start
      a full run by hand: `gh workflow run ci.yml --ref main`, then `gh run watch`.
- [ ] #66's dependencies are closed: #56, #61, #63, #64, #65.
- [ ] Decide how people download from a **private** repository: release assets of a private
      repo need authentication. Either make the repository public first, or tell testers to
      use `gh release download` or set `SCULL_CLI_GITHUB_TOKEN` (a token with read access)
      before running the installer.
- [ ] Linux arm64 builds use the `ubuntu-22.04-arm` runner. If it is not available to this
      repository (jobs wait in "Queued"), cancel and either drop the two `aarch64-unknown-linux-*`
      targets from `dist-workspace.toml` for this alpha or point them at an x86_64 runner via
      `[dist.github-custom-runners]`, then `dist generate`.

## Release steps

### 1. Dry run (no tag, nothing published)

On GitHub: **Actions → Release gates → Run workflow** on `main`, tick "Also build". It runs the
gates, `dist plan` (which also fails if `release.yml` is stale) and a local x86_64 Linux build
kept as a workflow artifact for 7 days.

Or locally (Linux), with `dist` installed from its
[GitHub release](https://github.com/axodotdev/cargo-dist/releases/tag/v0.33.1) after checking
its `.sha256`:

```sh
python3 packaging/check_inspect_only.py deps
cargo build -p scull-cli --release --locked
python3 packaging/check_inspect_only.py binary target/release/scull
dist plan
dist build --artifacts=local --target x86_64-unknown-linux-gnu   # local only, no upload
```

Never run `dist host` or `dist publish` locally.

### 2. Version bump PR

One PR through the **core lane** (it owns the workspace `Cargo.toml`):

1. `[workspace.package] version = "0.1.0-alpha.1"` in `Cargo.toml`, then `cargo build` to
   refresh `Cargo.lock`.
2. In `CHANGELOG.md`, make sure the `## 0.1.0-alpha.1` heading matches the version exactly and
   update "Known limitations" to the current state (drop the "not tagged until #203" line once
   #203 is merged; list any `ci-pending` issue still open).
3. Merge after CI (3 OSs) and the release-gates dry run pass.

dist refuses to release if the tag and the crate version differ.

### 3. Tag (maintainer only)

```sh
git fetch origin
git checkout main && git pull --ff-only
git log -1                       # the merged version-bump commit, CI green
git tag -a v0.1.0-alpha.1 -m "scull 0.1.0-alpha.1 (inspect-only alpha)"
git push origin v0.1.0-alpha.1
gh run watch "$(gh run list --workflow release.yml --limit 1 --json databaseId -q '.[0].databaseId')"
```

### 4. Verify and collect evidence for #66

```sh
gh release view v0.1.0-alpha.1 --web          # screenshot: prerelease, notes, 7 archives + installers
mkdir /tmp/scull-rel && cd /tmp/scull-rel
gh release download v0.1.0-alpha.1
sha256sum -c sha256.sum --ignore-missing      # screenshot: every archive OK
```

On **each** OS (macOS arm64 and/or x86_64, Linux x86_64 and/or arm64, Windows x86_64), install
fresh with the installer and run, from a checkout of the tagged commit:

```sh
scull --version
scull doctor
python3 packaging/check_inspect_only.py binary "$(command -v scull)"
```

(`(Get-Command scull).Source` instead of `command -v scull` in PowerShell.) Screenshot each
run and post them on #66 with `scripts/evidence/post.py` (screenshots are attached, never
committed).

### If something goes wrong

- A gate or build fails: nothing was published. Delete the tag
  (`git push --delete origin v0.1.0-alpha.1 && git tag -d v0.1.0-alpha.1`), fix on `main`
  through a PR, and tag again.
- The release is published but broken: `gh release delete v0.1.0-alpha.1 --cleanup-tag`, fix,
  and release the next prerelease number (`0.1.0-alpha.2`) rather than reusing the tag.

## Changing the pipeline

- Edit `dist-workspace.toml`, run `dist generate`, commit both it and the regenerated
  `release.yml`.
- Upgrading dist: change `cargo-dist-version`, run `dist generate`, and update `DIST_VERSION`
  and `DIST_SHA256` in `release-gates.yml` (from the new release's
  `cargo-dist-x86_64-unknown-linux-gnu.tar.xz.sha256`).
- The inspect-only gate stays on for every release until the Executor ships with its own MVP
  gates (PRD §21.3). Changing its list of refused commands needs a safety-lane review.
