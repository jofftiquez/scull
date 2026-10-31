//! [`Fixture`]: a fresh temporary root plus a sibling canary directory, and a deterministic
//! builder API that can only write inside that temporary directory.

use crate::error::{FixtureError, Result};
use crate::invariants::{InvariantInputs, InvariantReport};
use crate::os;
use crate::snapshot::TreeSnapshot;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

/// The canary directory that sits **beside** (never inside) every fixture root (invariant I-2).
///
/// Layout, relative to [`Canary::dir`]:
///
/// ```text
/// canary/
///   canary.txt                  plain file
///   nested/deeper/data.bin      file in a nested directory
///   symlink-target/             directory fixtures point symlinks at
///     keep.txt
///   symlink-target-file.txt     file fixtures point file symlinks / hardlinks at
///   junction-target/            directory fixtures point Windows junctions at
///     keep.txt                  (created on every OS for a uniform layout)
/// ```
#[derive(Debug, Clone)]
pub struct Canary {
    /// The canary directory itself.
    pub dir: PathBuf,
    /// `canary.txt`.
    pub file: PathBuf,
    /// `nested/deeper/data.bin`.
    pub nested_file: PathBuf,
    /// `symlink-target/`: a directory for fixtures to aim directory symlinks at.
    pub symlink_target_dir: PathBuf,
    /// `symlink-target-file.txt`: a file for fixtures to aim file symlinks or hardlinks at.
    pub symlink_target_file: PathBuf,
    /// `junction-target/`: a directory for fixtures to aim Windows junctions at.
    pub junction_target: PathBuf,
}

/// Result of a link-creation step that may be unsupported on the current OS or privilege level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The link was created.
    Created,
    /// The link could not be created here (e.g. Windows without symlink privilege, or a junction
    /// on a non-Windows OS). The step is also recorded in [`Fixture::skipped`].
    Skipped(String),
}

impl LinkOutcome {
    /// Whether the link exists.
    pub fn is_created(&self) -> bool {
        matches!(self, Self::Created)
    }
}

/// A builder step that was skipped, with the reason (reported, never silently ignored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The step, e.g. `symlink a/link -> ../b`.
    pub step: String,
    /// Why it was skipped.
    pub reason: String,
}

/// A generated fixture: a fresh temporary directory holding the fixture **root** and a sibling
/// **canary** directory.
///
/// ```text
/// <tmp>/scull-fx-XXXXXX/
///   root/       the tree under test; all builder paths are relative to this
///   canary/     see [`Canary`]; snapshotted at creation for I-2
///   git-home/   isolated $HOME for git (created on first use)
/// ```
///
/// Builder methods take paths **relative to the root**. Absolute paths, `..`, drive prefixes
/// and paths that would be written through an existing symlink or junction are refused with
/// [`FixtureError::UnsafePath`], so the harness never writes outside its temporary directory.
///
/// Every fixture must have its invariants checked ([`Fixture::check_invariants`]) before it is
/// dropped; dropping an unchecked fixture panics (unless the thread is already panicking), so
/// "invariants run after every fixture" (docs/FIXTURES.md §1) is enforced, not hoped for.
#[derive(Debug)]
pub struct Fixture {
    base: PathBuf,
    root: PathBuf,
    canary: Canary,
    canary_baseline: TreeSnapshot,
    root_baseline: Option<TreeSnapshot>,
    skipped: Vec<Skipped>,
    checked: Cell<bool>,
    // Declared last so the directory is removed after everything else is dropped.
    _tmp: TempDir,
}

