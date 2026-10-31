//! Object ids and ref listings returned by [`GitRunner`](crate::GitRunner).

use std::fmt;

/// A full Git object id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
///
/// Validated on construction, so it is always safe to pass as a `git` argument (it can never
/// look like an option).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(String);

impl ObjectId {
    /// Parses a full hex object id. Uppercase hex is accepted and normalized to lowercase.
    pub fn parse(s: &str) -> Option<Self> {
        let ok = matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
        ok.then(|| Self(s.to_ascii_lowercase()))
    }

    /// The hex form.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the all-zero id Git uses for "no object".
    pub fn is_null(&self) -> bool {
        self.0.bytes().all(|b| b == b'0')
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One ref, as listed by `git for-each-ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// Full ref name, e.g. `refs/heads/main` or `refs/remotes/origin/main`.
    pub name: String,
    /// The object the ref points at (for annotated tags, the tag object).
    pub oid: ObjectId,
    /// Type of that object: `commit`, `tag`, `tree` or `blob`.
    pub object_type: String,
    /// Configured upstream (full ref name) for local branches, e.g. `refs/remotes/origin/main`.
    /// Read from `branch.<name>.remote`/`merge`; the upstream ref itself may not exist.
    pub upstream: Option<String>,
    /// Target of a symbolic ref (e.g. `refs/remotes/origin/HEAD`).
    pub symref: Option<String>,
}

impl RefEntry {
    /// The branch name for `refs/heads/<name>` refs.
    pub fn branch_name(&self) -> Option<&str> {
        self.name.strip_prefix("refs/heads/")
    }

    /// Whether this is a remote-tracking ref (`refs/remotes/...`).
    pub fn is_remote_tracking(&self) -> bool {
        self.name.starts_with("refs/remotes/")
    }
}

/// `--format` for [`parse_refs`]: NUL-separated fields, one ref per line (ref names can contain
/// neither NUL nor newline).
pub(crate) const REF_FORMAT: &str =
    "--format=%(refname)%00%(objectname)%00%(objecttype)%00%(upstream)%00%(symref)";

/// Parses `git for-each-ref` output produced with [`REF_FORMAT`].
pub(crate) fn parse_refs(out: &[u8]) -> Result<Vec<RefEntry>, String> {
    let mut refs = Vec::new();
    for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let line = String::from_utf8_lossy(line);
        let f: Vec<&str> = line.split('\0').collect();
        let [name, oid, object_type, upstream, symref] = f[..] else {
            return Err(format!("unexpected for-each-ref line: {line:?}"));
        };
        let oid = ObjectId::parse(oid).ok_or_else(|| format!("bad object id in {line:?}"))?;
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        refs.push(RefEntry {
            name: name.to_owned(),
            oid,
            object_type: object_type.to_owned(),
            upstream: opt(upstream),
            symref: opt(symref),
        });
    }
    Ok(refs)
}

/// Parses one object id per line (`rev-list`, `merge-base`, `rev-parse`).
pub(crate) fn parse_oid_lines(out: &[u8]) -> Result<Vec<ObjectId>, String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| ObjectId::parse(l.trim()).ok_or_else(|| format!("bad object id: {l:?}")))
        .collect()
}

/// One line of `git cherry` output (see [`GitRunner::cherry`](crate::GitRunner::cherry)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CherryCommit {
    /// The commit on the head side.
    pub oid: ObjectId,
    /// Whether the upstream side has a patch-equivalent commit (`-` in `git cherry` output).
    pub equivalent: bool,
}

/// Parses `git cherry` output: `+ <oid>` or `- <oid>` per line.
pub(crate) fn parse_cherry(out: &[u8]) -> Result<Vec<CherryCommit>, String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let bad = || format!("bad cherry line: {l:?}");
            let (sign, oid) = l.split_once(' ').ok_or_else(bad)?;
            let equivalent = match sign {
                "-" => true,
                "+" => false,
                _ => return Err(bad()),
            };
            let oid = ObjectId::parse(oid.trim()).ok_or_else(bad)?;
            Ok(CherryCommit { oid, equivalent })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn object_ids_are_validated() {
        assert!(ObjectId::parse(A).is_some());
        assert_eq!(
            ObjectId::parse(&A.to_uppercase()).map(|o| o.0),
            Some(A.to_owned())
        );
        assert!(ObjectId::parse(&"a".repeat(64)).is_some());
        assert!(ObjectId::parse("-0123456789abcdef0123456789abcdef0123456").is_none());
        assert!(ObjectId::parse("HEAD").is_none());
        assert!(ObjectId::parse(&"g".repeat(40)).is_none());
        assert!(ObjectId::parse(&"0".repeat(40)).is_some_and(|o| o.is_null()));
    }

    #[test]
    fn refs_parse() {
        let out = format!(
            "refs/heads/main\0{A}\0commit\0refs/remotes/origin/main\0\n\
             refs/remotes/origin/HEAD\0{A}\0commit\0\0refs/remotes/origin/main\n"
        );
        let refs = parse_refs(out.as_bytes()).expect("parse");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].branch_name(), Some("main"));
        assert_eq!(
            refs[0].upstream.as_deref(),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(refs[0].symref, None);
        assert!(refs[1].is_remote_tracking());
        assert_eq!(refs[1].symref.as_deref(), Some("refs/remotes/origin/main"));
        assert!(parse_refs(b"refs/heads/x\0nothex\0commit\0\0\n").is_err());
        assert!(parse_refs(b"just one field\n").is_err());
    }

    #[test]
    fn cherry_parses() {
        let out = format!("+ {A}\n- {A}\n");
        let c = parse_cherry(out.as_bytes()).expect("parse");
        assert_eq!(c.len(), 2);
        assert!(!c[0].equivalent && c[1].equivalent);
        assert!(parse_cherry(b"? abc\n").is_err());
        assert!(parse_cherry(b"+ nothex\n").is_err());
        assert!(parse_cherry(b"+\n").is_err());
    }

    #[test]
    fn oid_lines_parse() {
        let out = format!("{A}\n{A}\n");
        assert_eq!(parse_oid_lines(out.as_bytes()).expect("parse").len(), 2);
        assert!(parse_oid_lines(b"zzz\n").is_err());
    }
}
