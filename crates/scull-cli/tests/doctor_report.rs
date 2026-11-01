//! `scull doctor --report` (#248): FX-DATA-07, report half. "Audit logs and `doctor --report`
//! contain no file contents or environment values; report shows a redaction preview" (T-56,
//! INS-05, INS-07).
//!
//! Every run points `HOME`, `USERPROFILE` and `SCULL_HOME` at a fresh temp dir; the only file a
//! run may create is the `--out` report inside it.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Secrets planted in the environment, the config file and folder names; none may reach the
/// report.
const ENV_SECRET: &str = "sk-live-ENVSECRET-4417";
const UNRELATED_ENV_SECRET: &str = "ghp_UNRELATEDENVSECRET9";
const FILE_SECRET: &str = "ghp_FILECONTENTSECRET77";
const USER_SECRET: &str = "secretuser8812";
const NAME_SECRET: &str = "acme-secret-client";

/// `scull` with `HOME`/`USERPROFILE` at `home` and `SCULL_HOME` at `home/scull-home`.
fn scull(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args)
        .env("SCULL_HOME", home.join("scull-home"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("TERM", "dumb")
        .stdin(Stdio::null());
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            cmd.env_remove(k);
        }
    }
    for k in ["NO_COLOR", "SUDO_USER", "DOAS_USER"] {
        cmd.env_remove(k);
    }
    cmd.envs(env.iter().copied());
    cmd.output().expect("run scull")
}

fn validator() -> jsonschema::Validator {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/doctor-report.schema.json");
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(p).expect("schema")).expect("json");
    jsonschema::validator_for(&v).expect("compiles")
}

fn assert_valid(v: &Value) {
    let errors: Vec<String> = validator()
        .iter_errors(v)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{errors:?}\n{v:#}");
}

fn write_config(home: &Path, text: &str) {
    let dir = home.join("scull-home").join("config");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("config.toml"), text).expect("write config");
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).expect("read_dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            out.extend(files(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// No planted secret, and the home folder only as `~`.
fn assert_clean(what: &str, text: &str, home: &Path) {
    for secret in [
        ENV_SECRET,
        UNRELATED_ENV_SECRET,
        FILE_SECRET,
        USER_SECRET,
        NAME_SECRET,
    ] {
        assert!(!text.contains(secret), "{what} leaks {secret}:\n{text}");
    }
    // JSON escapes Windows backslashes, so check the raw and the escaped spelling.
    let h = home.to_string_lossy().into_owned();
    let escaped = serde_json::to_string(&h).expect("json");
    assert!(
        !text.contains(&h) && !text.contains(escaped.trim_matches('"')),
        "{what} shows the home folder {h}:\n{text}"
    );
}

/// FX-DATA-07: the report never contains file contents (a config parse error quoting the
/// file), environment values (an invalid `SCULL_*` value, an unrelated token) or user names,
/// in any output format, although plain `scull doctor` shows the first two to the local user.
#[test]
fn fx_data_07_doctor_report_has_no_file_contents_or_environment_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path();
    write_config(home, &format!("token = \"{FILE_SECRET}\"\n"));
    let env = [
        ("SCULL_THREADS", ENV_SECRET),
        ("GITHUB_TOKEN", UNRELATED_ENV_SECRET),
        ("SUDO_USER", USER_SECRET),
    ];

    // The local, unshared report carries the parse error and the bad value: the planted
    // secrets are reachable, so their absence below means something.
    let plain = scull(home, &["doctor", "--json"], &env);
    let plain = String::from_utf8(plain.stdout).expect("utf-8");
    assert!(plain.contains(FILE_SECRET), "{plain}");
    assert!(plain.contains(ENV_SECRET), "{plain}");

    for args in [
        &["doctor", "--report", "--json"][..],
        &["doctor", "--report", "--ndjson"],
        &["doctor", "--report"],
        &["doctor", "--report", "--hash-names", "--json"],
    ] {
        let o = scull(home, args, &env);
        let out = String::from_utf8(o.stdout).expect("utf-8");
        let err = String::from_utf8_lossy(&o.stderr).into_owned();
        assert_eq!(o.status.code(), Some(0), "{args:?}: {err}");
        assert_clean(&format!("{args:?} stdout"), &out, home);
        assert_clean(&format!("{args:?} stderr"), &err, home);
        if args.contains(&"--json") {
            let v: Value = serde_json::from_str(&out).expect("one JSON document");
            assert_valid(&v);
            assert_eq!(v["schema"], "scull/doctor-report@1");
            let r = &v["data"]["redaction"];
            assert_eq!(r["uploaded"], false);
            assert_eq!(r["home_as_tilde"], true);
            let withheld: Vec<&str> = r["withheld"]
                .as_array()
                .expect("withheld")
                .iter()
                .map(|w| w["field"].as_str().expect("field"))
                .collect();
            assert!(withheld.contains(&"config.error"), "{withheld:?}");
            assert!(withheld.contains(&"config.env_error"), "{withheld:?}");
            let config = &v["data"]["doctor"]["config"];
            assert_eq!(config["file"], "invalid");
            if r["names_hashed"] == false {
                let env_error = config["env_error"].as_str().expect("env_error");
                assert!(env_error.contains("SCULL_THREADS"), "{env_error}");
            }
        }
        if !args.contains(&"--json") && !args.contains(&"--ndjson") {
            assert!(out.contains("Redaction preview"), "{out}");
            assert!(out.contains("never included"), "{out}");
            assert!(out.contains("nothing is ever uploaded"), "{out}");
        }
    }
    // Without --out nothing is written: only the planted config file exists.
    assert_eq!(
        files(home),
        vec![home.join("scull-home").join("config").join("config.toml")]
    );
}

