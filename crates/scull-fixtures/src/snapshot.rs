//! Full, no-follow snapshots of a directory tree and diffs between them (invariants I-2, I-3).

use crate::error::{FixtureError, Result};
use crate::os;
use scull_core::FileIdentity;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What kind of object an entry is. Symlinks (and, on Windows, junctions) are recorded with
/// their target and **never followed**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link or other name-surrogate reparse point (junction), with its raw target.
    Symlink {
        /// The link target as stored in the link (not resolved).
        target: PathBuf,
    },
    /// FIFO, socket, device node or anything else. Never opened.
    Other,
}

/// One object in a [`TreeSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Kind, including the target of a symlink.
    pub kind: EntryKind,
    /// Logical size in bytes for files and symlinks; `0` for directories (whose reported size is
    /// filesystem-specific noise).
    pub size: u64,
    /// Lowercase hex SHA-256 of a regular file's content; `None` for other kinds or if the file
    /// could not be read (see [`Entry::error`]).
    pub sha256: Option<String>,
    /// Modification time, where the platform reports one.
    pub mtime: Option<SystemTime>,
    /// Device + inode on Unix. `None` on Windows until #32 (P1-PLAT-02): stable `std` does not
    /// expose the volume serial number or file ID.
    pub identity: Option<FileIdentity>,
    /// Read-only attribute / no write bits.
    pub readonly: bool,
    /// Permission bits (`mode & 0o7777`) on Unix; `None` elsewhere.
    pub mode: Option<u32>,
    /// Error reading this entry's content or listing this directory, if any. The entry is still
    /// recorded so that a later diff notices if it changes.
    pub error: Option<String>,
}

/// A field that differs between two snapshots of the same path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangedField {
    /// Kind or symlink target.
    Kind,
    /// Logical size.
    Size,
    /// Content hash.
    Content,
    /// Modification time (files and symlinks only; see [`TreeSnapshot::diff`]).
    Mtime,
    /// File identity: the object at this path was replaced.
    Identity,
    /// Read-only flag or permission bits.
    Permissions,
    /// Readability (an error appeared or went away).
    Readability,
}

impl fmt::Display for ChangedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Kind => "kind",
            Self::Size => "size",
            Self::Content => "content",
            Self::Mtime => "mtime",
            Self::Identity => "identity",
            Self::Permissions => "permissions",
            Self::Readability => "readability",
        })
    }
}

/// A path present in both snapshots whose entry differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryChange {
    /// Path relative to the snapshot root.
    pub path: PathBuf,
    /// Which fields differ.
    pub fields: Vec<ChangedField>,
    /// The entry before.
    pub before: Entry,
    /// The entry after.
    pub after: Entry,
}

/// Differences between two snapshots. Paths are relative to the snapshot root; the root itself
/// is the empty path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeDiff {
    /// Paths only in the later snapshot.
    pub added: Vec<PathBuf>,
    /// Paths only in the earlier snapshot.
    pub removed: Vec<PathBuf>,
    /// Paths in both whose entries differ.
    pub changed: Vec<EntryChange>,
}

impl TreeDiff {
    /// No differences at all.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// Every path that differs (added, removed or changed), sorted.
    pub fn paths(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = self
            .added
            .iter()
            .chain(&self.removed)
            .map(PathBuf::as_path)
            .chain(self.changed.iter().map(|c| c.path.as_path()))
            .collect();
        v.sort();
        v
    }

    /// The diff with every path equal to, or below, one of `allowed` removed.
    /// `allowed` paths are relative to the snapshot root.
    pub fn without<P: AsRef<Path>>(&self, allowed: &[P]) -> TreeDiff {
        let ok = |p: &Path| allowed.iter().any(|a| p.starts_with(a.as_ref()));
        TreeDiff {
            added: self.added.iter().filter(|p| !ok(p)).cloned().collect(),
            removed: self.removed.iter().filter(|p| !ok(p)).cloned().collect(),
            changed: self
                .changed
                .iter()
                .filter(|c| !ok(&c.path))
                .cloned()
                .collect(),
        }
    }
}

impl fmt::Display for TreeDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("(no differences)");
        }
        let show = |p: &Path| {
            if p.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                p.display().to_string()
            }
        };
        for p in &self.added {
            writeln!(f, "  + {}", show(p))?;
        }
        for p in &self.removed {
            writeln!(f, "  - {}", show(p))?;
        }
        for c in &self.changed {
            let fields: Vec<String> = c.fields.iter().map(ToString::to_string).collect();
            writeln!(f, "  ~ {} ({})", show(&c.path), fields.join(", "))?;
        }
        Ok(())
    }
}

/// A complete record of a tree: every entry's relative path, kind, size, content hash, mtime,
/// identity and permissions. Symlinks and junctions are recorded, never followed; special files
/// are never opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSnapshot {
    root: PathBuf,
    entries: BTreeMap<PathBuf, Entry>,
}

