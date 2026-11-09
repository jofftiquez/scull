//! Protected roots, `.scull-protect` markers and user exclusions, compared by **file
//! identity** (SAFE-03, SAFE-13, CFG-03, THREAT_MODEL T-13).
//!
//! A [`ProtectedSet`] holds the identities (device + inode, volume + file ID) of every
//! protected root, allowed carve-out and exclusion. [`ProtectedSet::check`] resolves a
//! candidate's real ancestor chain and looks each directory up **by identity**, so a case alias
//! (`~/projects` vs `~/Projects`), a Unicode normalization alias (NFC vs NFD), a Windows 8.3
//! short name or a `..` segment reaches the same identity as the protected path and cannot
//! bypass it. Paths are only compared as strings to find protected entries *inside* a
//! candidate, and then on fully resolved paths (case-insensitively on macOS and Windows).
//!
//! The result feeds the rubric's [`Signals::in_protected_root`](crate::Signals),
//! [`Signals::protect_marker`](crate::Signals) and [`Signals::user_excluded`](crate::Signals),
//! with matching [`Evidence`].
//!
//! # What protects a candidate
//!
//! | Source | Signal | Evidence |
//! |---|---|---|
//! | A built-in protected root ([`default_protected`]) or a user `scan.protect` path, at the candidate or above it ([`Scope::Subtree`]), or exactly at it ([`Scope::Exact`]) | `in_protected_root` | `protected_root` |
//! | The candidate is a filesystem or volume root | `in_protected_root` | `protected_root` |
//! | A `.scull-protect` file in the candidate or any directory above it (FX-CLS-11), or a repository `.scull.toml` `protect` path ([`ProtectedSet::add_repo_policy`], CFG-03) | `protect_marker` | `protect_marker` |
//! | A user `scan.exclude` path (or a repository `exclude`), at or above the candidate | `user_excluded` | `user_exclusion` |
//! | Any protected root or exclusion **inside** the candidate (deleting the candidate would delete it) | as above | as above |
//! | The candidate's real path or an ancestor's metadata cannot be read | `in_protected_root` (protective) | `signal_unavailable` |
//!
//! Git-tracked paths (also SAFE-03) are the Git layer's [`GitTracking`](crate::GitTracking)
//! signal, not part of this set.
//!
//! # Built-in roots ([`default_protected`])
//!
//! [`Scope::Exact`] protects a directory itself but not its contents: the home directory, the
//! directory holding home directories, filesystem and mount roots, cloud-sync roots. A user's
//! projects live inside these, so their contents stay scannable. Cloud-synced contents get the
//! rubric's separate `cloud_sync` row (S2).
//!
//! [`Scope::Subtree`] protects everything inside: OS and system directories, credentials
//! (`~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.password-store`), and on macOS `~/Library`.
//!
//! *Carve-outs* ([`DefaultProtection::allowed`]) cancel **built-in** subtree roots for what is
//! strictly inside them, never user or repository protection, and never the carve-out directory
//! itself. They exist so that per-user caches and temp files inside protected system areas
//! stay reachable for Quick Clean rules:
//!
//! | OS | Carve-out | Why |
//! |---|---|---|
//! | all | the home directory | homes under a protected tree (e.g. Fedora Silverblue's `/var/home`) |
//! | macOS | `~/Library/Caches`, `~/Library/Developer/Xcode/DerivedData` | package and build caches (Quick Clean) |
//! | macOS | `~/Library/pnpm/store`, **itself included** (`allowed_self`, #139) | pnpm's store, cleaned only with `pnpm store prune`; the rest of `~/Library/pnpm` stays protected |
//! | macOS | `~/Library/CloudStorage`, `~/Library/Mobile Documents` | synced folders: S2 via `cloud_sync`, not S3 |
//! | macOS | `/private/var/folders`, `/private/tmp` | per-user temp directories |
//!
//! Everything else in `~/Library` (application data, preferences, keychains, mail) stays
//! protected. On Windows, `AppData` is not a protected root: it holds most package caches,
//! and its contents only become candidates through a rule's marker (CLS-03).

