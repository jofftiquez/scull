//! Classification rubric, revalidation and the approval store (PRD §8.3, §10).
//!
//! * [`rubric`]: the PRD §8.3 classification rubric as a pure function from observed
//!   [`Signals`] to tier reasons and a [`Tier`](scull_core::Tier) ([`classify`], [`apply`]).
//!
//! * [`activity`]: the PRD §8.4 activity signals: whether a candidate is in use (process
//!   working directories, open files, held tool locks, through an [`ActivityProbe`]) and
//!   whether it was modified within the activity window ([`check_in_use`],
//!   [`recent_activity`]).
//!
//! See `docs/PRD.md` and `HANDOVER.md` for responsibilities and boundaries.

pub mod activity;
pub mod rubric;

pub use activity::{
    ActivityProbe, ActivitySignal, GapPolicy, Holder, InUse, MtimeSample, NativeActivityProbe,
    ProbeResult, RecentActivity, Unavailable, check_in_use, recent_activity, sample_newest_mtime,
};
pub use rubric::{
    ActivityWindow, Classification, GitTracking, Regen, Signals, Tri, apply, classify,
};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-safety");
    }
}
