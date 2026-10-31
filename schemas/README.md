# JSON Schemas

JSON Schema **2020-12** files for scull's machine-readable output and plan files (CLI-01,
SAFE-11). Generated from the `scull-core` types, so they can't drift: the
`scull-core` test `tests/schemas.rs` fails if a committed schema differs from the code.

| Schema | Type | Used by |
|---|---|---|
| `v1/candidate.schema.json` | `scull_core::Candidate` | `scan`, `projects`, `worktrees`, `caches` JSON/NDJSON output; MCP results |
| `v1/plan.schema.json` | `scull_core::Plan` | `scull clean --output plan.json`, `--plan`, MCP plans |
| `v1/error.schema.json` | `scull_core::ScullError` | error objects in JSON mode and MCP |
| `v1/tier-breakdown.schema.json` | `scull_core::TierBreakdown` | per-tier byte rollups (charts, parents) |
| `v1/drop-reason.schema.json` | `scull_core::DropReason` | why a plan item was dropped at execution |

Command-level envelopes (`scull/scan@1`, …) are added with the commands that emit them
(P1-CLI-02).

`v1/examples/` holds example payloads: `<schema>.json` must validate, `<schema>.<case>.invalid.json`
must be rejected. Both are checked in CI.

## Regenerating

```bash
SCULL_UPDATE_SCHEMAS=1 cargo test -p scull-core --test schemas
```

Versioning: additive changes stay in `v1`. Removing or changing the meaning of a field needs
`v2/` (a new major schema version, CLI-01).
