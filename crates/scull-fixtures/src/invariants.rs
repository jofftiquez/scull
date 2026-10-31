//! Universal invariants I-1…I-8 (docs/FIXTURES.md §1).
//!
//! | ID | Status in this harness |
//! |---|---|
//! | I-1 | Implemented: [`no_s3_in_plan`]; needs the plan(s) a fixture produced |
//! | I-2 | Implemented: [`canary_untouched`]; always runs |
//! | I-3 | Implemented: [`root_unchanged_except`]; needs [`Fixture::checkpoint`] |
//! | I-4 | Stub ([`audit_matches_disk`]): needs the audit log, #75 (P2-EXEC-09) |
//! | I-5 | Stub ([`bytes_recorded`]): needs operation records, #76 (P2-EXEC-10) / #53 (P1-STORE-01) |
//! | I-6 | Implemented: [`network_blocked`]; a verified OS sandbox, else a best-effort socket detector (see [`crate::net`]) |
//! | I-7 | Stub ([`spawns_allowlisted`]): needs a process-spawn hook, #78 (P2-QC-01) |
//! | I-8 | Implemented: [`tiers_never_decrease`]; needs scan-time and execute-time tiers |
//!
//! Stubs return [`Outcome::Unimplemented`]. They never report a pass.
//!
//! I-6 reports *how* it held through [`Outcome::PassedWith`]: `blocked: ...` when a real
//! network block was in effect and verified, `best-effort: ...` when only the socket detector
//! ran. A fixture tagged `net` ([`InvariantInputs::tagged_net`]) is exempt.

use crate::fixture::Fixture;
use crate::snapshot::{TreeDiff, TreeSnapshot};
use scull_core::{Action, Candidate, ItemId, Tier};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// One of the universal invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InvariantId {
    /// No S3 item ever appears in an executable plan.
    I1,
    /// Canary files outside the fixture root are untouched.
    I2,
    /// Nothing inside the fixture root changes except plan items.
    I3,
    /// The audit log matches what happened on disk.
    I4,
    /// Predicted and measured bytes are both recorded.
    I5,
    /// No network access unless the fixture is tagged `net`.
    I6,
    /// Only allowlisted tools are spawned, from trusted paths.
    I7,
    /// Tiers never decrease on revalidation.
    I8,
}

impl InvariantId {
    /// All invariants in order.
    pub const ALL: [InvariantId; 8] = [
        Self::I1,
        Self::I2,
        Self::I3,
        Self::I4,
        Self::I5,
        Self::I6,
        Self::I7,
        Self::I8,
    ];

    /// One-line description.
    pub fn description(self) -> &'static str {
        match self {
            Self::I1 => "no S3 item in an executable plan",
            Self::I2 => "canary outside the root untouched",
            Self::I3 => "root unchanged except plan items",
            Self::I4 => "audit log matches disk",
            Self::I5 => "predicted and measured bytes recorded",
            Self::I6 => "no network access",
            Self::I7 => "only allowlisted tools spawned",
            Self::I8 => "tiers never decrease on revalidation",
        }
    }
}

impl fmt::Display for InvariantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = match self {
            Self::I1 => 1,
            Self::I2 => 2,
            Self::I3 => 3,
            Self::I4 => 4,
            Self::I5 => 5,
            Self::I6 => 6,
            Self::I7 => 7,
            Self::I8 => 8,
        };
        write!(f, "I-{n}")
    }
}

/// The result of checking one invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Checked and held.
    Passed,
    /// Checked and held; the note says how it was checked (e.g. I-6: `blocked: <sandbox>` or
    /// `best-effort: <detector>`), so a weaker check is never mistaken for a stronger one.
    PassedWith(String),
    /// Checked and violated.
    Failed(String),
    /// Not checked because the fixture gave nothing to check (e.g. no plan was produced).
    NotApplicable(String),
    /// The check does not exist yet, or cannot run on this host. Never counts as a pass.
    Unimplemented {
        /// The issue that will implement it, and why it did not run here.
        tracking: Cow<'static, str>,
    },
}

impl Outcome {
    /// Whether the invariant was actually evaluated (passed or failed).
    pub fn ran(&self) -> bool {
        matches!(self, Self::Passed | Self::PassedWith(_) | Self::Failed(_))
    }

    /// Whether this is a violation.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

static MISSING: Outcome = Outcome::Unimplemented {
    tracking: Cow::Borrowed("missing from report"),
};

/// Outcomes for all eight invariants of one fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantReport {
    results: Vec<(InvariantId, Outcome)>,
}

impl InvariantReport {
    /// Every `(invariant, outcome)` pair, I-1 to I-8.
    pub fn results(&self) -> &[(InvariantId, Outcome)] {
        &self.results
    }

