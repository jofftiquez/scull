//! Hard-link accounting (PHY-03): count every file once, and tell bytes that removing a
//! subtree would free from bytes pinned by links elsewhere.
//!
//! Two structures, both holding only files with a link count above 1:
//!
//! * [`LinkSet`]: one per scan, sharded. Decides which path of a multi-linked file is the
//!   one whose bytes go into the scan-wide running totals (the first one seen). An identity
//!   is dropped as soon as all its links were seen.
//! * [`OpenLinks`]: one per directory still in progress. For each multi-linked file in the
//!   subtree whose links are *not* all inside the subtree yet, how many links were seen. A
//!   finished subtree's map is merged into its parent's: links of the same file found in
//!   sibling subtrees are matched there, and a file whose links are all accounted for
//!   becomes internal (reclaimable) and leaves the map. What is still open when a directory
//!   finishes has links outside it: those bytes are its *shared* bytes.
//!
//! Per-subtree numbers depend only on which links are inside the subtree, never on the order
//! workers visit directories, so they are identical for any thread count.

use scull_core::FileIdentity;
use scull_platform::{EntryKind, Metadata};
use std::collections::HashMap;
use std::collections::hash_map::{Entry, RandomState};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::types::Totals;

/// A fast hasher for [`FileIdentity`] keys (a few integers): a multiply-rotate mix seeded
/// once per process from [`RandomState`], so the bucket layout isn't predictable from inode
/// numbers. SipHash (the std default) costs several times more per lookup, and the maps
/// here see one lookup per multi-linked path per directory level they bubble through.
#[derive(Clone, Copy)]
pub(crate) struct IdHasher(u64);

const MIX: u64 = 0x9E37_79B9_7F4A_7C15;

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(buf));
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(26) ^ n).wrapping_mul(MIX);
    }

    fn write_u128(&mut self, n: u128) {
        self.write_u64(n as u64);
        self.write_u64((n >> 64) as u64);
    }

    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 29)
    }
}

/// Builds [`IdHasher`]s with the process-wide seed.
#[derive(Clone, Copy)]
pub(crate) struct BuildIdHasher(u64);

impl Default for BuildIdHasher {
    fn default() -> Self {
        static SEED: OnceLock<u64> = OnceLock::new();
        Self(*SEED.get_or_init(|| RandomState::new().hash_one(0x5c_u8) | 1))
    }
}

impl BuildHasher for BuildIdHasher {
    type Hasher = IdHasher;

    fn build_hasher(&self) -> IdHasher {
        IdHasher(self.0)
    }
}

/// A [`FileIdentity`] packed into 24 bytes with 8-byte alignment (the `u128` file ID makes
/// `FileIdentity` 32 bytes, 16-aligned): map entries are a third smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key([u64; 3]);

impl From<FileIdentity> for Key {
    fn from(id: FileIdentity) -> Self {
        Key([id.volume, id.file as u64, (id.file >> 64) as u64])
    }
}

impl std::hash::Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.0[0]);
        state.write_u64(self.0[1]);
        state.write_u64(self.0[2]);
    }
}

type IdMap<V> = HashMap<Key, V, BuildIdHasher>;

/// [`OpenLink`] as stored: 24 bytes. Link counts beyond `u32::MAX` saturate (no filesystem
/// allows that many); an unknown allocated size is `u64::MAX`.
#[derive(Debug, Clone, Copy)]
struct Stored {
    seen: u32,
    nlink: u32,
    logical: u64,
    allocated: u64,
}

impl Stored {
    fn allocated(&self) -> Option<u64> {
        (self.allocated != u64::MAX).then_some(self.allocated)
    }
}

impl From<OpenLink> for Stored {
    fn from(l: OpenLink) -> Self {
        Stored {
            seen: u32::try_from(l.seen).unwrap_or(u32::MAX),
            nlink: u32::try_from(l.nlink).unwrap_or(u32::MAX),
            logical: l.logical,
            allocated: l.allocated.unwrap_or(u64::MAX),
        }
    }
}

