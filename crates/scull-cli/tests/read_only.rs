//! End-to-end tests of the read-only commands (P1-CLI-02): `scan`, `projects [inspect]`,
//! `worktrees [inspect]` and `caches` over generated fixtures. Every `--json` document
//! validates against its committed schema (and every candidate against
//! `schemas/v1/candidate.schema.json`), every `--ndjson` line parses and validates, history is
//! recorded unless `--no-history`, broad roots are refused (CFG-04), and nothing on disk
//! changes (I-1 no actionable S3, I-2 canary, I-3 root unchanged).
//!
//! Regenerate the committed schemas with `SCULL_BLESS=1 cargo test -p scull-cli --test read_only`.

#![allow(clippy::unwrap_used)]

use scull_fixtures::{Fixture, InvariantInputs, NodeLock};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Environment variables that point package managers (and so `scull caches`) elsewhere.
const CACHE_VARS: &[&str] = &[
    "npm_config_cache",
    "npm_config_userconfig",
    "npm_config_store_dir",
    "NPM_CONFIG_CACHE",
    "PNPM_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "BUN_INSTALL",
    "BUN_INSTALL_CACHE_DIR",
    "YARN_CACHE_FOLDER",
    "YARN_GLOBAL_FOLDER",
    "YARN_ENABLE_GLOBAL_CACHE",
    "PIP_CACHE_DIR",
    "PIP_CONFIG_FILE",
    "UV_CACHE_DIR",
    "UV_CONFIG_FILE",
    "CONDA_PKGS_DIRS",
    "CONDARC",
    "GOCACHE",
    "GOENV",
    "GOMODCACHE",
    "GOPATH",
    "GRADLE_USER_HOME",
    "MAVEN_OPTS",
    "MAVEN_ARGS",
    "NUGET_PACKAGES",
    "CARGO_TARGET_DIR",
];

struct Env {
    scull_home: tempfile::TempDir,
    _home_dir: Option<tempfile::TempDir>,
    home: PathBuf,
}

impl Env {
    /// `home` as the user's home folder.
    fn new(home: PathBuf) -> Self {
        Self {
            scull_home: tempfile::tempdir().unwrap(),
            _home_dir: None,
            home,
        }
    }

    /// An empty home folder outside every fixture (so fixture roots are not "broad").
    fn elsewhere() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            scull_home: tempfile::tempdir().unwrap(),
            home: dir.path().to_path_buf(),
            _home_dir: Some(dir),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_scull"));
        c.args(args);
        // Temp dirs are tmpfs on some machines; the D-22 volume policy is tested in
        // scull-platform.
        if matches!(args.first(), Some(&("scan" | "projects" | "worktrees"))) {
            c.arg("--allow-network-roots");
        }
        c.env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env("SCULL_HOME", self.scull_home.path())
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join("AppData").join("Local"))
            .env("APPDATA", self.home.join("AppData").join("Roaming"));
        for v in CACHE_VARS {
            c.env_remove(v);
        }
        c.output().unwrap()
    }

    fn db(&self) -> Option<PathBuf> {
        find(self.scull_home.path(), "scull.db")
    }
}

fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if e.file_name() == name {
            return Some(p);
        }
        if e.file_type().ok()?.is_dir()
            && let Some(f) = find(&p, name)
        {
            return Some(f);
        }
    }
    None
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn ok(o: &Output) {
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(o));
}

fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas")
}

fn validator(path: &Path) -> jsonschema::Validator {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    jsonschema::validator_for(&v).unwrap()
}

fn candidate_validator() -> jsonschema::Validator {
    validator(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/v1/candidate.schema.json"),
    )
}