impl Fixture {
    /// Create a fresh fixture: an empty root and a populated canary, whose baseline snapshot is
    /// taken immediately.
    pub fn new() -> Result<Self> {
        let tmp = tempfile::Builder::new()
            .prefix("scull-fx-")
            .tempdir()
            .map_err(|e| FixtureError::io("create temp dir", std::env::temp_dir(), e))?;
        // Resolve e.g. macOS /var -> /private/var so paths compare equal to what a scanner sees.
        // Keep the original if canonicalization yields a Windows verbatim (\\?\) path, which
        // some tools (cmd, older git) mishandle.
        let base = match fs::canonicalize(tmp.path()) {
            Ok(p) if !p.to_string_lossy().starts_with(r"\\?\") => p,
            _ => tmp.path().to_path_buf(),
        };
        let root = base.join("root");
        mkdir(&root)?;
        let canary = build_canary(&base.join("canary"))?;
        let canary_baseline = TreeSnapshot::capture(&canary.dir)?;
        Ok(Self {
            base,
            root,
            canary,
            canary_baseline,
            root_baseline: None,
            skipped: Vec::new(),
            checked: Cell::new(false),
            _tmp: tmp,
        })
    }

    /// The fixture root: the tree under test.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The temporary directory containing the root, the canary and the git home.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The canary directory beside the root.
    pub fn canary(&self) -> &Canary {
        &self.canary
    }

    /// Snapshot of the canary taken when the fixture was created (I-2 baseline).
    pub fn canary_baseline(&self) -> &TreeSnapshot {
        &self.canary_baseline
    }

    /// Snapshot of the root taken by [`Fixture::checkpoint`] (I-3 baseline), if any.
    pub fn root_baseline(&self) -> Option<&TreeSnapshot> {
        self.root_baseline.as_ref()
    }

    /// Builder steps skipped on this OS / privilege level.
    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }

    /// The absolute path of `rel` inside the root, after validating that `rel` is a plain
    /// relative path (no root, prefix, `.` or `..` components, not empty).
    pub fn path(&self, rel: impl AsRef<Path>) -> Result<PathBuf> {
        let rel = rel.as_ref();
        validate_relative(rel)?;
        Ok(self.root.join(rel))
    }

    /// Like [`Fixture::path`], and additionally refuses if any existing component of the path
    /// (including the last) is a symlink or junction, so writing there cannot escape the root.
    fn writable_path(&self, rel: &Path) -> Result<PathBuf> {
        validate_relative(rel)?;
        let mut cur = self.root.clone();
        for c in rel.components() {
            cur.push(c);
            if let Ok(m) = fs::symlink_metadata(&cur)
                && m.file_type().is_symlink()
            {
                return Err(FixtureError::unsafe_path(
                    rel,
                    "an existing component is a symlink or junction; writing would follow it",
                ));
            }
        }
        Ok(cur)
    }

    /// Create a directory (and missing parents).
    pub fn dir(&mut self, rel: impl AsRef<Path>) -> Result<&mut Self> {
        let p = self.writable_path(rel.as_ref())?;
        mkdir(&p)?;
        Ok(self)
    }

