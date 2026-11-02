//! Opt-in unattended auto-policies in `scull check` (PRO-10, decision D-11; threat T-05).
//!
//! A policy written in the user config file (`[[auto.policy]]`, see
//! [`scull_core::config::AutoConfig`]) lets the scheduled `scull check` move idle **S0** build
//! outputs into quarantine without a click. Nothing here deletes: every item is planned as a
//! quarantine, and the only thing that touches the disk is the Executor, through the same path
//! as a confirmed `scull clean` (plan recorded in the database → audited, journaled operation
//! under the execution lock → per-item revalidation against the disk right before the move,
//! which also re-checks that the item still qualifies for the policy → quarantine). There is
//! **no Trash fallback**: the OS may empty the Trash by itself, which would be a permanent
//! deletion nobody clicked, so an item whose quarantine is impossible is left in place (blocked,
//! audited). The Executor enforces this too (`Operation::with_trash` is ignored for auto-policy
//! plans).
//!
//! # Gates (in order; the first that applies decides, and nothing is touched)
//!
//! 1. **Off**: no policy says `enabled = true` in the user config file. Nothing is read or
//!    written (no state file, no lock, no plan).
//! 2. **Kill switches** (any one stops every policy): `paused = true` in `[auto]`; the file
//!    [`OFF_FILE`] in scull's config folder; the environment variable [`NO_AUTO_ENV`]; the
//!    `--no-auto-policies` flag. Each can only stop policies, never start one.
//! 3. **Trust**: the config file and its folder must be real (not links), owned by the
//!    account scull runs as, and not writable by any other account (group/other write bits on
//!    Unix, any extended ACL on macOS, write access granted to another account on Windows). A
//!    policy enabled in a file someone else can edit never runs.
//! 4. **Not elevated** (SAFE-14): an unattended run as root/administrator is refused; there is
//!    no `--allow-root` for it.
//! 5. **Nothing in the environment widens the scan** (`SCULL_ROOTS`, `SCULL_ACTIVITY_WINDOW`,
//!    `SCULL_RULES`), and the scan used exactly the file the policies come from (it is read
//!    twice; a change in between refuses). Only items below the file's own `scan.roots` are
//!    considered, and the quarantine location comes from the file's `[quarantine]` only
//!    (`SCULL_QUARANTINE_BASE` is ignored).
//! 6. **A scan of this run**, the **execution lock** (taken before anything is decided, so the
//!    state, the daily usage and what is due are read under it; a held lock refuses), valid
//!    caps, and a readable state file.
//!
//! Then each enabled, valid policy either previews (the first time, and after any edit of the
//! policy, the caps, the file's scan roots or its enabled rules: a dry run that records what it
//! would do), waits for its interval, or runs within the caps (see [`scull_proactive::auto`],
//! which also leaves out items the user restored recently and items whose project changed
//! within the idle window, [`project_activity`]). Each run is one plan
//! ([`PlanOrigin::Policy`]) and one operation ([`ApprovalMethod::AutoPolicy`]) in the audit
//! log: `scull history` lists it and `scull restore <operation>` brings its items back. Every
//! preview and run leaves a digest in the report and in the state file.
//!
//! # Why the folder environment variables are trusted
//!
//! `SCULL_HOME` and the platform's folder variables (`XDG_CONFIG_HOME`, ...) choose where the
//! config file, the database and the state live, for policy runs too. They are trusted because
//! whoever can set the environment of the user's scheduled job can already run any program as
//! that user (the job itself), so refusing them would protect nothing; and the file they point
//! to must still pass the trust check above (owned by the user, writable by nobody else), so
//! they can't make another account's file count. They can only move the opt-in, never make it.
//!
//! [`acquire`], [`prepare`] (decide) and [`execute`] (run) are separate so tests can change the
//! disk between them; [`run_in_check`] wires them to the real directories.

use super::exec::{self, Who};
use super::pipeline;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use schemars::JsonSchema;
use scull_cleaner::{
    Executor, ItemOutcome, LockOptions, PlanContext, QuarantineSettings, Revalidation,
};
use scull_core::config::{AutoConfig, ConfigLayer, QuarantineConfig, expand_home};
use scull_core::{
    Action, Candidate, ErrorKind, ItemId, Plan, PlanItem, PlanOrigin, PlanParams, ScanId,
    ScullError,
};
use scull_engine::{CancelToken, Engine, ScanRequest};
use scull_platform::Exposure;
use scull_proactive::auto::{
    self as rules, AutoState, Budget, Caps, Digest, Due, Excluded, PolicyState,
};
use scull_storage::{ApprovalMethod, OperationEnd, OperationStart, Store};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The policy state file in scull's data folder (owner-only).
pub const STATE_FILE: &str = "auto-policies.json";
/// Kill switch: while this file exists in scull's config folder, no policy runs.
pub const OFF_FILE: &str = "auto-policies.off";
/// Kill switch: while this environment variable is set (non-empty), no policy runs.
pub const NO_AUTO_ENV: &str = "SCULL_NO_AUTO_POLICIES";
/// Items listed per policy in the report.
pub const ITEMS_LISTED: usize = 20;

/// What the auto-policies did in this check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutoPolicyReport {
    /// Off, stopped by a kill switch, refused by a gate, or checked.
    pub state: RunState,
    /// Why, when stopped or refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Bytes auto-policies moved within the last 24 hours, before this run (from the audit
    /// log), when checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_used_bytes: Option<u64>,
    /// The caps, when checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caps: Option<Caps>,
    /// One entry per configured policy, when checked.
    pub policies: Vec<PolicyReport>,
}

impl AutoPolicyReport {
    fn with(state: RunState, reason: Option<String>) -> Self {
        Self {
            state,
            reason,
            day_used_bytes: None,
            caps: None,
            policies: Vec::new(),
        }
    }

    /// No policy is enabled.
    pub fn off() -> Self {
        Self::with(RunState::Off, None)
    }

    fn stopped(reason: impl Into<String>) -> Self {
        Self::with(RunState::Stopped, Some(reason.into()))
    }

    fn refused(reason: impl Into<String>) -> Self {
        Self::with(RunState::Refused, Some(reason.into()))
    }

    /// The IDs of items a policy moved in this run (they are no longer on disk).
    pub fn moved_items(&self) -> BTreeSet<ItemId> {
        self.policies
            .iter()
            .flat_map(|p| &p.items)
            .filter(|i| matches!(i.outcome.as_str(), "quarantined" | "trashed"))
            .map(|i| i.item_id.clone())
            .collect()
    }
}

