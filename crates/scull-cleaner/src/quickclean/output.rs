//! Tool output safety (SAFE-17): captured stdout/stderr is split into lines and every line is
//! escaped before anything shows it.
//!
//! The character set is the one of `scull_cli::output::escape::needs_escape` (copied: this
//! crate must not depend on the CLI, which will depend on it). Keep the two in sync.

use std::fmt::Write as _;

/// Captured output of one stream: escaped lines, capped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedOutput {
    /// The output split at line breaks, each line escaped (no control, bidirectional or
    /// invisible characters remain; they appear as `\xNN` / `\u{NNNN}`).
    pub lines: Vec<String>,
    /// Whether the stream produced more than the cap; the rest was read and discarded.
    pub truncated: bool,
}

impl CapturedOutput {
    pub(crate) fn from_bytes(bytes: &[u8], truncated: bool) -> Self {
        let text = String::from_utf8_lossy(bytes);
        let mut lines: Vec<String> = text
            .split('\n')
            .map(|l| escape_line(l.strip_suffix('\r').unwrap_or(l)))
            .collect();
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        Self { lines, truncated }
    }

    /// Whether nothing was captured.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Whether `c` must never reach a terminal verbatim (same set as the CLI's `needs_escape`).
pub fn needs_escape(c: char) -> bool {
    matches!(
        c,
        '\u{0000}'..='\u{001F}'
            | '\u{007F}'..='\u{009F}'
            | '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'
            | '\u{1160}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{3164}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// One line with every [`needs_escape`] character replaced by a visible escape.
pub fn escape_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !needs_escape(c) {
            out.push(c);
            continue;
        }
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{0000}'..='\u{001F}' | '\u{007F}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            _ => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_split_and_escaped() {
        let out = CapturedOutput::from_bytes(b"ok\r\n\x1b[2Jcleared\n\xe2\x80\xaeevil\tx\n", false);
        assert_eq!(out.lines, ["ok", "\\x1b[2Jcleared", "\\u{202e}evil\\tx"]);
        assert!(out.lines.iter().all(|l| !l.chars().any(needs_escape)));
    }

    #[test]
    fn invalid_utf8_is_replaced_not_dropped() {
        let out = CapturedOutput::from_bytes(b"a\xffb", true);
        assert_eq!(out.lines, ["a\u{FFFD}b"]);
        assert!(out.truncated);
    }
}
