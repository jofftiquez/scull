//! Minimal ANSI styling and tier rendering.
//!
//! Color is never the only signal (UX-01): a tier always renders as its code plus its label,
//! e.g. `S0 regenerable`, and color only reinforces it.

use scull_core::Tier;

/// A text style. Rendered as SGR escape codes when color is enabled, as nothing otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Style {
    /// No styling.
    #[default]
    Plain,
    /// Bold.
    Bold,
    /// Dimmed.
    Dim,
    /// Bold red, for errors.
    Error,
    /// Yellow, for warnings and flagged names.
    Warn,
    /// Bold magenta: S0 regenerable.
    TierS0,
    /// Magenta: S1 costly.
    TierS1,
    /// Yellow: S2 uncertain.
    TierS2,
    /// Bold blue: S3 protected.
    TierS3,
}

impl Style {
    fn sgr(self) -> Option<&'static str> {
        match self {
            Style::Plain => None,
            Style::Bold => Some("1"),
            Style::Dim => Some("2"),
            Style::Error => Some("1;31"),
            Style::Warn | Style::TierS2 => Some("33"),
            Style::TierS0 => Some("1;35"),
            Style::TierS1 => Some("35"),
            Style::TierS3 => Some("1;34"),
        }
    }

    /// Wrap `text` in this style if `color` is true. `text` must already be terminal-safe
    /// (see [`super::escape::escape_for_terminal`]).
    pub fn paint(self, text: &str, color: bool) -> String {
        match (color, self.sgr()) {
            (true, Some(code)) => format!("\x1b[{code}m{text}\x1b[0m"),
            _ => text.to_owned(),
        }
    }
}

/// The text shown for a tier: code plus label, e.g. `S2 uncertain`.
pub fn tier_text(tier: Tier) -> String {
    format!("{tier} {}", tier.label())
}

/// The style used for a tier.
pub fn tier_style(tier: Tier) -> Style {
    match tier {
        Tier::S0 => Style::TierS0,
        Tier::S1 => Style::TierS1,
        Tier::S2 => Style::TierS2,
        Tier::S3 => Style::TierS3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_text_carries_label_without_color() {
        let texts: Vec<String> = Tier::ALL.into_iter().map(tier_text).collect();
        assert_eq!(
            texts,
            [
                "S0 regenerable",
                "S1 costly",
                "S2 uncertain",
                "S3 protected"
            ]
        );
    }

    #[test]
    fn paint_respects_color_flag() {
        assert_eq!(Style::Error.paint("x", false), "x");
        assert_eq!(Style::Error.paint("x", true), "\x1b[1;31mx\x1b[0m");
        assert_eq!(Style::Plain.paint("x", true), "x");
        for t in Tier::ALL {
            let painted = tier_style(t).paint(&tier_text(t), true);
            assert!(painted.contains(&tier_text(t)));
        }
    }
}