/// The overall state of the auto-policies in this check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// No policy is enabled in the user config file (the default). Nothing was read or written.
    Off,
    /// A kill switch stopped every policy. Nothing was touched.
    Stopped,
    /// A safety gate refused to run any policy. Nothing was touched.
    Refused,
    /// The policies were evaluated (see each one's outcome).
    Checked,
}

/// One policy in this check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyReport {
    /// The policy's name.
    pub name: String,
    /// What it did.
    pub outcome: PolicyOutcome,
    /// Items it would clean (preview) or handled (run), oldest first, at most
    /// [`ITEMS_LISTED`].
    pub items: Vec<PolicyItem>,
    /// Items it chose but the caps deferred to a later run.
    pub deferred: u64,
    /// Candidates of its kind it left out, by reason.
    pub excluded: Excluded,
}

/// What one policy did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PolicyOutcome {
    /// Not enabled (`enabled = true` is missing).
    Disabled,
    /// Its definition is invalid; it never runs until fixed.
    Invalid {
        /// Why.
        reason: String,
    },
    /// Dry run (first time, or the definition changed): nothing was touched.
    Previewed {
        /// Items it would quarantine.
        items: u64,
        /// Their bytes.
        bytes: u64,
        /// The earliest check that may run it for real.
        runs_after: i64,
    },
    /// Ran recently; it may run again at `next_at`.
    NotDue {
        /// Unix seconds.
        next_at: i64,
    },
    /// Due, but nothing qualifies (or the caps left no room: see `deferred`).
    NothingToDo,
    /// Planned by [`prepare`], not run yet (only seen between [`prepare`] and [`execute`]).
    Planned {
        /// Items planned.
        items: u64,
    },
    /// Ran: one audited operation.
    Ran {
        /// `scull history <id>`, `scull restore <id>`.
        operation_id: String,
        /// Items moved into quarantine.
        quarantined: u64,
        /// Items moved to the Trash (quarantine impossible there).
        trashed: u64,
        /// Items left in place (skipped at revalidation, refused or blocked).
        not_done: u64,
        /// Bytes moved.
        bytes: u64,
        /// How to undo it.
        restore_command: String,
    },
    /// Could not run (e.g. another scull operation holds the execution lock); retried at the
    /// next check. Nothing was touched unless an operation is named.
    Failed {
        /// Why.
        reason: String,
        /// The operation, if one was started.
        #[serde(skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
    },
}

/// One item of a policy's preview or run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyItem {
    /// Stable item ID.
    pub item_id: ItemId,
    /// Its path.
    pub path: String,
    /// Estimated bytes.
    pub bytes: u64,
    /// `would_quarantine` (preview), `quarantined`, `trashed`, or the reason code it was left
    /// in place (e.g. `tier_raised`, `now_protected`, `identity_changed`, `in_use`).
    pub outcome: String,
}

/// Everything [`prepare`] and [`execute`] need besides the scan.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    /// `[auto]` from the trusted user config file (with `~` expanded).
    pub auto: &'a AutoConfig,
    /// The quarantine location from `[quarantine]` of the trusted config file only (never
    /// `SCULL_QUARANTINE_BASE`). There is no Trash fallback for auto-policies.
    pub quarantine: &'a QuarantineConfig,
    /// The machine and user plans are bound to.
    pub who: &'a Who,
    /// scull's data folder (the state file lives here).
    pub data_dir: &'a Path,
    /// `scan.roots` of the config file (canonical): only items below them are considered, and
    /// a preview covers them.
    pub roots: &'a [PathBuf],
    /// `rules.enabled` of the config file (a preview covers them).
    pub rules: &'a [String],
    /// SAFE-09: the last recorded operation before the scan started
    /// (`Store::operations_watermark`); an operation recorded after it that touched an item
    /// voids the run. `None`: operations started at or after the plan's creation count.
    pub operations_watermark: Option<u64>,
    /// Now (Unix seconds).
    pub now: i64,
}

/// The check's scan, for [`run_in_check`].
pub struct CheckScan<'a> {
    /// The scan's ID.
    pub id: &'a ScanId,
    /// Its candidates.
    pub candidates: &'a [Candidate],
    /// The engine it ran with (for revalidation).
    pub engine: &'a Engine,
    /// Its roots.
    pub roots: &'a [PathBuf],
    /// The SAFE-09 watermark read before it started, or why it could not be read.
    pub operations_watermark: Result<u64, String>,
}

/// A policy run [`prepare`] decided on.
#[derive(Debug, Clone)]
struct PlannedRun {
    /// Index into the report's policies.
    report: usize,
    /// The policy (re-checked against every fresh candidate right before its move).
    policy: scull_core::config::AutoPolicy,
    /// The plan (S0 quarantines, [`PlanOrigin::Policy`]).
    plan: Plan,
    /// Planned bytes per item.
    bytes: Vec<(ItemId, String, u64)>,
}

/// What [`prepare`] decided: the report so far, the runs to execute, the state to save, and
/// what the per-item re-check needs.
#[derive(Debug, Clone)]
pub struct Prepared {
    report: AutoPolicyReport,
    runs: Vec<PlannedRun>,
    state: Option<AutoState>,
    /// Paths restored from quarantine, with when.
    restored: BTreeMap<String, i64>,
    /// Every candidate path of the scan (left out of project-activity walks).
    artifacts: BTreeSet<PathBuf>,
}

impl Prepared {
    /// The report as decided (before anything runs).
    pub fn report(&self) -> &AutoPolicyReport {
        &self.report
    }

    /// A decision that runs nothing.
    fn only(report: AutoPolicyReport) -> Self {
        Self {
            report,
            runs: Vec::new(),
            state: None,
            restored: BTreeMap::new(),
            artifacts: BTreeSet::new(),
        }
    }
}

/// Why the auto-policies may not run at all, checked before anything else ([`Gate::check`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Gate {
    /// `--no-auto-policies`.
    pub flag_off: bool,
    /// [`NO_AUTO_ENV`] is set.
    pub env_off: bool,
    /// [`OFF_FILE`] exists.
    pub file_off: bool,
    /// Why the config file can't be trusted, if it can't.
    pub untrusted: Option<String>,
    /// Whether scull runs elevated (or can't tell).
    pub elevated: bool,
}

