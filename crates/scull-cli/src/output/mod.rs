//! The output layer: where data, progress and errors go, and how they look.
//!
//! * Data goes to **stdout**; progress, diagnostics and errors go to **stderr** (CLI-02).
//! * Human mode renders [`table::Table`]s with every path escaped (SAFE-17) and every size
//!   labeled (PHY-01). Color is optional and never the only signal.
//! * `--json` / `--ndjson` use the versioned envelopes in [`json`].

pub mod bytes;
pub mod escape;
pub mod json;
pub mod style;
pub mod table;

use clap::ValueEnum;
use scull_core::ScullError;
use std::io::{self, IsTerminal, Write};
use style::Style;

/// How data is written to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Tables and prose for people.
    Human,
    /// One JSON document (`--json`).
    Json,
    /// One JSON object per line (`--ndjson`).
    Ndjson,
}

impl Format {
    /// Whether this is a machine-readable format.
    pub fn is_machine(self) -> bool {
        self != Format::Human
    }
}

/// The `--color` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ColorWhen {
    /// Color when writing to a terminal, unless `NO_COLOR` is set or `TERM=dumb`.
    #[default]
    Auto,
    /// Always color, even when piped (overrides `NO_COLOR`).
    Always,
    /// Never color.
    Never,
}

impl ColorWhen {
    /// Resolve against the environment for a stream that is (or is not) a terminal.
    ///
    /// `no_color` is the value of `NO_COLOR` (any non-empty value disables auto color, per
    /// <https://no-color.org>), `term` the value of `TERM`.
    pub fn resolve(self, is_terminal: bool, no_color: Option<&str>, term: Option<&str>) -> bool {
        match self {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto => {
                is_terminal && no_color.is_none_or(str::is_empty) && term != Some("dumb")
            }
        }
    }
}

/// Settings shared by every command, derived from the global flags.
#[derive(Debug, Clone)]
pub struct Ui {
    /// Stdout format.
    pub format: Format,
    /// `--quiet`: suppress progress and informational messages (errors still print).
    pub quiet: bool,
    /// Color stdout.
    pub color_out: bool,
    /// Color stderr.
    pub color_err: bool,
    /// Show progress on stderr.
    pub progress: bool,
    /// `-v` count.
    pub verbosity: u8,
}

impl Ui {
    /// Build from the parsed global flags and the process environment.
    pub fn new(
        format: Format,
        quiet: bool,
        color: ColorWhen,
        no_progress: bool,
        verbosity: u8,
    ) -> Self {
        let no_color = std::env::var("NO_COLOR").ok();
        let term = std::env::var("TERM").ok();
        let stderr_tty = io::stderr().is_terminal();
        let resolve = |tty| color.resolve(tty, no_color.as_deref(), term.as_deref());
        Self {
            format,
            quiet,
            // Machine output is never colored.
            color_out: !format.is_machine() && resolve(io::stdout().is_terminal()),
            color_err: resolve(stderr_tty),
            progress: !quiet && !no_progress && stderr_tty,
            verbosity: if quiet { 0 } else { verbosity },
        }
    }

    /// A transient progress message on stderr, shown only when progress is enabled.
    pub fn progress(&self, msg: &str) {
        if self.progress {
            let _ = writeln!(io::stderr(), "{}", escape::escape_for_terminal(msg));
        }
    }

    /// A debug message on stderr, shown at `-v` and above.
    pub fn debug(&self, msg: &str) {
        if self.verbosity > 0 {
            let line = format!("debug: {}", escape::escape_for_terminal(msg));
            let _ = writeln!(io::stderr(), "{}", Style::Dim.paint(&line, self.color_err));
        }
    }

    /// Report an error on stderr: a JSON error envelope in machine modes, a one-line message
    /// otherwise. Errors are printed even with `--quiet`.
    pub fn report_error(&self, err: &ScullError) {
        let stderr = io::stderr();
        let mut lock = stderr.lock();
        if self.format.is_machine() {
            let _ = json::write_error(&mut lock, err);
        } else {
            let _ = writeln!(lock, "{}", human_error(err, self.color_err));
        }
    }
}

/// The human form of an error: `scull: error: <message> (<path>)`, escaped for the terminal.
pub fn human_error(err: &ScullError, color: bool) -> String {
    let mut msg = escape::escape_for_terminal(&err.message).into_owned();
    if let Some(p) = &err.path {
        msg.push_str(" (path: ");
        msg.push_str(&escape::escape_for_terminal(p.as_str()));
        msg.push(')');
    }
    format!("scull: {} {msg}", Style::Error.paint("error:", color))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::{ErrorKind, FsPath};

    #[test]
    fn color_resolution() {
        use ColorWhen::*;
        assert!(Always.resolve(false, Some("1"), Some("dumb")));
        assert!(!Never.resolve(true, None, None));
        assert!(Auto.resolve(true, None, Some("xterm-256color")));
        assert!(
            Auto.resolve(true, Some(""), None),
            "empty NO_COLOR is ignored"
        );
        assert!(!Auto.resolve(true, Some("1"), None));
        assert!(!Auto.resolve(true, None, Some("dumb")));
        assert!(!Auto.resolve(false, None, None));
    }

    #[test]
    fn human_errors_are_escaped() {
        let e = ScullError::new(ErrorKind::NotFound, "missing\x1b[2J")
            .with_path(FsPath::from("/p/\u{202E}gpj.exe"));
        assert_eq!(
            human_error(&e, false),
            "scull: error: missing\\x1b[2J (path: /p/\\u{202e}gpj.exe)"
        );
    }
}
