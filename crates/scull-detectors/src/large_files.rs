//! Large-files inspector (P1-DET-05, PRD §7.1 "Arbitrary large files: inspect-only").
//!
//! Lists the largest regular files under a directory, by **allocated** size (a sparse file
//! counts what it really occupies), with modification time and a kind hint guessed from the
//! file **name only** (VM disk images, disk images, archives, model weights, ...). File contents
//! are never read.
//!
//! Large files are **never** cleanup candidates: every [`LargeFile::into_candidate`] is
//! [`Category::LargeFile`], tier S2 with [`ReasonCode::InspectOnly`] and [`Action::None`], which
//! [`scull_core::PlanItem::from_candidate`] and [`scull_core::Plan::new`] refuse.
//!
//! Memory is bounded: [`LargeFiles`] keeps only the top `N` files in a min-heap however many
//! files are seen. Feed it the scanner's [`ScanEvent::Listing`] events
//! ([`LargeFiles::observe`]), or let [`inspect_large_files`] run the scan.
//!
//! Links are never followed (the scanner reports a symlink or junction with its own metadata,
//! and only [`EntryKind::File`] entries are considered), and a file with several hard links is
//! listed once.

use scull_core::{
    Action, Candidate, Category, Confidence, Evidence, FileIdentity, Flags, FsPath, ItemId,
    ReasonCode, Reclaimable, Regeneration, RegenerationCost, RuleRef, Sizes, Tier, TierReason,
};
use scull_platform::{EntryKind, Platform, native_path_bytes};
use scull_scanner::{
    CancelToken, DirListing, EntryInfo, ScanError, ScanEvent, ScanOptions, ScanOutcome,
};
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// Rule ID recorded on large-file candidates.
pub const LARGE_FILE_RULE_ID: &str = "files.large";
/// Version of the inspector; bumps when selection or tiering changes.
pub const LARGE_FILE_RULE_VERSION: u32 = 1;
/// Default size threshold: 500 MiB (allocated).
pub const DEFAULT_THRESHOLD: u64 = 500 * 1024 * 1024;
/// Default number of files listed.
pub const DEFAULT_TOP: usize = 50;

/// What a large file probably is, from its name alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum FileKind {
    /// Virtual machine disk (`.vmdk`, `.vdi`, `.vhd(x)`, `.qcow2`, Docker Desktop's `Docker.raw`, ...).
    VmDisk,
    /// Disk or installer image (`.iso`, `.dmg`, `.img`, `.wim`, ...).
    DiskImage,
    /// Archive or compressed file (`.zip`, `.tar`, `.gz`, `.7z`, ...).
    Archive,
    /// Machine-learning model weights (`.safetensors`, `.gguf`, `.pt`, `.onnx`, ...).
    ModelWeights,
    /// Video (`.mp4`, `.mov`, `.mkv`, ...).
    Video,
    /// Database file or dump (`.sqlite`, `.db`, `.sql`, `.dump`, `.bak`).
    Database,
    /// Installer or package (`.pkg`, `.msi`, `.exe`, `.deb`, `.rpm`, `.AppImage`, ...).
    Installer,
    /// Log file (`.log`).
    Log,
    /// Anything else.
    Other,
}

impl FileKind {
    /// Guess the kind from a file name (case-insensitive extension, a few well-known names).
    /// Pure: no file is opened.
    pub fn from_name(name: &str) -> Self {
        let lower = name.to_ascii_lowercase();
        if lower == "docker.raw" || lower == "docker.qcow2" {
            return FileKind::VmDisk;
        }
        let Some((_, ext)) = lower.rsplit_once('.') else {
            return FileKind::Other;
        };
        match ext {
            "vmdk" | "vdi" | "vhd" | "vhdx" | "qcow" | "qcow2" | "hdd" | "vmem" | "vmsn"
            | "avhdx" => FileKind::VmDisk,
            "iso" | "dmg" | "img" | "sparseimage" | "toast" | "wim" | "esd" | "xip" => {
                FileKind::DiskImage
            }
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "tbz2" | "xz" | "txz" | "zst" | "7z" | "rar"
            | "lz4" | "cab" => FileKind::Archive,
            "safetensors" | "gguf" | "ggml" | "pt" | "pth" | "ckpt" | "onnx" | "h5" | "keras"
            | "tflite" | "mlmodel" => FileKind::ModelWeights,
            "mp4" | "mov" | "mkv" | "avi" | "m4v" | "webm" | "wmv" => FileKind::Video,
            "sqlite" | "sqlite3" | "db" | "sql" | "dump" | "bak" => FileKind::Database,
            "pkg" | "msi" | "exe" | "deb" | "rpm" | "appimage" | "apk" | "ipa" => {
                FileKind::Installer
            }
            "log" => FileKind::Log,
            _ => FileKind::Other,
        }
    }

