# scull

**scull** is a free, open-source, local-first developer storage analyzer and cleanup tool for
macOS, Linux and Windows. It finds what development tools leave behind (`node_modules`,
`target/`, build outputs, package caches, Git worktrees from AI agents, ML models, Docker data),
explains each item, classifies it by risk, and reclaims space **without ever risking source code,
Git history or unique files**.

> Status: pre-alpha. Planning and design are complete; the engine is being built issue by issue.
> Progress lives in the pinned **Roadmap tracker** issue.

## Risk tiers

| Tier | Meaning | Default |
|---|---|---|
| S0 | Regenerable | Preselected for review, never executed without confirmation |
| S1 | Costly to recreate | Opt-in, quarantined |
| S2 | Uncertain | Per-item opt-in, quarantined |
| S3 | Protected | Never touched |

## Layout

One Rust engine shared by a CLI (`scull`), a Tauri 2 + Vue 3 desktop app, and an MCP server.

```text
crates/   scull-core, -platform, -scanner, -detectors, -git, -safety,
          -cleaner, -storage, -cli, -mcp, -fixtures
apps/     desktop (Tauri 2 + Vue 3, Phase 3)
rules/    declarative detector rules (data only)
schemas/  JSON Schemas for CLI/MCP output and plan files
docs/     PRD, decisions, threat model, fixture matrix
design/   interactive design reference (open design/html/index.html)
```

## Building

```bash
cargo build
cargo test
```

The toolchain is pinned in `rust-toolchain.toml`.

## Privacy

No telemetry. Everything stays on your machine.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in scull by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
