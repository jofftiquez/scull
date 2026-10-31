//! `scull insights [--since WHEN]` and `scull insights export [--format json|csv]`
//! (P4-INS-01; PRD §15 INS-02…04, DATA-07).
//!
//! * **`insights`** reads the local history database and prints top growers, regrowth,
//!   duplication, agent footprint, predicted-vs-measured accuracy, ecosystem mix, free space
//!   and scull's own footprint against its storage budget ([`scull_insights`]). Read-only: it
//!   never creates, migrates or writes the database ([`crate::db`]); with no history yet it
//!   reports an empty window. `--since` takes a duration back from now (`7d`, `30d`, `12w`,
//!   `48h`) or a UTC date (`2026-10-01`); the default is 30 days. Machine output:
//!   `scull/insights@1` (`--ndjson`: one `insights` record).
//! * **`export`** writes the same report to stdout: `--format json` (default) is the
//!   `scull/insights@1` document; `--format csv` is one tidy table
//!   ([`scull_insights::csv`]). Redirect it to a file to keep it. Parquet (DATA-07) is not built.
//! * **`purge`** is refused in this build: the inspect-only release gate requires it during the
//!   alpha. The storage side exists ([`scull_storage::Store::purge_insights`]); enabling the
//!   command is tracked in #266.

use super::{emit_machine, emit_text};
use crate::cli::{ExportFormat, InsightsAction};
use crate::commands::{Planned, not_implemented};
use crate::db::{self, NotOpened, Probe};
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::{SchemaId, write_document};
use crate::output::style::Style;
use scull_core::{ErrorKind, ScullError};
use scull_insights::time::{DAY, iso_date, unix_of};
use scull_insights::{InsightsInput, InsightsReport};
use scull_proactive::ForecastState;
use scull_storage::{DEFAULT_BUDGET_BYTES, Footprint, Store, StoreError};
use std::fmt::Write as _;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

/// Schema of `scull insights --json` and `scull insights export --format json`.
pub const INSIGHTS_SCHEMA: SchemaId = SchemaId::new("insights", 1);

/// `scull insights purge` is refused in this build (inspect-only release gate); see #266.
pub const PURGE: Planned = Planned {
    name: "insights purge",
    issue: Some(266),
};
/// A point in time given on the command line: a duration back from now, or a UTC date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// This many seconds before now.
    Ago(i64),
    /// 00:00 UTC on a date (Unix seconds).
    At(i64),
}

impl When {
    /// The Unix time this resolves to at `now`.
    pub fn resolve(self, now: i64) -> i64 {
        match self {
            When::Ago(s) => now.saturating_sub(s),
            When::At(t) => t,
        }
    }
}

