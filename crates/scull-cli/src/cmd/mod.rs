//! Implemented commands. Each module builds a serializable report, renders it for people,
//! and writes it as a versioned JSON document (`--json`) or one NDJSON record (`--ndjson`).

pub mod doctor;

use crate::commands::ignore_broken_pipe;
use crate::output::Format;
use crate::output::json::{NdjsonWriter, SchemaId, write_document};
use scull_core::ScullError;
use serde::Serialize;
use std::io::{self, Write};

/// Write `data` to stdout in the machine format `format` (`--json` document or a single
/// `--ndjson` record of type `kind`). A closed stdout is not an error.
pub fn emit_machine<T: Serialize>(
    format: Format,
    schema: SchemaId,
    kind: &str,
    data: &T,
) -> Result<(), ScullError> {
    let out = io::stdout().lock();
    let r = match format {
        Format::Ndjson => NdjsonWriter::new(out, schema).record(kind, data),
        _ => write_document(out, schema, data),
    };
    r.or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))
}

/// Write already terminal-safe text to stdout. A closed stdout is not an error.
pub fn emit_text(text: &str) -> Result<(), ScullError> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))
}