use scull_core::config::{RepoPolicy, ScanConfig};
use scull_core::{Evidence, FileIdentity, FsPath};
use scull_platform::{EntryKind, Platform};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Name of the marker file that protects its directory and everything inside (FX-CLS-11).
pub const PROTECT_MARKER: &str = ".scull-protect";

/// What a protected path covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The directory and everything inside it.
    Subtree,
    /// The directory itself only (it never becomes a candidate), not its contents.
    Exact,
}

/// Where a protection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Built in ([`default_protected`]).
    Default,
    /// The user's config (`scan.protect`, `scan.exclude`).
    User,
    /// A repository's own `.scull.toml` (adds protection only, CFG-03).
    Repo,
}

/// One protected path before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedPath {
    /// The path as configured.
    pub path: PathBuf,
    /// What it covers.
    pub scope: Scope,
}

impl ProtectedPath {
    fn subtree(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            scope: Scope::Subtree,
        }
    }

    fn exact(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            scope: Scope::Exact,
        }
    }
}

/// The operating system whose built-in roots to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetOs {
    /// Linux.
    Linux,
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
    /// Another Unix: Linux's list.
    OtherUnix,
}

impl TargetOs {
    /// The OS this build runs on.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            TargetOs::MacOs
        } else if cfg!(windows) {
            TargetOs::Windows
        } else if cfg!(target_os = "linux") {
            TargetOs::Linux
        } else {
            TargetOs::OtherUnix
        }
    }
}

/// The built-in protected roots and carve-outs for one OS (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DefaultProtection {
    /// Protected roots.
    pub protected: Vec<ProtectedPath>,
    /// Carve-outs: built-in subtree roots don't apply strictly inside these.
    pub allowed: Vec<PathBuf>,
    /// Carve-outs that also cover the directory itself (#139): a tool-owned cache directory
    /// inside a protected area that is cleaned only with the tool's own command. Each is also
    /// in [`DefaultProtection::allowed`] for what is inside it.
    pub allowed_self: Vec<PathBuf>,
}

/// Credential directories under home, protected with their contents.
const HOME_SECRETS: &[&str] = &[".ssh", ".gnupg", ".aws", ".kube", ".password-store"];
/// Cloud-sync roots under home (protected themselves; contents are `cloud_sync`, S2).
const HOME_CLOUD_ROOTS: &[&str] = &["Dropbox", "OneDrive", "Google Drive", "iCloud Drive"];

