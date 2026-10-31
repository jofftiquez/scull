//! The classification rubric (PRD §8.3): turns the observed facts about one candidate into the
//! [`TierReason`]s that fired and the resulting [`Tier`].
//!
//! The engine is **pure**: no filesystem, process, clock or network access. Detectors, the Git
//! layer and the platform layer collect the facts and hand them over as [`Signals`]; the same
//! signals always produce the same classification on every platform and in every front end
//! (PRD principle 6). Use [`classify`] to get a [`Classification`], or [`apply`] to fold it into a
//! [`Candidate`] through [`Candidate::raise`].
//!
//! # The rubric
//!
//! A candidate's tier is the **most protective** tier triggered by any row. Each row maps to one
//! field of [`Signals`] and one [`ReasonCode`]:
//!
//! | PRD §8.3 condition | Signal | Minimum tier | Code |
//! |---|---|---|---|
//! | Any file inside is tracked by Git | [`Signals::git`] = [`GitTracking::Tracked`] | S3 | [`ReasonCode::GitTracked`] |
//! | Inside a protected root | [`Signals::in_protected_root`] | S3 | [`ReasonCode::ProtectedRoot`] |
//! | Matches a user exclusion | [`Signals::user_excluded`] | S3 | [`ReasonCode::UserExclusion`] |
//! | Covered by a `.scull-protect` marker | [`Signals::protect_marker`] | S3 | [`ReasonCode::ProtectMarker`] |
//! | In use (open handles, process cwd, held tool lock) | [`Signals::in_use`] = [`Tri::Yes`] | S3 for this run | [`ReasonCode::InUse`] |
//! | Crosses a mount/volume boundary | [`Signals::crosses_mount`] | S3 | [`ReasonCode::CrossesMount`] |
//! | Reached through a link outside the scan root | [`Signals::link_escape`] | S3 | [`ReasonCode::LinkEscape`] |
//! | Cloud-sync placeholder or synced folder | [`Signals::cloud_sync`] | S2 | [`ReasonCode::CloudSync`] |
//! | Marker found, but no lockfile/manifest | [`Signals::lockfile_present`] = `false` | S2 | [`ReasonCode::NoLockfile`] |
//! | Modified within the activity window | [`Signals::recent_activity`] = [`Tri::Yes`] | one tier more protective | [`ReasonCode::RecentActivity`] |
//! | Marker + lockfile, known artifact, all checks pass, needs network or a long build | [`Signals::regen`] = [`Regen::NetworkOrLong`] | S1 | [`ReasonCode::CostlyToRegenerate`] |
//! | Marker + lockfile, known artifact, all checks pass, regenerable locally or quickly | [`Signals::regen`] = [`Regen::Local`] | S0 | [`ReasonCode::Regenerable`] |
//!
//! "All checks pass" means no S2 or S3 row fired (including the protective answers for unknown
//! signals below). The S0/S1 rows only fire then, so the preview never lists "regenerable" next
//! to a blocking reason. The activity bump is applied last, to the most protective tier of all
//! the other rows: S0 → S1, S1 → S2, S2 → S3 ([`Tier::bumped`]).
//!
//! # Missing evidence (PRD §8.4, THREAT_MODEL T-01, T-02, T-04)
//!
//! Missing evidence is never evidence of safety. Each unknown gets a fixed protective answer,
//! recorded as a [`ReasonCode::SignalUnavailable`] reason so the preview says so:
//!
//! | Unknown | Treated as | Why |
//! |---|---|---|
//! | No project marker ([`Signals::marker_present`] = `false`) | **S3**, not a candidate | CLS-03: a directory name alone is never evidence. Callers should drop the item ([`Classification::is_candidate`]); if they keep it, it is blocked. |
//! | Artifact type not known to a rule | S2 | Reproducibility is unclear (S2 by definition, §8.2); the S0/S1 rows require a known artifact. |
//! | Regeneration cost unknown ([`Regen::Unknown`]) | S1 | Reproducible from the lockfile, cost unproven: assume the costly case (D-5). Same tier as the worst concrete value. |
//! | In-use unknown ([`Tri::Unknown`]) | S2 | §8.4: an unavailable signal is disclosed, not silently ignored, and never counts as "idle". S2 is never preselected, needs per-item opt-in and goes to quarantine; revalidation re-checks use before execution (SAFE-05). S3 would block every candidate on platforms without open-file detection, which §8.4 ("where available") does not intend. |
//! | Git tracking unknown ([`GitTracking::Unknown`]) | S2 | Ownership is unclear; THREAT_MODEL T-01 residual: "Unknown → S2, never S0". |
//! | Activity unknown ([`Tri::Unknown`]) | bumped, like recent activity | §8.4: "missing evidence never counts as evidence of inactivity". Same tier as the worst concrete value. |
//!
//! The boolean signals ([`Signals::crosses_mount`], [`Signals::link_escape`],
//! [`Signals::cloud_sync`] and the protection flags) have no unknown state: a caller that cannot
//! determine one must pass `true` (the protective value) and record
//! [`scull_core::Evidence::SignalUnavailable`] on the candidate.
//!
//! # Scope
//!
//! The Git worktree decision table (GIT-03) is a separate table evaluated by the Git layer; its
//! reasons combine with these through [`Candidate::raise`] (most protective wins). This module
//! does not implement it.
//!
//! The exhaustive property test over every combination of signals lives in
//! `crates/scull-safety/tests/rubric_exhaustive.rs`.

