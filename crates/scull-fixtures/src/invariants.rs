//! Universal invariants I-1…I-8 (docs/FIXTURES.md §1).
//!
//! | ID | Status in this harness |
//! |---|---|
//! | I-1 | Implemented: [`no_s3_in_plan`]; needs the plan(s) a fixture produced |
//! | I-2 | Implemented: [`canary_untouched`]; always runs |
//! | I-3 | Implemented: [`root_unchanged_except`]; needs [`Fixture::checkpoint`] |
//! | I-4 | Implemented: [`audit_matches_disk`]; needs the operation's audit ([`InvariantInputs::audit`]) and [`Fixture::checkpoint`] |
//! | I-5 | Implemented: [`bytes_recorded`]; needs an operation record ([`InvariantInputs::operation_bytes`]) or explicit [`InvariantInputs::bytes`] |
//! | I-6 | Implemented: [`network_blocked`]; a verified OS sandbox, else a best-effort socket detector (see [`crate::net`]) |
//! | I-7 | Implemented: [`spawns_allowlisted`]; needs the spawn records ([`InvariantInputs::spawned`]) and the trusted directories ([`InvariantInputs::trusted_dir`]) |
//! | I-8 | Implemented: [`tiers_never_decrease`]; needs scan-time and execute-time tiers |
//!
//! An invariant whose input the fixture did not provide is [`Outcome::NotApplicable`], never a
//! pass. In particular a fixture whose code under test starts processes (Quick Clean, Git) must
//! pass the spawn records, or I-7 says `n/a`; a fixture that executes a plan through an audited
//! operation passes its audit, or I-4 says `n/a`.
//!
//! I-6 reports *how* it held through [`Outcome::PassedWith`]: `blocked: ...` when a real
//! network block was in effect and verified, `best-effort: ...` when only the socket detector
//! ran. A fixture tagged `net` ([`InvariantInputs::tagged_net`]) is exempt.

use crate::fixture::Fixture;
use crate::snapshot::{TreeDiff, TreeSnapshot};
use scull_core::{Action, Candidate, FsPath, ItemId, OperationId, Tier};
use scull_storage::{DiskExpectations, ObservedChange, OperationRecord, OperationStatus, Store};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

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
            Self::I2 => "canary outside the root untouched; nothing created outside the fixture",
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

/// The tools scull may start (SAFE-12, PRD §10.8): Git for repository state, and the package
/// managers whose own cleanup Quick Clean runs (`scull_detectors::caches::CacheTool::binary`).
/// Matched against the binary's file name without an `.exe` extension. Tool-native ML removal
/// (`ollama rm`) is not run by scull, so `ollama` is not listed; nor are `gradle` and `mvn`
/// (their caches are inspect-only: no cleanup command). Of the long-tail tool caches, Quick Clean
/// runs `ccache`, `conan`, `arduino-cli`, `deno` and `cypress` (#44); `pod`, `playwright` and
/// `puppeteer` are never started.
pub const ALLOWED_TOOLS: &[&str] = &[
    "git",
    "npm",
    "pnpm",
    "bun",
    "yarn",
    "pip",
    "uv",
    "conda",
    "go",
    "dotnet",
    "ccache",
    "conan",
    "arduino-cli",
    "deno",
    "cypress",
];

/// One process scull started, for I-7: the binary as spawned and its arguments.
///
/// Build it from whatever the code under test recorded: Quick Clean's
/// `QuickCleanReport::spawned` / `QuickCleanPlan::spawned` (`Spawn { binary, args }`), or the
/// hardened `GitRunner::resolved_binary()` (the only binary a runner ever starts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRecord {
    /// The binary path the process was started from.
    pub binary: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
}

