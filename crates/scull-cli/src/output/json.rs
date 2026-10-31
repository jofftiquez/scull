//! Machine-readable output (CLI-01): JSON documents and NDJSON streams with a versioned
//! envelope.
//!
//! * `--json`: one document on stdout,
//!   `{"schema": "scull/<name>@<major>", "version": "<scull version>", "data": ...}`.
//! * `--ndjson`: one object per line on stdout, each self-describing:
//!   `{"schema": ..., "version": ..., "type": "<record type>", "data": ...}`.
//! * Errors, in either mode: one line on **stderr**,
//!   `{"schema": "scull/error@1", "version": ..., "error": <ScullError>}`, and the process
//!   exits with [`ScullError::exit_code`]. Stdout then carries no partial document, so a
//!   consumer can always parse stdout when the exit code is 0.
//!
//! `schema` names the payload's JSON Schema; its major version changes only on breaking
//! changes (CLI-01). `version` is the scull release that produced the output.

use schemars::JsonSchema;
use scull_core::ScullError;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Write};

/// scull's version, as embedded in every envelope.
pub const SCULL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Regex every `schema` identifier matches.
pub const SCHEMA_ID_PATTERN: &str = r"^scull/[a-z][a-z0-9-]*@[1-9][0-9]*$";

/// A payload schema identifier, displayed as `scull/<name>@<major>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaId {
    name: &'static str,
    major: u32,
}

impl SchemaId {
    /// A schema identifier. `name` is lowercase kebab-case; `major` starts at 1.
    pub const fn new(name: &'static str, major: u32) -> Self {
        Self { name, major }
    }
}

impl fmt::Display for SchemaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scull/{}@{}", self.name, self.major)
    }
}

/// Schema of the error envelope.
pub const ERROR_SCHEMA: SchemaId = SchemaId::new("error", 1);

/// A JSON document: schema identifier, scull version and the payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    /// Payload schema, `scull/<name>@<major>`.
    #[schemars(regex(pattern = SCHEMA_ID_PATTERN))]
    pub schema: String,
    /// The scull version that produced this document.
    pub version: String,
    /// The payload.
    pub data: T,
}

impl<T> Envelope<T> {
    /// Wrap `data` under `schema` with the running scull version.
    pub fn new(schema: SchemaId, data: T) -> Self {
        Self {
            schema: schema.to_string(),
            version: SCULL_VERSION.to_owned(),
            data,
        }
    }
}

/// One NDJSON line: schema identifier, scull version, record type and payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NdjsonRecord<T> {
    /// Stream schema, `scull/<name>@<major>`.
    #[schemars(regex(pattern = SCHEMA_ID_PATTERN))]
    pub schema: String,
    /// The scull version that produced this record.
    pub version: String,
    /// Record type within the stream (e.g. `candidate`, `progress`, `summary`).
    #[serde(rename = "type")]
    pub kind: String,
    /// The payload.
    pub data: T,
}

/// The error document written to stderr in `--json`/`--ndjson` mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    /// Always `scull/error@1`.
    #[schemars(extend("const" = "scull/error@1"))]
    pub schema: String,
    /// The scull version that produced this document.
    pub version: String,
    /// The error, with a stable `kind` code.
    pub error: ScullError,
}

impl ErrorEnvelope {
    /// Wrap an error.
    pub fn new(error: ScullError) -> Self {
        Self {
            schema: ERROR_SCHEMA.to_string(),
            version: SCULL_VERSION.to_owned(),
            error,
        }
    }
}

/// Write `data` as one pretty-printed JSON document under `schema`, followed by a newline.
pub fn write_document<W: Write, T: Serialize>(
    mut w: W,
    schema: SchemaId,
    data: &T,
) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut w, &Envelope::new(schema, data))?;
    w.write_all(b"\n")?;
    w.flush()
}

/// Write an error envelope as a single compact JSON line.
pub fn write_error<W: Write>(mut w: W, error: &ScullError) -> io::Result<()> {
    serde_json::to_writer(&mut w, &ErrorEnvelope::new(error.clone()))?;
    w.write_all(b"\n")?;
    w.flush()
}

/// Writes an NDJSON stream: one compact, self-describing record per line, flushed per line so
/// consumers see records as they are produced.
pub struct NdjsonWriter<W: Write> {
    out: W,
    schema: String,
}

impl<W: Write> NdjsonWriter<W> {
    /// A writer for the stream `schema`.
    pub fn new(out: W, schema: SchemaId) -> Self {
        Self {
            out,
            schema: schema.to_string(),
        }
    }

