//! Allowlisted roots (MCP-03, threat T-43).
//!
//! Every path a tool receives is checked here before anything else looks at it:
//!
//! 1. **Shape.** Non-empty, bounded length, no NUL or other control characters, not
//!    option-like (`-x`), absolute after `~` expansion, and no `..` component at all (no
//!    traversal, even one that would land back inside a root).
//! 2. **Lexical containment.** The path must lie inside an allowlisted root, compared
//!    component-wise against both the root as configured and its canonical form. A path
//!    outside is refused **without touching the filesystem**, so a refusal never reveals
//!    whether something exists outside the allowlist.
//! 3. **Canonical containment.** The path is canonicalized (symlinks resolved) and must still
//!    lie inside a canonical root, so a symlink inside a root that points outside is refused
//!    with the same message as step 2.
//!
//! The allowlist is the configured scan roots, narrowed to the client's MCP roots when the
//! client provides them ([`Allowlist::narrowed`]): a path must be inside both.

use scull_engine::paths::{canon, is_within};
use serde::Serialize;
use std::path::{Component, Path, PathBuf};

/// Longest path argument accepted, in bytes.
pub const MAX_PATH_ARG_BYTES: usize = 4096;

/// Why a path argument was refused. The message never contains the path itself or anything
/// learned about it from the filesystem outside the allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// Empty, too long, or contains control characters.
    InvalidPath,
    /// Looks like a command-line option (`-rf`, `--help`).
    OptionLike,
    /// Not absolute (and not `~`-relative).
    NotAbsolute,
    /// Contains a `..` component.
    Traversal,
    /// Outside every allowlisted root (directly or through a symlink).
    OutsideAllowlist,
    /// Inside the allowlist but does not exist or can't be resolved.
    NotFound,
    /// No roots are allowlisted at all.
    NoRoots,
}

impl Refusal {
    /// Stable machine code.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::InvalidPath => "invalid_path",
            Refusal::OptionLike => "option_like",
            Refusal::NotAbsolute => "not_absolute",
            Refusal::Traversal => "traversal",
            Refusal::OutsideAllowlist => "outside_allowlist",
            Refusal::NotFound => "not_found",
            Refusal::NoRoots => "no_roots",
        }
    }

    /// Human explanation (never includes the path).
    pub fn message(self) -> &'static str {
        match self {
            Refusal::InvalidPath => "the path is empty, too long, or contains control characters",
            Refusal::OptionLike => "the path looks like a command-line option",
            Refusal::NotAbsolute => {
                "the path must be absolute or start with `~/`; use a path returned by \
                 get_status (allowlisted roots) or another tool"
            }
            Refusal::Traversal => "`..` components are not allowed in paths",
            Refusal::OutsideAllowlist => {
                "the path is outside the allowlisted roots; call get_status to see them"
            }
            Refusal::NotFound => "the path does not exist inside the allowlisted roots",
            Refusal::NoRoots => {
                "no roots are allowlisted: set scan.roots in scull's config file (scull init)"
            }
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// One allowlisted root: as configured (absolute, `~` expanded) and canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// As configured.
    pub given: PathBuf,
    /// Canonical (symlinks resolved).
    pub canonical: PathBuf,
}

/// The set of roots tools may look inside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowlist {
    roots: Vec<Root>,
    home: Option<PathBuf>,
}

impl Allowlist {
    /// The allowlist from configured roots (absolute, `~` already expanded). Relative roots
    /// are dropped; roots inside another root are kept (harmless, and keeps them visible).
    pub fn new(roots: &[PathBuf], home: Option<&Path>) -> Self {
        let mut out: Vec<Root> = Vec::new();
        for r in roots {
            if !r.is_absolute() || has_parent_component(r) {
                continue;
            }
            let root = Root {
                given: r.clone(),
                canonical: canon(r),
            };
            if !out.iter().any(|o| o.canonical == root.canonical) {
                out.push(root);
            }
        }
        Self {
            roots: out,
            home: home.map(Path::to_path_buf),
        }
    }

    /// The roots, in configuration order.
    pub fn roots(&self) -> &[Root] {
        &self.roots
    }