/// How an entry takes part in hard-link accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkKind {
    /// A directory, or a file with exactly one link: its bytes belong to its path.
    Single,
    /// More than one link, identity known: deduplicated and attributed.
    Linked(FileIdentity, u64),
    /// More than one link but no identity: can't be matched with its other links.
    NoIdentity,
    /// Link count unknown: treated as single, with low confidence.
    UnknownCount,
}

impl LinkKind {
    pub(crate) fn of(md: &Metadata) -> Self {
        if md.kind == EntryKind::Dir {
            return LinkKind::Single;
        }
        match (md.nlink, md.identity) {
            (Some(n), Some(id)) if n > 1 => LinkKind::Linked(id, n),
            (Some(n), None) if n > 1 => LinkKind::NoIdentity,
            (Some(_), _) => LinkKind::Single,
            (None, _) => LinkKind::UnknownCount,
        }
    }
}

/// A multi-linked file seen in a subtree that also has links not (yet) seen in it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpenLink {
    /// Links seen inside the subtree.
    pub seen: u64,
    /// The file's total link count.
    pub nlink: u64,
    pub logical: u64,
    pub allocated: Option<u64>,
}

/// Multi-linked files of one subtree whose links are not all inside it, plus the bytes they
/// account for.
#[derive(Debug, Default)]
pub(crate) struct OpenLinks {
    open: IdMap<Stored>,
    /// Allocated bytes of the files in `open` plus those of multi-linked files without
    /// identity: the subtree's shared bytes.
    pub shared_allocated: u64,
}

impl OpenLinks {
    /// Record a link of `id` whose bytes were *already added* to `totals` (and are not yet in
    /// `shared_allocated`). If the file was already counted in this subtree, the bytes are
    /// taken back out; if all its links are now inside, it becomes internal.
    pub(crate) fn add(&mut self, totals: &mut Totals, id: FileIdentity, link: OpenLink) {
        self.shared_allocated = self
            .shared_allocated
            .saturating_add(link.allocated.unwrap_or(0));
        self.insert_counted(totals, Key::from(id), Stored::from(link));
    }

    /// Like [`OpenLinks::add`] for a link whose bytes are already in both `totals` and
    /// `shared_allocated` (an entry of a merged child map).
    fn insert_counted(&mut self, totals: &mut Totals, key: Key, link: Stored) {
        let complete = match self.open.entry(key) {
            Entry::Vacant(v) => {
                if link.seen < link.nlink {
                    v.insert(link);
                    return;
                }
                link
            }
            Entry::Occupied(mut o) => {
                // The same file through another link: its bytes are already counted here.
                totals.remove_bytes(link.logical, link.allocated());
                totals.hardlinks_deduped += 1;
                self.shared_allocated = self
                    .shared_allocated
                    .saturating_sub(link.allocated().unwrap_or(0));
                let e = o.get_mut();
                e.seen = e.seen.saturating_add(link.seen);
                if e.seen < e.nlink {
                    return;
                }
                o.remove()
            }
        };
        // Every link is inside this subtree: removing it frees the file.
        self.shared_allocated = self
            .shared_allocated
            .saturating_sub(complete.allocated().unwrap_or(0));
    }

    /// Add bytes of a multi-linked file whose other links can't be located (no identity).
    pub(crate) fn add_unattributable(&mut self, allocated: Option<u64>) {
        self.shared_allocated = self.shared_allocated.saturating_add(allocated.unwrap_or(0));
    }

    /// Merge a finished part of the subtree (whose totals were already merged into
    /// `totals`). Small-to-large: the bigger map is kept, the smaller one re-inserted.
    pub(crate) fn merge(&mut self, totals: &mut Totals, mut other: OpenLinks) {
        self.shared_allocated = self.shared_allocated.saturating_add(other.shared_allocated);
        if other.open.len() > self.open.len() {
            std::mem::swap(&mut self.open, &mut other.open);
        }
        for (key, link) in other.open {
            self.insert_counted(totals, key, link);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.open.len()
    }
}

const SHARDS: usize = 64;

/// Scan-wide set of multi-linked files with links still unseen, sharded to keep lock
/// contention low. Each entry holds the number of links not yet seen.
pub(crate) struct LinkSet {
    shards: Box<[Mutex<IdMap<u64>>]>,
    len: AtomicUsize,
    peak: AtomicUsize,
}

impl LinkSet {
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            len: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    fn shard(&self, key: &Key) -> &Mutex<IdMap<u64>> {
        let [volume, lo, hi] = key.0;
        let mixed = (lo ^ hi ^ volume.rotate_left(29)).wrapping_mul(MIX);
        &self.shards[(mixed >> 58) as usize % SHARDS]
    }

