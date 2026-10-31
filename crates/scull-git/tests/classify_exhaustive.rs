//! Exhaustive tests for the GIT-03 worktree decision table (P1-GIT-03).
//!
//! The state space the classifier looks at is finite once each fact is reduced to its
//! decision-relevant values, so every combination is enumerated (including contradictory ones
//! a real repository can't produce: the classifier must stay protective on those too).
//!
//! Checked over every combination:
//! 1. Agreement with an independent reference model written straight from the PRD table.
//! 2. Every table row that applies fires with its code and at least its tier.
//! 3. Never S0 except a stale registration (GIT-04); a present worktree is at least S1.
//! 4. Monotonicity: making any one fact worse (or unknown) never lowers the tier.
//! 5. Unknown facts are disclosed as `SignalUnavailable` and never lead to S1.
//! 6. Action / artifact-cleanup columns, and the `Candidate` is consistent.

use scull_core::{Action, FileIdentity, FsPath, ReasonCode, Sizes, Tier};
use scull_git::{
    ArtifactCleanup, CommitList, HeadState, LockState, MergeStatus, Observed, ParentRepo, PathList,
    Reachability, Submodule, Tri, WorktreeKind, WorktreeState, classify_worktree,
    worktree_candidate,
};
use std::path::Path;

const OID: &str = "0123456789abcdef0123456789abcdef01234567";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Changes {
    Clean,
    Dirty,
    Unknown,
}
const CHANGES: [Changes; 3] = [Changes::Clean, Changes::Dirty, Changes::Unknown];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subs {
    Clean,
    Dirty,
    OneUnknown,
    ListUnknown,
}
const SUBS: [Subs; 4] = [
    Subs::Clean,
    Subs::Dirty,
    Subs::OneUnknown,
    Subs::ListUnknown,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ignored {
    None,
    ArtifactOnly,
    Unique,
    Truncated,
    Unknown,
}
const IGNORED: [Ignored; 5] = [
    Ignored::None,
    Ignored::ArtifactOnly,
    Ignored::Unique,
    Ignored::Truncated,
    Ignored::Unknown,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    Branch,
    Unborn,
    Detached,
    Unknown,
}
const HEADS: [Head; 4] = [Head::Branch, Head::Unborn, Head::Detached, Head::Unknown];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Reachable,
    Unreachable,
    Unknown,
}
const REACH: [Reach; 3] = [Reach::Reachable, Reach::Unreachable, Reach::Unknown];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unpushed {
    Zero,
    Some,
    Unknown,
}
const UNPUSHED: [Unpushed; 3] = [Unpushed::Zero, Unpushed::Some, Unpushed::Unknown];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Merge {
    Ancestor,
    NoCommits,
    Pushed,
    PatchId,
    NotMerged,
    Unknown,
}
const MERGES: [Merge; 6] = [
    Merge::Ancestor,
    Merge::NoCommits,
    Merge::Pushed,
    Merge::PatchId,
    Merge::NotMerged,
    Merge::Unknown,
];

/// One point of the state space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Point {
    missing: bool,
    locked: bool,
    tracked: Changes,
    untracked: Changes,
    subs: Subs,
    ignored: Ignored,
    head: Head,
    reach: Reach,
    unpushed: Unpushed,
    merge: Merge,
    in_use: Tri,
}

/// Mixed-radix dimensions, in index order (most significant first).
const RADIX: [usize; 11] = [2, 2, 3, 3, 4, 5, 4, 3, 3, 6, 3];
const TOTAL: usize = 2 * 2 * 3 * 3 * 4 * 5 * 4 * 3 * 3 * 6 * 3;

fn decode(mut i: usize) -> [usize; 11] {
    let mut d = [0; 11];
    for k in (0..11).rev() {
        d[k] = i % RADIX[k];
        i /= RADIX[k];
    }
    d
}

fn encode(d: &[usize; 11]) -> usize {
    d.iter().zip(RADIX).fold(0, |acc, (&v, r)| acc * r + v)
}

fn point(d: &[usize; 11]) -> Point {
    Point {
        missing: d[0] == 1,
        locked: d[1] == 1,
        tracked: CHANGES[d[2]],
        untracked: CHANGES[d[3]],
        subs: SUBS[d[4]],
        ignored: IGNORED[d[5]],
        head: HEADS[d[6]],
        reach: REACH[d[7]],
        unpushed: UNPUSHED[d[8]],
        merge: MERGES[d[9]],
        in_use: Tri::ALL[d[10]],
    }
}

fn list(paths: &[&str]) -> PathList {
    PathList {
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        total: paths.len() as u64,
        truncated: false,
    }
}

