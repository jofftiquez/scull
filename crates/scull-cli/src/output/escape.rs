//! Terminal output safety (SAFE-17, threat T-51).
//!
//! File names are attacker-controlled: a directory can be called `\x1b[2J`, contain a newline
//! that fakes an extra table row, or use a right-to-left override so `txt.exe` reads as
//! `exe.txt`. Every path or name printed in human mode goes through [`escape_for_terminal`],
//! and [`is_suspicious_name`] decides when a name is additionally flagged.
//!
//! Machine output (`--json`, `--ndjson`) does not use this module: JSON string encoding already
//! escapes control characters, and consumers need the exact bytes.

use std::borrow::Cow;
use std::fmt::Write as _;

/// Whether `c` must never reach a terminal verbatim.
///
/// Covers:
/// * C0 controls U+0000–U+001F (including tab, newline, carriage return and ESC), DEL U+007F,
///   and C1 controls U+0080–U+009F (including NEL U+0085 and CSI U+009B);
/// * bidirectional formatting: LRM/RLM U+200E/U+200F, ALM U+061C, embeddings and overrides
///   U+202A–U+202E, isolates U+2066–U+2069;
/// * line and paragraph separators U+2028/U+2029 (some terminals break lines on them);
/// * invisible and zero-width characters: soft hyphen U+00AD, combining grapheme joiner
///   U+034F, Hangul fillers U+115F/U+1160/U+3164/U+FFA0, Mongolian vowel separator U+180E,
///   U+200B–U+200D (zero-width space, non-joiner, joiner), U+2060–U+2064 (word joiner and
///   invisible operators), U+FEFF (byte-order mark), interlinear annotations U+FFF9–U+FFFB,
///   and tag characters U+E0000–U+E007F.
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

/// Escape `s` for display on a terminal (SAFE-17).
///
/// Characters for which [`needs_escape`] is true are replaced by a visible ASCII escape:
/// `\t`, `\n`, `\r` for the common whitespace controls, `\xNN` for other C0 controls and DEL,
/// and `\u{NNNN}` for everything else. All other characters, including non-ASCII letters and
/// emoji, pass through unchanged. Returns `Cow::Borrowed` when nothing needed escaping, so the
/// common case does not allocate.
///
/// The output never contains a character for which [`needs_escape`] is true.
pub fn escape_for_terminal(s: &str) -> Cow<'_, str> {
    let Some(first) = s.find(needs_escape) else {
        return Cow::Borrowed(s);
    };
    let mut out = String::with_capacity(s.len() + 8);
    out.push_str(&s[..first]);
    for c in s[first..].chars() {
        if !needs_escape(c) {
            out.push(c);
            continue;
        }
        match c {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{0000}'..='\u{001F}' | '\u{007F}' => {
                // Writing to a String cannot fail.
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            _ => {
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
        }
    }
    Cow::Owned(out)
}

/// Why a name looks suspicious. See [`suspicion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suspicion {
    /// Contains a control, bidi, separator or invisible character (see [`needs_escape`]).
    HiddenCharacter,
    /// Contains U+FFFD: the original name was not valid Unicode and was converted lossily.
    NotUnicode,
    /// A path component starts or ends with whitespace, or ends with a dot (Windows strips
    /// trailing dots and spaces, so the name may not round-trip).
    EdgeWhitespaceOrDot,
    /// Contains a whitespace character other than ASCII space (no-break space, ideographic
    /// space, ...), which can make two different names look identical.
    UnusualWhitespace,
}

impl Suspicion {
    /// Short human explanation, used next to flagged names.
    pub fn describe(self) -> &'static str {
        match self {
            Suspicion::HiddenCharacter => "contains hidden or control characters",
            Suspicion::NotUnicode => "name is not valid Unicode",
            Suspicion::EdgeWhitespaceOrDot => "leading/trailing space or trailing dot",
            Suspicion::UnusualWhitespace => "contains unusual whitespace",
        }
    }
}

/// The first reason `name` (a file name or a whole path) looks suspicious, if any (SAFE-17).
///
/// Paths are checked component by component, split on `/` and `\`.
pub fn suspicion(name: &str) -> Option<Suspicion> {
    if name.chars().any(needs_escape) {
        return Some(Suspicion::HiddenCharacter);
    }
    if name.contains('\u{FFFD}') {
        return Some(Suspicion::NotUnicode);
    }
    let edge = name.split(['/', '\\']).any(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && (part.starts_with(char::is_whitespace)
                || part.ends_with(char::is_whitespace)
                || part.ends_with('.'))
    });
    if edge {
        return Some(Suspicion::EdgeWhitespaceOrDot);
    }
    if name.chars().any(|c| c.is_whitespace() && c != ' ') {
        return Some(Suspicion::UnusualWhitespace);
    }
    None
}

