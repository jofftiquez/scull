//! CSV export of an [`InsightsReport`] (`scull insights export --format csv`, DATA-07).
//!
//! One tidy table, so every figure lands in a spreadsheet or dataframe the same way:
//!
//! ```text
//! section,period,subject,metric,value
//! growth_category,30d,project_artifact,growth_bytes,8053063680
//! free_space,2026-10-30,/,free_bytes,44236839936
//! ```
//!
//! * `section`: which part of the report (`growth_category`, `growth_project`, `regrowth`,
//!   `duplication`, `agent_footprint`, `agent_weekly`, `accuracy`, `ecosystem_mix`,
//!   `free_space`, `own_footprint`).
//! * `period`: `<n>d` for a window, `YYYY-MM-DD` for a day or week start, `YYYY-MM` for a month,
//!   empty when the row describes now.
//! * `subject`: category key, project root, path, `<ecosystem>/<artifact>`, agent, operation ID,
//!   ecosystem or volume.
//! * `value`: a number (integers for bytes, counts and seconds; ratios as decimals).
//!
//! Fields are quoted per RFC 4180 when needed. A text field that starts with `=`, `+`, `-`,
//! `@`, tab or carriage return is prefixed with `'` so a spreadsheet never evaluates a crafted
//! folder name as a formula; numeric values are written as they are.

use crate::InsightsReport;
use crate::time::{iso_date, iso_month};

/// Header row of [`to_csv`].
pub const HEADER: &str = "section,period,subject,metric,value";

/// The report as one CSV table (header plus rows, `\n` line ends).
pub fn to_csv(r: &InsightsReport) -> String {
    let mut w = Writer::default();
    for g in &r.growth {
        let period = format!("{}d", g.days);
        w.row("growth_total", &period, "all", "net_bytes", g.net_bytes);
        w.row("growth_total", &period, "all", "grown_bytes", g.grown_bytes);
        for (section, list) in [
            ("growth_category", &g.categories),
            ("growth_project", &g.projects),
        ] {
            for x in list {
                w.row(section, &period, &x.key, "start_bytes", x.start_bytes);
                w.row(section, &period, &x.key, "end_bytes", x.end_bytes);
                w.row(section, &period, &x.key, "growth_bytes", x.growth_bytes);
                w.row(
                    section,
                    &period,
                    &x.key,
                    "share_of_growth",
                    x.share_of_growth,
                );
            }
        }
    }
    let window = format!("{}d", r.window_days);
    for x in &r.regrowth {
        let p = x.path.as_str();
        w.row("regrowth", &window, p, "rebuilds", x.rebuilds);
        w.row("regrowth", &window, p, "restores", x.restores);
        if let Some(s) = x.median_back_within_secs {
            w.row("regrowth", &window, p, "median_back_within_secs", s);
        }
        w.row("regrowth", &window, p, "last_back_at", x.last_back_at);
    }
    for x in &r.duplication {
        let subject = format!("{}/{}", x.ecosystem, x.artifact);
        w.row("duplication", "", &subject, "projects", x.projects);
        w.row("duplication", "", &subject, "copies", x.copies);
        w.row("duplication", "", &subject, "total_bytes", x.total_bytes);
        w.row(
            "duplication",
            "",
            &subject,
            "largest_bytes",
            x.largest_bytes,
        );
    }
    let a = &r.agent_footprint;
    w.row("agent_footprint", "", "all", "worktrees", a.worktrees);
    w.row(
        "agent_footprint",
        "",
        "all",
        "worktree_bytes",
        a.worktree_bytes,
    );
    w.row(
        "agent_footprint",
        "",
        "agents",
        "worktrees",
        a.agent_worktrees,
    );
    w.row("agent_footprint", "", "agents", "bytes", a.agent_bytes);
    if let Some(s) = a.average_agent_lifetime_secs {
        w.row("agent_footprint", "", "agents", "average_lifetime_secs", s);
    }
    for x in &a.by_agent {
        w.row("agent_footprint", "", &x.agent, "worktrees", x.worktrees);
        w.row("agent_footprint", "", &x.agent, "bytes", x.bytes);
    }
    for x in &a.weeks {
        let week = iso_date(x.week_start);
        w.row("agent_weekly", &week, "all", "created", x.created);
        w.row("agent_weekly", &week, "agents", "created", x.agent_created);
    }
    for x in &r.accuracy.operations {
        let day = iso_date(x.at);
        let id = x.operation_id.as_str();
        w.row("accuracy", &day, id, "predicted_bytes", x.predicted_bytes);
        w.row("accuracy", &day, id, "measured_bytes", x.measured_bytes);
        if let Some(e) = x.error {
            w.row("accuracy", &day, id, "error", e);
        }
        w.row("accuracy", &day, id, "regrown_items", x.regrown_items);
    }
    if let Some(m) = r.accuracy.median_abs_error {
        w.row("accuracy", &window, "all", "median_abs_error", m);
    }
    for m in &r.ecosystem_mix {
        let month = iso_month(m.at);
        for e in &m.ecosystems {
            w.row("ecosystem_mix", &month, &e.ecosystem, "bytes", e.bytes);
            w.row("ecosystem_mix", &month, &e.ecosystem, "share", e.share);
        }
    }
    for v in &r.free_space {
        for p in &v.points {
            w.row(
                "free_space",
                &iso_date(p.at),
                &v.volume_id,
                "free_bytes",
                p.free_bytes,
            );
        }
    }
    let f = &r.own_footprint;
    w.row("own_footprint", "", "scull", "total_bytes", f.total_bytes);
    w.row(
        "own_footprint",
        "",
        "scull",
        "budget_used_bytes",
        f.budget_used_bytes,
    );
    if let Some(b) = f.budget_bytes {
        w.row("own_footprint", "", "scull", "budget_bytes", b);
    }
    for p in &f.points {
        w.row(
            "own_footprint",
            &iso_date(p.at),
            "scull",
            "total_bytes",
            p.total_bytes,
        );
    }
    w.out
}

struct Writer {
    out: String,
}

impl Default for Writer {
    fn default() -> Self {
        Self {
            out: format!("{HEADER}\n"),
        }
    }
}

impl Writer {
    fn row(
        &mut self,
        section: &str,
        period: &str,
        subject: &str,
        metric: &str,
        value: impl ToString,
    ) {
        let fields = [
            field(section),
            field(period),
            field(subject),
            field(metric),
            value.to_string(),
        ];
        self.out.push_str(&fields.join(","));
        self.out.push('\n');
    }
}

/// One text field: formula-neutralized, then quoted if it holds a comma, quote or line break.
pub fn field(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{s}")
    } else {
        s.to_owned()
    };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_quoted_and_formula_safe() {
        assert_eq!(field("plain"), "plain");
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(field("=HYPERLINK(1)"), "'=HYPERLINK(1)");
        assert_eq!(field("-1+1"), "'-1+1");
        assert_eq!(field("@x,y"), "\"'@x,y\"");
    }
}
