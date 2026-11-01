//! Config loading and precedence (P1-CFG-01; PRD §11 CFG-01…04; FIXTURES FX-CFG-01…04,
//! FX-CLS-11, FX-CLS-12; THREAT_MODEL T-20).

// Helpers outside `#[test]` fns build fixtures from known-good literals.
#![allow(clippy::unwrap_used)]

use proptest::prelude::*;
use scull_core::config::*;
use scull_core::{ErrorKind, Tier};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// An absolute path on the current platform: `/a/b` on Unix, `C:\a\b` on Windows.
fn abs(p: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{}", p.replace('/', "\\")))
    } else {
        PathBuf::from(p)
    }
}

fn env_from(pairs: &[(&str, &str)]) -> EnvLayer {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    EnvLayer::from_lookup(|k| map.get(k).cloned()).expect("valid env")
}

fn codes(ws: &[ConfigWarning]) -> Vec<ConfigWarningCode> {
    ws.iter().map(|w| w.code).collect()
}

fn has_warning(ws: &[ConfigWarning], code: ConfigWarningCode, key: &str) -> bool {
    ws.iter().any(|w| w.code == code && w.key == key)
}

// ---------------------------------------------------------------------------------------------
// FX-CFG-01: repo `.scull.toml` tries to lower tiers to S0 → ignored with a warning.
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_cfg_01_repo_toml_lowering_tiers_to_s0_is_ignored_with_warning() {
    let repo = abs("/home/me/Projects/cloned");

    // Shorthand form: "everything here is S0".
    let a = RepoPolicy::parse_toml("[scan]\nmin_tier = \"S0\"\n");
    assert!(a.policy.is_empty(), "nothing applied: {:?}", a.policy);
    assert!(has_warning(
        &a.warnings,
        ConfigWarningCode::RepoLowersProtection,
        "scan.min_tier"
    ));

    // Per-path form plus invented keys an attacker might try.
    let b = RepoPolicy::parse_toml(
        r#"
tier = "S0"
[tiers]
"." = "S0"
[scan.min_tier]
"." = "S0"
"target" = "S0"
"#,
    );
    assert!(b.policy.is_empty(), "nothing applied: {:?}", b.policy);
    assert!(has_warning(
        &b.warnings,
        ConfigWarningCode::RepoLowersProtection,
        "scan.min_tier.\".\""
    ));
    assert!(has_warning(
        &b.warnings,
        ConfigWarningCode::RepoLowersProtection,
        "scan.min_tier.\"target\""
    ));
    assert!(has_warning(
        &b.warnings,
        ConfigWarningCode::RepoUnknownKey,
        "tier"
    ));
    assert!(has_warning(
        &b.warnings,
        ConfigWarningCode::RepoUnknownKey,
        "tiers.."
    ));
    assert_eq!(b.warnings.len(), 4, "{:?}", b.warnings);

    // An existing user floor is not lowered by the repo file.
    let mut user = Config::default();
    user.scan.min_tier.insert(repo.join("target"), Tier::S2);
    let (applied, _) = user.with_repo_policy(&repo, &b.policy);
    assert_eq!(applied, user);
}

// ---------------------------------------------------------------------------------------------
// FX-CFG-02: repo config adds a scan root or enables a rule → ignored with a warning.
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_cfg_02_repo_adding_roots_or_enabling_rules_is_ignored_with_warning() {
    let repo = abs("/home/me/Projects/cloned");
    let r = RepoPolicy::parse_toml(
        r#"
[scan]
roots = ["/", "~", "C:\\"]
threads = 512
low_resource = false
activity_window = "1s"

[rules]
enabled = ["evil.delete-everything"]

[mcp]
enabled = true
destructive = true
redact_home = false

[forge]
enabled = true

[output]
color = "always"
"#,
    );
    for key in [
        "scan.roots",
        "scan.threads",
        "scan.low_resource",
        "rules.enabled",
        "mcp.enabled",
        "mcp.destructive",
        "mcp.redact_home",
        "forge.enabled",
        "output.color",
    ] {
        assert!(
            has_warning(&r.warnings, ConfigWarningCode::RepoForbiddenSetting, key),
            "no warning for {key}: {:?}",
            r.warnings
        );
    }
    assert!(
        r.warnings.iter().all(|w| w.message.contains("ignored")),
        "{:?}",
        r.warnings
    );

    // Applying the policy changes nothing; the shorter activity window is reported too.
    let mut base = Config::default();
    base.scan.roots = vec![abs("/home/me/Projects")];
    let (applied, ws) = base.with_repo_policy(&repo, &r.policy);
    assert_eq!(applied, base);
    assert_eq!(codes(&ws), vec![ConfigWarningCode::RepoLowersProtection]);
    assert_eq!(ws[0].key, "scan.activity_window");
}

