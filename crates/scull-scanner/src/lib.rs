//! Parallel, bounded, no-follow filesystem traversal and sizing (PRD §18.2).
//!
//! [`scan`] walks a directory tree with a pool of worker threads, reading every entry's
//! metadata through [`scull_platform::Platform`] (this crate never touches the filesystem
//! directly), and streams [`ScanEvent`]s to an [`EventSink`]:
//!
//! * [`ScanEvent::Progress`]: throttled running totals and the directory being listed;
//! * [`ScanEvent::Listing`]: the entries of each directory, top-down (opt-in via
//!   [`ScanOptions::emit_listings`]), for detectors that ask "what is in directory X?";
//! * [`ScanEvent::DirDone`]: each directory's subtree totals (logical and allocated bytes,
//!   file/dir/link counts, newest mtime), bottom-up, for "how big is subtree X?";
//! * [`ScanEvent::Boundary`]: directories counted but not descended (depth limit, other
//!   volume, identity loop);
//! * [`ScanEvent::Issue`]: permission denied and other errors, reported and never fatal.
//!
//! Memory stays bounded: only directories whose subtree is still in progress are held, and
//! each finished directory is folded into its parent and dropped. A [`CancelToken`] stops the
//! scan within tens of milliseconds and the result is marked [`ScanStatus::Cancelled`];
//! nothing is persisted here.
//!
//! ```no_run
//! use std::sync::Arc;
//! use scull_platform::Native;
//! use scull_scanner::{scan, CancelToken, ScanEvent, ScanOptions};
//!
//! let cancel = CancelToken::new();
//! let outcome = scan(
//!     Arc::new(Native),
//!     std::path::Path::new("."),
//!     &ScanOptions::default(),
//!     &cancel,
//!     |event| {
//!         if let ScanEvent::Issue(issue) = event {
//!             eprintln!("warning: {}", issue.message);
//!         }
//!     },
//! )?;
//! println!("{} entries, {} bytes allocated", outcome.totals.entries, outcome.totals.allocated);
//! # Ok::<(), scull_scanner::ScanError>(())
//! ```
//!
//! # Reclaim estimate (PRD §9, PHY-01)
//!
//! [`estimate`] turns a [`DirSummary`] and caller-supplied volume facts ([`EstimateContext`]:
//! filesystem type, snapshots) into core [`scull_core::Sizes`] whose reclaimable figure
//! carries a [`scull_core::Confidence`]; [`assess`] adds the [`EstimateNote`]s behind it and
//! [`ActionReclaim`] splits it into freed-now versus moved (quarantine, Trash). The confidence
//! reaches JSON through `Candidate.sizes.reclaimable.confidence`, e.g. for a pnpm
//! `node_modules` on APFS, hard-linked into the store:
//!
//! ```json
//! "sizes": {
//!   "logical": 412000000,
//!   "allocated": 398000000,
//!   "reclaimable": { "bytes": 1200000, "confidence": "low", "shared_bytes": 396800000 }
//! }
//! ```
//!
//! # Hard links (PHY-03)
//!
//! Sizes count every file once, however many hard links to it the scan meets (deduplicated
//! by [`scull_core::FileIdentity`]; only multi-linked files are tracked, so memory stays
//! bounded). Each [`DirSummary`] also splits its allocated bytes into
//! [`DirSummary::unique_allocated`] (files whose links are all inside the subtree: freed if it
//! is removed) and [`DirSummary::shared_allocated`] (files with links elsewhere, e.g. a pnpm
//! `node_modules` hard-linked into the store: removing it frees ~0 until `pnpm store prune`),
//! with a [`DirSummary::reclaim_confidence`] that drops to low where the platform can't
//! identify a multi-linked file. [`DirSummary::reclaimable`] packages these as a core
//! [`scull_core::Reclaimable`].
//!
//! # Links, loops and special files (SAFE-04, SAFE-18, T-14, T-53)
//!
//! The scanner asks the platform for exactly two things: no-follow metadata of a path and the
//! listing of a path whose no-follow kind is [`scull_platform::EntryKind::Dir`]. Hence:
//!
//! * **Symlinks, junctions and other reparse points** are counted once each as a link
//!   ([`Totals::links`]) with their own size and never listed through, so symlink loops,
//!   chains, dangling links and Windows junction loops cannot make a scan loop or escape.
//! * **Directory loops** that don't involve a link (bind mounts, directory hard links: the same
//!   directory identity reachable below itself) are detected by identity: the repeated
//!   directory is reported once as a [`BoundaryReason::Loop`] boundary and not descended.
//! * **FIFOs, sockets (Windows: AF_UNIX reparse points) and device nodes** are identified from
//!   metadata, counted in [`Totals::specials`] (so the subtree containing them is flagged in
//!   its [`DirSummary`]) and never listed or opened: a FIFO without a writer can't block a
//!   scan, and opening a device can't have side effects.
//! * A root that is a link or special file is refused with [`ScanError::RootNotADirectory`].

mod cancel;
mod estimate;
mod links;
mod types;
mod walk;

pub use cancel::CancelToken;
pub use estimate::{
    ActionReclaim, CloneSupport, Estimate, EstimateContext, EstimateNote, Snapshots, VolumeFs,
    assess, estimate,
};
pub use types::{
    Boundary, BoundaryReason, DirListing, DirSummary, EntryInfo, EventSink, IssueKind, IssueOp,
    NullSink, Progress, ScanError, ScanEvent, ScanIssue, ScanOptions, ScanOutcome, ScanStatus,
    Totals,
};
pub use walk::scan;

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-scanner");
    }
}
