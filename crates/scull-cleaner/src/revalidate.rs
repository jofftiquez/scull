//! Revalidation of one plan item against the disk, immediately before it is executed (SAFE-05,
//! SAFE-11, P2-EXEC-03).
//!
//! For each item, right before acting on it, the Executor:
//!
//! 1. locates the item's native path below one of the trusted scan roots (a plan's paths are
//!    display strings: an item whose path is not valid Unicode can't be located and is skipped);
//! 2. **re-derives** candidates with the read-only engine for that item's parent directory only
//!    (bounded: never a whole scan root), which re-runs every classification signal with fresh
//!    observations: Git tracking, a fresh in-use probe (open files, process working
//!    directories, held tool locks), recent activity, protection markers and policies, mounts,
//!    cloud sync;
//! 3. intersects the plan with those candidates ([`scull_core::narrow`]): the item stays only
//!    with the same item ID, the same identity (device + inode / volume + file ID) and path, a
//!    tier that is not S3 and not raised (I-8), and an action that is the same or safer and
//!    still allowed for the tier (D-28);
//! 4. checks that the path, looked up no-follow, still has the item's identity, and records its
//!    kind so the removal walker refuses an object of another type.
//!
//! Any failure skips the item with a reason (never "best effort"). What passes is handed to the
//! removal walker, which re-checks identity and kind once more at open time, opening every
//! component below the anchor no-follow.

use scull_core::{DropReason, ErrorKind, FileIdentity, Plan, PlanItem, Sizes, Tier, narrow};
use scull_engine::paths::canon;
use scull_engine::{CancelToken, Engine, ScanRequest};
use scull_platform::{EntryKind, Native, Platform};
use std::fmt;
use std::path::PathBuf;

/// What the Executor needs to revalidate items: the read-only engine (configured like the scan
/// that produced the plan: Git, in-use probe, config) and the trusted scan roots.
pub struct Revalidation<'e> {
    engine: &'e Engine,
    roots: Vec<Root>,
    request: ScanRequest,
    condition: Option<Condition<'e>>,
}

/// An extra condition the freshly re-derived candidate must still meet for its item to run,
/// on top of [`scull_core::narrow`]: `Err((code, message))` skips the item
/// ([`Skip::Condition`]). Auto-policies (D-11) use it to re-check, right before each move, that
/// the item still qualifies for the policy (still idle long enough, unflagged, ...).
pub type Condition<'e> =
    &'e (dyn Fn(&scull_core::Candidate) -> Result<(), (&'static str, String)> + Sync);

/// A trusted scan root: canonical path and the identity it had when execution started.
#[derive(Debug, Clone)]
struct Root {
    path: PathBuf,
    identity: Option<FileIdentity>,
}

impl fmt::Debug for Revalidation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Revalidation")
            .field("roots", &self.roots)
            .field("request", &self.request)
            .field("condition", &self.condition.is_some())
            .finish_non_exhaustive()
    }
}

impl<'e> Revalidation<'e> {
    /// Revalidate with `engine` below `roots` (the scan roots of the plan's scan). Roots are
    /// canonicalized and their identity is recorded now; every item's removal starts from its
    /// root and checks it still has that identity.
    pub fn new(engine: &'e Engine, roots: &[PathBuf]) -> Self {
        let roots = roots
            .iter()
            .map(|r| {
                let path = canon(r);
                let identity = Native.metadata(&path).ok().and_then(|m| m.identity);
                Root { path, identity }
            })
            .collect();
        Self {
            engine,
            roots,
            request: ScanRequest::full().history(false),
            condition: None,
        }
    }

    /// Also require `condition` of every freshly re-derived candidate (see [`Condition`]).
    #[must_use]
    pub fn with_condition(mut self, condition: Condition<'e>) -> Self {
        self.condition = Some(condition);
        self
    }

    /// Use `request` for the re-derivation scans (e.g. a fixed clock in tests). History is
    /// never recorded for a revalidation scan, whatever `request` says.
    #[must_use]
    pub fn with_request(mut self, request: ScanRequest) -> Self {
        self.request = request.history(false);
        self
    }

