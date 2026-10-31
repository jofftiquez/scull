//! `git status --porcelain=v2 -z` options, result types and parser.

use crate::refs::ObjectId;
use std::path::{Path, PathBuf};

/// Which untracked files to list (`--untracked-files`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UntrackedMode {
    /// Don't list untracked files.
    No,
    /// List untracked files, collapsing wholly untracked folders to one entry (`dir/`).
    #[default]
    Normal,
    /// List every untracked file individually.
    All,
}

/// Which ignored files to list (`--ignored`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IgnoredMode {
    /// Don't list ignored files.
    No,
    /// List ignored files, collapsing wholly ignored folders to one entry (`node_modules/`).
    #[default]
    Traditional,
    /// List the paths that match an ignore pattern (a matching folder is listed as `dir/`).
    Matching,
}

/// Options for [`GitRunner::status`](crate::GitRunner::status).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusOptions {
    /// Untracked files to list.
    pub untracked: UntrackedMode,
    /// Ignored files to list.
    pub ignored: IgnoredMode,
}

/// A repository-relative path as Git printed it (raw bytes, `/`-separated).
///
/// Git paths are byte strings. They are kept verbatim so that nothing is lost on Unix systems
/// with non-UTF-8 file names; [`GitPath::to_path`] converts when the bytes are UTF-8 (always
/// the case on Windows).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitPath(Vec<u8>);

impl GitPath {
    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// A display form (invalid UTF-8 replaced).
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }

    /// Whether Git reported a whole folder (`dir/`), as it does for wholly untracked or ignored
    /// folders.
    pub fn is_dir(&self) -> bool {
        self.0.ends_with(b"/")
    }

    /// The absolute path under `worktree`, or `None` if the bytes aren't UTF-8.
    pub fn to_path(&self, worktree: &Path) -> Option<PathBuf> {
        let s = std::str::from_utf8(&self.0).ok()?;
        let mut p = worktree.to_path_buf();
        p.extend(s.split('/').filter(|c| !c.is_empty()));
        Some(p)
    }
}

impl From<&str> for GitPath {
    fn from(s: &str) -> Self {
        Self(s.as_bytes().to_vec())
    }
}

/// What kind of status record an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// A tracked path with staged and/or unstaged changes (record `1`).
    Changed,
    /// A renamed or copied path (record `2`; not produced since renames are disabled, but
    /// parsed).
    RenamedOrCopied,
    /// An unmerged path, i.e. a merge conflict (record `u`).
    Unmerged,
    /// An untracked path (record `?`).
    Untracked,
    /// An ignored path (record `!`).
    Ignored,
}

/// Submodule state of a changed entry (`S<c><m><u>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmoduleState {
    /// The submodule's checked-out commit differs from the one recorded.
    pub commit_changed: bool,
    /// The submodule has tracked changes. Always `false` here: submodule work trees are not
    /// inspected (each must be analysed on its own; see [`GitRunner::status`]).
    ///
    /// [`GitRunner::status`]: crate::GitRunner::status
    pub has_tracked_changes: bool,
    /// The submodule has untracked files. Always `false` here, as above.
    pub has_untracked: bool,
}

/// One `git status` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// The record kind.
    pub kind: StatusKind,
    /// Staged status (`X`): `.` unmodified, `M`, `T`, `A`, `D`, `R`, `C`, `U`. `?`/`!` for
    /// untracked/ignored entries.
    pub index: char,
    /// Unstaged status (`Y`), same codes.
    pub worktree: char,
    /// Set when the path is a submodule.
    pub submodule: Option<SubmoduleState>,
    /// The path, relative to the worktree root.
    pub path: GitPath,
    /// The source path of a rename or copy.
    pub orig_path: Option<GitPath>,
}

/// The `# branch.*` headers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BranchInfo {
    /// The commit HEAD points at; `None` before the first commit.
    pub oid: Option<ObjectId>,
    /// The checked-out branch (short name); `None` when HEAD is detached.
    pub head: Option<String>,
    /// The upstream as `<remote>/<branch>`, when configured.
    pub upstream: Option<String>,
    /// Commits on HEAD not on the upstream (only when the upstream ref exists).
    pub ahead: Option<u64>,
    /// Commits on the upstream not on HEAD (only when the upstream ref exists).
    pub behind: Option<u64>,
}

