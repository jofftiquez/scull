# Installing scull

scull is one command-line program, `scull`. It needs no runtime, no daemon and no account.
This page covers macOS, Linux and Windows.

> **Status: alpha.** Expect rough edges and report them as issues.
>
> * The first alpha, `0.1.0-alpha.1`, is **inspect-only**: it is tagged from `main` before
>   cleanup landed, and every cleanup command in it exits with "not implemented yet".
> * Cleanup (plans, quarantine, restore, audit) ships with the MVP release, built from this
>   code once its release gates pass. The [CLI reference](CLI.md) and
>   [safety model](SAFETY.md) describe it.
> * No release has been published from this repository yet. Until the first one, build from
>   source (below).
> * Signing and notarization run only when the signing keys are configured, so builds may be
>   unsigned. Homebrew and Scoop packages are published for stable versions only, not for
>   alphas. There is no winget or MSI package yet.

## What you download

Each release on the GitHub **Releases** page has:

| File | For |
|---|---|
| `scull-cli-aarch64-apple-darwin.tar.gz` | macOS, Apple silicon |
| `scull-cli-x86_64-apple-darwin.tar.gz` | macOS, Intel |
| `scull-cli-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 (glibc) |
| `scull-cli-aarch64-unknown-linux-gnu.tar.gz` | Linux arm64 (glibc) |
| `scull-cli-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64, static (Alpine, old glibc) |
| `scull-cli-aarch64-unknown-linux-musl.tar.gz` | Linux arm64, static |
| `scull-cli-x86_64-pc-windows-msvc.zip` | Windows x86_64 |
| `scull-cli-installer.sh` | Installer script for macOS and Linux |
| `scull-cli-installer.ps1` | Installer script for Windows (PowerShell) |
| `*.sha256`, `sha256.sum` | Checksums |

The archive holds one binary: `scull` (`scull.exe` on Windows). There is no self-updater:
scull never contacts the network on its own.

## Downloads

The repository is public, so release files download without signing in: from the release
page, with `curl`, or with the installer scripts. With the GitHub CLI:

```sh
gh release download <version> --repo jofftiquez/scull \
  --pattern 'scull-cli-x86_64-unknown-linux-gnu.tar.gz*' --pattern sha256.sum
```

Swap in the archive name for your platform from the table above.

## Verify the checksum

Always check the download before you run it.

| OS | Command |
|---|---|
| Linux | `sha256sum -c sha256.sum --ignore-missing` |
| macOS | `shasum -a 256 -c sha256.sum --ignore-missing` |
| Windows (PowerShell) | `Get-FileHash .\scull-cli-x86_64-pc-windows-msvc.zip -Algorithm SHA256`, then compare with the `.sha256` file |

Each line must say `OK`. If one does not, delete the file and download it again. If it still
fails, do not run it; open an issue.

## macOS

1. Download the archive for your Mac (Apple silicon or Intel) and verify it.
2. Unpack it and move the binary onto your `PATH`:

   ```sh
   tar -xzf scull-cli-aarch64-apple-darwin.tar.gz
   mkdir -p ~/.local/bin && mv scull ~/.local/bin/
   ```

3. Builds are unsigned. A file downloaded with a browser carries the quarantine attribute,
   and Gatekeeper refuses to open it. Clear the attribute once:

   ```sh
   xattr -d com.apple.quarantine ~/.local/bin/scull
   ```

   Files fetched with `gh`, `curl` or the installer script do not get this attribute.

Or run the installer script: `sh scull-cli-installer.sh`. It installs into `~/.cargo/bin`
(`$CARGO_HOME/bin`).

## Linux

1. Pick the archive: `gnu` for most distributions, `musl` for Alpine or an old glibc.
2. Verify it, unpack it and move the binary onto your `PATH`:

   ```sh
   tar -xzf scull-cli-x86_64-unknown-linux-gnu.tar.gz
   mkdir -p ~/.local/bin && mv scull ~/.local/bin/
   ```

Or run `sh scull-cli-installer.sh`, which installs into `~/.cargo/bin`.

scull reads Git state with the `git` command. Install Git 2.20 or newer for worktree and
tracking checks; `scull doctor` reports the version it found.

## Windows

1. Download `scull-cli-x86_64-pc-windows-msvc.zip` and verify it.
2. Unpack it and put `scull.exe` in a folder on your `PATH`, for example
   `%USERPROFILE%\.cargo\bin`.
3. Builds are unsigned, so SmartScreen may warn on first run. Choose **More info**, then
   **Run anyway**, only if the checksum matched.

Or run the installer in PowerShell: `powershell -ExecutionPolicy Bypass -File
.\scull-cli-installer.ps1`. It installs into `%USERPROFILE%\.cargo\bin`.

Do not run scull from an Administrator prompt. Cleanup commands refuse to run elevated
(see [SAFETY.md](SAFETY.md#scull-refuses-to-run-as-root-or-administrator)).

## Build from source

You need Git and the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it
automatically). The database engine (SQLite) is bundled, so you also need a C compiler.

```sh
git clone https://github.com/jofftiquez/scull.git
cd scull
cargo build --release -p scull-cli
./target/release/scull --version
```

## First run

```sh
scull --version          # confirm the install
scull doctor             # what this machine supports, where scull keeps its files
scull init               # pick the folders to scan (asks a few questions)
scull scan               # scan those folders; read-only
```

`scull init` writes your scan roots to the config file. scull never scans your whole home
folder or a drive root unless you confirm it.

### Shell completions

```sh
scull completions bash > ~/.local/share/bash-completion/completions/scull
scull completions zsh  > "${fpath[1]}/_scull"
scull completions fish > ~/.config/fish/completions/scull.fish
scull completions powershell >> $PROFILE
```

## Where scull keeps its files

| | Linux | macOS | Windows |
|---|---|---|---|
| Config (`config.toml`, `profile.toml`, `rules/`) | `~/.config/scull` | `~/Library/Application Support/scull` | `%APPDATA%\scull` |
| Data (database, audit mirrors) | `~/.local/share/scull` | `~/Library/Application Support/scull` | `%LOCALAPPDATA%\scull\data` |
| Cache | `~/.cache/scull` | `~/Library/Caches/scull` | `%LOCALAPPDATA%\scull\cache` |
| State (execution lock, logs) | `~/.local/state/scull` | `~/Library/Logs/scull` | `%LOCALAPPDATA%\scull\state` |

On Linux the `XDG_*_HOME` variables move these folders. Set `SCULL_HOME` to an absolute path
to put all four under one folder (`$SCULL_HOME/config`, `/data`, `/cache`, `/state`); this is
handy for tests and portable setups. All folders are created owner-only.

## Uninstall

1. Restore or purge anything still in quarantine (`scull history` shows it), then delete
   scull's records:

   ```sh
   scull data purge --all
   ```

   This removes scull's database and its own files only. It refuses while quarantined items
   still need their records.
2. Delete the config folder from the table above if you want your settings gone too.
3. Delete the `scull` binary.
