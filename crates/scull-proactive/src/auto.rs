//! Opt-in unattended auto-policies (PRO-10, decision D-11; threat T-05): the decisions.
//!
//! Pure logic over the policies in `[auto]` (user config file only, see
//! [`scull_core::config::AutoConfig`]), a scan's candidates, the policy state the CLI keeps, and
//! the audit log. Nothing here touches the disk or runs anything: the scheduled `scull check`
//! runs what [`select`] and [`take`] chose through the Executor (plan → per-item revalidation →
//! quarantine, under the execution lock, fully audited).
//!
//! # What a policy may choose ([`select`])
//!
//! A [`AutoKind::BuildOutputs`] policy chooses a scan candidate only when **all** hold:
//!
//! * category `project_artifact`, tier **S0**, action `delete` (what the classifier proposes
//!   for a regenerable build output), and the candidate is consistent;
//! * no flag says it is in use, hardlinked, a link or containing links, or a cloud
//!   placeholder, and it is known (`false`, not merely unset) not to cross a mount;
//! * its last activity is known (a bounded mtime sample counts as unknown) and older than the
//!   policy's `idle` window, and nothing recorded it in use;
//! * it lies inside one of the policy's `under` folders, when it lists any;
//! * its size is known (allocated bytes above zero, a reclaim estimate that isn't "unknown");
//! * the user did not restore it from quarantine within the idle window;
//! * nothing else in its project (sources, manifests, Git metadata) changed within the idle
//!   window ([`ProjectActivity`], gathered by the caller; "can't tell" counts as active).
//!
//! The chosen items are planned as **quarantine** (never delete), oldest activity first.
//!
//! # Caps ([`Caps`], [`take`])
//!
//! Caps are charged the allocated bytes (what a quarantine move holds).
//! `max_items_per_run` and `max_gib_per_run` bound one run (all policies together);
//! `max_gib_per_day` bounds what auto-policies moved within the last 24 hours, read from the
//! audit log ([`day_usage`], so deleting scull's state file does not reset it). An item that
//! doesn't fit is deferred (reported, not run); smaller ones after it may still fit. A cap
//! above its hard ceiling makes every policy refuse instead of being clamped ([`Caps::new`]).
//!
//! # Dry run first ([`due`])
//!
//! A policy whose definition (kind, windows, folders, the caps, and the config file's scan
//! roots and enabled rules, [`Scope`]) has not been previewed runs
//! as a **dry run**: what it would clean is recorded and reported, nothing is touched. It acts
//! only on a later check, at least one `every` interval after that preview, and again at most
//! once per `every`. Editing the policy (or the caps) starts over with a new preview.

use schemars::JsonSchema;
use scull_core::config::{
    AutoConfig, AutoKind, AutoPolicy, MAX_GIB_PER_DAY_CEILING, MAX_GIB_PER_RUN_CEILING,
    MAX_ITEMS_PER_RUN_CEILING, MIN_EVERY, MIN_IDLE,
};
use scull_core::{Action, Candidate, Category, Confidence, ItemId, Tier};
use scull_storage::{ApprovalMethod, ItemOutcome, Store, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::policy::activity_from_candidate;

/// One GiB.
pub const GIB: u64 = 1024 * 1024 * 1024;
/// The window [`day_usage`] counts: 24 hours.
pub const DAY_SECS: i64 = 24 * 60 * 60;
/// Version of [`AutoState`] this build reads and writes.
pub const STATE_VERSION: u32 = 1;
/// Digests kept in [`AutoState::digests`].
pub const DIGESTS_KEPT: usize = 20;
/// Longest accepted policy name.
pub const MAX_NAME_LEN: usize = 64;

/// The caps of a run, in items and bytes (from `[auto]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Caps {
    /// Items per run, all policies together.
    pub items_per_run: u64,
    /// Bytes per run, all policies together.
    pub bytes_per_run: u64,
    /// Bytes within any 24 hours, all runs together.
    pub bytes_per_day: u64,
}

