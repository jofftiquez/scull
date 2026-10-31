//! Single-name glob patterns used by rules (`target`, `*.uproject`, `next.config.{js,mjs}`).
//!
//! The syntax is deliberately small so it can be validated strictly and matched without I/O:
//!
//! * `*` matches any run of characters (including none) within one name;
//! * `?` matches exactly one character;
//! * `{a,b,c}` matches any one of the comma-separated literal alternatives (no nesting, no
//!   wildcards inside braces, at most [`MAX_EXPANSIONS`] expansions in total).
//!
//! A pattern always matches a **single path component**: `/`, `\`, `**`, `[`/`]` (reserved),
//! NUL, `.` and `..` are rejected.

use std::fmt;

/// Longest accepted pattern, in bytes (a generous file-name limit).
pub const MAX_PATTERN_LEN: usize = 255;
/// Most alternatives a pattern may expand to.
pub const MAX_EXPANSIONS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Lit(char),
    Star,
    Question,
}

/// A validated single-name glob pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamePattern {
    raw: String,
    /// Brace alternatives expanded; each is a token sequence.
    expansions: Vec<Vec<Token>>,
}

/// Why a pattern was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternError(pub String);

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl NamePattern {
    /// Parse and validate a pattern.
    pub fn parse(raw: &str) -> Result<Self, PatternError> {
        let err = |m: &str| Err(PatternError(m.to_string()));
        if raw.is_empty() {
            return err("pattern is empty");
        }
        if raw.len() > MAX_PATTERN_LEN {
            return err("pattern is longer than 255 bytes");
        }
        if raw == "." || raw == ".." {
            return err("`.` and `..` are not names");
        }
        if raw.contains('/') || raw.contains('\\') {
            return err("pattern must be a single name (no `/` or `\\`)");
        }
        if raw.contains("**") {
            return err("`**` is not supported; patterns match one name");
        }
        if raw.contains('[') || raw.contains(']') {
            return err("`[` and `]` are reserved");
        }
        if raw.chars().any(char::is_control) {
            return err("pattern contains a control character");
        }

        // Split into segments: literal/wildcard text and brace groups.
        let mut expansions: Vec<Vec<Token>> = vec![Vec::new()];
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    let mut group = String::new();
                    let mut closed = false;
                    for g in chars.by_ref() {
                        match g {
                            '}' => {
                                closed = true;
                                break;
                            }
                            '{' => return err("nested `{` is not supported"),
                            '*' | '?' => {
                                return err("wildcards inside `{...}` are not supported");
                            }
                            other => group.push(other),
                        }
                    }
                    if !closed {
                        return err("unclosed `{`");
                    }
                    let alts: Vec<&str> = group.split(',').collect();
                    if alts.len() < 2 {
                        return err("`{...}` needs at least two alternatives");
                    }
                    if alts.iter().any(|a| a.is_empty()) {
                        return err("empty alternative in `{...}`");
                    }
                    if expansions.len() * alts.len() > MAX_EXPANSIONS {
                        return err("pattern expands to too many alternatives");
                    }
                    let mut next = Vec::with_capacity(expansions.len() * alts.len());
                    for prefix in &expansions {
                        for alt in &alts {
                            let mut e = prefix.clone();
                            e.extend(alt.chars().map(Token::Lit));
                            next.push(e);
                        }
                    }
                    expansions = next;
                }
                '}' => return err("unmatched `}`"),
                '*' => expansions.iter_mut().for_each(|e| e.push(Token::Star)),
                '?' => expansions.iter_mut().for_each(|e| e.push(Token::Question)),
                other => expansions
                    .iter_mut()
                    .for_each(|e| e.push(Token::Lit(other))),
            }
        }
        Ok(Self {
            raw: raw.to_string(),
            expansions,
        })
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether the pattern contains `*` or `?` (brace groups alone are still literal).
    pub fn has_wildcards(&self) -> bool {
        self.expansions
            .iter()
            .flatten()
            .any(|t| matches!(t, Token::Star | Token::Question))
    }

    /// Whether some expansion consists only of `*`/`?` (matches nearly anything).
    pub fn is_wildcard_only(&self) -> bool {
        self.expansions
            .iter()
            .any(|e| e.iter().all(|t| matches!(t, Token::Star | Token::Question)))
    }

    /// The literal names this pattern stands for, if it has no wildcards.
    pub fn literals(&self) -> Option<Vec<String>> {
        if self.has_wildcards() {
            return None;
        }
        Some(
            self.expansions
                .iter()
                .map(|e| {
                    e.iter()
                        .map(|t| match t {
                            Token::Lit(c) => *c,
                            // Unreachable: no wildcards.
                            Token::Star | Token::Question => '*',
                        })
                        .collect()
                })
                .collect(),
        )
    }

    /// Case-sensitive match against a single name.
    pub fn matches(&self, name: &str) -> bool {
        let n: Vec<char> = name.chars().collect();
        self.expansions.iter().any(|e| match_tokens(e, &n))
    }

    /// Case-insensitive match (Unicode lowercase on both sides).
    pub fn matches_ignore_case(&self, name: &str) -> bool {
        let n: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
        self.expansions.iter().any(|e| {
            let folded: Vec<Token> = e
                .iter()
                .flat_map(|t| match t {
                    Token::Lit(c) => c.to_lowercase().map(Token::Lit).collect::<Vec<_>>(),
                    other => vec![other.clone()],
                })
                .collect();
            match_tokens(&folded, &n)
        })
    }
}

