//! Human rendering shared by the read-only commands: candidate tables, totals by tier,
//! unavailable signals and issues. Every path and message is escaped (SAFE-17) and every
//! size is labeled (PHY-01).

use crate::output::bytes::{confidence_label, format_bytes};
use crate::output::escape::escape_for_terminal;
use crate::output::style::{Style, tier_text};
use crate::output::table::{Align, Cell, Table};
use scull_core::{Action, Candidate, FsPath, SizeKind, Tier};
use scull_engine::{Issue, SignalUnavailable, Totals};
use std::path::Path;

/// The reason that decided a candidate's tier (the first reason at its tier).
pub fn deciding_reason(c: &Candidate) -> &str {
    c.reasons
        .iter()
        .find(|r| r.tier == c.tier)
        .or_else(|| c.reasons.first())
        .map_or("", |r| r.message.as_str())
}

/// Short label of an action.
pub fn action_label(a: &Action) -> String {
    match a {
        Action::None => "none".into(),
        Action::Delete => "delete".into(),
        Action::Quarantine => "quarantine".into(),
        Action::Trash => "trash".into(),
        Action::ToolCommand { tool, args } => format!("{tool} {}", args.join(" ")),
        Action::GitWorktreePrune => "git worktree prune".into(),
        Action::GitWorktreeRemove => "git worktree remove".into(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// How a candidate's path is shown: relative to `base` when it lies inside it (the header
/// names the root), otherwise in full. Machine output always carries full paths.
pub fn display_path(path: &FsPath, base: Option<&Path>) -> String {
    let full = path.to_path_buf();
    match base.and_then(|b| full.strip_prefix(b).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        _ => path.as_str().to_owned(),
    }
}

/// The candidate table: tier, sizes (labeled), confidence, artifact, path, why. Paths inside
/// `base` are shown relative to it.
pub fn candidates_table(cands: &[Candidate], base: Option<&Path>, color: bool) -> String {
    let mut t = Table::new([
        ("Tier", Align::Left),
        Table::size_column(SizeKind::Reclaimable),
        Table::size_column(SizeKind::Allocated),
        Table::size_column(SizeKind::Logical),
        ("Confidence", Align::Left),
        ("Artifact", Align::Left),
        ("Path", Align::Left),
        ("Why", Align::Left),
    ]);
    for c in cands {
        t.push(vec![
            Cell::tier(c.tier),
            Cell::bytes(c.sizes.reclaimable.bytes),
            Cell::bytes(c.sizes.allocated),
            Cell::bytes(c.sizes.logical),
            Cell::text(confidence_label(c.sizes.reclaimable.confidence)),
            Cell::text(&c.artifact),
            Cell::path(&display_path(&c.path, base)),
            Cell::text(deciding_reason(c)),
        ]);
    }
    t.render(color)
}

/// Totals per tier (items and labeled sizes), plus a total row.
pub fn totals_table(t: &Totals, color: bool) -> String {
    let mut table = Table::new([
        ("Tier", Align::Left),
        ("Items", Align::Right),
        Table::size_column(SizeKind::Reclaimable),
        Table::size_column(SizeKind::Allocated),
        Table::size_column(SizeKind::Logical),
    ]);
    for tier in Tier::ALL {
        table.push(vec![
            Cell::tier(tier),
            Cell::text(&t.count.get(tier).to_string()),
            Cell::bytes(t.reclaimable.get(tier)),
            Cell::bytes(t.allocated.get(tier)),
            Cell::bytes(t.logical.get(tier)),
        ]);
    }
    table.push(vec![
        Cell::text("total").styled(Style::Bold),
        Cell::text(&t.count.total().to_string()),
        Cell::bytes(t.reclaimable.total()),
        Cell::bytes(t.allocated.total()),
        Cell::bytes(t.logical.total()),
    ]);
    table.render(color)
}

/// One line saying what a cleanup could free at most, and that nothing was changed.
pub fn reclaim_line(t: &Totals) -> String {
    format!(
        "Up to {} reclaimable in S0-S2 ({} preselectable as {}). Nothing was changed: scan is read-only.",
        format_bytes(t.actionable_reclaimable()),
        format_bytes(t.reclaimable.s0),
        tier_text(Tier::S0)
    )
}

/// The "signals unavailable" section (PRD §8.4: the preview says so), or nothing.
pub fn signals_section(signals: &[SignalUnavailable], color: bool) -> String {
    if signals.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\n{}\n",
        Style::Warn.paint(
            "Signals unavailable (affected items are classified as if the answer were risky):",
            color
        )
    );
    for s in signals {
        out.push_str(&format!(
            "  {} ({} item(s)): {}\n",
            escape_for_terminal(&s.signal),
            s.affected,
            escape_for_terminal(&s.reason)
        ));
    }
    out
}

/// The issues section (unreadable or skipped places), or nothing.
pub fn issues_section(issues: &[Issue], truncated: u64, color: bool) -> String {
    if issues.is_empty() {
        return String::new();
    }
    let mut out = format!("\n{}\n", Style::Bold.paint("Issues:", color));
    for i in issues {
        out.push_str(&format!(
            "  {}: {}\n",
            escape_for_terminal(i.path.as_str()),
            escape_for_terminal(&i.message)
        ));
    }
    if truncated > 0 {
        out.push_str(&format!("  ... and {truncated} more\n"));
    }
    out
}