impl Caps {
    /// The caps of `auto`, or why they are refused (a value above its hard ceiling: refused,
    /// never clamped, so a typo can't silently mean "a lot").
    pub fn new(auto: &AutoConfig) -> Result<Caps, String> {
        let over = |key: &str, v: u32, max: u32| {
            (v > max).then(|| format!("[auto] {key} = {v} is above the limit of {max}"))
        };
        if let Some(why) = over(
            "max_items_per_run",
            auto.max_items_per_run,
            MAX_ITEMS_PER_RUN_CEILING,
        )
        .or_else(|| {
            over(
                "max_gib_per_run",
                auto.max_gib_per_run,
                MAX_GIB_PER_RUN_CEILING,
            )
        })
        .or_else(|| {
            over(
                "max_gib_per_day",
                auto.max_gib_per_day,
                MAX_GIB_PER_DAY_CEILING,
            )
        }) {
            return Err(why);
        }
        Ok(Caps {
            items_per_run: u64::from(auto.max_items_per_run),
            bytes_per_run: u64::from(auto.max_gib_per_run) * GIB,
            bytes_per_day: u64::from(auto.max_gib_per_day) * GIB,
        })
    }
}

/// Why a policy definition can't run (`None`: it is valid). `all` is every policy, for the
/// unique-name check.
pub fn invalid(policy: &AutoPolicy, all: &[AutoPolicy]) -> Option<String> {
    let name = &policy.name;
    if name.is_empty()
        || name.len() > MAX_NAME_LEN
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Some(format!(
            "the name must be 1 to {MAX_NAME_LEN} letters, digits, `.`, `_` or `-`"
        ));
    }
    if all.iter().filter(|p| p.name == *name).count() > 1 {
        return Some("another policy has the same name".into());
    }
    if policy.idle < MIN_IDLE {
        return Some(format!(
            "idle = \"{}\" is shorter than the minimum of {}",
            policy.idle, MIN_IDLE
        ));
    }
    if policy.every < MIN_EVERY {
        return Some(format!(
            "every = \"{}\" is shorter than the minimum of {}",
            policy.every, MIN_EVERY
        ));
    }
    if let Some(u) = policy.under.iter().find(|u| !u.is_absolute()) {
        return Some(format!(
            "under = {:?} is not an absolute folder (`~` is allowed)",
            u.to_string_lossy()
        ));
    }
    None
}

/// The text a preview is bound to: the policy (kind, windows, folders) and the caps. A change
/// to either needs a new dry run ([`due`]).
pub fn definition(policy: &AutoPolicy, caps: &Caps, scope: &Scope<'_>) -> String {
    #[derive(Serialize)]
    struct Def<'a> {
        name: &'a str,
        kind: AutoKind,
        idle_secs: u64,
        every_secs: u64,
        under: &'a [PathBuf],
        caps: &'a Caps,
        roots: Vec<&'a PathBuf>,
        rules: Vec<&'a String>,
    }
    let mut roots: Vec<&PathBuf> = scope.roots.iter().collect();
    roots.sort();
    roots.dedup();
    let mut rules: Vec<&String> = scope.rules.iter().collect();
    rules.sort();
    rules.dedup();
    serde_json::to_string(&Def {
        name: &policy.name,
        kind: policy.kind,
        idle_secs: policy.idle.as_secs(),
        every_secs: policy.every.as_secs(),
        under: &policy.under,
        caps,
        roots,
        rules,
    })
    .unwrap_or_default()
}

/// What a preview covers besides the policy and the caps: the config file's scan roots
/// (canonical) and its enabled optional rules. Adding a root or a rule after a dry run means a
/// new dry run, so nothing is ever cleaned that no preview showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope<'a> {
    /// `scan.roots` of the config file, canonical.
    pub roots: &'a [PathBuf],
    /// `rules.enabled` of the config file.
    pub rules: &'a [String],
}

/// How recently a project's own files (outside the item) changed, for [`Context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectActivity {
    /// Nothing in the project changed at or after the cutoff (or the item has no project).
    Idle,
    /// Something in the project changed at or after the cutoff.
    Active,
    /// Could not tell (too many entries, unreadable): never counted as idle.
    Unknown,
}

/// What [`select`] needs besides the policy and the candidates.
pub struct Context<'a> {
    /// IDs another policy of the same run already took.
    pub taken: &'a BTreeSet<&'a ItemId>,
    /// Paths the user restored from quarantine, with when (any operation): an item restored
    /// within the policy's idle window is not taken again.
    pub restored: &'a BTreeMap<String, i64>,
    /// Whether the candidate's project (other than the item itself) changed at or after the
    /// given cutoff (Unix seconds).
    pub project_activity: &'a (dyn Fn(&Candidate, i64) -> ProjectActivity + Sync),
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("taken", &self.taken.len())
            .field("restored", &self.restored.len())
            .finish_non_exhaustive()
    }
}

