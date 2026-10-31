//! Exhaustive property tests for the classification rubric (PRD §8.3, P1-SAFE-01).
//!
//! The signal space is finite (9 booleans and 4 three-valued signals: 2^9 * 3^4 = 41,472
//! combinations), so every combination is enumerated rather than sampled. Random tests (proptest)
//! only cover what enumeration cannot: arbitrary Git file counts and sequences of revalidations.
//!
//! Invariants checked over every combination:
//! 1. The tier is never less protective than any individually triggered rubric row.
//! 2. Any S3 row ⇒ S3.
//! 3. S0 if and only if marker + lockfile + known artifact + local regeneration + every check
//!    passes + not recently active.
//! 4. Monotonicity: making any one signal more protective never lowers the tier.
//! 5. Unknown is never more permissive than the concrete safe answer, is disclosed as
//!    `SignalUnavailable`, and equals the worst concrete value for activity and regeneration
//!    (for in-use and Git tracking it is at least S2; see the `rubric` module docs).
//!
//! Plus: agreement with an independent reference model written straight from the PRD table,
//! internal consistency of every result and of `apply`ed candidates, and the "no marker ⇒ not a
//! candidate" rule (CLS-03).

use proptest::prelude::*;
use scull_core::{
    Action, Candidate, Category, FileIdentity, Flags, FsPath, ItemId, ReasonCode, Regeneration,
    RuleRef, Sizes, Tier,
};
use scull_safety::{GitTracking, Regen, Signals, Tri, apply, classify};

const GIT: [GitTracking; 3] = [
    GitTracking::NotTracked,
    GitTracking::Tracked { files: 1 },
    GitTracking::Unknown,
];
const EXPECTED_COMBINATIONS: usize = 512 * 81;

/// Every combination of signals.
fn all_signals() -> Vec<Signals> {
    let mut out = Vec::with_capacity(EXPECTED_COMBINATIONS);
    for bits in 0u16..512 {
        let b = |i: u16| bits & (1 << i) != 0;
        for git in GIT {
            for in_use in Tri::ALL {
                for regen in Regen::ALL {
                    for recent_activity in Tri::ALL {
                        out.push(Signals {
                            git,
                            in_protected_root: b(0),
                            user_excluded: b(1),
                            protect_marker: b(2),
                            in_use,
                            crosses_mount: b(3),
                            link_escape: b(4),
                            cloud_sync: b(5),
                            marker_present: b(6),
                            lockfile_present: b(7),
                            artifact_known: b(8),
                            regen,
                            recent_activity,
                        });
                    }
                }
            }
        }
    }
    out
}

/// The non-activity rows of the rubric that fire for `s`, with their minimum tier. Written
/// directly from PRD §8.3 plus the documented protective answers for unknowns, independently of
/// the engine's code.
fn triggered_rows(s: &Signals) -> Vec<(&'static str, Tier)> {
    let rows = [
        (
            "git-tracked",
            matches!(s.git, GitTracking::Tracked { .. }),
            Tier::S3,
        ),
        ("protected-root", s.in_protected_root, Tier::S3),
        ("user-exclusion", s.user_excluded, Tier::S3),
        ("protect-marker", s.protect_marker, Tier::S3),
        ("in-use", s.in_use == Tri::Yes, Tier::S3),
        ("crosses-mount", s.crosses_mount, Tier::S3),
        ("link-escape", s.link_escape, Tier::S3),
        ("no-marker", !s.marker_present, Tier::S3),
        ("git-unknown", s.git == GitTracking::Unknown, Tier::S2),
        ("in-use-unknown", s.in_use == Tri::Unknown, Tier::S2),
        ("cloud-sync", s.cloud_sync, Tier::S2),
        (
            "no-lockfile",
            s.marker_present && !s.lockfile_present,
            Tier::S2,
        ),
        (
            "unknown-artifact",
            s.marker_present && s.lockfile_present && !s.artifact_known,
            Tier::S2,
        ),
    ];
    rows.into_iter()
        .filter(|(_, fired, _)| *fired)
        .map(|(name, _, tier)| (name, tier))
        .collect()
}

fn bump(t: Tier) -> Tier {
    match t {
        Tier::S0 => Tier::S1,
        Tier::S1 => Tier::S2,
        Tier::S2 | Tier::S3 => Tier::S3,
    }
}

