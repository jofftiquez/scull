//! Aligned human-readable tables (CLI-01).
//!
//! Every [`Cell`] constructor escapes its text for the terminal (SAFE-17), so nothing
//! attacker-controlled can reach the terminal unescaped through a table. Paths are never
//! truncated: the preview must show the exact path (SAFE-01).

use super::bytes::{format_bytes, size_header};
use super::escape::{escape_for_terminal, suspicion};
use super::style::{Style, tier_style, tier_text};
use scull_core::{SizeKind, Tier};
use unicode_width::UnicodeWidthStr;

/// Horizontal alignment of a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Pad on the right (text, paths).
    Left,
    /// Pad on the left (numbers, sizes).
    Right,
}

/// One table cell: terminal-safe text plus a style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    text: String,
    style: Style,
}

impl Cell {
    /// Arbitrary text, escaped for the terminal.
    pub fn text(s: &str) -> Self {
        Self {
            text: escape_for_terminal(s).into_owned(),
            style: Style::Plain,
        }
    }

    /// A path or file name: escaped, and flagged with `[! reason]` when suspicious (SAFE-17).
    pub fn path(p: &str) -> Self {
        let escaped = escape_for_terminal(p);
        match suspicion(p) {
            None => Self {
                text: escaped.into_owned(),
                style: Style::Plain,
            },
            Some(why) => Self {
                text: format!("{escaped} [! {}]", why.describe()),
                style: Style::Warn,
            },
        }
    }

    /// A byte count, formatted with binary units. The column header carries the size kind.
    pub fn bytes(n: u64) -> Self {
        Self {
            text: format_bytes(n),
            style: Style::Plain,
        }
    }

    /// A tier, as code plus label (`S0 regenerable`) so color is never the only cue.
    pub fn tier(t: Tier) -> Self {
        Self {
            text: tier_text(t),
            style: tier_style(t),
        }
    }

    /// Override the style.
    #[must_use]
    pub fn styled(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// The terminal-safe text, without styling.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    fn width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }
}

#[derive(Debug, Clone)]
struct Column {
    header: Cell,
    align: Align,
}

/// A table with a header row and aligned columns.
#[derive(Debug, Clone)]
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
}

impl Table {
    /// A table with the given column headers and alignments.
    pub fn new<'a>(columns: impl IntoIterator<Item = (&'a str, Align)>) -> Self {
        Self {
            columns: columns
                .into_iter()
                .map(|(h, align)| Column {
                    header: Cell::text(h),
                    align,
                })
                .collect(),
            rows: Vec::new(),
        }
    }

    /// Column spec for a size column: labeled header (PHY-01), right-aligned.
    pub fn size_column(kind: SizeKind) -> (&'static str, Align) {
        (size_header(kind), Align::Right)
    }

    /// Append a row. Missing cells render empty; extra cells are ignored.
    pub fn push(&mut self, row: Vec<Cell>) {
        self.rows.push(row);
    }

    /// Number of data rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the table has no data rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render with two-space gutters and a bold header. Lines end with `\n`; no trailing
    /// spaces.
    pub fn render(&self, color: bool) -> String {
        let empty = Cell::text("");
        let n = self.columns.len();
        let mut widths: Vec<usize> = self.columns.iter().map(|c| c.header.width()).collect();
        for row in &self.rows {
            for (i, w) in widths.iter_mut().enumerate() {
                *w = (*w).max(row.get(i).map_or(0, Cell::width));
            }
        }

        let mut out = String::new();
        let header: Vec<Cell> = self
            .columns
            .iter()
            .map(|c| c.header.clone().styled(Style::Bold))
            .collect();
        for row in std::iter::once(&header).chain(&self.rows) {
            let mut line = String::new();
            for (i, (col, width)) in self.columns.iter().zip(&widths).enumerate() {
                let cell = row.get(i).unwrap_or(&empty);
                let pad = width - cell.width();
                let last = i + 1 == n;
                if i > 0 {
                    line.push_str("  ");
                }
                let painted = cell.style.paint(&cell.text, color);
                match col.align {
                    Align::Right => {
                        line.push_str(&" ".repeat(pad));
                        line.push_str(&painted);
                    }
                    Align::Left => {
                        line.push_str(&painted);
                        if !last {
                            line.push_str(&" ".repeat(pad));
                        }
                    }
                }
            }
            out.push_str(line.trim_end_matches(' '));
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::escape::needs_escape;

    fn sample() -> Table {
        let mut t = Table::new([
            ("Tier", Align::Left),
            Table::size_column(SizeKind::Allocated),
            Table::size_column(SizeKind::Reclaimable),
            ("Path", Align::Left),
        ]);
        t.push(vec![
            Cell::tier(Tier::S0),
            Cell::bytes(2048),
            Cell::bytes(1024),
            Cell::path("/p/app/node_modules"),
        ]);
        t.push(vec![
            Cell::tier(Tier::S3),
            Cell::bytes(5),
            Cell::bytes(0),
            Cell::path("/p/evil\n\x1b[31mS0 regenerable  /x"),
        ]);
        t
    }

    #[test]
    fn renders_aligned_with_labels() {
        let out = sample().render(false);
        let want = "\
Tier            Allocated  Reclaimable  Path
S0 regenerable    2.0 KiB      1.0 KiB  /p/app/node_modules
S3 protected          5 B          0 B  /p/evil\\n\\x1b[31mS0 regenerable  /x [! contains hidden or control characters]
";
        assert_eq!(out, want);
    }

    #[test]
    fn hostile_names_cannot_add_rows_or_escapes() {
        let t = sample();
        let plain = t.render(false);
        assert_eq!(plain.lines().count(), 1 + t.len());
        assert!(!plain.chars().any(|c| c != '\n' && needs_escape(c)));
        // With color, the only ESC bytes are our own SGR sequences.
        let colored = t.render(true);
        let stripped = strip_sgr(&colored);
        assert_eq!(stripped, plain);
    }

    #[test]
    fn wide_characters_align_by_display_width() {
        let mut t = Table::new([("Name", Align::Left), ("N", Align::Right)]);
        t.push(vec![Cell::path("日本"), Cell::text("1")]);
        t.push(vec![Cell::path("abcd"), Cell::text("22")]);
        let out = t.render(false);
        assert_eq!(out, "Name   N\n日本   1\nabcd  22\n");
    }

    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                assert_eq!(chars.next(), Some('['));
                for d in chars.by_ref() {
                    if d == 'm' {
                        break;
                    }
                    assert!(d.is_ascii_digit() || d == ';');
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