/// Why candidates of the policy's kind were not chosen, counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Excluded {
    /// Not S0 (S1–S3 are never cleaned unattended).
    pub not_s0: u64,
    /// S0, but its action is not the plain filesystem delete the policy turns into a
    /// quarantine, or its tier, reasons and action disagree.
    pub other_action: u64,
    /// In use, or flagged hardlinked, link, cloud placeholder or cross-mount (or not known
    /// not to cross a mount).
    pub flagged: u64,
    /// Last activity unknown (never counted as idle).
    pub activity_unknown: u64,
    /// Active within the policy's idle window.
    pub recent: u64,
    /// Outside the policy's `under` folders.
    pub outside: u64,
    /// Its size is unknown or zero (nothing to charge to the caps honestly).
    pub size_unknown: u64,
    /// The user restored it from quarantine within the policy's idle window.
    pub recently_restored: u64,
    /// Its project's own files changed within the idle window, or that could not be told.
    pub project_active: u64,
}

/// A candidate a policy chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen<'a> {
    /// The candidate.
    pub candidate: &'a Candidate,
    /// Its last activity (Unix seconds).
    pub last_activity_at: i64,
    /// Bytes it would move (the candidate's allocated size: what a quarantine holds).
    pub bytes: u64,
}

/// What [`select`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection<'a> {
    /// Chosen candidates, oldest activity first (then by path).
    pub chosen: Vec<Chosen<'a>>,
    /// Candidates of the policy's kind left out, counted by reason.
    pub excluded: Excluded,
}

/// The candidates `policy` may clean at `now` (see the [module docs](self)).
pub fn select<'a>(
    policy: &AutoPolicy,
    candidates: &'a [Candidate],
    ctx: &Context<'_>,
    now: i64,
) -> Selection<'a> {
    let idle = i64::try_from(policy.idle.as_secs()).unwrap_or(i64::MAX);
    let cutoff = now.saturating_sub(idle);
    let skip = ctx.taken;
    let mut sel = Selection::default();
    let x = &mut sel.excluded;
    for c in candidates {
        let of_kind = match policy.kind {
            AutoKind::BuildOutputs => c.category == Category::ProjectArtifact,
        };
        if !of_kind || skip.contains(&c.id) {
            continue;
        }
        if !policy.under.is_empty() {
            let p = c.path.to_path_buf();
            if !policy.under.iter().any(|u| p.starts_with(u)) {
                x.outside += 1;
                continue;
            }
        }
        if c.tier != Tier::S0 {
            x.not_s0 += 1;
            continue;
        }
        if c.action != Action::Delete || !c.is_consistent() {
            x.other_action += 1;
            continue;
        }
        let f = &c.flags;
        if [f.active, f.hardlinked, f.cloud_placeholder, f.symlink].contains(&Some(true))
            || f.cross_mount != Some(false)
        {
            x.flagged += 1;
            continue;
        }
        let Some(activity) = activity_from_candidate(c) else {
            x.other_action += 1;
            continue;
        };
        if activity.in_use {
            x.flagged += 1;
            continue;
        }
        let Some(last) = activity.last_activity_at else {
            x.activity_unknown += 1;
            continue;
        };
        if now.saturating_sub(last) <= idle {
            x.recent += 1;
            continue;
        }
        // What moves is the allocated size; an unknown estimate can't be charged honestly.
        if c.sizes.allocated == 0 || c.sizes.reclaimable.confidence == Confidence::Unknown {
            x.size_unknown += 1;
            continue;
        }
        if ctx
            .restored
            .get(c.path.as_str())
            .is_some_and(|at| *at >= cutoff)
        {
            x.recently_restored += 1;
            continue;
        }
        if (ctx.project_activity)(c, cutoff) != ProjectActivity::Idle {
            x.project_active += 1;
            continue;
        }
        sel.chosen.push(Chosen {
            candidate: c,
            last_activity_at: last,
            bytes: c.sizes.allocated,
        });
    }
    sel.chosen.sort_by(|a, b| {
        a.last_activity_at
            .cmp(&b.last_activity_at)
            .then_with(|| a.candidate.path.cmp(&b.candidate.path))
    });
    sel
}

