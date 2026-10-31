//! Property tests for the plan model (P1-CORE-02, SAFE-11, T-30).
//!
//! 1. Narrowing never grows a plan.
//! 2. Narrowing never yields an S3 item.
//! 3. Narrowing never yields an item whose fresh tier is more protective than planned, and never
//!    escalates the action.
//! 4. Any single-field edit of a serialized plan is rejected (by deserialization or `verify`).
//! 5. The content hash does not depend on item order.

use proptest::prelude::*;
use scull_core::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MACHINE: &str = "machine-a";
const USER: &str = "dev";

fn arb_actionable_tier() -> impl Strategy<Value = Tier> {
    prop_oneof![Just(Tier::S0), Just(Tier::S1), Just(Tier::S2)]
}

fn arb_tier() -> impl Strategy<Value = Tier> {
    prop_oneof![
        Just(Tier::S0),
        Just(Tier::S1),
        Just(Tier::S2),
        Just(Tier::S3)
    ]
}

fn arb_any_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        Just(Action::None),
        Just(Action::Delete),
        Just(Action::Quarantine),
        Just(Action::Trash),
        Just(Action::GitWorktreeRemove),
        Just(Action::GitWorktreePrune),
        prop_oneof![Just("npm"), Just("pnpm")].prop_map(|t| Action::ToolCommand {
            tool: t.into(),
            args: vec!["cache".into(), "clean".into()],
        }),
    ]
}

/// A tier with an action allowed for it (D-3).
fn arb_tier_and_action() -> impl Strategy<Value = (Tier, Action)> {
    (arb_actionable_tier(), arb_any_action()).prop_filter("action allowed for tier", |(t, a)| {
        action_allowed_for(*t, a)
    })
}

fn arb_category() -> impl Strategy<Value = Category> {
    prop_oneof![
        Just(Category::ProjectArtifact),
        Just(Category::PackageCache),
        Just(Category::Worktree),
        Just(Category::Other),
    ]
}

/// Plan items with distinct IDs (the index is part of the path).
fn arb_items() -> impl Strategy<Value = Vec<PlanItem>> {
    prop::collection::vec(
        (
            any::<u64>(),
            any::<u128>(),
            "[a-z_]{1,8}",
            arb_tier_and_action(),
            any::<u64>(),
            arb_category(),
        ),
        0..8,
    )
    .prop_map(|specs| {
        specs
            .into_iter()
            .enumerate()
            .map(
                |(n, (volume, file, name, (tier, action), bytes, category))| {
                    let identity = FileIdentity::new(volume, file);
                    let path = format!("/work/{n}/{name}");
                    PlanItem {
                        item_id: ItemId::derive(identity, path.as_bytes()),
                        identity,
                        path: FsPath::from(path.as_str()),
                        category,
                        rule: RuleRef {
                            id: "rust.target".into(),
                            version: 1,
                        },
                        tier,
                        action,
                        reclaimable_bytes: bytes,
                    }
                },
            )
            .collect()
    })
}

fn arb_origin() -> impl Strategy<Value = PlanOrigin> {
    prop_oneof![
        Just(PlanOrigin::Cli),
        Just(PlanOrigin::Mcp),
        Just(PlanOrigin::Desktop)
    ]
}

fn params(created_unix: i64, origin: PlanOrigin) -> PlanParams {
    PlanParams {
        id: PlanId::from_suffix("01PROP").expect("valid id"),
        scan_id: ScanId::from_suffix("01SCAN").expect("valid id"),
        created_unix,
        ttl_secs: None,
        machine_id: MACHINE.into(),
        user: USER.into(),
        origin,
    }
}

fn arb_plan() -> impl Strategy<Value = Plan> {
    (arb_items(), 0i64..4_000_000_000, arb_origin()).prop_map(|(items, created, origin)| {
        Plan::new(params(created, origin), items).expect("valid plan")
    })
}

