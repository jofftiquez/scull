//! Trusted resolution of package-manager binaries (SAFE-12, T-22).
//!
//! A local copy of the approach of `scull-git`'s `resolve.rs` (crate-private there),
//! generalized to any tool name. The duplication is deliberate for now: extracting a shared
//! helper (e.g. into `scull-platform`) would touch two other lanes.
//!
//! The search list (normally the user's `PATH`, injected by the caller so tests can point it
//! at a temp dir) is walked explicitly instead of letting the OS search. An entry is skipped
//! when it is:
//!
//! * empty or relative (`""`, `.`, `bin`): these resolve against the current directory, which
//!   may be a scan root or a malicious checkout;
//! * the current directory itself;
//! * inside a `node_modules` folder (`node_modules/.bin` is package-controlled);
//! * inside one of the caller's excluded roots (the scan roots);
//! * not an existing directory.
//!
//! The candidate binary is canonicalized and re-checked, so a symlink in a trusted directory
//! that points into an excluded root, the current directory or a `node_modules` folder is
//! rejected too. Only `<tool>` (Unix) / `<tool>.exe` (Windows) is run directly: never a
//! `.cmd`/`.bat`/`.ps1` shim, which would need a shell.
//!
//! # `.cmd` shims (Windows, P2-QC-02)
//!
//! Without a trusted executable, a `<tool>.cmd` in a trusted directory is **read** (never run)
//! with [`super::shim`]. When it is an npm-style shim starting Node on a script next to it,
//! the tool is run as `node <script> <args>` ([`Lookup::NodeScript`]), with no shell, after
//! both paths pass the trusted-locations rules:
//!
//! * **Node**: the shim folder's own `node.exe` when the shim prefers it and it exists
//!   (checked like any binary: canonical path, not in an untrusted place), otherwise a trusted
//!   `node` executable found exactly like a tool (never another shim);
//! * **the script**: canonicalized, a regular `.js`/`.cjs`/`.mjs` file **inside the trusted
//!   directory holding the shim** (a symlink or `..` out of it is refused), and not in the
//!   current directory or a scan root. A `node_modules` folder *below* the trusted directory
//!   is allowed here: that is where a global install keeps the tool's own code (unlike a
//!   `node_modules/.bin` on the search list, which belongs to a project).
//!
//! A shim that fails any of this, and any `.bat`/`.ps1`, is reported as
//! [`Lookup::ShellShimOnly`] so the preview can say why the tool was skipped. Shims are
//! considered on Windows only ([`TrustedDirs::with_cmd_shims`] turns it on elsewhere, for the
//! fixtures).

use super::shim;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Script shims that would need a shell to run (Windows npm/pnpm/yarn installs ship these).
const SHELL_SHIMS: [&str; 3] = ["cmd", "bat", "ps1"];

/// The outcome of looking one tool up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// A trusted executable: its canonical absolute path.
    Found(PathBuf),
    /// A trusted `.cmd` shim that starts Node on a script: run `node <script> <args>` instead
    /// (P2-QC-02; see the module docs).
    NodeScript(NodeScript),
    /// No trusted executable, but a script shim that needs a shell exists in a trusted
    /// directory (SAFE-12: no shell); its path.
    ShellShimOnly(PathBuf),
    /// No trusted executable at all.
    NotFound,
}

/// A tool installed as an npm-style `.cmd` shim, resolved to what it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeScript {
    /// The shim (read, never run).
    pub shim: PathBuf,
    /// The trusted Node executable: canonical absolute path.
    pub node: PathBuf,
    /// The tool's JavaScript entry point: canonical absolute path inside the shim's trusted
    /// directory.
    pub script: PathBuf,
}