/// Parse `90m`, `12h`, `30d`, `2w` or `YYYY-MM-DD` (clap value parser).
pub fn parse_when(s: &str) -> Result<When, String> {
    let bad = || {
        format!(
            "invalid time {s:?}: use a duration like 7d, 12h, 2w, 90m or a date like 2026-10-01"
        )
    };
    let s = s.trim();
    if let [y, m, d] = s.split('-').collect::<Vec<_>>()[..]
        && y.len() == 4
        && m.len() == 2
        && d.len() == 2
    {
        let (y, m, d): (i64, u32, u32) = (
            y.parse().map_err(|_| bad())?,
            m.parse().map_err(|_| bad())?,
            d.parse().map_err(|_| bad())?,
        );
        let t = unix_of(y, m, d);
        // Reject dates that normalize to another day (2026-02-30).
        if !(1..=12).contains(&m) || d == 0 || iso_date(t) != s {
            return Err(bad());
        }
        return Ok(When::At(t));
    }
    let unit = s.chars().last().ok_or_else(bad)?;
    let n: i64 = s[..s.len() - unit.len_utf8()].parse().map_err(|_| bad())?;
    let secs = match unit {
        'm' => 60,
        'h' => 3_600,
        'd' => DAY,
        'w' => 7 * DAY,
        _ => return Err(bad()),
    };
    if n <= 0 {
        return Err(bad());
    }
    n.checked_mul(secs).map(When::Ago).ok_or_else(bad)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `scull insights [--since]` and its subcommands.
pub fn run(
    since: Option<When>,
    action: Option<&InsightsAction>,
    ui: &Ui,
) -> Result<(), ScullError> {
    let now = now();
    match action {
        None => {
            let report = collect(now, since)?;
            if ui.format.is_machine() {
                return emit_machine(ui.format, INSIGHTS_SCHEMA, "insights", &report);
            }
            emit_text(&render(&report, ui.color_out))
        }
        Some(InsightsAction::Export {
            format,
            since: export_since,
        }) => {
            if ui.format.is_machine() && *format == ExportFormat::Csv {
                return Err(ScullError::new(
                    ErrorKind::Usage,
                    "--json/--ndjson can't be combined with `export --format csv`",
                ));
            }
            let report = collect(now, export_since.or(since))?;
            match format {
                ExportFormat::Json => {
                    let out = io::stdout().lock();
                    write_document(out, INSIGHTS_SCHEMA, &report)
                        .or_else(crate::commands::ignore_broken_pipe)
                        .map_err(|e| ScullError::from_io(&e))
                }
                ExportFormat::Csv => emit_text(&scull_insights::csv::to_csv(&report)),
            }
        }
        Some(InsightsAction::Purge { .. }) => Err(not_implemented(PURGE)),
    }
}

fn data_dir() -> Result<std::path::PathBuf, ScullError> {
    scull_platform::app_dirs()
        .map(|d| d.data)
        .map_err(|e| ScullError::new(ErrorKind::Config, e.to_string()))
}

/// Open the existing current-schema database, or `None` when there is none yet.
fn open_existing() -> Result<Option<Store>, ScullError> {
    let path = db::db_path(&data_dir()?);
    match db::open_current(&path) {
        Ok(s) => Ok(Some(s)),
        Err(NotOpened::Probe(Probe::Missing | Probe::Uninitialized)) => Ok(None),
        Err(NotOpened::Probe(Probe::Scull { version }))
            if version > scull_storage::SCHEMA_VERSION =>
        {
            Err(ScullError::new(
                ErrorKind::Config,
                "the history database was written by a newer scull",
            ))
        }
        Err(NotOpened::Probe(Probe::Scull { .. })) => Err(ScullError::new(
            ErrorKind::Config,
            "the history database needs a migration; run `scull scan` first",
        )),
        Err(NotOpened::Probe(_)) => Err(ScullError::new(
            ErrorKind::Config,
            "the history database is not a scull database; see `scull doctor`",
        )),
        Err(NotOpened::Io(e)) => Err(ScullError::from_io(&e)),
        Err(NotOpened::Store(e)) => Err(store_err(e)),
    }
}

fn store_err(e: StoreError) -> ScullError {
    let kind = e.kind();
    match e {
        StoreError::Corrupt { .. } => {
            ScullError::new(kind, "the history database is damaged; see `scull doctor`")
        }
        other => ScullError::new(kind, other.to_string()),
    }
}

/// The report over `[since, now]` from the database in scull's data directory (an empty
/// report when nothing was recorded yet).
pub fn collect(now: i64, since: Option<When>) -> Result<InsightsReport, ScullError> {
    let since = since.map_or(
        now - i64::from(scull_insights::DEFAULT_WINDOW_DAYS) * DAY,
        |w| w.resolve(now),
    );
    if since > now {
        return Err(ScullError::new(
            ErrorKind::Usage,
            "--since is in the future",
        ));
    }
    let budget = Some(DEFAULT_BUDGET_BYTES);
    match open_existing()? {
        Some(store) => scull_insights::report(&store, now, since, budget).map_err(store_err),
        None => Ok(scull_insights::compute(&InsightsInput {
            now,
            since,
            items: Vec::new(),
            regrowth: Vec::new(),
            operations: Vec::new(),
            volumes: Vec::new(),
            footprint: Footprint::default(),
            footprint_history: Vec::new(),
            budget_used_bytes: 0,
            budget_bytes: budget,
        })),
    }
}

// ---------------------------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------------------------

fn signed_bytes(v: i64) -> String {
    let s = format_bytes(v.unsigned_abs());
    if v < 0 {
        format!("-{s}")
    } else {
        format!("+{s}")
    }
}

fn percent(r: f64) -> String {
    format!("{:.0}%", r * 100.0)
}

fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 2 * DAY {
        format!("{} days", secs / DAY)
    } else if secs >= 3_600 {
        format!("{} h", secs / 3_600)
    } else {
        format!("{} min", secs / 60)
    }
}