    /// Canonical roots, in configuration order.
    pub fn canonical_roots(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|r| r.canonical.clone()).collect()
    }

    /// Whether nothing is allowlisted.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The intersection with the client's roots (absolute paths): for each configured root
    /// and client root, the deeper of the two when one contains the other.
    pub fn narrowed(&self, client_roots: &[PathBuf]) -> Self {
        let mut out: Vec<Root> = Vec::new();
        for c in client_roots {
            if !c.is_absolute() || has_parent_component(c) {
                continue;
            }
            let cc = canon(c);
            for r in &self.roots {
                let pick = if is_within(&cc, &r.canonical) {
                    Some(Root {
                        given: c.clone(),
                        canonical: cc.clone(),
                    })
                } else if is_within(&r.canonical, &cc) {
                    Some(r.clone())
                } else {
                    None
                };
                if let Some(p) = pick
                    && !out.iter().any(|o| o.canonical == p.canonical)
                {
                    out.push(p);
                }
            }
        }
        Self {
            roots: out,
            home: self.home.clone(),
        }
    }

    /// Check a path argument and return its canonical form (see the module docs).
    pub fn check(&self, arg: &str) -> Result<PathBuf, Refusal> {
        let path = self.shape(arg)?;
        if self.roots.is_empty() {
            return Err(Refusal::NoRoots);
        }
        let lexically_inside = self
            .roots
            .iter()
            .any(|r| is_within(&path, &r.given) || is_within(&path, &r.canonical));
        if !lexically_inside {
            return Err(Refusal::OutsideAllowlist);
        }
        let real = std::fs::canonicalize(&path).map_err(|_| Refusal::NotFound)?;
        let real = canon(&real);
        if self.contains_canonical(&real) {
            Ok(real)
        } else {
            Err(Refusal::OutsideAllowlist)
        }
    }

    /// Whether an already-canonical path lies inside a canonical root.
    pub fn contains_canonical(&self, p: &Path) -> bool {
        self.roots.iter().any(|r| is_within(p, &r.canonical))
    }

    /// Step 1 of [`Allowlist::check`]: the argument as an absolute path, or why not.
    fn shape(&self, arg: &str) -> Result<PathBuf, Refusal> {
        if arg.is_empty() || arg.len() > MAX_PATH_ARG_BYTES || arg.chars().any(char::is_control) {
            return Err(Refusal::InvalidPath);
        }
        if arg.starts_with('-') {
            return Err(Refusal::OptionLike);
        }
        let p = Path::new(arg);
        let expanded = match (&self.home, arg) {
            (Some(h), a) if a == "~" || a.starts_with("~/") || a.starts_with("~\\") => {
                scull_core::config::expand_home(p, h)
            }
            _ => p.to_path_buf(),
        };
        if has_parent_component(&expanded) {
            return Err(Refusal::Traversal);
        }
        if !expanded.is_absolute() {
            return Err(Refusal::NotAbsolute);
        }
        Ok(expanded)
    }
}

fn has_parent_component(p: &Path) -> bool {
    p.components().any(|c| matches!(c, Component::ParentDir))
}