fn assert_valid(v: &jsonschema::Validator, doc: &Value, what: &str) {
    let errors: Vec<String> = v.iter_errors(doc).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{what}: {errors:#?}\n{doc:#}");
}

/// `--json`: one document valid against `<name>.schema.json`; every candidate valid too.
fn json(env: &Env, args: &[&str], name: &str) -> Value {
    let mut a = args.to_vec();
    a.push("--json");
    let o = env.run(&a);
    ok(&o);
    let doc: Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(doc["schema"], format!("scull/{name}@1"));
    assert_valid(
        &validator(&schema_dir().join(format!("{name}.schema.json"))),
        &doc,
        name,
    );
    let cv = candidate_validator();
    if let Some(cands) = doc["data"]["candidates"].as_array() {
        for c in cands {
            assert_valid(&cv, c, "candidate");
        }
    }
    doc
}

/// `--ndjson`: every line parses and is valid against `<name>.ndjson.schema.json`.
fn ndjson(env: &Env, args: &[&str], name: &str) -> Vec<Value> {
    let mut a = args.to_vec();
    a.push("--ndjson");
    let o = env.run(&a);
    ok(&o);
    let v = validator(&schema_dir().join(format!("{name}.ndjson.schema.json")));
    let cv = candidate_validator();
    let lines: Vec<Value> = stdout(&o)
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect();
    for l in &lines {
        assert_eq!(l["schema"], format!("scull/{name}@1"));
        assert_valid(&v, l, &format!("{name} line"));
        if l["type"] == "candidate" {
            assert_valid(&cv, &l["data"], "candidate line");
        }
    }
    lines
}

/// Cargo + npm (with and without lockfile) + pnpm workspace + `.scull-protect` project, and a
/// Git repository with a committed `target/` and a linked worktree when `git` exists.
fn tree(with_git: bool) -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 4096)
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .npm_project("nolock", "nolock", NodeLock::None)
        .unwrap()
        .node_modules("nolock", &["x"])
        .unwrap()
        .pnpm_workspace("mono", "mono", &["a"])
        .unwrap()
        .node_modules("mono/packages/a", &["lodash"])
        .unwrap()
        .cargo_project("vault", "vault")
        .unwrap()
        .cargo_target("vault", "vault", 1024)
        .unwrap()
        .file("vault/.scull-protect", "")
        .unwrap();
    if with_git {
        let repo = fx.git_repo("repo").unwrap();
        fx.file("repo/.gitignore", "node_modules/\n")
            .unwrap()
            .cargo_project("repo/tracked", "tracked")
            .unwrap()
            .cargo_target("repo/tracked", "tracked", 512)
            .unwrap();
        repo.commit_all("initial").unwrap();
        repo.run(["worktree", "add", "--quiet", "-b", "feature", "../repo-wt"])
            .unwrap();
    }
    fx.checkpoint().unwrap();
    fx
}

fn root(fx: &Fixture) -> String {
    fx.root().to_string_lossy().into_owned()
}