/// Parsed `git status --porcelain=v2 -z --branch` output for one worktree.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// Branch headers.
    pub branch: BranchInfo,
    /// All entries, in Git's order.
    pub entries: Vec<StatusEntry>,
}

impl Status {
    /// Tracked paths with staged or unstaged changes, including conflicts.
    pub fn tracked_changes(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| {
            matches!(
                e.kind,
                StatusKind::Changed | StatusKind::RenamedOrCopied | StatusKind::Unmerged
            )
        })
    }

    /// Untracked (non-ignored) paths.
    pub fn untracked(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind == StatusKind::Untracked)
    }

    /// Ignored paths (only listed when asked for in [`StatusOptions`]).
    pub fn ignored(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.kind == StatusKind::Ignored)
    }

    /// No tracked changes and no untracked files (ignored files don't count).
    pub fn is_clean(&self) -> bool {
        self.tracked_changes().next().is_none() && self.untracked().next().is_none()
    }
}

/// Parses `git status --porcelain=v2 -z --branch` output.
pub(crate) fn parse_status(out: &[u8]) -> Result<Status, String> {
    let mut status = Status::default();
    let mut records = out.split(|&b| b == 0);
    while let Some(rec) = records.next() {
        if rec.is_empty() {
            continue;
        }
        let err = |what: &str| format!("{what}: {:?}", String::from_utf8_lossy(rec));
        match rec[0] {
            b'#' => parse_header(rec, &mut status.branch).map_err(|e| err(&e))?,
            b'1' => {
                let f = fields(rec, 9).ok_or_else(|| err("short `1` record"))?;
                status
                    .entries
                    .push(changed(StatusKind::Changed, f[1], f[2], f[8], None)?);
            }
            b'2' => {
                let f = fields(rec, 10).ok_or_else(|| err("short `2` record"))?;
                let orig = records
                    .next()
                    .filter(|o| !o.is_empty())
                    .ok_or_else(|| err("rename without source path"))?;
                status.entries.push(changed(
                    StatusKind::RenamedOrCopied,
                    f[1],
                    f[2],
                    f[9],
                    Some(GitPath(orig.to_vec())),
                )?);
            }
            b'u' => {
                let f = fields(rec, 11).ok_or_else(|| err("short `u` record"))?;
                status
                    .entries
                    .push(changed(StatusKind::Unmerged, f[1], f[2], f[10], None)?);
            }
            b'?' | b'!' => {
                let path = rec
                    .strip_prefix(&[rec[0], b' '])
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| err("bad untracked/ignored record"))?;
                let (kind, code) = if rec[0] == b'?' {
                    (StatusKind::Untracked, '?')
                } else {
                    (StatusKind::Ignored, '!')
                };
                status.entries.push(StatusEntry {
                    kind,
                    index: code,
                    worktree: code,
                    submodule: None,
                    path: GitPath(path.to_vec()),
                    orig_path: None,
                });
            }
            _ => return Err(err("unknown status record")),
        }
    }
    Ok(status)
}

/// Splits `rec` into exactly `n` space-separated fields; the last one (the path) may contain
/// spaces.
fn fields(rec: &[u8], n: usize) -> Option<Vec<&[u8]>> {
    let f: Vec<&[u8]> = rec.splitn(n, |&b| b == b' ').collect();
    (f.len() == n && !f[n - 1].is_empty()).then_some(f)
}

fn changed(
    kind: StatusKind,
    xy: &[u8],
    sub: &[u8],
    path: &[u8],
    orig_path: Option<GitPath>,
) -> Result<StatusEntry, String> {
    let [x, y] = xy else {
        return Err(format!("bad XY field {:?}", String::from_utf8_lossy(xy)));
    };
    let submodule = match sub {
        b"N..." => None,
        [b'S', c, m, u] => Some(SubmoduleState {
            commit_changed: *c == b'C',
            has_tracked_changes: *m == b'M',
            has_untracked: *u == b'U',
        }),
        _ => {
            return Err(format!(
                "bad submodule field {:?}",
                String::from_utf8_lossy(sub)
            ));
        }
    };
    Ok(StatusEntry {
        kind,
        index: char::from(*x),
        worktree: char::from(*y),
        submodule,
        path: GitPath(path.to_vec()),
        orig_path,
    })
}