/// Whether the freshly re-derived candidate `c` still qualifies for `policy` at `now` (the
/// same rules as [`select`]), checked right before each move: `Err((code, message))` says why
/// not, e.g. `no_longer_idle` after a source edit since the scan.
pub fn still_qualifies(
    policy: &AutoPolicy,
    c: &Candidate,
    ctx: &Context<'_>,
    now: i64,
) -> Result<(), (&'static str, String)> {
    let none = BTreeSet::new();
    let ctx = Context {
        taken: &none,
        ..*ctx
    };
    let sel = select(policy, std::slice::from_ref(c), &ctx, now);
    if !sel.chosen.is_empty() {
        return Ok(());
    }
    let x = sel.excluded;
    let name = &policy.name;
    Err(if x.recent > 0 {
        (
            "no_longer_idle",
            format!("active within auto-policy {name}'s idle window since the scan"),
        )
    } else if x.not_s0 > 0 {
        ("no_longer_s0", format!("no longer S0 ({})", c.tier))
    } else if x.flagged > 0 {
        ("flagged", "now in use or flagged since the scan".to_owned())
    } else if x.activity_unknown > 0 {
        (
            "activity_unknown",
            "its last activity is no longer known".to_owned(),
        )
    } else if x.project_active > 0 {
        (
            "project_active",
            format!("its project changed within auto-policy {name}'s idle window"),
        )
    } else if x.recently_restored > 0 {
        (
            "recently_restored",
            "you restored it from quarantine recently".to_owned(),
        )
    } else if x.size_unknown > 0 {
        ("size_unknown", "its size is no longer known".to_owned())
    } else if x.outside > 0 {
        (
            "outside_policy_folders",
            format!("outside auto-policy {name}'s folders"),
        )
    } else {
        (
            "no_longer_qualifies",
            format!("no longer qualifies for auto-policy {name}"),
        )
    })
}

/// What is left of the caps for the rest of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Items.
    pub items: u64,
    /// Bytes.
    pub bytes: u64,
}

impl Budget {
    /// The budget of a run that starts after auto-policies moved `day_used_bytes` within the
    /// last 24 hours.
    pub fn for_run(caps: &Caps, day_used_bytes: u64) -> Budget {
        Budget {
            items: caps.items_per_run,
            bytes: caps
                .bytes_per_run
                .min(caps.bytes_per_day.saturating_sub(day_used_bytes)),
        }
    }
}

/// Take what fits in `budget` from `chosen` (in order) and charge it. Returns what fits and how
/// many items were deferred by the caps.
pub fn take<'a>(chosen: Vec<Chosen<'a>>, budget: &mut Budget) -> (Vec<Chosen<'a>>, u64) {
    let mut fit = Vec::new();
    let mut deferred = 0;
    for c in chosen {
        if budget.items > 0 && c.bytes > 0 && c.bytes <= budget.bytes {
            budget.items -= 1;
            budget.bytes -= c.bytes;
            fit.push(c);
        } else {
            deferred += 1;
        }
    }
    (fit, deferred)
}

/// What auto-policies changed within the last 24 hours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// Items quarantined, moved to the Trash or otherwise changed.
    pub items: u64,
    /// Their bytes.
    pub bytes: u64,
}

/// What auto-policy operations ([`ApprovalMethod::AutoPolicy`]) that started within
/// [`DAY_SECS`] before `now` changed, from the audit log (unfinished operations included).
pub fn day_usage(store: &Store, now: i64) -> Result<Usage, StoreError> {
    let mut u = Usage::default();
    for op in store.operations(now.saturating_sub(DAY_SECS))? {
        if op.method != ApprovalMethod::AutoPolicy {
            continue;
        }
        for item in store.audit_items(&op.operation_id)? {
            if matches!(
                item.outcome,
                ItemOutcome::Quarantined
                    | ItemOutcome::MovedToTrash
                    | ItemOutcome::Removed
                    | ItemOutcome::Partial
                    | ItemOutcome::ToolRun
            ) {
                u.items += 1;
                u.bytes = u.bytes.saturating_add(
                    item.moved_bytes
                        .or(item.freed_bytes)
                        .unwrap_or(item.predicted_bytes),
                );
            }
        }
    }
    Ok(u)
}

