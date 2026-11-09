//! `scull scan --ssh HOST:PATH` (P6-RES-01 preview; docs/REMOTE_SCANNING.md): run the remote
//! host's own `scull scan --json --no-history` over the system `ssh` client and show the
//! validated report. Inspect-only: nothing is written on the host or locally.
//!
//! * **Trusted `ssh`.** Resolved from absolute `PATH` entries only (never the current folder,
//!   relative entries or `node_modules`), spawned by absolute path, no shell, stdin closed.
//! * **Locked-down client options.** Host keys must already be known (`StrictHostKeyChecking=yes`,
//!   `BatchMode=yes`, `UpdateHostKeys=no`); no agent, X11 or port forwarding, no local command,
//!   no TTY, no escape character.
//! * **Fixed remote command.** Built here from an allowlist, each argument single-quoted for the
//!   remote POSIX shell; the host is validated and passed after `--`.
//! * **Untrusted output.** stdout capped at [`MAX_OUTPUT_BYTES`], a whole-run timeout, the
//!   document validated against the published `scan.schema.json`, every string escaped when
//!   shown, remote paths prefixed with the host.

use super::render;
use crate::cancel;
use crate::commands::ignore_broken_pipe;
use crate::output::escape::escape_for_terminal;
use crate::output::style::Style;
use crate::output::{Format, Ui};
use scull_core::{Candidate, Category, ErrorKind, ScullError, TierBreakdown};
use scull_engine::{Totals, WorktreeTotals};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Largest remote report read (bytes).
pub const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Largest remote stderr kept (bytes).
const MAX_STDERR_BYTES: u64 = 64 * 1024;

/// Longest a remote scan may take, connection included.
pub const TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The published schema the remote document must match.
const SCAN_SCHEMA_JSON: &str = include_str!("../../schemas/scan.schema.json");

/// The client options every run uses (docs/REMOTE_SCANNING.md §4).
pub const SSH_OPTIONS: [&str; 10] = [
    "BatchMode=yes",
    "StrictHostKeyChecking=yes",
    "UpdateHostKeys=no",
    "ForwardAgent=no",
    "ForwardX11=no",
    "ClearAllForwardings=yes",
    "PermitLocalCommand=no",
    "RequestTTY=no",
    "ConnectTimeout=15",
    "ServerAliveInterval=30",
];

/// A parsed `HOST:PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `host`, `user@host`, an ssh config alias, or `[v6addr]`.
    pub host: String,
    /// The remote path as typed (absolute or `~/...`).
    pub path: String,
}

fn usage(msg: impl Into<String>) -> ScullError {
    ScullError::new(ErrorKind::Usage, msg)
}

/// Parse and validate `HOST:PATH`.
pub fn parse_target(s: &str) -> Result<Target, ScullError> {
    let (host, path) = if let Some(rest) = s.strip_prefix('[') {
        let end = rest
            .find("]:")
            .ok_or_else(|| usage("--ssh: an IPv6 host must look like [addr]:PATH"))?;
        (format!("[{}]", &rest[..end]), &rest[end + 2..])
    } else {
        let (h, p) = s
            .split_once(':')
            .ok_or_else(|| usage("--ssh takes HOST:PATH, e.g. build-box:~/src"))?;
        (h.to_owned(), p)
    };
    let bad_host = host.is_empty()
        || host.starts_with('-')
        || host.len() > 255
        || host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "'\"`$;&|<>\\(){}*?!#".contains(c));
    if bad_host {
        return Err(usage(
            "--ssh: invalid host (letters, digits and .-_@[]: only; it cannot start with -)",
        ));
    }
    if path.is_empty() {
        return Err(usage(
            "--ssh: give a remote path after the colon (HOST:PATH)",
        ));
    }
    if path.chars().any(char::is_control) {
        return Err(usage("--ssh: the remote path contains control characters"));
    }
    if !(path.starts_with('/') || path == "~" || path.starts_with("~/")) {
        return Err(usage(
            "--ssh: the remote path must be absolute or start with ~/",
        ));
    }
    Ok(Target {
        host,
        path: path.to_owned(),
    })
}

/// Quote `s` for a POSIX shell: `'...'` with each `'` written as `'\''`.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The remote path argument: quoted, except a leading `~/` that the remote shell must expand.
pub fn remote_path_arg(path: &str) -> String {
    if path == "~" {
        return "~".to_owned();
    }
    match path.strip_prefix("~/") {
        Some("") => "~/".to_owned(),
        Some(rest) => format!("~/{}", sh_quote(rest)),
        None => sh_quote(path),
    }
}

