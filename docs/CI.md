# CI

Workflow: `.github/workflows/ci.yml`.

| Job | Runs on | What |
|---|---|---|
| `rustfmt` | Linux | `cargo fmt --all --check` |
| `test (<os>)` | macOS, Linux, Windows on PRs; Linux on pushes to `main` | `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace`, `cargo doc -D warnings` (Linux) |
| `CI ok` | Linux | Passes only if every job above passed. |
| `cargo-deny` (`policy.yml`) | Linux | Licenses (permissive only, no copyleft), RustSec advisories (same DB as cargo-audit), yanked crates, sources. Config: `deny.toml`. Also weekly. |
| `platform-boundary` (`policy.yml`) | Linux | `scripts/ci/platform_boundary.py`: no `std::os::unix/windows`, `libc`, `windows-sys`, Apple frameworks or `extern "C"` outside `crates/scull-platform` (PLT-11). Test code and `crates/scull-fixtures` are exempt. Self-tested first. |

| `cli-deps` (`policy.yml`) | Linux | `packaging/check_inspect_only.py deps`: the `scull` binary's dependency tree has no Tauri/webview crates (HANDOVER §8.1). |
| `release-gates` (`release-gates.yml`) | Linux | Runs before every release build (and as a manual dry run): inspect-only binary gate (every destructive command still a stub, MCP read-only, canary unchanged), `dist plan`. See `RELEASING.md`. |

**Required checks:** `CI ok`, `cargo-deny`, `platform-boundary`.

- Docs/design/task-only changes skip CI (`paths-ignore`).
- Superseded runs on the same branch are cancelled.
- Evidence screenshots are attached to issue comments (`gh issue comment --attach`), never committed (D-31).
- Windows runners enable `core.longpaths` and disable `autocrlf` so fixtures see real bytes.

## Branch protection (maintainer)

Require the `CI ok`, `cargo-deny` and `platform-boundary` checks and one review on `main`, squash merges, auto-delete branches
(HANDOVER.md §4.4). Branch protection on a **private** repository needs GitHub Pro/Team;
on a public repository it is free.

## Running the policy checks locally

```bash
cargo deny check                         # https://github.com/EmbarkStudios/cargo-deny
python3 scripts/ci/platform_boundary.py
```

The boundary check exempts the item right after a `#[cfg(test)]` attribute (found by brace
counting), files under `tests/`/`benches/`, `[dev-dependencies]`, and `crates/scull-fixtures`.
