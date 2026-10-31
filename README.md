# scull

**scull** is a free, open-source, local-first developer storage analyzer and cleanup tool for
macOS, Linux and Windows. It finds what development tools leave behind (`node_modules`,
`target/`, build outputs, package caches, Git worktrees from AI agents, ML models, Docker data),
explains each item, classifies it by risk, and reclaims space **without ever risking source code,
Git history or unique files**.

> Status: alpha. The first alpha is inspect-only; cleanup arrives with the MVP. Progress lives
> in the pinned **Roadmap tracker** issue.

## Documentation

| Guide | What it covers |
|---|---|
| [Install](docs/user/INSTALL.md) | Downloads per OS, checksums, private-repo downloads, first run |
| [Safety model](docs/user/SAFETY.md) | Tiers, what scull never touches, plans, quarantine and restore, privacy |
| [CLI reference](docs/user/CLI.md) | Every command and flag, exit codes, JSON and NDJSON output |
| [AI agents (MCP)](docs/user/MCP.md) | The read-only MCP server and how to connect a client |

```sh
scull init                         # choose the folders to scan
scull scan                         # read-only: what is using space, and how risky it is
scull clean --output plan.json     # dry run: write a plan you can review
scull clean --plan plan.json       # review it against a fresh scan; confirm to run it
scull restore <operation-id>       # undo: put quarantined items back
```

## Risk tiers

| Tier | Meaning | Default |
|---|---|---|
| S0 | Regenerable | Preselected for review, never executed without confirmation |
| S1 | Costly to recreate | Opt-in, quarantined |
| S2 | Uncertain | Per-item opt-in, quarantined |
| S3 | Protected | Never touched |

S0 items are deleted after you confirm (rebuilding is the undo). S1 and S2 items go to a
7-day quarantine that `scull restore` reverses. See the [safety model](docs/user/SAFETY.md).

## Layout

One Rust engine shared by a CLI (`scull`), a Tauri 2 + Vue 3 desktop app, and an MCP server.

```text
crates/   scull-core, -platform, -scanner, -detectors, -git, -safety,
          -cleaner, -storage, -cli, -mcp, -fixtures
apps/     desktop (Tauri 2 + Vue 3, Phase 3)
rules/    declarative detector rules (data only)
schemas/  JSON Schemas for CLI/MCP output and plan files
docs/     PRD, decisions, threat model, fixture matrix; user guides in docs/user/
design/   interactive design reference (open design/html/index.html)
```

## Building

```bash
cargo build
cargo test
```

The toolchain is pinned in `rust-toolchain.toml`.

## Privacy

No telemetry. Everything stays on your machine. The opt-in MCP server is the one exception:
its results go to your AI client ([details](docs/user/MCP.md)).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in scull by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