/// The terminal rendering of a report. Every name from the database is escaped (SAFE-17).
pub fn render(r: &InsightsReport, color: bool) -> String {
    let e = |s: &str| escape_for_terminal(s).into_owned();
    let h = |s: &str| Style::Bold.paint(s, color);
    let dim = |s: &str| Style::Dim.paint(s, color);
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{}",
        h(&format!(
            "Insights \u{b7} last {} days (since {}) \u{b7} {} items tracked",
            r.window_days,
            iso_date(r.since),
            r.coverage.items
        ))
    );
    if r.coverage.items == 0 && r.free_space.is_empty() {
        let _ = writeln!(
            o,
            "No history recorded yet; run `scull scan <path>` to start."
        );
    }

    for g in &r.growth {
        let _ = writeln!(o, "\n{}", h(&format!("Top growers \u{b7} {} days", g.days)));
        if g.projects.is_empty() && g.categories.is_empty() {
            let _ = writeln!(o, "  nothing grew (net {})", signed_bytes(g.net_bytes));
            continue;
        }
        for p in &g.projects {
            let _ = writeln!(
                o,
                "  {:<24} {:>12}  {:<8} {} of growth",
                e(&p.label),
                signed_bytes(i64::try_from(p.growth_bytes).unwrap_or(i64::MAX)),
                e(p.ecosystem.as_deref().unwrap_or("")),
                percent(p.share_of_growth)
            );
        }
        let cats: Vec<String> = g
            .categories
            .iter()
            .map(|c| {
                format!(
                    "{} {}",
                    e(&c.key),
                    signed_bytes(i64::try_from(c.growth_bytes).unwrap_or(i64::MAX))
                )
            })
            .collect();
        let _ = writeln!(
            o,
            "  {}",
            dim(&format!(
                "by category: {} \u{b7} net {}",
                cats.join(" \u{b7} "),
                signed_bytes(g.net_bytes)
            ))
        );
    }

    let _ = writeln!(o, "\n{}", h("Regrowth"));
    if r.regrowth.is_empty() {
        let _ = writeln!(o, "  nothing came back after a cleanup");
    }
    for x in r.regrowth.iter().take(5) {
        let mut line = format!(
            "  {} came back {} times",
            e(x.path.as_str()),
            x.rebuilds + x.restores
        );
        if let Some(s) = x.median_back_within_secs {
            let _ = write!(line, " (within {} each, median)", duration(s));
        }
        let _ = writeln!(o, "{line}");
        if let Some(s) = &x.suggestion {
            let _ = writeln!(o, "    {}", dim(&e(s)));
        }
    }

    let _ = writeln!(o, "\n{}", h("Duplication"));
    if r.duplication.is_empty() {
        let _ = writeln!(
            o,
            "  no artifact kind in {}+ projects",
            scull_insights::DUPLICATION_MIN_PROJECTS
        );
    }
    for d in &r.duplication {
        let _ = writeln!(
            o,
            "  {} in {} projects \u{b7} {}",
            e(&d.artifact),
            d.projects,
            format_bytes(d.total_bytes)
        );
        if let Some(s) = &d.suggestion {
            let _ = writeln!(o, "    {}", dim(&e(s)));
        }
    }

    let a = &r.agent_footprint;
    let _ = writeln!(o, "\n{}", h("Agent footprint"));
    let agents: Vec<String> = a
        .by_agent
        .iter()
        .map(|x| format!("{} {}", e(&x.agent), x.worktrees))
        .collect();
    let mut line = format!(
        "  {} agent worktrees \u{b7} {} (of {} worktrees, {})",
        a.agent_worktrees,
        format_bytes(a.agent_bytes),
        a.worktrees,
        format_bytes(a.worktree_bytes)
    );
    if !agents.is_empty() {
        let _ = write!(line, " \u{b7} {}", agents.join(", "));
    }
    let _ = writeln!(o, "{line}");
    let weeks: Vec<String> = a
        .weeks
        .iter()
        .map(|w| w.agent_created.to_string())
        .collect();
    let mut line = format!("  created per week (oldest first): {}", weeks.join(" "));
    if let Some(l) = a.average_agent_lifetime_secs {
        let _ = write!(line, " \u{b7} average life {}", duration(l));
    }
    let _ = writeln!(o, "{line}");

    let acc = &r.accuracy;
    let _ = writeln!(o, "\n{}", h("Predicted vs measured"));
    if acc.operations.is_empty() {
        let _ = writeln!(o, "  no cleanups with measured results in this window");
    } else {
        let _ = writeln!(
            o,
            "  {} cleanups \u{b7} median error {} \u{b7} {} items rebuilt afterwards",
            acc.operations.len(),
            acc.median_abs_error
                .map_or_else(|| "n/a".to_owned(), percent),
            acc.regrown_items
        );
        for op in acc.operations.iter().rev().take(5) {
            let _ = writeln!(
                o,
                "  {}  {:>10} \u{2192} {:<10} {}",
                iso_date(op.at),
                format_bytes(op.predicted_bytes),
                format_bytes(op.measured_bytes),
                op.error
                    .map_or_else(String::new, |x| format!("{:+.1}%", x * 100.0))
            );
        }
    }

    if let Some(m) = r.ecosystem_mix.last().filter(|m| m.total_bytes > 0) {
        let mix: Vec<String> = m
            .ecosystems
            .iter()
            .map(|x| format!("{} {}", e(&x.ecosystem), percent(x.share)))
            .collect();
        let _ = writeln!(o, "\n{}", h(&format!("Ecosystem mix \u{b7} {}", m.month)));
        let _ = writeln!(o, "  {}", mix.join(" \u{b7} "));
    }

    if !r.free_space.is_empty() {
        let _ = writeln!(o, "\n{}", h("Free space"));
    }
    for v in &r.free_space {
        if let Some(last) = v.points.last() {
            let _ = writeln!(
                o,
                "  {}: {} free of {} ({} over {} days)",
                e(&v.volume_id),
                format_bytes(last.free_bytes),
                format_bytes(v.total_bytes),
                signed_bytes(v.change_bytes),
                r.window_days
            );
        }
        let fc = &v.forecast;
        let line = match fc.state {
            ForecastState::Filling => fc.days_rounded().map(|d| {
                format!(
                    "forecast: \u{2248} {d} days until less than {} free \u{b7} {}/day \u{b7} \
                     confidence {}",
                    format_bytes(fc.reserve_bytes),
                    signed_bytes(-(fc.fill_bytes_per_day.unwrap_or(0.0).round() as i64)),
                    fc.confidence.as_str()
                )
            }),
            ForecastState::BelowReserve => Some(format!(
                "forecast: already below {} free",
                format_bytes(fc.reserve_bytes)
            )),
            ForecastState::NotFilling => Some("forecast: not filling".to_owned()),
            ForecastState::NotEnoughHistory => Some("forecast: not enough history yet".to_owned()),
        };
        if let Some(line) = line {
            let _ = writeln!(o, "    {}", dim(&line));
        }
    }

    let f = &r.own_footprint;
    let _ = writeln!(o, "\n{}", h("What scull keeps"));
    let _ = writeln!(
        o,
        "  {} of history{} \u{b7} all on this machine",
        format_bytes(f.budget_used_bytes),
        f.budget_bytes
            .map_or_else(String::new, |b| format!(" (budget {})", format_bytes(b)))
    );

    let _ = writeln!(o, "\n{}", dim("Notes:"));
    for n in &r.notes {
        let _ = writeln!(o, "  {}", dim(&format!("- {}", e(n))));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_parses_durations_and_dates() {
        assert_eq!(parse_when("7d"), Ok(When::Ago(7 * DAY)));
        assert_eq!(parse_when("12h"), Ok(When::Ago(12 * 3_600)));
        assert_eq!(parse_when("2w"), Ok(When::Ago(14 * DAY)));
        assert_eq!(parse_when("90m"), Ok(When::Ago(90 * 60)));
        assert_eq!(parse_when("2026-10-01"), Ok(When::At(unix_of(2026, 10, 1))));
        for bad in [
            "",
            "d",
            "0d",
            "-3d",
            "7y",
            "2026-02-30",
            "2026-13-01",
            "26-10-01",
            "1e3d",
        ] {
            assert!(parse_when(bad).is_err(), "{bad:?}");
        }
        assert_eq!(When::Ago(10).resolve(100), 90);
        assert_eq!(When::At(5).resolve(100), 5);
    }

    #[test]
    fn empty_report_renders_a_hint() {
        let r = scull_insights::compute(&InsightsInput {
            now: 1_800_000_000,
            since: 1_800_000_000 - 30 * DAY,
            items: vec![],
            regrowth: vec![],
            operations: vec![],
            volumes: vec![],
            footprint: Footprint::default(),
            footprint_history: vec![],
            budget_used_bytes: 0,
            budget_bytes: None,
        });
        let text = render(&r, false);
        assert!(text.contains("No history recorded yet"), "{text}");
        assert!(!text.contains('\x1b'));
    }
}