    /// The outcome for `id`.
    pub fn outcome(&self, id: InvariantId) -> &Outcome {
        self.results
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, o)| o)
            .unwrap_or(&MISSING)
    }

    /// Invariants that were actually evaluated.
    pub fn ran(&self) -> Vec<InvariantId> {
        self.results
            .iter()
            .filter(|(_, o)| o.ran())
            .map(|(i, _)| *i)
            .collect()
    }

    /// Invariants that were violated.
    pub fn failures(&self) -> Vec<(InvariantId, &str)> {
        self.results
            .iter()
            .filter_map(|(i, o)| match o {
                Outcome::Failed(m) => Some((*i, m.as_str())),
                _ => None,
            })
            .collect()
    }

    /// No invariant was violated. (Unimplemented and not-applicable ones are not violations,
    /// but they are listed by the `Display` impl so nobody mistakes them for passes.)
    pub fn is_ok(&self) -> bool {
        self.failures().is_empty()
    }

    /// Panic with the full report if any invariant was violated.
    #[track_caller]
    pub fn assert_ok(&self) {
        assert!(self.is_ok(), "invariant violation(s):\n{self}");
    }
}

impl fmt::Display for InvariantReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (id, o) in &self.results {
            let (status, detail) = match o {
                Outcome::Passed => ("passed", String::new()),
                Outcome::PassedWith(note) => ("passed", format!(" ({note})")),
                Outcome::Failed(m) => ("FAILED", format!("\n{m}")),
                Outcome::NotApplicable(r) => ("n/a", format!(" ({r})")),
                Outcome::Unimplemented { tracking } => {
                    ("unimplemented", format!(" (tracked in {tracking})"))
                }
            };
            writeln!(f, "{id} {status:<13} {}{detail}", id.description())?;
        }
        Ok(())
    }
}

/// One item of an executable plan, as far as I-1 is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    /// Item ID, if known (for messages).
    pub id: Option<ItemId>,
    /// Tier at planning time.
    pub tier: Tier,
    /// Proposed action.
    pub action: Action,
}

impl From<&Candidate> for PlanEntry {
    fn from(c: &Candidate) -> Self {
        Self {
            id: Some(c.id.clone()),
            tier: c.tier,
            action: c.action.clone(),
        }
    }
}

impl From<Candidate> for PlanEntry {
    fn from(c: Candidate) -> Self {
        Self::from(&c)
    }
}

impl From<(Tier, Action)> for PlanEntry {
    fn from((tier, action): (Tier, Action)) -> Self {
        Self {
            id: None,
            tier,
            action,
        }
    }
}

impl From<(ItemId, Tier, Action)> for PlanEntry {
    fn from((id, tier, action): (ItemId, Tier, Action)) -> Self {
        Self {
            id: Some(id),
            tier,
            action,
        }
    }
}

/// What a fixture produced, for the invariants that need more than the filesystem.
#[derive(Debug, Clone, Default)]
pub struct InvariantInputs {
    allowed: Vec<PathBuf>,
    plans: Option<Vec<PlanEntry>>,
    tiers: Option<(BTreeMap<ItemId, Tier>, BTreeMap<ItemId, Tier>)>,
    net: bool,
}

impl InvariantInputs {
    /// Nothing produced: a read-only fixture. I-3 then requires the root to be unchanged.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow changes at or below `rel` (relative to the root) for I-3, e.g. an executed plan
    /// item or the quarantine directory.
    pub fn allow(mut self, rel: impl Into<PathBuf>) -> Self {
        self.allowed.push(rel.into());
        self
    }

    /// Add the entries of an executable plan the fixture produced (I-1). May be called once per
    /// plan.
    pub fn plan<I, E>(mut self, entries: I) -> Self
    where
        I: IntoIterator<Item = E>,
        E: Into<PlanEntry>,
    {
        self.plans
            .get_or_insert_with(Vec::new)
            .extend(entries.into_iter().map(Into::into));
        self
    }

    /// The fixture is tagged `net` (FIXTURES §1, I-6): it may use the network, so I-6 is
    /// reported as not applicable. No fixture of FIXTURES §3 is tagged `net` today; a future one
    /// belongs in its own test binary, because the I-6 socket detector sees the whole process.
    pub fn tagged_net(mut self) -> Self {
        self.net = true;
        self
    }

    /// Scan-time and execute-time tiers per item (I-8).
    pub fn tiers(mut self, scan: BTreeMap<ItemId, Tier>, execute: BTreeMap<ItemId, Tier>) -> Self {
        self.tiers = Some((scan, execute));
        self
    }
}