use scull_core::{Candidate, ReasonCode, Tier, TierReason};

/// A three-valued observation. `Unknown` means the signal could not be read; it is never
/// treated as `No` (PRD §8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tri {
    /// Observed: no.
    No,
    /// Observed: yes.
    Yes,
    /// Could not be observed on this platform or with current permissions.
    Unknown,
}

impl Tri {
    /// All values, for exhaustive tests.
    pub const ALL: [Tri; 3] = [Tri::No, Tri::Yes, Tri::Unknown];

    /// `Some(true)` / `Some(false)` / `None`, matching [`scull_core::Flags`].
    pub fn as_option(self) -> Option<bool> {
        match self {
            Tri::No => Some(false),
            Tri::Yes => Some(true),
            Tri::Unknown => None,
        }
    }
}

/// Whether any file inside the candidate is tracked by Git.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitTracking {
    /// Checked: nothing inside is tracked (or the candidate is not inside a repository).
    NotTracked,
    /// At least one file inside is tracked. Any count, including a lower bound, triggers S3.
    Tracked {
        /// Number of tracked files found (may be a lower bound).
        files: u64,
    },
    /// Tracking could not be determined (e.g. Git failed or refused to run).
    Unknown,
}

/// How the artifact would be regenerated, as declared by the rule and backed by evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Regen {
    /// Regenerable from local inputs, or quickly.
    Local,
    /// Rebuilding needs network access or a long build.
    NetworkOrLong,
    /// No evidence about the cost (D-5). Treated like [`Regen::NetworkOrLong`].
    Unknown,
}

impl Regen {
    /// All values, for exhaustive tests.
    pub const ALL: [Regen; 3] = [Regen::Local, Regen::NetworkOrLong, Regen::Unknown];
}

/// The observed facts about one candidate that the rubric needs.
///
/// There is deliberately no `Default`: every signal must be stated by the caller, so a new
/// signal added here is a compile error at every call site rather than a silent "safe" default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signals {
    /// Git tracking of files inside the candidate.
    pub git: GitTracking,
    /// The path is inside a protected root (SAFE-03, compared by identity, SAFE-13).
    pub in_protected_root: bool,
    /// The path matches a user exclusion.
    pub user_excluded: bool,
    /// A `.scull-protect` marker covers the path.
    pub protect_marker: bool,
    /// In use: open handles, the cwd of a running process, or a lock file held by a known tool.
    pub in_use: Tri,
    /// The candidate crosses a mount or volume boundary.
    pub crosses_mount: bool,
    /// The candidate is reached through a symlink or junction pointing outside the scan root.
    pub link_escape: bool,
    /// Cloud-sync placeholder, or inside a synced folder (iCloud, OneDrive, Dropbox).
    pub cloud_sync: bool,
    /// The rule's required project marker was found at the expected relative location (CLS-03).
    pub marker_present: bool,
    /// A lockfile or manifest the artifact can be reproduced from is present.
    pub lockfile_present: bool,
    /// The artifact type is known to a rule.
    pub artifact_known: bool,
    /// Regeneration cost.
    pub regen: Regen,
    /// Modified within the activity window (see [`ActivityWindow::observe`]).
    pub recent_activity: Tri,
}