/// FX-DATA-07: settings from the environment show only the variable name; `--hash-names`
/// hides folder names under the home folder; `--out` writes a new owner-only file only after
/// consent (`--yes` without a terminal), never replaces a file, and holds the same report.
#[test]
fn fx_data_07_doctor_report_preview_then_consent_to_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path();
    let root = home.join("work").join(NAME_SECRET);
    std::fs::create_dir_all(&root).expect("mkdir");
    let root_toml = root.to_string_lossy().replace('\\', "\\\\");
    write_config(home, &format!("[scan]\nroots = [\"{root_toml}\"]\n"));
    let env = [("SCULL_MCP_MAX_RESULTS", "4417")];
    let out_file = home.join("report.json");
    let out_arg = out_file.to_string_lossy().into_owned();

    // Without a terminal and without --yes: the preview, then a refusal; nothing written.
    let o = scull(
        home,
        &["doctor", "--report", "--hash-names", "--out", &out_arg],
        &env,
    );
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!out_file.exists());
    let preview = String::from_utf8(o.stdout).expect("utf-8");
    assert!(preview.contains("Redaction preview"), "{preview}");
    assert!(
        preview.contains("Nothing has been written yet"),
        "{preview}"
    );
    assert_clean("preview", &preview, home);
    // Machine output never prompts: refused before anything is printed.
    let o = scull(
        home,
        &["doctor", "--report", "--json", "--out", &out_arg],
        &env,
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(o.stdout.is_empty());
    assert!(!out_file.exists());

    // With --yes: written, owner-only, valid, clean, and what was printed.
    let o = scull(
        home,
        &[
            "doctor",
            "--report",
            "--hash-names",
            "--json",
            "--out",
            &out_arg,
            "--yes",
        ],
        &env,
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let printed: Value = serde_json::from_slice(&o.stdout).expect("JSON");
    let text = std::fs::read_to_string(&out_file).expect("report file");
    assert_clean("report file", &text, home);
    assert!(!text.contains("4417"), "environment value leaked:\n{text}");
    let written: Value = serde_json::from_str(&text).expect("JSON file");
    assert_valid(&written);
    assert_eq!(written["data"]["redaction"], printed["data"]["redaction"]);
    assert_eq!(written["data"]["redaction"]["names_hashed"], true);
    let effective = written["data"]["doctor"]["config"]["effective"]
        .as_array()
        .expect("effective");
    let field = |name: &str| {
        effective
            .iter()
            .find(|v| v["field"] == name)
            .unwrap_or_else(|| panic!("{name}"))
    };
    assert_eq!(
        field("mcp.max_results")["value"],
        "<withheld>: set by SCULL_MCP_MAX_RESULTS"
    );
    let roots = field("scan.roots")["value"].as_array().expect("roots");
    assert_eq!(roots.len(), 1);
    let shown = roots[0].as_str().expect("root");
    assert!(shown.starts_with('~'), "{shown}");
    assert!(shown.contains("h-"), "names hashed: {shown}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&out_file)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "owner-only, got {mode:o}");
    }

    // An existing file is never replaced.
    let o = scull(
        home,
        &["doctor", "--report", "--out", &out_arg, "--yes"],
        &env,
    );
    assert_ne!(o.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&o.stderr).contains("already exists"));
    assert_eq!(std::fs::read_to_string(&out_file).expect("file"), text);
}