    /// Human-readable label, used as the candidate's artifact type.
    pub fn label(self) -> &'static str {
        match self {
            FileKind::VmDisk => "VM disk image",
            FileKind::DiskImage => "Disk image",
            FileKind::Archive => "Archive",
            FileKind::ModelWeights => "Model weights",
            FileKind::Video => "Video",
            FileKind::Database => "Database or dump",
            FileKind::Installer => "Installer or package",
            FileKind::Log => "Log file",
            FileKind::Other => "Large file",
        }
    }
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Inspector settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LargeFileOptions {
    /// Minimum allocated size (bytes) for a file to be listed.
    pub threshold: u64,
    /// At most this many files are kept (the largest).
    pub top: usize,
}

impl Default for LargeFileOptions {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            top: DEFAULT_TOP,
        }
    }
}

/// One large regular file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LargeFile {
    /// The file.
    pub path: PathBuf,
    /// Its no-follow identity, if the platform provides one.
    pub identity: Option<FileIdentity>,
    /// Logical size (end-of-file).
    pub logical: u64,
    /// Allocated size, if the platform reports it.
    pub allocated: Option<u64>,
    /// Hard-link count, if known.
    pub nlink: Option<u64>,
    /// Modification time, Unix seconds.
    pub mtime: Option<i64>,
    /// Kind hint from the name.
    pub kind: FileKind,
}

impl LargeFile {
    /// The size it is ranked and filtered by: allocated, or logical when allocation is unknown.
    pub fn size(&self) -> u64 {
        self.allocated.unwrap_or(self.logical)
    }

    /// The inspect-only candidate: [`Category::LargeFile`], S2, [`ReasonCode::InspectOnly`],
    /// [`Action::None`]. Never accepted into a plan.
    pub fn into_candidate(self) -> Candidate {
        let (identity, identity_missing) = match self.identity {
            Some(i) => (i, false),
            None => (FileIdentity::new(0, 0), true),
        };
        let multi = self.nlink.map(|n| n > 1);
        let allocated = self.size();
        let reclaimable = match (self.allocated, multi) {
            (Some(a), Some(false)) => Reclaimable {
                bytes: a,
                confidence: Confidence::High,
                shared_bytes: 0,
            },
            (_, Some(true)) => Reclaimable {
                bytes: 0,
                confidence: Confidence::Low,
                shared_bytes: allocated,
            },
            _ => Reclaimable {
                bytes: allocated,
                confidence: Confidence::Unknown,
                shared_bytes: 0,
            },
        };
        let mut evidence = vec![Evidence::Note {
            text: match self.path.extension() {
                Some(ext) if self.kind != FileKind::Other => format!(
                    "kind guessed from the `.{}` extension; contents are never read",
                    ext.to_string_lossy()
                ),
                _ => "kind not recognized from the name; contents are never read".into(),
            },
        }];
        if let Some(t) = self.mtime {
            evidence.push(Evidence::LastModified {
                newest_unix: t,
                sampled: false,
            });
        }
        if let Some(n) = self.nlink.filter(|n| *n > 1) {
            evidence.push(Evidence::Hardlinked {
                shared_bytes: allocated,
            });
            evidence.push(Evidence::Note {
                text: format!("this file has {n} hard links; listed once"),
            });
        }
        if self.allocated.is_none() {
            evidence.push(Evidence::SignalUnavailable {
                signal: "allocated_size".into(),
                reason: "the platform reported no allocated size; the logical size is shown".into(),
            });
        }
        if identity_missing {
            evidence.push(Evidence::SignalUnavailable {
                signal: "file_identity".into(),
                reason: "the platform reported no file identity for this file".into(),
            });
        }
        let reasons = vec![TierReason::new(
            Tier::S2,
            ReasonCode::InspectOnly,
            "inspect-only: large files are listed for review and never offered for cleanup",
        )];
        Candidate {
            id: ItemId::derive(identity, &native_path_bytes(&self.path)),
            path: FsPath::from_path(&self.path),
            identity,
            category: Category::LargeFile,
            artifact: self.kind.label().into(),
            rule: RuleRef {
                id: LARGE_FILE_RULE_ID.into(),
                version: LARGE_FILE_RULE_VERSION,
            },
            project: None,
            tier: Tier::S2,
            reasons,
            evidence,
            sizes: Sizes {
                logical: self.logical,
                allocated,
                reclaimable,
            },
            flags: Flags {
                hardlinked: multi,
                symlink: Some(false),
                ..Flags::default()
            },
            regeneration: Regeneration {
                instructions: None,
                cost: RegenerationCost::Unknown,
            },
            action: Action::None,
        }
    }
}

