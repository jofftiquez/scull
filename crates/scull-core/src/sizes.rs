//! Size accounting (PRD §6, §9). Every size shown to users is labeled logical, allocated or
//! reclaimable (PHY-01).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// How confident the engine is in a reclaim estimate (PRD §9).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// No sharing, snapshots or clones could affect the result.
    High,
    /// Minor uncertainty (e.g. compression we measured but can't fully attribute).
    Medium,
    /// Shared extents, clones or snapshots may pin some bytes.
    Low,
    /// The engine can't estimate; only the measured delta will tell.
    Unknown,
}

/// The three size figures for an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Sizes {
    /// Sum of file lengths (`st_size` / end-of-file).
    pub logical: u64,
    /// Blocks actually allocated on disk (accounts for sparse files and compression).
    pub allocated: u64,
    /// Estimated free-space increase if the item were removed now.
    pub reclaimable: Reclaimable,
}

/// Physically reclaimable bytes plus the confidence of that estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reclaimable {
    /// Estimated bytes freed. With `Confidence::Unknown` this is a best guess, not a promise.
    pub bytes: u64,
    /// How much to trust `bytes`.
    pub confidence: Confidence,
    /// Bytes shared with something else (e.g. hardlinked into the pnpm store) that removing
    /// this item will not free while the other links remain.
    #[serde(default)]
    pub shared_bytes: u64,
}

impl Default for Reclaimable {
    fn default() -> Self {
        Self {
            bytes: 0,
            confidence: Confidence::Unknown,
            shared_bytes: 0,
        }
    }
}

impl Sizes {
    /// Add another item's sizes to this one (saturating). The combined confidence is the weakest
    /// of the two.
    pub fn accumulate(&mut self, other: &Sizes) {
        self.logical = self.logical.saturating_add(other.logical);
        self.allocated = self.allocated.saturating_add(other.allocated);
        self.reclaimable.bytes = self
            .reclaimable
            .bytes
            .saturating_add(other.reclaimable.bytes);
        self.reclaimable.shared_bytes = self
            .reclaimable
            .shared_bytes
            .saturating_add(other.reclaimable.shared_bytes);
        self.reclaimable.confidence = self
            .reclaimable
            .confidence
            .max(other.reclaimable.confidence);
    }
}

/// Which kind of size a figure is (PHY-01), for labeling in tables, JSON and charts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SizeKind {
    /// Sum of file lengths.
    Logical,
    /// Allocated on disk.
    Allocated,
    /// Estimated physically reclaimable.
    Reclaimable,
    /// Observed change in volume free space (after execution).
    Measured,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulate_takes_weakest_confidence() {
        let mut a = Sizes {
            logical: 10,
            allocated: 8,
            reclaimable: Reclaimable {
                bytes: 8,
                confidence: Confidence::High,
                shared_bytes: 0,
            },
        };
        let b = Sizes {
            logical: 5,
            allocated: 4,
            reclaimable: Reclaimable {
                bytes: 0,
                confidence: Confidence::Low,
                shared_bytes: 4,
            },
        };
        a.accumulate(&b);
        assert_eq!((a.logical, a.allocated, a.reclaimable.bytes), (15, 12, 8));
        assert_eq!(a.reclaimable.shared_bytes, 4);
        assert_eq!(a.reclaimable.confidence, Confidence::Low);
    }

    #[test]
    fn accumulate_saturates() {
        let mut a = Sizes {
            logical: u64::MAX,
            ..Sizes::default()
        };
        a.accumulate(&Sizes {
            logical: 1,
            ..Sizes::default()
        });
        assert_eq!(a.logical, u64::MAX);
    }
}