/// What happened to a plan item between planning and execution.
#[derive(Debug, Clone)]
enum Fate {
    /// Unchanged.
    Same,
    /// Not found by the fresh scan.
    Missing,
    /// Found twice.
    Duplicated,
    /// Re-derived with arbitrary changes (identity, tier, reasons, action), possibly
    /// inconsistent.
    Changed {
        new_identity: Option<FileIdentity>,
        tier: Tier,
        reason_tier: Tier,
        action: Action,
    },
    /// Re-derived consistently at a new tier (action adjusted the way the rubric would).
    Retiered(Tier),
}

fn arb_fate() -> impl Strategy<Value = Fate> {
    prop_oneof![
        3 => Just(Fate::Same),
        1 => Just(Fate::Missing),
        1 => Just(Fate::Duplicated),
        3 => (
            prop::option::weighted(0.3, (any::<u64>(), any::<u128>())),
            arb_tier(),
            arb_tier(),
            arb_any_action()
        )
            .prop_map(|(id, tier, reason_tier, action)| Fate::Changed {
                new_identity: id.map(|(v, f)| FileIdentity::new(v, f)),
                tier,
                reason_tier,
                action,
            }),
        2 => arb_tier().prop_map(Fate::Retiered),
    ]
}

fn candidate_from(item: &PlanItem, tier: Tier, reason_tier: Tier, action: Action) -> Candidate {
    Candidate {
        id: item.item_id.clone(),
        path: item.path.clone(),
        identity: item.identity,
        category: item.category,
        artifact: "artifact".into(),
        rule: item.rule.clone(),
        project: None,
        tier,
        reasons: vec![TierReason::new(
            reason_tier,
            ReasonCode::Regenerable,
            "prop",
        )],
        evidence: vec![],
        sizes: Sizes::default(),
        flags: Flags::default(),
        regeneration: Regeneration::default(),
        action,
    }
}

fn fresh_for(plan: &Plan, fates: &[Fate], extras: &[PlanItem]) -> Vec<Candidate> {
    let mut fresh = Vec::new();
    for (item, fate) in plan.items().iter().zip(fates.iter().cycle()) {
        match fate {
            Fate::Same => fresh.push(candidate_from(
                item,
                item.tier,
                item.tier,
                item.action.clone(),
            )),
            Fate::Missing => {}
            Fate::Duplicated => {
                let c = candidate_from(item, item.tier, item.tier, item.action.clone());
                fresh.push(c.clone());
                fresh.push(c);
            }
            Fate::Changed {
                new_identity,
                tier,
                reason_tier,
                action,
            } => {
                let mut c = candidate_from(item, *tier, *reason_tier, action.clone());
                if let Some(id) = new_identity {
                    c.identity = *id;
                }
                fresh.push(c);
            }
            Fate::Retiered(t) => {
                let mut c = candidate_from(item, item.tier, item.tier, item.action.clone());
                c.raise(TierReason::new(*t, ReasonCode::RecentActivity, "prop"));
                fresh.push(c);
            }
        }
    }
    // Candidates that were never planned: must never be added.
    for e in extras {
        fresh.push(candidate_from(e, e.tier, e.tier, e.action.clone()));
    }
    fresh
}

fn fs_rank(a: &Action) -> Option<u8> {
    match a {
        Action::Quarantine => Some(0),
        Action::Trash => Some(1),
        Action::Delete => Some(2),
        _ => None,
    }
}

/// Every leaf of a JSON value, as a path of keys / indices.
#[derive(Debug, Clone)]
enum Seg {
    Key(String),
    Idx(usize),
}

fn leaves(v: &Value, prefix: &mut Vec<Seg>, out: &mut Vec<Vec<Seg>>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                prefix.push(Seg::Key(k.clone()));
                leaves(x, prefix, out);
                prefix.pop();
            }
        }
        Value::Array(a) if !a.is_empty() => {
            for (i, x) in a.iter().enumerate() {
                prefix.push(Seg::Idx(i));
                leaves(x, prefix, out);
                prefix.pop();
            }
        }
        _ => out.push(prefix.clone()),
    }
}

