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
//! Hardlink-aware sizing is #37: every entry carries its identity and link count
//! ([`EntryInfo`]) and [`Totals::multi_linked`] counts the candidates for deduplication.

mod cancel;
mod types;
mod walk;

pub use cancel::CancelToken;
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