fn unknown<T>() -> Observed<T> {
    Observed::unknown("simulated git failure")
}

fn state(p: &Point, kind: WorktreeKind) -> WorktreeState {
    let changes = |c: Changes, path: &str| match c {
        Changes::Clean => Observed::known(list(&[])),
        Changes::Dirty => Observed::known(list(&[path])),
        Changes::Unknown => unknown(),
    };
    let sub = |dirty: Observed<bool>| Submodule {
        path: "sub".into(),
        initialized: true,
        dirty,
    };
    WorktreeState {
        repo: ParentRepo {
            common_dir: FsPath::from("/r/repo/.git"),
            bare: false,
            main_worktree: Some(FsPath::from("/r/repo")),
        },
        kind,
        name: Some("wt".into()),
        path: FsPath::from("/r/wt"),
        git_dir: FsPath::from("/r/repo/.git/worktrees/wt"),
        missing: p.missing,
        head: match p.head {
            Head::Branch => Observed::known(HeadState::Branch {
                name: "refs/heads/feature".into(),
                oid: Some(OID.into()),
            }),
            Head::Unborn => Observed::known(HeadState::Branch {
                name: "refs/heads/feature".into(),
                oid: None,
            }),
            Head::Detached => Observed::known(HeadState::Detached { oid: OID.into() }),
            Head::Unknown => unknown(),
        },
        upstream: Observed::known(None),
        lock: if p.locked {
            LockState::Locked {
                reason: Some("agent busy".into()),
            }
        } else {
            LockState::Unlocked
        },
        activity: unknown(),
        tracked_changes: changes(p.tracked, "src/lib.rs"),
        untracked: changes(p.untracked, "notes.txt"),
        ignored: match p.ignored {
            Ignored::None => Observed::known(list(&[])),
            Ignored::ArtifactOnly => Observed::known(list(&["target/"])),
            Ignored::Unique => Observed::known(list(&["target/", ".env", "dev.sqlite"])),
            Ignored::Truncated => Observed::known(PathList {
                paths: vec!["target/".into()],
                total: 7,
                truncated: true,
            }),
            Ignored::Unknown => unknown(),
        },
        submodules: match p.subs {
            Subs::Clean => Observed::known(vec![sub(Observed::known(false))]),
            Subs::Dirty => Observed::known(vec![sub(Observed::known(true))]),
            Subs::OneUnknown => Observed::known(vec![sub(unknown())]),
            Subs::ListUnknown => unknown(),
        },
        unpushed: match p.unpushed {
            Unpushed::Zero => Observed::known(CommitList::default()),
            Unpushed::Some => Observed::known(CommitList {
                count: 2,
                oids: vec![OID.into()],
                truncated: false,
            }),
            Unpushed::Unknown => unknown(),
        },
        merge: match p.merge {
            Merge::Ancestor => MergeStatus::AncestorOfDefault {
                default_ref: "refs/remotes/origin/main".into(),
            },
            Merge::NoCommits => MergeStatus::NoCommits,
            Merge::Pushed => MergeStatus::ReachableFromPushedRef {
                refs: vec!["refs/remotes/origin/feature".into()],
            },
            Merge::PatchId => MergeStatus::PatchIdOnly {
                default_ref: "refs/remotes/origin/main".into(),
                commits: 2,
            },
            Merge::NotMerged => MergeStatus::NotMerged {
                checked: vec!["refs/remotes/origin/main".into()],
            },
            Merge::Unknown => MergeStatus::Unknown {
                reason: "simulated".into(),
            },
        },
        head_reachability: match p.reach {
            Reach::Reachable => Observed::known(Reachability::ReachableFrom {
                refs: vec!["refs/heads/x".into()],
                total: 1,
            }),
            Reach::Unreachable => Observed::known(Reachability::Unreachable),
            Reach::Unknown => unknown(),
        },
    }
}

/// `target/` is the only ignored entry an artifact rule recognises.
fn matcher(_: &Path, rel: &str) -> Option<String> {
    (rel == "target/").then(|| "rust.target".to_owned())
}

