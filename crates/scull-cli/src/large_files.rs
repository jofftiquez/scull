//! `scull large-files <path>`: the inspect-only large-files listing (P1-DET-05, PRD §7.1).
//!
//! Scans `<path>` read-only (no-follow) and lists the largest regular files at or above
//! `--min-size` by **allocated** size, with their kind (guessed from the name) and modification
//! time. Nothing listed here is ever a cleanup candidate: every entry is S2, inspect-only, with
//! no action.
//!
//! * Human: a table (allocated, logical, kind, modified, path) and a summary line.
//! * `--json`: one `scull/large-files@1` document ([`LargeFilesData`]: `summary` and `files`);
//!   each entry of `files` is a `scull_core::Candidate` (`schemas/v1/candidate.schema.json`).
//! * `--ndjson`: one `candidate` record per file, largest first, then one `summary` record
//!   ([`LargeFilesSummary`]).

use crate::cancel;
use crate::output::bytes::{format_bytes, labeled_bytes};
use crate::output::escape::escape_for_terminal;
use crate::output::json::{self, NdjsonWriter, SchemaId};
use crate::output::table::{Align, Cell, Table};
use crate::output::{Format, Ui};
use schemars::JsonSchema;
use scull_core::{Candidate, Evidence, FsPath, ScullError, SizeKind};
use scull_detectors::large_files::DEFAULT_TOP;
use scull_detectors::{LargeFileOptions, inspect_large_files};
use scull_platform::Native;
use scull_scanner::{CancelToken, ScanOptions};
use serde::Serialize;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Schema of the `--json` document and the `--ndjson` stream.
pub const LARGE_FILES_SCHEMA: SchemaId = SchemaId::new("large-files", 1);

/// Default `--min-size`.
pub const DEFAULT_MIN_SIZE: &str = "500MiB";

/// Default `--top`.
pub const DEFAULT_TOP_ARG: usize = DEFAULT_TOP;

/// Totals of a large-files run (`data.summary` in `--json`; the last `--ndjson` record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LargeFilesSummary {
    /// The inspected directory (absolute).
    pub root: FsPath,
    /// `--min-size`: smallest allocated size listed, in bytes.
    pub threshold_bytes: u64,
    /// `--top`: most files listed.
    pub top: usize,
    /// Files at or above the threshold (may exceed `files`).
    pub matched: u64,
    /// Allocated bytes of all matched files.
    pub matched_bytes: u64,
    /// Whether more files matched than are listed.
    pub truncated: bool,
    /// Whether every part of the tree could be read (unreadable folders may hide files).
    pub complete: bool,
    /// Entries that could not be read.
    pub issues: u64,
}

/// The `scull/large-files@1` document.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LargeFilesData {
    /// Totals.
    pub summary: LargeFilesSummary,
    /// The files, largest first: inspect-only candidates (`Category::LargeFile`, S2, no action).
    pub files: Vec<Candidate>,
}

/// The JSON Schema (2020-12) of the `--json` envelope.
pub fn large_files_schema() -> schemars::Schema {
    schemars::schema_for!(json::Envelope<LargeFilesData>)
}

/// Parse a size such as `500MiB`, `500M`, `1.5G`, `2GB` or `1048576` (binary units: K, M, G,
/// T = 1024^n; an optional `B` or `iB` suffix; case-insensitive).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let unit = unit.trim().to_ascii_lowercase();
    let mult: u64 = match unit.trim_end_matches("ib").trim_end_matches('b') {
        "" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return Err(format!("unknown size unit in `{s}` (use K, M, G or T)")),
    };
    let value: f64 = num
        .parse()
        .map_err(|_| format!("`{s}` is not a size (e.g. 500M, 1.5G)"))?;
    let bytes = value * mult as f64;
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err(format!("`{s}` is out of range"));
    }
    Ok(bytes as u64)
}

/// Parse `--top`: an integer from 1 to [`MAX_TOP`].
pub fn parse_top(s: &str) -> Result<usize, String> {
    match s.trim().parse::<usize>() {
        Ok(n) if (1..=MAX_TOP).contains(&n) => Ok(n),
        _ => Err(format!("`{s}` is not a number from 1 to {MAX_TOP}")),
    }
}

/// Largest `--top`.
pub const MAX_TOP: usize = 100_000;

