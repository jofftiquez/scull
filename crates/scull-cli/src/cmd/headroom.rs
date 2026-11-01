//! `scull headroom status|create|release` (PRO-06, off by default): an emergency reserve file
//! that scull releases on command when the disk is completely full, buying room to run the
//! cleanup itself.
//!
//! * **`status`** (read-only): whether there is a reserve, its size and path, and whether it
//!   can be released (`scull_proactive::headroom::status`).
//! * **`create [--size SIZE] [--yes]`**: writes the reserve in scull's own data directory
//!   (`<data>/headroom/reserve.bin`, owner-only, really allocated, recorded in
//!   `<data>/headroom.json`). Default 2 GiB; between 256 MiB and 10 GiB and at most 5 % of the
//!   volume. Refused when free space is already below the forecast reserve
//!   (`[proactive] reserve_gib`), or would drop below it.
//! * **`release [--yes]`**: removes the reserve, only if it is exactly the file scull created
//!   (identity, kind and size from the manifest; links are never followed). The removal goes
//!   through `scull_cleaner::headroom::release`, i.e. the cleaner's walker (SAFE-10).
//!
//! Both changes ask first on a terminal (`[y/N]`); without one, `--yes` is required. Each
//! outcome is audited in the history database (`headroom` events), recorded **after** a
//! release, because a full disk may need the freed space for the database to write. When the
//! audit can't be written, the report says so (`audit_error`) and the command still succeeds.
//! Machine output: `scull/headroom@1`.

use super::{emit_machine, emit_text};
use crate::cli::HeadroomAction;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use schemars::JsonSchema;
use scull_cleaner::headroom::{self as cleaner, CreateOptions, HeadroomEvent, Released};
use scull_core::{ErrorKind, ScullError};
use scull_proactive::headroom::{
    DEFAULT_SIZE, HeadroomManifest, HeadroomState, HeadroomStatus, reserve_relative, status,
};
use serde::Serialize;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

/// `scull/headroom@1`.
pub const HEADROOM_SCHEMA: SchemaId = SchemaId::new("headroom", 1);

/// What `scull headroom` did.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadroomReport {
    /// `status`, `create` or `release`.
    pub action: String,
    /// The reserve's status after the command.
    pub status: HeadroomStatus,
    /// The reserve that was created (`create`).
    pub created: Option<HeadroomManifest>,
    /// What was released (`release`).
    pub released: Option<Released>,
    /// Why the outcome could not be recorded in the history database, when it couldn't.
    pub audit_error: Option<String>,
}

/// Parse a size: `2G`, `2GiB`, `512M`, `512MiB`, or bytes. Units are binary.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let digits: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let unit = t[digits.len()..].trim().to_ascii_lowercase();
    let n: f64 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a size (e.g. 2G, 512M)"))?;
    let mult: u64 = match unit.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return Err(format!("{s:?}: unknown unit (use M or G)")),
    };
    Ok((n * mult as f64).round() as u64)
}

/// `scull headroom …`.
pub fn run(action: &HeadroomAction, ui: &Ui) -> Result<(), ScullError> {
    let dirs =
        scull_platform::app_dirs().map_err(|e| ScullError::new(ErrorKind::Io, e.to_string()))?;
    let data = dirs.data;
    let now = scull_engine::now_unix();
    let interactive =
        io::stdin().is_terminal() && io::stderr().is_terminal() && !ui.format.is_machine();
    let report = match action {
        HeadroomAction::Status => HeadroomReport {
            action: "status".into(),
            status: status(&data),
            created: None,
            released: None,
            audit_error: None,
        },
        HeadroomAction::Create { size, yes } => {
            let size = size.unwrap_or(DEFAULT_SIZE);
            let path = data.join(reserve_relative());
            gate(
                *yes,
                interactive,
                &format!(
                    "Reserve {} of disk space in {}? It stays until you release it.",
                    format_bytes(size),
                    escape_for_terminal(&path.display().to_string())
                ),
            )?;
            let keep_free = super::pipeline::load_config(&[])
                .map(|l| scull_proactive::forecast_policy(&l.config.proactive).reserve_bytes)
                .unwrap_or_else(|_| scull_proactive::ForecastPolicy::default().reserve_bytes);
            if std::fs::symlink_metadata(&data).is_err() {
                scull_platform::create_dir_owner_only(&data)
                    .map_err(|e| ScullError::new(ErrorKind::Io, e.to_string()))?;
            }
            let created = cleaner::create(
                &data,
                &CreateOptions {
                    size_bytes: size,
                    keep_free_bytes: keep_free,
                    now,
                },
            );
            let manifest = match created {
                Ok(m) => m,
                Err(e) => {
                    let _ = record(
                        &data,
                        &event("refused", &path, None, 0, Some(e.message.clone())),
                        now,
                    );
                    return Err(e);
                }
            };
            let audit_error = record(
                &data,
                &event(
                    "created",
                    &path,
                    Some(manifest.clone()),
                    manifest.allocated_bytes,
                    None,
                ),
                now,
            )
            .err();
            HeadroomReport {
                action: "create".into(),
                status: status(&data),
                created: Some(manifest),
                released: None,
                audit_error,
            }
        }
        HeadroomAction::Release { yes } => {
            let before = status(&data);
            if before.state == HeadroomState::None {
                return Err(ScullError::new(
                    ErrorKind::NotFound,
                    "there is no headroom file",
                ));
            }
            let bytes = before.releasable_bytes.unwrap_or(0);
            gate(
                *yes,
                interactive,
                &format!(
                    "Release the {} headroom file in {}?",
                    format_bytes(bytes),
                    escape_for_terminal(&before.path)
                ),
            )?;
            let path = data.join(reserve_relative());
            let released = match cleaner::release(&data) {
                Ok(r) => r,
                Err(e) => {
                    let _ = record(
                        &data,
                        &event(
                            "refused",
                            &path,
                            before.manifest.clone(),
                            0,
                            Some(e.message.clone()),
                        ),
                        now,
                    );
                    return Err(e);
                }
            };
            let action = match released.outcome {
                cleaner::ReleaseOutcome::Released => "released",
                cleaner::ReleaseOutcome::AlreadyGone => "already_gone",
            };
            // After the release: the database may need the space it freed.
            let audit_error = record(
                &data,
                &event(
                    action,
                    &path,
                    Some(released.manifest.clone()),
                    released.freed_bytes,
                    None,
                ),
                now,
            )
            .err();
            HeadroomReport {
                action: "release".into(),
                status: status(&data),
                created: None,
                released: Some(released),
                audit_error,
            }
        }
    };
    if ui.format.is_machine() {
        return emit_machine(ui.format, HEADROOM_SCHEMA, "headroom", &report);
    }
    emit_text(&render(&report))
}