/// The local path of a `file://` URI (MCP roots), or `None` for any other scheme or a remote
/// host. Percent-escapes are decoded. Windows drive URIs (`file:///C:/x`, `file:///c%3A/x`)
/// become native Windows paths (`C:\x`): backslashes and an upper-case drive letter, the
/// spelling the home folder has, so home redaction recognizes them (a `C:/Users/<name>`
/// spelling was returned unredacted).
pub fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let (host, path) = rest.split_at(rest.find('/')?);
    if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let decoded = percent_decode(path)?;
    // `/c:/x` → `C:\x` (Windows drive letter).
    let b = decoded.as_bytes();
    let decoded = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        let mut win = String::with_capacity(decoded.len());
        win.push(char::from(b[1].to_ascii_uppercase()));
        win.push_str(&decoded[2..].replace('/', "\\"));
        win
    } else {
        decoded
    };
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(dir: &Path) -> Allowlist {
        Allowlist::new(&[dir.to_path_buf()], None)
    }

    #[test]
    fn shape_rules() {
        let d = tempfile::tempdir().expect("tempdir");
        let a = list(d.path());
        assert_eq!(a.check(""), Err(Refusal::InvalidPath));
        assert_eq!(a.check("a\nb"), Err(Refusal::InvalidPath));
        assert_eq!(a.check("/x\0y"), Err(Refusal::InvalidPath));
        assert_eq!(
            a.check(&"/a".repeat(MAX_PATH_ARG_BYTES)),
            Err(Refusal::InvalidPath)
        );
        assert_eq!(a.check("-rf"), Err(Refusal::OptionLike));
        assert_eq!(a.check("--help"), Err(Refusal::OptionLike));
        assert_eq!(a.check("relative/dir"), Err(Refusal::NotAbsolute));
        assert_eq!(a.check("~/x"), Err(Refusal::NotAbsolute), "no home known");
        let inside_then_out = d.path().join("..").join("x");
        assert_eq!(
            a.check(&inside_then_out.to_string_lossy()),
            Err(Refusal::Traversal)
        );
        let inside_and_back = d.path().join("sub").join("..");
        assert_eq!(
            a.check(&inside_and_back.to_string_lossy()),
            Err(Refusal::Traversal)
        );
    }

    #[test]
    fn containment() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("root");
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::create_dir_all(d.path().join("rootish")).expect("mkdir");
        let a = list(&root);
        assert_eq!(
            a.check(&root.join("sub").to_string_lossy()),
            Ok(canon(&root.join("sub")))
        );
        assert_eq!(a.check(&root.to_string_lossy()), Ok(canon(&root)));
        assert_eq!(
            a.check(&d.path().join("rootish").to_string_lossy()),
            Err(Refusal::OutsideAllowlist)
        );
        assert_eq!(
            a.check(&d.path().to_string_lossy()),
            Err(Refusal::OutsideAllowlist)
        );
        // Outside and missing: same answer as outside and present (no existence leak).
        assert_eq!(
            a.check(&d.path().join("nope").to_string_lossy()),
            Err(Refusal::OutsideAllowlist)
        );
        assert_eq!(
            a.check(&root.join("nope").to_string_lossy()),
            Err(Refusal::NotFound)
        );
        assert_eq!(
            Allowlist::default().check(&root.to_string_lossy()),
            Err(Refusal::NoRoots)
        );
    }

    #[test]
    fn home_relative_paths_expand() {
        let d = tempfile::tempdir().expect("tempdir");
        let home = canon(d.path());
        std::fs::create_dir_all(home.join("Projects/app")).expect("mkdir");
        let a = Allowlist::new(&[home.join("Projects")], Some(&home));
        assert_eq!(
            a.check("~/Projects/app"),
            Ok(home.join("Projects").join("app"))
        );
        assert_eq!(a.check("~"), Err(Refusal::OutsideAllowlist));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused() {
        let d = tempfile::tempdir().expect("tempdir");
        let root = d.path().join("root");
        let outside = d.path().join("outside");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::os::unix::fs::symlink(&outside, root.join("escape")).expect("symlink");
        let a = list(&root);
        assert_eq!(
            a.check(&root.join("escape").to_string_lossy()),
            Err(Refusal::OutsideAllowlist)
        );
    }

    #[test]
    fn narrowing_intersects() {
        let d = tempfile::tempdir().expect("tempdir");
        let base = canon(d.path());
        for p in ["a/inner", "b", "c"] {
            std::fs::create_dir_all(base.join(p)).expect("mkdir");
        }
        let a = Allowlist::new(&[base.join("a"), base.join("b")], None);
        let n = a.narrowed(&[base.join("a/inner"), base.clone(), base.join("c")]);
        assert_eq!(
            n.canonical_roots(),
            vec![base.join("a/inner"), base.join("a"), base.join("b")]
        );
        let n = a.narrowed(&[base.join("c")]);
        assert!(n.is_empty());
        assert_eq!(
            n.check(&base.join("a").to_string_lossy()),
            Err(Refusal::NoRoots)
        );
    }

    #[test]
    fn file_uris() {
        assert_eq!(
            path_from_file_uri("file:///home/me/My%20Code"),
            Some(PathBuf::from("/home/me/My Code"))
        );
        assert_eq!(
            path_from_file_uri("file://localhost/srv/x"),
            Some(PathBuf::from("/srv/x"))
        );
        assert_eq!(
            path_from_file_uri("file:///C:/Users/me"),
            Some(PathBuf::from(r"C:\Users\me"))
        );
        assert_eq!(
            path_from_file_uri("file:///c%3A/Users/me/x"),
            Some(PathBuf::from(r"C:\Users\me\x"))
        );
        assert_eq!(path_from_file_uri("file://server/share/x"), None);
        assert_eq!(path_from_file_uri("https://example.com/x"), None);
        assert_eq!(path_from_file_uri("file:///bad%zz"), None);
    }
}