/// Run `scull large-files`.
pub fn run(path: &Path, threshold: u64, top: usize, ui: &Ui) -> Result<(), ScullError> {
    let root = std::path::absolute(path).map_err(|e| ScullError::from_io(&e))?;
    ui.progress(&format!("Scanning {} for large files…", root.display()));
    let token = CancelToken::new();
    let opts = LargeFileOptions { threshold, top };
    let done = CancelToken::new();
    let result = std::thread::scope(|s| {
        // Bridge Ctrl-C (a process-wide flag) to the scanner's token.
        s.spawn(|| {
            while !done.is_cancelled() {
                if cancel::is_cancelled() {
                    token.cancel();
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let r = inspect_large_files(
            Arc::new(Native::new()),
            &root,
            opts,
            &ScanOptions::default(),
            &token,
        );
        done.cancel();
        r
    });
    let (report, outcome) = result.map_err(|e| ScullError::from(&e))?;
    cancel::check()?;
    if outcome.is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    let issues = outcome.totals.issues;
    let summary = LargeFilesSummary {
        root: FsPath::from_path(&root),
        threshold_bytes: threshold,
        top,
        matched: report.matched,
        matched_bytes: report.matched_bytes,
        truncated: report.truncated(),
        complete: outcome.root.as_ref().is_some_and(|r| r.complete),
        issues,
    };
    let data = LargeFilesData {
        summary,
        files: report.into_candidates(),
    };
    let out = io::stdout();
    let w = out.lock();
    match ui.format {
        Format::Json => json::write_document(w, LARGE_FILES_SCHEMA, &data),
        Format::Ndjson => {
            let mut nd = NdjsonWriter::new(w, LARGE_FILES_SCHEMA);
            data.files
                .iter()
                .try_for_each(|c| nd.record("candidate", c))
                .and_then(|()| nd.record("summary", &data.summary))
        }
        Format::Human => write_human(w, &data, ui.color_out),
    }
    .or_else(crate::commands::ignore_broken_pipe)
    .map_err(|e| ScullError::from_io(&e))
}

fn write_human<W: Write>(mut w: W, data: &LargeFilesData, color: bool) -> io::Result<()> {
    let s = &data.summary;
    let threshold = format_bytes(s.threshold_bytes);
    let root = escape_for_terminal(s.root.as_str());
    if data.files.is_empty() {
        writeln!(
            w,
            "No files of {threshold} or more (allocated) under {root}."
        )?;
    } else {
        let mut t = Table::new([
            Table::size_column(SizeKind::Allocated),
            Table::size_column(SizeKind::Logical),
            ("Kind", Align::Left),
            ("Modified", Align::Left),
            ("Path", Align::Left),
        ]);
        for c in &data.files {
            let mtime = c.evidence.iter().find_map(|e| match e {
                Evidence::LastModified { newest_unix, .. } => Some(*newest_unix),
                _ => None,
            });
            t.push(vec![
                Cell::bytes(c.sizes.allocated),
                Cell::bytes(c.sizes.logical),
                Cell::text(&c.artifact),
                Cell::text(&mtime.map_or_else(|| "unknown".to_owned(), utc_date)),
                Cell::path(c.path.as_str()),
            ]);
        }
        w.write_all(t.render(color).as_bytes())?;
        let shown = if s.truncated {
            format!("; showing the largest {}", data.files.len())
        } else {
            String::new()
        };
        writeln!(
            w,
            "\n{} file(s) of {threshold} or more under {root} ({}){shown}.",
            s.matched,
            labeled_bytes(s.matched_bytes, SizeKind::Allocated)
        )?;
    }
    if !s.complete {
        writeln!(
            w,
            "Some folders could not be read ({} issue(s)); files inside them are not listed.",
            s.issues
        )?;
    }
    writeln!(
        w,
        "Inspect only: scull lists large files for review and never deletes them."
    )?;
    w.flush()
}

/// `YYYY-MM-DD` (UTC) for Unix seconds.
fn utc_date(secs: i64) -> String {
    // Civil-from-days (Howard Hinnant's algorithm), proleptic Gregorian calendar.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_with_binary_units() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("1048576"), Ok(1 << 20));
        assert_eq!(parse_size("500M"), Ok(500 << 20));
        assert_eq!(parse_size("500MiB"), Ok(500 << 20));
        assert_eq!(parse_size("500mb"), Ok(500 << 20));
        assert_eq!(parse_size("1.5G"), Ok(3 << 29));
        assert_eq!(parse_size("2 GB"), Ok(2 << 30));
        assert_eq!(parse_size("1T"), Ok(1 << 40));
        assert_eq!(parse_size("4k"), Ok(4096));
        assert_eq!(parse_size(DEFAULT_MIN_SIZE), Ok(500 << 20));
        assert!(parse_size("").is_err());
        assert!(parse_size("M").is_err());
        assert!(parse_size("5X").is_err());
        assert!(parse_size("1.2.3G").is_err());
        assert!(parse_size("-1").is_err());
    }

    #[test]
    fn top_is_bounded() {
        assert_eq!(parse_top("5"), Ok(5));
        assert!(parse_top("0").is_err());
        assert!(parse_top("100001").is_err());
        assert!(parse_top("x").is_err());
    }

    #[test]
    fn dates_are_utc_civil() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_800_000_000), "2027-01-15");
        assert_eq!(utc_date(-86_400), "1969-12-31");
    }

    #[test]
    fn schema_id() {
        assert_eq!(LARGE_FILES_SCHEMA.to_string(), "scull/large-files@1");
    }
}