/// The activity window from PRD §8.3 (D-8: 24 hours by default, configurable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActivityWindow {
    secs: u64,
}

impl Default for ActivityWindow {
    fn default() -> Self {
        Self::from_secs(Self::DEFAULT_SECS)
    }
}

impl ActivityWindow {
    /// Default window: 24 hours (D-8).
    pub const DEFAULT_SECS: u64 = 24 * 60 * 60;

    /// A window of `secs` seconds.
    pub const fn from_secs(secs: u64) -> Self {
        Self { secs }
    }

    /// Window length in seconds.
    pub const fn secs(self) -> u64 {
        self.secs
    }

    /// Turn a newest-mtime observation into [`Signals::recent_activity`].
    ///
    /// * `newest_unix`: newest observed modification time (Unix seconds), `None` if unreadable.
    /// * `sampled`: whether sampling hit its bound, so the true newest may be later.
    /// * `now_unix`: the current time, supplied by the caller (this module reads no clock).
    ///
    /// Returns `Yes` when the newest mtime is inside the window or in the future (clock skew is
    /// read as activity), `No` only when a *complete* observation is older than the window, and
    /// `Unknown` otherwise: an unreadable mtime, or a sampled one that looks old, is not
    /// evidence of inactivity (PRD §8.4).
    pub fn observe(self, newest_unix: Option<i64>, sampled: bool, now_unix: i64) -> Tri {
        let Some(newest) = newest_unix else {
            return Tri::Unknown;
        };
        let age = i128::from(now_unix) - i128::from(newest);
        if age <= i128::from(self.secs) {
            Tri::Yes
        } else if sampled {
            Tri::Unknown
        } else {
            Tri::No
        }
    }
}

/// The rubric's verdict for one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    tier: Tier,
    reasons: Vec<TierReason>,
    is_candidate: bool,
}

impl Classification {
    /// The final tier: the most protective of [`Classification::reasons`].
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Every rubric row that fired, in table order, with the activity bump last. Never empty.
    pub fn reasons(&self) -> &[TierReason] {
        &self.reasons
    }

    /// Consume the classification, returning its reasons.
    pub fn into_reasons(self) -> Vec<TierReason> {
        self.reasons
    }

    /// Whether the item is a candidate at all. `false` when the required project marker is
    /// missing (CLS-03); such items are classified S3 and should not be listed.
    pub fn is_candidate(&self) -> bool {
        self.is_candidate
    }
}