/// What scull remembers about auto-policies between runs (kept by the CLI, owner-only, in its
/// data directory). Losing it is safe: every policy starts over with a dry run, and the daily
/// cap is read from the audit log, not from here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutoState {
    /// [`STATE_VERSION`].
    pub version: u32,
    /// Per policy name.
    pub policies: BTreeMap<String, PolicyState>,
    /// The latest run digests, newest last (at most [`DIGESTS_KEPT`]).
    pub digests: Vec<Digest>,
}

impl Default for AutoState {
    fn default() -> Self {
        AutoState {
            version: STATE_VERSION,
            policies: BTreeMap::new(),
            digests: Vec::new(),
        }
    }
}

impl AutoState {
    /// Append a digest, keeping the latest [`DIGESTS_KEPT`].
    pub fn push_digest(&mut self, d: Digest) {
        self.digests.push(d);
        let extra = self.digests.len().saturating_sub(DIGESTS_KEPT);
        self.digests.drain(..extra);
    }
}

/// One policy's memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyState {
    /// The [`definition`] the dry run was made for.
    pub definition: String,
    /// When the dry run was made.
    pub previewed_at: i64,
    /// Items the dry run would have cleaned.
    pub preview_items: u64,
    /// Their bytes.
    pub preview_bytes: u64,
    /// When the policy last ran for real.
    pub last_run_at: Option<i64>,
}

/// The digest of one policy run or dry run (D-11: every run sends a digest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Digest {
    /// When (Unix seconds).
    pub at: i64,
    /// The policy.
    pub policy: String,
    /// The audited operation (`scull history <id>`); `None` for a dry run.
    pub operation_id: Option<String>,
    /// Items moved into quarantine.
    pub quarantined: u64,
    /// Items moved to the Trash (quarantine impossible there).
    pub trashed: u64,
    /// Items skipped, refused or blocked (left in place).
    pub not_done: u64,
    /// Bytes moved.
    pub bytes: u64,
    /// One sentence for people.
    pub message: String,
}

/// What a valid, enabled policy does at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Dry run: record what it would do, touch nothing.
    Preview,
    /// Not yet (`next_at`: when it may run).
    NotDue {
        /// Unix seconds.
        next_at: i64,
    },
    /// Run for real.
    Run,
}