/// Heap entry ordered by size, then path (so ties are deterministic).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ranked(LargeFile);

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .size()
            .cmp(&other.0.size())
            // Smaller path ranks higher on ties, so the heap evicts the larger path first.
            .then_with(|| other.0.path.cmp(&self.0.path))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// What the inspector found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct LargeFileReport {
    /// The largest files at or above the threshold, largest first, at most `top`.
    pub files: Vec<LargeFile>,
    /// How many files were at or above the threshold (each hard-linked file once, as far as
    /// the kept entries tell).
    pub matched: u64,
    /// Bytes (ranking size) of every file at or above the threshold.
    pub matched_bytes: u64,
}

impl LargeFileReport {
    /// Whether more files matched than are listed.
    pub fn truncated(&self) -> bool {
        self.matched > self.files.len() as u64
    }

    /// The inspect-only candidates, largest first.
    pub fn into_candidates(self) -> Vec<Candidate> {
        self.files
            .into_iter()
            .map(LargeFile::into_candidate)
            .collect()
    }
}

/// Collects the top-N large files from scanner listings in bounded memory.
#[derive(Debug, Clone)]
pub struct LargeFiles {
    opts: LargeFileOptions,
    heap: BinaryHeap<Reverse<Ranked>>,
    matched: u64,
    matched_bytes: u64,
}

impl LargeFiles {
    /// An empty collector.
    pub fn new(opts: LargeFileOptions) -> Self {
        Self {
            opts,
            heap: BinaryHeap::with_capacity(opts.top.min(4096) + 1),
            matched: 0,
            matched_bytes: 0,
        }
    }

    /// Offer every regular file of one scanner listing.
    pub fn observe(&mut self, listing: &DirListing) {
        for e in &listing.entries {
            self.offer(&listing.path, e);
        }
    }

    /// Offer one entry of the directory `dir`. Anything but a regular file (links, special
    /// files, directories) is ignored: links are never followed.
    pub fn offer(&mut self, dir: &Path, e: &EntryInfo) {
        if e.kind != EntryKind::File {
            return;
        }
        let size = e.allocated.unwrap_or(e.logical);
        if size < self.opts.threshold {
            return;
        }
        self.add(LargeFile {
            kind: FileKind::from_name(&e.name.to_string_lossy()),
            path: dir.join(&e.name),
            identity: e.identity,
            logical: e.logical,
            allocated: e.allocated,
            nlink: e.nlink,
            mtime: e.mtime,
        });
    }

    /// Count a file at or above the threshold and keep it if it is among the largest `top`.
    fn add(&mut self, f: LargeFile) {
        // Another link to a file already kept: count it once.
        if f.nlink.is_some_and(|n| n > 1)
            && f.identity.is_some()
            && self
                .heap
                .iter()
                .any(|Reverse(r)| r.0.identity == f.identity)
        {
            return;
        }
        self.matched += 1;
        self.matched_bytes = self.matched_bytes.saturating_add(f.size());
        if self.opts.top == 0 {
            return;
        }
        let candidate = Ranked(f);
        if self.heap.len() < self.opts.top {
            self.heap.push(Reverse(candidate));
        } else if let Some(Reverse(min)) = self.heap.peek()
            && candidate > *min
        {
            self.heap.pop();
            self.heap.push(Reverse(candidate));
        }
    }

    /// The report, largest first.
    pub fn finish(self) -> LargeFileReport {
        // Ascending in `Reverse` order is descending by size: largest first.
        let files: Vec<LargeFile> = self
            .heap
            .into_sorted_vec()
            .into_iter()
            .map(|Reverse(r)| r.0)
            .collect();
        LargeFileReport {
            files,
            matched: self.matched,
            matched_bytes: self.matched_bytes,
        }
    }
}