/// Classify one candidate from its signals (PRD §8.3). Pure and deterministic.
pub fn classify(s: &Signals) -> Classification {
    let mut reasons = Vec::new();
    let mut fire = |tier: Tier, code: ReasonCode, message: String| {
        reasons.push(TierReason::new(tier, code, message));
    };

    // Checks: S3 and S2 rows, and the protective answers for unknown signals.
    match s.git {
        GitTracking::NotTracked => {}
        GitTracking::Tracked { files } => fire(
            Tier::S3,
            ReasonCode::GitTracked,
            format!("contains files tracked by Git (at least {files})"),
        ),
        GitTracking::Unknown => fire(
            Tier::S2,
            ReasonCode::SignalUnavailable,
            "could not determine whether files are tracked by Git; assumed they may be".into(),
        ),
    }
    if s.in_protected_root {
        fire(
            Tier::S3,
            ReasonCode::ProtectedRoot,
            "inside a protected root".into(),
        );
    }
    if s.user_excluded {
        fire(
            Tier::S3,
            ReasonCode::UserExclusion,
            "matches a user exclusion".into(),
        );
    }
    if s.protect_marker {
        fire(
            Tier::S3,
            ReasonCode::ProtectMarker,
            "covered by a .scull-protect marker".into(),
        );
    }
    match s.in_use {
        Tri::No => {}
        Tri::Yes => fire(Tier::S3, ReasonCode::InUse, "in use (this run)".into()),
        Tri::Unknown => fire(
            Tier::S2,
            ReasonCode::SignalUnavailable,
            "could not check whether it is in use; not treated as idle".into(),
        ),
    }
    if s.crosses_mount {
        fire(
            Tier::S3,
            ReasonCode::CrossesMount,
            "crosses a mount or volume boundary".into(),
        );
    }
    if s.link_escape {
        fire(
            Tier::S3,
            ReasonCode::LinkEscape,
            "reached through a link outside the scan root".into(),
        );
    }
    if s.cloud_sync {
        fire(
            Tier::S2,
            ReasonCode::CloudSync,
            "cloud-sync placeholder or synced folder (never hydrated)".into(),
        );
    }

    // Positive evidence: marker, lockfile, known artifact, regeneration cost.
    if !s.marker_present {
        fire(
            Tier::S3,
            ReasonCode::SignalUnavailable,
            "no project marker at the expected location; not a candidate".into(),
        );
    } else if !s.lockfile_present {
        fire(
            Tier::S2,
            ReasonCode::NoLockfile,
            "project marker found, but no lockfile or manifest to reproduce from".into(),
        );
    } else if !s.artifact_known {
        fire(
            Tier::S2,
            ReasonCode::SignalUnavailable,
            "artifact type is not known to any rule".into(),
        );
    }
    let checks_pass = reasons.is_empty();
    if checks_pass {
        // Only reachable with marker + lockfile + known artifact and no S2/S3 row.
        match s.regen {
            Regen::Local => reasons.push(TierReason::new(
                Tier::S0,
                ReasonCode::Regenerable,
                "regenerable from local inputs",
            )),
            Regen::NetworkOrLong => reasons.push(TierReason::new(
                Tier::S1,
                ReasonCode::CostlyToRegenerate,
                "reproducible, but needs network or a long build",
            )),
            Regen::Unknown => reasons.push(TierReason::new(
                Tier::S1,
                ReasonCode::CostlyToRegenerate,
                "reproducible, regeneration cost unknown; assumed costly",
            )),
        }
    }

    // Activity window: one tier more protective than everything else.
    let base = Candidate::tier_from_reasons(&reasons);
    match s.recent_activity {
        Tri::No => {}
        Tri::Yes => reasons.push(TierReason::new(
            base.bumped(),
            ReasonCode::RecentActivity,
            "modified within the activity window",
        )),
        Tri::Unknown => reasons.push(TierReason::new(
            base.bumped(),
            ReasonCode::SignalUnavailable,
            "could not read recent modification times; treated as recent activity",
        )),
    }

    let tier = Candidate::tier_from_reasons(&reasons);
    Classification {
        tier,
        reasons,
        is_candidate: s.marker_present,
    }
}