/// The built-in protected roots for `os`. `home` is the user's home directory (if known) and
/// `env` looks up environment variables (Windows locations: `SystemDrive`, `SystemRoot`,
/// `ProgramFiles`, `ProgramData`, `OneDrive`, ...). Pure: nothing is read from the system,
/// so every OS's list is testable on any OS.
pub fn default_protected(
    os: TargetOs,
    home: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> DefaultProtection {
    // Windows paths are joined as strings so every OS's list is testable on any OS.
    let join = |base: &Path, name: &str| -> PathBuf {
        if os == TargetOs::Windows {
            let mut s = base.as_os_str().to_os_string();
            if !base.as_os_str().to_string_lossy().ends_with('\\') {
                s.push("\\");
            }
            s.push(name);
            PathBuf::from(s)
        } else {
            base.join(name)
        }
    };
    let mut d = DefaultProtection::default();
    if let Some(home) = home {
        d.protected.push(ProtectedPath::exact(home));
        d.allowed.push(home.to_path_buf());
        // The directory holding home directories (/home, /Users, C:\Users).
        let parent = if os == TargetOs::Windows {
            let s = home.as_os_str().to_string_lossy();
            s.trim_end_matches('\\')
                .rsplit_once('\\')
                .map(|(p, _)| PathBuf::from(p))
                .filter(|p| !p.as_os_str().to_string_lossy().ends_with(':'))
        } else {
            home.parent()
                .filter(|p| p.parent().is_some())
                .map(Path::to_path_buf)
        };
        if let Some(parent) = parent {
            d.protected.push(ProtectedPath::exact(parent));
        }
        for s in HOME_SECRETS {
            d.protected.push(ProtectedPath::subtree(join(home, s)));
        }
        for c in HOME_CLOUD_ROOTS {
            d.protected.push(ProtectedPath::exact(join(home, c)));
        }
    }
    match os {
        TargetOs::Linux | TargetOs::OtherUnix => {
            d.protected.push(ProtectedPath::exact("/"));
            for p in [
                "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/libx32", "/nix",
                "/opt", "/proc", "/run", "/sbin", "/snap", "/srv", "/sys", "/usr", "/var",
            ] {
                d.protected.push(ProtectedPath::subtree(p));
            }
            if home != Some(Path::new("/root")) {
                d.protected.push(ProtectedPath::subtree("/root"));
            }
            for p in ["/home", "/tmp", "/mnt", "/media"] {
                d.protected.push(ProtectedPath::exact(p));
            }
        }
        TargetOs::MacOs => {
            d.protected.push(ProtectedPath::exact("/"));
            for p in [
                "/System",
                "/Library",
                "/Applications",
                "/bin",
                "/sbin",
                "/usr",
                "/opt",
                "/cores",
                "/private",
                "/etc",
                "/var",
            ] {
                d.protected.push(ProtectedPath::subtree(p));
            }
            for p in ["/Users", "/Volumes", "/tmp"] {
                d.protected.push(ProtectedPath::exact(p));
            }
            d.allowed.push(PathBuf::from("/private/var/folders"));
            d.allowed.push(PathBuf::from("/private/tmp"));
            if let Some(home) = home {
                let lib = home.join("Library");
                d.protected.push(ProtectedPath::subtree(&lib));
                let icloud = lib.join("Mobile Documents");
                d.protected.push(ProtectedPath::exact(&icloud));
                d.protected
                    .push(ProtectedPath::exact(icloud.join("com~apple~CloudDocs")));
                d.protected
                    .push(ProtectedPath::exact(lib.join("CloudStorage")));
                d.allowed.push(lib.join("Caches"));
                // pnpm's default macOS store (#139): cleaned only with `pnpm store prune`. The
                // store itself is reachable; the rest of ~/Library/pnpm (global packages,
                // pnpm-managed Node versions) stays protected.
                let pnpm_store = lib.join("pnpm").join("store");
                d.allowed.push(pnpm_store.clone());
                d.allowed_self.push(pnpm_store);
                d.allowed
                    .push(lib.join("Developer").join("Xcode").join("DerivedData"));
                d.allowed.push(lib.join("CloudStorage"));
                d.allowed.push(icloud);
            }
        }
        TargetOs::Windows => {
            let var = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
            let drive = var("SystemDrive").unwrap_or_else(|| PathBuf::from("C:"));
            let drive_root = {
                let mut s = drive.into_os_string();
                s.push("\\");
                PathBuf::from(s)
            };
            d.protected.push(ProtectedPath::exact(&drive_root));
            let windir = var("SystemRoot")
                .or_else(|| var("windir"))
                .unwrap_or_else(|| join(&drive_root, "Windows"));
            d.protected.push(ProtectedPath::subtree(windir));
            for k in [
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramW6432",
                "ProgramData",
            ] {
                if let Some(p) = var(k) {
                    d.protected.push(ProtectedPath::subtree(p));
                }
            }
            for p in [
                "Program Files",
                "Program Files (x86)",
                "ProgramData",
                "$Recycle.Bin",
                "System Volume Information",
                "Recovery",
            ] {
                d.protected
                    .push(ProtectedPath::subtree(join(&drive_root, p)));
            }
            d.protected
                .push(ProtectedPath::exact(join(&drive_root, "Users")));
            for k in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
                if let Some(p) = var(k) {
                    d.protected.push(ProtectedPath::exact(p));
                }
            }
        }
    }
    d
}