#[test]
fn repo_paths_cannot_escape_their_directory() {
    let repo = abs("/home/me/Projects/cloned");
    let r = RepoPolicy::parse_toml(
        r#"
[scan]
protect = ["../sibling", "/etc", "C:\\Windows", "\\\\server\\share", "~/.ssh", "a/../../b", "keep"]
exclude = ["..", "data/.. /x"]
[scan.min_tier]
"../.." = "S3"
"#,
    );
    let rejected: Vec<_> = r
        .warnings
        .iter()
        .filter(|w| w.code == ConfigWarningCode::RepoPathRejected)
        .collect();
    assert_eq!(rejected.len(), 9, "{:?}", r.warnings);
    assert_eq!(r.policy.protect().len(), 1);
    assert!(r.policy.exclude().is_empty() && r.policy.min_tier().is_empty());
    let (applied, _) = Config::default().with_repo_policy(&repo, &r.policy);
    assert_eq!(
        applied.scan.protect.into_iter().collect::<Vec<_>>(),
        vec![repo.join("keep")]
    );
}

// ---------------------------------------------------------------------------------------------
// FX-CFG-03: repo `.scull-protect` / config adds protection → honored.
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_cfg_03_repo_protect_marker_and_config_add_protection_honored() {
    let repo = abs("/home/me/Projects/ml");
    let r = RepoPolicy::parse_toml(
        r#"
[scan]
protect = ["data", "notebooks/output"]
exclude = ["vendor/huge"]
activity_window = "72h"
[scan.min_tier]
"." = "S2"
"build" = "S1"
"#,
    );
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);

    let (c, ws) = Config::default().with_repo_policy(&repo, &r.policy);
    assert!(ws.is_empty());
    assert!(c.scan.protect.contains(&repo.join("data")));
    assert!(
        c.scan
            .protect
            .contains(&repo.join("notebooks").join("output"))
    );
    assert!(c.scan.exclude.contains(&repo.join("vendor").join("huge")));
    assert_eq!(c.scan.min_tier.get(&repo), Some(&Tier::S2));
    assert_eq!(c.scan.min_tier.get(&repo.join("build")), Some(&Tier::S1));
    assert_eq!(c.scan.activity_window.as_secs(), 72 * 3600);

    // `.scull-protect` marker: the whole directory.
    let (m, _) = Config::default().with_repo_policy(&repo, &RepoPolicy::protect_marker());
    assert!(m.scan.protect.contains(&repo));

    // Both files in one directory combine.
    let mut both = r.policy.clone();
    both.merge(&RepoPolicy::protect_marker());
    let (b, _) = Config::default().with_repo_policy(&repo, &both);
    assert!(b.scan.protect.contains(&repo) && b.scan.protect.contains(&repo.join("data")));
}

#[test]
fn fx_cls_11_scull_protect_marker_makes_everything_inside_s3() {
    let project = abs("/home/me/Projects/precious");
    let (c, ws) = Config::default().with_repo_policy(&project, &RepoPolicy::protect_marker());
    assert!(ws.is_empty());
    // The project root is a protected path, so every candidate below it is S3 (PRD §8.3).
    assert_eq!(c.scan.protect.iter().collect::<Vec<_>>(), vec![&project]);
}

#[test]
fn invalid_repo_toml_never_blocks_and_protects_the_directory() {
    let repo = abs("/r");
    let r = RepoPolicy::parse_toml("[scan\nroots = [\"/\"");
    assert_eq!(codes(&r.warnings), vec![ConfigWarningCode::RepoParseError]);
    let (c, _) = Config::default().with_repo_policy(&repo, &r.policy);
    assert!(c.scan.protect.contains(&repo));
    assert!(c.scan.roots.is_empty());
}

