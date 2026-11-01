//! `scull doctor --report [--hash-names] [--out FILE [--yes]]` (#248; PRD data table "`scull
//! doctor --report` for user-initiated bug reports", INS-05, INS-07, T-56, FX-DATA-07).
//!
//! A shareable copy of the [`doctor`](super::doctor::run) report, redacted the way the MCP server
//! redacts its results ([`scull_mcp::Redactor`]):
//!
//! * the home folder becomes `~` everywhere (paths and free text);
//! * with `--hash-names`, path components under the home folder and the scan roots (their own
//!   names included) are hashed (well-known tool folders such as `target` are kept) with a
//!   salt made for this report only, and free-text messages are omitted;
//! * **withheld**: user names (`privileges.sudo_user`), config parse errors (they can quote
//!   the config file), environment values (an invalid `SCULL_*` variable is named, its value
//!   is not; effective settings that come from the environment show only the variable name);
//! * **never included**: file contents (doctor reads none besides parsing the config file),
//!   environment values, scan results and history.
//!
//! The report is shown first as a **redaction preview**: what was redacted and withheld, then
//! the full report. Nothing is written unless `--out FILE` is given and confirmed (a terminal
//! prompt, or `--yes`); the file is created new and owner-only. scull never uploads it
//! (INS-05). Machine output: `scull/doctor-report@1`
//! (`crates/scull-cli/schemas/doctor-report.schema.json`).

use super::doctor::{self, DoctorReport};
use super::{emit_machine, emit_text};
use crate::cli::DoctorArgs;
use crate::output::Ui;
use crate::output::escape::escape_for_terminal;
use crate::output::json::{SchemaId, write_document};
use crate::output::style::Style;
use schemars::JsonSchema;
use scull_core::config::Origin;
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_mcp::{NameSalt, Redactor};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Schema of `scull doctor --report --json` and of the `--out` file.
pub const DOCTOR_REPORT_SCHEMA: SchemaId = SchemaId::new("doctor-report", 1);

/// What a withheld value is replaced with.
pub const WITHHELD: &str = "<withheld>";

/// What free text is replaced with under `--hash-names`.
pub const OMITTED: &str = "<omitted: --hash-names>";

/// What the report never contains, in the order shown.
pub const NEVER_INCLUDED: &[&str] = &[
    "file contents (doctor reads no file besides parsing the config file, and config parse \
     errors are withheld)",
    "environment variable values",
    "user names",
    "scan results, project lists and history",
];

/// A shareable `scull doctor` report.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShareableReport {
    /// What was redacted, withheld and never included.
    pub redaction: Redaction,
    /// The `scull doctor` report after redaction (same shape as `scull/doctor@1` data).
    #[schemars(with = "DoctorReport")]
    pub doctor: Value,
}

/// What the redaction did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Redaction {
    /// Whether the home folder prefix is shown as `~` (`false` only when it is unknown).
    pub home_as_tilde: bool,
    /// How many strings the home redaction (and escaping) rewrote.
    pub strings_rewritten: u64,
    /// Whether project and folder names are hashed (`--hash-names`).
    pub names_hashed: bool,
    /// Values left out, by field.
    pub withheld: Vec<Withheld>,
    /// What the report never contains.
    pub never_included: Vec<String>,
    /// Always `false`: scull never uploads the report (INS-05).
    pub uploaded: bool,
}

/// One withheld value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Withheld {
    /// Where it was (`privileges.sudo_user`, `config.effective.scan.roots.value`, ...).
    pub field: String,
    /// Why it was left out.
    pub reason: String,
}

/// `scull doctor --report`.
pub fn run(args: &DoctorArgs, ui: &Ui) -> Result<(), ScullError> {
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    // Machine output never prompts; refuse before printing so stdout holds no document on
    // failure.
    if args.out.is_some() && !args.yes && ui.format.is_machine() {
        return Err(needs_yes());
    }
    let home = scull_safety::protect::home_from_env();
    let shareable = build(doctor::collect(), home.as_deref(), args.hash_names);
    if ui.format.is_machine() {
        emit_machine(ui.format, DOCTOR_REPORT_SCHEMA, "doctor-report", &shareable)?;
    } else {
        emit_text(&render_preview(&shareable, args, ui.color_out))?;
    }
    let Some(out) = &args.out else {
        return Ok(());
    };
    if !args.yes {
        if !interactive {
            return Err(needs_yes());
        }
        if !confirm(out)? {
            return Err(ScullError::new(
                ErrorKind::Cancelled,
                "the report was not written",
            ));
        }
    }
    write_report(out, &shareable)?;
    let _ = writeln!(
        io::stderr(),
        "Report written to {} (readable only by you). scull never uploads it: read it, then \
         attach it to your bug report yourself.",
        escape_for_terminal(&out.to_string_lossy())
    );
    Ok(())
}