/// The PRD GIT-03 table, written independently of the engine.
fn reference(p: &Point) -> Tier {
    if p.missing {
        return if p.locked || p.in_use == Tri::Yes {
            Tier::S3
        } else {
            Tier::S0
        };
    }
    let mut rows = vec![];
    if p.locked {
        rows.push(Tier::S3);
    }
    if p.tracked != Changes::Clean || p.untracked != Changes::Clean || p.subs != Subs::Clean {
        rows.push(Tier::S3);
    }
    if matches!(
        p.ignored,
        Ignored::Unique | Ignored::Truncated | Ignored::Unknown
    ) {
        rows.push(Tier::S2);
    }
    if p.head == Head::Unknown {
        rows.push(Tier::S3);
    } else {
        if p.head == Head::Detached && p.reach != Reach::Reachable {
            rows.push(Tier::S3);
        }
        let proven =
            matches!(p.merge, Merge::Ancestor | Merge::NoCommits) || p.unpushed == Unpushed::Zero;
        if !proven {
            rows.push(match (p.unpushed, p.merge) {
                (Unpushed::Some, Merge::PatchId) => Tier::S2,
                _ => Tier::S3,
            });
        }
    }
    match p.in_use {
        Tri::Yes => rows.push(Tier::S3),
        Tri::Unknown => rows.push(Tier::S2),
        Tri::No => {}
    }
    rows.into_iter().max().unwrap_or(Tier::S1).max(Tier::S1)
}

fn has(reasons: &[scull_core::TierReason], code: ReasonCode, min: Tier) -> bool {
    reasons.iter().any(|r| r.code == code && r.tier >= min)
}

