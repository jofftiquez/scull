//! Published JSON Schemas (P0-ARCH-02, CLI-01, SAFE-11).
//!
//! The files under `schemas/v1/` are generated from the `scull-core` types. This test fails if
//! a committed schema drifts from the code, and validates every example payload under
//! `schemas/v1/examples/` (valid ones must pass, `*.invalid.json` must fail).
//!
//! Regenerate after changing a type:
//! `SCULL_UPDATE_SCHEMAS=1 cargo test -p scull-core --test schemas`
//! A breaking change needs a new major schema version directory (CLI-01).

use scull_core::*;
use std::path::{Path, PathBuf};

fn schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1")
}

/// (file stem, schema) for every published type.
fn published() -> Vec<(&'static str, serde_json::Value)> {
    fn with_id<T: schemars::JsonSchema>(stem: &str) -> serde_json::Value {
        let mut v = serde_json::to_value(schemars::schema_for!(T)).expect("schema to json");
        let obj = v.as_object_mut().expect("schema is an object");
        obj.insert(
            "$id".into(),
            format!("https://scull.dev/schemas/v1/{stem}.schema.json").into(),
        );
        v
    }
    vec![
        ("candidate", with_id::<Candidate>("candidate")),
        ("plan", with_id::<Plan>("plan")),
        ("error", with_id::<ScullError>("error")),
        ("tier-breakdown", with_id::<TierBreakdown>("tier-breakdown")),
        ("drop-reason", with_id::<DropReason>("drop-reason")),
        ("config", with_id::<config::ConfigLayer>("config")),
    ]
}

fn pretty(v: &serde_json::Value) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("pretty json");
    s.push('\n');
    s
}