impl Lookup {
    /// What to start for this tool: the binary, and the script that comes before the tool's
    /// own arguments (shims). `None` when there is nothing Quick Clean may run.
    pub fn program(&self) -> Option<(PathBuf, Option<PathBuf>)> {
        match self {
            Lookup::Found(b) => Some((b.clone(), None)),
            Lookup::NodeScript(n) => Some((n.node.clone(), Some(n.script.clone()))),
            Lookup::ShellShimOnly(_) | Lookup::NotFound => None,
        }
    }
}

/// The trusted directories of a search list, resolved once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedDirs {
    dirs: Vec<PathBuf>,
    untrusted: Untrusted,
    cmd_shims: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Untrusted {
    cwd: Option<PathBuf>,
    excluded: Vec<PathBuf>,
}

impl Untrusted {
    fn contains(&self, p: &Path) -> bool {
        has_node_modules(p) || self.contains_location(p)
    }

    /// The current directory or inside a scan root (the `node_modules` rule aside).
    fn contains_location(&self, p: &Path) -> bool {
        self.cwd.as_deref() == Some(p) || self.excluded.iter().any(|root| p.starts_with(root))
    }
}

impl TrustedDirs {
    /// Filters `search_path` (a `PATH`-style list) as described in the module docs. `cwd` is
    /// the process's current directory and `excluded` the scan roots.
    pub fn new(search_path: &OsStr, cwd: Option<&Path>, excluded: &[PathBuf]) -> Self {
        let untrusted = Untrusted {
            cwd: cwd.and_then(canonical),
            excluded: excluded.iter().filter_map(|r| canonical(r)).collect(),
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in std::env::split_paths(search_path) {
            if entry.as_os_str().is_empty() || !entry.is_absolute() || has_node_modules(&entry) {
                continue;
            }
            let Some(dir) = canonical(&entry) else {
                continue;
            };
            if !dir.is_dir() || untrusted.contains(&dir) || dirs.contains(&dir) {
                continue;
            }
            dirs.push(dir);
        }
        Self {
            dirs,
            untrusted,
            cmd_shims: cfg!(windows),
        }
    }

    /// Whether `.cmd` shims are resolved ([`Lookup::NodeScript`]). On by default on Windows
    /// only, where npm, pnpm and Yarn install them; the fixtures turn it on elsewhere to run
    /// the same resolution with fake shims.
    #[must_use]
    pub fn with_cmd_shims(mut self, on: bool) -> Self {
        self.cmd_shims = on;
        self
    }

    /// The trusted directories, in search order (also the child's `PATH`).
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The directories joined into a `PATH` value (entries that can't be joined are dropped).
    pub fn joined(&self) -> OsString {
        let joinable: Vec<&PathBuf> = self
            .dirs
            .iter()
            .filter(|d| std::env::join_paths([d.as_os_str()]).is_ok())
            .collect();
        std::env::join_paths(joinable).unwrap_or_default()
    }

    /// Looks `tool` (a bare name such as `npm`) up in the trusted directories: a trusted
    /// executable first, then (Windows) an npm-style `.cmd` shim resolved to Node and its
    /// script (see the module docs).
    pub fn find(&self, tool: &str) -> Lookup {
        if tool.is_empty() || tool.contains(['/', '\\']) || tool.starts_with('.') {
            return Lookup::NotFound;
        }
        if let Some(b) = self.executable(tool) {
            return Lookup::Found(b);
        }
        if !self.cmd_shims {
            return Lookup::NotFound;
        }
        let mut shell_only = None;
        for dir in &self.dirs {
            for ext in SHELL_SHIMS {
                let shim = dir.join(format!("{tool}.{ext}"));
                if !shim.is_file() {
                    continue;
                }
                if ext == "cmd"
                    && let Ok(n) = self.node_script(dir, &shim)
                {
                    return Lookup::NodeScript(n);
                }
                shell_only.get_or_insert(shim);
            }
        }
        shell_only.map_or(Lookup::NotFound, Lookup::ShellShimOnly)
    }

    /// A trusted `<tool>` executable (`.exe` on Windows): canonical, not in an untrusted place.
    fn executable(&self, tool: &str) -> Option<PathBuf> {
        let name = format!("{tool}{}", std::env::consts::EXE_SUFFIX);
        self.dirs
            .iter()
            .find_map(|dir| self.checked_binary(&dir.join(&name)))
    }

    /// `candidate` canonicalized, if it is a file outside every untrusted place.
    fn checked_binary(&self, candidate: &Path) -> Option<PathBuf> {
        if !candidate.is_file() {
            return None;
        }
        let real = canonical(candidate)?;
        let parent_ok = real.parent().is_some_and(|p| !self.untrusted.contains(p));
        (parent_ok && !self.untrusted.contains(&real) && real.is_file()).then_some(real)
    }

    /// The Node program and script of the shim `cmd` in the trusted directory `dir`, or why
    /// the shim can't be run without a shell.
    pub fn node_script(&self, dir: &Path, cmd: &Path) -> Result<NodeScript, String> {
        if !self.dirs.iter().any(|d| d == dir) {
            return Err(format!("{} is not a trusted directory", dir.display()));
        }
        let md = fs::symlink_metadata(cmd).map_err(|e| e.to_string())?;
        if !md.is_file() || md.len() > shim::MAX_SHIM_BYTES {
            return Err("the shim is not a small regular file".into());
        }
        let text = fs::read_to_string(cmd).map_err(|e| e.to_string())?;
        let target = shim::parse(&text).map_err(|e| e.to_string())?;
        // The script: strictly inside the trusted directory holding the shim.
        let mut script = dir.to_path_buf();
        for part in &target.script {
            script.push(part);
        }
        let real = canonical(&script).ok_or("the script does not exist")?;
        let inside = real
            .strip_prefix(dir)
            .is_ok_and(|rest| !rest.as_os_str().is_empty());
        if !inside || !real.is_file() || self.untrusted.contains_location(&real) {
            return Err(format!(
                "the script {} is not a file inside {}",
                real.display(),
                dir.display()
            ));
        }
        // Node: next to the shim when the shim prefers it, else a trusted `node` executable.
        let adjacent = dir.join(format!("node{}", std::env::consts::EXE_SUFFIX));
        let node = if target.node == shim::NodeRef::AdjacentThenPath && adjacent.is_file() {
            self.checked_binary(&adjacent)
        } else {
            self.executable("node")
        }
        .ok_or("no trusted node executable")?;
        Ok(NodeScript {
            shim: cmd.to_path_buf(),
            node,
            script: real,
        })
    }
}

/// Canonical form of an existing path, without Windows' verbatim prefix.
pub(crate) fn canonical(p: &Path) -> Option<PathBuf> {
    fs::canonicalize(p).ok().map(strip_verbatim)
}

/// Removes Windows' `\\?\` prefix from a canonicalized path where it isn't needed.
pub(crate) fn strip_verbatim(p: PathBuf) -> PathBuf {
    let Some(s) = p.to_str() else { return p };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    p
}

fn has_node_modules(p: &Path) -> bool {
    p.components().any(|c| match c {
        Component::Normal(s) => s
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("node_modules")),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(tool: &str) -> String {
        format!("{tool}{}", std::env::consts::EXE_SUFFIX)
    }

    #[test]
    fn node_modules_is_detected_case_insensitively() {
        assert!(has_node_modules(Path::new("/a/node_modules/.bin")));
        assert!(has_node_modules(Path::new("/a/Node_Modules/x")));
        assert!(!has_node_modules(Path::new("/a/node_modules_x/bin")));
    }

    #[test]
    fn empty_and_relative_entries_are_never_trusted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).expect("mkdir");
        fs::write(bin.join(exe("npm")), "").expect("write");
        let path = std::env::join_paths([OsStr::new(""), OsStr::new("."), OsStr::new("bin")])
            .expect("join");
        let t = TrustedDirs::new(&path, Some(tmp.path()), &[]);
        assert_eq!(t.find("npm"), Lookup::NotFound);
        assert!(t.dirs().is_empty());
    }

    #[test]
    fn cwd_excluded_roots_and_node_modules_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mk = |rel: &str| {
            let d = tmp.path().join(rel);
            fs::create_dir_all(&d).expect("mkdir");
            fs::write(d.join(exe("npm")), "").expect("write");
            d
        };
        let cwd = mk("cwd");
        let scan = mk("scan/repo");
        let nm = mk("proj/node_modules/.bin");
        let good = mk("trusted");
        let path = std::env::join_paths([&cwd, &scan, &nm, &good]).expect("join");
        let t = TrustedDirs::new(&path, Some(&cwd), &[tmp.path().join("scan")]);
        let good = canonical(&good).expect("canon");
        assert_eq!(t.find("npm"), Lookup::Found(good.join(exe("npm"))));
        assert_eq!(t.dirs(), [good]);
    }

