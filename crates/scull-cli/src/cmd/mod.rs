//! Implemented commands. Each module builds a serializable report, renders it for people,
//! and writes it as a versioned JSON document (`--json`) or one NDJSON record (`--ndjson`).

pub mod agent;
pub mod approve;
pub mod caches;
pub mod check;
pub mod clean;
pub mod clean_caches;
pub mod data;
pub mod doctor;
pub mod doctor_report;
pub mod exec;
pub mod forecast;
pub mod history;
pub mod init;
pub mod insights;
pub mod mcp;
pub mod mcp_runner;
pub mod pipeline;
pub mod projects;
pub mod render;
pub mod restore;
pub mod review_plan;
pub mod rules;
pub mod scan;
pub mod schedule;
pub mod schemas;
pub mod status;
pub mod worktrees;

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

/// A multi-line parser message (e.g. TOML's, with a source excerpt and caret line) on one
/// line: the excerpt is dropped and the remaining lines are joined with `: `.
pub(crate) fn one_line(msg: &str) -> String {
    msg.lines()
        .map(str::trim)
        .filter(|l| {
            let excerpt = l.starts_with('|')
                || l.split_once(" |")
                    .is_some_and(|(n, _)| n.chars().all(|c| c.is_ascii_digit()));
            !l.is_empty() && !excerpt
        })
        .collect::<Vec<_>>()
        .join(": ")
}

/// The serde name of a unit enum variant.
pub(crate) fn json_name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_messages_fit_on_one_line() {
        let msg = "invalid user config: TOML parse error at line 2, column 1\n  |\n2 | rootz = 1\n  | ^^^^^\nunknown field `rootz`";
        assert_eq!(
            one_line(msg),
            "invalid user config: TOML parse error at line 2, column 1: unknown field `rootz`"
        );
        assert_eq!(one_line("plain"), "plain");
    }
}