/// Reference model of PRD §8.3.
fn reference_tier(s: &Signals) -> Tier {
    let rows = triggered_rows(s);
    let base = if rows.is_empty() {
        match s.regen {
            Regen::Local => Tier::S0,
            Regen::NetworkOrLong | Regen::Unknown => Tier::S1,
        }
    } else {
        rows.iter().map(|(_, t)| *t).max().unwrap_or(Tier::S3)
    };
    if s.recent_activity == Tri::No {
        base
    } else {
        bump(base)
    }
}

/// Every signals value that differs from `s` in exactly one signal, made more protective.
fn more_protective_neighbours(s: &Signals) -> Vec<(&'static str, Signals)> {
    let mut out = Vec::new();
    let mut push = |name, n: Signals| {
        if n != *s {
            out.push((name, n));
        }
    };
    match s.git {
        GitTracking::NotTracked => {
            push(
                "git→unknown",
                Signals {
                    git: GitTracking::Unknown,
                    ..*s
                },
            );
            push(
                "git→tracked",
                Signals {
                    git: GitTracking::Tracked { files: 1 },
                    ..*s
                },
            );
        }
        GitTracking::Unknown => {
            push(
                "git→tracked",
                Signals {
                    git: GitTracking::Tracked { files: 1 },
                    ..*s
                },
            );
        }
        GitTracking::Tracked { .. } => {}
    }
    push(
        "protected-root",
        Signals {
            in_protected_root: true,
            ..*s
        },
    );
    push(
        "user-exclusion",
        Signals {
            user_excluded: true,
            ..*s
        },
    );
    push(
        "protect-marker",
        Signals {
            protect_marker: true,
            ..*s
        },
    );
    match s.in_use {
        Tri::No => {
            push(
                "in-use→unknown",
                Signals {
                    in_use: Tri::Unknown,
                    ..*s
                },
            );
            push(
                "in-use→yes",
                Signals {
                    in_use: Tri::Yes,
                    ..*s
                },
            );
        }
        Tri::Unknown => push(
            "in-use→yes",
            Signals {
                in_use: Tri::Yes,
                ..*s
            },
        ),
        Tri::Yes => {}
    }
    push(
        "crosses-mount",
        Signals {
            crosses_mount: true,
            ..*s
        },
    );
    push(
        "link-escape",
        Signals {
            link_escape: true,
            ..*s
        },
    );
    push(
        "cloud-sync",
        Signals {
            cloud_sync: true,
            ..*s
        },
    );
    push(
        "marker-removed",
        Signals {
            marker_present: false,
            ..*s
        },
    );
    push(
        "lockfile-removed",
        Signals {
            lockfile_present: false,
            ..*s
        },
    );
    push(
        "artifact-unknown",
        Signals {
            artifact_known: false,
            ..*s
        },
    );
    if s.regen == Regen::Local {
        push(
            "regen→network",
            Signals {
                regen: Regen::NetworkOrLong,
                ..*s
            },
        );
        push(
            "regen→unknown",
            Signals {
                regen: Regen::Unknown,
                ..*s
            },
        );
    }
    match s.recent_activity {
        Tri::No => {
            push(
                "activity→unknown",
                Signals {
                    recent_activity: Tri::Unknown,
                    ..*s
                },
            );
            push(
                "activity→yes",
                Signals {
                    recent_activity: Tri::Yes,
                    ..*s
                },
            );
        }
        Tri::Unknown => push(
            "activity→yes",
            Signals {
                recent_activity: Tri::Yes,
                ..*s
            },
        ),
        Tri::Yes => {}
    }
    out
}

fn tier(s: &Signals) -> Tier {
    classify(s).tier()
}

