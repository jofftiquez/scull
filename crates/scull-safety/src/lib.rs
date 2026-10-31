//! Classification rubric, revalidation and the approval store (PRD §8.3, §10).
//!
//! * [`rubric`]: the PRD §8.3 classification rubric as a pure function from observed
//!   [`Signals`] to tier reasons and a [`Tier`](scull_core::Tier) ([`classify`], [`apply`]).
//!
//! See `docs/PRD.md` and `HANDOVER.md` for responsibilities and boundaries.

pub mod rubric;

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