/// Home directory from the environment (`HOME`, or `USERPROFILE` on Windows).
pub fn home_from_env() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// A resolved protected root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Root {
    path: PathBuf,
    scope: Scope,
    source: Source,
}

/// A path that could not be resolved when added to a [`ProtectedSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    /// The path as configured.
    pub path: PathBuf,
    /// Why.
    pub reason: String,
}

/// Protected roots, carve-outs and exclusions by file identity. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct ProtectedSet {
    roots: HashMap<FileIdentity, Vec<Root>>,
    allowed: HashMap<FileIdentity, PathBuf>,
    allowed_self: HashMap<FileIdentity, PathBuf>,
    exclusions: HashMap<FileIdentity, PathBuf>,
    /// Resolved paths of roots and exclusions, for the "inside the candidate" check.
    inner: Vec<(PathBuf, Root, bool /* exclusion */)>,
    unresolved: Vec<Unresolved>,
}

/// How [`ProtectedSet::check`] judged one candidate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Protection {
    /// For [`Signals::in_protected_root`](crate::Signals::in_protected_root).
    pub in_protected_root: bool,
    /// For [`Signals::protect_marker`](crate::Signals::protect_marker).
    pub protect_marker: bool,
    /// For [`Signals::user_excluded`](crate::Signals::user_excluded).
    pub user_excluded: bool,
    /// Why: `protected_root`, `protect_marker`, `user_exclusion` and `signal_unavailable`
    /// evidence.
    pub evidence: Vec<Evidence>,
}

impl Protection {
    /// Whether anything protects the candidate (it must be S3).
    pub fn is_protected(&self) -> bool {
        self.in_protected_root || self.protect_marker || self.user_excluded
    }
}

/// Identities a path resolves to: its own (no-follow) and, if it is a link, its target's.
struct Resolved {
    identities: Vec<FileIdentity>,
    canonical: Option<PathBuf>,
}

