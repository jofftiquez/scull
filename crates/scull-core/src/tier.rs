//! Risk tiers (PRD §8.2) and the "most protective wins" ordering used by the rubric (§8.3).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// How much risk removing an item carries.
///
/// The ordering is by **protectiveness**: `S0 < S1 < S2 < S3`. Combining evidence always takes
/// the maximum (the most protective tier), and a tier may only ever move *up* this order after a
/// scan (invariant I-8: tiers never decrease on revalidation).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum Tier {
    /// Regenerable: a verified generated artifact, reproducible from tracked inputs.
    /// Preselected for review, but never executed without confirmation.
    S0,
    /// Costly to recreate: reproducible, but needs a significant download or build. Opt-in.
    S1,
    /// Uncertain: ownership, activity or reproducibility is unclear. Never preselected.
    S2,
    /// Protected: possibly unique data, in active use, or explicitly protected. Never acted on.
    S3,
}

impl Tier {
    /// All tiers, least to most protective.
    pub const ALL: [Tier; 4] = [Tier::S0, Tier::S1, Tier::S2, Tier::S3];

    /// The more protective of two tiers.
    #[must_use]
    pub fn max_protective(self, other: Tier) -> Tier {
        self.max(other)
    }

    /// The most protective tier in `tiers`, or `None` if empty.
    pub fn most_protective<I: IntoIterator<Item = Tier>>(tiers: I) -> Option<Tier> {
        tiers.into_iter().max()
    }

    /// One tier more protective (S0 → S1 → S2 → S3; S3 stays S3). Used for the activity
    /// window rule in PRD §8.3.
    #[must_use]
    pub fn bumped(self) -> Tier {
        match self {
            Tier::S0 => Tier::S1,
            Tier::S1 => Tier::S2,
            Tier::S2 | Tier::S3 => Tier::S3,
        }
    }

    /// Whether an item at this tier may appear in an executable plan at all (invariant I-1).
    pub fn is_actionable(self) -> bool {
        self != Tier::S3
    }

    /// Whether items at this tier are preselected in a review (only S0, PRD §8.2).
    pub fn is_preselected(self) -> bool {
        self == Tier::S0
    }

    /// Short human label, e.g. "regenerable".
    pub fn label(self) -> &'static str {
        match self {
            Tier::S0 => "regenerable",
            Tier::S1 => "costly",
            Tier::S2 => "uncertain",
            Tier::S3 => "protected",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Tier::S0 => "S0",
            Tier::S1 => "S1",
            Tier::S2 => "S2",
            Tier::S3 => "S3",
        };
        f.write_str(s)
    }
}

/// Bytes per tier, used to aggregate parent nodes for charts (PRD §8.3 "Aggregation").
///
/// A parent's *badge* is the most protective tier that has any bytes; charts must never color a
/// parent as safe when it contains S2/S3 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TierBreakdown {
    /// Bytes classified S0.
    pub s0: u64,
    /// Bytes classified S1.
    pub s1: u64,
    /// Bytes classified S2.
    pub s2: u64,
    /// Bytes classified S3.
    pub s3: u64,
}

impl TierBreakdown {
    /// Add `bytes` to the bucket for `tier` (saturating).
    pub fn add(&mut self, tier: Tier, bytes: u64) {
        let slot = match tier {
            Tier::S0 => &mut self.s0,
            Tier::S1 => &mut self.s1,
            Tier::S2 => &mut self.s2,
            Tier::S3 => &mut self.s3,
        };
        *slot = slot.saturating_add(bytes);
    }

    /// Merge another breakdown into this one (saturating).
    pub fn merge(&mut self, other: &TierBreakdown) {
        self.add(Tier::S0, other.s0);
        self.add(Tier::S1, other.s1);
        self.add(Tier::S2, other.s2);
        self.add(Tier::S3, other.s3);
    }

    /// Bytes in `tier`.
    pub fn get(&self, tier: Tier) -> u64 {
        match tier {
            Tier::S0 => self.s0,
            Tier::S1 => self.s1,
            Tier::S2 => self.s2,
            Tier::S3 => self.s3,
        }
    }

    /// Total bytes (saturating).
    pub fn total(&self) -> u64 {
        Tier::ALL
            .iter()
            .fold(0u64, |acc, t| acc.saturating_add(self.get(*t)))
    }

    /// The badge for a parent node: the most protective tier holding any bytes.
    pub fn badge(&self) -> Option<Tier> {
        Tier::ALL.iter().rev().copied().find(|t| self.get(*t) > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_by_protectiveness() {
        assert!(Tier::S0 < Tier::S1 && Tier::S1 < Tier::S2 && Tier::S2 < Tier::S3);
        assert_eq!(Tier::S1.max_protective(Tier::S2), Tier::S2);
        assert_eq!(
            Tier::most_protective([Tier::S0, Tier::S3, Tier::S1]),
            Some(Tier::S3)
        );
        assert_eq!(Tier::most_protective([]), None);
    }

    #[test]
    fn bump_never_lowers() {
        for t in Tier::ALL {
            assert!(t.bumped() >= t);
        }
        assert_eq!(Tier::S0.bumped(), Tier::S1);
        assert_eq!(Tier::S3.bumped(), Tier::S3);
    }

    #[test]
    fn only_s3_is_not_actionable() {
        assert!(!Tier::S3.is_actionable());
        assert!(Tier::S2.is_actionable());
        assert!(Tier::S0.is_preselected() && !Tier::S1.is_preselected());
    }

    #[test]
    fn badge_is_most_protective_nonzero() {
        let mut b = TierBreakdown::default();
        assert_eq!(b.badge(), None);
        b.add(Tier::S0, 100);
        assert_eq!(b.badge(), Some(Tier::S0));
        b.add(Tier::S2, 1);
        assert_eq!(b.badge(), Some(Tier::S2));
        assert_eq!(b.total(), 101);
    }
}