// ---------------------------------------------------------------------------------------------
// FX-CFG-04: user config protects a path → honored everywhere.
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_cfg_04_user_config_protected_path_survives_every_layer() {
    let thesis = abs("/home/me/thesis");
    let user = ConfigLayer::from_toml(&format!(
        "[scan]\nprotect = [{:?}]\nexclude = [{:?}]\n",
        thesis.to_str().unwrap(),
        abs("/home/me/vm").to_str().unwrap()
    ))
    .unwrap();

    // The CLI, desktop app and MCP server all resolve through the same function with their own
    // CLI layer: none of them can drop the user's protection.
    let mut cli = ConfigLayer::default();
    cli.scan.roots = Some(vec![abs("/home/me")]);
    cli.scan.protect = Some(vec![abs("/home/me/other")]);
    cli.scan.exclude = Some(vec![]);
    let env = env_from(&[("SCULL_PROTECT", abs("/home/me/env").to_str().unwrap())]);
    for (cli, env) in [
        (ConfigLayer::default(), EnvLayer::default()),
        (cli.clone(), EnvLayer::default()),
        (ConfigLayer::default(), env.clone()),
        (cli.clone(), env.clone()),
    ] {
        let r = resolve(&cli, &env, &user);
        assert!(
            r.config.scan.protect.contains(&thesis),
            "{:?}",
            r.config.scan
        );
        assert!(r.config.scan.exclude.contains(&abs("/home/me/vm")));
        assert!(r.origin("scan.protect").contains(&Origin::UserConfig));

        // A repository inside the protected path can't undo it either.
        let hostile =
            RepoPolicy::parse_toml("[scan]\nprotect = []\nexclude = []\nmin_tier = \"S0\"\n");
        let (applied, _) = r.config.with_repo_policy(&thesis, &hostile.policy);
        assert!(applied.scan.protect.contains(&thesis));
    }
    let r = resolve(&cli, &env, &user);
    assert_eq!(r.config.scan.protect.len(), 3);
    assert_eq!(
        r.origin("scan.protect"),
        &[
            Origin::Cli,
            Origin::Env {
                var: "SCULL_PROTECT".into()
            },
            Origin::UserConfig
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// CFG-02 precedence: every field from every layer.
// ---------------------------------------------------------------------------------------------

fn full_user() -> ConfigLayer {
    ConfigLayer::from_toml(&format!(
        r#"
[scan]
roots = [{root:?}]
exclude = [{ex:?}]
protect = [{prot:?}]
activity_window = "48h"
threads = 2
low_resource = true
[scan.min_tier]
{shared:?} = "S1"
{umt:?} = "S2"

[rules]
enabled = ["user.rule"]

[mcp]
enabled = true
destructive = true
redact_home = false
hash_names = true
max_results = 10

[forge]
enabled = true

[output]
color = "always"
units = "decimal"

[quarantine]
base = {qb:?}
[quarantine.bases]
{qk:?} = {qv:?}
"#,
        root = abs("/u/root").to_str().unwrap(),
        ex = abs("/u/ex").to_str().unwrap(),
        prot = abs("/u/prot").to_str().unwrap(),
        shared = abs("/shared").to_str().unwrap(),
        umt = abs("/u/mt").to_str().unwrap(),
        qb = abs("/u/qbase").to_str().unwrap(),
        qk = abs("/vol").to_str().unwrap(),
        qv = abs("/vol/u-q").to_str().unwrap(),
    ))
    .expect("user config parses")
}

fn full_env() -> EnvLayer {
    let qbase = abs("/e/qbase");
    let root = abs("/e/root");
    let ex = abs("/e/ex");
    let prot = abs("/e/prot");
    env_from(&[
        ("SCULL_ROOTS", root.to_str().unwrap()),
        ("SCULL_EXCLUDE", ex.to_str().unwrap()),
        ("SCULL_PROTECT", prot.to_str().unwrap()),
        ("SCULL_ACTIVITY_WINDOW", "72h"),
        ("SCULL_THREADS", "3"),
        ("SCULL_LOW_RESOURCE", "false"),
        ("SCULL_RULES", "env.rule"),
        ("SCULL_MCP", "false"),
        ("SCULL_MCP_REDACT_HOME", "true"),
        ("SCULL_MCP_HASH_NAMES", "false"),
        ("SCULL_MCP_MAX_RESULTS", "20"),
        ("SCULL_FORGE", "false"),
        ("SCULL_COLOR", "never"),
        ("SCULL_UNITS", "binary"),
        ("SCULL_QUARANTINE_BASE", qbase.to_str().unwrap()),
    ])
}

fn full_cli() -> ConfigLayer {
    let mut l = ConfigLayer::default();
    l.scan.roots = Some(vec![abs("/c/root")]);
    l.scan.exclude = Some(vec![abs("/c/ex")]);
    l.scan.protect = Some(vec![abs("/c/prot")]);
    l.scan.min_tier = Some([(abs("/shared"), Tier::S3)].into_iter().collect());
    l.scan.activity_window = Some(DurationSpec::from_hours(96));
    l.scan.threads = Some(4);
    l.scan.low_resource = Some(true);
    l.rules.enabled = Some(vec!["cli.rule".into()]);
    l.mcp.enabled = Some(true);
    l.mcp.destructive = Some(false); // ignored: file-only
    l.mcp.redact_home = Some(false);
    l.mcp.hash_names = Some(true);
    l.mcp.max_results = Some(30);
    l.forge.enabled = Some(true);
    l.output.color = Some(ColorChoice::Auto);
    l.output.units = Some(SizeUnits::Decimal);
    l.quarantine.base = Some(abs("/c/qbase"));
    l.quarantine.bases = Some([(abs("/vol"), abs("/vol/c-q"))].into_iter().collect());
    l
}

fn field(c: &Config, key: &str) -> serde_json::Value {
    let (sec, f) = key.split_once('.').unwrap();
    serde_json::to_value(c).unwrap()[sec][f].clone()
}

fn p(s: &str) -> serde_json::Value {
    json!(abs(s).to_str().unwrap())
}

#[test]
fn precedence_table_every_field_every_layer() {
    // (field, default, user, env, cli) for every override field.
    let table: Vec<(
        &str,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
    )> = vec![
        (
            "scan.roots",
            json!([]),
            json!([p("/u/root")]),
            json!([p("/e/root")]),
            json!([p("/c/root")]),
        ),
        (
            "scan.activity_window",
            json!("24h"),
            json!("48h"),
            json!("72h"),
            json!("96h"),
        ),
        ("scan.threads", json!(0), json!(2), json!(3), json!(4)),
        (
            "scan.low_resource",
            json!(false),
            json!(true),
            json!(false),
            json!(true),
        ),
        (
            "rules.enabled",
            json!([]),
            json!(["user.rule"]),
            json!(["env.rule"]),
            json!(["cli.rule"]),
        ),
        (
            "mcp.enabled",
            json!(false),
            json!(true),
            json!(false),
            json!(true),
        ),
        (
            "mcp.redact_home",
            json!(true),
            json!(false),
            json!(true),
            json!(false),
        ),
        (
            "mcp.hash_names",
            json!(false),
            json!(true),
            json!(false),
            json!(true),
        ),
        (
            "mcp.max_results",
            json!(1000),
            json!(10),
            json!(20),
            json!(30),
        ),
        (
            "quarantine.base",
            json!(null),
            p("/u/qbase"),
            p("/e/qbase"),
            p("/c/qbase"),
        ),
        (
            "forge.enabled",
            json!(false),
            json!(true),
            json!(false),
            json!(true),
        ),
        (
            "output.color",
            json!("auto"),
            json!("always"),
            json!("never"),
            json!("auto"),
        ),
        (
            "output.units",
            json!("binary"),
            json!("decimal"),
            json!("binary"),
            json!("decimal"),
        ),
    ];
    let none = ConfigLayer::default();
    let no_env = EnvLayer::default();
    let (user, env, cli) = (full_user(), full_env(), full_cli());
    let stages = [
        resolve(&none, &no_env, &none),
        resolve(&none, &no_env, &user),
        resolve(&none, &env, &user),
        resolve(&cli, &env, &user),
    ];
    let env_var = |f: &str| {
        ENV_VARS
            .iter()
            .find(|v| v.field == f)
            .map(|v| v.name.to_string())
            .unwrap()
    };
    for (key, d, u, e, c) in &table {
        let want = [d, u, e, c];
        let want_origin = [
            Origin::Default,
            Origin::UserConfig,
            Origin::Env { var: env_var(key) },
            Origin::Cli,
        ];
        for (i, r) in stages.iter().enumerate() {
            assert_eq!(&field(&r.config, key), want[i], "{key} at stage {i}");
            assert_eq!(
                r.origin(key),
                std::slice::from_ref(&want_origin[i]),
                "{key} origin at stage {i}"
            );
        }
    }
    // Every override field changes at every stage (the table really tests each layer).
    for (key, d, u, e, c) in &table {
        assert!(
            d != u && u != e && e != c,
            "{key}: pick distinct values per layer"
        );
    }
    // Skipping a layer: CLI over user with no env; env over defaults with no user file.
    let r = resolve(&cli, &no_env, &user);
    assert_eq!(r.config.scan.threads, 4);
    assert_eq!(r.origin("scan.threads"), &[Origin::Cli]);
    let r = resolve(&none, &env, &none);
    assert_eq!(r.config.scan.threads, 3);

    // quarantine.bases has no environment variable: user, then CLI.
    assert_eq!(field(&stages[0].config, "quarantine.bases"), json!({}));
    for (i, want, origin) in [
        (1, "/vol/u-q", Origin::UserConfig),
        (2, "/vol/u-q", Origin::UserConfig),
        (3, "/vol/c-q", Origin::Cli),
    ] {
        let r = &stages[i];
        assert_eq!(
            r.config.quarantine.bases.get(&abs("/vol")),
            Some(&abs(want)),
            "stage {i}"
        );
        assert_eq!(r.origin("quarantine.bases"), &[origin], "stage {i}");
    }

    // The table plus the additive fields, mcp.destructive and quarantine.bases cover every
    // field.
    let mut covered: Vec<&str> = table.iter().map(|t| t.0).collect();
    covered.extend(ADDITIVE_FIELDS);
    covered.push("mcp.destructive");
    covered.extend(["mcp.max_pending_plans", "mcp.prepare_per_minute"]);
    // Alert policies are user-file settings (no environment variables or flags).
    covered.extend(FIELDS.iter().filter(|f| f.starts_with("proactive.")));
    covered.push("quarantine.bases");
    covered.sort();
    let mut all = FIELDS.to_vec();
    all.sort();
    assert_eq!(covered, all);
}

#[test]
fn precedence_additive_fields_combine_all_layers() {
    let (user, env, cli) = (full_user(), full_env(), full_cli());
    let r = resolve(&cli, &env, &user);
    let c = &r.config.scan;
    for s in ["/u/ex", "/e/ex", "/c/ex"] {
        assert!(c.exclude.contains(&abs(s)), "{s}");
    }
    for s in ["/u/prot", "/e/prot", "/c/prot"] {
        assert!(c.protect.contains(&abs(s)), "{s}");
    }
    // min_tier: user S1 for /shared raised to S3 by the CLI; user-only entry kept.
    assert_eq!(c.min_tier.get(&abs("/shared")), Some(&Tier::S3));
    assert_eq!(c.min_tier.get(&abs("/u/mt")), Some(&Tier::S2));
    let env_o = |v: &str| Origin::Env { var: v.into() };
    assert_eq!(
        r.origin("scan.exclude"),
        &[Origin::Cli, env_o("SCULL_EXCLUDE"), Origin::UserConfig]
    );
    assert_eq!(
        r.origin("scan.protect"),
        &[Origin::Cli, env_o("SCULL_PROTECT"), Origin::UserConfig]
    );
    assert_eq!(
        r.origin("scan.min_tier"),
        &[Origin::Cli, Origin::UserConfig]
    );

    // A lower CLI tier can't lower the user's floor.
    let mut low = ConfigLayer::default();
    low.scan.min_tier = Some([(abs("/u/mt"), Tier::S0)].into_iter().collect());
    let r = resolve(&low, &EnvLayer::default(), &user);
    assert_eq!(r.config.scan.min_tier.get(&abs("/u/mt")), Some(&Tier::S2));

    let r = resolve(
        &ConfigLayer::default(),
        &EnvLayer::default(),
        &ConfigLayer::default(),
    );
    for f in ADDITIVE_FIELDS {
        assert_eq!(r.origin(f), &[Origin::Default], "{f}");
    }
}

#[test]
fn mcp_destructive_only_from_user_config_file() {
    let none = ConfigLayer::default();
    let mut cli = ConfigLayer::default();
    cli.mcp.destructive = Some(true);
    let mut env = EnvLayer::default();
    env.layer.mcp.destructive = Some(true);
    let r = resolve(&cli, &env, &none);
    assert!(!r.config.mcp.destructive);
    assert_eq!(r.origin("mcp.destructive"), &[Origin::Default]);

    let user = full_user(); // destructive = true
    let r = resolve(&full_cli(), &full_env(), &user); // CLI says false: ignored
    assert!(r.config.mcp.destructive);
    assert_eq!(r.origin("mcp.destructive"), &[Origin::UserConfig]);
}

/// #299: the agent limits are file-only too: an agent that sets its MCP server's environment
/// or flags can't raise them.
#[test]
fn mcp_agent_limits_only_from_user_config_file() {
    use scull_core::config::{DEFAULT_MCP_MAX_PENDING_PLANS, DEFAULT_MCP_PREPARE_PER_MINUTE};
    let mut cli = ConfigLayer::default();
    cli.mcp.max_pending_plans = Some(1000);
    cli.mcp.prepare_per_minute = Some(1000);
    let mut env = EnvLayer::default();
    env.layer.mcp.max_pending_plans = Some(999);
    env.layer.mcp.prepare_per_minute = Some(999);
    let r = resolve(&cli, &env, &ConfigLayer::default());
    assert_eq!(
        r.config.mcp.max_pending_plans,
        DEFAULT_MCP_MAX_PENDING_PLANS
    );
    assert_eq!(
        r.config.mcp.prepare_per_minute,
        DEFAULT_MCP_PREPARE_PER_MINUTE
    );
    assert_eq!(r.origin("mcp.max_pending_plans"), &[Origin::Default]);
    let user =
        ConfigLayer::from_toml("[mcp]\nmax_pending_plans = 2\nprepare_per_minute = 3\n").unwrap();
    let r = resolve(&cli, &env, &user);
    assert_eq!(r.config.mcp.max_pending_plans, 2);
    assert_eq!(r.config.mcp.prepare_per_minute, 3);
    assert_eq!(r.origin("mcp.prepare_per_minute"), &[Origin::UserConfig]);
}

#[test]
fn origins_reported_for_every_field() {
    for r in [
        resolve(
            &ConfigLayer::default(),
            &EnvLayer::default(),
            &ConfigLayer::default(),
        ),
        resolve(&full_cli(), &full_env(), &full_user()),
    ] {
        for f in FIELDS {
            assert!(!r.origin(f).is_empty(), "{f} has no origin");
        }
        assert_eq!(r.origins.len(), FIELDS.len());
        // Serializable for `scull doctor --json` / `config show --json`.
        let v = serde_json::to_value(&r).unwrap();
        let back: ResolvedConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }
    let r = resolve(
        &ConfigLayer::default(),
        &full_env(),
        &ConfigLayer::default(),
    );
    assert_eq!(r.origin("scan.threads")[0].to_string(), "env SCULL_THREADS");
    assert_eq!(Origin::UserConfig.to_string(), "user config");
}

// ---------------------------------------------------------------------------------------------
// Environment layer.
// ---------------------------------------------------------------------------------------------

#[test]
fn env_vars_parse_and_report_names() {
    let a = abs("/a");
    let b = abs("/b");
    let joined = std::env::join_paths([&a, &b]).unwrap();
    let e = env_from(&[
        ("SCULL_ROOTS", joined.to_str().unwrap()),
        ("SCULL_RULES", " x , y ,,"),
        ("SCULL_LOW_RESOURCE", "YES"),
        ("SCULL_THREADS", ""), // empty = unset
    ]);
    assert_eq!(e.layer.scan.roots, Some(vec![a, b]));
    assert_eq!(e.layer.rules.enabled, Some(vec!["x".into(), "y".into()]));
    assert_eq!(e.layer.scan.low_resource, Some(true));
    assert_eq!(e.layer.scan.threads, None);
    assert_eq!(e.vars.get("scan.roots"), Some(&"SCULL_ROOTS"));
    // Every documented variable maps to a real field.
    for v in ENV_VARS {
        assert!(FIELDS.contains(&v.field), "{}", v.name);
    }
}

#[test]
fn env_color_variables() {
    assert_eq!(
        env_from(&[("NO_COLOR", "1")]).layer.output.color,
        Some(ColorChoice::Never)
    );
    let e = env_from(&[("NO_COLOR", "1"), ("SCULL_COLOR", "always")]);
    assert_eq!(e.layer.output.color, Some(ColorChoice::Always));
    assert_eq!(e.vars.get("output.color"), Some(&"SCULL_COLOR"));
    let e = env_from(&[("NO_COLOR", "1"), ("SCULL_NO_COLOR", "1")]);
    assert_eq!(e.vars.get("output.color"), Some(&"SCULL_NO_COLOR"));
    assert_eq!(env_from(&[("NO_COLOR", "")]).layer.output.color, None);
}

#[test]
fn invalid_env_values_are_config_errors_naming_the_variable() {
    for (k, v) in [
        ("SCULL_THREADS", "many"),
        ("SCULL_LOW_RESOURCE", "maybe"),
        ("SCULL_ACTIVITY_WINDOW", "24"),
        ("SCULL_COLOR", "rainbow"),
        ("SCULL_UNITS", "furlongs"),
        ("SCULL_MCP_MAX_RESULTS", "0"),
    ] {
        let err = EnvLayer::from_lookup(|q| (q == k).then(|| v.to_string())).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config, "{k}");
        assert_eq!(err.exit_code().as_i32(), 2);
        assert!(err.message.contains(k), "{}", err.message);
    }
}

// ---------------------------------------------------------------------------------------------
// User config file.
// ---------------------------------------------------------------------------------------------

#[test]
fn user_config_unknown_fields_rejected_with_helpful_error() {
    for (text, needle) in [
        ("[scan]\nrootz = [\"/x\"]\n", "rootz"),
        ("[scna]\nroots = []\n", "scna"),
        ("[mcp]\ndestructve = true\n", "destructve"),
        ("verbose = true\n", "verbose"),
    ] {
        let err = ConfigLayer::from_toml(text).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config);
        assert_eq!(err.exit_code().as_i32(), 2, "exit 2 kind config");
        assert!(
            err.message.contains(&format!("unknown field `{needle}`")),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("expected"),
            "lists valid keys: {}",
            err.message
        );
        assert!(
            err.message.contains("line"),
            "points at the line: {}",
            err.message
        );
    }
    for bad in [
        "[scan]\nthreads = \"many\"\n",
        "[scan]\nactivity_window = \"soon\"\n",
        "[output]\ncolor = \"rainbow\"\n",
        "[scan.min_tier]\n\"/x\" = \"S9\"\n",
    ] {
        let err = ConfigLayer::from_toml(bad).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config, "{bad}");
    }
}