/// Classify `candidate` from `signals` and fold the result in with [`Candidate::raise`], so the
/// tier only ever goes up and the action follows it (S3 → no action, `Delete` only at S0).
///
/// Also records the risk flags the signals establish (`active`, `cross_mount`,
/// `cloud_placeholder`).
///
/// A freshly detected candidate should start at [`Tier::S0`] with no reasons: the rubric is what
/// sets its tier. On revalidation the existing reasons are kept and the tier never decreases
/// (invariant I-8).
pub fn apply(candidate: &mut Candidate, signals: &Signals) -> Classification {
    let classification = classify(signals);
    for reason in classification.reasons() {
        candidate.raise(reason.clone());
    }
    candidate.flags.active = signals.in_use.as_option();
    candidate.flags.cross_mount = Some(signals.crosses_mount);
    candidate.flags.cloud_placeholder = Some(signals.cloud_sync);
    classification
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::{
        Action, Category, FileIdentity, Flags, FsPath, ItemId, Regeneration, RuleRef, Sizes,
    };

    /// Marker + lockfile + known artifact + local regen, every check passing, idle.
    fn verified_local() -> Signals {
        Signals {
            git: GitTracking::NotTracked,
            in_protected_root: false,
            user_excluded: false,
            protect_marker: false,
            in_use: Tri::No,
            crosses_mount: false,
            link_escape: false,
            cloud_sync: false,
            marker_present: true,
            lockfile_present: true,
            artifact_known: true,
            regen: Regen::Local,
            recent_activity: Tri::No,
        }
    }

    fn codes(c: &Classification) -> Vec<ReasonCode> {
        c.reasons().iter().map(|r| r.code).collect()
    }

    fn fresh_candidate() -> Candidate {
        let identity = FileIdentity::new(1, 2);
        Candidate {
            id: ItemId::derive(identity, b"/p/target"),
            path: FsPath::from("/p/target"),
            identity,
            category: Category::ProjectArtifact,
            artifact: "Cargo build output".into(),
            rule: RuleRef {
                id: "rust.target".into(),
                version: 1,
            },
            project: None,
            tier: Tier::S0,
            reasons: Vec::new(),
            evidence: Vec::new(),
            sizes: Sizes::default(),
            flags: Flags::default(),
            regeneration: Regeneration::default(),
            action: Action::Delete,
        }
    }

    #[test]
    fn fx_cls_01_cargo_target_with_lockfile_idle_is_s0() {
        let c = classify(&verified_local());
        assert_eq!(c.tier(), Tier::S0);
        assert_eq!(codes(&c), vec![ReasonCode::Regenerable]);
        assert!(c.is_candidate());
    }

    #[test]
    fn fx_cls_03_git_tracked_dist_is_s3() {
        let s = Signals {
            git: GitTracking::Tracked { files: 12 },
            ..verified_local()
        };
        let c = classify(&s);
        assert_eq!(c.tier(), Tier::S3);
        assert_eq!(codes(&c), vec![ReasonCode::GitTracked]);
        let mut cand = fresh_candidate();
        apply(&mut cand, &s);
        assert_eq!(cand.action, Action::None, "never in a plan");
        assert!(cand.is_consistent());
    }

    #[test]
    fn fx_cls_05_node_modules_with_lockfile_s0_without_s2() {
        assert_eq!(classify(&verified_local()).tier(), Tier::S0);
        let without = classify(&Signals {
            lockfile_present: false,
            ..verified_local()
        });
        assert_eq!(without.tier(), Tier::S2);
        assert_eq!(codes(&without), vec![ReasonCode::NoLockfile]);
    }

    #[test]
    fn fx_cls_09_recent_activity_bumps_s0_to_s1() {
        let c = classify(&Signals {
            recent_activity: Tri::Yes,
            ..verified_local()
        });
        assert_eq!(c.tier(), Tier::S1);
        assert_eq!(
            codes(&c),
            vec![ReasonCode::Regenerable, ReasonCode::RecentActivity]
        );
    }

    #[test]
    fn fx_cls_11_scull_protect_marker_is_s3() {
        let c = classify(&Signals {
            protect_marker: true,
            ..verified_local()
        });
        assert_eq!(c.tier(), Tier::S3);
        assert_eq!(codes(&c), vec![ReasonCode::ProtectMarker]);
    }

    #[test]
    fn fx_cls_02_no_marker_is_not_a_candidate_and_s3() {
        let c = classify(&Signals {
            marker_present: false,
            lockfile_present: false,
            artifact_known: false,
            ..verified_local()
        });
        assert!(!c.is_candidate());
        assert_eq!(c.tier(), Tier::S3);
    }

    #[test]
    fn network_or_unknown_regen_is_s1() {
        for regen in [Regen::NetworkOrLong, Regen::Unknown] {
            let c = classify(&Signals {
                regen,
                ..verified_local()
            });
            assert_eq!(c.tier(), Tier::S1);
            assert_eq!(codes(&c), vec![ReasonCode::CostlyToRegenerate]);
        }
    }

    #[test]
    fn unknown_in_use_is_s2_and_disclosed() {
        let c = classify(&Signals {
            in_use: Tri::Unknown,
            ..verified_local()
        });
        assert_eq!(c.tier(), Tier::S2);
        assert_eq!(codes(&c), vec![ReasonCode::SignalUnavailable]);
    }

    #[test]
    fn unknown_activity_bumps_like_recent_activity() {
        let c = classify(&Signals {
            recent_activity: Tri::Unknown,
            ..verified_local()
        });
        assert_eq!(c.tier(), Tier::S1);
        assert_eq!(
            codes(&c),
            vec![ReasonCode::Regenerable, ReasonCode::SignalUnavailable]
        );
    }

    #[test]
    fn cloud_sync_recently_modified_becomes_s3() {
        let c = classify(&Signals {
            cloud_sync: true,
            recent_activity: Tri::Yes,
            ..verified_local()
        });
        assert_eq!(c.tier(), Tier::S3);
    }

    #[test]
    fn positive_rows_do_not_fire_when_a_check_fails() {
        let c = classify(&Signals {
            in_use: Tri::Yes,
            ..verified_local()
        });
        assert_eq!(codes(&c), vec![ReasonCode::InUse]);
    }

    #[test]
    fn apply_sets_tier_action_and_flags_consistently() {
        let mut cand = fresh_candidate();
        let c = apply(
            &mut cand,
            &Signals {
                regen: Regen::NetworkOrLong,
                in_use: Tri::No,
                ..verified_local()
            },
        );
        assert_eq!(cand.tier, Tier::S1);
        assert_eq!(cand.tier, c.tier());
        assert_eq!(cand.action, Action::Quarantine, "Delete only at S0 (D-3)");
        assert_eq!(cand.flags.active, Some(false));
        assert_eq!(cand.flags.cross_mount, Some(false));
        assert_eq!(cand.flags.cloud_placeholder, Some(false));
        assert!(cand.is_consistent());
    }

    #[test]
    fn apply_never_lowers_an_existing_tier() {
        let mut cand = fresh_candidate();
        apply(
            &mut cand,
            &Signals {
                in_use: Tri::Yes,
                ..verified_local()
            },
        );
        assert_eq!(cand.tier, Tier::S3);
        // Revalidation later finds it idle: the tier stays S3 (I-8).
        apply(&mut cand, &verified_local());
        assert_eq!(cand.tier, Tier::S3);
        assert_eq!(cand.action, Action::None);
        assert!(cand.is_consistent());
    }

    #[test]
    fn activity_window_observe() {
        let w = ActivityWindow::default();
        assert_eq!(w.secs(), 86_400);
        let now = 1_000_000_000;
        assert_eq!(w.observe(None, false, now), Tri::Unknown);
        assert_eq!(w.observe(Some(now - 3600), false, now), Tri::Yes);
        assert_eq!(w.observe(Some(now - 3600), true, now), Tri::Yes);
        assert_eq!(w.observe(Some(now - 86_400), false, now), Tri::Yes);
        assert_eq!(w.observe(Some(now - 3 * 86_400), false, now), Tri::No);
        assert_eq!(w.observe(Some(now - 3 * 86_400), true, now), Tri::Unknown);
        assert_eq!(w.observe(Some(now + 60), false, now), Tri::Yes, "future");
        assert_eq!(w.observe(Some(i64::MIN), false, i64::MAX), Tri::No);
        assert_eq!(w.observe(Some(i64::MAX), false, i64::MIN), Tri::Yes);
        let zero = ActivityWindow::from_secs(0);
        assert_eq!(zero.observe(Some(now - 1), false, now), Tri::No);
    }
}