/// Whether `name` should be flagged in human output (SAFE-17). See [`suspicion`].
pub fn is_suspicious_name(name: &str) -> bool {
    suspicion(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn plain_text_is_borrowed_and_unchanged() {
        for s in [
            "",
            "node_modules",
            "/home/me/Projects/app/target",
            r"C:\Users\me\AppData\Local\npm-cache",
            "café/日本語/📦 builds",
            "with space",
        ] {
            let out = escape_for_terminal(s);
            assert!(matches!(out, Cow::Borrowed(_)), "{s:?} allocated");
            assert_eq!(out, s);
        }
    }

    #[test]
    fn ansi_escape_sequences_are_neutralized() {
        assert_eq!(escape_for_terminal("\x1b[2Jboom"), "\\x1b[2Jboom");
        assert_eq!(
            escape_for_terminal("\x1b]8;;http://evil\x07link\x1b]8;;\x07"),
            "\\x1b]8;;http://evil\\x07link\\x1b]8;;\\x07"
        );
        // 8-bit CSI (C1) is escaped too.
        assert_eq!(escape_for_terminal("a\u{9b}31mb"), "a\\u{009b}31mb");
    }

    #[test]
    fn newlines_cannot_fake_rows() {
        assert_eq!(
            escape_for_terminal("a\nS0 regenerable  /fake\r\tz"),
            "a\\nS0 regenerable  /fake\\r\\tz"
        );
        assert_eq!(escape_for_terminal("x\u{85}y"), "x\\u{0085}y");
        assert_eq!(
            escape_for_terminal("x\u{2028}y\u{2029}"),
            "x\\u{2028}y\\u{2029}"
        );
    }

    #[test]
    fn controls_and_del_use_hex_escapes() {
        assert_eq!(escape_for_terminal("\0"), "\\x00");
        assert_eq!(escape_for_terminal("\x7f"), "\\x7f");
        assert_eq!(escape_for_terminal("\x08\x08\x08"), "\\x08\\x08\\x08");
    }

    #[test]
    fn bidi_overrides_and_isolates_are_escaped() {
        // "invoice\u{202E}fdp.exe" displays as "invoiceexe.pdf".
        assert_eq!(
            escape_for_terminal("invoice\u{202E}fdp.exe"),
            "invoice\\u{202e}fdp.exe"
        );
        for c in [
            '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}', '\u{200E}', '\u{200F}', '\u{061C}',
        ] {
            let s = format!("a{c}b");
            let out = escape_for_terminal(&s);
            assert_eq!(out, format!("a\\u{{{:04x}}}b", c as u32));
            assert!(is_suspicious_name(&s));
        }
    }

    #[test]
    fn zero_width_and_invisible_characters_are_escaped() {
        for c in [
            '\u{200B}',
            '\u{200C}',
            '\u{200D}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{180E}',
            '\u{3164}',
            '\u{E0041}',
        ] {
            let s = format!("node{c}_modules");
            assert_ne!(
                escape_for_terminal(&s),
                s,
                "U+{:04X} passed through",
                c as u32
            );
            assert!(is_suspicious_name(&s));
        }
    }

    #[test]
    fn suspicious_names() {
        assert_eq!(suspicion("target"), None);
        assert_eq!(suspicion("/home/me/a b/c"), None);
        assert_eq!(suspicion("./x/../y"), None);
        assert_eq!(suspicion("日本語"), None);
        assert_eq!(suspicion("a\x1bb"), Some(Suspicion::HiddenCharacter));
        assert_eq!(suspicion("bad\u{FFFD}name"), Some(Suspicion::NotUnicode));
        assert_eq!(
            suspicion("/p/ node_modules"),
            Some(Suspicion::EdgeWhitespaceOrDot)
        );
        assert_eq!(
            suspicion(r"C:\p\target "),
            Some(Suspicion::EdgeWhitespaceOrDot)
        );
        assert_eq!(
            suspicion("/p/target."),
            Some(Suspicion::EdgeWhitespaceOrDot)
        );
        assert_eq!(
            suspicion("node\u{00A0}modules"),
            Some(Suspicion::UnusualWhitespace)
        );
        assert_eq!(
            suspicion("node\u{3000}modules"),
            Some(Suspicion::UnusualWhitespace)
        );
        for s in Suspicion::HiddenCharacter.describe().chars() {
            assert!(!needs_escape(s));
        }
    }

    /// Characters biased towards the dangerous ranges, mixed with ordinary text.
    fn nasty_char() -> impl Strategy<Value = char> {
        prop_oneof![
            any::<char>(),
            (0u32..0x20).prop_map(|c| char::from_u32(c).unwrap_or('?')),
            (0x7Fu32..0xA0).prop_map(|c| char::from_u32(c).unwrap_or('?')),
            (0x2000u32..0x2070).prop_map(|c| char::from_u32(c).unwrap_or('?')),
            (0xE0000u32..0xE0080).prop_map(|c| char::from_u32(c).unwrap_or('?')),
            Just('\u{061C}'),
            Just('\u{FEFF}'),
            Just('\\'),
            "[a-z/.]".prop_map(|s| s.chars().next().unwrap_or('a')),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        /// SAFE-17: escaped output never contains a control, bidi or invisible character.
        #[test]
        fn escaped_output_is_terminal_safe(chars in proptest::collection::vec(nasty_char(), 0..64)) {
            let s: String = chars.into_iter().collect();
            let out = escape_for_terminal(&s);
            prop_assert!(!out.chars().any(needs_escape), "unsafe char survived in {:?}", out);
            prop_assert!(!out.chars().any(char::is_control));
            // Output changes exactly when something needed escaping, and such names are flagged.
            let had_unsafe = s.chars().any(needs_escape);
            prop_assert_eq!(out != s, had_unsafe);
            if had_unsafe {
                prop_assert!(is_suspicious_name(&s));
            }
            // Escaping is idempotent on already-safe text.
            prop_assert_eq!(escape_for_terminal(&out), out.clone());
        }

        /// Safe characters are preserved in order.
        #[test]
        fn safe_characters_are_preserved(s in "\\PC*") {
            let out = escape_for_terminal(&s);
            let kept: String = s.chars().filter(|c| !needs_escape(*c)).collect();
            let out_kept: String = out.chars().filter(|c| !needs_escape(*c)).collect();
            if !s.chars().any(needs_escape) {
                prop_assert_eq!(out_kept, kept);
            } else {
                // Every safe character still appears, in order, as a subsequence.
                let mut it = out.chars();
                for c in kept.chars() {
                    prop_assert!(it.any(|o| o == c), "lost {:?}", c);
                }
            }
        }
    }
}