impl Gate {
    /// The report when a gate stops everything (`None`: go on). `auto.paused` is checked here
    /// too.
    pub fn check(&self, auto: &AutoConfig) -> Option<AutoPolicyReport> {
        if !auto.any_enabled() {
            return Some(AutoPolicyReport::off());
        }
        let stop = if auto.paused {
            Some("paused = true in [auto] of the config file".to_owned())
        } else if self.file_off {
            Some(format!(
                "the kill switch file {OFF_FILE} exists in scull's config folder"
            ))
        } else if self.env_off {
            Some(format!("{NO_AUTO_ENV} is set"))
        } else if self.flag_off {
            Some("--no-auto-policies was given".to_owned())
        } else {
            None
        };
        if let Some(why) = stop {
            return Some(AutoPolicyReport::stopped(format!(
                "stopped by a kill switch ({why}); nothing was touched"
            )));
        }
        // Elevated first: it refuses whatever the file says, and an elevated Windows account
        // owns what it creates as BUILTIN\Administrators, which the trust check would report
        // instead of the real reason (#120).
        if self.elevated {
            return Some(AutoPolicyReport::refused(
                "not run: auto-policies never run as root/administrator (SAFE-14), and an \
                 unattended run has no --allow-root; nothing was touched",
            ));
        }
        if let Some(why) = &self.untrusted {
            return Some(AutoPolicyReport::refused(format!(
                "not run: the config file can't be trusted ({why}). Make it and its folder \
                 writable only by you (e.g. `chmod go-w`); nothing was touched"
            )));
        }
        None
    }
}

/// Take the execution lock (waiting for nobody) and reconcile any operation a crash left open
/// (SAFE-09, SAFE-19), before anything is decided: the state, the daily usage and what is due
/// are read under the lock, so a scheduled and a manual check can't both spend the budget.
/// A held lock is reported (`refused`, retried at the next check); nothing is touched.
pub fn acquire(
    lock: &LockOptions,
    store: &mut Store,
    now: i64,
) -> Result<Executor, AutoPolicyReport> {
    Executor::acquire(lock)
        .and_then(|ex| {
            ex.recover(store, now)?;
            Ok(ex)
        })
        .map_err(|e| {
            AutoPolicyReport::refused(format!(
                "not run: {}; nothing was touched, auto-policies are retried at the next check",
                e.message
            ))
        })
}

/// Newest change inside a project, for the project-activity check: the project folder and
/// everything below it except the scan's items (`artifacts`, e.g. `target/`) and the inside of
/// `.git` (only `.git` itself and its direct entries such as `HEAD` and `index` count).
/// Links are not followed (their own time counts). More than [`PROJECT_ENTRY_LIMIT`] entries,
/// or anything unreadable, is "can't tell", which never counts as idle.
pub fn project_activity(
    c: &Candidate,
    cutoff: i64,
    artifacts: &BTreeSet<PathBuf>,
) -> rules::ProjectActivity {
    use rules::ProjectActivity::{Active, Idle, Unknown};
    let Some(project) = &c.project else {
        return Unknown;
    };
    let root = project.root.to_path_buf();
    let item = c.path.to_path_buf();
    let recent = |md: &std::fs::Metadata| {
        md.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .is_none_or(|t| t >= cutoff)
    };
    let mut seen = 0usize;
    let mut stack = vec![(root, false)];
    while let Some((dir, shallow)) = stack.pop() {
        let Ok(md) = std::fs::symlink_metadata(&dir) else {
            return Unknown;
        };
        if recent(&md) {
            return Active;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Unknown;
        };
        for e in entries {
            let Ok(e) = e else { return Unknown };
            seen += 1;
            if seen > PROJECT_ENTRY_LIMIT {
                return Unknown;
            }
            let path = e.path();
            if path == item || artifacts.contains(&path) {
                continue;
            }
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                return Unknown;
            };
            if md.is_dir() && !shallow {
                stack.push((path.clone(), path.file_name().is_some_and(|n| n == ".git")));
            } else if recent(&md) {
                return Active;
            }
        }
    }
    Idle
}

/// Entries a project-activity walk looks at before it gives up ("can't tell").
pub const PROJECT_ENTRY_LIMIT: usize = 20_000;

/// Paths restored from quarantine (any operation), with the latest restore time.
fn restored_paths(store: &Store) -> Result<BTreeMap<String, i64>, String> {
    let mut out = BTreeMap::new();
    for e in store.quarantine_entries(None).map_err(|e| e.to_string())? {
        if let Some((at, _)) = e.restored {
            let v = out
                .entry(e.entry.original_path.as_str().to_owned())
                .or_insert(at);
            *v = (*v).max(at);
        }
    }
    Ok(out)
}