/// The command line the remote shell runs. Only this fixed, read-only command is ever sent.
pub fn remote_command(target: &Target, remote_bin: Option<&str>) -> Result<String, ScullError> {
    let bin = match remote_bin {
        Some(b) if b.chars().any(char::is_control) || b.is_empty() => {
            return Err(usage("--remote-bin: invalid path"));
        }
        Some(b) => remote_path_arg(b),
        None => sh_quote("scull"),
    };
    Ok(format!(
        "{bin} {} {} {} -- {}",
        sh_quote("scan"),
        sh_quote("--json"),
        sh_quote("--no-history"),
        remote_path_arg(&target.path)
    ))
}

/// The arguments given to `ssh` (after the binary).
pub fn ssh_args(target: &Target, remote_cmd: &str) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec!["-T".into(), "-e".into(), "none".into()];
    for o in SSH_OPTIONS {
        a.push("-o".into());
        a.push(o.into());
    }
    a.push("--".into());
    a.push(target.host.clone().into());
    a.push(remote_cmd.into());
    a
}

fn has_node_modules(p: &Path) -> bool {
    p.components().any(|c| c.as_os_str() == "node_modules")
}

/// The `ssh` client from trusted `PATH` entries (absolute, existing, not the current folder,
/// not inside `node_modules`), canonicalized. `None` if there is none.
pub fn resolve_ssh(path_env: &OsStr, cwd: Option<&Path>) -> Option<PathBuf> {
    let cwd = cwd.and_then(|c| std::fs::canonicalize(c).ok());
    let name = format!("ssh{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(path_env).find_map(|entry| {
        if entry.as_os_str().is_empty() || !entry.is_absolute() || has_node_modules(&entry) {
            return None;
        }
        let dir = std::fs::canonicalize(&entry).ok()?;
        if cwd.as_deref() == Some(dir.as_path()) || has_node_modules(&dir) {
            return None;
        }
        let candidate = dir.join(&name);
        if !candidate.is_file() {
            return None;
        }
        let real = std::fs::canonicalize(&candidate).ok()?;
        (!has_node_modules(&real)).then_some(real)
    })
}

/// What the remote run produced.
struct RunOutput {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

fn read_capped<R: Read + Send + 'static>(
    r: R,
    cap: u64,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut limited = r.take(cap + 1);
        let _ = limited.read_to_end(&mut buf);
        let truncated = buf.len() as u64 > cap;
        buf.truncate(cap as usize);
        // Drain the rest so the child never blocks on a full pipe.
        let _ = io::copy(&mut limited.into_inner(), &mut io::sink());
        (buf, truncated)
    })
}

fn run_ssh(ssh: &Path, args: &[OsString], timeout: Duration) -> Result<RunOutput, ScullError> {
    let mut child = Command::new(ssh)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ScullError::new(ErrorKind::Io, format!("could not start ssh: {e}")))?;
    let out = read_capped(child.stdout.take().expect("piped stdout"), MAX_OUTPUT_BYTES);
    let err = read_capped(child.stderr.take().expect("piped stderr"), MAX_STDERR_BYTES);
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => return Err(ScullError::from_io(&e)),
        }
        if cancel::is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(cancel::cancelled_error());
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ScullError::new(
                ErrorKind::Io,
                format!(
                    "the remote scan took longer than {} minutes",
                    timeout.as_secs() / 60
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stdout, truncated) = out.join().unwrap_or_default();
    let (stderr, _) = err.join().unwrap_or_default();
    Ok(RunOutput {
        status: status.code(),
        stdout,
        stderr,
        truncated,
    })
}

/// The last few lines of `stderr`, escaped and capped, for an error message.
fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    // A remote scull in --json mode prints an error envelope: show just its message.
    if let Some(msg) = text.lines().rev().find_map(|l| {
        serde_json::from_str::<Value>(l).ok().and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    }) {
        return escape_for_terminal(&msg.chars().take(500).collect::<String>()).into_owned();
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" / ");
    escape_for_terminal(&tail.chars().take(500).collect::<String>()).into_owned()
}