fn sample_candidate() -> Candidate {
    let identity = FileIdentity::new(16_777_220, 0x2b_9c1f);
    let path = "/Users/dev/Projects/api/target";
    let reasons = vec![TierReason::new(
        Tier::S0,
        ReasonCode::Regenerable,
        "Cargo.toml and Cargo.lock present; rebuild with `cargo build`",
    )];
    Candidate {
        id: ItemId::derive(identity, path.as_bytes()),
        path: path.into(),
        identity,
        category: Category::ProjectArtifact,
        artifact: "Cargo build output".into(),
        rule: RuleRef {
            id: "rust.target".into(),
            version: 1,
        },
        project: Some(ProjectRef {
            root: "/Users/dev/Projects/api".into(),
            name: "api".into(),
            ecosystem: "rust".into(),
        }),
        tier: Tier::S0,
        reasons,
        evidence: vec![
            Evidence::Marker {
                path: "/Users/dev/Projects/api/Cargo.toml".into(),
            },
            Evidence::Lockfile {
                path: "/Users/dev/Projects/api/Cargo.lock".into(),
            },
            Evidence::LastModified {
                newest_unix: 1_761_600_000,
                sampled: false,
            },
        ],
        sizes: Sizes {
            logical: 4_400_000_000,
            allocated: 4_100_000_000,
            reclaimable: Reclaimable {
                bytes: 4_100_000_000,
                confidence: Confidence::High,
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
        regeneration: Regeneration {
            instructions: Some("cargo build".into()),
            cost: RegenerationCost::Estimate {
                seconds: 180,
                network: false,
            },
        },
        action: Action::Delete,
    }
}

fn sample_user_config() -> config::ConfigLayer {
    config::ConfigLayer::from_toml(
        r#"
[scan]
roots = ["~/Projects", "~/work"]
exclude = ["~/Projects/huge-monorepo"]
protect = ["~/Projects/thesis"]
activity_window = "48h"
threads = 0
low_resource = false

[scan.min_tier]
"~/Projects/legacy" = "S2"

[mcp]
enabled = true
redact_home = true
max_results = 500

[output]
color = "auto"
"#,
    )
    .expect("sample config")
}

fn examples() -> Vec<(&'static str, serde_json::Value)> {
    let c = sample_candidate();
    let plan = Plan::new(
        PlanParams {
            id: PlanId::from_suffix("01JBX7Q2M4").expect("plan id"),
            scan_id: ScanId::from_suffix("01JBX7PZ9K").expect("scan id"),
            created_unix: 1_761_900_000,
            ttl_secs: None,
            machine_id: "c0ffee00c0ffee00".into(),
            user: "dev".into(),
            origin: PlanOrigin::Cli,
        },
        vec![PlanItem::from_candidate(&c).expect("plan item")],
    )
    .expect("plan");
    let err = ScullError::new(ErrorKind::PlanExpired, "plan expired 2 hours ago")
        .with_path("/Users/dev/plan.json".into());
    let mut tb = TierBreakdown::default();
    tb.add(Tier::S0, 9_000);
    tb.add(Tier::S2, 120);
    vec![
        ("candidate", serde_json::to_value(&c).expect("json")),
        ("plan", serde_json::to_value(&plan).expect("json")),
        ("error", serde_json::to_value(&err).expect("json")),
        ("tier-breakdown", serde_json::to_value(tb).expect("json")),
        (
            "config",
            serde_json::to_value(sample_user_config()).expect("json"),
        ),
    ]
}

#[test]
fn committed_schemas_match_the_code() {
    let dir = schemas_dir();
    let update = std::env::var_os("SCULL_UPDATE_SCHEMAS").is_some();
    let mut stale = Vec::new();
    for (stem, schema) in published() {
        let path = dir.join(format!("{stem}.schema.json"));
        let want = pretty(&schema);
        if update {
            std::fs::create_dir_all(&dir).expect("mkdir schemas");
            std::fs::write(&path, &want).expect("write schema");
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(have) if have.replace("\r\n", "\n") == want => {}
            _ => stale.push(path.display().to_string()),
        }
    }
    if update {
        let ex = dir.join("examples");
        std::fs::create_dir_all(&ex).expect("mkdir examples");
        for (stem, v) in examples() {
            std::fs::write(ex.join(format!("{stem}.json")), pretty(&v)).expect("write example");
        }
    }
    assert!(
        stale.is_empty(),
        "schemas out of date (run SCULL_UPDATE_SCHEMAS=1 cargo test -p scull-core --test schemas): {stale:?}"
    );
}

#[test]
fn schemas_are_draft_2020_12() {
    for (stem, schema) in published() {
        assert_eq!(
            schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{stem}"
        );
        jsonschema::draft202012::meta::validate(&schema)
            .unwrap_or_else(|e| panic!("{stem} is not a valid 2020-12 schema: {e}"));
    }
}

#[test]
fn example_payloads_validate() {
    let ex = schemas_dir().join("examples");
    let mut checked = 0;
    for (stem, schema) in published() {
        let validator = jsonschema::draft202012::new(&schema).expect("compile schema");
        for entry in std::fs::read_dir(&ex).expect("examples dir") {
            let path = entry.expect("entry").path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let invalid = name.ends_with(".invalid.json");
            let base = name
                .trim_end_matches(".invalid.json")
                .trim_end_matches(".json");
            let base = base.split('.').next().unwrap_or(base);
            if base != stem {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read example");
            let value: serde_json::Value = serde_json::from_str(&text).expect("example is JSON");
            let ok = validator.is_valid(&value);
            assert_eq!(ok, !invalid, "{name}: expected valid={}", !invalid);
            checked += 1;
        }
    }
    assert!(
        checked >= 7,
        "expected at least 7 example payloads, checked {checked}"
    );
}

#[test]
fn generated_examples_still_validate() {
    // Guards against examples that were committed once and then drifted from the types.
    for (stem, value) in examples() {
        let (_, schema) = published()
            .into_iter()
            .find(|(s, _)| *s == stem)
            .expect("schema for example");
        let validator = jsonschema::draft202012::new(&schema).expect("compile schema");
        let errors: Vec<String> = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{stem}: {errors:?}");
    }
}