/// Decide what each policy does at `inputs.now` over the check's scan (`scan_id`,
/// `candidates`; only those below the config file's roots count). Reads the state file, the
/// audit log and the projects' modification times; writes nothing. The gates
/// ([`Gate::check`]) must have passed and `_lock` is the proof that the execution lock is
/// held ([`acquire`]).
pub fn prepare(
    inputs: &Inputs<'_>,
    _lock: &Executor,
    scan_id: &ScanId,
    candidates: &[Candidate],
    store: &Store,
) -> Prepared {
    let auto = inputs.auto;
    if !auto.any_enabled() {
        return Prepared::only(AutoPolicyReport::off());
    }
    if auto.paused {
        // Also checked by [`Gate::check`]; repeated here so no caller can skip it.
        return Prepared::only(AutoPolicyReport::stopped(
            "stopped by a kill switch (paused = true in [auto] of the config file); nothing was \
             touched",
        ));
    }
    let caps = match Caps::new(auto) {
        Ok(c) => c,
        Err(why) => {
            return Prepared::only(AutoPolicyReport::refused(format!(
                "{why}; no policy runs until it is fixed, nothing was touched"
            )));
        }
    };
    let state = match read_state(inputs.data_dir) {
        Ok(s) => s,
        Err(why) => return Prepared::only(AutoPolicyReport::refused(why)),
    };
    let restored = match restored_paths(store) {
        Ok(r) => r,
        Err(e) => {
            return Prepared::only(AutoPolicyReport::refused(format!(
                "the quarantine records could not be read ({e}); nothing was touched"
            )));
        }
    };
    let candidates: Vec<Candidate> = candidates
        .iter()
        .filter(|c| {
            let p = c.path.to_path_buf();
            inputs.roots.iter().any(|r| p.starts_with(r))
        })
        .cloned()
        .collect();
    let artifacts: BTreeSet<PathBuf> = candidates.iter().map(|c| c.path.to_path_buf()).collect();
    let activity = |c: &Candidate, cutoff: i64| project_activity(c, cutoff, &artifacts);
    let scope = rules::Scope {
        roots: inputs.roots,
        rules: inputs.rules,
    };
    let used = match rules::day_usage(store, inputs.now) {
        Ok(u) => u,
        Err(e) => {
            return Prepared::only(AutoPolicyReport::refused(format!(
                "the audit log could not be read to apply the daily cap ({e}); nothing was \
                 touched"
            )));
        }
    };
    let mut report = AutoPolicyReport {
        state: RunState::Checked,
        reason: None,
        day_used_bytes: Some(used.bytes),
        caps: Some(caps),
        policies: Vec::new(),
    };
    let mut new_state = state.clone();
    let mut runs = Vec::new();
    let mut budget = Budget::for_run(&caps, used.bytes);
    let mut taken: BTreeSet<&ItemId> = BTreeSet::new();
    let mut policies: Vec<_> = auto.policy.iter().collect();
    policies.sort_by(|a, b| a.name.cmp(&b.name));
    for policy in policies {
        let mut entry = PolicyReport {
            name: policy.name.clone(),
            outcome: PolicyOutcome::Disabled,
            items: Vec::new(),
            deferred: 0,
            excluded: Excluded::default(),
        };
        if !policy.enabled {
            report.policies.push(entry);
            continue;
        }
        if let Some(reason) = rules::invalid(policy, &auto.policy) {
            entry.outcome = PolicyOutcome::Invalid { reason };
            report.policies.push(entry);
            continue;
        }
        let def = rules::definition(policy, &caps, &scope);
        let due = rules::due(policy, &def, state.policies.get(&policy.name), inputs.now);
        if let Due::NotDue { next_at } = due {
            entry.outcome = PolicyOutcome::NotDue { next_at };
            report.policies.push(entry);
            continue;
        }
        let ctx = rules::Context {
            taken: &taken,
            restored: &restored,
            project_activity: &activity,
        };
        let sel = rules::select(policy, &candidates, &ctx, inputs.now);
        entry.excluded = sel.excluded;
        if due == Due::Preview {
            // What it would take with the full caps (a preview spends nothing).
            let mut fresh = Budget::for_run(&caps, used.bytes);
            let (fit, deferred) = rules::take(sel.chosen, &mut fresh);
            let bytes: u64 = fit.iter().map(|c| c.bytes).sum();
            entry.deferred = deferred;
            entry.items = list(&fit, "would_quarantine");
            let runs_after = inputs
                .now
                .saturating_add(i64::try_from(policy.every.as_secs()).unwrap_or(i64::MAX));
            entry.outcome = PolicyOutcome::Previewed {
                items: fit.len() as u64,
                bytes,
                runs_after,
            };
            new_state.policies.insert(
                policy.name.clone(),
                PolicyState {
                    definition: def,
                    previewed_at: inputs.now,
                    preview_items: fit.len() as u64,
                    preview_bytes: bytes,
                    last_run_at: None,
                },
            );
            new_state.push_digest(Digest {
                at: inputs.now,
                policy: policy.name.clone(),
                operation_id: None,
                quarantined: 0,
                trashed: 0,
                not_done: 0,
                bytes: 0,
                message: format!(
                    "dry run: would quarantine {} item{} ({}); nothing was touched",
                    fit.len(),
                    if fit.len() == 1 { "" } else { "s" },
                    format_bytes(bytes)
                ),
            });
            report.policies.push(entry);
            continue;
        }
        let chose_any = !sel.chosen.is_empty();
        let (fit, deferred) = rules::take(sel.chosen, &mut budget);
        entry.deferred = deferred;
        if fit.is_empty() {
            entry.outcome = PolicyOutcome::NothingToDo;
            if !chose_any {
                // Nothing qualified: it ran, as far as the interval goes. (Deferred by the caps:
                // try again at the next check.)
                if let Some(s) = new_state.policies.get_mut(&policy.name) {
                    s.last_run_at = Some(inputs.now);
                }
            }
            report.policies.push(entry);
            continue;
        }
        let made = fit
            .iter()
            .map(|c| {
                PlanItem::from_candidate(c.candidate).map(|i| PlanItem {
                    action: Action::Quarantine,
                    ..i
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .and_then(|items| {
                Plan::new(
                    PlanParams {
                        id: exec::new_plan_id(inputs.now)?,
                        scan_id: scan_id.clone(),
                        created_unix: inputs.now,
                        ttl_secs: None,
                        machine_id: inputs.who.machine_id.clone(),
                        user: inputs.who.user.clone(),
                        origin: PlanOrigin::Policy,
                        operations_watermark: inputs.operations_watermark,
                    },
                    items,
                )
            });
        match made {
            Ok(plan) => {
                for c in &fit {
                    taken.insert(&c.candidate.id);
                }
                entry.items = list(&fit, "planned");
                entry.outcome = PolicyOutcome::Planned {
                    items: fit.len() as u64,
                };
                runs.push(PlannedRun {
                    report: report.policies.len(),
                    policy: policy.clone(),
                    plan,
                    bytes: fit
                        .iter()
                        .map(|c| {
                            (
                                c.candidate.id.clone(),
                                c.candidate.path.as_str().to_owned(),
                                c.bytes,
                            )
                        })
                        .collect(),
                });
            }
            Err(e) => {
                entry.outcome = PolicyOutcome::Failed {
                    reason: format!("no plan could be made: {}", e.message),
                    operation_id: None,
                };
            }
        }
        report.policies.push(entry);
    }
    Prepared {
        report,
        runs,
        state: Some(new_state),
        restored,
        artifacts,
    }
}

/// The listed items of a preview or plan.
fn list(chosen: &[rules::Chosen<'_>], outcome: &str) -> Vec<PolicyItem> {
    chosen
        .iter()
        .take(ITEMS_LISTED)
        .map(|c| PolicyItem {
            item_id: c.candidate.id.clone(),
            path: c.candidate.path.as_str().to_owned(),
            bytes: c.bytes,
            outcome: outcome.to_owned(),
        })
        .collect()
}

/// Run what [`prepare`] decided, under the same execution lock (`executor`, from
/// [`acquire`]), then save the state. Each run records the plan, starts an audited operation
/// and executes it with per-item revalidation (`engine`, `roots`: the check's scan;
/// `request`: the revalidation scan, `None` for the default).
#[allow(clippy::too_many_arguments)]
pub fn execute(
    inputs: &Inputs<'_>,
    executor: &Executor,
    prepared: Prepared,
    engine: &Engine,
    roots: &[PathBuf],
    request: Option<ScanRequest>,
    store: &mut Store,
    cancel: &CancelToken,
) -> AutoPolicyReport {
    let Prepared {
        mut report,
        runs,
        state,
        restored,
        artifacts,
    } = prepared;
    let Some(mut state) = state else {
        return report;
    };
    let recheck = Recheck {
        restored: &restored,
        artifacts: &artifacts,
    };
    {
        for run in &runs {
            let entry = &mut report.policies[run.report];
            let outcome = run_one(
                inputs,
                executor,
                run,
                &recheck,
                engine,
                roots,
                request.clone(),
                store,
                cancel,
            );
            match outcome {
                Ok((outcome, items)) => {
                    if let PolicyOutcome::Ran {
                        operation_id,
                        quarantined,
                        trashed,
                        not_done,
                        bytes,
                        ..
                    } = &outcome
                    {
                        if let Some(s) = state.policies.get_mut(&entry.name) {
                            s.last_run_at = Some(inputs.now);
                        }
                        state.push_digest(Digest {
                            at: inputs.now,
                            policy: entry.name.clone(),
                            operation_id: Some(operation_id.clone()),
                            quarantined: *quarantined,
                            trashed: *trashed,
                            not_done: *not_done,
                            bytes: *bytes,
                            message: ran_message(&entry.name, &outcome),
                        });
                    }
                    entry.items = items;
                    entry.outcome = outcome;
                }
                Err(e) => {
                    entry.outcome = PolicyOutcome::Failed {
                        reason: format!("{}; it is retried at the next check", e.message),
                        operation_id: None,
                    };
                }
            }
            if let PolicyOutcome::Failed {
                reason,
                operation_id: Some(op),
            } = &entry.outcome
            {
                state.push_digest(Digest {
                    at: inputs.now,
                    policy: entry.name.clone(),
                    operation_id: Some(op.clone()),
                    quarantined: 0,
                    trashed: 0,
                    not_done: 0,
                    bytes: 0,
                    message: format!(
                        "auto-policy {} stopped: {reason}; see `scull history {op}`",
                        entry.name
                    ),
                });
            }
            if cancel.is_cancelled() {
                break;
            }
        }
    }
    for p in &mut report.policies {
        if matches!(p.outcome, PolicyOutcome::Planned { .. }) {
            p.outcome = PolicyOutcome::Failed {
                reason: "cancelled before it ran; nothing was touched".into(),
                operation_id: None,
            };
        }
    }
    if let Err(why) = write_state(inputs.data_dir, &state) {
        report.reason = Some(format!(
            "the auto-policy state could not be saved ({why}); the next check previews again"
        ));
    }
    report
}

/// What the per-item re-check needs from [`prepare`].
struct Recheck<'a> {
    restored: &'a BTreeMap<String, i64>,
    artifacts: &'a BTreeSet<PathBuf>,
}

/// One policy run: record the plan, start its operation, execute, finish.
#[allow(clippy::too_many_arguments)]
fn run_one(
    inputs: &Inputs<'_>,
    executor: &Executor,
    run: &PlannedRun,
    recheck: &Recheck<'_>,
    engine: &Engine,
    roots: &[PathBuf],
    request: Option<ScanRequest>,
    store: &mut Store,
    cancel: &CancelToken,
) -> Result<(PolicyOutcome, Vec<PolicyItem>), ScullError> {
    let plan = &run.plan;
    store.insert_plan(plan)?;
    let operation_id = exec::new_operation_id(inputs.now)?;
    let mut op = executor
        .begin_operation(
            store,
            plan,
            &OperationStart {
                operation_id: operation_id.clone(),
                plan_id: plan.id().clone(),
                content_hash: plan.content_hash().clone(),
                approver: inputs.who.user.clone(),
                method: ApprovalMethod::AutoPolicy,
                started_at: inputs.now,
            },
        )?
        // No Trash fallback (`with_trash`): the OS may empty the Trash by itself, a permanent
        // deletion nobody clicked. Where quarantine is impossible the item is blocked.
        .with_quarantine(QuarantineSettings::from_config(inputs.quarantine));
    // Right before each move the fresh candidate must still qualify for the policy (still
    // S0, idle, unflagged, its project idle, not restored recently), on top of the usual
    // narrowing.
    let now = inputs.now;
    let none = BTreeSet::new();
    let artifacts = recheck.artifacts;
    let activity = |c: &Candidate, cutoff: i64| project_activity(c, cutoff, artifacts);
    let ctx = rules::Context {
        taken: &none,
        restored: recheck.restored,
        project_activity: &activity,
    };
    let still = |c: &Candidate| rules::still_qualifies(&run.policy, c, &ctx, now);
    let mut revalidation = Revalidation::new(engine, roots).with_condition(&still);
    if let Some(r) = request {
        revalidation = revalidation.with_request(r);
    }
    let ctx = PlanContext {
        now_unix: inputs.now,
        machine_id: &inputs.who.machine_id,
        user: &inputs.who.user,
    };
    let result = executor.execute_plan(plan, &ctx, &revalidation, Some(&mut op), cancel);
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = op.finish(OperationEnd::Cancelled, scull_engine::now_unix());
            return Ok((
                PolicyOutcome::Failed {
                    reason: e.message,
                    operation_id: Some(operation_id.to_string()),
                },
                Vec::new(),
            ));
        }
    };
    op.finish(
        if report.cancelled {
            OperationEnd::Cancelled
        } else {
            OperationEnd::Normal
        },
        scull_engine::now_unix(),
    )?;
    let (mut quarantined, mut trashed, mut not_done, mut bytes) = (0, 0, 0, 0u64);
    let mut items = Vec::new();
    for r in &report.items {
        let planned = run
            .bytes
            .iter()
            .find(|(id, ..)| *id == r.item_id)
            .map_or(0, |b| b.2);
        let outcome = match &r.outcome {
            ItemOutcome::Quarantined(q) => {
                quarantined += 1;
                bytes = bytes.saturating_add(q.bytes);
                "quarantined".to_owned()
            }
            ItemOutcome::Trashed(t) => {
                trashed += 1;
                bytes = bytes.saturating_add(t.moved_bytes());
                "trashed".to_owned()
            }
            _ => {
                not_done += 1;
                r.reason_code().unwrap_or("not_done").to_owned()
            }
        };
        if items.len() < ITEMS_LISTED {
            items.push(PolicyItem {
                item_id: r.item_id.clone(),
                path: r.path.as_str().to_owned(),
                bytes: planned,
                outcome,
            });
        }
    }
    Ok((
        PolicyOutcome::Ran {
            restore_command: format!("scull restore {operation_id}"),
            operation_id: operation_id.to_string(),
            quarantined,
            trashed,
            not_done,
            bytes,
        },
        items,
    ))
}

fn ran_message(name: &str, o: &PolicyOutcome) -> String {
    let PolicyOutcome::Ran {
        quarantined,
        trashed,
        not_done,
        bytes,
        restore_command,
        ..
    } = o
    else {
        return String::new();
    };
    let mut s = format!(
        "auto-policy {name} moved {} item{} ({}) into quarantine",
        quarantined + trashed,
        if quarantined + trashed == 1 { "" } else { "s" },
        format_bytes(*bytes)
    );
    if *trashed > 0 {
        s.push_str(&format!(
            " ({trashed} to the Trash: quarantine was impossible there)"
        ));
    }
    if *not_done > 0 {
        s.push_str(&format!(
            "; {not_done} left in place (changed since the scan)"
        ));
    }
    s.push_str(&format!("; undo: {restore_command}"));
    s
}

// ---------------------------------------------------------------------------------------------
// State file
// ---------------------------------------------------------------------------------------------

/// The state in `data_dir` ([`STATE_FILE`]): missing → empty (every policy previews); present
/// but not owner-only, unreadable or invalid → refused (fail closed; removing the file starts
/// over with dry runs).
pub fn read_state(data_dir: &Path) -> Result<AutoState, String> {
    let path = data_dir.join(STATE_FILE);
    let refuse = |why: String| {
        format!(
            "the auto-policy state {} {why}; no policy runs (remove the file to start over: \
             every policy then previews again); nothing was touched",
            path.display()
        )
    };
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AutoState::default()),
        Err(e) => return Err(refuse(format!("can't be read ({e})"))),
        Ok(_) => {}
    }
    scull_platform::ensure_owner_only(&path)
        .map_err(|e| refuse(format!("is not private to you ({e})")))?;
    let bytes = std::fs::read(&path).map_err(|e| refuse(format!("can't be read ({e})")))?;
    let state: AutoState =
        serde_json::from_slice(&bytes).map_err(|e| refuse(format!("is invalid ({e})")))?;
    if state.version != rules::STATE_VERSION {
        return Err(refuse(format!("has unknown version {}", state.version)));
    }
    Ok(state)
}