    /// Create (or overwrite) a file with `contents`, creating parent directories.
    pub fn file(&mut self, rel: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<&mut Self> {
        let p = self.prepare_file(rel.as_ref())?;
        fs::write(&p, contents.as_ref()).map_err(|e| FixtureError::io("write file", &p, e))?;
        Ok(self)
    }

    /// Create (or overwrite) a file of exactly `size` bytes of deterministic, path-seeded
    /// content (the same `rel` and `size` always give the same bytes).
    pub fn file_of_size(&mut self, rel: impl AsRef<Path>, size: u64) -> Result<&mut Self> {
        let rel = rel.as_ref();
        let p = self.prepare_file(rel)?;
        let seed = {
            let d = Sha256::digest(rel.to_string_lossy().as_bytes());
            u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
        };
        let mut f = fs::File::create(&p).map_err(|e| FixtureError::io("create file", &p, e))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut offset: u64 = 0;
        while offset < size {
            let n = usize::try_from((size - offset).min(buf.len() as u64)).unwrap_or(buf.len());
            for (i, b) in buf[..n].iter_mut().enumerate() {
                let x = (offset + i as u64).wrapping_mul(31).wrapping_add(seed) % 251;
                *b = u8::try_from(x).unwrap_or(0);
            }
            f.write_all(&buf[..n])
                .map_err(|e| FixtureError::io("write file", &p, e))?;
            offset += n as u64;
        }
        f.sync_all()
            .map_err(|e| FixtureError::io("sync file", &p, e))?;
        Ok(self)
    }

    /// Create a file with `contents` and set its modification time to `mtime`.
    pub fn file_with_mtime(
        &mut self,
        rel: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
        mtime: SystemTime,
    ) -> Result<&mut Self> {
        let rel = rel.as_ref();
        self.file(rel, contents)?;
        self.set_mtime(rel, mtime)
    }

    /// Set the modification time of an existing **file** (via `File::set_modified`).
    pub fn set_mtime(&mut self, rel: impl AsRef<Path>, mtime: SystemTime) -> Result<&mut Self> {
        let p = self.writable_path(rel.as_ref())?;
        let f = fs::File::options()
            .write(true)
            .open(&p)
            .map_err(|e| FixtureError::io("open for set_modified", &p, e))?;
        f.set_modified(mtime)
            .map_err(|e| FixtureError::io("set_modified", &p, e))?;
        Ok(self)
    }

    /// Set the modification time of `rel` and of **everything beneath it** (files, directories
    /// and the links themselves, never their targets) to `mtime`, deepest first, so that a
    /// directory's own mtime isn't refreshed after it was set.
    ///
    /// Use it after building a subtree that should look idle: creating entries updates every
    /// directory's mtime, and scanners (rightly) count directory mtimes as activity.
    ///
    /// A hard-linked file shares its time with every other link: don't age a subtree holding a
    /// link to a file outside the root (age it before linking, then [`Fixture::age_one`] the
    /// directory).
    pub fn age(&mut self, rel: impl AsRef<Path>, mtime: SystemTime) -> Result<&mut Self> {
        let top = self.writable_path(rel.as_ref())?;
        // Links are aged by copying the time of a reference file (Unix `touch -h -r`). It lives
        // beside the root, never inside it or the canary.
        let reference = self.base.join(".mtime-reference");
        fs::write(&reference, b"").map_err(|e| FixtureError::io("write", &reference, e))?;
        fs::File::options()
            .write(true)
            .open(&reference)
            .and_then(|f| f.set_modified(mtime))
            .map_err(|e| FixtureError::io("set_modified", &reference, e))?;

        // Post-order: children before their directory.
        let mut order = Vec::new();
        let mut stack = vec![(top, false)];
        while let Some((p, expanded)) = stack.pop() {
            let ft = fs::symlink_metadata(&p)
                .map_err(|e| FixtureError::io("lstat", &p, e))?
                .file_type();
            if ft.is_dir() && !expanded {
                stack.push((p.clone(), true));
                for e in fs::read_dir(&p).map_err(|e| FixtureError::io("read_dir", &p, e))? {
                    let e = e.map_err(|e| FixtureError::io("read_dir", &p, e))?;
                    stack.push((e.path(), false));
                }
            } else {
                order.push((p, ft));
            }
        }
        for (p, ft) in order {
            os::set_mtime_nofollow(&p, ft, mtime, &reference)
                .map_err(|e| FixtureError::io("set mtime (no-follow)", &p, e))?;
        }
        let _ = fs::remove_file(&reference);
        Ok(self)
    }

    /// Like [`Fixture::age`] for `rel` itself only (a directory's contents are left alone).
    /// Use it where the subtree holds a hard link to a file outside the root (e.g. the canary):
    /// setting that file's time through the link would change the canary too.
    pub fn age_one(&mut self, rel: impl AsRef<Path>, mtime: SystemTime) -> Result<&mut Self> {
        let p = self.writable_path(rel.as_ref())?;
        let ft = fs::symlink_metadata(&p)
            .map_err(|e| FixtureError::io("lstat", &p, e))?
            .file_type();
        let reference = self.base.join(".mtime-reference");
        fs::write(&reference, b"").map_err(|e| FixtureError::io("write", &reference, e))?;
        fs::File::options()
            .write(true)
            .open(&reference)
            .and_then(|f| f.set_modified(mtime))
            .map_err(|e| FixtureError::io("set_modified", &reference, e))?;
        os::set_mtime_nofollow(&p, ft, mtime, &reference)
            .map_err(|e| FixtureError::io("set mtime (no-follow)", &p, e))?;
        let _ = fs::remove_file(&reference);
        Ok(self)
    }

    /// Create a symlink at `link` (relative to the root) pointing to `target`, which is stored
    /// verbatim: relative targets resolve against the link's parent, absolute targets (e.g.
    /// [`Canary::symlink_target_dir`]) are allowed because creating a link never modifies its
    /// target. Where symlinks are not permitted (Windows without Developer Mode or admin) the
    /// step is skipped and recorded rather than failed.
    pub fn symlink(
        &mut self,
        link: impl AsRef<Path>,
        target: impl AsRef<Path>,
    ) -> Result<LinkOutcome> {
        let (link, target) = (link.as_ref(), target.as_ref());
        let p = self.prepare_file(link)?;
        let resolved = match p.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target.to_path_buf(),
        };
        let target_is_dir = fs::metadata(&resolved).map(|m| m.is_dir()).unwrap_or(false);
        match os::symlink(target, &p, target_is_dir) {
            Ok(()) => Ok(LinkOutcome::Created),
            Err(e) if os::is_symlink_unsupported(&e) => Ok(self.skip(
                format!("symlink {} -> {}", link.display(), target.display()),
                format!("symlinks not permitted here: {e}"),
            )),
            Err(e) => Err(FixtureError::io("create symlink", &p, e)),
        }
    }