#[test]
fn every_state_matches_the_table() {
    let mut tiers = vec![Tier::S0; TOTAL];
    let mut seen_tiers = [0usize; 4];
    for (i, slot) in tiers.iter_mut().enumerate() {
        let d = decode(i);
        assert_eq!(encode(&d), i);
        let p = point(&d);
        let st = state(&p, WorktreeKind::Linked);
        let dec = classify_worktree(&st, p.in_use, &matcher);
        let ctx = || format!("{p:?}\n{dec:#?}");
        *slot = dec.tier;
        seen_tiers[dec.tier as usize] += 1;

        // 1. Reference model.
        assert_eq!(dec.tier, reference(&p), "{}", ctx());
        assert_eq!(
            dec.tier,
            scull_core::Candidate::tier_from_reasons(&dec.reasons),
            "{}",
            ctx()
        );

        // 3. Never S0 except a stale registration.
        if dec.tier == Tier::S0 {
            assert!(p.missing, "{}", ctx());
            assert_eq!(dec.reasons.len(), 1, "{}", ctx());
            assert_eq!(dec.reasons[0].code, ReasonCode::StaleWorktreeRegistration);
        }
        if !p.missing {
            assert!(dec.tier >= Tier::S1, "{}", ctx());
            assert!(
                !dec.reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::StaleWorktreeRegistration),
                "{}",
                ctx()
            );
        }

        // 2. Rows.
        if p.locked {
            assert!(
                has(&dec.reasons, ReasonCode::WorktreeLocked, Tier::S3),
                "{}",
                ctx()
            );
            assert!(
                dec.reasons.iter().any(|r| r.message.contains("agent busy")),
                "lock reason shown: {}",
                ctx()
            );
        }
        if p.in_use == Tri::Yes {
            assert!(has(&dec.reasons, ReasonCode::InUse, Tier::S3), "{}", ctx());
        }
        if p.missing {
            assert!(
                has(
                    &dec.reasons,
                    ReasonCode::StaleWorktreeRegistration,
                    Tier::S0
                ),
                "{}",
                ctx()
            );
        } else {
            let dirty = p.tracked == Changes::Dirty
                || p.untracked == Changes::Dirty
                || p.subs == Subs::Dirty;
            if dirty {
                assert!(
                    has(&dec.reasons, ReasonCode::WorktreeDirty, Tier::S3),
                    "{}",
                    ctx()
                );
            }
            if p.ignored == Ignored::Unique {
                assert!(dec.tier >= Tier::S2, "{}", ctx());
                assert_eq!(dec.unique_ignored, vec![".env", "dev.sqlite"], "{}", ctx());
                let r = dec
                    .reasons
                    .iter()
                    .find(|r| r.code == ReasonCode::WorktreeUniqueIgnored)
                    .unwrap_or_else(|| panic!("{}", ctx()));
                assert!(r.message.contains(".env") && r.message.contains("dev.sqlite"));
                assert!(
                    !r.message.contains("target/"),
                    "artifacts are not listed as unique"
                );
            }
            if matches!(p.ignored, Ignored::ArtifactOnly | Ignored::Unique) {
                assert_eq!(dec.artifacts.len(), 1, "{}", ctx());
                assert_eq!(dec.artifacts[0].rule, "rust.target");
            }
            if p.ignored == Ignored::ArtifactOnly {
                assert!(
                    !has(&dec.reasons, ReasonCode::WorktreeUniqueIgnored, Tier::S0),
                    "{}",
                    ctx()
                );
            }
            if p.head != Head::Unknown {
                if p.head == Head::Detached && p.reach == Reach::Unreachable {
                    assert!(
                        has(&dec.reasons, ReasonCode::OrphanHead, Tier::S3),
                        "{}",
                        ctx()
                    );
                }
                let proven = matches!(p.merge, Merge::Ancestor | Merge::NoCommits)
                    || p.unpushed == Unpushed::Zero;
                if !proven && p.unpushed == Unpushed::Some {
                    if p.merge == Merge::PatchId {
                        assert!(
                            has(&dec.reasons, ReasonCode::MergeUnproven, Tier::S2),
                            "{}",
                            ctx()
                        );
                    } else {
                        assert!(
                            has(&dec.reasons, ReasonCode::UnpushedCommits, Tier::S3),
                            "{}",
                            ctx()
                        );
                    }
                }
                if proven {
                    assert!(
                        !dec.reasons.iter().any(|r| matches!(
                            r.code,
                            ReasonCode::UnpushedCommits | ReasonCode::MergeUnproven
                        )),
                        "{}",
                        ctx()
                    );
                }
            }
            // Clean row: S1 exactly when nothing else applies.
            if dec.tier == Tier::S1 {
                assert!(
                    has(&dec.reasons, ReasonCode::WorktreeClean, Tier::S1),
                    "{}",
                    ctx()
                );
                assert_eq!(dec.reasons.len(), 1, "{}", ctx());
            } else {
                assert!(
                    !dec.reasons
                        .iter()
                        .any(|r| r.code == ReasonCode::WorktreeClean),
                    "{}",
                    ctx()
                );
            }
        }

        // 5. Unknowns are disclosed and never S1.
        let any_unknown = !p.missing
            && (p.tracked == Changes::Unknown
                || p.untracked == Changes::Unknown
                || matches!(p.subs, Subs::OneUnknown | Subs::ListUnknown)
                || p.ignored == Ignored::Unknown
                || p.head == Head::Unknown
                || (p.head == Head::Detached && p.reach == Reach::Unknown)
                || p.in_use == Tri::Unknown);
        if any_unknown {
            assert!(dec.tier >= Tier::S2, "{}", ctx());
            assert!(
                dec.reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::SignalUnavailable),
                "{}",
                ctx()
            );
        }

        // 6. Columns and candidate.
        let expected_action = match (dec.tier, p.missing) {
            (Tier::S3, _) => Action::None,
            (Tier::S2, _) => Action::Quarantine,
            (_, true) => Action::GitWorktreePrune,
            (_, false) => Action::GitWorktreeRemove,
        };
        assert_eq!(dec.action, expected_action, "{}", ctx());
        // PRD §8.2 / D-3: S2 is quarantine only; irreversible tool/Git actions and Delete only
        // at S0/S1 (Delete never proposed here at all).
        if dec.tier == Tier::S2 {
            assert!(
                matches!(dec.action, Action::Quarantine | Action::None),
                "{}",
                ctx()
            );
        }
        if matches!(
            dec.action,
            Action::GitWorktreeRemove | Action::GitWorktreePrune | Action::ToolCommand { .. }
        ) {
            assert!(dec.tier <= Tier::S1, "{}", ctx());
        }
        assert_ne!(dec.action, Action::Delete, "{}", ctx());
        if dec.action.is_mutating() {
            assert!(
                scull_core::action_allowed_for(dec.tier, &dec.action),
                "{}",
                ctx()
            );
        }
        let expected_cleanup = if p.missing {
            ArtifactCleanup::NotApplicable
        } else if p.in_use == Tri::Yes {
            ArtifactCleanup::BlockedForThisRun
        } else {
            ArtifactCleanup::Allowed
        };
        assert_eq!(dec.artifact_cleanup, expected_cleanup, "{}", ctx());
        let c = worktree_candidate(
            &st,
            &dec,
            p.in_use,
            FileIdentity::new(1, i as u128),
            Sizes::default(),
        );
        assert!(c.is_consistent(), "{}", ctx());
        assert_eq!(c.category, scull_core::Category::Worktree);
        assert_eq!(c.tier, dec.tier);
        assert_eq!(c.flags.active, p.in_use.as_option());
    }

    // Every tier is reachable.
    for (t, n) in seen_tiers.iter().enumerate() {
        assert!(*n > 0, "tier S{t} never produced");
    }

    // 4. Monotonicity over every dimension (missing is a different kind of item, skipped).
    // Pairs (better, worse) per dimension, by value index.
    let worse: [&[(usize, usize)]; 11] = [
        &[],                       // missing
        &[(0, 1)],                 // locked
        &[(0, 1), (0, 2)],         // tracked
        &[(0, 1), (0, 2)],         // untracked
        &[(0, 1), (0, 2), (0, 3)], // submodules
        &[
            (0, 1),
            (1, 0),
            (0, 2),
            (0, 3),
            (0, 4),
            (1, 2),
            (1, 3),
            (1, 4),
        ], // ignored
        &[(0, 3), (1, 3), (2, 3)], // head
        &[(0, 1), (0, 2)],         // reachability
        &[(0, 1), (0, 2), (1, 2)], // unpushed
        // merge: proven (ancestor, no commits) <= patch-id <= not merged / unknown / pushed-only
        &[
            (0, 3),
            (1, 3),
            (0, 1),
            (1, 0),
            (3, 4),
            (3, 5),
            (3, 2),
            (0, 2),
            (0, 4),
            (0, 5),
        ],
        &[(0, 2), (2, 1), (0, 1)], // in use: no <= unknown <= yes
    ];
    let mut checked = 0usize;
    for i in 0..TOTAL {
        let d = decode(i);
        for (dim, pairs) in worse.iter().enumerate() {
            for &(better, worse) in pairs.iter() {
                if d[dim] != better {
                    continue;
                }
                let mut e = d;
                e[dim] = worse;
                let j = encode(&e);
                assert!(
                    tiers[j] >= tiers[i],
                    "monotonicity: {:?} ({}) -> {:?} ({})",
                    point(&d),
                    tiers[i],
                    point(&e),
                    tiers[j]
                );
                checked += 1;
            }
        }
    }
    assert!(checked > TOTAL, "monotonicity pairs checked: {checked}");
}