/// Run every invariant for `fx`: the implemented ones are evaluated, the rest are reported as
/// [`Outcome::NotApplicable`] or [`Outcome::Unimplemented`]. Marks the fixture as checked.
pub fn check_invariants(fx: &Fixture, inputs: &InvariantInputs) -> InvariantReport {
    fx.mark_checked();

    let i1 = match &inputs.plans {
        Some(p) => no_s3_in_plan(p.iter().cloned()),
        None => Outcome::NotApplicable("fixture produced no plan".into()),
    };

    let i2 = match fx.snapshot_canary() {
        Ok(after) => canary_untouched(fx.canary_baseline(), &after),
        Err(e) => Outcome::Failed(format!("could not snapshot the canary: {e}")),
    };

    let i3 = match (fx.root_baseline(), fx.snapshot_root()) {
        (None, _) => Outcome::Failed(
            "no root baseline: call `Fixture::checkpoint()` after building the fixture".into(),
        ),
        (Some(_), Err(e)) => Outcome::Failed(format!("could not snapshot the root: {e}")),
        (Some(before), Ok(after)) => root_unchanged_except(before, &after, &inputs.allowed),
    };

    let i8 = match &inputs.tiers {
        Some((scan, exec)) => tiers_never_decrease(scan, exec),
        None => Outcome::NotApplicable("fixture performed no revalidation".into()),
    };

    InvariantReport {
        results: vec![
            (InvariantId::I1, i1),
            (InvariantId::I2, i2),
            (InvariantId::I3, i3),
            (InvariantId::I4, audit_matches_disk()),
            (InvariantId::I5, bytes_recorded()),
            (
                InvariantId::I6,
                if inputs.net {
                    Outcome::NotApplicable("fixture is tagged `net`".into())
                } else {
                    network_blocked()
                },
            ),
            (InvariantId::I7, spawns_allowlisted()),
            (InvariantId::I8, i8),
        ],
    }
}

/// I-1: no entry of an executable plan is S3, whatever its action.
pub fn no_s3_in_plan<I, E>(entries: I) -> Outcome
where
    I: IntoIterator<Item = E>,
    E: Into<PlanEntry>,
{
    let bad: Vec<String> = entries
        .into_iter()
        .map(Into::into)
        .filter(|e| e.tier == Tier::S3)
        .map(|e| {
            let id = e.id.map_or_else(|| "<no id>".to_owned(), |i| i.to_string());
            format!("  {id}: S3 with action {:?}", e.action)
        })
        .collect();
    if bad.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(format!(
            "S3 item(s) in an executable plan:\n{}",
            bad.join("\n")
        ))
    }
}

/// I-2: the canary snapshot after the run equals the one before it (content hash, size, mtime,
/// identity, permissions, symlink targets; nothing added or removed).
pub fn canary_untouched(before: &TreeSnapshot, after: &TreeSnapshot) -> Outcome {
    let d = before.diff(after);
    if d.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(format!("canary {} changed:\n{d}", before.root().display()))
    }
}

/// I-3: nothing in the root changed except at or below the `allowed` relative paths.
pub fn root_unchanged_except<P: AsRef<Path>>(
    before: &TreeSnapshot,
    after: &TreeSnapshot,
    allowed: &[P],
) -> Outcome {
    let d: TreeDiff = before.diff(after).without(allowed);
    if d.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(format!(
            "unexpected change(s) under {}:\n{d}",
            before.root().display()
        ))
    }
}

/// I-8: no item's execute-time tier is lower than its scan-time tier. Items missing at execute
/// time (gone) or new at execute time are not decreases.
pub fn tiers_never_decrease(
    scan: &BTreeMap<ItemId, Tier>,
    execute: &BTreeMap<ItemId, Tier>,
) -> Outcome {
    let bad: Vec<String> = execute
        .iter()
        .filter_map(|(id, after)| {
            scan.get(id)
                .filter(|before| after < *before)
                .map(|before| format!("  {id}: {before} -> {after}"))
        })
        .collect();
    if bad.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(format!(
            "tier decreased on revalidation:\n{}",
            bad.join("\n")
        ))
    }
}

/// I-4 (stub): the audit log matches the filesystem. Needs the append-only audit log.
pub fn audit_matches_disk() -> Outcome {
    Outcome::Unimplemented {
        tracking: Cow::Borrowed("#75 (P2-EXEC-09 append-only audit log)"),
    }
}

/// I-5 (stub): predicted and measured bytes are both recorded. Needs operation records.
pub fn bytes_recorded() -> Outcome {
    Outcome::Unimplemented {
        tracking: Cow::Borrowed("#76 (P2-EXEC-10 measured free-space delta) / #53 (P1-STORE-01)"),
    }
}

/// I-6: no network access, for the current process (the caller handles the `net` tag).
///
/// * Inside a network sandbox ([`crate::net::active_sandbox`], e.g. a test re-executed by
///   [`crate::net::run_isolated`]) the block is verified with a zero-traffic route probe; the
///   result is [`Outcome::PassedWith`] `blocked: ...`, or [`Outcome::Failed`] if the sandbox is
///   claimed but a route to the outside exists.
/// * Otherwise only the best-effort socket detector runs: [`Outcome::PassedWith`]
///   `best-effort: ...` when it saw no non-loopback socket.
/// * Either way, a non-loopback socket held by this process is a [`Outcome::Failed`].
/// * Without a sandbox and without a working detector: [`Outcome::Unimplemented`] with the
///   reason. Never a pass.
pub fn network_blocked() -> Outcome {
    crate::net::check_current_process()
}

/// I-7 (stub): only allowlisted tools are spawned, from trusted paths. Needs a process-spawn
/// hook that records every child process.
pub fn spawns_allowlisted() -> Outcome {
    Outcome::Unimplemented {
        tracking: Cow::Borrowed("#78 (P2-QC-01 trusted tool resolution)"),
    }
}