#[test]
fn user_config_round_trips() {
    let u = full_user();
    let text = toml::to_string(&u).unwrap();
    assert_eq!(ConfigLayer::from_toml(&text).unwrap(), u);
    assert!(ConfigLayer::from_toml("").unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// CFG-04 / FX-CLS-12: scan roots.
// ---------------------------------------------------------------------------------------------

#[test]
fn fx_cls_12_home_and_filesystem_roots_require_confirmation() {
    let home = abs("/home/me");
    let fs_roots = vec![abs("/"), abs("/mnt/data")];

    // Defaults never contain roots: with nothing configured there is nothing to scan.
    let defaults = Config::default();
    let err = validate_roots(&defaults.scan.roots, &home, &fs_roots).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Usage);
    assert_eq!(err.exit_code().as_i32(), 2);

    let check = |r: &str| validate_roots(&[abs(r)], &home, &fs_roots).unwrap();
    assert_eq!(check("/home/me")[0].concern, RootConcern::HomeRoot);
    assert_eq!(check("/home/me/")[0].concern, RootConcern::HomeRoot);
    assert_eq!(check("/home/./me")[0].concern, RootConcern::HomeRoot);
    assert_eq!(check("/")[0].concern, RootConcern::FilesystemRoot);
    assert_eq!(check("/mnt/data")[0].concern, RootConcern::FilesystemRoot);
    assert_eq!(check("/home")[0].concern, RootConcern::ContainsHome);
    assert!(check("/home/me/Projects").is_empty());
    assert!(check("/home/meme").is_empty());
    assert!(check("/mnt/data/projects").is_empty());

    // Several roots: only the risky ones are listed, in order.
    let many = validate_roots(
        &[abs("/home/me/Projects"), abs("/"), home.clone()],
        &home,
        &[],
    )
    .unwrap();
    assert_eq!(
        many.iter().map(|c| c.concern).collect::<Vec<_>>(),
        vec![RootConcern::FilesystemRoot, RootConcern::HomeRoot]
    );
    assert!(many[1].message.contains("home directory"));
}

#[test]
fn validate_roots_rejects_unexpanded_relative_and_dotdot() {
    let home = abs("/home/me");
    for bad in [
        PathBuf::from("~/Projects"),
        PathBuf::from("Projects"),
        abs("/home/me/../.."),
    ] {
        let err = validate_roots(std::slice::from_ref(&bad), &home, &[]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Usage, "{}", bad.display());
    }
    // After expansion the home-relative root is fine.
    let ok = expand_home(Path::new("~/Projects"), &home);
    assert!(validate_roots(&[ok], &home, &[]).unwrap().is_empty());
}

#[cfg(windows)]
#[test]
fn validate_roots_windows_forms() {
    let home = PathBuf::from(r"C:\Users\Me");
    let check = |r: &str| validate_roots(&[PathBuf::from(r)], &home, &[]).unwrap();
    assert_eq!(check(r"C:\")[0].concern, RootConcern::FilesystemRoot);
    assert_eq!(check(r"D:\")[0].concern, RootConcern::FilesystemRoot);
    assert_eq!(check(r"\\?\C:\")[0].concern, RootConcern::FilesystemRoot);
    assert_eq!(check(r"c:\users\me")[0].concern, RootConcern::HomeRoot);
    assert_eq!(check(r"\\?\C:\Users\Me")[0].concern, RootConcern::HomeRoot);
    assert_eq!(check(r"C:\Users")[0].concern, RootConcern::ContainsHome);
    assert!(check(r"C:\Users\Me\src").is_empty());
}

// ---------------------------------------------------------------------------------------------
// Property tests: repo policy never reduces protection (CFG-03, T-20).
// ---------------------------------------------------------------------------------------------

fn path_str() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "a", "b", ".", "..", "/", "\\", ":", "~", " ", "C:", "x y",
        ]),
        0..6,
    )
    .prop_map(|v| v.concat())
}

fn toml_value() -> impl Strategy<Value = toml::Value> {
    let leaf = prop_oneof![
        path_str().prop_map(toml::Value::String),
        prop::sample::select(vec![
            "S0", "S1", "S2", "S3", "S9", "1s", "72h", "9999d", "auto"
        ])
        .prop_map(|s| toml::Value::String(s.into())),
        any::<i32>().prop_map(|i| toml::Value::Integer(i.into())),
        any::<bool>().prop_map(toml::Value::Boolean),
    ];
    leaf.prop_recursive(2, 12, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(toml::Value::Array),
            prop::collection::btree_map(path_str(), inner, 0..4)
                .prop_map(|m| toml::Value::Table(m.into_iter().collect())),
        ]
    })
}

fn repo_toml() -> impl Strategy<Value = String> {
    let section = prop::sample::select(vec![
        "scan", "rules", "mcp", "forge", "output", "tier", "scan",
    ]);
    let key = prop::sample::select(vec![
        "roots",
        "protect",
        "exclude",
        "min_tier",
        "activity_window",
        "threads",
        "low_resource",
        "enabled",
        "destructive",
        "redact_home",
        "color",
        "tier",
    ]);
    prop_oneof![
        3 => prop::collection::vec((section, key, toml_value()), 0..10).prop_map(|entries| {
            let mut root = toml::Table::new();
            for (s, k, v) in entries {
                let sec = root
                    .entry(s.to_string())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if let toml::Value::Table(t) = sec {
                    t.insert(k.to_string(), v);
                }
            }
            toml::to_string(&root).unwrap_or_default()
        }),
        1 => ".{0,60}",
    ]
}

fn base_config() -> impl Strategy<Value = Config> {
    (
        prop::collection::btree_set("[a-c]{1,3}", 0..3),
        prop::collection::btree_map("[a-c]{1,3}", prop::sample::select(Tier::ALL.to_vec()), 0..3),
        0u64..200,
        any::<(bool, bool, bool, u32)>(),
    )
        .prop_map(|(prot, min, hours, (mcp, forge, low, threads))| {
            let mut c = Config::default();
            let base = abs("/home/me/repo");
            c.scan.roots = vec![abs("/home/me")];
            c.scan.protect = prot.iter().map(|p| base.join(p)).collect();
            c.scan.exclude = prot.iter().map(|p| base.join(p).join("x")).collect();
            c.scan.min_tier = min.into_iter().map(|(p, t)| (base.join(p), t)).collect();
            c.scan.activity_window = DurationSpec::from_hours(hours);
            c.scan.threads = threads;
            c.scan.low_resource = low;
            c.mcp.enabled = mcp;
            c.forge.enabled = forge;
            c.rules.enabled.insert("user.rule".into());
            c
        })
}

fn non_protection(c: &Config) -> Config {
    let mut c = c.clone();
    c.scan.protect.clear();
    c.scan.exclude.clear();
    c.scan.min_tier.clear();
    c.scan.activity_window = DurationSpec::default();
    c
}

fn inside(p: &Path, base: &Path) -> bool {
    p.starts_with(base) && !p.components().any(|c| c == Component::ParentDir)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn repo_policy_never_reduces_protection(text in repo_toml(), base in base_config(), marker in any::<bool>()) {
        let repo = abs("/home/me/repo");
        let mut parsed = RepoPolicy::parse_toml(&text);
        if marker {
            parsed.policy.merge(&RepoPolicy::protect_marker());
        }
        let (after, _) = base.with_repo_policy(&repo, &parsed.policy);

        prop_assert!(after.scan.protect.is_superset(&base.scan.protect));
        prop_assert!(after.scan.exclude.is_superset(&base.scan.exclude));
        for (p, t) in &base.scan.min_tier {
            prop_assert!(after.scan.min_tier.get(p).is_some_and(|a| a >= t));
        }
        prop_assert!(after.scan.activity_window >= base.scan.activity_window);
        prop_assert_eq!(non_protection(&after), non_protection(&base));

        // Everything added stays inside the repository.
        for p in after.scan.protect.difference(&base.scan.protect) {
            prop_assert!(inside(p, &repo), "{}", p.display());
        }
        for p in after.scan.exclude.difference(&base.scan.exclude) {
            prop_assert!(inside(p, &repo), "{}", p.display());
        }
        for p in after.scan.min_tier.keys() {
            prop_assert!(base.scan.min_tier.contains_key(p) || inside(p, &repo));
        }
        // A repo policy never carries an S0 floor.
        prop_assert!(parsed.policy.min_tier().values().all(|t| *t > Tier::S0));
    }

    #[test]
    fn forbidden_repo_settings_always_warn(section in prop::sample::select(vec!["rules", "mcp", "forge", "output"]), key in "[a-z_]{1,10}", v in any::<bool>()) {
        let text = format!("[{section}]\n{key} = {v}\n");
        let r = RepoPolicy::parse_toml(&text);
        prop_assert!(r.policy.is_empty());
        let want = format!("{section}.{key}");
        prop_assert!(has_warning(&r.warnings, ConfigWarningCode::RepoForbiddenSetting, &want));
    }

    #[test]
    fn repo_rel_path_never_escapes(s in path_str()) {
        let base = abs("/home/me/repo");
        if let Ok(rp) = RepoRelPath::parse(&s) {
            let joined = rp.under(&base);
            prop_assert!(inside(&joined, &base), "{:?} -> {}", s, joined.display());
            prop_assert!(!rp.as_str().contains(':') && !rp.as_str().starts_with('~'));
        }
    }

    #[test]
    fn duration_display_round_trips(secs in 0u64..10_000_000_000) {
        let d = DurationSpec::from_secs(secs);
        prop_assert_eq!(d.to_string().parse::<DurationSpec>().unwrap(), d);
    }
}

// ---------------------------------------------------------------------------------------------
// P2-EXEC-13: where the quarantine lives.
// ---------------------------------------------------------------------------------------------

#[test]
fn quarantine_base_for_picks_the_longest_matching_prefix_then_the_global_base() {
    let mut q = QuarantineConfig::default();
    assert!(!q.is_configured());
    assert_eq!(q.base_for(&abs("/home/me/p/target")), None, "automatic");
    q.base = Some(abs("/home/me/.q"));
    q.bases.insert(abs("/vol"), abs("/vol/me/q"));
    q.bases.insert(abs("/vol/deep"), abs("/vol/deep/me/q"));
    assert!(q.is_configured());
    assert_eq!(
        q.base_for(&abs("/home/me/p/target")),
        Some(abs("/home/me/.q").as_path())
    );
    assert_eq!(
        q.base_for(&abs("/vol/p/target")),
        Some(abs("/vol/me/q").as_path())
    );
    assert_eq!(
        q.base_for(&abs("/vol/deep/p/target")),
        Some(abs("/vol/deep/me/q").as_path())
    );
    // Component-wise prefixes only: /volume is not under /vol.
    assert_eq!(
        q.base_for(&abs("/volume/p")),
        Some(abs("/home/me/.q").as_path())
    );
}

#[test]
fn quarantine_settings_parse_expand_home_and_reject_relative_env() {
    let l = ConfigLayer::from_toml(
        "[quarantine]\nbase = \"~/.q\"\n[quarantine.bases]\n\"~/vol\" = \"~/vol/q\"\n",
    )
    .unwrap();
    let r = resolve(&ConfigLayer::default(), &EnvLayer::default(), &l);
    let home = abs("/home/me");
    let c = r.config.with_home_expanded(&home);
    assert_eq!(c.quarantine.base, Some(home.join(".q")));
    assert_eq!(
        c.quarantine.bases.get(&home.join("vol")),
        Some(&home.join("vol").join("q"))
    );
    assert!(ConfigLayer::from_toml("[quarantine]\nbasis = \"/x\"\n").is_err());

    let e = EnvLayer::from_lookup(|k| (k == QUARANTINE_BASE_ENV).then(|| "relative/q".into()))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Config);
    assert!(e.message.contains(QUARANTINE_BASE_ENV), "{}", e.message);
}

#[test]
fn repository_files_cannot_move_the_quarantine() {
    let r = RepoPolicy::parse_toml("[quarantine]\nbase = \"/tmp/evil\"\n");
    assert!(r.policy.is_empty(), "{:?}", r.policy);
    assert!(
        r.warnings
            .iter()
            .any(|w| w.key.starts_with("quarantine")
                && w.message.contains("where the quarantine lives")),
        "{:?}",
        r.warnings
    );
}
