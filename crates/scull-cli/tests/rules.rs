//! `scull rules list | validate` end to end (P1-CLI-06). User rule files and configs are written
//! to temp dirs; `SCULL_HOME` points at a temp dir so the real config is never read.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A valid user rule (S1, filesystem, marker-backed).
const VALID: &str = r#"schema_version = 1

[[rule]]
id = "community.godot-import"
version = 1
artifact = "Godot import cache"
ecosystem = "godot"
category = "project_artifact"
platforms = ["macos", "linux", "windows"]
base_tier = "S1"
strategy = "filesystem"
evidence = ["marker"]

[rule.match]
names = [".godot"]

[rule.markers]
all = ["../project.godot"]
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
    use scull_cli::cmd::rules_test::{RULES_TEST_SCHEMA, RulesTest};
    use scull_cli::output::json::document_schema;
    for (name, generated) in [
        (
            "rules-test",
            document_schema::<RulesTest>(RULES_TEST_SCHEMA),
        ),
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
        .find(|r| r["id"] == "community.godot-import")
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
        "[rules]\nenabled = [\"community.godot-import\", \"nope.missing\"]\n",
    )
    .expect("config");
    let v = list(&[]);
    let unity = &v["data"]["rules"]
        .as_array()
        .expect("rules")
        .iter()
        .find(|r| r["id"] == "community.godot-import")
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
        .find(|r| r["id"] == "community.godot-import")
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
        serde_json::json!(["community.godot-import"])
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
            variant("\n[rule.markers]\nall = [\"../project.godot\"]\n", ""),
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
                "\nversion = 1\ncleanup_command = \"rm -rf .godot\"\n",
            ),
        ),
        (
            "schema",
            variant("\nversion = 1\n", "\nversion = 1\ncolour = \"red\"\n"),
        ),
        (
            "invalid_pattern",
            variant("names = [\".godot\"]", "names = [\".go{dot\"]"),
        ),
        (
            "wildcard_marker",
            variant("all = [\"../project.godot\"]", "all = [\"../*\"]"),
        ),
        (
            "marker_matches_candidate",
            variant("all = [\"../project.godot\"]", "all = [\"../.godot\"]"),
        ),
        // P6-COM-01 guardrails for community rules.
        (
            "user_work_folder",
            variant("names = [\".godot\"]", "names = [\"Library\"]"),
        ),
        (
            "user_work_folder",
            variant("names = [\".godot\"]", "names = [\"S{rc,ource}\"]"),
        ),
        (
            "wildcard_name",
            variant("names = [\".godot\"]", "names = [\"*\"]"),
        ),
        (
            "no_project_marker",
            variant("all = [\"../project.godot\"]", "all = [\"imported\"]"),
        ),
        (
            "duplicate_id",
            variant("id = \"community.godot-import\"", "id = \"rust.target\""),
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

// ---------------------------------------------------------------------------------------------
// `scull rules test` (P6-COM-01): the rule authors' harness.

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples/rules")
        .join(name)
}

/// Files in `dir`, sorted (the harness writes nothing but `--init`'s cases file).
fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .expect("dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn rules_test_runs_the_shipped_example() {
    let home = tempfile::tempdir().expect("tempdir");
    let rule = example("godot.toml");
    let path = rule.to_str().expect("utf-8");
    let o = scull_in(home.path(), &["rules", "validate", path], &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let o = scull_in(home.path(), &["rules", "test", path, "--json"], &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    let v: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    validate_against("rules-test", &v);
    let f = &v["data"]["files"][0];
    assert_eq!(f["passed"], true, "{f:#}");
    let outcomes: Vec<&str> = f["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .map(|c| c["outcome"].as_str().expect("outcome"))
        .collect();
    assert_eq!(
        outcomes,
        ["match", "missing_marker", "excluded", "name_mismatch"]
    );
    let o = scull_in(home.path(), &["rules", "test", path], &[]);
    let out = String::from_utf8(o.stdout).expect("utf-8");
    assert!(out.contains("4 of 4 case(s) passed"), "{out}");
}

#[test]
fn rules_test_reports_failures_missing_cases_and_rejections() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rule = dir.path().join("godot.toml");
    std::fs::write(&rule, VALID).expect("rule");
    let path = rule.to_str().expect("utf-8");
    let json = |args: &[&str]| -> (Option<i32>, Value) {
        let o = scull_in(dir.path(), args, &[]);
        let v: Value = serde_json::from_slice(&o.stdout).expect("JSON report");
        validate_against("rules-test", &v);
        (o.status.code(), v)
    };

    // No cases file: refused, with the way to make one.
    let (code, v) = json(&["rules", "test", path, "--json"]);
    assert_eq!(code, Some(2));
    assert_eq!(v["data"]["files"][0]["rejection"]["code"], "no_cases");

    // A wrong expectation fails that case; a missing negative case is reported.
    std::fs::write(
        dir.path().join("godot.cases.toml"),
        "schema_version = 1\n\n[[case]]\nname = \"wrong\"\nrule = \"community.godot-import\"\n\
         tree = [\"g/.godot/\"]\ncandidate = \"g/.godot\"\nexpect = \"match\"\n",
    )
    .expect("cases");
    let (code, v) = json(&["rules", "test", path, "--json"]);
    assert_eq!(code, Some(2));
    let f = &v["data"]["files"][0];
    assert_eq!(f["passed"], false);
    assert_eq!(f["cases"][0]["passed"], false);
    assert_eq!(f["cases"][0]["outcome"], "missing_marker");
    assert_eq!(
        f["missing"],
        serde_json::json!(["community.godot-import: no case expects no match"])
    );

    // Unknown fields and unknown rules in the cases file are refused.
    std::fs::write(
        dir.path().join("godot.cases.toml"),
        "schema_version = 1\nrun = \"x\"\n",
    )
    .expect("cases");
    let (_, v) = json(&["rules", "test", path, "--json"]);
    assert_eq!(v["data"]["files"][0]["rejection"]["code"], "cases_syntax");

    // A rule the loader refuses never gets to its cases.
    std::fs::write(&rule, variant("names = [\".godot\"]", "names = [\"src\"]")).expect("rule");
    let (code, v) = json(&["rules", "test", path, "--json"]);
    assert_eq!(code, Some(2));
    assert_eq!(
        v["data"]["files"][0]["rejection"]["code"],
        "user_work_folder"
    );
    assert_eq!(listing(dir.path()), ["godot.cases.toml", "godot.toml"]);
}

#[test]
fn rules_test_init_writes_a_passing_starter_and_never_overwrites() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rule = dir.path().join("godot.toml");
    std::fs::write(&rule, VALID).expect("rule");
    let path = rule.to_str().expect("utf-8");
    let o = scull_in(dir.path(), &["rules", "test", "--init", path], &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let cases = dir.path().join("godot.cases.toml");
    let written = std::fs::read_to_string(&cases).expect("cases written");
    assert!(written.contains("expect = \"match\""), "{written}");
    assert!(written.contains("expect = \"no_match\""), "{written}");
    let o = scull_in(dir.path(), &["rules", "test", path], &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    // A second --init refuses and leaves the file as it is.
    std::fs::write(&cases, "# mine\n").expect("edit");
    let o = scull_in(dir.path(), &["rules", "test", "--init", path], &[]);
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&cases).expect("cases"), "# mine\n");
    assert_eq!(listing(dir.path()), ["godot.cases.toml", "godot.toml"]);
}