fn get_mut<'a>(v: &'a mut Value, path: &[Seg]) -> &'a mut Value {
    path.iter().fold(v, |v, s| match s {
        Seg::Key(k) => &mut v[k.as_str()],
        Seg::Idx(i) => &mut v[*i],
    })
}

/// How to mutate one leaf.
#[derive(Debug, Clone)]
enum Mutation {
    Append(String),
    Replace(String),
    Add(i64),
}

fn arb_mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        "[0-9a-fS/_x]{1,3}".prop_map(Mutation::Append),
        "[0-9a-zS/_]{0,12}".prop_map(Mutation::Replace),
        prop_oneof![-1000i64..-1, 1i64..1000, Just(i64::MIN), Just(i64::MAX)]
            .prop_map(Mutation::Add),
    ]
}

fn mutate_leaf(leaf: &mut Value, m: &Mutation) {
    let new = match (&*leaf, m) {
        (Value::String(s), Mutation::Append(x)) => Value::from(format!("{s}{x}")),
        (Value::String(_), Mutation::Replace(x)) => Value::from(x.as_str()),
        (Value::String(s), Mutation::Add(d)) => {
            // Change one character to a different one.
            let mut chars: Vec<char> = s.chars().collect();
            match chars.last_mut() {
                Some(c) => {
                    *c = if (*d).rem_euclid(2) == 0 {
                        if *c == '0' { '1' } else { '0' }
                    } else if *c == 'a' {
                        'b'
                    } else {
                        'a'
                    }
                }
                None => chars.push('a'),
            }
            Value::from(chars.into_iter().collect::<String>())
        }
        (Value::Number(n), m) => {
            let d = match m {
                Mutation::Add(d) => *d,
                Mutation::Append(s) | Mutation::Replace(s) => s.len() as i64 + 1,
            };
            if let Some(u) = n.as_u64() {
                Value::from(u.wrapping_add(d as u64))
            } else {
                Value::from(n.as_i64().unwrap_or(0).wrapping_add(d))
            }
        }
        (Value::Bool(b), _) => Value::Bool(!b),
        (_, _) => Value::from(1),
    };
    *leaf = new;
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// (1)–(3): narrowing only ever shrinks the plan, never yields S3, never yields an item whose
    /// fresh tier is above the planned one, and never escalates the action.
    #[test]
    fn narrowing_never_grows_or_escalates(
        plan in arb_plan(),
        fates in prop::collection::vec(arb_fate(), 1..8),
        extras in arb_items(),
    ) {
        let fresh = fresh_for(&plan, &fates, &extras);
        let result = narrow(&plan, &fresh);

        let planned: BTreeMap<&ItemId, &PlanItem> =
            plan.items().iter().map(|i| (&i.item_id, i)).collect();
        let mut fresh_by_id: BTreeMap<&ItemId, Vec<&Candidate>> = BTreeMap::new();
        for c in &fresh {
            fresh_by_id.entry(&c.id).or_default().push(c);
        }

        // (1) Never grows; executable ⊆ plan; every plan item accounted for exactly once.
        prop_assert!(result.executable.len() <= plan.items().len());
        let mut ids = BTreeSet::new();
        for e in &result.executable {
            let p = planned.get(&e.item_id);
            prop_assert!(p.is_some(), "executable item {} not in plan", e.item_id);
            let p = p.unwrap();
            prop_assert_eq!(e.identity, p.identity);
            prop_assert_eq!(&e.path, &p.path);
            prop_assert!(ids.insert(&e.item_id), "duplicate executable item");
        }
        prop_assert_eq!(result.executable.len() + result.dropped.len(), plan.items().len());
        for (id, _) in &result.dropped {
            prop_assert!(planned.contains_key(id));
            prop_assert!(!ids.contains(id));
        }

        for e in &result.executable {
            let p = planned[&e.item_id];
            let cands = &fresh_by_id[&e.item_id];
            prop_assert_eq!(cands.len(), 1);
            let c = cands[0];
            // (2) Never S3, and the action is allowed for the tier it runs at.
            prop_assert!(e.tier != Tier::S3);
            prop_assert!(c.tier != Tier::S3);
            prop_assert!(action_allowed_for(e.tier, &e.action));
            // (3) Fresh tier never more protective than planned.
            prop_assert!(c.tier <= p.tier, "fresh {} > planned {}", c.tier, p.tier);
            prop_assert_eq!(e.tier, c.tier);
            prop_assert!(c.is_consistent());
            prop_assert_eq!(c.identity, p.identity);
            // The action is the reviewed one or a safer filesystem action.
            if e.action != p.action {
                let (er, pr) = (fs_rank(&e.action), fs_rank(&p.action));
                prop_assert!(er.is_some() && pr.is_some() && er < pr,
                    "action escalated from {:?} to {:?}", p.action, e.action);
            }
        }

        // Unplanned fresh candidates are never added.
        for x in &extras {
            if !planned.contains_key(&x.item_id) {
                prop_assert!(result.executable.iter().all(|e| e.item_id != x.item_id));
            }
        }
    }

    /// Narrowing against an identical fresh scan keeps the whole plan unchanged.
    #[test]
    fn narrowing_against_identical_scan_keeps_everything(plan in arb_plan()) {
        let fresh = fresh_for(&plan, &[Fate::Same], &[]);
        let result = narrow(&plan, &fresh);
        prop_assert!(result.dropped.is_empty());
        let kept: Vec<_> = result
            .executable
            .iter()
            .map(|e| (&e.item_id, &e.action, e.tier))
            .collect();
        let want: Vec<_> = plan.items().iter().map(|e| (&e.item_id, &e.action, e.tier)).collect();
        prop_assert_eq!(kept, want);
    }

    /// (4): any single-leaf edit of a serialized plan is rejected, either when deserializing or
    /// by `verify` (as `plan_tampered`, before the binding and expiry checks).
    #[test]
    fn single_field_edits_are_detected(
        plan in arb_plan(),
        which in any::<prop::sample::Index>(),
        mutation in arb_mutation(),
    ) {
        let original = serde_json::to_value(&plan).unwrap();
        let mut paths = Vec::new();
        leaves(&original, &mut Vec::new(), &mut paths);
        let path = &paths[which.index(paths.len())];

        let mut edited = original.clone();
        mutate_leaf(get_mut(&mut edited, path), &mutation);
        prop_assume!(edited != original);

        if let Ok(p) = serde_json::from_value::<Plan>(edited) {
            // A different spelling of the same value (e.g. hex with a leading zero) is not an
            // edit of the plan's content.
            prop_assume!(p != plan);
            let now = plan.created_unix();
            let err = p.verify(now, MACHINE, USER).unwrap_err();
            prop_assert_eq!(err.kind, ErrorKind::PlanTampered, "path {:?}: {}", path, err);
            prop_assert_eq!(err.exit_code().as_i32(), 4);
        }
    }

    /// Removing or duplicating an item in the file is rejected.
    #[test]
    fn item_removal_or_duplication_is_detected(plan in arb_plan(), which in any::<prop::sample::Index>(), dup in any::<bool>()) {
        prop_assume!(!plan.items().is_empty());
        let mut v = serde_json::to_value(&plan).unwrap();
        let items = v["items"].as_array_mut().unwrap();
        let i = which.index(items.len());
        if dup {
            let x = items[i].clone();
            items.push(x);
        } else {
            items.remove(i);
        }
        let p: Plan = serde_json::from_value(v).unwrap();
        prop_assert_eq!(p.verify(plan.created_unix(), MACHINE, USER).unwrap_err().kind, ErrorKind::PlanTampered);
    }

    /// (5): the content hash is independent of item order, both in the builder and in the file.
    #[test]
    fn hash_is_stable_under_item_reordering(
        (items, shuffled) in arb_items().prop_flat_map(|items| (Just(items.clone()), Just(items).prop_shuffle())),
        created in 0i64..4_000_000_000,
        origin in arb_origin(),
    ) {
        let a = Plan::new(params(created, origin), items).unwrap();
        let b = Plan::new(params(created, origin), shuffled.clone()).unwrap();
        prop_assert_eq!(a.content_hash(), b.content_hash());
        prop_assert_eq!(a.canonical_json().unwrap(), b.canonical_json().unwrap());

        // Same plan file with its items written in another order still verifies.
        let mut v = serde_json::to_value(&a).unwrap();
        v["items"] = serde_json::to_value(&shuffled).unwrap();
        let reordered: Plan = serde_json::from_value(v).unwrap();
        prop_assert_eq!(reordered.compute_hash().unwrap(), a.content_hash().clone());
        prop_assert!(reordered.verify(created, MACHINE, USER).is_ok());
    }

    /// A valid plan verifies throughout its lifetime on its own machine, and is rejected as
    /// foreign elsewhere and as expired afterwards.
    #[test]
    fn verify_respects_window_and_binding(plan in arb_plan(), offset in 0i64..86_400, other in "[a-z]{1,8}") {
        let created = plan.created_unix();
        let ttl = plan.expires_unix() - created;
        let now = created + offset % ttl;
        prop_assert!(plan.verify(now, MACHINE, USER).is_ok());
        prop_assert!(plan.verify(created - MAX_CLOCK_SKEW_SECS, MACHINE, USER).is_ok());
        prop_assert_eq!(plan.verify(plan.expires_unix() + offset, MACHINE, USER).unwrap_err().kind, ErrorKind::PlanExpired);
        prop_assert_eq!(plan.verify(created - MAX_CLOCK_SKEW_SECS - 1 - offset, MACHINE, USER).unwrap_err().kind, ErrorKind::PlanExpired);
        let machine = format!("{MACHINE}{other}");
        prop_assert_eq!(plan.verify(now, &machine, USER).unwrap_err().kind, ErrorKind::PlanForeign);
        let user = format!("{USER}{other}");
        prop_assert_eq!(plan.verify(now, MACHINE, &user).unwrap_err().kind, ErrorKind::PlanForeign);
    }
}