impl SpawnRecord {
    /// A record for `binary` started with `args`.
    pub fn new<I, A>(binary: impl Into<PathBuf>, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        Self {
            binary: binary.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<PathBuf> for SpawnRecord {
    fn from(binary: PathBuf) -> Self {
        Self {
            binary,
            args: Vec::new(),
        }
    }
}

impl From<&Path> for SpawnRecord {
    fn from(binary: &Path) -> Self {
        Self::from(binary.to_path_buf())
    }
}

/// Predicted and measured bytes of one action, for I-5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BytesRecord {
    /// What the bytes belong to (an operation ID, a tool name), for messages.
    pub label: String,
    /// Predicted reclaimable bytes, if recorded.
    pub predicted: Option<u64>,
    /// Measured free-space delta, if recorded.
    pub measured: Option<i64>,
}

type Fallible<T> = std::result::Result<T, String>;

/// An executed operation's audit, read from the store when the input was added (I-4).
#[derive(Debug, Clone)]
struct AuditInput {
    operation: OperationId,
    data: Fallible<(DiskExpectations, Option<OperationStatus>)>,
}

/// Quarantine paths recorded in a store (I-2: they must lie inside the fixture's temp dir).
fn recorded_quarantine_paths(store: &Store, op: Option<&OperationId>) -> Fallible<Vec<PathBuf>> {
    store
        .quarantine_entries(op)
        .map(|es| {
            es.iter()
                .map(|e| e.entry.quarantine_path.to_path_buf())
                .collect()
        })
        .map_err(|e| format!("could not read the quarantine manifest: {e}"))
}

/// What a fixture produced, for the invariants that need more than the filesystem.
#[derive(Debug, Clone, Default)]
pub struct InvariantInputs {
    allowed: Vec<PathBuf>,
    plans: Option<Vec<PlanEntry>>,
    tiers: Option<(BTreeMap<ItemId, Tier>, BTreeMap<ItemId, Tier>)>,
    net: bool,
    audits: Vec<AuditInput>,
    quarantine_paths: Vec<Fallible<Vec<PathBuf>>>,
    bytes: Vec<Fallible<BytesRecord>>,
    spawned: Option<Vec<SpawnRecord>>,
    trusted_dirs: Vec<PathBuf>,
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

    /// The audit of operation `op` in `store` (I-4): every plan item must have exactly one
    /// outcome, consistent with the before/after snapshot of the root
    /// ([`Store::disk_expectations`]). Read now, so add it after the operation finished. May be
    /// called once per operation. A store error makes I-4 fail, never pass.
    pub fn audit(mut self, store: &Store, op: &OperationId) -> Self {
        let data = store
            .disk_expectations(op)
            .and_then(|exp| {
                let status = store
                    .operation(op)?
                    .and_then(|r| r.finished.map(|f| f.status));
                Ok((exp, status))
            })
            .map_err(|e| e.to_string());
        self.audits.push(AuditInput {
            operation: op.clone(),
            data,
        });
        self.quarantine_paths
            .push(recorded_quarantine_paths(store, Some(op)));
        self
    }

    /// Every quarantine path recorded in `store` (I-2, P2-EXEC-13): each must lie inside the
    /// fixture's temp dir. Read now, so add it after the runs. [`InvariantInputs::audit`] adds
    /// the paths of its operation already.
    pub fn quarantine(mut self, store: &Store) -> Self {
        self.quarantine_paths
            .push(recorded_quarantine_paths(store, None));
        self
    }

    /// The predicted and measured bytes recorded for operation `op` in `store` (I-5): the
    /// finished operation's totals must hold the plan's predicted bytes and a measured
    /// free-space delta (`measured_delta_bytes`). A store error, or a missing or unfinished
    /// operation, makes I-5 fail.
    pub fn operation_bytes(mut self, store: &Store, op: &OperationId) -> Self {
        let rec = match store.operation(op) {
            Ok(Some(r)) => operation_bytes_record(&r),
            Ok(None) => Err(format!("{op}: no recorded operation")),
            Err(e) => Err(format!("{op}: could not read the operation: {e}")),
        };
        self.bytes.push(rec);
        self
    }

    /// Predicted and measured bytes of an action that has no operation record, e.g. one Quick
    /// Clean tool: its reclaimable estimate and its volume's free-space delta (I-5). `None` for
    /// either makes I-5 fail.
    pub fn bytes(
        mut self,
        label: impl Into<String>,
        predicted: Option<u64>,
        measured: Option<i64>,
    ) -> Self {
        self.bytes.push(Ok(BytesRecord {
            label: label.into(),
            predicted,
            measured,
        }));
        self
    }

    /// Every process the code under test started (I-7), e.g. Quick Clean's spawn records or a
    /// Git runner's resolved binary. An empty list means "ran, started nothing" and is checked
    /// (it passes); not calling this at all leaves I-7 not applicable. May be called more than
    /// once.
    pub fn spawned<I, S>(mut self, records: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<SpawnRecord>,
    {
        self.spawned
            .get_or_insert_with(Vec::new)
            .extend(records.into_iter().map(Into::into));
        self
    }

    /// A directory the fixture treats as trusted for tool binaries (I-7): the stand-in for the
    /// system's trusted locations (e.g. the fake tools folder beside the root, or the absolute
    /// `PATH` entry Git was resolved from). A spawned binary must sit directly in one of them.
    pub fn trusted_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.trusted_dirs.push(dir.into());
        self
    }
}

fn operation_bytes_record(r: &OperationRecord) -> Fallible<BytesRecord> {
    let Some(fin) = &r.finished else {
        return Err(format!(
            "{}: operation not finished, no totals recorded",
            r.operation_id
        ));
    };
    Ok(BytesRecord {
        label: r.operation_id.to_string(),
        predicted: (fin.totals.items_planned > 0).then_some(fin.totals.predicted_bytes),
        measured: fin.totals.measured_delta_bytes,
    })
}

/// Collects the failure messages of several checks into one outcome.
fn combine(problems: Vec<String>) -> Outcome {
    if problems.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(problems.join("\n"))
    }
}

fn check_audits(fx: &Fixture, audits: &[AuditInput]) -> Outcome {
    if audits.is_empty() {
        return Outcome::NotApplicable("fixture executed no audited operation".into());
    }
    let (before, after) = match (fx.root_baseline(), fx.snapshot_root()) {
        (None, _) => {
            return Outcome::Failed(
                "no root baseline: call `Fixture::checkpoint()` after building the fixture".into(),
            );
        }
        (Some(_), Err(e)) => return Outcome::Failed(format!("could not snapshot the root: {e}")),
        (Some(before), Ok(after)) => (before, after),
    };
    let problems = audits
        .iter()
        .filter_map(|a| match &a.data {
            Err(e) => Some(format!("{}: could not read the audit: {e}", a.operation)),
            Ok((exp, status)) => match audit_matches_disk(exp, *status, before, &after) {
                Outcome::Failed(m) => Some(m),
                _ => None,
            },
        })
        .collect();
    combine(problems)
}

fn check_bytes(bytes: &[Fallible<BytesRecord>]) -> Outcome {
    if bytes.is_empty() {
        return Outcome::NotApplicable("fixture recorded no operation or byte counts".into());
    }
    let mut problems = Vec::new();
    let mut records = Vec::new();
    for b in bytes {
        match b {
            Ok(r) => records.push(r.clone()),
            Err(e) => problems.push(e.clone()),
        }
    }
    if let Outcome::Failed(m) = bytes_recorded(&records) {
        problems.push(m);
    }
    combine(problems)
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
    let i2 = match (i2, nothing_outside(fx, &inputs.quarantine_paths)) {
        (Outcome::Failed(a), Outcome::Failed(b)) => Outcome::Failed(format!("{a}\n{b}")),
        (Outcome::Failed(a), _) => Outcome::Failed(a),
        (_, Outcome::Failed(b)) => Outcome::Failed(b),
        (ok, _) => ok,
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
            (InvariantId::I4, check_audits(fx, &inputs.audits)),
            (InvariantId::I5, check_bytes(&inputs.bytes)),
            (
                InvariantId::I6,
                if inputs.net {
                    Outcome::NotApplicable("fixture is tagged `net`".into())
                } else {
                    network_blocked()
                },
            ),
            (
                InvariantId::I7,
                match &inputs.spawned {
                    None => Outcome::NotApplicable("fixture gave no spawn records".into()),
                    Some(records) => spawns_allowlisted(records, &inputs.trusted_dirs, fx.root()),
                },
            ),
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

/// I-2 (P2-EXEC-13): nothing appeared in the automatic quarantine or Trash locations above the
/// fixture's temp dir, and every recorded quarantine path lies inside it.
fn nothing_outside(fx: &Fixture, recorded: &[Fallible<Vec<PathBuf>>]) -> Outcome {
    let mut problems = Vec::new();
    let added = fx.outside_additions();
    if !added.is_empty() {
        problems.push(format!(
            "created outside the fixture's temp dir {} (automatic quarantine or Trash \
             locations):\n{}",
            fx.base().display(),
            added
                .iter()
                .map(|p| format!("  + {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    for r in recorded {
        match r {
            Err(e) => problems.push(e.clone()),
            Ok(paths) => {
                for p in paths
                    .iter()
                    .filter(|p| !crate::outside::inside(p, fx.base()))
                {
                    problems.push(format!(
                        "quarantined outside the fixture's temp dir {}: {}",
                        fx.base().display(),
                        p.display()
                    ));
                }
            }
        }
    }
    combine(problems)
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

/// `path` relative to `root`, also when one of them is spelled through a symlinked ancestor
/// (macOS `/var` vs `/private/var`). `None` when `path` is not at or below `root`.
fn strip_root(path: &Path, root: &Path) -> Option<PathBuf> {
    if let Ok(rel) = path.strip_prefix(root) {
        return Some(rel.to_path_buf());
    }
    let croot = std::fs::canonicalize(root).ok()?;
    if let Ok(rel) = path.strip_prefix(&croot) {
        return Some(rel.to_path_buf());
    }
    // The path itself may be gone; resolve its parent.
    let cparent = std::fs::canonicalize(path.parent()?).ok()?;
    let rel = cparent.strip_prefix(&croot).ok()?;
    Some(rel.join(path.file_name()?))
}

/// What the snapshots saw at the absolute `path`. `None` when the path is outside the snapshot
/// root, so nothing can be said about it.
fn observe(
    path: &Path,
    before: &TreeSnapshot,
    after: &TreeSnapshot,
    diff: &TreeDiff,
) -> Option<ObservedChange> {
    let rel = strip_root(path, before.root())?;
    Some(if before.get(&rel).is_none() {
        ObservedChange::Absent
    } else if after.get(&rel).is_none() {
        ObservedChange::Gone
    } else if diff.paths().iter().any(|p| p.starts_with(&rel)) {
        ObservedChange::Changed
    } else {
        ObservedChange::Unchanged
    })
}

/// I-4: every plan item of the operation has exactly one audit outcome, consistent with what
/// the before/after snapshots of the root saw at the item's path
/// ([`DiskExpectations::mismatches`]): removed, quarantined or trashed items are gone; failed,
/// refused and skipped items are unchanged (the path and everything below it); partial items
/// still exist with something below them changed; tool commands are not checked.
///
/// A plan item without an outcome is a violation, except in an operation that ended
/// `cancelled` or `interrupted` (`status`), where it must be unchanged on disk ("items without
/// an outcome were not touched"). An item path outside the snapshot root is a violation: the
/// fixture cannot vouch for it.
pub fn audit_matches_disk(
    exp: &DiskExpectations,
    status: Option<OperationStatus>,
    before: &TreeSnapshot,
    after: &TreeSnapshot,
) -> Outcome {
    let diff = before.diff(after);
    let mut problems = Vec::new();
    let mut exp = exp.clone();
    if matches!(
        status,
        Some(OperationStatus::Cancelled | OperationStatus::Interrupted)
    ) {
        for (id, path) in std::mem::take(&mut exp.without_outcome) {
            match observe(&path.to_path_buf(), before, after, &diff) {
                Some(ObservedChange::Unchanged) => {}
                seen => problems.push(format!(
                    "{id} ({path}): no audit outcome in a stopped operation, but disk shows {seen:?}"
                )),
            }
        }
    }
    let mut outside = Vec::new();
    problems.extend(exp.mismatches(|p: &FsPath| {
        observe(&p.to_path_buf(), before, after, &diff).unwrap_or_else(|| {
            outside.push(p.to_string());
            ObservedChange::Absent
        })
    }));
    problems.extend(outside.into_iter().map(|p| {
        format!(
            "{p}: outside the fixture root {}, cannot be checked",
            before.root().display()
        )
    }));
    if problems.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(format!(
            "audit of {} disagrees with the disk:\n  {}",
            exp.operation_id,
            problems.join("\n  ")
        ))
    }
}

/// I-5: every record holds both a predicted and a measured byte count.
pub fn bytes_recorded(records: &[BytesRecord]) -> Outcome {
    let shown = |b: Option<String>| b.unwrap_or_else(|| "not recorded".to_owned());
    let bad: Vec<String> = records
        .iter()
        .filter(|r| r.predicted.is_none() || r.measured.is_none())
        .map(|r| {
            format!(
                "  {}: predicted {}, measured {}",
                r.label,
                shown(r.predicted.map(|b| format!("{b} B"))),
                shown(r.measured.map(|b| format!("{b:+} B"))),
            )
        })
        .collect();
    if bad.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(format!(
            "predicted and measured bytes not both recorded:\n{}",
            bad.join("\n")
        ))
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

/// The tool name of `binary`: its file name without an `.exe` extension (any case).
fn tool_name(binary: &Path) -> Option<&str> {
    let name = binary.file_name()?.to_str()?;
    let cut = name.len().checked_sub(4).filter(|&i| {
        name.get(i..)
            .is_some_and(|ext| ext.eq_ignore_ascii_case(".exe"))
    });
    Some(cut.map_or(name, |i| &name[..i]))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Whether `binary` is what the entry of the same name in the trusted directory `dir` resolves
/// to: a link there (Homebrew's `/opt/homebrew/bin/git` → `/opt/homebrew/Cellar/git/<v>/bin/git`)
/// that the hardened runners follow and then start by its real path, so a swap of the link
/// can't change what runs. Only that exact entry counts, never anything else in its folder.
fn reached_through(dir: &Path, binary: &Path) -> bool {
    let Some(name) = binary.file_name() else {
        return false;
    };
    matches!(
        (std::fs::canonicalize(dir.join(name)), std::fs::canonicalize(binary)),
        (Ok(a), Ok(b)) if a == b
    )
}

/// The `node` rule of [`spawns_allowlisted`]: the script it runs.
fn node_runs_allowlisted_script<P: AsRef<Path>>(
    r: &SpawnRecord,
    trusted: &[P],
    root: &Path,
) -> std::result::Result<(), String> {
    let script = r
        .args
        .first()
        .map(PathBuf::from)
        .ok_or("node started without a script")?;
    let js = script
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["js", "cjs", "mjs"].contains(&e.to_ascii_lowercase().as_str()));
    if !script.is_absolute() || !js {
        return Err(format!(
            "node's first argument {} is not an absolute JavaScript file",
            script.display()
        ));
    }
    if strip_root(&script, root).is_some() {
        return Err(format!(
            "node's script is inside the scanned root {}",
            root.display()
        ));
    }
    let real = std::fs::canonicalize(&script).map_err(|e| format!("node's script: {e}"))?;
    let below = trusted.iter().find_map(|t| {
        let t = std::fs::canonicalize(t.as_ref()).ok()?;
        real.strip_prefix(&t).ok().map(Path::to_path_buf)
    });
    let Some(below) = below else {
        return Err("node's script is not inside a trusted directory".to_owned());
    };
    let names_tool = below.components().any(|c| match c {
        Component::Normal(n) => n.to_str().is_some_and(|n| ALLOWED_TOOLS.contains(&n)),
        _ => false,
    }) || below
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.split(['-', '.']).next())
        .is_some_and(|s| ALLOWED_TOOLS.contains(&s));
    if !names_tool {
        return Err(format!(
            "node's script {} is not an allowlisted tool's",
            below.display()
        ));
    }
    Ok(())
}

/// I-7: every spawned process is an allowlisted tool ([`ALLOWED_TOOLS`]) started by absolute
/// path from a trusted directory: the binary sits directly in one of `trusted` (or is the real
/// file the same-named entry there links to, as Homebrew installs tools), no component of
/// its path is `node_modules`, and it is not inside `root` (the scanned tree; project content is
/// never trusted, SAFE-12). With spawn records but no trusted directory declared, nothing is
/// trusted and every spawn fails.
///
/// `node` counts as an allowlisted tool only when it runs one (a Windows `.cmd` shim resolved
/// by Quick Clean, P2-QC-02): its first argument must be an absolute `.js`/`.cjs`/`.mjs` file
/// inside one of `trusted` (after resolving links), outside `root`, whose path below that
/// directory names an allowlisted tool (a folder such as `node_modules/npm/`, or the file's
/// stem such as `yarn.js` / `npm-cli.js`). Node itself must pass every rule above.
pub fn spawns_allowlisted<P: AsRef<Path>>(
    records: &[SpawnRecord],
    trusted: &[P],
    root: &Path,
) -> Outcome {
    let mut bad = Vec::new();
    for r in records {
        let b = &r.binary;
        let mut why = Vec::new();
        match tool_name(b) {
            Some(n) if ALLOWED_TOOLS.contains(&n) => {}
            Some("node") => {
                if let Err(e) = node_runs_allowlisted_script(r, trusted, root) {
                    why.push(e);
                }
            }
            n => why.push(format!("{n:?} is not an allowlisted tool")),
        }
        if !b.is_absolute() {
            why.push("not an absolute path".to_owned());
        }
        if b.components()
            .any(|c| matches!(c, Component::Normal(n) if n == "node_modules"))
        {
            why.push("inside a node_modules folder".to_owned());
        }
        if strip_root(b, root).is_some() {
            why.push(format!("inside the scanned root {}", root.display()));
        }
        let in_trusted = b
            .parent()
            .is_some_and(|dir| trusted.iter().any(|t| same_dir(dir, t.as_ref())))
            || trusted.iter().any(|t| reached_through(t.as_ref(), b));
        if !in_trusted {
            why.push(if trusted.is_empty() {
                "no trusted directory declared".to_owned()
            } else {
                "not in a trusted directory".to_owned()
            });
        }
        if !why.is_empty() {
            bad.push(format!(
                "  {} {:?}: {}",
                b.display(),
                r.args,
                why.join("; ")
            ));
        }
    }
    if bad.is_empty() {
        Outcome::Passed
    } else {
        Outcome::Failed(format!(
            "untrusted or non-allowlisted process(es) spawned:\n{}",
            bad.join("\n")
        ))
    }
}