impl fmt::Display for NamePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Iterative wildcard matcher with single-star backtracking (linear-ish, no recursion).
fn match_tokens(pat: &[Token], name: &[char]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pat.get(p) {
            Some(Token::Star) => {
                star = Some((p, n));
                p += 1;
            }
            Some(Token::Question) => {
                p += 1;
                n += 1;
            }
            Some(Token::Lit(c)) if *c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pat[p..].iter().all(|t| matches!(t, Token::Star))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> NamePattern {
        NamePattern::parse(s).unwrap()
    }

    #[test]
    fn literals_and_wildcards() {
        assert!(p("target").matches("target"));
        assert!(!p("target").matches("Target"));
        assert!(p("target").matches_ignore_case("TARGET"));
        assert!(p("*.uproject").matches("Game.uproject"));
        assert!(!p("*.uproject").matches("Game.uproject.bak"));
        assert!(p("a?c").matches("abc") && !p("a?c").matches("ac"));
        assert!(p("*").matches("") && p("*").matches("anything"));
        assert!(p("a*b*c").matches("aXXbYYc") && !p("a*b*c").matches("aXXbYY"));
    }

    #[test]
    fn braces_expand() {
        let pat = p("next.config.{js,mjs,ts}");
        assert!(pat.matches("next.config.mjs"));
        assert!(!pat.matches("next.config.json"));
        assert_eq!(
            pat.literals().unwrap(),
            vec!["next.config.js", "next.config.mjs", "next.config.ts"]
        );
        assert!(!pat.has_wildcards());
    }

    #[test]
    fn wildcard_only_detection() {
        assert!(p("*").is_wildcard_only());
        assert!(p("?*").is_wildcard_only());
        assert!(!p("*.toml").is_wildcard_only());
    }

    #[test]
    fn bad_patterns_are_rejected() {
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "**",
            "a**b",
            "[ab]",
            "{a",
            "a}",
            "{a}",
            "{a,}",
            "{a,{b,c}}",
            "{*,b}",
            "a\u{0}b",
        ] {
            assert!(
                NamePattern::parse(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(NamePattern::parse(&"x".repeat(256)).is_err());
        assert!(NamePattern::parse("{a,b}{c,d}{e,f}{g,h}{i,j}{k,l}").is_err());
    }
}