/// Validate `bytes` as a `scull/scan@1` document.
pub fn validate(bytes: &[u8]) -> Result<Value, ScullError> {
    let bad = |m: String| {
        ScullError::new(
            ErrorKind::Io,
            format!("the remote host did not send a valid scull/scan@1 report: {m}"),
        )
    };
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| bad(format!("not JSON ({e})")))?;
    let schema: Value = serde_json::from_str(SCAN_SCHEMA_JSON)
        .map_err(|e| ScullError::new(ErrorKind::Internal, e.to_string()))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|e| ScullError::new(ErrorKind::Internal, e.to_string()))?;
    if let Some(e) = validator.iter_errors(&doc).next() {
        let path = e.instance_path().to_string();
        let msg = e.to_string();
        return Err(bad(escape_for_terminal(&format!(
            "{} at {}",
            msg.chars().take(200).collect::<String>(),
            if path.is_empty() { "/" } else { &path }
        ))
        .into_owned()));
    }
    Ok(doc)
}

/// Run the remote scan and print it.
pub fn scan(target: &str, remote_bin: Option<&str>, ui: &Ui) -> Result<(), ScullError> {
    if ui.format == Format::Ndjson {
        return Err(usage(
            "--ssh supports the human report and --json, not --ndjson",
        ));
    }
    let target = parse_target(target)?;
    let cmd = remote_command(&target, remote_bin)?;
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let cwd = std::env::current_dir().ok();
    let ssh = resolve_ssh(&path_env, cwd.as_deref()).ok_or_else(|| {
        ScullError::new(
            ErrorKind::NotFound,
            "no trusted ssh client found on PATH (absolute PATH entries only)",
        )
    })?;
    ui.progress(&format!(
        "scanning {}:{} over ssh...",
        target.host, target.path
    ));
    ui.debug(&format!("ssh: {}", ssh.display()));
    let out = run_ssh(&ssh, &ssh_args(&target, &cmd), TIMEOUT)?;
    match out.status {
        Some(0) => {}
        Some(255) => {
            return Err(ScullError::new(
                ErrorKind::Io,
                format!(
                    "ssh could not run the scan on {} (unknown host key, authentication or \
                     connection): {}",
                    escape_for_terminal(&target.host),
                    stderr_tail(&out.stderr)
                ),
            ));
        }
        Some(127) => {
            return Err(ScullError::new(
                ErrorKind::NotFound,
                format!(
                    "scull was not found on {} (install it there, or pass --remote-bin): {}",
                    escape_for_terminal(&target.host),
                    stderr_tail(&out.stderr)
                ),
            ));
        }
        code => {
            return Err(ScullError::new(
                ErrorKind::Io,
                format!(
                    "the remote scull failed (exit {}): {}",
                    code.map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                    stderr_tail(&out.stderr)
                ),
            ));
        }
    }
    if out.truncated {
        return Err(ScullError::new(
            ErrorKind::Io,
            format!(
                "the remote report is larger than {} MiB",
                MAX_OUTPUT_BYTES >> 20
            ),
        ));
    }
    let doc = validate(&out.stdout)?;
    let text = match ui.format {
        Format::Json => {
            let mut s = serde_json::to_string_pretty(&doc)
                .map_err(|e| ScullError::new(ErrorKind::Internal, e.to_string()))?;
            s.push('\n');
            s
        }
        _ => render_remote(&target.host, &doc, ui.color_out)?,
    };
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))
}

fn tiers(v: &Value, ptr: &str) -> Result<TierBreakdown, ScullError> {
    serde_json::from_value(v.pointer(ptr).cloned().unwrap_or(Value::Null))
        .map_err(|e| ScullError::new(ErrorKind::Io, format!("remote totals: {e}")))
}

