//! `scull`: a safe developer storage analyzer and cleanup tool (PRD §12).
//!
//! `main` stays thin so cold startup is fast: parse arguments, build the [`output::Ui`],
//! dispatch, and map any [`ScullError`] to its stable exit code (CLI-05).

use scull_cli::{cli, commands, output};

use clap::{CommandFactory, FromArgMatches};
use cli::Cli;
use output::{ColorWhen, Format, Ui};
use scull_core::{ErrorKind, ExitCode, ScullError};
use std::ffi::OsString;

fn main() -> std::process::ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match parse(&args) {
        Ok(cli) => cli,
        Err(code) => return code.into(),
    };
    let g = &cli.global;
    let ui = Ui::new(g.format(), g.quiet, g.color, g.no_progress, g.verbose);
    match commands::run(cli, &ui) {
        Ok(()) => ExitCode::Success.into(),
        Err(err) => {
            ui.report_error(&err);
            err.exit_code().into()
        }
    }
}

/// Parse arguments. Help and version print to stdout and exit 0; usage errors exit 2, as a
/// JSON error envelope on stderr when `--json`/`--ndjson` was requested.
fn parse(args: &[OsString]) -> Result<Cli, ExitCode> {
    let color = prescan_color(args);
    let cmd = Cli::command().color(match color {
        ColorWhen::Always => clap::ColorChoice::Always,
        ColorWhen::Never => clap::ColorChoice::Never,
        ColorWhen::Auto if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) => {
            clap::ColorChoice::Never
        }
        ColorWhen::Auto => clap::ColorChoice::Auto,
    });
    let err = match cmd
        .try_get_matches_from(args)
        .and_then(|m| Cli::from_arg_matches(&m))
        .and_then(|cli| cli.check_global_conflicts().map(|()| cli))
    {
        Ok(cli) => return Ok(cli),
        Err(err) => err,
    };
    if !err.use_stderr() {
        // --help / --version: not an error.
        let _ = err.print();
        return Err(ExitCode::Success);
    }
    if prescan_machine(args) {
        let e = ScullError::new(ErrorKind::Usage, usage_message(&err.render().to_string()));
        let ui = Ui::new(Format::Json, false, ColorWhen::Never, true, 0);
        ui.report_error(&e);
    } else {
        let _ = err.print();
    }
    Err(ExitCode::Usage)
}

/// The first paragraph of a rendered clap error, without the `error: ` prefix, for the JSON
/// error envelope (the usage hint and "try --help" lines are for people).
fn usage_message(rendered: &str) -> String {
    let first = rendered.split("\n\n").next().unwrap_or(rendered).trim();
    first.strip_prefix("error: ").unwrap_or(first).to_owned()
}

/// The `--color` value, read before clap so help and usage errors honor it.
fn prescan_color(args: &[OsString]) -> ColorWhen {
    let mut it = args.iter().skip(1).filter_map(|a| a.to_str());
    let mut found = ColorWhen::Auto;
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        let value = match a.strip_prefix("--color") {
            Some("") => it.next(),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        };
        match value {
            Some("always") => found = ColorWhen::Always,
            Some("never") => found = ColorWhen::Never,
            Some("auto") => found = ColorWhen::Auto,
            _ => {}
        }
    }
    found
}

/// Whether `--json` or `--ndjson` appears, read before clap so usage errors can be JSON.
fn prescan_machine(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--json" || a == "--ndjson")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn prescan_reads_color_and_machine_flags() {
        assert_eq!(
            prescan_color(&os(&["scull", "--color", "never"])),
            ColorWhen::Never
        );
        assert_eq!(
            prescan_color(&os(&["scull", "x", "--color=always"])),
            ColorWhen::Always
        );
        assert_eq!(
            prescan_color(&os(&["scull", "--", "--color=never"])),
            ColorWhen::Auto
        );
        assert!(prescan_machine(&os(&["scull", "nope", "--ndjson"])));
        assert!(!prescan_machine(&os(&["scull", "scan", "--", "--json"])));
    }

    #[test]
    fn usage_message_keeps_first_paragraph() {
        assert_eq!(
            usage_message("error: unrecognized subcommand 'x'\n\nUsage: scull\n\nFor more\n"),
            "unrecognized subcommand 'x'"
        );
        assert_eq!(usage_message("plain"), "plain");
    }
}