fn fresh_candidate() -> Candidate {
    let identity = FileIdentity::new(7, 42);
    Candidate {
        id: ItemId::derive(identity, b"/fixture/proj/target"),
        path: FsPath::from("/fixture/proj/target"),
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
fn enumeration_covers_every_combination_once() {
    let all = all_signals();
    assert_eq!(all.len(), EXPECTED_COMBINATIONS);
    let unique: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(unique.len(), EXPECTED_COMBINATIONS);
}

#[test]
fn exhaustive_matches_reference_model_of_prd_8_3() {
    for s in all_signals() {
        assert_eq!(tier(&s), reference_tier(&s), "{s:?}");
    }
}

#[test]
fn exhaustive_inv1_never_less_protective_than_any_triggered_row() {
    for s in all_signals() {
        let t = tier(&s);
        for (row, min) in triggered_rows(&s) {
            assert!(t >= min, "row {row} requires {min}, got {t} for {s:?}");
            if s.recent_activity != Tri::No {
                assert!(t >= bump(min), "activity bump missing on {row} for {s:?}");
            }
        }
    }
}

#[test]
fn exhaustive_inv2_any_s3_row_gives_s3() {
    for s in all_signals() {
        if triggered_rows(&s).iter().any(|(_, t)| *t == Tier::S3) {
            assert_eq!(tier(&s), Tier::S3, "{s:?}");
        }
    }
}

#[test]
fn exhaustive_inv3_s0_only_for_verified_idle_local_artifacts() {
    let mut s0_count = 0;
    for s in all_signals() {
        let verified = s.marker_present
            && s.lockfile_present
            && s.artifact_known
            && s.regen == Regen::Local
            && s.git == GitTracking::NotTracked
            && !s.in_protected_root
            && !s.user_excluded
            && !s.protect_marker
            && s.in_use == Tri::No
            && !s.crosses_mount
            && !s.link_escape
            && !s.cloud_sync
            && s.recent_activity == Tri::No;
        assert_eq!(tier(&s) == Tier::S0, verified, "{s:?}");
        s0_count += usize::from(verified);
    }
    assert_eq!(s0_count, 1, "exactly one combination is S0");
}

#[test]
fn exhaustive_inv4_more_protective_signal_never_lowers_tier() {
    let mut checked = 0usize;
    for s in all_signals() {
        let t = tier(&s);
        for (step, n) in more_protective_neighbours(&s) {
            let tn = tier(&n);
            assert!(tn >= t, "{step} lowered {t} → {tn} from {s:?}");
            checked += 1;
        }
    }
    assert!(checked > EXPECTED_COMBINATIONS, "neighbours were generated");
}

#[test]
fn exhaustive_inv5_unknown_is_never_treated_as_safe() {
    for s in all_signals() {
        // In use.
        let [no, yes, unknown] = Tri::ALL.map(|in_use| classify(&Signals { in_use, ..s }));
        assert!(unknown.tier() >= no.tier(), "in-use unknown < no: {s:?}");
        assert!(unknown.tier() <= yes.tier(), "in-use unknown > yes: {s:?}");
        assert!(unknown.tier() >= Tier::S2, "in-use unknown below S2: {s:?}");
        assert!(
            unknown
                .reasons()
                .iter()
                .any(|r| r.code == ReasonCode::SignalUnavailable),
            "in-use unknown not disclosed: {s:?}"
        );

        // Git tracking.
        let [not, tracked, unknown] = GIT.map(|git| classify(&Signals { git, ..s }));
        assert!(
            unknown.tier() >= not.tier(),
            "git unknown < not tracked: {s:?}"
        );
        assert!(
            unknown.tier() <= tracked.tier(),
            "git unknown > tracked: {s:?}"
        );
        assert!(unknown.tier() >= Tier::S2, "git unknown below S2: {s:?}");
        assert!(
            unknown
                .reasons()
                .iter()
                .any(|r| r.code == ReasonCode::SignalUnavailable),
            "git unknown not disclosed: {s:?}"
        );

        // Activity: unknown is exactly as protective as recent activity.
        let [no, yes, unknown] = Tri::ALL.map(|recent_activity| {
            tier(&Signals {
                recent_activity,
                ..s
            })
        });
        assert!(unknown >= no, "activity unknown < idle: {s:?}");
        assert_eq!(unknown, yes, "activity unknown ≠ recent: {s:?}");

        // Regeneration: unknown cost is exactly as protective as network/long build.
        let [local, network, unknown] = Regen::ALL.map(|regen| tier(&Signals { regen, ..s }));
        assert!(unknown >= local, "regen unknown < local: {s:?}");
        assert_eq!(unknown, network, "regen unknown ≠ network: {s:?}");
    }
}

#[test]
fn exhaustive_no_marker_is_never_a_candidate_and_always_s3() {
    for s in all_signals() {
        let c = classify(&s);
        assert_eq!(c.is_candidate(), s.marker_present, "{s:?}");
        if !s.marker_present {
            assert_eq!(c.tier(), Tier::S3, "{s:?}");
        }
    }
}

#[test]
fn exhaustive_s0_s1_require_marker_lockfile_and_known_artifact() {
    for s in all_signals() {
        if tier(&s) <= Tier::S1 {
            assert!(
                s.marker_present && s.lockfile_present && s.artifact_known,
                "{s:?}"
            );
        }
    }
}

#[test]
fn exhaustive_results_are_consistent_and_deterministic() {
    for s in all_signals() {
        let c = classify(&s);
        assert!(!c.reasons().is_empty(), "{s:?}");
        assert_eq!(c.tier(), Candidate::tier_from_reasons(c.reasons()), "{s:?}");
        assert_eq!(c, classify(&s), "not deterministic: {s:?}");
        // The activity reason, when present, is always last; everything before it is a row.
        let rows = if s.recent_activity == Tri::No {
            c.reasons()
        } else {
            &c.reasons()[..c.reasons().len() - 1]
        };
        let positive = rows.iter().any(|r| {
            matches!(
                r.code,
                ReasonCode::Regenerable | ReasonCode::CostlyToRegenerate
            )
        });
        if positive {
            assert_eq!(rows.len(), 1, "S0/S1 row beside a failed check: {s:?}");
        }
        assert_eq!(positive, triggered_rows(&s).is_empty(), "{s:?}");
        assert_eq!(rows.len(), triggered_rows(&s).len().max(1), "{s:?}");
    }
}

#[test]
fn exhaustive_apply_produces_consistent_candidates() {
    for s in all_signals() {
        let mut cand = fresh_candidate();
        let c = apply(&mut cand, &s);
        assert_eq!(cand.tier, c.tier(), "{s:?}");
        assert!(cand.is_consistent(), "{s:?}");
        assert_eq!(cand.action == Action::None, cand.tier == Tier::S3, "{s:?}");
        assert_eq!(
            cand.action == Action::Delete,
            cand.tier == Tier::S0,
            "{s:?}"
        );
        assert_eq!(cand.flags.active, s.in_use.as_option());
        assert_eq!(cand.flags.cross_mount, Some(s.crosses_mount));
        assert_eq!(cand.flags.cloud_placeholder, Some(s.cloud_sync));
        // Revalidating with the same signals changes nothing about the tier.
        apply(&mut cand, &s);
        assert_eq!(cand.tier, c.tier(), "{s:?}");
    }
}

#[test]
fn any_git_tracked_count_is_s3() {
    for files in [0, 1, 2, 1_000, u64::MAX] {
        for s in all_signals() {
            let s = Signals {
                git: GitTracking::Tracked { files },
                ..s
            };
            assert_eq!(tier(&s), Tier::S3, "{s:?}");
        }
    }
}

fn arb_signals() -> impl Strategy<Value = Signals> {
    let git = prop_oneof![
        Just(GitTracking::NotTracked),
        any::<u64>().prop_map(|files| GitTracking::Tracked { files }),
        Just(GitTracking::Unknown),
    ];
    let tri = || prop::sample::select(Tri::ALL.to_vec());
    (
        git,
        prop::array::uniform9(any::<bool>()),
        tri(),
        prop::sample::select(Regen::ALL.to_vec()),
        tri(),
    )
        .prop_map(|(git, b, in_use, regen, recent_activity)| Signals {
            git,
            in_protected_root: b[0],
            user_excluded: b[1],
            protect_marker: b[2],
            in_use,
            crosses_mount: b[3],
            link_escape: b[4],
            cloud_sync: b[5],
            marker_present: b[6],
            lockfile_present: b[7],
            artifact_known: b[8],
            regen,
            recent_activity,
        })
}

proptest! {
    /// Repeated revalidation with changing signals (I-8): the candidate's tier never decreases,
    /// always equals the most protective classification seen so far, and stays consistent.
    #[test]
    fn prop_revalidation_never_lowers_tier(seq in prop::collection::vec(arb_signals(), 1..12)) {
        let mut cand = fresh_candidate();
        let mut highest = Tier::S0;
        for s in &seq {
            let before = cand.tier;
            let t = apply(&mut cand, s).tier();
            highest = highest.max(t);
            prop_assert!(cand.tier >= before);
            prop_assert_eq!(cand.tier, highest);
            prop_assert!(cand.is_consistent());
        }
    }

    /// Random signals (including arbitrary Git file counts) agree with the reference model.
    #[test]
    fn prop_random_signals_match_reference(s in arb_signals()) {
        prop_assert_eq!(tier(&s), reference_tier(&s));
    }
}
