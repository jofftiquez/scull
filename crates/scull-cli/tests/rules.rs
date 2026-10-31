//! `scull rules list | validate` end to end (P1-CLI-06). User rule files and configs are written
//! to temp dirs; `SCULL_HOME` points at a temp dir so the real config is never read.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A valid user rule (S1, filesystem, marker-backed).
const VALID: &str = r#"schema_version = 1

[[rule]]
id = "community.unity-library"
version = 1
artifact = "Unity Library cache"
ecosystem = "unity"
category = "project_artifact"
platforms = ["macos", "linux", "windows"]
base_tier = "S1"
strategy = "filesystem"
evidence = ["marker"]

[rule.match]
names = ["Library"]

[rule.markers]
all = ["../Assets", "../ProjectSettings"]
"#;

/// `VALID` with `from` replaced by `to` (exactly once).
fn variant(from: &str, to: &str) -> String {
    assert_eq!(VALID.matches(from).count(), 1, "{from}");
    VALID.replace(from, to)
}

fn scull_in(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args).env("SCULL_HOME", home).env("TERM", "dumb");
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            cmd.env_remove(k);
        }
    }
    cmd.envs(env.iter().copied());
    cmd.output().expect("run scull")
}

fn schema(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("schemas/{name}.schema.json"))
}