    /// The trusted scan roots (canonical), e.g. to measure their volumes.
    pub(crate) fn root_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|r| r.path.clone()).collect()
    }

    /// Revalidate `item` of `plan` right now. `Ok` is the item as it may be executed (see
    /// [`Revalidated`]); `Err` is why it must be skipped.
    pub fn revalidate(
        &self,
        plan: &Plan,
        item: &PlanItem,
        cancel: &CancelToken,
    ) -> Result<Revalidated, Skip> {
        if item.path.is_lossy() {
            return Err(Skip::PathNotRepresentable);
        }
        let native = item.path.to_path_buf();
        let root = self
            .roots
            .iter()
            .filter(|r| native.starts_with(&r.path) && native != r.path)
            .max_by_key(|r| r.path.components().count())
            .ok_or(Skip::OutsideRoots)?;
        let parent = native.parent().ok_or(Skip::OutsideRoots)?.to_path_buf();

        // Fresh, bounded re-derivation: only the item's parent directory.
        let report = self
            .engine
            .scan(std::slice::from_ref(&parent), &self.request, cancel, |_| {})
            .map_err(|e| Skip::Rederive {
                kind: e.kind,
                message: e.message,
            })?;
        if report.cancelled {
            return Err(Skip::Cancelled);
        }
        let fresh = report.candidates.iter().find(|c| c.id == item.item_id);
        let fresh_tier = fresh.map(|c| c.tier);
        let fresh_sizes = fresh.map(|c| c.sizes);
        let narrowed = narrow(plan, &report.candidates);
        let kept = match narrowed
            .executable
            .into_iter()
            .find(|k| k.item_id == item.item_id)
        {
            Some(k) => k,
            None => {
                let reason = narrowed
                    .dropped
                    .into_iter()
                    .find(|(id, _)| *id == item.item_id)
                    .map_or(DropReason::Missing, |(_, r)| r);
                return Err(Skip::Dropped { reason, fresh_tier });
            }
        };

        // The path, looked up no-follow, must still be the planned object.
        let md = Native.metadata(&native).map_err(|e| {
            if e.is_not_found() {
                Skip::Dropped {
                    reason: DropReason::Missing,
                    fresh_tier,
                }
            } else {
                Skip::Rederive {
                    kind: if e.is_permission_denied() {
                        ErrorKind::PermissionDenied
                    } else {
                        ErrorKind::Io
                    },
                    message: e.to_string(),
                }
            }
        })?;
        if md.identity != Some(item.identity) {
            return Err(Skip::Dropped {
                reason: DropReason::IdentityChanged,
                fresh_tier,
            });
        }

        // The run's own extra condition, once the item passed everything else (so the usual
        // reasons, e.g. `now_protected`, take precedence).
        // Fails closed: no fresh candidate to check means the item does not run.
        if let Some(cond) = self.condition {
            let verdict = report
                .candidates
                .iter()
                .find(|c| c.id == item.item_id)
                .map_or_else(
                    || Err(("no_longer_qualifies", "not re-derived".to_owned())),
                    cond,
                );
            if let Err((code, message)) = verdict {
                return Err(Skip::Condition {
                    code,
                    message,
                    fresh_tier,
                });
            }
        }

        let fresh = kept.tier;
        let sizes = fresh_sizes.unwrap_or_else(|| crate::measure::planned_sizes(&kept));
        Ok(Revalidated {
            item: PlanItem {
                // Executed under the more protective of the planned and fresh tiers (I-8: a
                // tier never decreases on revalidation); `narrow` already refused a raise.
                tier: item.tier.max_protective(fresh),
                ..kept
            },
            fresh_tier: fresh,
            path: native,
            anchor: root.path.clone(),
            anchor_identity: root.identity,
            kind: md.kind,
            sizes,
        })
    }
}