fn resolve(platform: &dyn Platform, path: &Path) -> Result<Option<Resolved>, String> {
    let md = match platform.metadata(path) {
        Ok(md) => md,
        Err(e) if e.is_not_found() => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let mut identities = Vec::new();
    identities.extend(md.identity);
    let canonical = std::fs::canonicalize(path).ok();
    let is_link = matches!(
        md.kind,
        EntryKind::Symlink | EntryKind::Junction | EntryKind::ReparsePoint
    );
    if is_link && let Some(target) = &canonical {
        match platform.metadata(target) {
            Ok(t) => identities.extend(t.identity),
            Err(e) if e.is_not_found() => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    if identities.is_empty() {
        return Err("this platform reports no file identity".into());
    }
    Ok(Some(Resolved {
        identities,
        canonical,
    }))
}

impl ProtectedSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The built-in roots for this OS, with home and Windows locations from the environment.
    pub fn with_defaults(platform: &dyn Platform) -> Self {
        let home = home_from_env();
        let env = |k: &str| std::env::var_os(k);
        let mut set = Self::new();
        set.add_defaults(
            platform,
            &default_protected(TargetOs::current(), home.as_deref(), &env),
        );
        set
    }

    /// Add built-in roots and carve-outs. Missing paths are skipped.
    pub fn add_defaults(&mut self, platform: &dyn Platform, defaults: &DefaultProtection) {
        for p in &defaults.protected {
            self.protect(platform, &p.path, p.scope, Source::Default);
        }
        for a in &defaults.allowed {
            if let Some(r) = self.resolve_or_record(platform, a) {
                for id in r.identities {
                    self.allowed.insert(id, a.clone());
                }
            }
        }
        for a in &defaults.allowed_self {
            if let Some(r) = self.resolve_or_record(platform, a) {
                for id in r.identities {
                    self.allowed_self.insert(id, a.clone());
                }
            }
        }
    }

    /// Protect `path` with `scope`. Missing paths are skipped (nothing to protect); other
    /// failures are recorded in [`ProtectedSet::unresolved`].
    pub fn protect(&mut self, platform: &dyn Platform, path: &Path, scope: Scope, source: Source) {
        let Some(r) = self.resolve_or_record(platform, path) else {
            return;
        };
        let root = Root {
            path: path.to_path_buf(),
            scope,
            source,
        };
        for id in r.identities {
            self.roots.entry(id).or_default().push(root.clone());
        }
        if let Some(c) = r.canonical {
            self.inner.push((c, root, false));
        }
    }

    /// Exclude `path` and everything inside (user or repository exclusion).
    pub fn exclude(&mut self, platform: &dyn Platform, path: &Path, source: Source) {
        let Some(r) = self.resolve_or_record(platform, path) else {
            return;
        };
        for id in r.identities {
            self.exclusions.insert(id, path.to_path_buf());
        }
        if let Some(c) = r.canonical {
            let root = Root {
                path: path.to_path_buf(),
                scope: Scope::Subtree,
                source,
            };
            self.inner.push((c, root, true));
        }
    }

    /// Add the user's `scan.protect` (subtree) and `scan.exclude` paths. Expand `~` first
    /// (`Config::with_home_expanded`).
    pub fn add_config(&mut self, platform: &dyn Platform, scan: &ScanConfig) {
        for p in &scan.protect {
            self.protect(platform, p, Scope::Subtree, Source::User);
        }
        for p in &scan.exclude {
            self.exclude(platform, p, Source::User);
        }
    }

    /// Add a repository's `.scull.toml` policy for `repo_dir` (CFG-03: protection only).
    pub fn add_repo_policy(
        &mut self,
        platform: &dyn Platform,
        repo_dir: &Path,
        policy: &RepoPolicy,
    ) {
        for p in policy.protect() {
            self.protect(platform, &p.under(repo_dir), Scope::Subtree, Source::Repo);
        }
        for p in policy.exclude() {
            self.exclude(platform, &p.under(repo_dir), Source::Repo);
        }
    }

    /// Paths that could not be resolved when added (for `scull doctor`).
    pub fn unresolved(&self) -> &[Unresolved] {
        &self.unresolved
    }

    fn resolve_or_record(&mut self, platform: &dyn Platform, path: &Path) -> Option<Resolved> {
        match resolve(platform, path) {
            Ok(r) => r,
            Err(reason) => {
                self.unresolved.push(Unresolved {
                    path: path.to_path_buf(),
                    reason,
                });
                None
            }
        }
    }

    /// Whether anything protects `path` (see [`ProtectedSet::check`]).
    pub fn is_protected(&self, path: &Path, platform: &dyn Platform) -> bool {
        self.check(path, platform).is_protected()
    }

    /// Judge one candidate path: walk its real ancestor chain by identity, look for
    /// `.scull-protect` markers, and look for protected entries inside it.
    pub fn check(&self, path: &Path, platform: &dyn Platform) -> Protection {
        let mut out = Protection::default();
        let Some(chain) = real_chain(path) else {
            out.unresolvable(path, "its real path could not be resolved");
            return out;
        };
        // chain[0] is the candidate (final component not followed), then its ancestors.
        let mut ids: Vec<Option<FileIdentity>> = Vec::with_capacity(chain.len());
        let mut kinds = Vec::with_capacity(chain.len());
        for (depth, p) in chain.iter().enumerate() {
            match platform.metadata(p) {
                Ok(md) if md.identity.is_some() => {
                    ids.push(md.identity);
                    kinds.push(md.kind);
                }
                Ok(_) => {
                    out.unresolvable(p, "this platform reports no file identity");
                    return out;
                }
                Err(e) => {
                    let what = if depth == 0 {
                        "the candidate"
                    } else {
                        "an ancestor"
                    };
                    out.unresolvable(p, &format!("cannot read metadata of {what}: {e}"));
                    return out;
                }
            }
        }

        // Filesystem or volume root.
        if chain.len() == 1 {
            out.root(&chain[0]);
        } else if let (Some(Some(me)), Some(Some(parent))) = (ids.first(), ids.get(1))
            && me.volume != parent.volume
        {
            out.root(&chain[0]);
        }

        // Nearest carve-out strictly above the candidate, or the candidate itself when it is a
        // self-inclusive carve-out (#139).
        let carve = if ids
            .first()
            .is_some_and(|id| id.is_some_and(|id| self.allowed_self.contains_key(&id)))
        {
            Some(0)
        } else {
            (1..chain.len()).find(|&d| ids[d].is_some_and(|id| self.allowed.contains_key(&id)))
        };

        for (depth, id) in ids.iter().enumerate() {
            let Some(id) = id else { continue };
            if let Some(roots) = self.roots.get(id) {
                for r in roots {
                    if r.scope == Scope::Exact && depth > 0 {
                        continue;
                    }
                    let cancelled = r.source == Source::Default
                        && depth > 0
                        && carve.is_some_and(|c| c < depth);
                    if !cancelled {
                        out.add_root(r);
                    }
                }
            }
            if let Some(ex) = self.exclusions.get(id) {
                out.excluded(ex);
            }
        }

        // `.scull-protect` in the candidate (if a directory) or any directory above it.
        for (depth, dir) in chain.iter().enumerate() {
            if depth == 0 && kinds[0] != EntryKind::Dir {
                continue;
            }
            let marker = dir.join(PROTECT_MARKER);
            match platform.metadata(&marker) {
                Ok(_) => out.marker(&marker),
                Err(e) if e.is_not_found() => {}
                Err(_) => out.unresolvable(&marker, "cannot check for a protect marker"),
            }
        }

        // Protected entries strictly inside the candidate.
        let me = &chain[0];
        for (inner, root, exclusion) in &self.inner {
            if inner != me && path_starts_with(inner, me) {
                if *exclusion {
                    out.excluded(&root.path);
                } else {
                    out.add_root(root);
                }
            }
        }
        out
    }
}

impl Protection {
    fn add_root(&mut self, r: &Root) {
        let ev = match r.source {
            Source::Default | Source::User => {
                self.in_protected_root = true;
                Evidence::ProtectedRoot {
                    root: FsPath::from_path(&r.path),
                }
            }
            Source::Repo => {
                self.protect_marker = true;
                Evidence::ProtectMarker {
                    path: FsPath::from_path(&r.path),
                }
            }
        };
        self.push(ev);
    }

    fn root(&mut self, p: &Path) {
        self.in_protected_root = true;
        self.push(Evidence::ProtectedRoot {
            root: FsPath::from_path(p),
        });
    }

    fn marker(&mut self, p: &Path) {
        self.protect_marker = true;
        self.push(Evidence::ProtectMarker {
            path: FsPath::from_path(p),
        });
    }

    fn excluded(&mut self, p: &Path) {
        self.user_excluded = true;
        self.push(Evidence::UserExclusion {
            rule: p.display().to_string(),
        });
    }

    /// Protection could not be determined: protective answer, disclosed.
    fn unresolvable(&mut self, p: &Path, why: &str) {
        self.in_protected_root = true;
        self.push(Evidence::SignalUnavailable {
            signal: "protected roots".into(),
            reason: format!("{}: {why}; treated as protected", p.display()),
        });
    }

    fn push(&mut self, e: Evidence) {
        if !self.evidence.contains(&e) {
            self.evidence.push(e);
        }
    }
}

/// The candidate's real path, final component **not** followed, then each real ancestor up to
/// the filesystem root. `None` if the parent cannot be resolved.
fn real_chain(path: &Path) -> Option<Vec<PathBuf>> {
    let real = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            std::fs::canonicalize(parent).ok()?.join(name)
        }
        // A root, or a path ending in `..` / a bare name: resolve it whole.
        _ => std::fs::canonicalize(path).ok()?,
    };
    Some(real.ancestors().map(Path::to_path_buf).collect())
}

