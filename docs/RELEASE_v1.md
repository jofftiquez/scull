# v1.0 release plan and checklist

P4-REL-01 (#17): the first release of **all three surfaces from one tag**: the `scull` CLI,
the MCP server inside it (`scull mcp serve`), and the desktop app. This file is the plan and
the checklist; `RELEASING.md` has the general mechanics (dist pipeline, signing secrets,
verification commands) and stays the reference for every release. Agents prepare everything
here; only the maintainer sets secrets, pushes tags and publishes.

## 1. What ships

| Surface | Built by | Artifacts |
|---|---|---|
| CLI + MCP (`scull`) | `.github/workflows/release.yml` (dist) | 7 archives (macOS arm64/x86_64, Linux gnu/musl arm64/x86_64, Windows x86_64) with `.sha256`, `sha256.sum`, shell/PowerShell installers, Homebrew formula, Scoop manifest, CycloneDX SBOMs, attestations (RELEASING.md) |
| Desktop app | `.github/workflows/release-desktop.yml` (Tauri) | macOS `.dmg` + `.app.tar.gz` (arm64, x86_64), Windows `.msi` + NSIS `-setup.exe`, Linux `.deb`, `.rpm`, `.AppImage`; each with `.sha256`, plus `desktop-sha256.sum` and attestations |

`release-desktop.yml` runs on tags `v1*` and later only (CLI alphas never build the desktop)
and attaches its bundles to the release dist creates for the same tag.

## 2. Release gates (PRD §21.3)

Do not tag `v1.0.0` until every box is ticked, each with a link to its evidence.

**MVP gates** (must still hold):

- [ ] G1: 0 S3 items in executable plans across the fixture suite on all three OSs: green
      3-OS CI on the release commit (`gh workflow run ci.yml --ref main`) and the nightly
      fixture job (`nightly-fixtures.yml`).
- [ ] 0 open issues with severity "data loss" or "safety bypass": review
      `gh issue list -R jofftiquez/scull --label safety-critical --state open` and
      `gh issue list -R jofftiquez/scull --label bug --state open` (there are no severity labels
      yet; add `severity:data-loss` / `severity:safety-bypass` if triage needs them).
- [ ] Independent safety review of the Executor and rubric, by someone who did not write them
      (link the review).
- [ ] G3: predicted vs. measured reclaim within target on the reference machines
      (docs/BENCHMARKS.md run on the release commit).
- [ ] Published JSON schemas and exit codes: `schemas/v1/`, `crates/scull-cli/schemas/`, CLI-05
      table in the docs; `cargo test -p scull-cli --test ndjson_schemas` green.

**v1.0 gates** (in addition):

- [ ] G6: MCP approval tests pass (P4-MCP-02).
- [ ] WCAG 2.2 AA audit of the key desktop workflows (P3-A11Y-01, docs/ACCESSIBILITY.md).
- [ ] Desktop end-to-end flows green (P3-E2E-01, `desktop-e2e.yml`) and the v1.0 UI work
      merged (P4-UI-01).
- [ ] Cross-platform parity report (what differs between macOS, Windows and Linux, and why).
- [ ] Complete public documentation, including the MCP threat-model limits.

**Release-pipeline gates:**

- [ ] `python3 scripts/release/bump_version.py --check --tag v1.0.0` passes on the release
      commit (also run by `release-desktop.yml` before building).
- [ ] The **Release gates** dry run is green, and its *Signing readiness* job shows every
      signing group `ready` (RELEASING.md).
- [ ] A desktop dry run with all bundles (Actions → Release desktop → Run workflow →
      `all`) is green; with secrets set, its signature/notarization checks pass.
- [x] The inspect-only gate is replaced by the MVP gates for every non-alpha tag (the
      Executor ships in v1.0; the safety lane owns `packaging/check_mvp_gates.py` and
      `release-gates.yml`). Desktop bundles for a tag wait for the same gates
      (`release-desktop.yml`, job `gates`).

## 3. Version bump plan

One PR, through the core lane (it owns the workspace `Cargo.toml`), after the gates above are
green on `main`:

```sh
python3 scripts/release/bump_version.py 1.0.0
cargo update --workspace                                   # Cargo.lock: workspace crates
(cd apps/desktop/src-tauri && cargo update -p scull-desktop) # the Tauri crate's own lock
python3 scripts/release/changelog.py --version 1.0.0 --since <last tag> > /tmp/v1.md
# edit /tmp/v1.md, then put it at the top of CHANGELOG.md (below the intro)
python3 scripts/release/bump_version.py --check --tag v1.0.0
```

`bump_version.py` rewrites the four version strings (workspace `Cargo.toml`, the Tauri
crate's `Cargo.toml`, `tauri.conf.json`, `apps/desktop/package.json`) and nothing else; the
`release-versions` Policy job fails any PR where they disagree.

Recommended sequence: `v1.0.0-rc.1` first (a GitHub prerelease: dist does not update the
Homebrew tap or Scoop bucket for prereleases; the desktop bundles are built and attached), test
it on the three OSs, then `v1.0.0`.

## 4. Changelog

`scripts/release/changelog.py` drafts the `## <version>` section from `git log` (offline):
squash-merge subjects `[KEY] title (#PR)` grouped by area (Safety, Desktop app, CLI, MCP and
agents, Scanning and detection, Insights and monitoring, Release/packaging/CI, Research,
Other), PR links added, evidence-only `[skip ci]` commits and reverted commits dropped. The
draft always needs an edit: user-facing wording, the safety notes and "Known limitations".
dist copies that section into the GitHub release notes, so the heading must be exactly
`## 1.0.0`.

## 5. Desktop bundling and signing

`release-desktop.yml` builds on `ubuntu-22.04` (WebKitGTK 4.1), `macos-14` (arm64, and x86_64
by cross-compiling) and `windows-2022`. It reuses the CLI pipeline's secrets (RELEASING.md
"Signing"), so the maintainer sets them once:

| What | Secrets | How |
|---|---|---|
| macOS Developer ID signing (hardened runtime) | `CODESIGN_CERTIFICATE`, `CODESIGN_CERTIFICATE_PASSWORD`, `CODESIGN_IDENTITY` | passed to the Tauri bundler as `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` |
| macOS notarization + stapling | `APPLE_API_KEY` (the .p8 text), `APPLE_API_KEY_ID`, `APPLE_API_ISSUER` | written to a temporary `.p8`; the bundler gets `APPLE_API_KEY`(id), `APPLE_API_ISSUER`, `APPLE_API_KEY_PATH` |
| Windows Authenticode (exe, MSI, NSIS) | `SSLDOTCOM_USERNAME`, `SSLDOTCOM_PASSWORD`, `SSLDOTCOM_CREDENTIAL_ID`, `SSLDOTCOM_TOTP_SECRET`; optional variable `CODESIGNTOOL_SHA256` | Tauri `bundle.windows.signCommand` → `apps/desktop/scripts/sign-windows.ps1` (SSL.com CodeSignTool) |
| Linux | none | unsigned packages; `.sha256` files and build-provenance attestations |

Each group is skipped with a notice in the run log when its secrets are missing, so unsigned
dry runs work today. With secrets set, the job verifies the result (codesign, `stapler
validate`, `spctl`; `Get-AuthenticodeSignature` on every exe/MSI) and fails otherwise.

Not wired, maintainer decisions first:

- **Auto-update** (PLT-05: opt-in, signed, check-only by default). Needs a Tauri updater key
  pair (`tauri signer generate`), the public key in `tauri.conf.json`, the private key as
  `TAURI_SIGNING_PRIVATE_KEY`(+ password), `bundle.createUpdaterArtifacts`, and an update
  endpoint. Until then the app never checks for updates (no network calls).
- **Homebrew cask / winget** for the desktop app: the `.app.tar.gz` and the MSI are the inputs;
  add after v1.0 if wanted.
- **Linux package signing** (deb/rpm repositories): out of scope; GitHub release downloads
  with checksums and attestations only.

## 6. Tag and publish (maintainer)

```sh
git fetch origin && git checkout main && git pull --ff-only
python3 scripts/release/bump_version.py --check --tag v1.0.0
git tag -a v1.0.0 -m "scull 1.0.0"
git push origin v1.0.0
gh run watch "$(gh run list --workflow release.yml --limit 1 --json databaseId -q '.[0].databaseId')"
gh run watch "$(gh run list --workflow release-desktop.yml --limit 1 --json databaseId -q '.[0].databaseId')"
```

Both workflows start from the tag. The desktop workflow waits (up to 80 minutes) for the
release dist creates and then attaches its bundles; if `release.yml` fails, the desktop job
fails too and nothing desktop-related is published.

## 7. Evidence for #17

- The GitHub release page: CLI archives, installers, formula, Scoop manifest, SBOMs, desktop
  bundles, `sha256.sum` and `desktop-sha256.sum`.
- `sha256sum -c sha256.sum --ignore-missing` and `sha256sum -c desktop-sha256.sum`;
  `gh attestation verify <file> -R jofftiquez/scull` for one CLI archive and one desktop bundle.
- On each OS, a fresh install and first run:
  - macOS: open the DMG, drag to Applications, launch with no Gatekeeper warning;
    `spctl --assess --type execute -vv /Applications/scull.app`; `brew install
    jofftiquez/scull/scull && scull --version`.
  - Windows: run the MSI (or the setup exe) with no SmartScreen "unknown publisher";
    `Get-AuthenticodeSignature` on the installer; `scoop install scull/scull; scull --version`.
  - Linux: `sudo apt install ./scull_1.0.0_amd64.deb` (or run the AppImage), launch;
    `scull --version` from the installer script.
- `scull mcp serve` listed in an MCP client (`scull agent install claude-code --dry-run`).

## 8. If something goes wrong

- Before anything is published (a gate, build or signing check fails): delete the tag
  (`git push --delete origin v1.0.0 && git tag -d v1.0.0`), fix through a PR, tag again.
- Published but broken: `gh release delete v1.0.0 --cleanup-tag`, fix, and release `v1.0.1`
  (never reuse a tag that was public). If the tap or bucket were updated, revert their last
  commit.
- Desktop-only failure after the CLI release is out: re-run the failed `release-desktop.yml`
  jobs for the same tag (the upload uses `--clobber`).

## 9. Maintainer-only items

- [ ] Accounts and secrets (#11; RELEASING.md "Signing"): Apple Developer ID certificate, App
      Store Connect API key, SSL.com eSigner (or another Windows provider), tap/bucket repos and
      tokens, crates.io name.
- [ ] Updater key pair and endpoint, if auto-update ships in v1.0.
- [ ] The independent safety review.
- [ ] Tags, release notes edit, announcement.