    /// Create a Windows junction at `link` (relative to the root) pointing to the absolute
    /// directory `target`. On other OSs the step is skipped and recorded.
    pub fn junction(
        &mut self,
        link: impl AsRef<Path>,
        target: impl AsRef<Path>,
    ) -> Result<LinkOutcome> {
        let (link, target) = (link.as_ref(), target.as_ref());
        let p = self.prepare_file(link)?;
        match os::junction(target, &p) {
            Ok(()) => Ok(LinkOutcome::Created),
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => Ok(self.skip(
                format!("junction {} -> {}", link.display(), target.display()),
                e.to_string(),
            )),
            Err(e) => Err(FixtureError::io("create junction", &p, e)),
        }
    }

    /// Create a hardlink at `link` (relative to the root) to `original`. A relative `original`
    /// is resolved against the root; an absolute one must lie inside this fixture's temporary
    /// directory (e.g. a canary file, for FX-FS-05).
    pub fn hardlink(
        &mut self,
        link: impl AsRef<Path>,
        original: impl AsRef<Path>,
    ) -> Result<&mut Self> {
        let original = original.as_ref();
        let src = if original.is_absolute() {
            if !original.starts_with(&self.base) {
                return Err(FixtureError::unsafe_path(
                    original,
                    "hardlink source must be inside the fixture's temporary directory",
                ));
            }
            original.to_path_buf()
        } else {
            self.path(original)?
        };
        let p = self.prepare_file(link.as_ref())?;
        fs::hard_link(&src, &p).map_err(|e| FixtureError::io("create hardlink", &p, e))?;
        Ok(self)
    }

    /// Finish building: record the root baseline used by I-3. Call after the last builder step
    /// and before running the code under test.
    pub fn checkpoint(&mut self) -> Result<&mut Self> {
        self.root_baseline = Some(self.snapshot_root()?);
        Ok(self)
    }

    /// A fresh snapshot of the root.
    pub fn snapshot_root(&self) -> Result<TreeSnapshot> {
        TreeSnapshot::capture(&self.root)
    }

    /// A fresh snapshot of the canary directory.
    pub fn snapshot_canary(&self) -> Result<TreeSnapshot> {
        TreeSnapshot::capture(&self.canary.dir)
    }