fn needs_yes() -> ScullError {
    ScullError::new(
        ErrorKind::Usage,
        "--out without a terminal needs --yes: review the preview (`scull doctor --report`) \
         first, then add --yes to write the file",
    )
}

/// Ask on the terminal whether to write `out`.
fn confirm(out: &Path) -> Result<bool, ScullError> {
    let mut err = io::stderr().lock();
    let _ = write!(
        err,
        "Write this report to {}? Nothing is uploaded. [y/N] ",
        escape_for_terminal(&out.to_string_lossy())
    );
    let _ = err.flush();
    drop(err);
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| ScullError::from_io(&e))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Create `out` (new, owner-only) and write the `scull/doctor-report@1` document.
pub fn write_report(out: &Path, shareable: &ShareableReport) -> Result<(), ScullError> {
    let fail = |e: &io::Error| {
        let msg = if e.kind() == io::ErrorKind::AlreadyExists {
            "a file already exists there; choose a new file name (the report never replaces \
             a file)"
                .to_owned()
        } else {
            format!("cannot write the report: {e}")
        };
        ScullError::new(ErrorKind::Io, msg).with_path(FsPath::from_path(out))
    };
    let file = scull_platform::owner_only::create_file(out).map_err(|e| fail(&e))?;
    write_document(io::BufWriter::new(file), DOCTOR_REPORT_SCHEMA, shareable).map_err(|e| fail(&e))
}

/// Redact `report` for sharing (see the module docs). `home` is the user's home folder.
pub fn build(mut report: DoctorReport, home: Option<&Path>, hash_names: bool) -> ShareableReport {
    let mut withheld = Vec::new();
    let mut withhold = |field: String, reason: &str| {
        withheld.push(Withheld {
            field,
            reason: reason.to_owned(),
        });
    };
    if report.privileges.sudo_user.is_some() {
        report.privileges.sudo_user = Some(WITHHELD.to_owned());
        withhold("privileges.sudo_user".into(), "a user name");
    }
    if report.config.error.is_some() {
        report.config.error = Some(format!(
            "{WITHHELD}: the config file could not be read or parsed; run `scull doctor` \
             locally for the details (they can quote the file)"
        ));
        withhold(
            "config.error".into(),
            "parse errors can quote the config file",
        );
    }
    if let Some(e) = &report.config.env_error {
        let vars = scull_vars(e);
        report.config.env_error = Some(if vars.is_empty() {
            format!("{WITHHELD}: an invalid SCULL_* environment variable")
        } else {
            format!(
                "invalid environment variable {} (value withheld)",
                vars.join(", ")
            )
        });
        withhold(
            "config.env_error".into(),
            "it can quote an environment variable's value",
        );
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for v in &mut report.config.effective {
        if let Some(Origin::Env { var }) = v.origins.first() {
            v.value = Value::String(format!("{WITHHELD}: set by {var}"));
            withhold(
                format!("config.effective.{}.value", v.field),
                "set by an environment variable",
            );
        } else if v.field == "scan.roots" {
            // Hashing keeps a prefix verbatim and hashes what is below it: below each root's
            // parent, so the root's own name is hidden too (outside the home folder as well).
            roots.extend(
                strings(&v.value)
                    .iter()
                    .filter_map(|r| Path::new(r).parent().map(Path::to_path_buf)),
            );
        }
    }
    // Recomputed so they quote the withheld values, not the originals.
    report.warnings = doctor::warnings(&report);

    let salt = hash_names.then(|| NameSalt::load_or_create(None));
    let redactor = Redactor::new(home, true, &roots, salt);
    let original = serde_json::to_value(&report).unwrap_or_default();
    let mut doc = original.clone();
    if hash_names {
        hash(&redactor, &mut doc);
    }
    finish(&redactor, &mut doc);
    let strings_rewritten = changed_strings(&original, &doc);
    ShareableReport {
        redaction: Redaction {
            home_as_tilde: home.is_some(),
            strings_rewritten,
            names_hashed: hash_names,
            withheld,
            never_included: NEVER_INCLUDED.iter().map(|s| (*s).to_owned()).collect(),
            uploaded: false,
        },
        doctor: doc,
    }
}

/// `SCULL_*` variable names mentioned in `text`.
fn scull_vars(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("SCULL_") {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..end];
        if !out.iter().any(|n| n == name) {
            out.push(name.to_owned());
        }
        rest = &tail[end..];
    }
    out
}

/// Every string in `v` (a string or an array of strings).
fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

