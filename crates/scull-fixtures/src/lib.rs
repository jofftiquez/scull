//! Generated fixture trees, canary directories and the universal invariants I-1…I-8
//! (docs/FIXTURES.md §1–2).
//!
//! Fixtures are **generated, not committed**: each test builds its tree in a fresh temporary
//! directory, with a canary directory beside the root, then checks the invariants.
//!
//! ```no_run
//! use scull_fixtures::{days_ago, Fixture, InvariantInputs, NodeLock};
//!
//! # fn main() -> Result<(), scull_fixtures::FixtureError> {
//! let mut fx = Fixture::new()?;
//! fx.cargo_project("app", "app")?
//!     .cargo_target("app", "app", 4096)?
//!     .npm_project("web", "web", NodeLock::Npm)?
//!     .file_with_mtime("app/target/old.o", b"obj", days_ago(3))?;
//! let canary = fx.canary().symlink_target_dir.clone();
//! fx.symlink("app/target/escape", &canary)?; // skipped (and recorded) if not permitted
//! fx.checkpoint()?; // I-3 baseline
//!
//! // ... run the code under test against fx.root() ...
//!
//! fx.check_invariants(&InvariantInputs::new()).assert_ok();
//! # Ok(())
//! # }
//! ```
//!
//! * [`Fixture`]: temp root + canary, deterministic builders that can only write inside the
//!   temporary directory, Git repos via the real `git` binary in an isolated environment, and
//!   Cargo/npm/pnpm project helpers.
//! * [`TreeSnapshot`] / [`TreeDiff`]: no-follow snapshots (path, kind, size, SHA-256, mtime,
//!   identity, permissions) and diffs.
//! * [`check_invariants`]: runs I-2 and I-3 always, I-1 and I-8 when given a plan / tiers, and
//!   reports I-4…I-7 as [`Outcome::Unimplemented`] with the issue that will implement them.
//!
//! * [`registry`]: every fixture ID of `docs/FIXTURES.md` §3 this lane tracks, with where it is
//!   implemented or which issue tracks it (checked against the document by `tests/registry.rs`).
//!
//! OS gaps that stable `std` cannot fill (Windows file identity) are documented on
//! [`Entry::identity`] and left to #32 (P1-PLAT-02).

mod error;
mod fixture;
mod git;
pub mod invariants;
mod os;
mod projects;
pub mod registry;
mod snapshot;

pub use error::{FixtureError, Result};
pub use fixture::{Canary, Fixture, LinkOutcome, Skipped, ago, days_ago};
pub use git::{GIT_DATE, GIT_USER_EMAIL, GIT_USER_NAME, GitRepo};
pub use invariants::{
    InvariantId, InvariantInputs, InvariantReport, Outcome, PlanEntry, check_invariants,
};
pub use projects::{CACHEDIR_TAG, NodeLock};
pub use snapshot::{ChangedField, Entry, EntryChange, EntryKind, TreeDiff, TreeSnapshot, diff};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-fixtures");
    }
}