    /// Run every implemented invariant and report which ran (see
    /// [`crate::check_invariants`]). Marks the fixture as checked.
    pub fn check_invariants(&self, inputs: &InvariantInputs) -> InvariantReport {
        crate::invariants::check_invariants(self, inputs)
    }

    pub(crate) fn mark_checked(&self) {
        self.checked.set(true);
    }

    pub(crate) fn git_home(&self) -> Result<PathBuf> {
        let home = self.base.join("git-home");
        mkdir(&home)?;
        Ok(home)
    }

    fn prepare_file(&self, rel: &Path) -> Result<PathBuf> {
        let p = self.writable_path(rel)?;
        if let Some(parent) = p.parent() {
            mkdir(parent)?;
        }
        Ok(p)
    }

    fn skip(&mut self, step: String, reason: String) -> LinkOutcome {
        self.skipped.push(Skipped {
            step,
            reason: reason.clone(),
        });
        LinkOutcome::Skipped(reason)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.checked.get() && !std::thread::panicking() {
            panic!(
                "fixture {} dropped without running invariants; call `check_invariants` \
                 before the end of the test (docs/FIXTURES.md §1)",
                self.base.display()
            );
        }
    }
}

/// `now - d` (or the Unix epoch if that underflows). Handy for activity-window fixtures.
pub fn ago(d: Duration) -> SystemTime {
    SystemTime::now()
        .checked_sub(d)
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// `now - days * 24h`.
pub fn days_ago(days: u64) -> SystemTime {
    ago(Duration::from_secs(days.saturating_mul(24 * 60 * 60)))
}

fn validate_relative(rel: &Path) -> Result<()> {
    if rel.as_os_str().is_empty() {
        return Err(FixtureError::unsafe_path(rel, "empty path"));
    }
    for c in rel.components() {
        match c {
            Component::Normal(_) => {}
            Component::ParentDir => {
                return Err(FixtureError::unsafe_path(rel, "`..` is not allowed"));
            }
            Component::CurDir => {
                return Err(FixtureError::unsafe_path(rel, "`.` is not allowed"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(FixtureError::unsafe_path(
                    rel,
                    "must be relative to the fixture root",
                ));
            }
        }
    }
    Ok(())
}

fn mkdir(p: &Path) -> Result<()> {
    fs::create_dir_all(p).map_err(|e| FixtureError::io("create dir", p, e))
}

fn write(p: &Path, contents: &[u8]) -> Result<()> {
    fs::write(p, contents).map_err(|e| FixtureError::io("write file", p, e))
}

fn build_canary(dir: &Path) -> Result<Canary> {
    let c = Canary {
        dir: dir.to_path_buf(),
        file: dir.join("canary.txt"),
        nested_file: dir.join("nested").join("deeper").join("data.bin"),
        symlink_target_dir: dir.join("symlink-target"),
        symlink_target_file: dir.join("symlink-target-file.txt"),
        junction_target: dir.join("junction-target"),
    };
    mkdir(&dir.join("nested").join("deeper"))?;
    mkdir(&c.symlink_target_dir)?;
    mkdir(&c.junction_target)?;
    write(&c.file, b"scull canary: must never change\n")?;
    write(&c.nested_file, &[0u8, 1, 2, 3, 0xfe, 0xff])?;
    write(
        &c.symlink_target_dir.join("keep.txt"),
        b"symlink target content\n",
    )?;
    write(&c.symlink_target_file, b"file symlink / hardlink target\n")?;
    write(
        &c.junction_target.join("keep.txt"),
        b"junction target content\n",
    )?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_plain_relative_paths() {
        for bad in ["", ".", "..", "a/../b", "./a"] {
            assert!(validate_relative(Path::new(bad)).is_err(), "{bad:?}");
        }
        assert!(validate_relative(&std::env::temp_dir()).is_err());
        assert!(validate_relative(Path::new("a/b/c.txt")).is_ok());
    }
}