#[test]
fn main_and_bare_entries_are_never_removable() {
    for kind in [WorktreeKind::Main, WorktreeKind::Bare] {
        for in_use in Tri::ALL {
            let p = Point {
                missing: false,
                locked: false,
                tracked: Changes::Clean,
                untracked: Changes::Clean,
                subs: Subs::Clean,
                ignored: Ignored::None,
                head: Head::Branch,
                reach: Reach::Reachable,
                unpushed: Unpushed::Zero,
                merge: Merge::Ancestor,
                in_use,
            };
            let dec = classify_worktree(&state(&p, kind), in_use, &matcher);
            assert_eq!(dec.tier, Tier::S3, "{kind:?}");
            assert_eq!(dec.action, Action::None);
            assert!(has(&dec.reasons, ReasonCode::ProtectedRoot, Tier::S3));
        }
    }
}

#[test]
fn every_present_worktree_is_s1_at_best_even_when_old_and_clean() {
    // GIT-04: age and a clean status never make a worktree S0.
    let p = Point {
        missing: false,
        locked: false,
        tracked: Changes::Clean,
        untracked: Changes::Clean,
        subs: Subs::Clean,
        ignored: Ignored::None,
        head: Head::Branch,
        reach: Reach::Reachable,
        unpushed: Unpushed::Zero,
        merge: Merge::Ancestor,
        in_use: Tri::No,
    };
    let mut st = state(&p, WorktreeKind::Linked);
    st.activity = Observed::known(scull_git::Activity {
        newest_unix: Some(0),
        newest_path: None,
        entries_sampled: 1,
        complete: true,
    });
    let dec = classify_worktree(&st, Tri::No, &matcher);
    assert_eq!(dec.tier, Tier::S1);
    assert_eq!(dec.action, Action::GitWorktreeRemove);
    assert!(
        dec.evidence
            .iter()
            .any(|e| matches!(e, scull_core::Evidence::LastModified { newest_unix: 0, .. }))
    );
}

#[test]
fn decision_serializes() {
    let p = Point {
        missing: true,
        locked: false,
        tracked: Changes::Unknown,
        untracked: Changes::Unknown,
        subs: Subs::ListUnknown,
        ignored: Ignored::Unknown,
        head: Head::Unknown,
        reach: Reach::Unknown,
        unpushed: Unpushed::Unknown,
        merge: Merge::Unknown,
        in_use: Tri::Unknown,
    };
    let dec = classify_worktree(&state(&p, WorktreeKind::Linked), Tri::Unknown, &matcher);
    assert_eq!(dec.tier, Tier::S0);
    let v = serde_json::to_value(&dec).expect("json");
    assert_eq!(v["action"]["type"], "git_worktree_prune");
    assert_eq!(v["artifact_cleanup"], "not_applicable");
}
