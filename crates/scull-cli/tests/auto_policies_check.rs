//! Auto-policies through the real `scull check` binary (P5-PRO-02; PRO-10, D-11, T-05).
//!
//! Every run points `SCULL_HOME` and `HOME` into the fixture's temp directory (beside the
//! root, never inside it) and gets the fixture's sandbox environment, so a quarantine or Trash
//! move can only land there. These tests cover what only the binary decides: policies are off
//! unless the user config file enables one, every kill switch stops them, a config file others
//! can write is not trusted, an elevated or scan-less check refuses, `scull clean` refuses an
//! auto-policy plan, and (where this machine's in-use probe can prove the build output idle, so
//! it is S0) the dry run, the run, the audit and `scull restore` end to end. The in-process
//! adversarial cases are in `tests/auto_policies.rs`. Every test checks the fixture
//! invariants.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{
    Action, Category, FileIdentity, FsPath, ItemId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams,
    RuleRef, ScanId, Tier,
};
use scull_fixtures::{Fixture, InvariantInputs, days_ago};
use scull_storage::{DefaultPermissions, Store};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Env {
    fx: Fixture,
}

impl Env {
    /// Two idle Cargo projects (90 days) and a config whose `scan.roots` is the fixture root,
    /// plus `extra` TOML.
    fn new(extra: &str) -> Self {
        let mut fx = Fixture::new().unwrap();
        for n in ["app", "lib"] {
            fx.cargo_project(n, n)
                .unwrap()
                .cargo_target(n, n, 16 * 1024)
                .unwrap()
                .age(n, days_ago(90))
                .unwrap();
        }
        fx.checkpoint().unwrap();
        std::fs::create_dir_all(fx.base().join("userhome")).unwrap();
        let env = Self { fx };
        env.write_config(extra);
        env
    }

    fn home(&self) -> PathBuf {
        self.fx.base().join("scull-home")
    }

    fn root(&self) -> String {
        scull_engine::paths::canon(self.fx.root())
            .to_string_lossy()
            .into_owned()
    }

    /// The quarantine base the config file names.
    fn file_quarantine(&self) -> PathBuf {
        self.fx.base().join("file-quarantine")
    }

    fn config_file(&self) -> PathBuf {
        self.home().join("config").join("config.toml")
    }

    fn state_file(&self) -> PathBuf {
        self.home()
            .join("data")
            .join(scull_cli::cmd::auto_policy::STATE_FILE)
    }