/// P1-CORE-04 / D-28: an uncertain (S2) item may only be quarantined or trashed. A plan that
/// proposes an irreversible action for it is refused at creation.
#[test]
fn s2_items_with_irreversible_actions_are_refused() {
    let identity = FileIdentity::new(7, 42);
    let path = "/work/repo-wt";
    let item = |action: Action| PlanItem {
        item_id: ItemId::derive(identity, path.as_bytes()),
        identity,
        path: FsPath::from(path),
        category: Category::Worktree,
        rule: RuleRef {
            id: "git.worktree".into(),
            version: 1,
        },
        tier: Tier::S2,
        action,
        reclaimable_bytes: 1,
    };
    let irreversible = [
        Action::Delete,
        Action::GitWorktreeRemove,
        Action::GitWorktreePrune,
        Action::ToolCommand {
            tool: "npm".into(),
            args: vec!["cache".into(), "clean".into(), "--force".into()],
        },
    ];
    for action in irreversible {
        let err = Plan::new(params(1_000, PlanOrigin::Cli), vec![item(action.clone())])
            .expect_err("S2 + irreversible action must be refused");
        assert_eq!(err.kind, ErrorKind::SafetyBlocked, "{action:?}");
    }
    for action in [Action::Quarantine, Action::Trash] {
        Plan::new(params(1_000, PlanOrigin::Cli), vec![item(action)]).expect("restorable is fine");
    }
}