/// Scan `root` with the scanner (no-follow, listings on) and collect its large files.
///
/// `scan_options.emit_listings` is forced on. Returns the report with the scan outcome (totals,
/// issues, cancellation).
pub fn inspect_large_files(
    platform: Arc<dyn Platform>,
    root: &Path,
    opts: LargeFileOptions,
    scan_options: &ScanOptions,
    cancel: &CancelToken,
) -> Result<(LargeFileReport, ScanOutcome), ScanError> {
    let collector = Arc::new(Mutex::new(LargeFiles::new(opts)));
    let sink = {
        let collector = Arc::clone(&collector);
        move |e: ScanEvent| {
            if let ScanEvent::Listing(l) = e {
                collector
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .observe(&l);
            }
        }
    };
    let mut options = scan_options.clone();
    options.emit_listings = true;
    let outcome = scull_scanner::scan(platform, root, &options, cancel, sink)?;
    let collector = collector
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    Ok((collector.finish(), outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_names_only() {
        let k = FileKind::from_name;
        assert_eq!(k("ubuntu.vmdk"), FileKind::VmDisk);
        assert_eq!(k("Docker.raw"), FileKind::VmDisk);
        assert_eq!(k("disk.QCOW2"), FileKind::VmDisk);
        assert_eq!(k("ubuntu-24.04.iso"), FileKind::DiskImage);
        assert_eq!(k("Xcode.dmg"), FileKind::DiskImage);
        assert_eq!(k("backup.tar.gz"), FileKind::Archive);
        assert_eq!(k("data.7z"), FileKind::Archive);
        assert_eq!(k("llama.gguf"), FileKind::ModelWeights);
        assert_eq!(k("model.safetensors"), FileKind::ModelWeights);
        assert_eq!(k("talk.mov"), FileKind::Video);
        assert_eq!(k("prod.sql"), FileKind::Database);
        assert_eq!(k("setup.msi"), FileKind::Installer);
        assert_eq!(k("server.log"), FileKind::Log);
        assert_eq!(k("blob"), FileKind::Other);
        assert_eq!(k(".hidden"), FileKind::Other);
        assert_eq!(k("weird.bin"), FileKind::Other);
    }

    fn file(name: &str, size: u64, id: Option<(u64, u128, u64)>) -> LargeFile {
        LargeFile {
            path: PathBuf::from("/d").join(name),
            identity: id.map(|(v, i, _)| FileIdentity::new(v, i)),
            logical: size,
            allocated: Some(size),
            nlink: Some(id.map_or(1, |(_, _, n)| n)),
            mtime: Some(1_700_000_000),
            kind: FileKind::from_name(name),
        }
    }

    #[test]
    fn heap_keeps_only_the_largest_top_n() {
        let mut c = LargeFiles::new(LargeFileOptions {
            threshold: 0,
            top: 3,
        });
        for i in 0..1000u64 {
            c.add(file(&format!("f{i:04}"), (i * 7919) % 1009, None));
            assert!(c.heap.len() <= 3, "bounded");
        }
        let r = c.finish();
        assert_eq!(r.matched, 1000);
        assert!(r.truncated());
        let sizes: Vec<u64> = r.files.iter().map(LargeFile::size).collect();
        assert_eq!(sizes, [1008, 1007, 1006]);
    }

    #[test]
    fn ties_are_deterministic_and_links_counted_once() {
        let mut c = LargeFiles::new(LargeFileOptions {
            threshold: 0,
            top: 2,
        });
        c.add(file("b", 10, None));
        c.add(file("a", 10, None));
        c.add(file("c", 10, None));
        let names: Vec<String> = c
            .finish()
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a", "b"]);

        let mut c = LargeFiles::new(LargeFileOptions {
            threshold: 0,
            top: 10,
        });
        c.add(file("x.iso", 50, Some((1, 9, 2))));
        c.add(file("y.iso", 50, Some((1, 9, 2))));
        let r = c.finish();
        assert_eq!((r.files.len(), r.matched), (1, 1));
        let cand = r.into_candidates().remove(0);
        assert_eq!(cand.flags.hardlinked, Some(true));
        assert_eq!(cand.sizes.reclaimable.bytes, 0);
        assert_eq!(cand.sizes.reclaimable.shared_bytes, 50);
    }

    #[test]
    fn top_zero_only_counts() {
        let mut c = LargeFiles::new(LargeFileOptions {
            threshold: 0,
            top: 0,
        });
        c.add(file("a", 1, None));
        let r = c.finish();
        assert!(r.files.is_empty());
        assert_eq!(r.matched, 1);
    }

    #[test]
    fn options_default() {
        let o = LargeFileOptions::default();
        assert_eq!(o.threshold, 500 * 1024 * 1024);
        assert_eq!(o.top, DEFAULT_TOP);
    }
}