/// Write the state owner-only (a fresh file renamed over the old one).
pub fn write_state(data_dir: &Path, state: &AutoState) -> Result<(), String> {
    let path = data_dir.join(STATE_FILE);
    let tmp = data_dir.join(format!(".{STATE_FILE}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let json = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let mut f = scull_platform::create_file_owner_only(&tmp).map_err(|e| e.to_string())?;
    use std::io::Write;
    f.write_all(&json)
        .and_then(|()| f.sync_all())
        .map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

// ---------------------------------------------------------------------------------------------
// Trusting the config file
// ---------------------------------------------------------------------------------------------

/// Windows access rights that let another account change a file or its permissions.
const WRITE_RIGHTS: u32 = 0x0000_0002 // FILE_WRITE_DATA / FILE_ADD_FILE
    | 0x0000_0004 // FILE_APPEND_DATA / FILE_ADD_SUBDIRECTORY
    | 0x0000_0010 // FILE_WRITE_EA
    | 0x0000_0040 // FILE_DELETE_CHILD
    | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x0200_0000 // MAXIMUM_ALLOWED
    | 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE

/// The exposures that let another account change an entry: group/other write bits, another
/// owner, write access granted to another account, no DACL, an access entry that can't be
/// checked, or a link. Read access for others is fine (a config file holds no secret).
pub fn write_exposures(exposures: &[Exposure]) -> Vec<String> {
    exposures
        .iter()
        .filter(|e| match e {
            Exposure::Mode { mode } => mode & 0o022 != 0,
            Exposure::Grant { mask, .. } => mask & WRITE_RIGHTS != 0,
            _ => true,
        })
        .map(ToString::to_string)
        .collect()
}

/// Why `path` (the config file or its folder) can't be trusted, if it can't.
/// macOS: any extended ACL (`chmod +a`) counts, since permission bits don't show it; remove it
/// with `chmod -N`.
pub fn untrusted(path: &Path) -> Option<String> {
    let bits = match scull_platform::ensure_owner_only(path) {
        Ok(()) => None,
        Err(e) => match e.kind() {
            scull_platform::ErrorKind::NotOwnerOnly { exposures } => {
                let bad = write_exposures(exposures);
                (!bad.is_empty()).then(|| format!("{} {}", path.display(), bad.join(", ")))
            }
            _ => Some(format!("{}: {e}", path.display())),
        },
    };
    bits.or_else(|| match scull_platform::has_extended_acl(path) {
        Ok(false) => None,
        Ok(true) => Some(format!(
            "{} has an extended ACL that may let other accounts write it (`chmod -N` removes it)",
            path.display()
        )),
        Err(e) => Some(format!("{}: {e}", path.display())),
    })
}

/// What the user config file says about auto-policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAuto {
    /// `[auto]`, with `~` expanded and `under` folders made canonical.
    pub auto: AutoConfig,
    /// `scan.roots` **of the file** (canonical): policies act only below these, whatever an
    /// environment variable says.
    pub roots: Vec<PathBuf>,
    /// `rules.enabled` of the file.
    pub rules: Vec<String>,
    /// `[quarantine]` of the file, `~` expanded: where policy runs quarantine
    /// (`SCULL_QUARANTINE_BASE` is ignored for them).
    pub quarantine: QuarantineConfig,
    /// The whole parsed file, to check that the scan used the same settings.
    pub layer: ConfigLayer,
    /// Why the file can't be trusted, when a policy is enabled in it.
    pub untrusted: Option<String>,
}

/// `[auto]` and `scan.roots` from the user config file at `file`, `~` expanded with `home`.
/// `Ok(None)` when there is no file.
///
/// When a policy is enabled, the file is read, checked (file and folder), and read again: the
/// policies come from the first read only if the second is identical, so a file swapped while
/// it was checked is refused.
pub fn load_auto(file: &Path, home: Option<&Path>) -> Result<Option<FileAuto>, ScullError> {
    let read = || match std::fs::read_to_string(file) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ScullError::new(
            ErrorKind::Config,
            format!("cannot read the config file: {e}"),
        )),
    };
    let Some(first) = read()? else {
        return Ok(None);
    };
    let layer = ConfigLayer::from_toml(&first)
        .map_err(|e| e.with_path(scull_core::FsPath::from_path(file)))?;
    let fix = |p: &PathBuf| {
        let p = home.map_or_else(|| p.clone(), |h| expand_home(p, h));
        if p.is_absolute() {
            scull_engine::paths::canon(&p)
        } else {
            p
        }
    };
    let mut auto = layer.auto.resolve();
    for p in &mut auto.policy {
        p.under = p.under.iter().map(fix).collect();
    }
    let roots = layer
        .scan
        .roots
        .iter()
        .flatten()
        .map(fix)
        .filter(|p| p.is_absolute())
        .collect();
    let expand = |p: &PathBuf| home.map_or_else(|| p.clone(), |h| expand_home(p, h));
    let quarantine = QuarantineConfig {
        base: layer.quarantine.base.as_ref().map(expand),
        bases: layer
            .quarantine
            .bases
            .iter()
            .flatten()
            .map(|(k, v)| (expand(k), expand(v)))
            .collect(),
    };
    let rules = layer.rules.enabled.clone().unwrap_or_default();
    let mut out = FileAuto {
        auto,
        roots,
        rules,
        quarantine,
        layer,
        untrusted: None,
    };
    if !out.auto.any_enabled() {
        return Ok(Some(out));
    }
    let mut why = untrusted(file).or_else(|| file.parent().and_then(untrusted));
    if why.is_none() && read()?.as_deref() != Some(first.as_str()) {
        why = Some("it changed while it was being checked".into());
    }
    out.untrusted = why;
    Ok(Some(out))
}

