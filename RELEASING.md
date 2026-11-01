# Releasing scull

How the maintainer cuts a release. The first one is the **inspect-only alpha**
(`v0.1.0-alpha.1`, P1-REL-01 / #66, PRD §22 Phase 1); the **MVP** (P2-REL-01 / #15) adds code
signing and notarization, Homebrew and Scoop, SBOMs and build-provenance attestations. Agents
prepare the pipeline; only the maintainer creates accounts, sets secrets and pushes tags.

## What is in the pipeline

| File | Role |
|---|---|
| `dist-workspace.toml` | [cargo-dist](https://github.com/axodotdev/cargo-dist) 0.33.1 config: targets, installers, checksums. |
| `.github/workflows/release.yml` | **Generated** by `dist generate`; do not edit by hand. Runs on version tags only. |
| `.github/workflows/release-gates.yml` | Gates every build waits for; also a manual dry run. |
| `packaging/check_inspect_only.py` | The gates: no Tauri in the CLI tree; no destructive command or MCP tool wired. |
| `.github/build-setup.yml` | Steps dist puts in every build job: reports whether macOS/Windows signing is on, and turns on the hardened runtime for notarization. |
| `.github/workflows/release-extras.yml` | After the builds, before hosting: SBOMs, Scoop manifest, macOS signature check + notarization, Windows signature check. A failure stops the release. |
| `.github/workflows/release-packages.yml` | After the release exists (stable versions only): pushes `scull.rb` to the Homebrew tap and `scull.json` to the Scoop bucket. |
| `packaging/sbom.py` | CycloneDX 1.5 SBOM per target from `cargo metadata` + `Cargo.lock` (stdlib Python). |
| `packaging/scoop.py` | The Scoop manifest from the Windows archive's SHA-256. |
| `packaging/test_release_packaging.py` | Self-tests for both (run in the Policy workflow and the dry run). |
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
- `scull.rb`: the Homebrew formula (all four macOS/Linux archives with their SHA-256).
- `scull.json`: the Scoop manifest for the Windows archive.
- `scull-cli-<target>.cdx.json`: a CycloneDX 1.5 SBOM per target (the crates linked into that
  binary, with licenses and crates.io SHA-256s; build-only crates marked `scope: excluded`),
  each with a `.sha256`.
- A GitHub **build-provenance attestation** (Sigstore, keyless) for every archive, verifiable
  with `gh attestation verify <file> -R jofftiquez/scull`.
- With the signing secrets set (see [Signing](#signing-and-publishing-secrets)): `scull` on
  macOS signed with a Developer ID (hardened runtime) and notarized; `scull.exe` Authenticode
  signed. Without them the release still builds, unsigned, and every skipped step leaves a
  notice in the run log ("macOS signing skipped: ...").

Not in the pipeline: MSI, winget, crates.io publishing (the name is reserved by hand, below).

Job order: `plan` → `custom-release-gates` (Linux: dependency check, release build, inspect-only
check) → 7 `build-local-artifacts` jobs (signing happens here; each archive is attested) →
`build-global-artifacts` (installers, formula, checksums) and `custom-release-extras` (SBOMs,
Scoop manifest, notarization and signature checks) → `sign-artifacts` (Windows installer
signing; a no-op without secrets) → `host` (creates the GitHub release and uploads every
artifact) → `custom-release-packages` (tap and bucket; stable releases only) → `announce`.
Nothing is uploaded unless every gate, build, signature check and notarization passed.

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

## Signing and publishing secrets

Everything below is a one-time maintainer setup (accounts are tracked in #11). Set each value
under **Settings → Secrets and variables → Actions → New repository secret**. Until a group is
complete, its step is skipped with a notice and the rest of the release still works. Check what
is configured at any time with the dry run (step 1): its **Signing readiness** job lists each
group as `ready` or `skipped (not set: ...)`, names only.

### macOS: Developer ID signing (dist `macos-sign`)

1. Join the Apple Developer Program (individual or organization; #11).
2. Create a **Developer ID Application** certificate: Xcode → Settings → Accounts → Manage
   Certificates → `+` → Developer ID Application (or developer.apple.com → Certificates, with a
   CSR from Keychain Access).
3. In Keychain Access, export the certificate **with its private key** as a `.p12` with a
   strong password.
4. Set the secrets:

   | Secret | Value |
   |---|---|
   | `CODESIGN_CERTIFICATE` | `base64 -i DeveloperID.p12 \| tr -d '\n'` |
   | `CODESIGN_CERTIFICATE_PASSWORD` | the `.p12` password |
   | `CODESIGN_IDENTITY` | the exact identity, e.g. `Developer ID Application: Jane Doe (AB12CD34EF)` (`security find-identity -v -p codesigning`) |

dist imports the certificate into a throwaway keychain on the macOS runners and signs `scull`
before it is archived, so the archives, `sha256.sum` and the Homebrew formula all describe the
signed binary. `.github/build-setup.yml` sets `CODESIGN_OPTIONS=runtime` (hardened runtime,
required for notarization).

### macOS: notarization (`xcrun notarytool`, release-extras.yml)

1. App Store Connect → Users and Access → Integrations → **App Store Connect API** → Team Keys →
   `+`, access **Developer**. Download `AuthKey_<KEYID>.p8` (it can be downloaded only once).
2. Set the secrets:

   | Secret | Value |
   |---|---|
   | `APPLE_API_KEY` | the full text of `AuthKey_<KEYID>.p8` (including the BEGIN/END lines) |
   | `APPLE_API_KEY_ID` | the Key ID |
   | `APPLE_API_ISSUER` | the Issuer ID shown above the keys table |

The `macOS signature and notarization` job checks each macOS `scull` for a Developer ID
signature, the hardened runtime and a secure timestamp, then submits it and waits. A rejection
prints Apple's log and fails the release before anything is uploaded. A bare executable cannot
be stapled; Gatekeeper looks the ticket up online. Notarization does not change the binary, so
the checksums stay valid.

### Windows: Authenticode (dist `ssldotcom-windows-sign`, SSL.com eSigner)

1. Buy an OV (or EV) code signing certificate from SSL.com with **eSigner** cloud signing, and
   finish validation (#11).
2. In the eSigner dashboard, enroll for signing and note the **credential ID** and the **TOTP
   secret** (the base32 secret behind the enrollment QR code).
3. Set the secrets: `SSLDOTCOM_USERNAME`, `SSLDOTCOM_PASSWORD`, `SSLDOTCOM_CREDENTIAL_ID`,
   `SSLDOTCOM_TOTP_SECRET`.

dist signs `scull.exe` before it is zipped and signs the PowerShell installer in
`sign-artifacts`. The `Windows Authenticode check` job then fails the release unless `scull.exe`
has a valid, timestamped signature.

Other providers: dist also supports Azure Artifact Signing (`azure-windows-sign` in
`dist-workspace.toml`, with `AZURE_CLIENT_ID`/`AZURE_TENANT_ID`/`AZURE_SUBSCRIPTION_ID` and a
`release` environment); switching is a config change plus `dist generate` (only one Windows
provider may be set). A plain `.pfx` with `signtool` (e.g. a SignPath or other OSS-program
certificate) is not supported by dist 0.33 and would need its own signing step.

### Homebrew tap and Scoop bucket (release-packages.yml)

1. Create two public repositories: `jofftiquez/homebrew-scull` (with an empty `Formula/` folder
   or just a README) and `jofftiquez/scoop-scull` (with a `bucket/` folder).
2. Create two **fine-grained** personal access tokens, each limited to one of those
   repositories with **Contents: Read and write** and nothing else, and set them as
   `HOMEBREW_TAP_TOKEN` and `SCOOP_BUCKET_TOKEN`.

On each stable release the job copies `scull.rb` to `Formula/scull.rb` and `scull.json` to
`bucket/scull.json` and pushes. Prereleases (`-alpha`, `-beta`) never update them. Without a
token, copy the two files from the release assets by hand.

Users then install with:

```sh
brew install jofftiquez/scull/scull
scoop bucket add scull https://github.com/jofftiquez/scoop-scull
scoop install scull/scull
```

### crates.io name (manual, once)

The workspace is `publish = false`; nothing is published to crates.io by the pipeline. To keep
the name `scull` (PRD D-1), the maintainer publishes a placeholder once from an empty folder
outside the repository: `cargo new scull`, set `version = "0.0.0"`, a description saying it is
reserved for https://github.com/jofftiquez/scull, the MIT OR Apache-2.0 license and repository
URL, then `cargo login` and `cargo publish`. Replace it with the real crate if scull is ever
published there.

## Release steps

### 1. Dry run (no tag, nothing published)

On GitHub: **Actions → Release gates → Run workflow** on `main`, tick "Also build". It runs the
gates, `dist plan` (which also fails if `release.yml` is stale), the packaging self-tests, the
SBOMs, a local x86_64 Linux build plus dist's global artifacts for it (installers, `scull.rb`,
`sha256.sum`, checked with `sha256sum -c`), all kept as the `scull-dry-run` workflow artifact
for 7 days, and the **Signing readiness** report. Nothing is signed, tagged or uploaded.

Or locally (Linux), with `dist` installed from its
[GitHub release](https://github.com/axodotdev/cargo-dist/releases/tag/v0.33.1) after checking
its `.sha256`:

```sh
python3 packaging/check_inspect_only.py deps
cargo build -p scull-cli --release --locked
python3 packaging/check_inspect_only.py binary target/release/scull
dist plan
dist build --artifacts=local --target x86_64-unknown-linux-gnu --output-format=json \
  > /tmp/local-dist-manifest.json                                   # local only, no upload
cp /tmp/local-dist-manifest.json target/distrib/x86_64-unknown-linux-gnu-dist-manifest.json
dist build --artifacts=global                                       # installers, scull.rb, sha256.sum
(cd target/distrib && sha256sum -c sha256.sum)
python3 packaging/test_release_packaging.py
python3 packaging/sbom.py --all-targets --out-dir target/sbom
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

For the MVP (#15) also collect:

```sh
# provenance of what you downloaded (any OS with gh)
gh attestation verify scull-cli-x86_64-unknown-linux-gnu.tar.gz -R jofftiquez/scull
jq '.components | length' scull-cli-x86_64-unknown-linux-gnu.cdx.json

# macOS: Developer ID, hardened runtime, timestamp, notarization
codesign -dvv --verbose=4 "$(command -v scull)" 2>&1 | grep -E 'Authority|Timestamp|flags'
syspolicy_check distribution "$(command -v scull)"     # macOS 14+: "passed all pre-distribution checks"

# package managers (stable releases)
brew install jofftiquez/scull/scull && scull --version
```

```powershell
Get-AuthenticodeSignature (Get-Command scull).Source | Format-List Status, SignerCertificate, TimeStamperCertificate
scoop bucket add scull https://github.com/jofftiquez/scoop-scull; scoop install scull/scull; scull --version
```

### If something goes wrong

- A gate or build fails: nothing was published. Delete the tag
  (`git push --delete origin v0.1.0-alpha.1 && git tag -d v0.1.0-alpha.1`), fix on `main`
  through a PR, and tag again.
- The release is published but broken: `gh release delete v0.1.0-alpha.1 --cleanup-tag`, fix,
  and release the next prerelease number (`0.1.0-alpha.2`) rather than reusing the tag.

## Reproducible builds (notes)

Goal (PRD §18): anyone can rebuild a release and get the same `scull` binary. Where it stands:

- **Pinned inputs.** `rust-toolchain.toml` pins the compiler, `Cargo.lock` pins every crate
  (all builds use `--locked`), dist is pinned (`cargo-dist-version`, and by checksum in
  `release-gates.yml`), and the release profile is fixed (`[profile.dist]` = `release`: thin
  LTO, `codegen-units = 1`, symbols stripped).
- **Verified locally:** two clean `dist build --artifacts=local --target
  x86_64-unknown-linux-gnu` runs of the same commit in the same checkout produce a
  byte-identical `scull` (P2-REL-01 evidence). The `.tar.gz` around it is not byte-identical
  yet, because the archive records file times.
- **Known differences between machines:** absolute paths are embedded in panic locations (the
  checkout path, `$CARGO_HOME/registry/src/...`, and build-script `OUT_DIR`s). To reproduce a
  release, build in the same paths the runner used (`/home/runner/work/scull/scull`,
  `CARGO_HOME=/home/runner/.cargo`), e.g. in a container. `--remap-path-prefix` (or the
  unstable `trim-paths`) would remove that requirement; it is not on yet because dist sets its
  own `RUSTFLAGS` on some targets.
- **Signed binaries** differ from an unsigned rebuild by the signature only. On macOS compare
  after `codesign --remove-signature`; on Windows compare the PE without its certificate table
  (e.g. `osslsigncode remove-signature`).
- The SBOMs are deterministic for a commit: `SOURCE_DATE_EPOCH` (the commit time) sets their
  timestamp, and their serial number is derived from the version, target and `Cargo.lock`.

## Changing the pipeline

- Edit `dist-workspace.toml`, run `dist generate`, commit both it and the regenerated
  `release.yml`.
- Upgrading dist: change `cargo-dist-version`, run `dist generate`, and update `DIST_VERSION`
  and `DIST_SHA256` in `release-gates.yml` (from the new release's
  `cargo-dist-x86_64-unknown-linux-gnu.tar.xz.sha256`).
- The inspect-only gate stays on for every release until the Executor ships with its own MVP
  gates (PRD §21.3). Changing its list of refused commands needs a safety-lane review. The MVP
  binary is built from `phase2`, where this gate is expected to fail once `scull clean` is
  wired: the MVP release needs the safety lane's replacement gates in `release-gates.yml`
  before it is tagged.
- `dist generate` warns "A Homebrew tap was specified but the Homebrew publish job is
  disabled". That is intended: `tap`/`formula` only name the formula; release-packages.yml
  pushes it (and skips cleanly without a token, which dist's own job does not).
- Signing secrets are only passed to the jobs that need them (dist's build and sign jobs,
  release-extras.yml's checks). Never print them; the jobs only test whether they are set.
