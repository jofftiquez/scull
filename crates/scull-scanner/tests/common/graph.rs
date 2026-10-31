//! An in-memory filesystem *graph* behind the [`Platform`] trait, for conditions a temp dir
//! can't produce: directory hard links and bind mounts that make real cycles (the same
//! directory reachable below itself), device nodes (need root to create), and Windows AF_UNIX
//! socket reparse points, and cloud-sync placeholders (macOS `SF_DATALESS`, Windows recall /
//! offline attributes; [`Graph::set_cloud`]).
//!
//! It is deliberately adversarial: like an OS, it resolves links in the middle of a path, and
//! its `read_dir` *follows* a link at the end of the path instead of refusing. So a scanner
//! that ever asked to list a link, list a special file, or look at a path beneath a link would
//! get an answer, and the attempt is recorded in [`Graph::violations`]. Listing a special
//! file is what "opening" it means at this layer (SAFE-18). Likewise, listing a cloud
//! placeholder directory (which would hydrate it, PLT-04) or touching anything beneath one is
//! answered and recorded as a violation.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // test helpers: panics are failures

use scull_core::FileIdentity;
use scull_platform::{
    Capabilities, DirEntry, DirIter, EntryKind, Metadata, Native, Operation, Platform,
    PlatformError,
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Windows `IO_REPARSE_TAG_AF_UNIX`: the reparse tag of an AF_UNIX socket file.
pub const IO_REPARSE_TAG_AF_UNIX: u32 = 0x8000_0023;

/// macOS `SF_DATALESS` (`st_flags`): content evicted to the cloud.
pub const SF_DATALESS: u32 = 0x4000_0000;
/// Windows `FILE_ATTRIBUTE_DIRECTORY`.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// Windows `FILE_ATTRIBUTE_OFFLINE`.
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
/// Windows `FILE_ATTRIBUTE_RECALL_ON_OPEN`: cloud directory whose children aren't populated.
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
/// Windows `FILE_ATTRIBUTE_PINNED`: "Always keep on this device" (cloud-managed, local).
pub const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
/// Windows `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: dehydrated file placeholder.
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Cloud-related metadata bits of one node, as an OS would report them.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cloud {
    /// Windows `FILE_ATTRIBUTE_*` bits.
    pub file_attributes: Option<u32>,
    /// macOS `st_flags`.
    pub bsd_flags: Option<u32>,
}

impl Cloud {
    /// Whether these bits mark content that is not on disk.
    pub fn is_placeholder(self) -> bool {
        let win = FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
        self.file_attributes.is_some_and(|a| a & win != 0)
            || self.bsd_flags.is_some_and(|f| f & SF_DATALESS != 0)
    }
}

/// Volume reported in every identity of the graph.
const VOLUME: u64 = 7;

/// One node of the graph. A node can have several names (several parents, or the same parent
/// twice): that is a directory hard link / bind mount when the node is a directory.
#[derive(Debug, Clone)]
pub enum Node {
    /// A real directory and its children `(name, node)`.
    Dir(Vec<(String, usize)>),
    /// A regular file of this many bytes.
    File(u64),
    /// A symlink, junction or other reparse point (`kind`) pointing at a node.
    Link(EntryKind, usize),
    /// A FIFO, socket or device node, with an optional Windows reparse tag.
    Special(EntryKind, Option<u32>),
}

/// The graph and a record of every listing the scanner asked for.
pub struct Graph {
    root: PathBuf,
    nodes: Vec<Node>,
    /// Cloud bits per node id (absent: none).
    cloud: std::collections::HashMap<usize, Cloud>,
    md: Metadata,
    de: DirEntry,
    _tmp: tempfile::TempDir,
    reads: Mutex<Vec<PathBuf>>,
    violations: Mutex<Vec<String>>,
}

/// What a path resolves to.
struct Resolved {
    node: usize,
    /// Node ids of every directory walked through, root first, then the final node.
    chain: Vec<usize>,
    /// A link was resolved on the way (or at the end, when following).
    via_link: bool,
}

#[derive(Debug)]
enum Miss {
    NotFound,
    NotADirectory(EntryKind),
}

impl Graph {
    /// A graph with an empty root directory (node 0).
    pub fn new() -> Self {
        // `Metadata` and `DirEntry` are non-exhaustive: take real ones as templates and
        // overwrite every public field.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("t"), b"").unwrap();
        let md = Native.metadata(&tmp.path().join("t")).unwrap();
        let de = Native
            .read_dir(tmp.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        Self {
            root: tmp.path().join("graph"),
            nodes: vec![Node::Dir(Vec::new())],
            cloud: std::collections::HashMap::new(),
            md,
            de,
            _tmp: tmp,
            reads: Mutex::default(),
            violations: Mutex::default(),
        }
    }

    /// Path of the root directory (node 0). Nothing exists there on disk.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Add `node` as `name` in directory `parent`; returns its id.
    pub fn add(&mut self, parent: usize, name: &str, node: Node) -> usize {
        self.nodes.push(node);
        let id = self.nodes.len() - 1;
        self.alias(parent, name, id);
        id
    }

    /// A new empty directory.
    pub fn dir(&mut self, parent: usize, name: &str) -> usize {
        self.add(parent, name, Node::Dir(Vec::new()))
    }

    /// Give the existing node `id` another name in `parent` (a directory hard link or bind
    /// mount when `id` is a directory).
    pub fn alias(&mut self, parent: usize, name: &str, id: usize) {
        match &mut self.nodes[parent] {
            Node::Dir(children) => children.push((name.to_owned(), id)),
            other => panic!("parent {parent} is not a directory: {other:?}"),
        }
    }

    /// Report cloud bits for node `id` (e.g. make it a dataless or recall-on-open placeholder).
    pub fn set_cloud(&mut self, id: usize, cloud: Cloud) {
        self.cloud.insert(id, cloud);
    }

    fn is_placeholder(&self, id: usize) -> bool {
        self.cloud.get(&id).is_some_and(|c| c.is_placeholder())
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// The node `id`.
    pub fn node(&self, id: usize) -> &Node {
        &self.nodes[id]
    }

    /// Paths the scanner listed, in call order.
    pub fn reads(&self) -> Vec<PathBuf> {
        self.reads.lock().unwrap().clone()
    }

    /// Every forbidden request the scanner made (empty for a correct scanner).
    pub fn violations(&self) -> Vec<String> {
        self.violations.lock().unwrap().clone()
    }

    /// Whether `path` lies beneath a link (or a special file), i.e. reaching it would mean
    /// following one. `None` if the path doesn't exist in the graph.
    pub fn beneath_link(&self, path: &Path) -> Option<bool> {
        self.resolve(path, false).ok().map(|r| r.via_link)
    }

    /// Node ids along `path` (root first), resolving links like an OS would.
    pub fn chain(&self, path: &Path) -> Option<Vec<usize>> {
        self.resolve(path, false).ok().map(|r| r.chain)
    }

    fn violation(&self, what: String) {
        self.violations.lock().unwrap().push(what);
    }

    /// Follow links starting at `id`; `Err` on a link cycle (ELOOP).
    fn follow(&self, mut id: usize) -> Result<(usize, bool), Miss> {
        let mut followed = false;
        for _ in 0..40 {
            match self.nodes[id] {
                Node::Link(_, target) => {
                    id = target;
                    followed = true;
                }
                _ => return Ok((id, followed)),
            }
        }
        Err(Miss::NotFound)
    }

    fn resolve(&self, path: &Path, follow_last: bool) -> Result<Resolved, Miss> {
        let rel = path.strip_prefix(&self.root).map_err(|_| Miss::NotFound)?;
        let mut cur = 0;
        let mut chain = vec![0];
        let mut via_link = false;
        for comp in rel.components() {
            let name = comp.as_os_str().to_string_lossy();
            let (dir, followed) = self.follow(cur)?;
            via_link |= followed;
            let Node::Dir(children) = &self.nodes[dir] else {
                return Err(Miss::NotADirectory(self.kind(dir)));
            };
            cur = children
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, id)| *id)
                .ok_or(Miss::NotFound)?;
            chain.push(cur);
        }
        if follow_last {
            let (end, followed) = self.follow(cur)?;
            via_link |= followed;
            cur = end;
        }
        Ok(Resolved {
            node: cur,
            chain,
            via_link,
        })
    }

    fn kind(&self, id: usize) -> EntryKind {
        match self.nodes[id] {
            Node::Dir(_) => EntryKind::Dir,
            Node::File(_) => EntryKind::File,
            Node::Link(k, _) | Node::Special(k, _) => k,
        }
    }

    fn reparse_tag(&self, id: usize) -> Option<u32> {
        match self.nodes[id] {
            Node::Special(_, tag) => tag,
            _ => None,
        }
    }

    fn metadata_of(&self, id: usize) -> Metadata {
        let (logical, allocated) = match self.nodes[id] {
            Node::Dir(_) => (4096, 4096),
            Node::File(n) => (n, n.div_ceil(4096) * 4096),
            Node::Link(..) => (12, 0),
            Node::Special(..) => (0, 0),
        };
        let mut md = self.md.clone();
        md.kind = self.kind(id);
        md.identity = Some(FileIdentity::new(VOLUME, id as u128));
        md.logical_size = logical;
        md.allocated_size = Some(allocated);
        md.nlink = Some(1);
        md.mtime_unix = Some(1_700_000_000);
        md.readonly = false;
        let cloud = self.cloud.get(&id).copied().unwrap_or_default();
        md.file_attributes = cloud.file_attributes;
        md.bsd_flags = cloud.bsd_flags;
        md.reparse_tag = self.reparse_tag(id);
        md
    }

    /// Reaching a path through a placeholder directory means it was listed or hydrated.
    fn check_beneath_placeholder(&self, r: &Resolved, path: &Path) {
        let above = &r.chain[..r.chain.len() - 1];
        if above.iter().any(|&id| self.is_placeholder(id)) {
            self.violation(format!("beneath a cloud placeholder: {}", path.display()));
        }
    }

    fn error(op: Operation, path: &Path, miss: Miss) -> PlatformError {
        match miss {
            Miss::NotFound => PlatformError::from_io(op, path, io::ErrorKind::NotFound.into()),
            Miss::NotADirectory(kind) => PlatformError::not_a_directory(path, kind),
        }
    }
}

impl Platform for Graph {
    fn capabilities(&self) -> Capabilities {
        Native.capabilities()
    }

    fn metadata(&self, path: &Path) -> scull_platform::Result<Metadata> {
        let r = self
            .resolve(path, false)
            .map_err(|m| Self::error(Operation::Metadata, path, m))?;
        if r.via_link {
            self.violation(format!("metadata beneath a link: {}", path.display()));
        }
        self.check_beneath_placeholder(&r, path);
        Ok(self.metadata_of(r.node))
    }

    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<DirIter<'a>> {
        self.reads.lock().unwrap().push(path.to_path_buf());
        let lstat = self
            .resolve(path, false)
            .map_err(|m| Self::error(Operation::ReadDir, path, m))?;
        let kind = self.kind(lstat.node);
        if kind.is_special() {
            // A real OS would open it here: a FIFO without a writer blocks forever.
            self.violation(format!("listed (opened) a {kind}: {}", path.display()));
            return Err(PlatformError::not_a_directory(path, kind));
        }
        if kind.is_link() || lstat.via_link {
            self.violation(format!("listed through a {kind}: {}", path.display()));
        }
        if self.is_placeholder(lstat.node) {
            // A real OS would ask the sync provider to download it here (PLT-04).
            self.violation(format!(
                "listed (hydrated) a cloud placeholder: {}",
                path.display()
            ));
        }
        self.check_beneath_placeholder(&lstat, path);
        // Adversarial: follow a final link like `opendir` would.
        let r = self
            .resolve(path, true)
            .map_err(|m| Self::error(Operation::ReadDir, path, m))?;
        let Node::Dir(children) = &self.nodes[r.node] else {
            return Err(PlatformError::not_a_directory(path, self.kind(r.node)));
        };
        let entries: Vec<_> = children
            .iter()
            .map(|(name, id)| {
                let mut de = self.de.clone();
                de.path = path.join(name);
                de.file_name = name.into();
                de.kind = self.kind(*id);
                de.reparse_tag = self.reparse_tag(*id);
                Ok(de)
            })
            .collect();
        Ok(Box::new(entries.into_iter()))
    }
}
