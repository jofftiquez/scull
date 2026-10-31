//! Serde round-trip tests for every public core type (P1-CORE-01).

use proptest::prelude::*;
use scull_core::*;
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;

fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + Debug>(v: &T) -> String {
    let json = serde_json::to_string(v).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&back, v, "round-trip changed value; json = {json}");
    json
}

fn sample_candidate() -> Candidate {
    let identity = FileIdentity::new(0x1000004, 0x00ab_cdef);
    let path = "/home/dev/Projects/api/target";
    let reasons = vec![TierReason::new(
        Tier::S0,
        ReasonCode::Regenerable,
        "Cargo.toml + Cargo.lock present; rebuild with cargo build",
    )];
    Candidate {
        id: ItemId::derive(identity, path.as_bytes()),
        path: FsPath::from(path),
        identity,
        category: Category::ProjectArtifact,
        artifact: "Cargo build output".into(),
        rule: RuleRef {
            id: "rust.target".into(),
            version: 1,
        },
        project: Some(ProjectRef {
            root: "/home/dev/Projects/api".into(),
            name: "api".into(),
            ecosystem: "rust".into(),
        }),
        tier: Candidate::tier_from_reasons(&reasons),
        reasons,
        evidence: vec![
            Evidence::Marker {
                path: "/home/dev/Projects/api/Cargo.toml".into(),
            },
            Evidence::Lockfile {
                path: "/home/dev/Projects/api/Cargo.lock".into(),
            },
            Evidence::LastModified {
                newest_unix: 1_761_800_000,
                sampled: false,
            },
            Evidence::SignalUnavailable {
                signal: "open files".into(),
                reason: "lsof not found".into(),
            },
        ],
        sizes: Sizes {
            logical: 4_200_000_000,
            allocated: 3_900_000_000,
            reclaimable: Reclaimable {
                bytes: 3_900_000_000,
                confidence: Confidence::High,
                shared_bytes: 0,
            },
        },
        flags: Flags {
            active: Some(false),
            hardlinked: Some(false),
            cross_mount: Some(false),
            symlink: Some(false),
            cloud_placeholder: None,
        },
        regeneration: Regeneration {
            instructions: Some("cargo build".into()),
            cost: RegenerationCost::Estimate {
                seconds: 240,
                network: false,
            },
        },
        action: Action::Delete,
    }
}

#[test]
fn candidate_round_trips() {
    let c = sample_candidate();
    assert!(c.is_consistent());
    let json = roundtrip(&c);
    assert!(json.contains(r#""tier":"S0""#));
    assert!(json.contains(r#""kind":"marker""#));
    assert!(json.contains(r#""type":"delete""#));
}

#[test]
fn every_action_round_trips() {
    for a in [
        Action::None,
        Action::Delete,
        Action::Quarantine,
        Action::Trash,
        Action::ToolCommand {
            tool: "pnpm".into(),
            args: vec!["store".into(), "prune".into()],
        },
        Action::GitWorktreeRemove,
        Action::GitWorktreePrune,
    ] {
        roundtrip(&a);
    }
}

#[test]
fn every_evidence_round_trips() {
    let all = vec![
        Evidence::Marker { path: "a".into() },
        Evidence::Lockfile { path: "a".into() },
        Evidence::KnownArtifact {
            artifact: "x".into(),
        },
        Evidence::GitTracked { files: 3 },
        Evidence::ProtectedRoot { root: "~".into() },
        Evidence::UserExclusion { rule: "x".into() },
        Evidence::ProtectMarker { path: "a".into() },
        Evidence::InUse {
            signal: "process cwd".into(),
            by: Some("node".into()),
        },
        Evidence::InUse {
            signal: "open handle".into(),
            by: None,
        },
        Evidence::LastModified {
            newest_unix: -5,
            sampled: true,
        },
        Evidence::CrossesMount,
        Evidence::LinkEscape {
            target: "/etc".into(),
        },
        Evidence::CloudSync {
            provider: "OneDrive".into(),
        },
        Evidence::Hardlinked { shared_bytes: 9 },
        Evidence::SignalUnavailable {
            signal: "s".into(),
            reason: "r".into(),
        },
        Evidence::Note { text: "n".into() },
    ];
    for e in &all {
        roundtrip(e);
    }
}

#[test]
fn ids_round_trip_and_reject_tampering() {
    roundtrip(&PlanId::from_suffix("01HZXK").unwrap());
    roundtrip(&ScanId::from_suffix("abc").unwrap());
    roundtrip(&OperationId::from_suffix("Z9").unwrap());
    assert!(serde_json::from_str::<PlanId>(r#""op_123""#).is_err());
    assert!(serde_json::from_str::<ItemId>(r#""../../etc/passwd""#).is_err());
}

#[test]
fn unknown_fields_are_rejected() {
    // Plans and outputs are untrusted input when read back; extra fields are refused.
    let json =
        r#"{"logical":1,"allocated":1,"reclaimable":{"bytes":1,"confidence":"high"},"evil":1}"#;
    assert!(serde_json::from_str::<Sizes>(json).is_err());
}

#[test]
fn tier_breakdown_round_trips() {
    let mut b = TierBreakdown::default();
    b.add(Tier::S1, 7);
    b.add(Tier::S3, 1);
    roundtrip(&b);
}

fn arb_tier() -> impl Strategy<Value = Tier> {
    prop_oneof![
        Just(Tier::S0),
        Just(Tier::S1),
        Just(Tier::S2),
        Just(Tier::S3)
    ]
}

proptest! {
    #[test]
    fn file_identity_round_trips(volume in any::<u64>(), file in any::<u128>()) {
        roundtrip(&FileIdentity::new(volume, file));
    }

    #[test]
    fn item_id_is_always_valid_hex(volume in any::<u64>(), file in any::<u128>(), path in proptest::collection::vec(any::<u8>(), 0..64)) {
        let id = ItemId::derive(FileIdentity::new(volume, file), &path);
        roundtrip(&id);
    }

    #[test]
    fn raising_never_lowers_and_keeps_consistency(start in arb_tier(), raises in proptest::collection::vec(arb_tier(), 0..6)) {
        let mut c = sample_candidate();
        c.reasons = vec![TierReason::new(start, ReasonCode::Regenerable, "start")];
        c.tier = start;
        c.action = Action::default_for(start);
        prop_assert!(c.is_consistent());
        for r in raises {
            let before = c.tier;
            c.raise(TierReason::new(r, ReasonCode::RecentActivity, "raise"));
            prop_assert!(c.tier >= before);
            prop_assert!(c.is_consistent());
        }
        roundtrip(&c);
    }

    #[test]
    fn sizes_round_trip(l in any::<u64>(), a in any::<u64>(), r in any::<u64>(), s in any::<u64>()) {
        roundtrip(&Sizes { logical: l, allocated: a, reclaimable: Reclaimable { bytes: r, confidence: Confidence::Medium, shared_bytes: s } });
    }
}

#[test]
fn candidate_without_reasons_is_s3() {
    assert_eq!(Candidate::tier_from_reasons(&[]), Tier::S3);
    let mut c = sample_candidate();
    c.reasons.clear();
    assert!(
        !c.is_consistent(),
        "S0 with no reasons must be inconsistent"
    );
}

#[test]
fn s3_with_action_is_inconsistent() {
    let mut c = sample_candidate();
    c.raise(TierReason::new(Tier::S3, ReasonCode::GitTracked, "tracked"));
    assert_eq!(c.action, Action::None);
    c.action = Action::Quarantine;
    assert!(!c.is_consistent());
}