fn parse_header(rec: &[u8], b: &mut BranchInfo) -> Result<(), String> {
    let line = String::from_utf8_lossy(rec);
    let Some(rest) = line.strip_prefix("# ") else {
        return Err("bad header".into());
    };
    let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
    match key {
        "branch.oid" if value == "(initial)" => b.oid = None,
        "branch.oid" => b.oid = Some(ObjectId::parse(value).ok_or("bad branch.oid")?),
        "branch.head" if value == "(detached)" => b.head = None,
        "branch.head" => b.head = Some(value.to_owned()),
        "branch.upstream" => b.upstream = Some(value.to_owned()),
        "branch.ab" => {
            let (a, r) = value.split_once(' ').ok_or("bad branch.ab")?;
            let num = |s: &str, sign: char| s.strip_prefix(sign).and_then(|n| n.parse().ok());
            b.ahead = Some(num(a, '+').ok_or("bad branch.ab")?);
            b.behind = Some(num(r, '-').ok_or("bad branch.ab")?);
        }
        // Other headers (e.g. `# stash`) are informational.
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn parses_every_record_kind() {
        let out = format!(
            "# branch.oid {H}\0# branch.head main\0# branch.upstream origin/main\0\
             # branch.ab +2 -1\0\
             1 .M N... 100644 100644 100644 {H} {H} dir/a file.txt\0\
             1 A. N... 000000 100644 100644 {H} {H} new\0\
             2 R. N... 100644 100644 100644 {H} {H} R100 to\0from\0\
             u UU N... 100644 100644 100644 100644 {H} {H} {H} conflict\0\
             1 .M SC.. 160000 160000 160000 {H} {H} sub\0\
             ? untracked dir/\0! node_modules/\0"
        );
        let s = parse_status(out.as_bytes()).expect("parse");
        assert_eq!(s.branch.oid.as_ref().map(ObjectId::as_str), Some(H));
        assert_eq!(s.branch.head.as_deref(), Some("main"));
        assert_eq!(s.branch.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.branch.ahead, s.branch.behind), (Some(2), Some(1)));
        assert_eq!(s.entries.len(), 7);
        assert_eq!(s.entries[0].path, GitPath::from("dir/a file.txt"));
        assert_eq!((s.entries[0].index, s.entries[0].worktree), ('.', 'M'));
        assert_eq!(s.entries[2].kind, StatusKind::RenamedOrCopied);
        assert_eq!(s.entries[2].orig_path, Some(GitPath::from("from")));
        assert_eq!(s.entries[3].kind, StatusKind::Unmerged);
        assert_eq!(
            s.entries[4].submodule,
            Some(SubmoduleState {
                commit_changed: true,
                has_tracked_changes: false,
                has_untracked: false
            })
        );
        assert!(s.entries[5].path.is_dir());
        assert_eq!(s.tracked_changes().count(), 5);
        assert_eq!(s.untracked().count(), 1);
        assert_eq!(s.ignored().count(), 1);
        assert!(!s.is_clean());
    }

    #[test]
    fn initial_and_detached_headers() {
        let s = parse_status(b"# branch.oid (initial)\0# branch.head (detached)\0").expect("parse");
        assert_eq!(s.branch, BranchInfo::default());
        assert!(s.is_clean());
    }

    #[test]
    fn ignored_only_is_clean() {
        let s = parse_status(b"! .env\0").expect("parse");
        assert!(s.is_clean());
        assert_eq!(s.ignored().count(), 1);
    }

    #[test]
    fn non_utf8_paths_are_kept_verbatim() {
        let s = parse_status(b"? caf\xe9\0").expect("parse");
        assert_eq!(s.entries[0].path.as_bytes(), b"caf\xe9");
        assert_eq!(s.entries[0].path.to_path(Path::new("/w")), None);
        let ok = GitPath::from("a/b c/d");
        assert_eq!(
            ok.to_path(Path::new("/w")),
            Some(Path::new("/w").join("a").join("b c").join("d"))
        );
    }

    #[test]
    fn malformed_output_is_rejected() {
        for bad in [
            &b"1 .M N... 100644"[..],
            b"2 R. N... 100644 100644 100644 a b R100 to\0",
            b"1 .MX N... 1 2 3 a b p",
            b"1 .M Z... 1 2 3 a b p",
            b"? \0",
            b"x what",
            b"# branch.oid nothex",
            b"# branch.ab 2 1",
        ] {
            assert!(
                parse_status(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