/// `--hash-names`: hash the path fields, omit free text.
fn hash(r: &Redactor, doc: &mut Value) {
    let path = |v: &mut Value| {
        if let Value::String(s) = v {
            *s = r.path(s);
        }
    };
    // Free text can embed names: omitted (the Redactor's MCP marker names the MCP setting).
    let text = |v: &mut Value| {
        if let Value::String(s) = v {
            *s = OMITTED.to_owned();
        }
    };
    for p in [
        "/dirs/scull_home",
        "/database/path",
        "/config/path",
        "/git/binary",
    ] {
        if let Some(v) = doc.pointer_mut(p) {
            path(v);
        }
    }
    for p in [
        "/dirs/error",
        "/database/detail",
        "/git/error",
        "/config/error",
    ] {
        if let Some(v) = doc.pointer_mut(p) {
            text(v);
        }
    }
    if let Some(Value::Array(ws)) = doc.pointer_mut("/warnings") {
        ws.iter_mut().for_each(text);
    }
    if let Some(Value::Array(es)) = doc.pointer_mut("/dirs/entries") {
        for e in es {
            if let Some(v) = e.get_mut("path") {
                path(v);
            }
            if let Some(v) = e.get_mut("error") {
                text(v);
            }
        }
    }
    if let Some(Value::Array(vals)) = doc.pointer_mut("/config/effective") {
        for v in vals {
            let is_path_list = v["field"]
                .as_str()
                .is_some_and(|f| matches!(f, "scan.roots" | "scan.protect" | "scan.exclude"));
            if !is_path_list {
                continue;
            }
            match v.get_mut("value") {
                Some(Value::Array(items)) => items.iter_mut().for_each(path),
                Some(s @ Value::String(_)) => path(s),
                _ => {}
            }
        }
    }
}

/// The backstop over every string: home → `~`, escaping, length cap.
fn finish(r: &Redactor, v: &mut Value) {
    match v {
        Value::String(s) => *s = r.finish_text(s),
        Value::Array(a) => a.iter_mut().for_each(|x| finish(r, x)),
        Value::Object(m) => m.values_mut().for_each(|x| finish(r, x)),
        _ => {}
    }
}

/// How many strings differ between `a` and `b` (same shape; strings replaced by others).
fn changed_strings(a: &Value, b: &Value) -> u64 {
    match (a, b) {
        (Value::String(x), Value::String(y)) => u64::from(x != y),
        (Value::Array(x), Value::Array(y)) => {
            x.iter().zip(y).map(|(p, q)| changed_strings(p, q)).sum()
        }
        (Value::Object(x), Value::Object(y)) => x
            .iter()
            .map(|(k, p)| y.get(k).map_or(0, |q| changed_strings(p, q)))
            .sum(),
        (Value::String(_), _) => 1,
        _ => 0,
    }
}

/// The human preview: what was redacted, then the whole report.
fn render_preview(s: &ShareableReport, args: &DoctorArgs, color: bool) -> String {
    let bold = |t: &str| Style::Bold.paint(t, color);
    let r = &s.redaction;
    let mut out = String::new();
    let line = |out: &mut String, label: &str, value: &str| {
        out.push_str(&format!("  {} {value}\n", bold(&format!("{label:<16}"))));
    };
    out.push_str(&format!(
        "{}\n",
        bold("Redaction preview: this is everything the report contains.")
    ));
    out.push_str(if args.out.is_some() {
        "Nothing has been written yet, and nothing is ever uploaded.\n\n"
    } else {
        "Nothing is written (add --out FILE to save it) and nothing is ever uploaded.\n\n"
    });
    line(
        &mut out,
        "home folder",
        &if r.home_as_tilde {
            format!("shown as ~ ({} strings rewritten)", r.strings_rewritten)
        } else {
            "unknown (HOME is not set); paths are shown as they are".to_owned()
        },
    );
    line(
        &mut out,
        "names",
        if r.names_hashed {
            "hashed in paths under the home folder and scan roots; free-text messages omitted"
        } else {
            "not hashed (add --hash-names to hide project and folder names)"
        },
    );
    if r.withheld.is_empty() {
        line(&mut out, "withheld", "nothing else needed withholding");
    }
    for (i, w) in r.withheld.iter().enumerate() {
        line(
            &mut out,
            if i == 0 { "withheld" } else { "" },
            &format!("{} ({})", w.field, w.reason),
        );
    }
    for (i, n) in r.never_included.iter().enumerate() {
        line(&mut out, if i == 0 { "never included" } else { "" }, n);
    }
    out.push('\n');
    let mut doc = Vec::new();
    let _ = write_document(&mut doc, DOCTOR_REPORT_SCHEMA, s);
    for l in String::from_utf8_lossy(&doc).lines() {
        out.push_str(&escape_for_terminal(l));
        out.push('\n');
    }
    out
}