/// The environment variables that would widen what a policy acts on (other scan roots, a
/// shorter activity window, more rules), if any is set. Variables that only add protection
/// (`SCULL_EXCLUDE`, `SCULL_PROTECT`) don't count.
pub fn widening_env(lookup: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    scull_core::config::ENV_VARS
        .iter()
        .filter(|v| {
            matches!(
                v.field,
                "scan.roots" | "scan.activity_window" | "rules.enabled"
            )
        })
        .filter(|v| lookup(v.name).is_some_and(|x| !x.is_empty()))
        .map(|v| v.name)
        .collect()
}

/// The auto-policies step of `scull check` (see the [module docs](self)). `scan`: the check's
/// scan, its engine and roots, when it scanned. Never fails the check: every problem is
/// reported in the returned report.
pub fn run_in_check(
    loaded: &pipeline::Loaded,
    no_auto_policies: bool,
    scan: Option<CheckScan<'_>>,
    store: &mut Store,
    ui: &Ui,
) -> AutoPolicyReport {
    let dirs = match exec::dirs() {
        Ok(d) => d,
        Err(e) => return AutoPolicyReport::refused(e.message),
    };
    let file = dirs.config.join(super::doctor::CONFIG_FILE_NAME);
    let fa = match load_auto(&file, loaded.home.as_deref()) {
        Ok(Some(a)) => a,
        Ok(None) => return AutoPolicyReport::off(),
        Err(e) => return AutoPolicyReport::refused(e.message),
    };
    let gate = Gate {
        flag_off: no_auto_policies,
        env_off: std::env::var_os(NO_AUTO_ENV).is_some_and(|v| !v.is_empty()),
        file_off: std::fs::symlink_metadata(dirs.config.join(OFF_FILE)).is_ok(),
        untrusted: fa.untrusted.clone(),
        elevated: exec::elevation().blocks_destructive(),
    };
    if let Some(r) = gate.check(&fa.auto) {
        return r;
    }
    // The environment may not widen what a policy acts on (only the config file decides).
    let widening = widening_env(|k| std::env::var(k).ok());
    if !widening.is_empty() {
        return AutoPolicyReport::refused(format!(
            "not run: {} would change what auto-policies act on; they run only with the scan \
             settings of the config file. Nothing was touched",
            widening.join(", ")
        ));
    }
    // One config for both: the scan this check ran must have used the file the policies come
    // from (it is read twice; refuse if it changed in between).
    if let Some(why) = scan_config_differs(loaded, &fa.layer) {
        return AutoPolicyReport::refused(format!("not run: {why}; nothing was touched"));
    }
    let Some(CheckScan {
        id: scan_id,
        candidates,
        engine,
        roots,
        operations_watermark,
    }) = scan
    else {
        return AutoPolicyReport::refused(
            "not run: auto-policies run only on the check's own scan of scan.roots, and this \
             check did not scan; nothing was touched",
        );
    };
    let watermark = match operations_watermark {
        Ok(w) => w,
        Err(why) => {
            return AutoPolicyReport::refused(format!(
                "not run: scull's database could not be read before the scan, so later changes \
                 to the items could not be detected ({why}); nothing was touched"
            ));
        }
    };
    let who = match exec::who() {
        Ok(w) => w,
        Err(e) => return AutoPolicyReport::refused(e.message),
    };
    let lock = match LockOptions::for_current_user() {
        Ok(l) => l,
        Err(e) => return AutoPolicyReport::refused(e.message),
    };
    let now = scull_engine::now_unix();
    let executor = match acquire(&lock, store, now) {
        Ok(ex) => ex,
        Err(r) => return r,
    };
    let inputs = Inputs {
        auto: &fa.auto,
        quarantine: &fa.quarantine,
        who: &who,
        data_dir: &dirs.data,
        roots: &fa.roots,
        rules: &fa.rules,
        operations_watermark: Some(watermark),
        now,
    };
    let prepared = prepare(&inputs, &executor, scan_id, candidates, store);
    if ui.progress && !prepared.runs.is_empty() {
        ui.progress("running auto-policies...");
    }
    let bridge = pipeline::CancelBridge::start();
    let report = execute(
        &inputs,
        &executor,
        prepared,
        engine,
        roots,
        None,
        store,
        bridge.token(),
    );
    drop(bridge);
    report
}