fn validate_against(name: &str, v: &Value) {
    let s: Value = serde_json::from_str(&std::fs::read_to_string(schema(name)).expect("schema"))
        .expect("schema JSON");
    let val = jsonschema::validator_for(&s).expect("compiles");
    let errors: Vec<String> = val.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

/// Regenerate both with `SCULL_BLESS=1 cargo test -p scull-cli --test rules`.
#[test]
fn committed_schemas_match_generated() {
    use scull_cli::cmd::rules::{
        RULES_LIST_SCHEMA, RULES_VALIDATE_SCHEMA, RulesList, RulesValidation,
    };
    use scull_cli::output::json::document_schema;
    for (name, generated) in [
        (
            "rules-list",
            document_schema::<RulesList>(RULES_LIST_SCHEMA),
        ),
        (
            "rules-validate",
            document_schema::<RulesValidation>(RULES_VALIDATE_SCHEMA),
        ),
    ] {
        let text = serde_json::to_string_pretty(&generated).expect("serialize") + "\n";
        if std::env::var_os("SCULL_BLESS").is_some() {
            std::fs::write(schema(name), &text).expect("write");
        }
        let committed = std::fs::read_to_string(schema(name)).expect("schema file");
        assert_eq!(
            committed.replace("\r\n", "\n"),
            text,
            "schemas/{name}.schema.json is stale; rerun with SCULL_BLESS=1"
        );
    }
}

#[test]
fn list_shows_all_bundled_rules() {
    let home = tempfile::tempdir().expect("tempdir");
    let o = scull_in(home.path(), &["rules", "list", "--json"], &[]);
    assert_eq!(o.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate_against("rules-list", &v);
    assert_eq!(v["schema"], "scull/rules-list@1");
    let rules = v["data"]["rules"].as_array().expect("rules");
    let bundled = scull_detectors::rules::RuleSet::bundled().expect("bundled");
    assert_eq!(rules.len(), bundled.len());
    for r in bundled.all() {
        let row = rules
            .iter()
            .find(|x| x["id"] == r.id())
            .unwrap_or_else(|| panic!("{} missing", r.id()));
        assert_eq!(row["version"], r.version());
        assert_eq!(row["ecosystem"], r.ecosystem());
        assert_eq!(row["source"], "bundled");
        assert_eq!(row["enabled"], true);
        assert!(row["base_tier"].is_string() && row["strategy"].is_string());
        assert!(!row["platforms"].as_array().expect("platforms").is_empty());
    }
    assert!(
        v["data"]["problems"]
            .as_array()
            .expect("problems")
            .is_empty()
    );

    let o = scull_in(home.path(), &["rules", "list"], &[]);
    assert_eq!(o.status.code(), Some(0));
    let out = String::from_utf8(o.stdout).expect("utf-8");
    assert!(out.starts_with("ID "), "{out}");
    for r in bundled.all() {
        assert!(out.contains(r.id()), "{out}");
    }
    assert!(out.contains("S0 regenerable"));
    assert!(
        std::fs::read_dir(home.path())
            .expect("dir")
            .next()
            .is_none(),
        "nothing created"
    );
}

#[test]
fn list_includes_user_rules_disabled_until_enabled() {
    let home = tempfile::tempdir().expect("tempdir");
    let rules = home.path().join("config").join("rules");
    std::fs::create_dir_all(&rules).expect("dirs");
    std::fs::write(rules.join("unity.toml"), VALID).expect("rule");
    std::fs::write(
        rules.join("zz-bad.toml"),
        variant("base_tier = \"S1\"", "base_tier = \"S0\""),
    )
    .expect("bad rule");
    std::fs::write(rules.join("notes.txt"), "ignored").expect("other file");

    let list = |env: &[(&str, &str)]| -> Value {
        let o = scull_in(home.path(), &["rules", "list", "--json"], env);
        assert_eq!(o.status.code(), Some(0));
        let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
        validate_against("rules-list", &v);
        v
    };
    let v = list(&[]);
    let d = &v["data"];
    let unity = d["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .find(|r| r["id"] == "community.unity-library")
        .expect("user rule listed")
        .clone();
    assert_eq!(unity["source"], "user");
    assert_eq!(unity["enabled"], false, "user rules load disabled (CLS-04)");
    assert_eq!(unity["base_tier"], "S1");
    let problems = d["problems"].as_array().expect("problems");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].as_str().expect("str").contains("zz-bad.toml"));

    // Enabled through the user config file...
    std::fs::write(
        home.path().join("config").join("config.toml"),
        "[rules]\nenabled = [\"community.unity-library\", \"nope.missing\"]\n",
    )
    .expect("config");
    let v = list(&[]);
    let unity = &v["data"]["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .find(|r| r["id"] == "community.unity-library")
        .expect("listed")
        .clone();
    assert_eq!(unity["enabled"], true);
    assert!(
        v["data"]["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.contains("nope.missing")))
    );
    // ...or the environment, which replaces the file's list.
    let v = list(&[("SCULL_RULES", "nope.other")]);
    let unity = v["data"]["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .find(|r| r["id"] == "community.unity-library")
        .expect("listed")
        .clone();
    assert_eq!(unity["enabled"], false);
}

#[test]
fn validate_bundled_by_default_and_valid_user_file() {
    let home = tempfile::tempdir().expect("tempdir");
    let o = scull_in(home.path(), &["rules", "validate"], &[]);
    assert_eq!(o.status.code(), Some(0));
    let out = String::from_utf8(o.stdout).expect("utf-8");
    assert!(out.contains("ok      rules/ecosystems/rust.toml"), "{out}");

    let file = home.path().join("unity.toml");
    std::fs::write(&file, VALID).expect("rule");
    let path = file.to_str().expect("utf-8 path");
    let o = scull_in(home.path(), &["rules", "validate", path, "--json"], &[]);
    assert_eq!(o.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate_against("rules-validate", &v);
    assert_eq!(v["data"]["valid"], true);
    assert_eq!(v["data"]["files"][0]["source"], "user");
    assert_eq!(
        v["data"]["files"][0]["rules"],
        serde_json::json!(["community.unity-library"])
    );
}

/// Every loader rejection path from #39 is reported with its code, and exits 2.
#[test]
fn validate_reports_each_rejection_with_exit_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cases: Vec<(&str, String)> = vec![
        ("syntax", "this is = = not toml".to_owned()),
        (
            "missing_schema_version",
            variant("schema_version = 1\n", ""),
        ),
        (
            "unsupported_schema_version",
            variant("schema_version = 1", "schema_version = 99"),
        ),
        (
            "name_only",
            variant(
                "\n[rule.markers]\nall = [\"../Assets\", \"../ProjectSettings\"]\n",
                "",
            ),
        ),
        (
            "s0_not_allowed",
            variant("base_tier = \"S1\"", "base_tier = \"S0\""),
        ),
        (
            "tool_native_not_allowed",
            variant("strategy = \"filesystem\"", "strategy = \"tool_native\""),
        ),
        (
            "command_field",
            variant(
                "\nversion = 1\n",
                "\nversion = 1\ncleanup_command = \"rm -rf Library\"\n",
            ),
        ),
        (
            "schema",
            variant("\nversion = 1\n", "\nversion = 1\ncolour = \"red\"\n"),
        ),
        (
            "invalid_pattern",
            variant("names = [\"Library\"]", "names = [\"Lib{rary\"]"),
        ),
        (
            "wildcard_marker",
            variant(
                "all = [\"../Assets\", \"../ProjectSettings\"]",
                "all = [\"../*\"]",
            ),
        ),
        (
            "marker_matches_candidate",
            variant(
                "all = [\"../Assets\", \"../ProjectSettings\"]",
                "all = [\"../Library\"]",
            ),
        ),
        (
            "duplicate_id",
            variant("id = \"community.unity-library\"", "id = \"rust.target\""),
        ),
    ];
    for (code, text) in &cases {
        let file = dir.path().join(format!("{code}.toml"));
        std::fs::write(&file, text).expect("write");
        let path = file.to_str().expect("utf-8 path");

        let o = scull_in(dir.path(), &["rules", "validate", path, "--json"], &[]);
        assert_eq!(o.status.code(), Some(2), "{code}");
        let v: Value = serde_json::from_slice(&o.stdout).expect("JSON report on stdout");
        validate_against("rules-validate", &v);
        let f = &v["data"]["files"][0];
        assert_eq!(v["data"]["valid"], false, "{code}");
        assert_eq!(f["valid"], false, "{code}");
        assert_eq!(f["rejection"]["code"], *code, "{code}: {f}");
        assert!(f["rules"].as_array().expect("rules").is_empty());
        let err: Value = serde_json::from_slice(&o.stderr).expect("JSON error on stderr");
        assert_eq!(err["error"]["kind"], "config", "{code}");

        let o = scull_in(dir.path(), &["rules", "validate", path], &[]);
        assert_eq!(o.status.code(), Some(2), "{code}");
        let out = String::from_utf8(o.stdout).expect("utf-8");
        assert!(out.starts_with("invalid "), "{code}: {out}");
        assert!(out.contains(&format!("[{code}]")), "{code}: {out}");
        assert_eq!(
            out.lines().count(),
            2,
            "{code}: one line per file + reason: {out}"
        );
        assert!(
            String::from_utf8(o.stderr)
                .expect("utf-8")
                .contains("1 of 1 rule file is invalid")
        );
    }

    // Several files at once: each reported, exit 2 when any is invalid.
    let good = dir.path().join("good.toml");
    std::fs::write(&good, VALID).expect("write");
    let missing = dir.path().join("missing.toml");
    let bad = dir.path().join("s0_not_allowed.toml");
    let args = [
        "rules",
        "validate",
        good.to_str().expect("p"),
        missing.to_str().expect("p"),
        bad.to_str().expect("p"),
        "--json",
    ];
    let o = scull_in(dir.path(), &args, &[]);
    assert_eq!(o.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    let files = v["data"]["files"].as_array().expect("files");
    let valid: Vec<bool> = files
        .iter()
        .map(|f| f["valid"].as_bool().expect("b"))
        .collect();
    assert_eq!(valid, [true, false, false]);
    assert_eq!(files[1]["rejection"]["code"], "unreadable");
}