/// Whether the policy with `definition` previews, waits or runs at `now`, given its state.
pub fn due(policy: &AutoPolicy, definition: &str, state: Option<&PolicyState>, now: i64) -> Due {
    let every = i64::try_from(policy.every.as_secs()).unwrap_or(i64::MAX);
    let Some(s) = state.filter(|s| s.definition == definition) else {
        return Due::Preview;
    };
    let since = s.last_run_at.unwrap_or(s.previewed_at).max(s.previewed_at);
    let next_at = since.saturating_add(every);
    if now < next_at {
        Due::NotDue { next_at }
    } else {
        Due::Run
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::config::DurationSpec;
    use scull_core::{
        Evidence, FileIdentity, Flags, FsPath, ItemId, ReasonCode, Regeneration, RuleRef, Sizes,
        TierReason,
    };

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn policy() -> AutoPolicy {
        AutoPolicy {
            name: "old".into(),
            enabled: true,
            kind: AutoKind::BuildOutputs,
            idle: DurationSpec::from_hours(60 * 24),
            every: DurationSpec::from_hours(7 * 24),
            under: Vec::new(),
        }
    }

    fn cand(n: u128, tier: Tier, idle_days: i64, bytes: u64) -> Candidate {
        let identity = FileIdentity::new(1, n);
        let path = format!("/w/p{n}/target");
        let action = Action::default_for(tier);
        Candidate {
            id: ItemId::derive(identity, path.as_bytes()),
            path: FsPath::from(path.as_str()),
            identity,
            category: Category::ProjectArtifact,
            artifact: "Cargo build output".into(),
            rule: RuleRef {
                id: "rust.target".into(),
                version: 1,
            },
            project: None,
            tier,
            reasons: vec![TierReason::new(tier, ReasonCode::Regenerable, "t")],
            evidence: vec![Evidence::LastModified {
                newest_unix: NOW - idle_days * DAY,
                sampled: false,
            }],
            sizes: Sizes {
                logical: bytes,
                allocated: bytes,
                reclaimable: scull_core::Reclaimable {
                    bytes,
                    confidence: scull_core::Confidence::High,
                    shared_bytes: 0,
                },
            },
            flags: Flags {
                active: Some(false),
                hardlinked: Some(false),
                cross_mount: Some(false),
                symlink: Some(false),
                cloud_placeholder: Some(false),
            },
            regeneration: Regeneration::default(),
            action,
        }
    }

    fn idle_project(_: &Candidate, _: i64) -> ProjectActivity {
        ProjectActivity::Idle
    }

    fn ctx<'a>(
        taken: &'a BTreeSet<&'a ItemId>,
        restored: &'a BTreeMap<String, i64>,
    ) -> Context<'a> {
        Context {
            taken,
            restored,
            project_activity: &idle_project,
        }
    }

    fn sel<'a>(p: &AutoPolicy, cands: &'a [Candidate]) -> Selection<'a> {
        select(p, cands, &ctx(&BTreeSet::new(), &BTreeMap::new()), NOW)
    }

    fn names(sel: &Selection<'_>) -> Vec<String> {
        sel.chosen
            .iter()
            .map(|c| c.candidate.path.as_str().to_owned())
            .collect()
    }

    /// Only idle, unflagged S0 build outputs known not to cross a mount are chosen; S1–S3,
    /// recent, unknown-activity, in-use, cross-mount, link, hardlinked and cloud items are not.
    #[test]
    fn select_takes_only_idle_unflagged_s0_build_outputs() {
        let mut cands = vec![
            cand(1, Tier::S0, 90, 10),
            cand(2, Tier::S1, 90, 10),
            cand(3, Tier::S2, 90, 10),
            cand(4, Tier::S3, 90, 10),
            cand(5, Tier::S0, 10, 10),
            cand(6, Tier::S0, 120, 10),
        ];
        let mut unknown = cand(7, Tier::S0, 90, 10);
        unknown.evidence = vec![Evidence::LastModified {
            newest_unix: NOW - 90 * DAY,
            sampled: true,
        }];
        cands.push(unknown);
        for (n, set) in [
            (
                8u128,
                (|f: &mut Flags| f.active = Some(true)) as fn(&mut Flags),
            ),
            (9, |f| f.cross_mount = Some(true)),
            (10, |f| f.cross_mount = None),
            (11, |f| f.symlink = Some(true)),
            (12, |f| f.hardlinked = Some(true)),
            (13, |f| f.cloud_placeholder = Some(true)),
        ] {
            let mut c = cand(n, Tier::S0, 90, 10);
            set(&mut c.flags);
            cands.push(c);
        }
        let mut in_use = cand(15, Tier::S0, 90, 10);
        in_use.evidence.push(Evidence::InUse {
            signal: "cargo lock".into(),
            by: None,
        });
        cands.push(in_use);
        let mut cache = cand(16, Tier::S0, 90, 10);
        cache.category = Category::PackageCache;
        cands.push(cache);
        let mut worktree = cand(17, Tier::S0, 90, 10);
        worktree.category = Category::Worktree;
        cands.push(worktree);

        let s = sel(&policy(), &cands);
        assert_eq!(names(&s), ["/w/p6/target", "/w/p1/target"], "oldest first");
        let x = &s.excluded;
        assert_eq!(x.not_s0, 3);
        assert_eq!(x.recent, 1);
        assert_eq!(x.activity_unknown, 1);
        assert_eq!(x.flagged, 7);

        // `under` narrows; IDs taken by another policy are skipped.
        let mut p = policy();
        p.under = vec![PathBuf::from("/w/p1")];
        let s = sel(&p, &cands);
        assert_eq!(names(&s), ["/w/p1/target"]);
        assert!(s.excluded.outside > 0);
        let taken: BTreeSet<&ItemId> = [&cands[0].id].into_iter().collect();
        assert_eq!(
            names(&select(
                &policy(),
                &cands,
                &ctx(&taken, &BTreeMap::new()),
                NOW
            )),
            ["/w/p6/target"]
        );
    }

    /// An unknown or zero size, a recent restore, or an active (or unknown) project keeps an
    /// otherwise idle S0 item out (review L3, L4, L5).
    #[test]
    fn size_restores_and_project_activity_keep_items_out() {
        let mut zero = cand(1, Tier::S0, 90, 0);
        zero.sizes.reclaimable.bytes = 0;
        let mut unknown = cand(2, Tier::S0, 90, 10);
        unknown.sizes.reclaimable.confidence = Confidence::Unknown;
        let restored_recently = cand(3, Tier::S0, 90, 10);
        let restored_long_ago = cand(4, Tier::S0, 90, 10);
        let active = cand(5, Tier::S0, 90, 10);
        let unsure = cand(6, Tier::S0, 90, 10);
        let cands = vec![
            zero,
            unknown,
            restored_recently,
            restored_long_ago,
            active,
            unsure,
        ];
        let restored: BTreeMap<String, i64> = [
            ("/w/p3/target".to_owned(), NOW - DAY),
            ("/w/p4/target".to_owned(), NOW - 70 * DAY),
        ]
        .into_iter()
        .collect();
        let project = |c: &Candidate, cutoff: i64| {
            assert_eq!(cutoff, NOW - 60 * DAY);
            match c.path.as_str() {
                "/w/p5/target" => ProjectActivity::Active,
                "/w/p6/target" => ProjectActivity::Unknown,
                _ => ProjectActivity::Idle,
            }
        };
        let taken = BTreeSet::new();
        let ctx = Context {
            taken: &taken,
            restored: &restored,
            project_activity: &project,
        };
        let s = select(&policy(), &cands, &ctx, NOW);
        assert_eq!(names(&s), ["/w/p4/target"]);
        assert_eq!(s.chosen[0].bytes, 10, "charged the allocated size");
        assert_eq!(s.excluded.size_unknown, 2);
        assert_eq!(s.excluded.recently_restored, 1);
        assert_eq!(s.excluded.project_active, 2);
        assert_eq!(
            still_qualifies(&policy(), &cands[4], &ctx, NOW)
                .unwrap_err()
                .0,
            "project_active"
        );
        assert_eq!(
            still_qualifies(&policy(), &cands[2], &ctx, NOW)
                .unwrap_err()
                .0,
            "recently_restored"
        );
        // A zero daily budget takes nothing.
        let mut b = Budget {
            items: 10,
            bytes: 0,
        };
        let (fit, deferred) = take(s.chosen, &mut b);
        assert!(fit.is_empty());
        assert_eq!(deferred, 1);
    }

    /// Caps: items and bytes per run, bytes per day; a big item is deferred, smaller ones after
    /// it still fit; an exhausted day cap takes nothing.
    #[test]
    fn caps_defer_what_does_not_fit() {
        let cands: Vec<Candidate> = (1..=4)
            .map(|n| cand(n, Tier::S0, 100 - n as i64, if n == 2 { 500 } else { 100 }))
            .collect();
        let s = sel(&policy(), &cands);
        let caps = Caps {
            items_per_run: 10,
            bytes_per_run: 350,
            bytes_per_day: 10_000,
        };
        let mut b = Budget::for_run(&caps, 0);
        let (fit, deferred) = take(s.chosen.clone(), &mut b);
        assert_eq!(fit.len(), 3);
        assert_eq!(deferred, 1, "the 500-byte item does not fit 350");
        assert_eq!(b.bytes, 50);

        let caps = Caps {
            items_per_run: 2,
            ..caps
        };
        let mut b = Budget::for_run(&caps, 0);
        let (fit, deferred) = take(s.chosen.clone(), &mut b);
        assert_eq!((fit.len(), deferred), (2, 2));

        let caps = Caps {
            items_per_run: 10,
            bytes_per_run: 10_000,
            bytes_per_day: 1_000,
        };
        let mut b = Budget::for_run(&caps, 1_000);
        assert_eq!(b.bytes, 0);
        let (fit, deferred) = take(s.chosen, &mut b);
        assert!(fit.is_empty());
        assert_eq!(deferred, 4);
    }

    #[test]
    fn caps_above_their_ceiling_are_refused_not_clamped() {
        let mut a = AutoConfig::default();
        let c = Caps::new(&a).unwrap();
        assert_eq!(c.bytes_per_run, 20 * GIB);
        a.max_gib_per_day = MAX_GIB_PER_DAY_CEILING + 1;
        assert!(Caps::new(&a).unwrap_err().contains("max_gib_per_day"));
        a.max_gib_per_day = 1;
        a.max_items_per_run = MAX_ITEMS_PER_RUN_CEILING + 1;
        assert!(Caps::new(&a).unwrap_err().contains("max_items_per_run"));
    }

    #[test]
    fn invalid_definitions_are_named() {
        let ok = policy();
        assert_eq!(invalid(&ok, std::slice::from_ref(&ok)), None);
        let check = |p: AutoPolicy, want: &str| {
            let why = invalid(&p, std::slice::from_ref(&p)).expect("invalid");
            assert!(why.contains(want), "{why}");
        };
        check(
            AutoPolicy {
                name: "bad name!".into(),
                ..policy()
            },
            "name",
        );
        check(
            AutoPolicy {
                idle: DurationSpec::from_hours(24),
                ..policy()
            },
            "idle",
        );
        check(
            AutoPolicy {
                every: DurationSpec::from_hours(6),
                ..policy()
            },
            "every",
        );
        check(
            AutoPolicy {
                under: vec![PathBuf::from("relative/dir")],
                ..policy()
            },
            "under",
        );
        assert!(
            invalid(&ok, &[ok.clone(), ok.clone()])
                .expect("dup")
                .contains("same name")
        );
    }

    /// The first time (or after any edit) a policy only previews; it runs one interval after
    /// the preview, then at most once per interval.
    #[test]
    fn dry_run_first_then_once_per_interval() {
        let p = policy();
        let caps = Caps::new(&AutoConfig::default()).unwrap();
        let roots = vec![PathBuf::from("/w")];
        let scope = Scope {
            roots: &roots,
            rules: &[],
        };
        let def = definition(&p, &caps, &scope);
        assert_eq!(due(&p, &def, None, NOW), Due::Preview);
        let mut s = PolicyState {
            definition: def.clone(),
            previewed_at: NOW,
            preview_items: 2,
            preview_bytes: 20,
            last_run_at: None,
        };
        assert_eq!(
            due(&p, &def, Some(&s), NOW + DAY),
            Due::NotDue {
                next_at: NOW + 7 * DAY
            }
        );
        assert_eq!(due(&p, &def, Some(&s), NOW + 7 * DAY), Due::Run);
        s.last_run_at = Some(NOW + 7 * DAY);
        assert!(matches!(
            due(&p, &def, Some(&s), NOW + 8 * DAY),
            Due::NotDue { .. }
        ));
        assert_eq!(due(&p, &def, Some(&s), NOW + 14 * DAY), Due::Run);

        // Any edit (a shorter idle window, a bigger cap) needs a new dry run.
        let mut edited = p.clone();
        edited.idle = DurationSpec::from_hours(30 * 24);
        let def2 = definition(&edited, &caps, &scope);
        assert_eq!(due(&edited, &def2, Some(&s), NOW + 14 * DAY), Due::Preview);
        let bigger = Caps {
            bytes_per_day: caps.bytes_per_day * 2,
            ..caps
        };
        assert_ne!(definition(&p, &bigger, &scope), def);
        // A new scan root or an enabled rule needs a new dry run too (review M1).
        let more = vec![PathBuf::from("/w"), PathBuf::from("/other")];
        let wider = Scope {
            roots: &more,
            rules: &[],
        };
        assert_ne!(definition(&p, &caps, &wider), def);
        let rules = vec!["community.x".to_owned()];
        let ruled = Scope {
            roots: &roots,
            rules: &rules,
        };
        assert_ne!(definition(&p, &caps, &ruled), def);
        // Order and duplicates don't matter.
        let same = vec![PathBuf::from("/w"), PathBuf::from("/w")];
        assert_eq!(
            definition(
                &p,
                &caps,
                &Scope {
                    roots: &same,
                    rules: &[]
                }
            ),
            def
        );
    }

    #[test]
    fn digests_keep_the_latest() {
        let mut st = AutoState::default();
        for i in 0..(DIGESTS_KEPT as i64 + 5) {
            st.push_digest(Digest {
                at: i,
                policy: "p".into(),
                operation_id: None,
                quarantined: 0,
                trashed: 0,
                not_done: 0,
                bytes: 0,
                message: String::new(),
            });
        }
        assert_eq!(st.digests.len(), DIGESTS_KEPT);
        assert_eq!(st.digests[0].at, 5);
    }
}