    #[test]
    fn names_with_separators_are_never_looked_up() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = std::env::join_paths([tmp.path()]).expect("join");
        let t = TrustedDirs::new(&path, None, &[]);
        for bad in ["", "../npm", "a/npm", r"a\npm", ".npm"] {
            assert_eq!(t.find(bad), Lookup::NotFound, "{bad}");
        }
    }

    /// A Yarn-installer style shim (`node "%~dp0\<script>" %*`) and its script.
    fn shim(dir: &Path, tool: &str, script: &str) {
        let p = script.split('/').fold(dir.to_path_buf(), |p, c| p.join(c));
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, "// js\n").expect("write script");
        let win = script.replace('/', r"\");
        fs::write(
            dir.join(format!("{tool}.cmd")),
            format!("@echo off\r\nnode \"%~dp0\\{win}\" %*\r\n"),
        )
        .expect("write shim");
    }

    #[test]
    fn cmd_shims_resolve_to_trusted_node_and_script_only_when_enabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tools = tmp.path().join("tools");
        let nodejs = tmp.path().join("nodejs");
        let cwd = tmp.path().join("cwd");
        for d in [&tools, &nodejs, &cwd] {
            fs::create_dir_all(d).expect("mkdir");
        }
        shim(&tools, "yarn", "yarn.js");
        let path = std::env::join_paths([&cwd, &tools, &nodejs]).expect("join");
        let trusted = |shims| TrustedDirs::new(&path, Some(&cwd), &[]).with_cmd_shims(shims);

        // Off: the shim is invisible. On, without any trusted node: shell-only.
        assert_eq!(trusted(false).find("yarn"), Lookup::NotFound);
        fs::write(cwd.join(exe("node")), "").expect("node in cwd");
        assert_eq!(
            trusted(true).find("yarn"),
            Lookup::ShellShimOnly(tools.join("yarn.cmd"))
        );
        // A trusted node on the search path: node + the canonical script.
        fs::write(nodejs.join(exe("node")), "").expect("node");
        let canon = |p: PathBuf| canonical(&p).expect("canonical");
        let expected = NodeScript {
            shim: canon(tools.clone()).join("yarn.cmd"),
            node: canon(nodejs.join(exe("node"))),
            script: canon(tools.join("yarn.js")),
        };
        assert_eq!(
            trusted(true).find("yarn"),
            Lookup::NodeScript(expected.clone())
        );
        assert_eq!(
            Lookup::NodeScript(expected.clone()).program(),
            Some((expected.node.clone(), Some(expected.script.clone())))
        );
        // A real executable wins over the shim.
        fs::write(tools.join(exe("yarn")), "").expect("yarn exe");
        assert_eq!(
            trusted(true).find("yarn"),
            Lookup::Found(canon(tools.join(exe("yarn"))))
        );
        // A shim outside the trusted directories is never read.
        assert!(
            trusted(true)
                .node_script(&cwd, &cwd.join("yarn.cmd"))
                .is_err()
        );
    }
}