/// `path` is `prefix` or inside it, comparing components; ASCII case-insensitively on macOS
/// and Windows, where the default filesystems are case-insensitive (protective: more matches).
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    if path.starts_with(prefix) {
        return true;
    }
    if !cfg!(any(target_os = "macos", windows)) {
        return false;
    }
    let norm = |c: Component<'_>| c.as_os_str().to_string_lossy().to_lowercase();
    let mut p = path.components();
    prefix
        .components()
        .all(|c| p.next().is_some_and(|pc| norm(pc) == norm(c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn has(d: &DefaultProtection, p: &str, scope: Scope) -> bool {
        d.protected
            .iter()
            .any(|x| x.path == Path::new(p) && x.scope == scope)
    }

    #[test]
    fn linux_defaults() {
        let d = default_protected(TargetOs::Linux, Some(Path::new("/home/u")), &no_env);
        assert!(has(&d, "/", Scope::Exact));
        assert!(has(&d, "/home", Scope::Exact));
        assert!(has(&d, "/home/u", Scope::Exact));
        assert!(has(&d, "/home/u/.ssh", Scope::Subtree));
        assert!(has(&d, "/home/u/.gnupg", Scope::Subtree));
        assert!(has(&d, "/etc", Scope::Subtree));
        assert!(has(&d, "/usr", Scope::Subtree));
        assert!(has(&d, "/var", Scope::Subtree));
        assert!(has(&d, "/root", Scope::Subtree));
        assert!(d.allowed.contains(&PathBuf::from("/home/u")));
        let root = default_protected(TargetOs::Linux, Some(Path::new("/root")), &no_env);
        assert!(
            !has(&root, "/root", Scope::Subtree),
            "root's own home stays usable"
        );
    }

    #[test]
    fn macos_defaults_protect_library_with_cache_carve_outs() {
        let d = default_protected(TargetOs::MacOs, Some(Path::new("/Users/u")), &no_env);
        assert!(has(&d, "/System", Scope::Subtree));
        assert!(has(&d, "/Applications", Scope::Subtree));
        assert!(has(&d, "/Users", Scope::Exact));
        assert!(has(&d, "/Users/u/Library", Scope::Subtree));
        assert!(has(
            &d,
            "/Users/u/Library/Mobile Documents/com~apple~CloudDocs",
            Scope::Exact
        ));
        for a in [
            "/Users/u/Library/Caches",
            "/Users/u/Library/Developer/Xcode/DerivedData",
            "/Users/u/Library/pnpm/store",
            "/private/var/folders",
        ] {
            assert!(d.allowed.contains(&PathBuf::from(a)), "{a}");
        }
        // #139: only pnpm's store is carved out, not the rest of ~/Library/pnpm.
        assert!(!d.allowed.contains(&PathBuf::from("/Users/u/Library/pnpm")));
    }

    #[test]
    fn windows_defaults_from_env() {
        let env = |k: &str| -> Option<OsString> {
            Some(
                match k {
                    "SystemDrive" => "D:",
                    "SystemRoot" => r"D:\Windows",
                    "ProgramFiles" => r"D:\Program Files",
                    "ProgramData" => r"D:\ProgramData",
                    "OneDrive" => r"D:\Users\u\OneDrive",
                    _ => return None,
                }
                .into(),
            )
        };
        let d = default_protected(TargetOs::Windows, Some(Path::new(r"D:\Users\u")), &env);
        assert!(has(&d, r"D:\", Scope::Exact));
        assert!(has(&d, r"D:\Windows", Scope::Subtree));
        assert!(has(&d, r"D:\Program Files", Scope::Subtree));
        assert!(has(&d, r"D:\ProgramData", Scope::Subtree));
        assert!(has(&d, r"D:\Users\u\OneDrive", Scope::Exact));
        assert!(has(&d, r"D:\Users\u", Scope::Exact));
        assert!(has(&d, r"D:\Users\u\.ssh", Scope::Subtree));
        let bare = default_protected(TargetOs::Windows, None, &no_env);
        assert!(has(&bare, r"C:\", Scope::Exact));
        assert!(has(&bare, r"C:\Windows", Scope::Subtree));
    }
}