/// The human report of a validated remote document: the same tables as a local scan, with the
/// host named and paths relative to the (single) remote root.
pub fn render_remote(host: &str, doc: &Value, color: bool) -> Result<String, ScullError> {
    let data = &doc["data"];
    let candidates: Vec<Candidate> = serde_json::from_value(data["candidates"].clone())
        .map_err(|e| ScullError::new(ErrorKind::Io, format!("remote candidates: {e}")))?;
    let totals = Totals {
        count: tiers(data, "/totals/count")?,
        logical: tiers(data, "/totals/logical")?,
        allocated: tiers(data, "/totals/allocated")?,
        reclaimable: tiers(data, "/totals/reclaimable")?,
        worktrees: WorktreeTotals {
            count: tiers(data, "/totals/worktrees/count")?,
            allocated: tiers(data, "/totals/worktrees/allocated")?,
            reclaimable: tiers(data, "/totals/worktrees/reclaimable")?,
        },
    };
    let roots: Vec<String> = data["roots"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let host_shown = escape_for_terminal(host).into_owned();
    let mut out = format!(
        "{} {} ({} entries, scanned on the remote host over ssh{})\n\n",
        Style::Bold.paint("Remote scan of", color),
        roots
            .iter()
            .map(|r| format!("{host_shown}:{}", escape_for_terminal(r)))
            .collect::<Vec<_>>()
            .join(", "),
        data["entries_scanned"].as_u64().unwrap_or(0),
        if data["cancelled"].as_bool() == Some(true) {
            "; CANCELLED: partial results"
        } else {
            ""
        }
    );
    let base = match roots.as_slice() {
        [one] => Some(PathBuf::from(one)),
        _ => None,
    };
    let (worktrees, artifacts): (Vec<Candidate>, Vec<Candidate>) = candidates
        .into_iter()
        .partition(|c| c.category == Category::Worktree);
    if artifacts.is_empty() {
        out.push_str("No reclaimable artifacts found.\n");
    } else {
        out.push_str(&render::candidates_table(
            &artifacts,
            base.as_deref(),
            color,
        ));
    }
    if !worktrees.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            Style::Bold.paint("Git worktrees:", color)
        ));
        out.push_str(&render::candidates_table(
            &worktrees,
            base.as_deref(),
            color,
        ));
    }
    out.push_str(&format!(
        "\n{}\n",
        Style::Bold.paint("Totals by tier:", color)
    ));
    out.push_str(&render::totals_table(&totals, color));
    out.push_str(&render::estimate_note(artifacts.iter().chain(&worktrees)));
    out.push('\n');
    out.push_str(&format!(
        "Paths are on {host_shown}. Read-only preview: nothing was changed or recorded on either \
         machine, and remote results cannot be cleaned from here.\n"
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn targets_parse_and_reject_option_injection() {
        let t = parse_target("me@build-box:~/src").unwrap();
        assert_eq!(
            (t.host.as_str(), t.path.as_str()),
            ("me@build-box", "~/src")
        );
        let t = parse_target("[::1]:/srv/a b").unwrap();
        assert_eq!((t.host.as_str(), t.path.as_str()), ("[::1]", "/srv/a b"));
        for bad in [
            "-oProxyCommand=evil:/x",
            "host",
            "host:",
            "host:relative/path",
            "ho st:/x",
            "h$(x):/x",
            "host:/a\nb",
            "[::1/x",
            ":/x",
        ] {
            assert!(parse_target(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn remote_arguments_are_quoted_for_a_posix_shell() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(remote_path_arg("/srv/a b"), "'/srv/a b'");
        assert_eq!(remote_path_arg("~/src; rm -rf /"), "~/'src; rm -rf /'");
        assert_eq!(remote_path_arg("~"), "~");
        let t = parse_target("h:/x'y").unwrap();
        assert_eq!(
            remote_command(&t, None).unwrap(),
            r"'scull' 'scan' '--json' '--no-history' -- '/x'\''y'"
        );
        assert_eq!(
            remote_command(&t, Some("~/.cargo/bin/scull")).unwrap(),
            r"~/'.cargo/bin/scull' 'scan' '--json' '--no-history' -- '/x'\''y'"
        );
    }

    #[test]
    fn ssh_is_locked_down() {
        let t = parse_target("h:/x").unwrap();
        let a: Vec<String> = ssh_args(&t, "cmd")
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        for o in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "ForwardAgent=no",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
        ] {
            assert!(a.windows(2).any(|w| w[0] == "-o" && w[1] == o), "{o}");
        }
        assert_eq!(&a[a.len() - 3..], ["--", "h", "cmd"]);
        assert_eq!(&a[..3], ["-T", "-e", "none"]);
    }

    #[test]
    fn ssh_is_only_resolved_from_trusted_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("bin");
        let nm = dir.path().join("node_modules").join(".bin");
        for d in [&good, &nm] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join(format!("ssh{}", std::env::consts::EXE_SUFFIX)), "").unwrap();
        }
        let path = std::env::join_paths([nm.clone(), PathBuf::from("rel"), good.clone()]).unwrap();
        let found = resolve_ssh(&path, None).unwrap();
        assert_eq!(
            found.parent().unwrap(),
            std::fs::canonicalize(&good).unwrap()
        );
        // The current folder is never trusted.
        assert_eq!(
            resolve_ssh(&std::env::join_paths([good.clone()]).unwrap(), Some(&good)),
            None
        );
    }

    #[test]
    fn invalid_documents_are_refused() {
        for bad in [
            &b"not json"[..],
            br#"{"schema":"scull/scan@1"}"#,
            br#"{"schema":"scull/scan@2","version":"9","data":{}}"#,
        ] {
            let e = validate(bad).unwrap_err();
            assert!(e.message.contains("valid scull/scan@1"), "{}", e.message);
        }
    }
}
