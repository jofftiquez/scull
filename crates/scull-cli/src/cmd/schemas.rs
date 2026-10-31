//! Published JSON Schemas for the read-only commands (CLI-01), committed under
//! `crates/scull-cli/schemas/` and checked against real output in the tests (regenerate with
//! `SCULL_BLESS=1 cargo test -p scull-cli --test read_only`).
//!
//! Each command has a `--json` document schema (`<name>.schema.json`, the versioned
//! [`Envelope`](crate::output::json::Envelope)) and an `--ndjson` line schema
//! (`<name>.ndjson.schema.json`): every line is `{schema, version, type, data}` where `type`
//! picks the shape of `data`.

use crate::output::json::{SCHEMA_ID_PATTERN, SchemaId, document_schema};
use schemars::JsonSchema;
use serde::Serialize;

/// One NDJSON line: the envelope fields plus an adjacently tagged record (`type` + `data`).
/// Only used to generate schemas; output is written by
/// [`NdjsonWriter`](crate::output::json::NdjsonWriter), which produces the same shape.
#[derive(Debug, Serialize, JsonSchema)]
pub struct NdjsonLine<T> {
    /// `scull/<name>@<major>`.
    #[schemars(regex(pattern = SCHEMA_ID_PATTERN))]
    pub schema: String,
    /// The scull version that wrote it.
    pub version: String,
    /// `type` and `data`.
    #[serde(flatten)]
    pub record: T,
}

/// The NDJSON line schema for records `T`, titled and pinned to `id`.
pub fn ndjson_schema<T: JsonSchema>(id: SchemaId) -> serde_json::Value {
    let mut v = serde_json::to_value(schemars::schema_for!(NdjsonLine<T>)).unwrap_or_default();
    v["title"] = format!("{id} (NDJSON line)").into();
    v["description"] = format!(
        "One line of `--ndjson` output with schema {id}: `type` selects the shape of `data`."
    )
    .into();
    v["properties"]["schema"]["const"] = id.to_string().into();
    v
}

/// Every read-only command schema: `(file name, schema)`.
pub fn all() -> Vec<(String, serde_json::Value)> {
    use super::{caches, projects, scan, worktrees};
    let mut out = Vec::new();
    let mut add = |id: SchemaId, doc: serde_json::Value, nd: serde_json::Value| {
        let name = id.to_string();
        let short = name
            .trim_start_matches("scull/")
            .split('@')
            .next()
            .unwrap_or_default()
            .to_owned();
        out.push((format!("{short}.schema.json"), doc));
        out.push((format!("{short}.ndjson.schema.json"), nd));
    };
    add(
        scan::SCAN_SCHEMA,
        document_schema::<scull_engine::ScanReport>(scan::SCAN_SCHEMA),
        ndjson_schema::<scan::ScanRecord>(scan::SCAN_SCHEMA),
    );
    add(
        projects::PROJECTS_SCHEMA,
        document_schema::<projects::ProjectsReport>(projects::PROJECTS_SCHEMA),
        ndjson_schema::<projects::ProjectsRecord>(projects::PROJECTS_SCHEMA),
    );
    add(
        projects::PROJECT_SCHEMA,
        document_schema::<projects::ProjectInspect>(projects::PROJECT_SCHEMA),
        ndjson_schema::<projects::ProjectRecord>(projects::PROJECT_SCHEMA),
    );
    add(
        worktrees::WORKTREES_SCHEMA,
        document_schema::<worktrees::WorktreesReport>(worktrees::WORKTREES_SCHEMA),
        ndjson_schema::<worktrees::WorktreesRecord>(worktrees::WORKTREES_SCHEMA),
    );
    add(
        worktrees::WORKTREE_SCHEMA,
        document_schema::<scull_engine::WorktreeReport>(worktrees::WORKTREE_SCHEMA),
        ndjson_schema::<worktrees::WorktreeRecord>(worktrees::WORKTREE_SCHEMA),
    );
    add(
        caches::CACHES_SCHEMA,
        document_schema::<scull_engine::CachesReport>(caches::CACHES_SCHEMA),
        ndjson_schema::<caches::CachesRecord>(caches::CACHES_SCHEMA),
    );
    out
}