impl TreeSnapshot {
    /// Snapshot the tree at `root` (which is itself recorded under the empty relative path).
    pub fn capture(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let mut entries = BTreeMap::new();
        let mut stack = vec![PathBuf::new()];
        while let Some(rel) = stack.pop() {
            let abs = if rel.as_os_str().is_empty() {
                root.clone()
            } else {
                root.join(&rel)
            };
            let meta = fs::symlink_metadata(&abs)
                .map_err(|e| FixtureError::io("stat (no follow)", &abs, e))?;
            let mut entry = entry_from_meta(&abs, &meta);
            if entry.kind == EntryKind::Dir {
                match fs::read_dir(&abs) {
                    Ok(rd) => {
                        for child in rd {
                            match child {
                                Ok(c) => stack.push(rel.join(c.file_name())),
                                Err(e) => entry.error = Some(format!("read_dir: {e}")),
                            }
                        }
                    }
                    Err(e) => entry.error = Some(format!("read_dir: {e}")),
                }
            }
            entries.insert(rel, entry);
        }
        Ok(Self { root, entries })
    }

    /// The absolute root this snapshot was taken of.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// All entries, keyed by path relative to the root (the root itself is the empty path).
    pub fn entries(&self) -> &BTreeMap<PathBuf, Entry> {
        &self.entries
    }

    /// The entry at `rel`, if present.
    pub fn get(&self, rel: impl AsRef<Path>) -> Option<&Entry> {
        self.entries.get(rel.as_ref())
    }

    /// Number of entries, including the root.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the snapshot is empty (never true for a successful capture: the root is always
    /// recorded).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Differences from `self` (before) to `after`.
    ///
    /// Compared per path: kind (and symlink target), size, content hash, identity, read-only
    /// flag / mode, readability, and mtime **for non-directories**. Directory mtimes are recorded
    /// but not compared: they change whenever a child is added or removed, which already shows up
    /// as an added/removed path, and comparing them would make every permitted removal also flag
    /// its parent.
    pub fn diff(&self, after: &TreeSnapshot) -> TreeDiff {
        diff(self, after)
    }
}

/// Differences from `before` to `after`. See [`TreeSnapshot::diff`].
pub fn diff(before: &TreeSnapshot, after: &TreeSnapshot) -> TreeDiff {
    let mut d = TreeDiff::default();
    for (path, b) in &before.entries {
        match after.entries.get(path) {
            None => d.removed.push(path.clone()),
            Some(a) => {
                let fields = changed_fields(b, a);
                if !fields.is_empty() {
                    d.changed.push(EntryChange {
                        path: path.clone(),
                        fields,
                        before: b.clone(),
                        after: a.clone(),
                    });
                }
            }
        }
    }
    for path in after.entries.keys() {
        if !before.entries.contains_key(path) {
            d.added.push(path.clone());
        }
    }
    d
}

fn changed_fields(b: &Entry, a: &Entry) -> Vec<ChangedField> {
    let mut v = Vec::new();
    if b.kind != a.kind {
        v.push(ChangedField::Kind);
    }
    if b.size != a.size {
        v.push(ChangedField::Size);
    }
    if b.sha256 != a.sha256 {
        v.push(ChangedField::Content);
    }
    let is_dir = b.kind == EntryKind::Dir && a.kind == EntryKind::Dir;
    if !is_dir && b.mtime != a.mtime {
        v.push(ChangedField::Mtime);
    }
    if b.identity != a.identity {
        v.push(ChangedField::Identity);
    }
    if b.readonly != a.readonly || b.mode != a.mode {
        v.push(ChangedField::Permissions);
    }
    if b.error.is_some() != a.error.is_some() {
        v.push(ChangedField::Readability);
    }
    v
}

fn entry_from_meta(abs: &Path, meta: &fs::Metadata) -> Entry {
    let ft = meta.file_type();
    let mut error = None;
    let kind = if ft.is_symlink() {
        match fs::read_link(abs) {
            Ok(target) => EntryKind::Symlink { target },
            Err(e) => {
                error = Some(format!("read_link: {e}"));
                EntryKind::Symlink {
                    target: PathBuf::new(),
                }
            }
        }
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    let sha256 = if kind == EntryKind::File {
        match hash_file(abs) {
            Ok(h) => Some(h),
            Err(e) => {
                error = Some(format!("read: {e}"));
                None
            }
        }
    } else {
        None
    };
    Entry {
        size: if kind == EntryKind::Dir {
            0
        } else {
            meta.len()
        },
        kind,
        sha256,
        mtime: meta.modified().ok(),
        identity: os::identity(meta),
        readonly: meta.permissions().readonly(),
        mode: os::mode(meta),
        error,
    }
}

/// Lowercase hex SHA-256 of a file's content.
pub(crate) fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0xf)]));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn without_filters_descendants_only() {
        let d = TreeDiff {
            added: vec![PathBuf::from("a/target/x"), PathBuf::from("a/targets")],
            removed: vec![PathBuf::from("a/target")],
            changed: vec![],
        };
        let f = d.without(&["a/target"]);
        assert_eq!(f.added, vec![PathBuf::from("a/targets")]);
        assert!(f.removed.is_empty());
    }
}