/// Why the scan's effective config (`loaded`) is not what the config file the policies come
/// from (`layer`) gives with this environment, if it isn't (the file changed between the two
/// reads).
fn scan_config_differs(loaded: &pipeline::Loaded, layer: &ConfigLayer) -> Option<String> {
    let env = match scull_core::config::EnvLayer::from_lookup(|k| std::env::var(k).ok()) {
        Ok(e) => e,
        Err(e) => return Some(e.message),
    };
    let resolved = scull_core::config::resolve(&ConfigLayer::default(), &env, layer).config;
    let expected = match &loaded.home {
        Some(h) => resolved.with_home_expanded(h),
        None => resolved,
    };
    (expected != loaded.config).then(|| {
        "the config file changed while this check ran (scan and policies would disagree)".into()
    })
}

/// The human lines of the report (nothing when off).
pub fn render(r: &AutoPolicyReport) -> String {
    let mut out = String::new();
    match r.state {
        RunState::Off => return out,
        RunState::Stopped | RunState::Refused => {
            out.push_str(&format!(
                "auto-policies: {}\n",
                r.reason.as_deref().unwrap_or("not run")
            ));
            return out;
        }
        RunState::Checked => {}
    }
    for p in &r.policies {
        let line = match &p.outcome {
            PolicyOutcome::Disabled => continue,
            PolicyOutcome::Invalid { reason } => format!("invalid, never runs: {reason}"),
            PolicyOutcome::Previewed {
                items,
                bytes,
                runs_after,
            } => format!(
                "dry run: would quarantine {items} item{} ({}); nothing was touched. It runs \
                 from a check after {}",
                if *items == 1 { "" } else { "s" },
                format_bytes(*bytes),
                super::clean::utc_datetime(*runs_after)
            ),
            PolicyOutcome::NotDue { next_at } => {
                format!("next run after {}", super::clean::utc_datetime(*next_at))
            }
            PolicyOutcome::NothingToDo if p.deferred > 0 => format!(
                "{} item{} deferred by the caps",
                p.deferred,
                if p.deferred == 1 { "" } else { "s" }
            ),
            PolicyOutcome::NothingToDo => "nothing qualifies".to_owned(),
            PolicyOutcome::Planned { items } => format!("{items} item(s) planned, not run"),
            o @ PolicyOutcome::Ran { .. } => ran_message(&p.name, o)
                .trim_start_matches(&format!("auto-policy {} ", p.name))
                .to_owned(),
            PolicyOutcome::Failed { reason, .. } => format!("not run: {reason}"),
        };
        out.push_str(&format!("auto-policy {}: {line}\n", p.name));
        if matches!(
            p.outcome,
            PolicyOutcome::Previewed { .. } | PolicyOutcome::Ran { .. }
        ) {
            for i in &p.items {
                out.push_str(&format!(
                    "    {} {} ({})\n",
                    i.outcome,
                    i.path,
                    format_bytes(i.bytes)
                ));
            }
        }
    }
    if let Some(why) = &r.reason {
        out.push_str(&format!("auto-policies: {why}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::config::{AutoKind, AutoPolicy, DurationSpec};

    fn auto(enabled: bool) -> AutoConfig {
        AutoConfig {
            policy: vec![AutoPolicy {
                name: "p".into(),
                enabled,
                kind: AutoKind::BuildOutputs,
                idle: DurationSpec::from_hours(60 * 24),
                every: DurationSpec::from_hours(7 * 24),
                under: Vec::new(),
            }],
            ..AutoConfig::default()
        }
    }

    /// Nothing is enabled → off, whatever else is set; every kill switch stops an enabled
    /// policy; an untrusted file or an elevated run is refused.
    #[test]
    fn gates_stop_before_anything_runs() {
        let open = Gate::default();
        assert_eq!(open.check(&auto(false)).unwrap().state, RunState::Off);
        assert!(open.check(&auto(true)).is_none());
        let mut paused = auto(true);
        paused.paused = true;
        assert_eq!(open.check(&paused).unwrap().state, RunState::Stopped);
        for g in [
            Gate {
                flag_off: true,
                ..Gate::default()
            },
            Gate {
                env_off: true,
                ..Gate::default()
            },
            Gate {
                file_off: true,
                ..Gate::default()
            },
        ] {
            let r = g.check(&auto(true)).unwrap();
            assert_eq!(r.state, RunState::Stopped, "{g:?}");
            assert!(r.reason.unwrap().contains("nothing was touched"));
        }
        let r = Gate {
            untrusted: Some("group-writable".into()),
            ..Gate::default()
        }
        .check(&auto(true))
        .unwrap();
        assert_eq!(r.state, RunState::Refused);
        let r = Gate {
            elevated: true,
            ..Gate::default()
        }
        .check(&auto(true))
        .unwrap();
        assert_eq!(r.state, RunState::Refused);
        assert!(r.reason.unwrap().contains("SAFE-14"));
    }

    /// A config file others can write, or that someone else owns, is not trusted; one others
    /// can only read is.
    #[test]
    fn only_write_access_for_others_makes_a_file_untrusted() {
        assert!(write_exposures(&[Exposure::Mode { mode: 0o644 }]).is_empty());
        assert!(write_exposures(&[Exposure::Mode { mode: 0o755 }]).is_empty());
        assert_eq!(write_exposures(&[Exposure::Mode { mode: 0o664 }]).len(), 1);
        assert_eq!(write_exposures(&[Exposure::Mode { mode: 0o646 }]).len(), 1);
        assert_eq!(
            write_exposures(&[Exposure::Owner {
                owner: "1001".into()
            }])
            .len(),
            1
        );
        let read_only = 0x0012_0089; // FILE_GENERIC_READ
        assert!(
            write_exposures(&[Exposure::Grant {
                sid: "S-1-5-32-545".into(),
                mask: read_only
            }])
            .is_empty()
        );
        assert_eq!(
            write_exposures(&[Exposure::Grant {
                sid: "S-1-1-0".into(),
                mask: 0x001F_01FF
            }])
            .len(),
            1
        );
        assert_eq!(write_exposures(&[Exposure::NullDacl]).len(), 1);
    }

    /// Only variables that widen the scan stop the policies; protective ones don't.
    #[test]
    fn widening_environment_is_named() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert!(widening_env(env(&[])).is_empty());
        assert!(widening_env(env(&[("SCULL_PROTECT", "/x"), ("SCULL_EXCLUDE", "/y")])).is_empty());
        assert!(widening_env(env(&[("SCULL_ROOTS", "")])).is_empty());
        assert_eq!(
            widening_env(env(&[
                ("SCULL_ROOTS", "/"),
                ("SCULL_ACTIVITY_WINDOW", "1h"),
                ("SCULL_RULES", "x")
            ])),
            ["SCULL_ROOTS", "SCULL_ACTIVITY_WINDOW", "SCULL_RULES"]
        );
    }

    /// `load_auto` reads the policies and the file's own roots; a disabled policy needs no
    /// trust check.
    #[test]
    fn load_auto_reads_the_file_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        assert_eq!(load_auto(&file, None).unwrap(), None);
        let root = dir.path().join("p");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            &file,
            format!(
                "[scan]\nroots = ['{}']\n\n[[auto.policy]]\nname = 'p'\nkind = 'build_outputs'\n\
                 idle = '60d'\nevery = '7d'\nunder = ['~/x']\n",
                root.display()
            ),
        )
        .unwrap();
        let home = Path::new(if cfg!(windows) { "C:\\h" } else { "/h" });
        let a = load_auto(&file, Some(home)).unwrap().unwrap();
        assert!(!a.auto.any_enabled());
        assert_eq!(a.untrusted, None);
        assert_eq!(a.roots, vec![scull_engine::paths::canon(&root)]);
        assert_eq!(
            a.auto.policy[0].under,
            vec![scull_engine::paths::canon(&home.join("x"))]
        );
    }

    #[test]
    fn render_is_silent_when_off() {
        assert_eq!(render(&AutoPolicyReport::off()), "");
        let r = AutoPolicyReport::stopped("stopped by a kill switch (x)");
        assert_eq!(render(&r), "auto-policies: stopped by a kill switch (x)\n");
    }
}