    /// The config file, writable only by its owner (the umask may allow group write).
    fn write_config(&self, extra: &str) {
        let dir = self.home().join("config");
        std::fs::create_dir_all(&dir).unwrap();
        // Policy runs quarantine only where the config file says (`SCULL_QUARANTINE_BASE`,
        // which the sandbox also sets, is ignored for them): a folder inside the fixture.
        let text = format!(
            "[scan]\nroots = [{}]\n\n[quarantine]\nbase = {}\n\n{extra}",
            toml_string(&self.root()),
            toml_string(&self.file_quarantine().to_string_lossy())
        );
        std::fs::write(self.config_file(), text).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(self.config_file(), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(args);
        let user_home = self.fx.base().join("userhome");
        c.current_dir(self.fx.base())
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("HOME", &user_home)
            .env("USERPROFILE", &user_home)
            .stdin(Stdio::null());
        for (k, _) in std::env::vars() {
            if k.starts_with("SCULL_") {
                c.env_remove(k);
            }
        }
        c.env("SCULL_HOME", self.home());
        self.fx.sandbox(&mut c);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// `scull check --json --no-progress` (plus `args`): the `auto_policies` part.
    fn check(&self, args: &[&str]) -> Value {
        let mut all = vec!["check", "--json", "--no-progress"];
        all.extend_from_slice(args);
        let v = json_ok(&self.run(&all));
        validate(&v);
        v["data"]["auto_policies"].clone()
    }

    fn invariants(&self, allowed: &[&str]) {
        let mut inputs = InvariantInputs::new();
        for a in allowed {
            inputs = inputs.allow(*a);
        }
        let r = self.fx.check_invariants(&inputs);
        println!("{r}");
        r.assert_ok();
    }

    fn store(&self) -> Store {
        Store::open(
            &self.home().join("data").join(scull_storage::DB_FILE_NAME),
            &DefaultPermissions,
        )
        .unwrap()
    }
}

const POLICY: &str = "[[auto.policy]]\nname = \"old-builds\"\nenabled = true\n\
                      kind = \"build_outputs\"\nidle = \"60d\"\nevery = \"7d\"\n";

fn toml_string(s: &str) -> String {
    format!("'{s}'")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn json_ok(o: &Output) -> Value {
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(o));
    serde_json::from_slice(&o.stdout).expect("JSON on stdout")
}

fn validate(v: &Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/check.schema.json");
    let s: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&s).unwrap();
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

fn policy_operations(store: &Store) -> usize {
    store
        .operations(0)
        .unwrap()
        .into_iter()
        .filter(|o| o.method == scull_storage::ApprovalMethod::AutoPolicy)
        .count()
}

// ------------------------------------------------------------------------------------------

/// Off by default: without an enabled policy the check reports `off`, prints nothing about
/// auto-policies, writes no state and starts no operation; the tree is unchanged.
#[test]
fn check_without_an_enabled_policy_is_off() {
    let env = Env::new("");
    let a = env.check(&[]);
    assert_eq!(a["state"], "off", "{a:#}");
    let env2 = Env::new(&POLICY.replace("enabled = true\n", ""));
    assert_eq!(env2.check(&[])["state"], "off");
    let text = String::from_utf8(env.run(&["check", "--no-progress"]).stdout).unwrap();
    assert!(!text.contains("auto-polic"), "{text}");
    for e in [&env, &env2] {
        assert!(!e.state_file().exists());
        assert_eq!(policy_operations(&e.store()), 0);
        e.invariants(&[]);
    }
}

/// Every kill switch stops an enabled policy before anything is decided: `paused = true`,
/// the `auto-policies.off` file, `SCULL_NO_AUTO_POLICIES`, `--no-auto-policies`.
#[test]
fn kill_switches_stop_enabled_policies() {
    let env = Env::new(&format!("[auto]\npaused = true\n\n{POLICY}"));
    let a = env.check(&[]);
    assert_eq!(a["state"], "stopped", "{a:#}");
    assert!(a["reason"].as_str().unwrap().contains("paused"));
    assert!(!env.state_file().exists());
    env.invariants(&[]);

    let env = Env::new(POLICY);
    std::fs::write(env.home().join("config").join("auto-policies.off"), "").unwrap();
    let a = env.check(&[]);
    assert_eq!(a["state"], "stopped", "{a:#}");
    assert!(a["reason"].as_str().unwrap().contains("auto-policies.off"));
    std::fs::remove_file(env.home().join("config").join("auto-policies.off")).unwrap();

    let o = env
        .cmd(&["check", "--json", "--no-progress"])
        .env("SCULL_NO_AUTO_POLICIES", "1")
        .output()
        .unwrap();
    let a = &json_ok(&o)["data"]["auto_policies"];
    assert_eq!(a["state"], "stopped", "{a:#}");
    assert!(
        a["reason"]
            .as_str()
            .unwrap()
            .contains("SCULL_NO_AUTO_POLICIES")
    );

    let a = env.check(&["--no-auto-policies"]);
    assert_eq!(a["state"], "stopped", "{a:#}");
    let text = String::from_utf8(
        env.run(&["check", "--no-progress", "--no-auto-policies"])
            .stdout,
    )
    .unwrap();
    assert!(
        text.contains("auto-policies: stopped by a kill switch"),
        "{text}"
    );

    assert!(
        !env.state_file().exists(),
        "a stopped check decides nothing"
    );
    assert_eq!(policy_operations(&env.store()), 0);
    env.invariants(&[]);
}

/// A config file (or its folder) that another account can write is not trusted: an enabled
/// policy in it never runs, not even as a dry run.
#[cfg(unix)]
#[test]
fn a_config_others_can_write_is_not_trusted() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new(POLICY);
    for (path, mode) in [
        (env.config_file(), 0o664),
        (env.config_file(), 0o602),
        (env.home().join("config"), 0o777),
    ] {
        let before = std::fs::metadata(&path).unwrap().permissions();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let a = env.check(&[]);
        assert_eq!(a["state"], "refused", "{mode:o}: {a:#}");
        assert!(
            a["reason"].as_str().unwrap().contains("can't be trusted"),
            "{a:#}"
        );
        std::fs::set_permissions(&path, before).unwrap();
    }
    // Readable by others is fine (a config holds no secret): it previews.
    std::fs::set_permissions(env.config_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(env.check(&[])["state"], "checked");
    env.invariants(&[]);
}

/// SAFE-14: an unattended run never runs elevated, and there is no `--allow-root` for it. A
/// check that did not scan (`--no-scan`), or whose scan an environment variable widened
/// (`SCULL_ROOTS`, `SCULL_ACTIVITY_WINDOW`), refuses too.
#[test]
fn elevated_or_scanless_checks_refuse() {
    let env = Env::new(POLICY);
    let o = env
        .cmd(&["check", "--json", "--no-progress"])
        .env("SCULL_TEST_ASSUME_ELEVATED", "1")
        .output()
        .unwrap();
    let a = &json_ok(&o)["data"]["auto_policies"];
    assert_eq!(a["state"], "refused", "{a:#}");
    assert!(a["reason"].as_str().unwrap().contains("SAFE-14"));
    let a = env.check(&["--no-scan"]);
    assert_eq!(a["state"], "refused", "{a:#}");
    // The environment can't widen what policies act on.
    for (var, value) in [
        ("SCULL_ROOTS", env.root()),
        ("SCULL_ACTIVITY_WINDOW", "1h".into()),
    ] {
        let o = env
            .cmd(&["check", "--json", "--no-progress"])
            .env(var, &value)
            .output()
            .unwrap();
        let a = &json_ok(&o)["data"]["auto_policies"];
        assert_eq!(a["state"], "refused", "{var}: {a:#}");
        assert!(a["reason"].as_str().unwrap().contains(var), "{a:#}");
    }
    assert!(!env.state_file().exists());
    env.invariants(&[]);
}

/// `scull clean --plan` never runs an auto-policy plan (it runs only from `scull check`).
#[test]
fn clean_refuses_an_auto_policy_plan() {
    let env = Env::new("");
    // Creates the database and the machine ID.
    env.check(&["--no-scan"]);
    let machine = scull_platform::machine_id(&env.home().join("data")).unwrap();
    let identity = FileIdentity::new(1, 2);
    let path = format!("{}/app/target", env.root());
    let item = PlanItem {
        item_id: ItemId::derive(identity, path.as_bytes()),
        identity,
        path: FsPath::from(path.as_str()),
        category: Category::ProjectArtifact,
        rule: RuleRef {
            id: "rust.target".into(),
            version: 1,
        },
        tier: Tier::S0,
        action: Action::Quarantine,
        reclaimable_bytes: 1,
    };
    let plan = Plan::new(
        PlanParams {
            id: PlanId::from_suffix("01POLICYPLAN").unwrap(),
            scan_id: ScanId::from_suffix("01SCAN").unwrap(),
            created_unix: scull_engine::now_unix(),
            ttl_secs: None,
            machine_id: machine,
            user: scull_platform::current_user().unwrap(),
            origin: PlanOrigin::Policy,
            operations_watermark: None,
        },
        vec![item],
    )
    .unwrap();
    env.store().insert_plan(&plan).unwrap();
    let file = env.fx.base().join("policy-plan.json");
    std::fs::write(&file, serde_json::to_vec(&plan).unwrap()).unwrap();
    let mut args = vec![
        "clean",
        "--plan",
        file.to_str().unwrap(),
        "--confirm",
        "--json",
    ];
    if scull_platform::is_elevated().blocks_destructive() {
        args.push("--allow-root");
    }
    let o = env.run(&args);
    assert_eq!(o.status.code(), Some(4), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("auto-policy"), "{}", stderr(&o));
    env.invariants(&[]);
}

/// Set in the child [`rerun_in_pid_namespace`] starts.
const PID_NS_ENV: &str = "SCULL_TEST_PID_NAMESPACE";

/// Re-run `test` alone in a new PID namespace (`sudo -n unshare --pid --fork --mount-proc`),
/// dropped back to this user with `setpriv`, and assert it passed. Only on disposable CI
/// runners (`SCULL_FIXTURES_ALLOW_SUDO=1`); the child never runs as root (scull refuses to
/// change anything elevated, SAFE-14).
#[cfg(target_os = "linux")]
fn rerun_in_pid_namespace(test: &str) {
    let id = |flag: &str| {
        let o = Command::new("id").arg(flag).output().unwrap();
        String::from_utf8(o.stdout).unwrap().trim().to_owned()
    };
    let (uid, gid) = (id("-u"), id("-g"));
    let mut c = Command::new("sudo");
    c.args([
        "-n",
        "unshare",
        "--pid",
        "--fork",
        "--mount-proc",
        "setpriv",
    ])
    .arg(format!("--reuid={uid}"))
    .arg(format!("--regid={gid}"))
    .args(["--init-groups", "env"])
    .arg(format!("{PID_NS_ENV}=1"));
    for k in [
        "PATH",
        "HOME",
        "TMPDIR",
        "CI",
        "RUST_BACKTRACE",
        "CARGO_TERM_COLOR",
    ] {
        if let Some(v) = std::env::var_os(k) {
            let mut kv = std::ffi::OsString::from(format!("{k}="));
            kv.push(v);
            c.arg(kv);
        }
    }
    c.arg(std::env::current_exe().unwrap()).args([
        test,
        "--exact",
        "--nocapture",
        "--test-threads",
        "1",
    ]);
    let o = c.output().unwrap();
    println!("{}", String::from_utf8_lossy(&o.stdout));
    eprintln!("{}", String::from_utf8_lossy(&o.stderr));
    assert!(
        o.status.success(),
        "{test} in a private PID namespace failed"
    );
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("1 passed"),
        "{test} did not run in the PID namespace"
    );
}

/// Set by CI's standard-user run of this test binary on Windows
/// (`scripts/ci/run-as-standard-user.ps1`): an elevated process then fails instead of skipping.
const REQUIRE_NOT_ELEVATED_ENV: &str = "SCULL_TEST_REQUIRE_NOT_ELEVATED";

/// Elevated, scull refuses (SAFE-14), so `test`'s non-elevated part stops here. GitHub's Windows
/// runners are elevated; CI runs this binary again as a standard user with
/// [`REQUIRE_NOT_ELEVATED_ENV`] set, where the skip is a failure and a `RUN` marker is printed.
fn skip_if_elevated(test: &str, why: &str) -> bool {
    let required = std::env::var_os(REQUIRE_NOT_ELEVATED_ENV).is_some_and(|v| !v.is_empty());
    if scull_platform::is_elevated().blocks_destructive() {
        assert!(
            !required,
            "{test}: {REQUIRE_NOT_ELEVATED_ENV} is set but this process runs elevated"
        );
        eprintln!("SKIP {test}: running elevated, {why} (SAFE-14)");
        return true;
    }
    if required {
        eprintln!("RUN {test}: not elevated (standard user)");
    }
    false
}

/// Whether this machine's in-use probe lets the fixture's build outputs be S0.
fn has_s0(env: &Env) -> bool {
    let o = env.run(&["scan", "--no-history", "--json", &env.root()]);
    let v: Value = serde_json::from_slice(&o.stdout).expect("scan JSON");
    v["data"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["tier"] == "S0")
}

/// FX-EXEC-15 end to end: the first check is a dry run (recorded, nothing touched); a check
/// one interval later quarantines the idle S0 build outputs in one audited auto-policy
/// operation that `scull history` lists and `scull restore` undoes. It quarantines in the
/// config file's location, never `SCULL_QUARANTINE_BASE`'s. On CI this must not skip.
#[test]
fn fx_exec_15_check_previews_then_quarantines_and_restore_undoes_it() {
    // CI (Linux, `SCULL_FIXTURES_ALLOW_SUDO=1`): hosted runners have processes the user can't
    // inspect, so "in use" is unknown and nothing is S0. Run this test in a fresh PID
    // namespace, back as the normal user, where every process is inspectable, and require the
    // run there.
    #[cfg(target_os = "linux")]
    if std::env::var_os(PID_NS_ENV).is_none()
        && std::env::var_os(scull_fixtures::privileged::ALLOW_SUDO_ENV).is_some_and(|v| v == "1")
    {
        rerun_in_pid_namespace("fx_exec_15_check_previews_then_quarantines_and_restore_undoes_it");
        return;
    }
    if skip_if_elevated(
        "fx_exec_15_check_previews_then_quarantines_and_restore_undoes_it",
        "so auto-policies refuse",
    ) {
        return;
    }
    let env = Env::new(POLICY);
    let a = env.check(&[]);
    assert_eq!(a["state"], "checked", "{a:#}");
    let p = &a["policies"][0];
    assert_eq!(p["name"], "old-builds");
    assert_eq!(p["outcome"]["state"], "previewed", "{p:#}");
    assert!(env.state_file().is_file());
    env.invariants(&[]);
    let text = String::from_utf8(env.run(&["check", "--no-progress"]).stdout).unwrap();
    assert!(
        text.contains("auto-policy old-builds: next run after"),
        "{text}"
    );

    if !has_s0(&env) {
        // This machine can't prove the build outputs idle (an uninspectable process makes
        // "in use" unknown, hence S2): nothing qualifies. `tests/auto_policies.rs` runs the
        // same path in process with a stand-in probe.
        assert!(
            std::env::var_os(PID_NS_ENV).is_none(),
            "no S0 candidate even in a private PID namespace: the end-to-end auto-policy run \
             can't happen here"
        );
        assert_eq!(p["outcome"]["items"], 0);
        eprintln!("no S0 candidate on this machine; end-to-end run skipped");
        return;
    }
    assert_eq!(p["outcome"]["items"], 2, "{p:#}");

    // Move the dry run one interval into the past (the state file stays owner-only).
    let mut state: Value =
        serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
    let at = state["policies"]["old-builds"]["previewed_at"]
        .as_i64()
        .unwrap();
    state["policies"]["old-builds"]["previewed_at"] = Value::from(at - 8 * 86_400);
    std::fs::write(env.state_file(), serde_json::to_vec(&state).unwrap()).unwrap();

    let a = env.check(&[]);
    let out = &a["policies"][0]["outcome"];
    assert_eq!(out["state"], "ran", "{a:#}");
    assert_eq!(out["quarantined"], 2, "{a:#}");
    let op = out["operation_id"].as_str().unwrap().to_owned();
    assert!(!env.fx.root().join("app/target").exists());
    assert!(!env.fx.root().join("lib/target").exists());
    // Quarantined where the config file says, not where SCULL_QUARANTINE_BASE points.
    assert!(
        env.file_quarantine()
            .join(scull_cleaner::QUARANTINE_DIR_NAME)
            .is_dir()
    );
    assert!(
        !env.fx
            .quarantine_base()
            .join(scull_cleaner::QUARANTINE_DIR_NAME)
            .exists()
    );
    let store = env.store();
    assert_eq!(policy_operations(&store), 1);
    let op_id = scull_core::OperationId::try_from(op.clone()).unwrap();
    let r = env.fx.check_invariants(
        &InvariantInputs::new()
            .allow("app/target")
            .allow("lib/target")
            .audit(&store, &op_id)
            .quarantine(&store),
    );
    println!("{r}");
    r.assert_ok();
    drop(store);

    let h = json_ok(&env.run(&["history", &op, "--json"]));
    assert_eq!(h["data"]["operation"]["method"], "auto_policy", "{h:#}");

    let o = env.run(&["restore", &op, "--confirm", "--json", "--no-progress"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    assert!(env.fx.root().join("app/target/debug").is_dir());
    assert!(env.fx.root().join("lib/target/debug").is_dir());
}