    /// Write one record of type `kind`.
    pub fn record<T: Serialize>(&mut self, kind: &str, data: &T) -> io::Result<()> {
        let rec = NdjsonRecord {
            schema: self.schema.clone(),
            version: SCULL_VERSION.to_owned(),
            kind: kind.to_owned(),
            data,
        };
        serde_json::to_writer(&mut self.out, &rec)?;
        self.out.write_all(b"\n")?;
        self.out.flush()
    }
}

/// The JSON Schema (2020-12) of a `--json` document: [`Envelope`] around `T`, with `schema`
/// pinned to `id`. Commands publish this under `crates/scull-cli/schemas/`.
pub fn document_schema<T: JsonSchema>(id: SchemaId) -> serde_json::Value {
    let mut v = serde_json::to_value(schemars::schema_for!(Envelope<T>)).unwrap_or_default();
    v["title"] = id.to_string().into();
    v["properties"]["schema"]["const"] = id.to_string().into();
    v
}

/// The JSON Schema (2020-12) of [`ErrorEnvelope`], as published for P0-ARCH-02.
pub fn error_envelope_schema() -> schemars::Schema {
    schemars::schema_for!(ErrorEnvelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::{ErrorKind, FsPath};
    use serde_json::{Value, json};

    fn validator(schema: &schemars::Schema) -> jsonschema::Validator {
        let v = serde_json::to_value(schema).expect("schema serializes");
        jsonschema::validator_for(&v).expect("schema compiles")
    }

    #[test]
    fn schema_ids_display() {
        assert_eq!(ERROR_SCHEMA.to_string(), "scull/error@1");
        assert_eq!(SchemaId::new("scan", 1).to_string(), "scull/scan@1");
    }

    #[test]
    fn document_envelope_shape() {
        let mut buf = Vec::new();
        write_document(&mut buf, SchemaId::new("scan", 1), &json!({"items": []})).expect("write");
        let v: Value = serde_json::from_slice(&buf).expect("valid JSON");
        assert_eq!(
            v,
            json!({"schema": "scull/scan@1", "version": SCULL_VERSION, "data": {"items": []}})
        );
        assert!(buf.ends_with(b"\n"));

        let schema = schemars::schema_for!(Envelope<Value>);
        let val = validator(&schema);
        assert!(val.is_valid(&v));
        assert!(!val.is_valid(&json!({"schema": "scan", "version": "0", "data": 1})));
        assert!(!val.is_valid(
            &json!({"schema": "scull/scan@1", "version": "0", "data": 1, "extra": true})
        ));
    }

    #[test]
    fn ndjson_lines_are_self_describing() {
        let mut buf = Vec::new();
        let mut w = NdjsonWriter::new(&mut buf, SchemaId::new("scan", 1));
        w.record("candidate", &json!({"path": "/a\nb"}))
            .expect("write");
        w.record("summary", &json!({"count": 1})).expect("write");
        let text = String::from_utf8(buf).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "embedded newline must stay escaped: {text:?}"
        );
        let schema = schemars::schema_for!(NdjsonRecord<Value>);
        let val = validator(&schema);
        for (line, kind) in lines.iter().zip(["candidate", "summary"]) {
            let v: Value = serde_json::from_str(line).expect("valid JSON line");
            assert_eq!(v["schema"], "scull/scan@1");
            assert_eq!(v["version"], SCULL_VERSION);
            assert_eq!(v["type"], kind);
            assert!(val.is_valid(&v), "{line}");
        }
    }

    #[test]
    fn error_envelope_validates_against_schema() {
        let err = ScullError::new(ErrorKind::NotFound, "no such plan")
            .with_path(FsPath::from("/tmp/plan.json"));
        let mut buf = Vec::new();
        write_error(&mut buf, &err).expect("write");
        assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 1);
        let v: Value = serde_json::from_slice(&buf).expect("valid JSON");
        assert_eq!(
            v,
            json!({
                "schema": "scull/error@1",
                "version": SCULL_VERSION,
                "error": {"kind": "not_found", "message": "no such plan", "path": "/tmp/plan.json"}
            })
        );
        let val = validator(&error_envelope_schema());
        assert!(val.is_valid(&v));
        let mut wrong = v.clone();
        wrong["schema"] = "scull/scan@1".into();
        assert!(!val.is_valid(&wrong));
        let mut bad_kind = v;
        bad_kind["error"]["kind"] = "oops".into();
        assert!(!val.is_valid(&bad_kind));
    }
}