    /// One link of `id` (link count `nlink` > 1) was seen. Returns `true` if it is the first,
    /// i.e. the file's bytes should be counted now.
    pub(crate) fn observe(&self, id: FileIdentity, nlink: u64) -> bool {
        let key = Key::from(id);
        let mut map = crate::walk::lock(self.shard(&key));
        match map.entry(key) {
            Entry::Occupied(mut o) => {
                let left = o.get_mut();
                *left = left.saturating_sub(1);
                if *left == 0 {
                    o.remove();
                    self.len.fetch_sub(1, Ordering::Relaxed);
                }
                false
            }
            Entry::Vacant(v) => {
                if nlink > 1 {
                    v.insert(nlink - 1);
                    let n = self.len.fetch_add(1, Ordering::Relaxed) + 1;
                    self.peak.fetch_max(n, Ordering::Relaxed);
                }
                true
            }
        }
    }

    /// Most identities held at once.
    pub(crate) fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// Identities held now.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(seen: u64, nlink: u64, alloc: u64) -> OpenLink {
        OpenLink {
            seen,
            nlink,
            logical: alloc,
            allocated: Some(alloc),
        }
    }

    /// Add a single seen link the way the walker does: bytes into totals first.
    fn see(t: &mut Totals, o: &mut OpenLinks, id: FileIdentity, nlink: u64, alloc: u64) {
        t.add_bytes(alloc, Some(alloc));
        o.add(t, id, link(1, nlink, alloc));
    }

    #[test]
    fn two_links_in_one_map_count_once_and_close() {
        let (mut t, mut o) = (Totals::default(), OpenLinks::default());
        let id = FileIdentity::new(1, 7);
        see(&mut t, &mut o, id, 2, 4096);
        assert_eq!((t.allocated, o.shared_allocated, o.len()), (4096, 4096, 1));
        see(&mut t, &mut o, id, 2, 4096);
        assert_eq!((t.allocated, o.shared_allocated, o.len()), (4096, 0, 0));
        assert_eq!(t.hardlinks_deduped, 1);
    }

    #[test]
    fn sibling_maps_close_in_parent() {
        let id = FileIdentity::new(1, 9);
        let (mut ta, mut a) = (Totals::default(), OpenLinks::default());
        let (mut tb, mut b) = (Totals::default(), OpenLinks::default());
        see(&mut ta, &mut a, id, 3, 100);
        see(&mut tb, &mut b, id, 3, 100);
        let mut tp = Totals::default();
        let mut p = OpenLinks::default();
        tp.merge(&ta);
        p.merge(&mut tp, a);
        tp.merge(&tb);
        p.merge(&mut tp, b);
        // Two of three links inside: counted once, still shared.
        assert_eq!((tp.allocated, p.shared_allocated, p.len()), (100, 100, 1));
        see(&mut tp, &mut p, id, 3, 100);
        assert_eq!((tp.allocated, p.shared_allocated, p.len()), (100, 0, 0));
        assert_eq!(tp.hardlinks_deduped, 2);
    }

    #[test]
    fn link_set_tracks_only_open_identities() {
        let s = LinkSet::new();
        let a = FileIdentity::new(1, 1);
        let b = FileIdentity::new(1, 2);
        assert!(s.observe(a, 2));
        assert!(s.observe(b, 3));
        assert_eq!(s.len(), 2);
        assert!(!s.observe(a, 2));
        assert_eq!(s.len(), 1);
        assert!(!s.observe(b, 3));
        assert!(!s.observe(b, 3));
        assert_eq!((s.len(), s.peak()), (0, 2));
    }
}