/// A plan item that passed revalidation, ready for the removal walker.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Revalidated {
    /// The item to execute: the planned item with the executed action (the same or safer) and
    /// the fresh size estimate. Its tier is the planned one (never lowered, I-8).
    pub item: PlanItem,
    /// The tier the fresh classification gave (the same as or lower than the planned tier).
    pub fresh_tier: Tier,
    /// Native path of the item.
    pub path: PathBuf,
    /// The scan root the item lies below (the walk's anchor).
    pub anchor: PathBuf,
    /// The anchor's identity when execution started.
    pub anchor_identity: Option<FileIdentity>,
    /// The item's kind at revalidation; the walker refuses any other kind.
    pub kind: EntryKind,
    /// The fresh size estimate (logical, allocated, reclaimable with confidence), for the
    /// operation's predicted reclaim (PHY-02).
    pub sizes: Sizes,
}

/// Why an item was skipped at revalidation. Nothing was touched.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Skip {
    /// The fresh classification dropped the item ([`scull_core::narrow`]): gone, replaced
    /// (identity), moved, now S3, tier raised, or action no longer the same or safer.
    Dropped {
        /// Why.
        reason: DropReason,
        /// The fresh tier, when the item was re-derived at all.
        fresh_tier: Option<Tier>,
    },
    /// The planned path is not valid Unicode, so the item can't be located from the plan.
    PathNotRepresentable,
    /// The item does not lie strictly below any of the trusted scan roots.
    OutsideRoots,
    /// Re-deriving the item failed (its parent directory could not be scanned).
    Rederive {
        /// Error kind.
        kind: ErrorKind,
        /// What went wrong.
        message: String,
    },
    /// The operation was cancelled before the item was revalidated.
    Cancelled,
    /// The fresh candidate no longer meets the run's extra condition ([`Condition`], e.g. an
    /// auto-policy's idle window).
    Condition {
        /// Stable reason code (e.g. `no_longer_idle`).
        code: &'static str,
        /// Why.
        message: String,
        /// The fresh tier.
        fresh_tier: Option<Tier>,
    },
}

impl Skip {
    /// Stable reason code for reports and the audit log (`DropReason` codes for dropped items).
    pub fn code(&self) -> &'static str {
        match self {
            Skip::Dropped { reason, .. } => reason.code(),
            Skip::PathNotRepresentable => "path_not_representable",
            Skip::OutsideRoots => "outside_scan_roots",
            Skip::Rederive { .. } => "rederive_failed",
            Skip::Cancelled => "cancelled",
            Skip::Condition { code, .. } => code,
        }
    }

    /// The fresh tier observed at revalidation, if any.
    pub fn fresh_tier(&self) -> Option<Tier> {
        match self {
            Skip::Dropped { fresh_tier, .. } | Skip::Condition { fresh_tier, .. } => *fresh_tier,
            _ => None,
        }
    }
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Skip::Dropped { reason, .. } => match reason {
                DropReason::Missing => f.write_str("no longer found at its path"),
                DropReason::IdentityChanged => {
                    f.write_str("replaced by another object since planning (identity changed)")
                }
                DropReason::PathChanged => f.write_str("its path changed since planning"),
                DropReason::NowProtected => f.write_str("now protected (S3)"),
                DropReason::TierRaised { planned, fresh } => {
                    write!(f, "tier raised from {planned} to {fresh} since planning")
                }
                DropReason::ActionChanged => {
                    f.write_str("the proposed action is no longer the same or safer")
                }
                other => write!(f, "dropped at revalidation ({})", other.code()),
            },
            Skip::PathNotRepresentable => {
                f.write_str("the planned path is not valid Unicode and can't be located")
            }
            Skip::OutsideRoots => f.write_str("not below any trusted scan root"),
            Skip::Rederive { kind, message } => {
                write!(f, "could not re-derive the item ({kind}): {message}")
            }
            Skip::Cancelled => f.write_str("cancelled before revalidation"),
            Skip::Condition { message, .. } => f.write_str(message),
        }
    }
}