/// I-1 over a document's candidates (S3 never actionable), and I-2/I-3 (nothing changed).
fn assert_read_only(fx: &Fixture, docs: &[&Value]) {
    for doc in docs {
        if let Some(cands) = doc["data"]["candidates"].as_array() {
            for c in cands {
                if c["tier"] == "S3" {
                    assert_eq!(
                        c["action"]["type"], "none",
                        "S3 must not be actionable: {c:#}"
                    );
                }
            }
        }
    }
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

fn rel_paths(doc: &Value, fx: &Fixture) -> Vec<String> {
    let base = scull_engine::paths::canon(fx.root());
    let mut v: Vec<String> = doc["data"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let p = PathBuf::from(c["path"].as_str().unwrap());
            p.strip_prefix(&base)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    v.sort();
    v
}

#[test]
fn scan_human_json_ndjson_and_history() {
    let with_git = git_available();
    let fx = tree(with_git);
    let env = Env::elsewhere();
    let r = root(&fx);

    // Human table: tier labels, labeled sizes, escaped paths, totals, signals.
    let o = env.run(&["scan", &r, "--no-history"]);
    ok(&o);
    let out = stdout(&o);
    for want in [
        "Tier",
        "Reclaimable",
        "Allocated",
        "Logical",
        "S3 protected",
        "Totals by tier:",
        "Nothing was changed",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    // Confidence is summarized once (`estimate_note`), not a column (#138).
    assert!(!out.contains("Confidence"), "{out}");
    // Paths are shown relative to the single root, with the OS separator.
    let app_target = Path::new("app").join("target");
    assert!(out.contains(&*app_target.to_string_lossy()), "{out}");
    assert!(
        env.db().is_none(),
        "--no-history must not create a database"
    );

    // JSON document.
    let doc = json(&env, &["scan", &r, "--no-history"], "scan");
    let paths = rel_paths(&doc, &fx);
    for want in [
        "app/target",
        "web/node_modules",
        "nolock/node_modules",
        "mono/packages/a/node_modules",
        "vault/target",
    ] {
        assert!(paths.contains(&want.to_owned()), "{want} not in {paths:?}");
    }
    let tier_of = |rel: &str| {
        let base = scull_engine::paths::canon(fx.root());
        let want = base.join(rel);
        doc["data"]["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| Path::new(c["path"].as_str().unwrap()) == want)
            .map(|c| c["tier"].as_str().unwrap().to_owned())
            .unwrap()
    };
    assert_eq!(tier_of("vault/target"), "S3", "FX-CLS-11");
    assert_ne!(tier_of("nolock/node_modules"), "S0", "FX-CLS-05");
    if with_git {
        assert_eq!(tier_of("repo/tracked/target"), "S3", "FX-CLS-03");
        assert!(paths.contains(&"repo-wt".to_owned()), "{paths:?}");
        assert!(
            doc["data"]["worktrees"].as_array().unwrap().len() >= 2,
            "main + linked worktree"
        );
    }
    assert_eq!(doc["data"]["cancelled"], false);
    assert_eq!(doc["data"]["history"]["state"], "disabled");
    assert!(env.db().is_none());

    // NDJSON: candidates then one summary.
    let lines = ndjson(&env, &["scan", &r, "--no-history"], "scan");
    let (last, cands) = lines.split_last().unwrap();
    assert_eq!(last["type"], "summary");
    assert!(cands.iter().all(|l| l["type"] == "candidate"));
    assert_eq!(last["data"]["candidate_count"], cands.len() as u64);
    assert_eq!(cands.len(), paths.len());

    // History is recorded by default, in SCULL_HOME.
    let recorded = json(&env, &["scan", &r], "scan");
    assert_eq!(recorded["data"]["history"]["state"], "recorded");
    assert!(env.db().is_some(), "history database created");

    assert_read_only(&fx, &[&doc, &recorded]);
}

#[test]
fn projects_and_project_inspect() {
    let fx = tree(false);
    let env = Env::elsewhere();
    let r = root(&fx);
    let o = env.run(&["projects", "--root", &r]);
    ok(&o);
    let out = stdout(&o);
    assert!(
        out.contains("Project") && out.contains("Badge") && out.contains("rust"),
        "{out}"
    );

    let doc = json(&env, &["projects", "--root", &r], "projects");
    let names: Vec<&str> = doc["data"]["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    for want in ["app", "web", "nolock", "mono", "a", "vault"] {
        assert!(names.contains(&want), "{want} not in {names:?}");
    }
    let lines = ndjson(&env, &["projects", "--root", &r], "projects");
    assert_eq!(lines.last().unwrap()["type"], "summary");

    let app = fx.root().join("app");
    let app = app.to_string_lossy();
    let o = env.run(&["projects", "inspect", &app]);
    ok(&o);
    let out = stdout(&o);
    assert!(
        out.contains("Project app") && out.contains("marker") && out.contains("regenerate:"),
        "{out}"
    );
    let doc = json(&env, &["projects", "inspect", &app], "project");
    assert_eq!(doc["data"]["project"]["name"], "app");
    assert_eq!(doc["data"]["candidates"].as_array().unwrap().len(), 1);
    let lines = ndjson(&env, &["projects", "inspect", &app], "project");
    assert_eq!(lines.len(), 2);
    assert!(env.db().is_none(), "projects never records history");
    assert_read_only(&fx, &[&doc]);
}

#[test]
fn worktrees_and_worktree_inspect() {
    if !git_available() {
        eprintln!("skipping: `git` not found");
        return;
    }
    let fx = tree(true);
    let env = Env::elsewhere();
    let r = root(&fx);
    let o = env.run(&["worktrees", "--root", &r]);
    ok(&o);
    let out = stdout(&o);
    assert!(
        out.contains("main") && out.contains("linked") && out.contains("feature"),
        "{out}"
    );
    assert!(out.contains("never removable"), "{out}");

    let doc = json(&env, &["worktrees", "--root", &r], "worktrees");
    let wts = doc["data"]["worktrees"].as_array().unwrap();
    assert_eq!(wts.len(), 2, "{doc:#}");
    let linked = wts.iter().find(|w| w["kind"] == "linked").unwrap();
    assert_ne!(
        linked["decision"]["tier"], "S0",
        "GIT-04: a present worktree is never S0"
    );
    let main = wts.iter().find(|w| w["kind"] == "main").unwrap();
    assert_eq!(main["decision"]["tier"], "S3");
    assert!(main["candidate"].is_null());
    let lines = ndjson(&env, &["worktrees", "--root", &r], "worktrees");
    assert_eq!(lines.len(), 3);

    let wt = fx.root().join("repo-wt");
    let wt = wt.to_string_lossy();
    let o = env.run(&["worktrees", "inspect", &wt]);
    ok(&o);
    assert!(
        stdout(&o).contains("kind:         linked"),
        "{}",
        stdout(&o)
    );
    let one = json(&env, &["worktrees", "inspect", &wt], "worktree");
    assert_eq!(one["data"]["kind"], "linked");
    let lines = ndjson(&env, &["worktrees", "inspect", &wt], "worktree");
    assert_eq!(lines.len(), 1);
    assert_read_only(&fx, &[]);
}

#[test]
fn caches_lists_located_caches() {
    let mut fx = Fixture::new().unwrap();
    // npm's layout at its Unix default (`~/.npm`) and Windows default
    // (`%LocalAppData%\npm-cache`).
    for base in ["home/.npm", "home/AppData/Local/npm-cache"] {
        fx.file(format!("{base}/_cacache/index-v5/1a/2b/3c4d"), "{}\n")
            .unwrap()
            .file_of_size(
                format!("{base}/_cacache/content-v2/sha512/ab/cd/ef01"),
                4096,
            )
            .unwrap();
    }
    fx.checkpoint().unwrap();
    let env = Env::new(fx.root().join("home"));
    let o = env.run(&["caches"]);
    ok(&o);
    let out = stdout(&o);
    assert!(
        out.contains("npm cache") && out.contains("npm cache verify"),
        "{out}"
    );
    let doc = json(&env, &["caches"], "caches");
    let cands = doc["data"]["candidates"].as_array().unwrap();
    assert_eq!(cands.len(), 1, "{doc:#}");
    assert_eq!(cands[0]["category"], "package_cache");
    assert_ne!(cands[0]["tier"], "S0");
    let cmds = doc["data"]["commands"].as_array().unwrap();
    assert_eq!(cmds.len(), 1, "{doc:#}");
    assert_eq!(cmds[0]["id"], cands[0]["id"]);
    assert_eq!(cmds[0]["rule"], "npm.cache");
    assert_eq!(cmds[0]["command"], "npm cache verify");
    let status = cmds[0]["status"].as_str().unwrap();
    assert!(matches!(status, "offered" | "not_offered"), "{doc:#}");
    // `commands` is additive: output from before #286 (without it) still validates.
    let mut older = doc.clone();
    older["data"].as_object_mut().unwrap().remove("commands");
    assert_valid(
        &validator(&schema_dir().join("caches.schema.json")),
        &older,
        "caches@1 without commands",
    );
    let lines = ndjson(&env, &["caches"], "caches");
    assert_eq!(lines.last().unwrap()["type"], "summary");
    assert_eq!(lines.last().unwrap()["data"]["candidate_count"], 1);
    assert_eq!(
        lines.last().unwrap()["data"]["commands"],
        doc["data"]["commands"]
    );
    assert_read_only(&fx, &[&doc]);
}

/// #286: with nothing located, the message names every tool scull looks for (`CacheTool::ALL`),
/// not just the first four; the machine output has no commands.
#[test]
fn caches_empty_state_names_every_tool() {
    let env = Env::elsewhere();
    let o = env.run(&["caches"]);
    ok(&o);
    let out = stdout(&o);
    assert!(out.contains("No package-manager caches found."), "{out}");
    for tool in scull_detectors::CacheTool::ALL {
        assert!(out.contains(tool.artifact()), "{tool:?} missing: {out}");
    }
    for name in [
        "Go module cache",
        "Gradle caches",
        "Maven local repository",
        "NuGet",
    ] {
        assert!(out.contains(name), "{name} missing: {out}");
    }
    let doc = json(&env, &["caches"], "caches");
    assert_eq!(doc["data"]["candidates"], serde_json::json!([]));
    assert_eq!(doc["data"]["commands"], serde_json::json!([]));
    let lines = ndjson(&env, &["caches"], "caches");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert_eq!(lines[0]["type"], "summary");
    assert_eq!(lines[0]["data"]["commands"], serde_json::json!([]));
}

/// #286: inspect-only caches (Gradle, Maven: S3, no native command) say so on the commands
/// line, in the human report and in `--json` / `--ndjson`, instead of "unknown (not offered
/// at this tier)".
#[test]
fn caches_inspect_only_commands_line() {
    let mut fx = Fixture::new().unwrap();
    let gradle = "home/.gradle/caches";
    fx.file_of_size(
        format!("{gradle}/modules-2/files-2.1/com.google.guava/guava/33.0.0-jre/abc/guava.jar"),
        4096,
    )
    .unwrap()
    .file(format!("{gradle}/modules-2/modules-2.lock"), "")
    .unwrap()
    .file_of_size(
        format!("{gradle}/transforms-4/0f1e/transformed/x.jar"),
        1024,
    )
    .unwrap()
    .file_of_size(format!("{gradle}/build-cache-1/0123abcd"), 1024)
    .unwrap()
    .file(format!("{gradle}/8.10/kotlin-dsl/accessors.txt"), "x")
    .unwrap();
    let maven = "home/.m2/repository/org/apache/commons/commons-lang3/3.14.0";
    fx.file_of_size(format!("{maven}/commons-lang3-3.14.0.jar"), 4096)
        .unwrap()
        .file(format!("{maven}/commons-lang3-3.14.0.pom"), "<project/>")
        .unwrap()
        .file(format!("{maven}/_remote.repositories"), "central\n")
        .unwrap();
    fx.checkpoint().unwrap();
    let env = Env::new(fx.root().join("home"));
    let o = env.run(&["caches"]);
    ok(&o);
    let out = stdout(&o);
    for artifact in ["Gradle caches", "Maven local repository"] {
        assert!(
            out.contains(&format!(
                "  {artifact}: none (inspect-only: no native cleanup command)\n"
            )),
            "{out}"
        );
    }
    assert!(!out.contains(": unknown"), "{out}");
    assert!(!out.contains("not offered at this tier"), "{out}");

    let doc = json(&env, &["caches"], "caches");
    let cands = doc["data"]["candidates"].as_array().unwrap();
    let cmds = doc["data"]["commands"].as_array().unwrap();
    assert_eq!(cands.len(), 2, "{doc:#}");
    assert_eq!(cmds.len(), 2, "{doc:#}");
    let mut rules = Vec::new();
    for (c, k) in cands.iter().zip(cmds) {
        assert_eq!(c["tier"], "S3", "{c:#}");
        assert_eq!(c["action"]["type"], "none", "{c:#}");
        assert_eq!(k["id"], c["id"]);
        assert_eq!(k["rule"], c["rule"]["id"]);
        assert_eq!(k["status"], "inspect_only", "{k:#}");
        assert_eq!(k["command"], Value::Null, "{k:#}");
        assert_eq!(k["text"], "none (inspect-only: no native cleanup command)");
        rules.push(k["rule"].as_str().unwrap().to_owned());
    }
    rules.sort();
    assert_eq!(rules, ["gradle.caches", "maven.repository"]);

    let lines = ndjson(&env, &["caches"], "caches");
    let summary = lines.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["data"]["commands"], doc["data"]["commands"]);
    assert_read_only(&fx, &[&doc]);
}

#[test]
fn broad_and_missing_roots_are_refused() {
    let mut fx = tree(false);
    fx.file("home/.keep", "").unwrap();
    fx.checkpoint().unwrap();
    let home = fx.root().join("home");
    let env = Env::new(home.clone());
    // CFG-04: the home folder itself needs the explicit flag (exit 5, blocked by policy).
    let h = home.to_string_lossy();
    let o = env.run(&["scan", &h, "--no-history"]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    assert!(stderr(&o).contains("--allow-broad-roots"), "{}", stderr(&o));
    // A folder containing home too.
    let o = env.run(&["scan", &root(&fx), "--no-history"]);
    assert_eq!(o.status.code(), Some(5), "{}", stderr(&o));
    let o = env.run(&["scan", &h, "--no-history", "--allow-broad-roots"]);
    ok(&o);
    // No roots at all: usage error.
    let o = env.run(&["scan"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("no scan roots"), "{}", stderr(&o));
    // A root that doesn't exist.
    let missing = fx.root().join("nope");
    let o = env.run(&["scan", &missing.to_string_lossy(), "--json"]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let err: Value = serde_json::from_str(&stderr(&o)).unwrap();
    assert_eq!(err["error"]["kind"], "not_found");
    assert!(o.stdout.is_empty());
    assert!(env.db().is_none());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// The committed schemas match the generated ones.
#[test]
fn committed_schemas_are_current() {
    let bless = std::env::var_os("SCULL_BLESS").is_some();
    for (file, schema) in scull_cli::cmd::schemas::all() {
        let path = schema_dir().join(&file);
        let text = serde_json::to_string_pretty(&schema).unwrap() + "\n";
        if bless {
            std::fs::write(&path, &text).unwrap();
        }
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{file} missing; run with SCULL_BLESS=1"));
        assert_eq!(
            committed.replace("\r\n", "\n"),
            text,
            "{file} is stale; rerun with SCULL_BLESS=1"
        );
        jsonschema::validator_for(&schema).unwrap();
    }
}