fn event(
    action: &str,
    path: &Path,
    manifest: Option<HeadroomManifest>,
    bytes: u64,
    reason: Option<String>,
) -> HeadroomEvent {
    HeadroomEvent {
        action: action.to_owned(),
        path: path.display().to_string(),
        manifest,
        bytes,
        reason,
    }
}

/// Append `e` to the history database (created if missing, like any recording command).
fn record(data: &Path, e: &HeadroomEvent, now: i64) -> Result<(), String> {
    let mut store = scull_engine::open_history(data).map_err(|e| e.to_string())?;
    cleaner::audit(&mut store, e, "cli", now).map_err(|e| e.to_string())
}

/// Ask on a terminal, or require `--yes`.
fn gate(yes: bool, interactive: bool, question: &str) -> Result<(), ScullError> {
    if yes {
        return Ok(());
    }
    if !interactive {
        return Err(ScullError::new(
            ErrorKind::Usage,
            "this changes the disk: pass --yes (no terminal to ask on)",
        ));
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut err = io::stderr().lock();
    if ask(&mut input, &mut err, question).map_err(|e| ScullError::from_io(&e))? {
        Ok(())
    } else {
        Err(ScullError::new(ErrorKind::Cancelled, "nothing was changed"))
    }
}

/// Write `question [y/N] ` to `err` and read the answer from `input`.
pub fn ask(input: &mut dyn BufRead, err: &mut dyn Write, question: &str) -> io::Result<bool> {
    write!(err, "{question} [y/N] ")?;
    err.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Human output.
pub fn render(r: &HeadroomReport) -> String {
    let path = escape_for_terminal(&r.status.path);
    let mut out = String::new();
    match (&r.created, &r.released) {
        (Some(m), _) => out.push_str(&format!(
            "Reserved {} in {path}. Release it with `scull headroom release` when the disk is full.\n",
            format_bytes(m.allocated_bytes)
        )),
        (_, Some(rel)) => match rel.outcome {
            cleaner::ReleaseOutcome::Released => out.push_str(&format!(
                "Released the headroom file: {} free again. Run the cleanup now (`scull clean`), \
                 then `scull headroom create` to set a new reserve.\n",
                format_bytes(rel.freed_bytes)
            )),
            cleaner::ReleaseOutcome::AlreadyGone => {
                out.push_str("The headroom file was already gone; its record was removed.\n");
            }
        },
        _ => out.push_str(&status_line(&r.status)),
    }
    if let Some(e) = &r.audit_error {
        out.push_str(&format!(
            "warning: the history database could not record this ({})\n",
            escape_for_terminal(e)
        ));
    }
    out
}

/// One line for `scull headroom status` (and `scull status`).
pub fn status_line(s: &HeadroomStatus) -> String {
    let path = escape_for_terminal(&s.path);
    let why = s
        .detail
        .as_deref()
        .map(escape_for_terminal)
        .unwrap_or_default();
    match s.state {
        HeadroomState::None => {
            "No headroom file (off). `scull headroom create` reserves 2.0 GiB for emergencies.\n"
                .to_owned()
        }
        HeadroomState::Ready => format!(
            "Headroom: {} reserved in {path}; `scull headroom release` frees it.\n",
            format_bytes(s.releasable_bytes.unwrap_or(0))
        ),
        HeadroomState::Missing => format!(
            "Headroom: the file is gone ({path}); `scull headroom release` clears its record.\n"
        ),
        HeadroomState::Changed => format!(
            "Headroom: {path} is not the file scull created ({why}); scull won't release it.\n"
        ),
        HeadroomState::Unreadable => {
            format!("Headroom: the record can't be read ({why}); scull won't release anything.\n")
        }
        HeadroomState::Unrecorded => {
            format!("Headroom: {path} exists but scull has no record of it; it is left alone.\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_in_binary_units() {
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert_eq!(parse_size("2GiB").unwrap(), 2 << 30);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("1.5g").unwrap(), 3 << 29);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("2T").is_err());
        assert!(parse_size("lots").is_err());
    }

    #[test]
    fn asking_needs_a_yes() {
        let mut err = Vec::new();
        assert!(ask(&mut &b"y\n"[..], &mut err, "Q?").unwrap());
        assert!(ask(&mut &b"YES\n"[..], &mut err, "Q?").unwrap());
        assert!(!ask(&mut &b"\n"[..], &mut err, "Q?").unwrap());
        assert!(!ask(&mut &b"n\n"[..], &mut err, "Q?").unwrap());
        assert!(String::from_utf8(err).unwrap().starts_with("Q? [y/N] "));
    }

    #[test]
    fn non_interactive_changes_need_yes() {
        let e = gate(false, false, "Q?").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(gate(true, false, "Q?").is_ok());
    }
}
